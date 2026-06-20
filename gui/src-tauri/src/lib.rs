//! Tauri v2 backend — full pyred daemon bridge for the pyre GUI.
//!
//! Architecture:
//!   - ONE shared tarpc control client held in Tauri managed state (AppState).
//!     Connected on first use via `ensure_client()`, reused for all subsequent
//!     RPC calls.
//!   - Per-pane output stream tasks tracked in `stream_tasks: HashMap<PaneId, JoinHandle>`.
//!     `attach_pane_stream` opens a 0x02 stream and spawns a pump task;
//!     `detach_pane_stream` aborts it.
//!   - All daemon-down paths return a clean `Err(String)` — no panics.
//!
//! Connection pattern (0x01 control + 0x02 stream) is the same as
//! crates/pyre-tui/src/app/pane_ops.rs and the original spike lib.rs.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use pyre_proto::{
    blocks::{ListBlocksReq, SearchBlocksReq},
    layout::{LayoutNode, Orient},
    OpenPaneSplitReq, OutputFrame, PaneId, PyreDaemonClient, ResizePaneReq, SessionId, SpawnReq,
    SpawnResp,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

// ─────────────────────────────────────────────────────────────────────────────
// Managed state
// ─────────────────────────────────────────────────────────────────────────────

/// All mutable state for the GUI bridge, held behind a single async mutex.
struct Inner {
    client: Option<PyreDaemonClient>,
    /// Per-pane pump tasks keyed by PaneId.  Aborted on detach or pane close.
    stream_tasks: HashMap<PaneId, JoinHandle<()>>,
    /// Pane forwarded by legacy `start_pane` / `send_keys` (kept for compat).
    legacy_pane: Option<PaneId>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            client: None,
            stream_tasks: HashMap::new(),
            legacy_pane: None,
        }
    }
}

pub struct AppState {
    inner: Arc<Mutex<Inner>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: socket resolution
// ─────────────────────────────────────────────────────────────────────────────

fn resolve_socket() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("PYRE_SOCK") {
        return std::path::PathBuf::from(s);
    }
    if let Ok(s) = std::env::var("PYRE_SOCKET") {
        return std::path::PathBuf::from(s);
    }
    pyre_proto::socket::default_socket()
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: connect-or-reuse control client
// ─────────────────────────────────────────────────────────────────────────────

async fn ensure_client(inner: &mut Inner) -> Result<PyreDaemonClient, String> {
    if let Some(ref c) = inner.client {
        return Ok(c.clone());
    }
    let socket = resolve_socket();
    let client = pyre_proto::socket::connect_control(&socket)
        .await
        .map_err(|e| {
            format!(
                "could not connect to pyred at {} — is the daemon running? ({e})",
                socket.display()
            )
        })?;
    inner.client = Some(client.clone());
    Ok(client)
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: parse UUIDs from strings
// ─────────────────────────────────────────────────────────────────────────────

fn parse_pane(s: &str) -> Result<PaneId, String> {
    uuid::Uuid::parse_str(s)
        .map(PaneId)
        .map_err(|e| format!("invalid pane id {s:?}: {e}"))
}

fn parse_session(s: &str) -> Result<SessionId, String> {
    uuid::Uuid::parse_str(s)
        .map(SessionId)
        .map_err(|e| format!("invalid session id {s:?}: {e}"))
}

fn parse_block_id(s: &str) -> Result<pyre_proto::BlockId, String> {
    uuid::Uuid::parse_str(s)
        .map(pyre_proto::BlockId)
        .map_err(|e| format!("invalid block id {s:?}: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: Rgb → "#rrggbb"
// ─────────────────────────────────────────────────────────────────────────────

fn rgb_hex(c: pyre_themes::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

// ─────────────────────────────────────────────────────────────────────────────
// DTO types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct DaemonStatusDto {
    pub connected: bool,
    pub socket: String,
}

#[derive(Serialize)]
pub struct ReconnectDto {
    pub connected: bool,
    pub socket: String,
}

#[derive(Serialize)]
pub struct SessionDto {
    pub id: String,
    pub name: String,
    pub pane_count: u32,
    pub created_at: String,
    pub last_active_at: String,
}

#[derive(Serialize)]
pub struct SpawnDto {
    pub session: String,
    pub pane: String,
}

#[derive(Serialize, Clone)]
pub struct PaneStateDto {
    pub pane: String,
    pub session: String,
    pub state: String,
    pub title: Option<String>,
    pub agent: String,
}

#[derive(Serialize)]
pub struct PidInspectDto {
    pub pid: u32,
    pub comm: String,
    pub env: Vec<(String, String)>,
    pub fds: Vec<String>,
    pub children: Vec<u32>,
}

#[derive(Serialize, Clone)]
pub struct BlockDto {
    pub id: String,
    pub pane: String,
    pub session: String,
    pub command: String,
    pub cwd: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub running: bool,
}

#[derive(Serialize)]
pub struct BlockHitDto {
    pub block: BlockDto,
    pub snippet: String,
}

#[derive(Serialize)]
pub struct OpenPaneDto {
    pub pane: String,
}

/// Recursive layout DTO. `kind` is either "leaf" or "split".
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayoutDto {
    Leaf {
        pane: String,
    },
    Split {
        dir: String,
        children: Vec<LayoutDto>,
        weights: Vec<f32>,
    },
}

#[derive(Serialize)]
pub struct ThemeListItemDto {
    pub name: String,
    pub display_name: String,
    pub kind: String,
    pub accent: String,
    pub bg: String,
    pub fg: String,
}

#[derive(Serialize)]
pub struct ThemeDetailDto {
    pub bg: String,
    pub bg_dim: String,
    pub fg: String,
    pub fg_dim: String,
    pub border: String,
    pub border_focus: String,
    pub cursor: String,
    pub accent: String,
    pub ok: String,
    pub warn: String,
    pub error: String,
    pub ansi: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers: domain → DTO conversions
// ─────────────────────────────────────────────────────────────────────────────

fn pane_state_label(state: &pyre_proto::PaneStateKind) -> &'static str {
    use pyre_proto::PaneStateKind;
    match state {
        PaneStateKind::Running => "running",
        PaneStateKind::WaitingInput => "waiting",
        PaneStateKind::Idle => "idle",
        PaneStateKind::Interactive => "interactive",
        PaneStateKind::Crashed => "crashed",
        PaneStateKind::Done => "done",
    }
}

fn block_to_dto(b: &pyre_proto::Block) -> BlockDto {
    BlockDto {
        id: b.id.0.to_string(),
        pane: b.pane.0.to_string(),
        session: b.session.0.to_string(),
        command: b.command.clone(),
        cwd: b.cwd.as_ref().map(|p| p.display().to_string()),
        started_at: b.started_at.to_rfc3339(),
        ended_at: b.ended_at.map(|t| t.to_rfc3339()),
        exit_code: b.exit_code,
        duration_ms: b.duration_ms(),
        running: b.ended_at.is_none(),
    }
}

fn layout_to_dto(node: &LayoutNode) -> LayoutDto {
    match node {
        LayoutNode::Leaf(pane_id) => LayoutDto::Leaf {
            pane: pane_id.0.to_string(),
        },
        LayoutNode::HSplit(children) => LayoutDto::Split {
            dir: "h".to_string(),
            children: children.iter().map(|(n, _)| layout_to_dto(n)).collect(),
            weights: children.iter().map(|(_, w)| *w as f32).collect(),
        },
        LayoutNode::VSplit(children) => LayoutDto::Split {
            dir: "v".to_string(),
            children: children.iter().map(|(n, _)| layout_to_dto(n)).collect(),
            weights: children.iter().map(|(_, w)| *w as f32).collect(),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: connection state
// ─────────────────────────────────────────────────────────────────────────────

/// Return the current daemon connection status, actively probing with ensure_client.
///
/// On first call (no client cached yet) this attempts a real connection so that
/// the frontend boot path gets an accurate `connected: true` when pyred is up,
/// rather than always reporting false on the first call because the cache is cold.
#[tauri::command]
async fn daemon_status(state: State<'_, AppState>) -> Result<DaemonStatusDto, String> {
    let mut guard = state.inner.lock().await;
    let socket = resolve_socket();
    // Actively try to connect (or reuse the cached client). On failure we still
    // return Ok with connected=false so the frontend can show the "daemon down"
    // panel rather than propagating an Err that tauri turns into a JS exception.
    let connected = ensure_client(&mut guard).await.is_ok();
    Ok(DaemonStatusDto {
        connected,
        socket: socket.display().to_string(),
    })
}

/// Force a fresh control connection (drops the old one if any).
#[tauri::command]
async fn reconnect(state: State<'_, AppState>) -> Result<ReconnectDto, String> {
    let mut guard = state.inner.lock().await;
    guard.client = None;
    let socket = resolve_socket();
    match ensure_client(&mut guard).await {
        Ok(_) => Ok(ReconnectDto {
            connected: true,
            socket: socket.display().to_string(),
        }),
        Err(_) => Ok(ReconnectDto {
            connected: false,
            socket: socket.display().to_string(),
        }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: sessions
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
async fn list_sessions(state: State<'_, AppState>) -> Result<Vec<SessionDto>, String> {
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| format!("list_sessions transport error: {e}"))?
        .map_err(|e| format!("list_sessions daemon error: {e}"))?;
    Ok(sessions
        .iter()
        .map(|s| SessionDto {
            id: s.id.0.to_string(),
            name: s.name.clone(),
            pane_count: s.pane_count,
            created_at: s.created_at.to_rfc3339(),
            last_active_at: s.last_active_at.to_rfc3339(),
        })
        .collect())
}

#[tauri::command]
async fn spawn_session(
    state: State<'_, AppState>,
    cols: u16,
    rows: u16,
) -> Result<SpawnDto, String> {
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let req = SpawnReq {
        shell: None,
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let SpawnResp { session, pane } = client
        .spawn(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("spawn transport error: {e}"))?
        .map_err(|e| format!("spawn daemon error: {e}"))?;
    Ok(SpawnDto {
        session: session.0.to_string(),
        pane: pane.0.to_string(),
    })
}

#[tauri::command]
async fn rename_session(
    state: State<'_, AppState>,
    session: String,
    name: String,
) -> Result<(), String> {
    let sid = parse_session(&session)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    client
        .rename_session(tarpc::context::current(), sid, name)
        .await
        .map_err(|e| format!("rename_session transport error: {e}"))?
        .map_err(|e| format!("rename_session daemon error: {e}"))
}

/// Fully terminate a session: close all its panes and evict it from the
/// registry. Uses the daemon's `close_session` RPC ("close all panes and
/// remove it from the registry"). We then call `gc_stale_sessions` as a
/// belt-and-suspenders sweep so any session left with zero live panes is
/// also evicted — this guards against the pre-eviction zombie-session case
/// the daemon documents on `gc_stale_sessions`.
///
/// Returns a clean `Err(String)` on any transport/daemon failure; never panics.
#[tauri::command]
async fn close_session(state: State<'_, AppState>, session: String) -> Result<(), String> {
    let sid = parse_session(&session)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    client
        .close_session(tarpc::context::current(), sid)
        .await
        .map_err(|e| format!("close_session transport error: {e}"))?
        .map_err(|e| format!("close_session daemon error: {e}"))?;
    // Sweep any sessions now left with zero live panes (idempotent, cheap).
    let _ = client
        .gc_stale_sessions(tarpc::context::current())
        .await
        .map_err(|e| format!("gc_stale_sessions transport error: {e}"))?
        .map_err(|e| format!("gc_stale_sessions daemon error: {e}"))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: panes
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
async fn close_pane(state: State<'_, AppState>, pane: String) -> Result<(), String> {
    let pid = parse_pane(&pane)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    client
        .close_pane(tarpc::context::current(), pid)
        .await
        .map_err(|e| format!("close_pane transport error: {e}"))?
        .map_err(|e| format!("close_pane daemon error: {e}"))
}

#[tauri::command]
async fn resize_pane(
    state: State<'_, AppState>,
    pane: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let pid = parse_pane(&pane)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let req = ResizePaneReq {
        pane_id: pid,
        size: pyre_proto::PaneSize { cols, rows },
    };
    client
        .resize_pane(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("resize_pane transport error: {e}"))?
        .map_err(|e| format!("resize_pane daemon error: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn pane_states(state: State<'_, AppState>) -> Result<Vec<PaneStateDto>, String> {
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let panes = client
        .list_all_panes(tarpc::context::current())
        .await
        .map_err(|e| format!("list_all_panes transport error: {e}"))?
        .map_err(|e| format!("list_all_panes daemon error: {e}"))?;
    Ok(panes
        .iter()
        .map(|p| PaneStateDto {
            pane: p.id.0.to_string(),
            session: p.session.0.to_string(),
            state: pane_state_label(&p.state).to_string(),
            title: p.name.clone(),
            agent: p.agent.label().to_string(),
        })
        .collect())
}

/// Return process metadata for the foreground PID of a pane (Linux-only).
///
/// Maps the daemon's `PidInspect` directly to a serializable DTO. Returns a
/// clean `Err(String)` on any transport or daemon failure — the frontend should
/// treat this as "metadata unavailable" and fall back gracefully.
#[tauri::command]
async fn inspect_pid(state: State<'_, AppState>, pane: String) -> Result<PidInspectDto, String> {
    let pid = parse_pane(&pane)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let info = client
        .inspect_pid(tarpc::context::current(), pid)
        .await
        .map_err(|e| format!("inspect_pid transport error: {e}"))?
        .map_err(|e| format!("inspect_pid daemon error: {e}"))?;
    Ok(PidInspectDto {
        pid: info.pid,
        comm: info.comm,
        env: info.env,
        fds: info.fds,
        children: info.children,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: layout / splits
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
async fn session_layout(
    state: State<'_, AppState>,
    session: String,
) -> Result<LayoutDto, String> {
    let sid = parse_session(&session)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let node = client
        .get_session_layout(tarpc::context::current(), sid)
        .await
        .map_err(|e| format!("get_session_layout transport error: {e}"))?
        .map_err(|e| format!("get_session_layout daemon error: {e}"))?;
    Ok(layout_to_dto(&node))
}

#[tauri::command]
async fn open_split(
    state: State<'_, AppState>,
    pane: String,
    direction: String,
) -> Result<OpenPaneDto, String> {
    let pid = parse_pane(&pane)?;
    let orient = match direction.as_str() {
        "h" => Orient::Horizontal,
        "v" => Orient::Vertical,
        other => return Err(format!("invalid direction {other:?}: expected \"h\" or \"v\"")),
    };
    let req = OpenPaneSplitReq {
        parent_pane: pid,
        orient,
        name: None,
        cwd: None,
        cmd: None,
    };
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let new_pane = client
        .open_pane_split(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("open_pane_split transport error: {e}"))?
        .map_err(|e| format!("open_pane_split daemon error: {e}"))?;
    Ok(OpenPaneDto {
        pane: new_pane.0.to_string(),
    })
}

#[tauri::command]
async fn set_weight(
    state: State<'_, AppState>,
    pane: String,
    weight: f32,
) -> Result<(), String> {
    let pid = parse_pane(&pane)?;
    // Clamp [5, 95] then convert to u16 as the RPC expects.
    let clamped = weight.clamp(5.0, 95.0) as u16;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    client
        .set_pane_weight(tarpc::context::current(), pid, clamped)
        .await
        .map_err(|e| format!("set_pane_weight transport error: {e}"))?
        .map_err(|e| format!("set_pane_weight daemon error: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: per-pane output streams
// ─────────────────────────────────────────────────────────────────────────────

/// Attach a PTY output stream for `pane` in `session`.
///
/// Opens a 0x02 stream connection, spawns a tokio pump task that emits
/// `pty-output` events (`{ pane, bytes }`) to the webview. When the stream
/// ends it emits `pane-closed` (`{ pane }`).
///
/// Idempotent: if a task for this pane already exists it is aborted first.
#[tauri::command]
async fn attach_pane_stream(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session: String,
    pane: String,
) -> Result<(), String> {
    let sid = parse_session(&session)?;
    let pid = parse_pane(&pane)?;
    let socket = resolve_socket();

    // Abort any existing task for this pane.
    {
        let mut guard = state.inner.lock().await;
        if let Some(handle) = guard.stream_tasks.remove(&pid) {
            handle.abort();
        }
    }

    // Open the 0x02 stream connection.
    let stream_sock = pyre_proto::socket::attach_stream(&socket, sid, pid)
        .await
        .map_err(|e| format!("attach_stream failed for pane {pane}: {e}"))?;

    let (rd, _wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());

    let pane_str = pane.clone();
    let app_handle = app.clone();

    let handle = tokio::spawn(async move {
        #[derive(Serialize, Clone)]
        struct PtyOutputPayload {
            pane: String,
            bytes: Vec<u8>,
        }
        #[derive(Serialize, Clone)]
        struct PaneClosedPayload {
            pane: String,
        }

        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    let payload = PtyOutputPayload {
                        pane: pane_str.clone(),
                        bytes: f.data.to_vec(),
                    };
                    if app_handle.emit("pty-output", payload).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = app_handle.emit(
            "pane-closed",
            PaneClosedPayload {
                pane: pane_str.clone(),
            },
        );
    });

    {
        let mut guard = state.inner.lock().await;
        guard.stream_tasks.insert(pid, handle);
    }

    Ok(())
}

/// Abort the pump task for `pane` if one exists.
#[tauri::command]
async fn detach_pane_stream(state: State<'_, AppState>, pane: String) -> Result<(), String> {
    let pid = parse_pane(&pane)?;
    let mut guard = state.inner.lock().await;
    if let Some(handle) = guard.stream_tasks.remove(&pid) {
        handle.abort();
    }
    Ok(())
}

/// Forward keystrokes to a pane's PTY via the `send_keys` RPC.
///
/// The `pane` string is the UUID of the target pane.  When called without
/// a pane argument (legacy `start_pane` flow), falls back to the legacy pane.
#[tauri::command]
async fn send_keys(
    state: State<'_, AppState>,
    pane: Option<String>,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let (client, pid) = {
        let mut guard = state.inner.lock().await;
        let pid = match pane {
            Some(ref s) => parse_pane(s)?,
            None => guard
                .legacy_pane
                .ok_or_else(|| "no active pane — call start_pane or provide pane id".to_string())?,
        };
        let client = ensure_client(&mut guard).await?;
        (client, pid)
    };
    client
        .send_keys(tarpc::context::current(), pid, bytes)
        .await
        .map_err(|e| format!("send_keys transport error: {e}"))?
        .map_err(|e| format!("send_keys daemon error: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: blocks
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
async fn list_blocks(state: State<'_, AppState>, pane: String) -> Result<Vec<BlockDto>, String> {
    let pid = parse_pane(&pane)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    // list_blocks takes a ListBlocksReq with an optional session filter.
    // We filter by pane client-side since the RPC only supports session-level filtering.
    let req = ListBlocksReq {
        session: None,
        limit: 100,
    };
    let blocks = client
        .list_blocks(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("list_blocks transport error: {e}"))?
        .map_err(|e| format!("list_blocks daemon error: {e}"))?;
    Ok(blocks
        .iter()
        .filter(|b| b.pane == pid)
        .map(block_to_dto)
        .collect())
}

#[tauri::command]
async fn block_stdout(state: State<'_, AppState>, block: String) -> Result<String, String> {
    let bid = parse_block_id(&block)?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let bytes = client
        .get_block_stdout(tarpc::context::current(), bid)
        .await
        .map_err(|e| format!("get_block_stdout transport error: {e}"))?
        .map_err(|e| format!("get_block_stdout daemon error: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[tauri::command]
async fn search_blocks(
    state: State<'_, AppState>,
    query: String,
    failures_only: bool,
    session: Option<String>,
) -> Result<Vec<BlockHitDto>, String> {
    let session_id = session.as_deref().map(parse_session).transpose()?;
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let req = SearchBlocksReq {
        query,
        limit: 50,
        failures_only,
        session: session_id,
        pane: None,
        exit_code: None,
    };
    let hits = client
        .search_blocks(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("search_blocks transport error: {e}"))?
        .map_err(|e| format!("search_blocks daemon error: {e}"))?;
    Ok(hits
        .iter()
        .map(|h| BlockHitDto {
            block: block_to_dto(&h.block),
            snippet: h.snippet.clone(),
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: event long-poll
// ─────────────────────────────────────────────────────────────────────────────

/// One pane lifecycle event serialized for the frontend.
///
/// `kind` is one of: "spawned", "closed", "state_changed", "layout_changed".
/// `pane` is the UUID string of the affected pane (always present).
/// `session` is always null — `PaneEvent` does not carry a session id.
/// `state` is the lowercase PaneStateKind string (null when absent).
#[derive(Serialize)]
pub struct PaneEventDto {
    pub kind: &'static str,
    pub pane: String,
    pub session: Option<String>,
    pub state: Option<String>,
}

/// Response from `poll_events`.
#[derive(Serialize)]
pub struct EventsDto {
    pub events: Vec<PaneEventDto>,
    pub last_seq: u64,
}

fn pane_event_kind_label(kind: &pyre_proto::PaneEventKind) -> &'static str {
    use pyre_proto::PaneEventKind;
    match kind {
        PaneEventKind::Spawned => "spawned",
        PaneEventKind::Closed => "closed",
        PaneEventKind::StateChanged => "state_changed",
        PaneEventKind::LayoutChanged => "layout_changed",
    }
}

/// Long-poll for pane lifecycle events from the daemon ring buffer.
///
/// Returns all events whose `seq` is strictly greater than `after_seq`,
/// waiting up to 2000 ms for at least one event to arrive. Returns an
/// empty events list on timeout — the caller should loop passing `last_seq`
/// back as `after_seq` on the next call.
///
/// On daemon error returns a clean `Err(String)` so the frontend can fall
/// back gracefully without a JS exception.
#[tauri::command]
async fn poll_events(
    state: State<'_, AppState>,
    after_seq: u64,
) -> Result<EventsDto, String> {
    let client = {
        let mut guard = state.inner.lock().await;
        ensure_client(&mut guard).await?
    };
    let raw = client
        .next_pane_event(tarpc::context::current(), after_seq, 2000)
        .await
        .map_err(|e| format!("poll_events transport error: {e}"))?
        .map_err(|e| format!("poll_events daemon error: {e}"))?;

    let last_seq = raw.iter().map(|e| e.seq).max().unwrap_or(after_seq);

    let events = raw
        .iter()
        .map(|e| PaneEventDto {
            kind: pane_event_kind_label(&e.kind),
            pane: e.pane_id.0.to_string(),
            session: None,
            state: e.state.as_ref().map(|s| pane_state_label(s).to_string()),
        })
        .collect();

    Ok(EventsDto { events, last_seq })
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: themes (no daemon call — pure registry lookup)
// ─────────────────────────────────────────────────────────────────────────────

#[tauri::command]
fn list_themes() -> Vec<ThemeListItemDto> {
    pyre_themes::Registry::builtin()
        .list()
        .iter()
        .map(|t| ThemeListItemDto {
            name: t.name.to_string(),
            display_name: t.display_name.to_string(),
            kind: match t.kind {
                pyre_themes::ThemeKind::Light => "light".to_string(),
                pyre_themes::ThemeKind::Dark => "dark".to_string(),
            },
            accent: rgb_hex(t.palette.accent),
            bg: rgb_hex(t.palette.bg),
            fg: rgb_hex(t.palette.fg),
        })
        .collect()
}

#[tauri::command]
fn get_theme(name: String) -> Result<ThemeDetailDto, String> {
    let reg = pyre_themes::Registry::builtin();
    let theme = reg
        .get(&name)
        .ok_or_else(|| format!("unknown theme {name:?}"))?;
    let p = &theme.palette;
    Ok(ThemeDetailDto {
        bg: rgb_hex(p.bg),
        bg_dim: rgb_hex(p.bg_dim),
        fg: rgb_hex(p.fg),
        fg_dim: rgb_hex(p.fg_dim),
        border: rgb_hex(p.border),
        border_focus: rgb_hex(p.border_focus),
        cursor: rgb_hex(p.cursor),
        accent: rgb_hex(p.accent),
        ok: rgb_hex(p.ok),
        warn: rgb_hex(p.warn),
        error: rgb_hex(p.error),
        ansi: p.ansi.iter().map(|c| rgb_hex(*c)).collect(),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Legacy: start_pane (kept for backward compat with the original spike frontend)
// ─────────────────────────────────────────────────────────────────────────────

/// Original spike command: connect, spawn a default 80x24 pane, and attach
/// its output stream.  The webview's existing `start_pane` call still works
/// as-is.  New code should prefer `spawn_session` + `attach_pane_stream`.
#[tauri::command]
async fn start_pane(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let socket = resolve_socket();

    let client = {
        let mut guard = state.inner.lock().await;
        // Force a fresh client on start_pane to mirror the original spike behaviour.
        guard.client = None;
        ensure_client(&mut guard).await?
    };

    let req = SpawnReq {
        shell: None,
        cwd: std::env::current_dir().ok(),
        cols: 80,
        rows: 24,
        env: std::env::vars().collect(),
        name: Some("gui-spike".to_string()),
    };
    let SpawnResp { session, pane } = client
        .spawn(tarpc::context::current(), req)
        .await
        .map_err(|e| format!("spawn RPC transport error: {e}"))?
        .map_err(|e| format!("daemon spawn failed: {e}"))?;

    let stream_sock = pyre_proto::socket::attach_stream(&socket, session, pane)
        .await
        .map_err(|e| format!("attach stream failed: {e}"))?;

    let (rd, _wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());

    let app2 = app.clone();
    let handle = tokio::spawn(async move {
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    let bytes: Vec<u8> = f.data.to_vec();
                    if app2.emit("pty-output", bytes).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = app2.emit("pty-closed", ());
    });

    {
        let mut guard = state.inner.lock().await;
        guard.legacy_pane = Some(pane);
        guard.stream_tasks.insert(pane, handle);
    }

    Ok(format!(
        "attached pane {} (session {})",
        &pane.0.to_string()[..8],
        &session.0.to_string()[..8]
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: splash / window lifecycle
// ─────────────────────────────────────────────────────────────────────────────

/// Close the splashscreen window and reveal the main window.
///
/// Called by the frontend once it has finished bootstrapping (daemon connected,
/// initial data loaded).  Tolerates either window being absent — no panic.
#[tauri::command]
async fn close_splash(app: AppHandle) {
    if let Some(splash) = app.get_webview_window("splashscreen") {
        let _ = splash.close();
    }
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands: OS desktop notifications
// ─────────────────────────────────────────────────────────────────────────────

/// Send an OS desktop notification.
///
/// Used by the frontend to surface agent-state changes and other events that
/// the embedded terminal header bar cannot convey (e.g. background job
/// finished, daemon reconnected).  Returns `Err(String)` on failure so the
/// caller can surface or ignore the error without a JS exception.
#[tauri::command]
async fn notify(app: AppHandle, title: String, body: String) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| format!("notification failed: {e}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            app.manage(AppState::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // connection
            daemon_status,
            reconnect,
            // sessions
            list_sessions,
            spawn_session,
            rename_session,
            close_session,
            // panes
            close_pane,
            resize_pane,
            pane_states,
            inspect_pid,
            // layout
            session_layout,
            open_split,
            set_weight,
            // streams
            attach_pane_stream,
            detach_pane_stream,
            send_keys,
            // events
            poll_events,
            // blocks
            list_blocks,
            block_stdout,
            search_blocks,
            // themes
            list_themes,
            get_theme,
            // legacy
            start_pane,
            // splash + notifications
            close_splash,
            notify,
        ])
        .run(tauri::generate_context!())
        .expect("error while running pyre-gui");
}
