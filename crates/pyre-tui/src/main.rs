//! pyre-tui — ratatui-based terminal UI for pyre.
//!
//! Theme: Ember palette (see `theme.rs`).
//!
//! Without a subcommand: spawn a new session+pane, then attach (full-screen).
//! `attach <session> [--pane <id>]`: attach to an existing session/pane.
//!
//! Key bindings (Ctrl-B prefix):
//!   Ctrl-B c  — new tab (opens a new pane)
//!   Ctrl-B n  — next tab
//!   Ctrl-B p  — previous tab
//!   Ctrl-B "  — horizontal split (HSplit active leaf)
//!   Ctrl-B %  — vertical split (VSplit active leaf)
//!   Ctrl-B ←/→/↑/↓ — cycle focus between panes (DFS order)
//!   Ctrl-B x  — close focused pane (removes dead or live pane from layout)
//!   Ctrl-B q  — quit
//!   Ctrl-B [  — enter block ribbon scrollback for focused pane
//!   Ctrl-B ]  — exit block ribbon scrollback
//!   Ctrl-B /  — open search overlay (Tantivy full-text search)
//!   Ctrl-B N  — toggle toast notifications on/off
//!   In scrollback mode: Left/h = prev block, Right/l = next block, Enter/Esc = exit
//!   Search overlay: type to query, Up/Ctrl-P / Down/Ctrl-N to navigate, Enter to jump, Esc to close
//!   Mouse scroll: ScrollUp/ScrollDown over a pane to scroll its scrollback buffer
//!   Mouse click: left-click tab strip to switch tabs; left-click pane to focus it
//!   PgUp/PgDn: scroll focused pane's scrollback buffer (when NOT in prefix / search)
//!   All other keys forwarded to the focused PTY.

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::SinkExt;
use futures::StreamExt;
use pyre_proto::{
    blocks::{BlockHit, SearchBlocksReq},
    layout::{LayoutNode, Orient},
    write_control_client, Block, InputFrame, OpenPaneReq, OpenPaneSplitReq, OutputFrame, PaneId,
    PidInspect, PyreDaemonClient, SessionId, SpawnReq, SpawnResp, MODE_STREAM,
};
use pyre_themes::{Registry, Theme};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph,
};
use ratatui::Terminal;
mod clipboard;
mod fire_motion;
mod model;
mod render;
mod splash;
mod theme;
use fire_motion::AnimClock;
use model::pane::{DragState, EventProxy, PaneEvent, PaneInit, PaneSlot, SplitBoundary};
use model::selection::{ClickTracker, Selection, SelectionBase};
use model::tab::{tab_reorder, Tab};
use model::toast::{pane_event_to_toast, Toast, ToastDeck, ToastKind};
use render::overlay::pager::{render_pager, PagerState};
use render::overlay::picker::{render_theme_picker, ThemePickerState};
use render::overlay::search::{parse_search_input, render_search_overlay, SearchState};
use render::overlay::session_lost::render_session_lost_overlay;
use render::pane::{pane_needs_attention, render_layout, render_pane, TermSize};
use render::sidebar::{
    agent_ui_label, render_sidebar, session_name_for, session_worst_pane,
};
use render::toast::render_toast_deck;
use std::collections::HashMap;
use std::process::Stdio;
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
// EMBER constant removed — all render paths use LegacyTheme::from_palette now.
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, watch};
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
use alacritty_terminal::Term;

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "pyre", version, about = "Pyre TUI — ratatui terminal frontend")]
struct Cli {
    /// Override socket path
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Shell to use when spawning (default: $SHELL)
    #[arg(long, global = true)]
    shell: Option<String>,

    /// Skip the startup flame animation
    #[arg(long, global = true)]
    no_splash: bool,

    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// Attach to an existing session (and optionally a specific pane)
    Attach {
        /// Session id or ≥8-char prefix
        session: String,
        /// Pane id or ≥8-char prefix (default: first pane)
        #[arg(long)]
        pane: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Compute the inner PTY content area for a single full-screen pane given
/// the raw terminal dimensions.
///
/// The ratatui layout removes:
///   - 1 row  sessions strip
///   - 1 row  tabs strip
///   - 1 row  status bar
///   - 2 rows pane border (top + bottom)
///   - 1 row  ribbon strip inside the pane
///   - 2 cols pane border (left + right)
///
/// Total overhead: 6 rows, 2 cols. For splits the first-frame resize RPC
/// corrects the exact size immediately; this gets the initial spawn close
/// enough to avoid shell / fastfetch layout corruption on startup.
fn compute_pane_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    let cols = term_cols.saturating_sub(2).max(1);
    let rows = term_rows.saturating_sub(6).max(1);
    (cols, rows)
}

fn resolve_shell(shell_arg: Option<String>) -> Option<String> {
    shell_arg
        .or_else(|| std::env::var("SHELL").ok())
        .or_else(|| {
            for candidate in ["/bin/bash", "/bin/sh"] {
                if std::path::Path::new(candidate).exists() {
                    return Some(candidate.to_owned());
                }
            }
            None
        })
}

async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    // Try connecting first. On ENOENT/ECONNREFUSED, spawn pyred and retry.
    if let Ok(sock) = try_connect_control(socket).await {
        return Ok(sock);
    }

    // Daemon not running — spawn it.
    let pyred_bin = std::env::var("PYRED_BIN").ok().unwrap_or_else(|| {
        // Sibling of current_exe() (e.g. target/release/pyred next to target/release/pyre-tui).
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.parent().map(|p| p.join("pyred"));
            if let Some(path) = sibling {
                if path.exists() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
        "pyred".to_owned()
    });

    tracing::info!(
        "pyred not reachable at {}; spawning {}",
        socket.display(),
        pyred_bin
    );
    let mut child = TokioCommand::new(&pyred_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Inherit stderr so startup errors from pyred are visible in the
        // user's terminal (e.g. Tantivy lock contention, bind failures).
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn pyred binary '{pyred_bin}'"))?;

    // Poll every 100 ms for up to 5 s (50 attempts).
    // If the child exits before the socket becomes ready, surface its exit
    // status immediately rather than waiting out the full timeout.
    let mut last_err = anyhow!("daemon did not come up after 5 s");
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check whether the child already exited (e.g. crashed on lock).
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(anyhow!(
                    "spawned pyred exited immediately with status {status}; \
                     check stderr for details (Tantivy lock? stale socket?)"
                ));
            }
            Ok(None) => {} // still running — keep polling
            Err(e) => tracing::warn!("try_wait on pyred child: {e}"),
        }

        match try_connect_control(socket).await {
            Ok(client) => return Ok(client),
            Err(e) => last_err = e,
        }
    }
    Err(last_err).with_context(|| {
        format!(
            "spawned pyred but socket {} never became ready; check pyred logs",
            socket.display()
        )
    })
}

/// Single non-retrying connect attempt; wraps the mode-byte handshake.
async fn try_connect_control(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    write_control_client(&mut sock).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

async fn resolve_session(client: &PyreDaemonClient, prefix: &str) -> Result<SessionId> {
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no session matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} sessions match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

async fn resolve_pane(
    client: &PyreDaemonClient,
    session: SessionId,
    prefix: &str,
) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no pane matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} panes match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

async fn first_pane(client: &PyreDaemonClient, session: SessionId) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    panes
        .into_iter()
        .next()
        .map(|p| p.id)
        .ok_or_else(|| anyhow!("session has no panes"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Terminal restore guard
// ─────────────────────────────────────────────────────────────────────────────

struct TermGuard;

impl TermGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Key serialization
// ─────────────────────────────────────────────────────────────────────────────

fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Bytes> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let byte = (c.to_ascii_lowercase() as u8) & 0x1f;
            return Some(Bytes::copy_from_slice(&[byte]));
        }
    }

    let bytes: &[u8] = match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            return Some(Bytes::copy_from_slice(s.as_bytes()));
        }
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        _ => return None,
    };
    Some(Bytes::copy_from_slice(bytes))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuItem {
    Copy,
    KillPane,
    SplitH,
    SplitV,
    ZoomToggle,
    InspectPid,
}

impl MenuItem {
    fn label(self) -> &'static str {
        match self {
            Self::Copy => " Copy selection",
            Self::KillPane => " Kill pane",
            Self::SplitH => " Split horizontal",
            Self::SplitV => " Split vertical",
            Self::ZoomToggle => " Zoom toggle",
            Self::InspectPid => " Inspect PID",
        }
    }
}

const MENU_ITEMS: &[MenuItem] = &[
    MenuItem::Copy,
    MenuItem::KillPane,
    MenuItem::SplitH,
    MenuItem::SplitV,
    MenuItem::ZoomToggle,
    MenuItem::InspectPid,
];

struct ContextMenu {
    rect: Rect,
    cursor: usize,
    target_slot: usize,
    item_rects: Vec<Rect>,
}

/// Per-session view: tabs and panes for one daemon session.
struct SessionView {
    id: SessionId,
    name: String,
    tabs: Vec<Tab>,
    active_tab: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptKind {
    NewSession,
    NewTab,
    RenameSession(SessionId),
}

struct NamePrompt {
    kind: PromptKind,
    input: String,
}

#[allow(dead_code)]
pub(crate) struct AppState {
    /// All known sessions (may have tabs loaded lazily).
    sessions: Vec<SessionView>,
    /// Index into `sessions` that is currently displayed.
    active_session: usize,
    /// All attached pane slots (shared across all sessions). None = closed/removed.
    slots: Vec<Option<PaneSlot>>,
    /// Set to `true` when the active session's active tab has no live pane slots.
    /// Triggers a "Session ended" overlay and accepts q/Esc/Ctrl-C to quit.
    session_lost: bool,
    control: PyreDaemonClient,
    socket: PathBuf,
    shell: Option<String>,
    search: SearchState,
    /// One-line status message shown when action feedback is needed.
    status_msg: Option<String>,
    /// Whether the sidebar is visible.
    sidebar_open: bool,
    /// Cached pane info — used for sidebar display AND pane border titles.
    /// Refreshed every second regardless of sidebar visibility so that
    /// `render_pane` can resolve user-provided names even when the sidebar
    /// is closed.
    sidebar_data: Vec<pyre_proto::PaneInfo>,
    /// Last time sidebar data was fetched.
    sidebar_last_poll: Instant,
    /// Selected row index within the sidebar.
    sidebar_cursor: usize,
    /// Whether the sidebar panel has keyboard focus.
    sidebar_focused: bool,
    /// Active text selection (drag or click-to-select).
    selection: Option<Selection>,
    /// State for double/triple-click detection.
    last_click: Option<ClickTracker>,
    /// Right-click context menu state.
    context_menu: Option<ContextMenu>,
    /// PID inspect overlay data.
    pid_inspect: Option<PidInspect>,
    /// Name-prompt overlay (new session or new tab).
    prompt: Option<NamePrompt>,
    /// Session strip hit-test rects: (session_vec_index, rect).
    session_strip_rects: Vec<(usize, Rect)>,
    /// Horizontal scroll offset (in columns) for the session strip.
    session_strip_scroll: usize,
    /// Rect of the left-scroll indicator `◄` in the session strip (when overflow left).
    session_strip_left_arrow: Option<Rect>,
    /// Rect of the right-scroll indicator `►` in the session strip (when overflow right).
    session_strip_right_arrow: Option<Rect>,
    /// Rect of the [+] button in the session strip.
    session_plus_rect: Option<Rect>,
    /// Rect of the [+] button in the tabs strip.
    tab_plus_rect: Option<Rect>,
    /// Queued resize RPCs collected by render_pane (sync); drained after each draw.
    pending_resizes: Vec<(PaneId, pyre_proto::PaneSize)>,
    /// Per-tab chip rects captured during last render: vec of (tab_vec_index, chip_rect).
    tab_chip_rects: Vec<(usize, Rect)>,
    /// Active tab-drag: (tab_vec_index, start_col) — set on mouse-down on a chip.
    dragging_tab: Option<(usize, u16)>,
    /// Rect of the pager overlay as rendered last frame (for mouse-wheel routing).
    pager_rect: Option<Rect>,
    /// Last time the session list was refreshed from the daemon.
    session_list_last_poll: Instant,
    /// Last time the active session's layout was resynced from the daemon.
    /// Acts as a safety-net periodic refresh of the tab's LayoutNode tree.
    layout_resync_last_poll: Instant,
    /// Latest block snapshot delivered by the background poll task.
    /// Key = PaneId, value = blocks for that pane (up to 20, newest last).
    blocks_rx: watch::Receiver<HashMap<PaneId, Vec<Block>>>,
    /// In-TUI ember motion (shared curves with startup splash).
    anim: AnimClock,
    /// Block stdout modal pager (Some = open, None = closed).
    pager: Option<PagerState>,
    /// Active theme (loaded from config on startup, switchable at runtime).
    theme: Theme,
    /// Theme picker overlay (Some = open, None = closed).
    theme_picker: Option<ThemePickerState>,
    /// Ephemeral toast notifications (pane state changes).
    toast_deck: ToastDeck,
    /// Receiver for toasts produced by the background push-event task.
    toast_rx: mpsc::Receiver<Toast>,
    /// Deferred async action queued by the (sync) mouse handler and drained in the event loop.
    pending_menu_action: Option<PendingMenuAction>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Deferred async actions (queued by sync mouse handler, drained in event loop)
// ─────────────────────────────────────────────────────────────────────────────

/// Actions that require async context (RPC calls) but originate from the sync
/// `handle_mouse` function. The event loop drains this after every mouse event.
#[allow(dead_code)]
enum PendingMenuAction {
    /// Execute the highlighted item of the context menu.
    ContextMenuCommit,
    /// Activate a specific context menu item by index (mouse-left on item row).
    ContextMenuActivate(usize),
    /// Split active pane horizontally (HSplit).
    SplitH,
    /// Split active pane vertically (VSplit).
    SplitV,
    /// Open a rename prompt for the active session.
    RenameSession,
    /// Jump to search result at given index (mouse click on result row).
    SearchJump(usize),
}

impl AppState {
    /// Convenience: active session's session id.
    fn active_session_id(&self) -> SessionId {
        self.sessions[self.active_session].id
    }

    /// Convenience: active session view (mutable).
    fn active_session_view_mut(&mut self) -> &mut SessionView {
        &mut self.sessions[self.active_session]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Layout helpers (M7-D: PaneId-keyed, delegates to pyre_proto::layout)
// ─────────────────────────────────────────────────────────────────────────────

/// Walk the tree DFS and collect `PaneId` for every leaf, in order.
fn pane_leaves_in_order(node: &LayoutNode) -> Vec<PaneId> {
    match node {
        LayoutNode::Leaf(id) => vec![*id],
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => children
            .iter()
            .flat_map(|(c, _)| pane_leaves_in_order(c))
            .collect(),
    }
}

/// Return the slot index for `focus_pane` in `slots`.
fn focused_slot_idx(focus_pane: PaneId, slots: &[Option<PaneSlot>]) -> Option<usize> {
    pane_to_slot_idx(slots, focus_pane)
}

/// Mutably access the children of the split node at `path`.
fn children_at_mut<'a>(
    root: &'a mut LayoutNode,
    path: &[usize],
) -> Option<&'a mut Vec<(LayoutNode, u16)>> {
    let mut node = root;
    for &idx in path {
        match node {
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                node = &mut children[idx].0;
            }
            LayoutNode::Leaf(_) => return None,
        }
    }
    match node {
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => Some(children),
        LayoutNode::Leaf(_) => None,
    }
}

/// Look up the slot index for a `PaneId` by scanning `slots`.
fn pane_to_slot_idx(slots: &[Option<PaneSlot>], pane_id: PaneId) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.as_ref().map(|sl| sl.pane_id == pane_id).unwrap_or(false))
}

/// Build a `PaneId → slot_idx` map for the current slots vec.
fn build_pane_slot_map(slots: &[Option<PaneSlot>]) -> HashMap<PaneId, usize> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|sl| (sl.pane_id, i)))
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Mouse hit-test helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk the layout tree and collect (PaneId, screen_rect) for each leaf,
/// computing rects the same way render_layout does (without actually rendering).
/// Callers convert PaneId → slot_idx via `pane_to_slot_idx`.
fn collect_leaf_rects(node: &LayoutNode, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        LayoutNode::Leaf(pane_id) => {
            out.push((*pane_id, area));
        }
        LayoutNode::HSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
        LayoutNode::VSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
    }
}

/// Returns true if (col, row) is inside `rect`.
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

// ─────────────────────────────────────────────────────────────────────────────
// Stream connection + background tasks for one pane
// ─────────────────────────────────────────────────────────────────────────────

async fn attach_pane(
    socket: &Path,
    session: SessionId,
    pane_id: PaneId,
    cols: u16,
    rows: u16,
) -> Result<PaneSlot> {
    tracing::debug!(cols, rows, pane_id = %pane_id.0, "attach_pane: entry");

    let mut stream_sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane_id.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // Bug B fix: bumped to 1024 to absorb output bursts without blocking the
    // net→UI task. The sender uses try_send so a full channel drops the chunk
    // (output loss) instead of hanging the UI loop (backpressure stall).
    let (net_tx, output_rx) = mpsc::channel::<PaneEvent>(1024);
    let (input_tx, mut key_rx) = mpsc::channel::<Bytes>(64);

    // net → UI
    tokio::spawn(async move {
        let mut frames: u64 = 0;
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    frames += 1;
                    if let Err(e) = net_tx.try_send(PaneEvent::Output(f.data)) {
                        match e {
                            mpsc::error::TrySendError::Full(_) => {
                                // Channel saturated during burst — drop chunk, keep running.
                                tracing::warn!("net→UI channel full; dropping output chunk");
                            }
                            mpsc::error::TrySendError::Closed(_) => break,
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Stream ended. Carry frame count so the UI can distinguish a
        // connection-level failure (0 frames) from a real pane exit (≥1 frames).
        let _ = net_tx.try_send(PaneEvent::Closed {
            frames_received: frames,
        });
    });

    // UI → net
    // Batch keystrokes: after the first byte arrives, drain all queued bytes
    // into a single concatenated buffer and send one InputFrame per tick.
    // This converts N sequential framed UDS writes down to 1 per render tick,
    // eliminating per-keystroke serialization latency for fast typists.
    tokio::spawn(async move {
        while let Some(first) = key_rx.recv().await {
            // Drain any additional bytes already queued in the channel.
            let mut buf: Vec<u8> = first.to_vec();
            while let Ok(more) = key_rx.try_recv() {
                buf.extend_from_slice(&more);
            }
            let batch_len = buf.len();
            let t0 = std::time::Instant::now();
            let send_result = input_frames
                .send(InputFrame {
                    session,
                    data: Bytes::from(buf),
                })
                .await;
            let elapsed_us = t0.elapsed().as_micros();
            tracing::debug!(
                batch_bytes = batch_len,
                elapsed_us,
                send_ok = send_result.is_ok(),
                "send_keys: input_frames RPC send"
            );
            if send_result.is_err() {
                break;
            }
        }
    });

    tracing::debug!(rows, cols, pane_id = %pane_id.0, "attach_pane: creating alacritty Term");
    let event_proxy = EventProxy::new();
    let term_config = TermConfig::default();
    // (cols, rows) implements Dimensions via the tuple impl in alacritty_terminal.
    let term = Term::new(
        term_config,
        &TermSize::new(cols as usize, rows as usize),
        event_proxy.clone(),
    );
    Ok(PaneSlot {
        pane_id,
        term,
        processor: AnsiProcessor::new(),
        event_proxy,
        input_tx,
        output_rx,
        recent_blocks: Vec::new(),
        ribbon_cursor: None,
        last_sent_size: (cols, rows),
        frames_received: 0,
        scroll_offset: 0,
        scrollback_capacity: 0,
        last_screen_rect: Rect::default(),
        ribbon_chip_rects: Vec::new(),
        pending_output: Vec::new(),
        parser_sized: false,
        last_output_log: None,
    })
}

/// Render the name-prompt overlay and position the host cursor.
fn render_name_prompt(
    frame: &mut ratatui::Frame,
    prompt: &NamePrompt,
    anim_frame: u64,
    t: &theme::LegacyTheme,
) {
    let area = frame.area();
    let w = (area.width as f32 * 0.60) as u16;
    let h: u16 = 5;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(30), h);

    frame.render_widget(Clear, overlay_rect);

    let title = match prompt.kind {
        PromptKind::NewSession => " new session name ",
        PromptKind::NewTab => " new tab label ",
        PromptKind::RenameSession(_) => " rename session ",
    };

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .title(Span::styled(title, t.title(t.primary)))
        .style(t.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Input row (row 0 of inner) + hint row (row 1).
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let input_area = split[0];
    let hint_area = split[1];

    let input_spans = vec![
        Span::styled("> ", Style::default().fg(t.primary)),
        Span::styled(prompt.input.as_str(), Style::default().fg(t.text)),
        Span::styled(
            "█",
            fire_motion::ember_fg_style(anim_frame, 0xc0ffee, t.spark, t.secondary, 0.9),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(input_spans)), input_area);

    let hint =
        Paragraph::new(" Enter = create  |  Esc = cancel").style(Style::default().fg(t.text_dim));
    frame.render_widget(hint, hint_area);

    // Host cursor at end of input.
    let cursor_col = (2u16 + prompt.input.len() as u16).min(input_area.width.saturating_sub(1));
    frame.set_cursor_position((input_area.x + cursor_col, input_area.y));
}

/// Render the theme picker overlay (Ctrl-B T).
///
/// Layout: centered modal ~60% wide × ~70% tall.
/// Each row shows: `[kind] display_name  ░ bg ░ fg ░ accent ░ border_focus ░ cursor ░ ok ░ warn ░ error`
/// Each swatch reflects THAT theme's own palette, not the active theme.

/// Render the right-click context menu overlay.
///
/// The menu is a small popup anchored at `menu.rect`. Items are drawn
/// with the cursor row highlighted; Esc/Enter/click outside dismisses.
fn render_context_menu(frame: &mut ratatui::Frame, state: &mut AppState, t: &theme::LegacyTheme) {
    let menu = match state.context_menu.as_ref() {
        Some(m) => m,
        None => return,
    };

    // Compute a rect that fits the menu — width = longest label + 2, height = items + 2 (border).
    let max_label = MENU_ITEMS
        .iter()
        .map(|i| i.label().len())
        .max()
        .unwrap_or(10) as u16;
    let w = max_label + 4; // left border + space + label + right border
    let h = MENU_ITEMS.len() as u16 + 2;
    let area = frame.area();
    // Clamp so the menu stays on screen.
    let x = menu.rect.x.min(area.width.saturating_sub(w));
    let y = menu.rect.y.min(area.height.saturating_sub(h));
    let popup = Rect::new(x, y, w, h);

    frame.render_widget(Clear, popup);

    let block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.border_focus())
        .style(t.overlay());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let cursor = menu.cursor;
    // Collect item rects so the mouse handler can hit-test individual rows.
    let mut new_item_rects: Vec<Rect> = Vec::with_capacity(MENU_ITEMS.len());
    for (idx, item) in MENU_ITEMS.iter().enumerate() {
        if idx >= inner.height as usize {
            break;
        }
        let row_y = inner.y + idx as u16;
        let is_selected = idx == cursor;
        let style = if is_selected {
            t.selection()
        } else {
            Style::default().fg(t.text).bg(t.bg)
        };
        let label = format!("{:<width$}", item.label(), width = inner.width as usize);
        let item_rect = Rect::new(inner.x, row_y, inner.width, 1);
        frame.render_widget(Paragraph::new(Span::styled(label, style)), item_rect);
        new_item_rects.push(item_rect);
    }
    // Write back so the mouse handler has fresh rects every frame.
    if let Some(ref mut m) = state.context_menu {
        m.item_rects = new_item_rects;
    }
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut AppState,
    prefix_active: bool,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let t = theme::LegacyTheme::from_palette(&state.theme.palette);

        // Short-circuit: when session_lost is active, render only the overlay.
        if state.session_lost {
            frame.render_widget(RatatuiBlock::default().style(t.bg_style()), area);
            render_session_lost_overlay(frame, &t);
            return;
        }

        // Four rows: sessions strip (1) + tabs strip (1) + body (min 0) + status bar (1)
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let sessions_area = outer[0];
        let tabs_area = outer[1];
        let body_area = outer[2];
        let status_area = outer[3];

        // Frame clear — paint entire frame with bg_style so no bleed.
        frame.render_widget(RatatuiBlock::default().style(t.bg_style()), frame.area());

        // ── Row 0: sessions strip (with horizontal scroll) ──
        {
            // Arrow indicator width: 1 column each.
            const ARROW_W: u16 = 1;
            let viewport_w = sessions_area.width as usize;

            // Build all pill labels and compute their natural (unscrolled) widths.
            // pill_items: (session_index, label, width, style)
            struct PillItem {
                sess_idx: usize,
                label: String,
                width: usize,
                style: Style,
            }
            let mut pill_items: Vec<PillItem> = Vec::new();
            let anim_f = state.anim.frame();
            for (i, sv) in state.sessions.iter().enumerate() {
                let rollup = session_worst_pane(&state.sidebar_data, sv.id);
                let rollup_tag = rollup
                    .map(|p| format!(":{}", agent_ui_label(p.state, p.seen)))
                    .unwrap_or_default();
                let label = format!(" {} {}{} ", i + 1, sv.name, rollup_tag);
                let len = label.chars().count();
                let needs_attention = rollup
                    .is_some_and(|p| p.state == pyre_proto::PaneStateKind::WaitingInput && !p.seen);
                let style = if i == state.active_session {
                    t.tab_active()
                } else if needs_attention {
                    fire_motion::ember_fg_style(
                        anim_f,
                        sv.id.0.as_u128() as u32,
                        t.spark,
                        t.primary,
                        1.0,
                    )
                    .bg(t.bg)
                } else {
                    t.tab_inactive()
                };
                pill_items.push(PillItem {
                    sess_idx: i,
                    label,
                    width: len,
                    style,
                });
            }

            // Compute cumulative column offsets (virtual, unscrolled).
            // Each pill is followed by a 1-column separator space (except the last).
            // Then " [+]" (1 space + 3 chars = 4 cols) at the end.
            let mut offsets: Vec<usize> = Vec::with_capacity(pill_items.len());
            let mut col_cur: usize = 0;
            for (idx, item) in pill_items.iter().enumerate() {
                offsets.push(col_cur);
                col_cur += item.width;
                if idx + 1 < pill_items.len() {
                    col_cur += 1; // separator space
                }
            }
            // [+] button: 1 space separator + 3 chars = 4 wide.
            let plus_virtual_x = col_cur + 1; // +1 space before [+]
            let total_virtual_w = plus_virtual_x + 3; // "[+]"

            // Auto-scroll: bring the active session pill into view.
            // Available viewport columns after reserving space for arrows.
            let needs_left_arrow = state.session_strip_scroll > 0;
            let needs_right_arrow = total_virtual_w > viewport_w + state.session_strip_scroll;
            // Reserve arrow slots when they will be shown.
            let left_reserved: usize = if needs_left_arrow {
                ARROW_W as usize
            } else {
                0
            };
            let right_reserved: usize = if needs_right_arrow {
                ARROW_W as usize
            } else {
                0
            };
            let visible_w = viewport_w.saturating_sub(left_reserved + right_reserved);

            if !pill_items.is_empty() {
                let active = state.active_session.min(pill_items.len() - 1);
                let pill_start = offsets[active];
                let pill_end = pill_start + pill_items[active].width;
                // Scroll left if pill start is behind the left viewport edge.
                if pill_start < state.session_strip_scroll + left_reserved {
                    state.session_strip_scroll = pill_start.saturating_sub(left_reserved);
                }
                // Scroll right if pill end is beyond the right viewport edge.
                let view_end = state.session_strip_scroll + left_reserved + visible_w;
                if pill_end > view_end {
                    state.session_strip_scroll = pill_end
                        .saturating_sub(visible_w)
                        .saturating_sub(left_reserved);
                }
            }
            // Clamp scroll so we don't over-scroll past content.
            let max_scroll = total_virtual_w.saturating_sub(viewport_w);
            state.session_strip_scroll = state.session_strip_scroll.min(max_scroll);

            // Recompute arrow visibility after potential scroll adjustment.
            let needs_left_arrow = state.session_strip_scroll > 0;
            let needs_right_arrow = total_virtual_w > viewport_w + state.session_strip_scroll;
            let left_reserved: usize = if needs_left_arrow {
                ARROW_W as usize
            } else {
                0
            };
            let right_reserved: usize = if needs_right_arrow {
                ARROW_W as usize
            } else {
                0
            };
            let content_start_col = sessions_area.x + left_reserved as u16;
            let content_viewport_w = viewport_w.saturating_sub(left_reserved + right_reserved);

            // Render left arrow.
            let left_arrow_rect = if needs_left_arrow && sessions_area.height > 0 {
                Some(Rect::new(sessions_area.x, sessions_area.y, ARROW_W, 1))
            } else {
                None
            };
            if let Some(r) = left_arrow_rect {
                frame.render_widget(
                    Paragraph::new("◄").style(Style::default().fg(t.text_dim).bg(t.bg)),
                    r,
                );
            }

            // Render right arrow.
            let right_arrow_x = sessions_area.x + sessions_area.width - ARROW_W;
            let right_arrow_rect = if needs_right_arrow && sessions_area.height > 0 {
                Some(Rect::new(right_arrow_x, sessions_area.y, ARROW_W, 1))
            } else {
                None
            };
            if let Some(r) = right_arrow_rect {
                frame.render_widget(
                    Paragraph::new("►").style(Style::default().fg(t.text_dim).bg(t.bg)),
                    r,
                );
            }

            // Build visible spans within [scroll, scroll + content_viewport_w).
            let scroll = state.session_strip_scroll;
            let mut new_session_rects: Vec<(usize, Rect)> = Vec::new();
            let mut spans: Vec<Span> = Vec::new();
            // Virtual x position in content space (relative to scroll origin).
            let mut vx: usize = 0;

            for (idx, item) in pill_items.iter().enumerate() {
                let pill_vstart = offsets[idx];
                let pill_vend = pill_vstart + item.width;

                // Skip pills entirely to the left of the viewport.
                if pill_vend <= scroll {
                    vx = pill_vend;
                    if idx + 1 < pill_items.len() {
                        vx += 1;
                    }
                    continue;
                }
                // Stop when the pill starts past the right edge.
                if pill_vstart >= scroll + content_viewport_w {
                    break;
                }

                // Add separator space between pills if needed.
                if idx > 0 && vx > scroll {
                    let sep_screen_col = content_start_col + (vx - scroll) as u16;
                    let _ = sep_screen_col; // drawn via span below
                    spans.push(Span::styled(" ", Style::default().bg(t.bg)));
                    vx += 1;
                } else if idx > 0 {
                    // The separator was scrolled off; move vx forward.
                    vx += 1;
                }

                // Clip the label to the visible window.
                let label_chars: Vec<char> = item.label.chars().collect();
                let clip_start = scroll.saturating_sub(vx);
                let clip_end = (scroll + content_viewport_w).saturating_sub(vx);
                let clip_end = clip_end.min(label_chars.len());
                let visible_label: String = label_chars[clip_start..clip_end].iter().collect();
                let visible_len = visible_label.chars().count() as u16;

                // Compute screen rect for hit-test (maps to full pill, even if clipped).
                // We store the screen rect for the visible portion so clicks land correctly.
                let screen_x = content_start_col + (vx + clip_start).saturating_sub(scroll) as u16;
                if sessions_area.height > 0 && visible_len > 0 {
                    new_session_rects.push((
                        item.sess_idx,
                        Rect::new(screen_x, sessions_area.y, visible_len, 1),
                    ));
                }

                spans.push(Span::styled(visible_label, item.style));
                vx = pill_vend;
            }

            // [+] button — show only if it fits in the viewport.
            let plus_visible_start = plus_virtual_x.saturating_sub(scroll);
            let plus_visible_end = plus_virtual_x + 3;
            let plus_rect = if sessions_area.height > 0
                && plus_visible_end > scroll
                && plus_virtual_x < scroll + content_viewport_w
            {
                let plus_screen_x = content_start_col + plus_visible_start as u16;
                // Add separator space before [+] when it fits.
                if plus_virtual_x > scroll && vx <= scroll + content_viewport_w {
                    spans.push(Span::styled(" ", Style::default().bg(t.bg)));
                }
                let clip_s = scroll.saturating_sub(plus_virtual_x);
                let clip_e = (scroll + content_viewport_w)
                    .saturating_sub(plus_virtual_x)
                    .min(3);
                let plus_chars: Vec<char> = "[+]".chars().collect();
                let plus_visible: String = plus_chars[clip_s..clip_e].iter().collect();
                let plus_w = plus_visible.chars().count() as u16;
                spans.push(Span::styled(plus_visible, t.tab_inactive()));
                if plus_w > 0 {
                    Some(Rect::new(plus_screen_x, sessions_area.y, plus_w, 1))
                } else {
                    None
                }
            } else {
                None
            };

            state.session_strip_rects = new_session_rects;
            state.session_strip_left_arrow = left_arrow_rect;
            state.session_strip_right_arrow = right_arrow_rect;
            state.session_plus_rect = plus_rect;

            // Render visible content into the content sub-area.
            let content_area = Rect::new(
                content_start_col,
                sessions_area.y,
                content_viewport_w as u16,
                sessions_area.height,
            );
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg)),
                content_area,
            );
        }

        // ── Row 1: tabs strip of active session ──
        {
            let sv = &state.sessions[state.active_session];
            let total_tabs = sv.tabs.len();
            let mut spans: Vec<Span> = Vec::new();
            let mut x_cursor: u16 = tabs_area.x;
            let mut new_tab_chip_rects: Vec<(usize, Rect)> = Vec::new();

            for (i, _) in sv.tabs.iter().enumerate() {
                // Each chip: " N ×" — label + close button.
                let label = format!(" {} ×", i + 1);
                let len = label.chars().count() as u16;
                let style = if i == sv.active_tab {
                    t.tab_active()
                } else {
                    t.tab_inactive()
                };
                if tabs_area.height > 0 {
                    new_tab_chip_rects.push((i, Rect::new(x_cursor, tabs_area.y, len, 1)));
                }
                x_cursor += len;
                spans.push(Span::styled(label, style));
                if i + 1 < total_tabs {
                    spans.push(Span::styled(" ", Style::default().bg(t.bg)));
                    x_cursor += 1;
                }
            }

            // [+] button immediately after the last tab label (browser-style).
            let plus_label = "[+]";
            let plus_len = plus_label.len() as u16;
            let plus_x = x_cursor;
            let plus_rect =
                if tabs_area.height > 0 && plus_x + plus_len <= tabs_area.x + tabs_area.width {
                    Some(Rect::new(plus_x, tabs_area.y, plus_len, 1))
                } else {
                    None
                };
            if !spans.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(t.bg)));
            }
            spans.push(Span::styled(plus_label, t.tab_inactive()));

            state.tab_plus_rect = plus_rect;
            state.tab_chip_rects = new_tab_chip_rects;

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg)),
                tabs_area,
            );
        }

        // Body — optionally split horizontally for sidebar.
        let (sidebar_area_opt, pane_body_area) = if state.sidebar_open {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Min(0)])
                .split(body_area);
            (Some(cols[0]), cols[1])
        } else {
            (None, body_area)
        };

        if let Some(sbar_area) = sidebar_area_opt {
            render_sidebar(frame, sbar_area, state, &t);
        }

        // Render active tab's layout in the remaining area.
        let active_tab_idx = state.sessions[state.active_session].active_tab;
        let focus_pane_id = state.sessions[state.active_session].tabs[active_tab_idx].focus_pane;
        let zoomed = state.sessions[state.active_session].tabs[active_tab_idx].zoomed;
        let mut new_boundaries: Vec<SplitBoundary> = Vec::new();

        // SAFETY: we only borrow root via a raw pointer to avoid the
        // simultaneous mutable borrow of slots. render_layout only reads `root`
        // and mutates `slots` at disjoint indices; no mutation of `tabs` occurs.
        let root_ptr: *const LayoutNode =
            &state.sessions[state.active_session].tabs[active_tab_idx].root;

        let anim_frame = state.anim.frame();
        let panes_meta = state.sidebar_data.as_slice();

        // Build pane_slot map once per frame for O(1) lookups in render_layout.
        let pane_slot_map = build_pane_slot_map(&state.slots);

        if let Some(zoom_pane) = zoomed {
            // Zoom mode: render only the zoomed pane filling pane_body_area.
            if let Some(&slot_idx) = pane_slot_map.get(&zoom_pane) {
                if let Some(slot) = state.slots[slot_idx].as_mut() {
                    let attention = pane_needs_attention(panes_meta, slot.pane_id);
                    render_pane(
                        frame,
                        pane_body_area,
                        slot,
                        true,
                        state.selection.as_ref(),
                        slot_idx,
                        &mut state.pending_resizes,
                        anim_frame,
                        attention,
                        &t,
                        panes_meta,
                    );
                }
            }
        } else {
            let mut current_path: Vec<usize> = Vec::new();
            render_layout(
                frame,
                pane_body_area,
                unsafe { &*root_ptr },
                &mut state.slots,
                focus_pane_id,
                &pane_slot_map,
                &mut current_path,
                &mut new_boundaries,
                state.selection.as_ref(),
                &mut state.pending_resizes,
                anim_frame,
                panes_meta,
                &t,
            );
        }
        state.sessions[state.active_session].tabs[active_tab_idx].boundaries = new_boundaries;

        // Status bar — two segments + optional middle message.
        {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot = focused_slot_idx(tab.focus_pane, &state.slots);
            let is_zoomed = tab.zoomed.is_some();

            // Determine mode label and mid message.
            let (mode_label, mid_msg) = if state.search.open {
                (
                    "SEARCH",
                    Some(format!(
                        " search: {} ({} results) ",
                        state.search.input,
                        state.search.results.len()
                    )),
                )
            } else if prefix_active {
                (
                    "PREFIX",
                    state.status_msg.as_ref().map(|m| format!(" {m} ")),
                )
            } else if let Some(slot_idx) = focused_slot {
                let in_ribbon = state.slots[slot_idx]
                    .as_ref()
                    .map(|s| s.ribbon_cursor.is_some())
                    .unwrap_or(false);
                if in_ribbon {
                    (
                        "SCROLL",
                        state.status_msg.as_ref().map(|m| format!(" {m} ")),
                    )
                } else {
                    ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
                }
            } else {
                ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
            };

            // Left: ` ● {session_name} ▸ {pane} `
            let left_text = if let Some(slot_idx) = focused_slot {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    let pane_short = &slot.pane_id.0.to_string()[..8];
                    format!(" ● {} ▸ {pane_short} ", sv.name)
                } else {
                    format!(" ● {} ", sv.name)
                }
            } else {
                format!(" ● {} ", sv.name)
            };

            // Right: mode indicator + optional ZOOM chip
            let right_text = format!(" {mode_label} ");

            let mut status_spans: Vec<Span> = vec![Span::styled(left_text, t.status())];
            if let Some(msg) = mid_msg {
                status_spans.push(Span::styled(
                    msg,
                    Style::default().fg(t.secondary).bg(t.surface),
                ));
            }
            // Spacer to push mode to right — approximate with bg fill.
            status_spans.push(Span::styled(" ", Style::default().bg(t.surface)));
            if is_zoomed {
                status_spans.push(Span::styled(
                    " ZOOM ",
                    Style::default()
                        .fg(t.bg)
                        .bg(t.primary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            status_spans.push(Span::styled(
                right_text,
                Style::default()
                    .fg(t.bg)
                    .bg(t.primary)
                    .add_modifier(Modifier::BOLD),
            ));

            frame.render_widget(
                Paragraph::new(Line::from(status_spans)).style(t.status()),
                status_area,
            );
        }

        // Toast deck — rendered before blocking overlays so toasts appear
        // under modal dialogs (which is fine; user can still see them).
        render_toast_deck(frame, &state.toast_deck, &t);

        // Host-terminal cursor positioning.
        // Only one pane (the focused one, live view) owns the cursor.
        // Overlays or scrollback suppress it.
        if let Some(ref pager) = state.pager {
            // Block pager — full-screen, draws over everything, no cursor.
            let pager_full = frame.area();
            render_pager(frame, pager, &t);
            state.pager_rect = Some(pager_full);
        } else {
            state.pager_rect = None;
            if let Some(ref picker) = state.theme_picker {
                render_theme_picker(frame, picker, &t);
            } else if let Some(ref prompt) = state.prompt {
                render_name_prompt(frame, prompt, state.anim.frame(), &t);
            } else if state.search.open {
                // Search overlay — drawn on top of everything else and owns cursor.
                let anim_frame = state.anim.frame();
                render_search_overlay(frame, &mut state.search, anim_frame, &t);
            }
        }

        // Context menu rendered on top of everything (including pager).
        if state.context_menu.is_some() {
            render_context_menu(frame, state, &t);
        }

        // Host-terminal cursor: only for live pane view (no overlay, no scrollback).
        if state.pager.is_none()
            && state.theme_picker.is_none()
            && state.prompt.is_none()
            && !state.search.open
            && state.context_menu.is_none()
            && state.pid_inspect.is_none()
        {
            // No blocking overlay: propagate vt100 cursor from focused pane.
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot_idx = if let Some(zoom_pane) = tab.zoomed {
                focused_slot_idx(zoom_pane, &state.slots)
            } else {
                focused_slot_idx(tab.focus_pane, &state.slots)
            };
            if let Some(slot_idx) = focused_slot_idx {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    if slot.scroll_offset == 0 {
                        let vt_area = slot.last_screen_rect;
                        let cursor_pt = slot.term.grid().cursor.point;
                        let vt_row = cursor_pt.line.0.max(0) as u16;
                        let vt_col = cursor_pt.column.0 as u16;
                        let cursor_x = vt_area
                            .x
                            .saturating_add(vt_col)
                            .min(vt_area.x.saturating_add(vt_area.width).saturating_sub(1));
                        let cursor_y = vt_area
                            .y
                            .saturating_add(vt_row)
                            .min(vt_area.y.saturating_add(vt_area.height).saturating_sub(1));
                        frame.set_cursor_position((cursor_x, cursor_y));
                    }
                }
            }
        }
    })?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Ctrl-B prefix actions
// ─────────────────────────────────────────────────────────────────────────────

/// Cycle focus to the next leaf (DFS order), wrapping around.
/// `slots` is passed so dead/None leaves can be skipped.
fn focus_next(tab: &mut Tab, slots: &[Option<PaneSlot>], forward: bool) {
    // Collect all PaneIds in DFS order, filtering to live slots only.
    let live_panes: Vec<PaneId> = pane_leaves_in_order(&tab.root)
        .into_iter()
        .filter(|&pid| {
            pane_to_slot_idx(slots, pid)
                .and_then(|i| slots.get(i))
                .and_then(|s| s.as_ref())
                .is_some()
        })
        .collect();

    if live_panes.is_empty() {
        return;
    }

    let current_pos = live_panes
        .iter()
        .position(|&p| p == tab.focus_pane)
        .unwrap_or(0);

    let next_pos = if forward {
        (current_pos + 1) % live_panes.len()
    } else {
        (current_pos + live_panes.len() - 1) % live_panes.len()
    };

    tab.focus_pane = live_panes[next_pos];
}

/// Split the active leaf. `horizontal` = true means HSplit (top/bottom).
/// M7-D: delegates to `open_pane_split` RPC; daemon owns layout.
/// Local layout is updated optimistically via `split_focused`; the daemon's
/// `LayoutChanged` event will reconcile on the next broadcast poll.
async fn split_active(state: &mut AppState, horizontal: bool) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();

    // Determine focused pane to split.
    let focused_pane = {
        let sv = state.active_session_view_mut();
        let tab = &mut sv.tabs[sv.active_tab];
        tab.zoomed = None; // clear zoom before splitting
        tab.focus_pane
    };

    // Call open_pane_split — daemon spawns PTY and updates its layout tree.
    let orient = if horizontal {
        Orient::Horizontal
    } else {
        Orient::Vertical
    };
    let req = OpenPaneSplitReq {
        parent_pane: focused_pane,
        orient,
        name: None,
        cwd: std::env::current_dir().ok(),
        cmd: None,
    };
    let new_pane_id = state
        .control
        .open_pane_split(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane_split: {e}"))?;

    // Attach the new pane stream locally.
    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    state.slots.push(Some(slot));

    // Optimistic local layout update: split focused leaf 50/50 in the tab tree.
    let sv = state.active_session_view_mut();
    let tab = &mut sv.tabs[sv.active_tab];
    tab.root.split_focused(&focused_pane, new_pane_id, orient);

    // Move focus to the new pane.
    tab.focus_pane = new_pane_id;

    Ok(())
}

/// Open a new pane in a new tab within the active session.
/// `label` is stored client-side only; pass `None` to auto-number.
async fn open_new_tab(state: &mut AppState, label: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();
    let req = OpenPaneReq {
        session: session_id,
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let new_pane_id = state
        .control
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;

    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    state.slots.push(Some(slot));

    let sv = state.active_session_view_mut();
    let tab_n = sv.tabs.len() + 1;
    let _label = label
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| format!("tab-{tab_n}"));
    sv.tabs.push(Tab {
        root: LayoutNode::Leaf(new_pane_id),
        focus_pane: new_pane_id,
        zoomed: None,
        boundaries: Vec::new(),
        drag: None,
    });
    sv.active_tab = sv.tabs.len() - 1;

    Ok(())
}

/// Spawn a brand-new daemon session and push a SessionView.
async fn open_new_session(state: &mut AppState, name: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let resolved_name = name.filter(|n| !n.is_empty());
    let req = SpawnReq {
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: resolved_name.clone(),
    };
    let SpawnResp { session, pane } = state
        .control
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    let slot = attach_pane(&state.socket, session, pane, cols, rows).await?;
    state.slots.push(Some(slot));

    // Derive display name: use provided or fall back to session-<short8>.
    let short8: String = session.0.to_string().chars().take(8).collect();
    let display_name = resolved_name.unwrap_or_else(|| format!("session-{short8}"));

    state.sessions.push(SessionView {
        id: session,
        name: display_name,
        tabs: vec![Tab {
            root: LayoutNode::Leaf(pane),
            focus_pane: pane,
            zoomed: None,
            boundaries: Vec::new(),
            drag: None,
        }],
        active_tab: 0,
    });
    state.active_session = state.sessions.len() - 1;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Mouse event handler
// ─────────────────────────────────────────────────────────────────────────────

/// Double/triple-click window in milliseconds.
const CLICK_WINDOW_MS: u64 = 500;

/// Walk a word boundary outward from `(row, col)` in the alacritty grid.
/// Returns (start_col, end_col) on the same row, clamped to [0, num_cols).
fn word_bounds(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    row: u16,
    col: u16,
) -> (u16, u16) {
    let num_cols = grid.columns();
    let r = row as i32;

    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';

    // Walk left.
    let mut c0 = col as usize;
    while c0 > 0 {
        let pt = TermPoint::new(TermLine(r), TermColumn(c0 - 1));
        let ch = grid[pt].c;
        if ch == '\0' || is_word_char(ch) {
            c0 -= 1;
        } else {
            break;
        }
    }

    // Walk right.
    let mut c1 = col as usize;
    while c1 + 1 < num_cols {
        let pt = TermPoint::new(TermLine(r), TermColumn(c1 + 1));
        let ch = grid[pt].c;
        if ch == '\0' || is_word_char(ch) {
            c1 += 1;
        } else {
            break;
        }
    }

    (c0 as u16, c1 as u16)
}

/// Apply a delta-percentage resize to two adjacent split children at `[idx]` and
/// `[idx+1]` within `weights`. Each child is clamped to a minimum of `min_pct`
/// (default 5). The pair's total is preserved. Returns the updated weights vec.
fn apply_resize_weights(weights: &[u16], idx: usize, delta_pct: i32, min_pct: u16) -> Vec<u16> {
    let mut out = weights.to_vec();
    if idx + 1 >= out.len() {
        return out;
    }
    let total = out[idx] as i32 + out[idx + 1] as i32;
    let left = (out[idx] as i32 + delta_pct).clamp(min_pct as i32, total - min_pct as i32);
    let right = total - left;
    out[idx] = left as u16;
    out[idx + 1] = right as u16;
    out
}

/// Handle a mouse event. Returns true if the event was consumed.
fn handle_mouse(state: &mut AppState, me: crossterm::event::MouseEvent, body_area: Rect) -> bool {
    let col = me.column;
    let row = me.row;

    // Any click dismisses an open context menu (unless it is on the menu itself).
    // We close it before dispatching the event so the click still lands normally.
    let menu_rect = state.context_menu.as_ref().map(|m| m.rect);
    if let MouseEventKind::Down(_) = me.kind {
        if let Some(mr) = menu_rect {
            if !rect_contains(mr, col, row) {
                state.context_menu = None;
            }
        }
    }

    // Context menu mouse-left: hit-test item_rects written by the last render frame.
    // Must run before the general Left handler so the click is consumed, not passed through.
    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
        if state.context_menu.is_some() {
            let item_rects = state
                .context_menu
                .as_ref()
                .map(|m| m.item_rects.clone())
                .unwrap_or_default();
            for (idx, rect) in item_rects.iter().enumerate() {
                if rect_contains(*rect, col, row) {
                    if let Some(ref mut m) = state.context_menu {
                        m.cursor = idx;
                    }
                    state.pending_menu_action = Some(PendingMenuAction::ContextMenuActivate(idx));
                    return true;
                }
            }
        }
    }

    // Search overlay click — intercept left-down inside the result list.
    if state.search.open {
        if let MouseEventKind::Down(MouseButton::Left) = me.kind {
            let rects = state.search.result_rects.clone();
            for (result_idx, rect) in &rects {
                if rect_contains(*rect, col, row) {
                    state.search.cursor = *result_idx;
                    state.pending_menu_action = Some(PendingMenuAction::SearchJump(*result_idx));
                    return true;
                }
            }
        }
        // Scroll-wheel events pass through to the pane when search is open.
    }

    match me.kind {
        MouseEventKind::ScrollUp => {
            // Mouse-wheel up over the session strip scrolls the strip left.
            if row == 0 {
                state.session_strip_scroll = state.session_strip_scroll.saturating_sub(1);
                return true;
            }
            // Route to pager when it is open and click is inside pager area.
            if let Some(pr) = state.pager_rect {
                if rect_contains(pr, col, row) {
                    if let Some(ref mut pager) = state.pager {
                        pager.scroll_up(3);
                    }
                    return true;
                }
            }
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (pane_id, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                        focus_slot(state, slot_idx);
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset =
                                (slot.scroll_offset + 3).min(slot.scrollback_capacity);
                        }
                    }
                    return true;
                }
            }
            false
        }
        MouseEventKind::ScrollDown => {
            // Mouse-wheel down over the session strip scrolls the strip right.
            if row == 0 {
                state.session_strip_scroll = state.session_strip_scroll.saturating_add(1);
                return true;
            }
            // Route to pager when it is open and click is inside pager area.
            if let Some(pr) = state.pager_rect {
                if rect_contains(pr, col, row) {
                    // We do not have the visible_rows count here; use a generous default
                    // (the pager will clamp). The pager body is the full frame minus 3 rows.
                    let visible = pr.height.saturating_sub(3) as usize;
                    if let Some(ref mut pager) = state.pager {
                        pager.scroll_down(3, visible.max(1));
                    }
                    return true;
                }
            }
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (pane_id, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                        focus_slot(state, slot_idx);
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset = slot.scroll_offset.saturating_sub(3);
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Right-click: open context menu ───────────────────────────────────
        MouseEventKind::Down(MouseButton::Right) => {
            // Dismiss existing menu before potentially re-opening.
            state.context_menu = None;

            // Find the pane under the cursor (body only).
            if row >= 2 {
                let sv = &state.sessions[state.active_session];
                let mut leaf_rects: Vec<(PaneId, Rect)> = Vec::new();
                collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
                for (pane_id, rect) in &leaf_rects {
                    if rect_contains(*rect, col, row) {
                        if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                            focus_slot(state, slot_idx);
                            let max_label = MENU_ITEMS
                                .iter()
                                .map(|i| i.label().len())
                                .max()
                                .unwrap_or(10) as u16;
                            let w = max_label + 4;
                            let h = MENU_ITEMS.len() as u16 + 2;
                            state.context_menu = Some(ContextMenu {
                                rect: Rect::new(col, row, w, h),
                                cursor: 0,
                                target_slot: slot_idx,
                                item_rects: Vec::new(),
                            });
                        }
                        return true;
                    }
                }
            }
            false
        }

        // ── Middle-click: paste clipboard to focused PTY ──────────────────────
        // read_from_clipboard is not yet implemented; arm reserved for M4.
        MouseEventKind::Down(MouseButton::Middle) => false,

        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();

            // ── Row 0: sessions strip ─────────────────────────────────────────
            if row == 0 {
                state.context_menu = None;
                // Check ◄ left-scroll arrow.
                if let Some(left_rect) = state.session_strip_left_arrow {
                    if rect_contains(left_rect, col, row) {
                        state.session_strip_scroll = state.session_strip_scroll.saturating_sub(1);
                        return true;
                    }
                }
                // Check ► right-scroll arrow.
                if let Some(right_rect) = state.session_strip_right_arrow {
                    if rect_contains(right_rect, col, row) {
                        state.session_strip_scroll = state.session_strip_scroll.saturating_add(1);
                        return true;
                    }
                }
                // Check [+] session button first.
                if let Some(plus_rect) = state.session_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewSession,
                            input: String::new(),
                        });
                        return true;
                    }
                }
                // Check session tab rects (cloned to avoid borrow issues).
                let session_rects = state.session_strip_rects.clone();
                for (sess_idx, rect) in &session_rects {
                    if rect_contains(*rect, col, row) {
                        state.active_session = *sess_idx;
                        return true;
                    }
                }
                return false;
            }

            // ── Row 1: tabs strip ─────────────────────────────────────────────
            if row == 1 {
                state.context_menu = None;

                // Check [+] tab button.
                if let Some(plus_rect) = state.tab_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewTab,
                            input: String::new(),
                        });
                        return true;
                    }
                }

                // Hit-test stored tab chip rects (populated by draw_frame).
                let chip_rects = state.tab_chip_rects.clone();
                for (tab_idx, chip_rect) in &chip_rects {
                    if rect_contains(*chip_rect, col, row) {
                        // Check if click lands on the trailing "×" character.
                        let close_col = chip_rect.x + chip_rect.width.saturating_sub(1);
                        if col == close_col {
                            // Kill the tab.
                            let slot_idx = {
                                let sv = &state.sessions[state.active_session];
                                let tab = &sv.tabs[*tab_idx];
                                focused_slot_idx(tab.focus_pane, &state.slots)
                            };
                            if let Some(si) = slot_idx {
                                close_pane_by_slot_idx(state, si);
                                if state.sessions.is_empty() {
                                    // Caller (event loop) will detect empty sessions.
                                    return true;
                                }
                            }
                            return true;
                        }

                        // Record drag start for tab reorder.
                        state.dragging_tab = Some((*tab_idx, col));
                        state.sessions[state.active_session].active_tab = *tab_idx;
                        return true;
                    }
                }
                return false;
            }

            // ── Body: boundary, pane, ribbon chips ────────────────────────────

            // Check if clicking near a split boundary to start a drag.
            // Double-click on boundary resets siblings to equal weights.
            {
                let sv = &mut state.sessions[state.active_session];
                let tab = &mut sv.tabs[sv.active_tab];
                for boundary in tab.boundaries.clone() {
                    let hit = if boundary.is_hsplit {
                        row.abs_diff(boundary.coord) <= 1
                    } else {
                        col.abs_diff(boundary.coord) <= 1
                    };
                    if hit {
                        let click_pos = (col, row);
                        let is_double = state
                            .last_click
                            .as_ref()
                            .map(|lc| {
                                ClickTracker::click_count(
                                    now,
                                    lc.last_at,
                                    lc.last_pos,
                                    click_pos,
                                    lc.count,
                                    CLICK_WINDOW_MS,
                                ) >= 2
                            })
                            .unwrap_or(false);

                        if is_double {
                            // Reset sibling weights to equal.
                            if let Some(children) =
                                children_at_mut(&mut tab.root, &boundary.parent_path)
                            {
                                let n = children.len() as u16;
                                if let Some(each) = 100u16.checked_div(n) {
                                    let rem = 100 - each * n;
                                    for (i, (_, w)) in children.iter_mut().enumerate() {
                                        *w = each + if i == 0 { rem } else { 0 };
                                    }
                                }
                            }
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: 2,
                                pane_idx: usize::MAX,
                            });
                            return true;
                        }

                        let start_coord = if boundary.is_hsplit { row } else { col };
                        let start_weights: Vec<u16> = if let Some(children) =
                            children_at_mut(&mut tab.root, &boundary.parent_path)
                        {
                            children.iter().map(|(_, w)| *w).collect()
                        } else {
                            continue;
                        };
                        tab.drag = Some(DragState {
                            boundary,
                            start_coord,
                            start_weights,
                        });
                        state.last_click = Some(ClickTracker {
                            last_at: now,
                            last_pos: click_pos,
                            count: 1,
                            pane_idx: usize::MAX,
                        });
                        return true;
                    }
                }
            }

            // Check if clicking inside a sidebar row.
            if state.sidebar_open {
                // The sidebar occupies the left 24 columns of body area.
                let sidebar_width: u16 = 24;
                let sidebar_rect =
                    Rect::new(body_area.x, body_area.y, sidebar_width, body_area.height);
                if rect_contains(sidebar_rect, col, row) {
                    // Each data row is 1 line tall; body starts 1 line after top border.
                    let inner_y = sidebar_rect.y.saturating_add(1);
                    let row_idx = row.saturating_sub(inner_y) as usize;
                    if row_idx < state.sidebar_data.len() {
                        state.sidebar_cursor = row_idx;
                        state.sidebar_focused = true;
                        // Focus the pane if it is loaded in the active tab.
                        let target_pane_id = state.sidebar_data[row_idx].id;
                        let pane_in_tab = {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            pane_leaves_in_order(&tab.root).contains(&target_pane_id)
                        };
                        if pane_in_tab {
                            let sv = &mut state.sessions[state.active_session];
                            let tab = &mut sv.tabs[sv.active_tab];
                            tab.focus_pane = target_pane_id;
                        }
                    }
                    return true;
                }
            }

            // Check if clicking inside a leaf pane (ribbon chips + text selection).
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            let leaf_rects_with_slots: Vec<(usize, Rect)> = leaf_rects
                .iter()
                .filter_map(|(pid, r)| pane_to_slot_idx(&state.slots, *pid).map(|i| (i, *r)))
                .collect();
            for (slot_idx, rect) in leaf_rects_with_slots {
                if rect_contains(rect, col, row) {
                    focus_slot(state, slot_idx);

                    // ── Ribbon chip click ──────────────────────────────────────
                    let chip_rects: Vec<(usize, Rect)> = state.slots[slot_idx]
                        .as_ref()
                        .map(|s| s.ribbon_chip_rects.clone())
                        .unwrap_or_default();
                    for (chip_idx, chip_rect) in &chip_rects {
                        if rect_contains(*chip_rect, col, row) {
                            let click_pos = (col, row);
                            let click_count = state
                                .last_click
                                .as_ref()
                                .map(|lc| {
                                    ClickTracker::click_count(
                                        now,
                                        lc.last_at,
                                        lc.last_pos,
                                        click_pos,
                                        lc.count,
                                        CLICK_WINDOW_MS,
                                    )
                                })
                                .unwrap_or(1);
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: click_count,
                                pane_idx: slot_idx,
                            });

                            if let Some(slot) = state.slots[slot_idx].as_mut() {
                                slot.ribbon_cursor = Some(*chip_idx);
                                if click_count >= 2 {
                                    // Double-click: open pager for this block.
                                    // Queue via pending action so the async path in the
                                    // event loop (Enter key handler) can handle it.
                                    // We mark ribbon_cursor and let the loop open pager.
                                    // The pager open logic is duplicated minimally here
                                    // via a sentinel: set ribbon_cursor and signal.
                                    // Actual pager open requires an RPC so we use pending.
                                    state.pending_menu_action =
                                        Some(PendingMenuAction::ContextMenuCommit);
                                    // Re-use ContextMenuCommit as "open pager for cursor" —
                                    // the event-loop drain sees context_menu=None and instead
                                    // checks if ribbon_cursor is Some → opens pager.
                                    // This avoids a new enum variant.
                                }
                            }
                            return true;
                        }
                    }

                    // ── Multi-click text selection ─────────────────────────────
                    if let Some(slot) = state.slots[slot_idx].as_ref() {
                        let content = slot.last_screen_rect;
                        if rect_contains(content, col, row) {
                            let sel_row = row.saturating_sub(content.y);
                            let sel_col = col.saturating_sub(content.x);
                            let click_pos = (col, row);

                            let click_count = state
                                .last_click
                                .as_ref()
                                .map(|lc| {
                                    ClickTracker::click_count(
                                        now,
                                        lc.last_at,
                                        lc.last_pos,
                                        click_pos,
                                        lc.count,
                                        CLICK_WINDOW_MS,
                                    )
                                })
                                .unwrap_or(1);
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: click_count,
                                pane_idx: slot_idx,
                            });

                            let sel_base = if slot.scroll_offset > 0 {
                                SelectionBase::Scrollback(slot.scroll_offset)
                            } else {
                                SelectionBase::Live
                            };

                            let (start, end) = if click_count >= 3 {
                                // Triple-click: select full row.
                                let grid = slot.term.grid();
                                let last_col = (grid.columns().saturating_sub(1)) as u16;
                                ((sel_row, 0u16), (sel_row, last_col))
                            } else if click_count == 2 {
                                // Double-click: select word.
                                let grid = slot.term.grid();
                                let (wc0, wc1) = word_bounds(grid, sel_row, sel_col);
                                ((sel_row, wc0), (sel_row, wc1))
                            } else {
                                ((sel_row, sel_col), (sel_row, sel_col))
                            };

                            state.selection = Some(Selection {
                                pane_idx: slot_idx,
                                start,
                                end,
                                dragging: click_count == 1, // only drag on single click
                                base: sel_base,
                            });
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Tab drag reorder ──────────────────────────────────────────────────
        MouseEventKind::Drag(MouseButton::Left) if row == 1 => {
            if let Some((from_idx, start_col)) = state.dragging_tab {
                // Determine which tab chip the cursor is currently over.
                let chip_rects = state.tab_chip_rects.clone();
                for (over_idx, chip_rect) in &chip_rects {
                    if rect_contains(*chip_rect, col, row) && *over_idx != from_idx {
                        // Swap when cursor crosses the midpoint of the target chip.
                        let mid = chip_rect.x + chip_rect.width / 2;
                        let dragging_right = col > start_col;
                        let cross = if dragging_right {
                            col >= mid
                        } else {
                            col <= mid
                        };
                        if cross {
                            let sv = &mut state.sessions[state.active_session];
                            let tabs = std::mem::take(&mut sv.tabs);
                            sv.tabs = tab_reorder(tabs, from_idx, *over_idx);
                            // Keep active tab pointing at the dragged chip.
                            sv.active_tab = *over_idx;
                            state.dragging_tab = Some((*over_idx, col));
                        }
                        return true;
                    }
                }
            }
            false
        }

        MouseEventKind::Drag(MouseButton::Left) => {
            // Clear tab drag if cursor leaves row 1.
            if row != 1 {
                state.dragging_tab = None;
            }
            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            // Split-resize drag takes priority.
            if let Some(ref drag) = tab.drag {
                let cur_coord = if drag.boundary.is_hsplit { row } else { col };
                let delta = cur_coord as i32 - drag.start_coord as i32;
                let parent_size = drag.boundary.parent_size.max(1) as i32;
                let delta_pct = (delta * 100) / parent_size;
                let idx = drag.boundary.child_idx;
                let new_weights = apply_resize_weights(&drag.start_weights, idx, delta_pct, 5);
                let parent_path = drag.boundary.parent_path.clone();
                if let Some(children) = children_at_mut(&mut tab.root, &parent_path) {
                    for (i, w) in new_weights.iter().enumerate() {
                        if i < children.len() {
                            children[i].1 = *w;
                        }
                    }
                }
                return true;
            }
            // Text selection drag: update end point if selection is active.
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    if let Some(slot) = state.slots[sel.pane_idx].as_ref() {
                        let content = slot.last_screen_rect;
                        // Extend selection — auto-scroll at body edges.
                        let new_row = if row < content.y {
                            // Dragged above viewport: scroll up.
                            if let Some(s) = state.slots[sel.pane_idx].as_mut() {
                                s.scroll_offset = (s.scroll_offset + 1).min(s.scrollback_capacity);
                            }
                            0u16
                        } else if row >= content.y + content.height {
                            // Dragged below viewport: scroll down.
                            if let Some(s) = state.slots[sel.pane_idx].as_mut() {
                                s.scroll_offset = s.scroll_offset.saturating_sub(1);
                            }
                            content.height.saturating_sub(1)
                        } else {
                            row.saturating_sub(content.y)
                        };
                        let new_col = col
                            .saturating_sub(content.x)
                            .min(content.width.saturating_sub(1));
                        sel.end = (new_row, new_col);
                        return true;
                    }
                }
            }
            false
        }

        MouseEventKind::Up(MouseButton::Left) => {
            // End tab drag.
            state.dragging_tab = None;

            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            if tab.drag.is_some() {
                tab.drag = None;
                return true;
            }
            // Finish text selection: copy to clipboard.
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    sel.dragging = false;
                    // Extract selected text from the alacritty grid via per-cell iteration.
                    let pane_idx = sel.pane_idx;
                    let ((r0, c0), (r1, c1)) = sel.normalized();
                    let scroll_offset = match &sel.base {
                        SelectionBase::Scrollback(off) => *off,
                        SelectionBase::Live => 0,
                    };
                    if let Some(slot) = state.slots[pane_idx].as_ref() {
                        let grid = slot.term.grid();
                        let num_cols = grid.columns();
                        let mut text = String::new();
                        for grid_row in r0..=r1 {
                            if grid_row > r0 {
                                text.push('\n');
                            }
                            // Offset into scrollback: negate because alacritty lines
                            // above the viewport are negative line indices.
                            let line_idx = grid_row as i32 - scroll_offset as i32;
                            let col_start = if grid_row == r0 { c0 as usize } else { 0usize };
                            let col_end = if grid_row == r1 {
                                c1 as usize
                            } else {
                                num_cols.saturating_sub(1)
                            };
                            for c in col_start..=col_end {
                                let pt = TermPoint::new(TermLine(line_idx), TermColumn(c));
                                let ch = grid[pt].c;
                                if ch == '\0' {
                                    text.push(' ');
                                } else {
                                    text.push(ch);
                                }
                            }
                        }
                        // Trim trailing whitespace from each line.
                        let trimmed: String = text
                            .lines()
                            .map(|l| l.trim_end())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !trimmed.is_empty() {
                            if let Err(e) = crate::clipboard::copy_to_clipboard(&trimmed) {
                                tracing::warn!("clipboard copy failed: {e}");
                            }
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Hover: boundary "drag to resize" status hint ──────────────────────
        MouseEventKind::Moved => {
            // Only show hint when no drag or selection is active (cheap path).
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            if tab.drag.is_some()
                || state
                    .selection
                    .as_ref()
                    .map(|s| s.dragging)
                    .unwrap_or(false)
            {
                return false;
            }
            let mut on_boundary = false;
            for boundary in &tab.boundaries {
                let hit = if boundary.is_hsplit {
                    row.abs_diff(boundary.coord) <= 1
                } else {
                    col.abs_diff(boundary.coord) <= 1
                };
                if hit {
                    on_boundary = true;
                    break;
                }
            }
            if on_boundary {
                state.status_msg = Some("drag to resize".to_owned());
            } else if state.status_msg.as_deref() == Some("drag to resize") {
                state.status_msg = None;
            }
            false // do not trigger a full redraw just for hover hint
        }

        _ => false,
    }
}

/// Poll the daemon for a pending `pyrec select-pane` focus request and apply it.
///
/// Replaces the former dropfile (`focus.request`) approach with an RPC call so
/// that concurrent `pyrec select-pane` invocations are handled atomically.
async fn apply_focus_request(state: &mut AppState) {
    let Ok(Ok(Some(pane_str))) = state
        .control
        .take_focus_request(tarpc::context::current())
        .await
    else {
        return;
    };
    let Ok(pane_uuid) = uuid::Uuid::parse_str(&pane_str) else {
        return;
    };
    let pane_id = PaneId(pane_uuid);
    if let Some(slot_idx) = state
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|slot| slot.pane_id == pane_id))
    {
        focus_slot(state, slot_idx);
        let short: String = pane_str.chars().take(8).collect();
        state.status_msg = Some(format!("focused pane {short} (select-pane)"));
    } else {
        state.status_msg = Some(format!(
            "select-pane: pane {pane_str} not open in this TUI — attach it first"
        ));
    }
}

/// Update active session's active tab focus_pane to point at the given slot index.
fn focus_slot(state: &mut AppState, target_slot_idx: usize) {
    // Find the PaneId for this slot and check it exists in the active tab.
    let target_pane_id = match state.slots.get(target_slot_idx).and_then(|s| s.as_ref()) {
        Some(slot) => slot.pane_id,
        None => return,
    };
    let sv = &state.sessions[state.active_session];
    let tab = &sv.tabs[sv.active_tab];
    if pane_leaves_in_order(&tab.root).contains(&target_pane_id) {
        let sv = &mut state.sessions[state.active_session];
        sv.tabs[sv.active_tab].focus_pane = target_pane_id;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane close / layout collapse
// ─────────────────────────────────────────────────────────────────────────────

/// Locate the (session_idx, tab_idx) for a given PaneId.
fn locate_pane(state: &AppState, target_pane: PaneId) -> Option<(usize, usize)> {
    for (si, sess) in state.sessions.iter().enumerate() {
        for (ti, tab) in sess.tabs.iter().enumerate() {
            if pane_leaves_in_order(&tab.root).contains(&target_pane) {
                return Some((si, ti));
            }
        }
    }
    None
}

/// Close a pane by its slot index.
/// Removes the leaf from the layout tree, drops the slot, cascades tab/session removal.
/// Uses `LayoutNode::close` from pyre-proto which handles collapse logic.
fn close_pane_by_slot_idx(state: &mut AppState, slot_idx: usize) {
    // Resolve pane_id from the slot.
    let pane_id = match state.slots.get(slot_idx).and_then(|s| s.as_ref()) {
        Some(slot) => slot.pane_id,
        None => return,
    };

    let (sess_idx, tab_idx) = match locate_pane(state, pane_id) {
        Some(loc) => loc,
        None => return,
    };

    // Fire close_pane RPC fire-and-forget so the daemon evicts the pane.
    {
        let client = state.control.clone();
        tokio::runtime::Handle::current().spawn(async move {
            let _ = client.close_pane(tarpc::context::current(), pane_id).await;
        });
    }

    // Use proto's LayoutNode::close which handles collapse of single-child splits.
    let new_focus_pane = state.sessions[sess_idx].tabs[tab_idx].root.close(&pane_id);

    // Drop the slot.
    if slot_idx < state.slots.len() {
        state.slots[slot_idx] = None;
    }

    let remaining = pane_leaves_in_order(&state.sessions[sess_idx].tabs[tab_idx].root);

    if remaining.is_empty() {
        // Tab is empty — remove it.
        state.sessions[sess_idx].tabs.remove(tab_idx);
        if state.sessions[sess_idx].tabs.is_empty() {
            // Session has no tabs — remove session view.
            state.sessions.remove(sess_idx);
            if state.sessions.is_empty() {
                return;
            }
            state.active_session = state.active_session.min(state.sessions.len() - 1);
        } else {
            state.sessions[sess_idx].active_tab =
                tab_idx.min(state.sessions[sess_idx].tabs.len() - 1);
            // Reset focus to first leaf of new active tab.
            let new_tab_idx = state.sessions[sess_idx].active_tab;
            if let Some(&first_pane) =
                pane_leaves_in_order(&state.sessions[sess_idx].tabs[new_tab_idx].root).first()
            {
                state.sessions[sess_idx].tabs[new_tab_idx].focus_pane = first_pane;
            }
        }
    } else {
        // Tab still has leaves — move focus to the suggested pane or first remaining.
        let focus = new_focus_pane
            .filter(|p| remaining.contains(p))
            .or_else(|| remaining.into_iter().next());
        if let Some(fp) = focus {
            state.sessions[sess_idx].tabs[tab_idx].focus_pane = fp;
        }
        state.sessions[sess_idx].tabs[tab_idx].zoomed = None;
    }
}

/// Close the focused pane in the active tab.
fn close_focused_pane(state: &mut AppState) {
    let sess_idx = state.active_session;
    let tab_idx = state.sessions[sess_idx].active_tab;
    let focus_pane = state.sessions[sess_idx].tabs[tab_idx].focus_pane;
    if let Some(slot_idx) = focused_slot_idx(focus_pane, &state.slots) {
        close_pane_by_slot_idx(state, slot_idx);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main TUI loop
// ─────────────────────────────────────────────────────────────────────────────

/// Build an `AppState` from one already-attached initial session/pane.
#[allow(clippy::too_many_arguments)]
fn initial_app_state(
    session: SessionId,
    session_name: String,
    initial_slot: PaneSlot,
    control: PyreDaemonClient,
    socket: PathBuf,
    shell: Option<String>,
    blocks_rx: watch::Receiver<HashMap<PaneId, Vec<Block>>>,
    theme: Theme,
    toast_deck: ToastDeck,
    toast_rx: mpsc::Receiver<Toast>,
) -> AppState {
    let initial_pane_id = initial_slot.pane_id;
    AppState {
        sessions: vec![SessionView {
            id: session,
            name: session_name,
            tabs: vec![Tab {
                root: LayoutNode::Leaf(initial_pane_id),
                focus_pane: initial_pane_id,
                zoomed: None,
                boundaries: Vec::new(),
                drag: None,
            }],
            active_tab: 0,
        }],
        active_session: 0,
        slots: vec![Some(initial_slot)],
        control,
        socket,
        shell,
        search: SearchState::default(),
        status_msg: None,
        sidebar_open: false,
        sidebar_data: Vec::new(),
        sidebar_last_poll: Instant::now() - Duration::from_secs(10),
        sidebar_cursor: 0,
        sidebar_focused: false,
        selection: None,
        last_click: None,
        context_menu: None,
        pid_inspect: None,
        prompt: None,
        session_strip_rects: Vec::new(),
        session_strip_scroll: 0,
        session_strip_left_arrow: None,
        session_strip_right_arrow: None,
        session_plus_rect: None,
        tab_plus_rect: None,
        pending_resizes: Vec::new(),
        tab_chip_rects: Vec::new(),
        dragging_tab: None,
        pager_rect: None,
        // Force an immediate session-list sync on the first loop iteration.
        session_list_last_poll: Instant::now() - Duration::from_secs(10),
        // Force an immediate layout resync on the first loop iteration.
        layout_resync_last_poll: Instant::now() - Duration::from_secs(10),
        blocks_rx,
        anim: AnimClock::new(),
        pager: None,
        theme,
        theme_picker: None,
        toast_deck,
        toast_rx,
        pending_menu_action: None,
        session_lost: false,
    }
}

async fn run_tui(
    socket: PathBuf,
    init: PaneInit,
    control: PyreDaemonClient,
    shell: Option<String>,
) -> Result<()> {
    // Enter alternate screen FIRST so that crossterm::terminal::size() returns
    // the full terminal dimensions when we compute the initial pane size below.
    // Any size query before this point reflects the pre-alt-screen scroll-region
    // height (typically 25 rows on most terminals), not the real window height.
    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // Now that we are in alternate-screen mode, ratatui's terminal.size() gives
    // the true frame dimensions. Use those to compute the initial inner pane area
    // so the PTY is spawned at the right size from the very first frame.
    let term_rect = terminal.size()?;
    let (init_cols, init_rows) = compute_pane_inner_size(term_rect.width, term_rect.height);

    // Resolve session/pane — spawning here (post-alt-screen) ensures the shell
    // is started at real terminal dimensions, not an 80×24 placeholder.
    let (session, session_name, pane) = match init {
        PaneInit::Existing {
            session,
            session_name,
            pane,
        } => (session, session_name, pane),
        PaneInit::Spawn => {
            let req = SpawnReq {
                shell: shell.clone(),
                cwd: std::env::current_dir().ok(),
                cols: init_cols,
                rows: init_rows,
                env: std::env::vars().collect(),
                name: None,
            };
            let SpawnResp { session, pane } = control
                .spawn(tarpc::context::current(), req)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon spawn: {e}"))?;
            let short8: String = session.0.to_string().chars().take(8).collect();
            (session, format!("session-{short8}"), pane)
        }
    };

    let mut initial_slot = attach_pane(&socket, session, pane, init_cols, init_rows).await?;

    // Pre-populate the block ribbon for the initial pane so that Ctrl-B [
    // shows previous command history immediately on reattach (S3).
    match tokio::time::timeout(
        Duration::from_secs(2),
        control.replay(tarpc::context::current(), pane, 20),
    )
    .await
    {
        Ok(Ok(Ok(replay))) => {
            if !replay.recent.is_empty() {
                tracing::debug!(
                    pane_id = %pane.0,
                    blocks = replay.recent.len(),
                    "reattach: pre-populated block ribbon"
                );
                initial_slot.recent_blocks = replay.recent;
            }
        }
        Ok(Ok(Err(e))) => tracing::debug!(pane_id = %pane.0, "replay rpc error (non-fatal): {e}"),
        Ok(Err(_)) => tracing::debug!(pane_id = %pane.0, "replay transport error (non-fatal)"),
        Err(_) => tracing::debug!(pane_id = %pane.0, "replay rpc timeout (non-fatal)"),
    }

    // ── Background block-poll task (Bug 2 fix) ──────────────────────────────
    // list_blocks is moved off the hot event loop into its own task. A
    // watch channel carries the latest snapshot; the event loop does a
    // non-blocking borrow_and_update read and never awaits the RPC directly.
    let (blocks_tx, blocks_rx) = watch::channel(HashMap::<PaneId, Vec<Block>>::new());
    {
        let poll_client = control.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let req = pyre_proto::blocks::ListBlocksReq {
                    session: None,
                    limit: 20,
                };
                // Apply a 2 s hard timeout so a stuck daemon cannot pin this task.
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    poll_client.list_blocks(tarpc::context::current(), req),
                )
                .await;
                if let Ok(Ok(Ok(blocks))) = result {
                    // Group by pane_id so the event loop can index directly.
                    let mut map: HashMap<PaneId, Vec<Block>> = HashMap::new();
                    for b in blocks {
                        map.entry(b.pane).or_default().push(b);
                    }
                    // send only fails when all receivers are dropped (TUI exited).
                    if blocks_tx.send(map).is_err() {
                        break;
                    }
                }
            }
        });
    }

    // Load theme from config (non-fatal — fall back to default on any error).
    let theme = {
        let reg = Registry::builtin();
        let name = pyre_themes::config::load_theme_name()
            .unwrap_or(None)
            .unwrap_or_else(|| Registry::default_theme().to_owned());
        reg.get(&name)
            .or_else(|| reg.get(Registry::default_theme()))
            .expect("ember always present")
            .clone()
    };

    // Load notification config (non-fatal — defaults on error).
    let notif_cfg = pyre_themes::config::load_notifications_config().unwrap_or_default();

    // ── Background push-event task ─────────────────────────────────────────
    // Long-polls `next_pane_event` and sends resulting Toasts into `toast_rx`.
    // The event loop drains the channel each tick without awaiting the RPC.
    let (toast_tx, toast_rx) = mpsc::channel::<Toast>(64);
    {
        let push_client_socket = socket.clone();
        let ttl = Duration::from_millis(notif_cfg.ttl_ms);
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            let mut backoff = Duration::from_millis(200);
            loop {
                let client = match try_connect_control(&push_client_socket).await {
                    Ok(c) => c,
                    Err(_) => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                        continue;
                    }
                };
                backoff = Duration::from_millis(200);

                match client
                    .next_pane_event(tarpc::context::current(), seq, 30_000)
                    .await
                {
                    Ok(Ok(events)) if !events.is_empty() => {
                        if let Some(last) = events.last() {
                            seq = last.seq;
                        }
                        for ev in &events {
                            if let Some(toast) = pane_event_to_toast(ev, ttl) {
                                // Silently drop if receiver is gone (TUI exiting).
                                let _ = toast_tx.try_send(toast);
                            }
                        }
                    }
                    Ok(Ok(_)) => {
                        // Normal long-poll timeout; loop immediately.
                    }
                    _ => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                    }
                }
            }
        });
    }

    let toast_deck = ToastDeck::new(notif_cfg.enabled, notif_cfg.ttl_ms, notif_cfg.max_visible);

    let mut state = initial_app_state(
        session,
        session_name,
        initial_slot,
        control,
        socket,
        shell,
        blocks_rx,
        theme,
        toast_deck,
        toast_rx,
    );

    // Eagerly discover all other sessions the daemon already knows about so
    // the top bar is populated before the first draw, not 1 s later.
    // Re-read terminal size here — we are already in alt-screen so the value is authoritative.
    if let Ok(Ok(daemon_sessions)) = state.control.list_sessions(tarpc::context::current()).await {
        let eager_rect = terminal.size().unwrap_or(term_rect);
        let (ec, er) = compute_pane_inner_size(eager_rect.width, eager_rect.height);
        for info in daemon_sessions {
            if info.id == session {
                continue; // already the active session
            }
            if let Ok(Ok(panes)) = state
                .control
                .list_panes(tarpc::context::current(), info.id)
                .await
            {
                if let Some(p) = panes.into_iter().next() {
                    if let Ok(mut slot) = attach_pane(&state.socket, info.id, p.id, ec, er).await {
                        // Pre-populate block ribbon for eagerly-attached panes.
                        if let Ok(Ok(Ok(replay))) = tokio::time::timeout(
                            Duration::from_secs(2),
                            state.control.replay(tarpc::context::current(), p.id, 20),
                        )
                        .await
                        {
                            if !replay.recent.is_empty() {
                                slot.recent_blocks = replay.recent;
                            }
                        }
                        let eager_pane_id = p.id;
                        state.slots.push(Some(slot));
                        state.sessions.push(SessionView {
                            id: info.id,
                            name: info.name,
                            tabs: vec![Tab {
                                root: LayoutNode::Leaf(eager_pane_id),
                                focus_pane: eager_pane_id,
                                zoomed: None,
                                boundaries: Vec::new(),
                                drag: None,
                            }],
                            active_tab: 0,
                        });
                    }
                }
            }
        }
        // Mark poll time so the in-loop poll won't fire again for 1 s.
        state.session_list_last_poll = Instant::now();
    }

    let mut prefix_active = false;

    // Observability counters for the 1 s periodic debug log.
    let mut loop_frames_drawn: u64 = 0;
    let mut loop_bytes_processed: u64 = 0;
    let mut loop_stats_at = Instant::now();

    loop {
        // Drain all pane output into their parsers and scrollback buffers.
        // Collect (slot_idx, frames_received) for Closed events so we can
        // decide whether to fire close_pane RPC after the borrow ends.
        let mut closed_slots: Vec<(usize, u64)> = Vec::new();
        for (slot_idx, slot_opt) in state.slots.iter_mut().enumerate() {
            if let Some(slot) = slot_opt {
                while let Ok(event) = slot.output_rx.try_recv() {
                    match event {
                        PaneEvent::Output(data) => {
                            slot.frames_received += 1;
                            loop_bytes_processed += data.len() as u64;
                            slot.process_output(&data);
                            // Forward any CPR/DSR responses generated by the
                            // terminal emulator (e.g. ?1000h, cursor position
                            // reports) back to the child PTY so nested TUIs
                            // don't hang waiting for a reply.
                            let responses = slot.drain_pty_responses();
                            if !responses.is_empty() {
                                let _ = slot.input_tx.try_send(Bytes::from(responses));
                            }
                        }
                        PaneEvent::Closed { frames_received } => {
                            closed_slots.push((slot_idx, frames_received));
                            // Stop draining this pane; it will be removed below.
                            break;
                        }
                    }
                }
            }
        }
        for (slot_idx, frames_received) in closed_slots {
            if frames_received == 0 {
                // The stream was rejected before any output arrived — the
                // worker did not have this pane (e.g. "pane not found").
                // Firing close_pane here would tell the worker to close a
                // slot it never opened, causing it to exit and triggering a
                // supervisor respawn loop.  Just remove the TUI slot; the
                // session-sync loop will reconcile daemon state on the next tick.
                tracing::warn!(
                    slot_idx,
                    "stream closed with 0 frames; skipping close_pane RPC"
                );
                if slot_idx < state.slots.len() {
                    state.slots[slot_idx] = None;
                }
            } else {
                close_pane_by_slot_idx(&mut state, slot_idx);
            }
        }

        // Drain latest block snapshot from the background poll task (non-blocking).
        // borrow_and_update marks the value as seen; subsequent calls return
        // Err(RecvError) until the background task publishes a new snapshot.
        if state.blocks_rx.has_changed().unwrap_or(false) {
            let map = state.blocks_rx.borrow_and_update().clone();
            for slot in state.slots.iter_mut().flatten() {
                let pane_id = slot.pane_id;
                if let Some(blocks) = map.get(&pane_id) {
                    slot.recent_blocks = blocks.clone();
                    if let Some(cursor) = slot.ribbon_cursor {
                        if !slot.recent_blocks.is_empty() {
                            slot.ribbon_cursor = Some(cursor.min(slot.recent_blocks.len() - 1));
                        } else {
                            slot.ribbon_cursor = None;
                        }
                    }
                } else {
                    // Pane not present in latest snapshot — clear stale data.
                    slot.recent_blocks.clear();
                    slot.ribbon_cursor = None;
                }
            }
        }

        // Drain toasts from the background push-event task (non-blocking).
        // tick() first to drop expired entries, then absorb any new arrivals.
        state.toast_deck.tick();
        while let Ok(toast) = state.toast_rx.try_recv() {
            // push() respects the enabled flag and trims to max_visible.
            state.toast_deck.push(toast.title, toast.body, toast.kind);
        }

        // Search debounce: fire query 150 ms after last keystroke.
        if state.search.open
            && state.search.pending_query.is_some()
            && state.search.last_query_at.elapsed() >= Duration::from_millis(150)
        {
            let raw = state.search.pending_query.take().expect("checked Some");
            let (query, failures_only) = parse_search_input(&raw);
            state.search.failures_only = failures_only;
            let (tx, rx) = mpsc::channel::<Vec<BlockHit>>(1);
            state.search.rx = Some(rx);
            let client = state.control.clone();
            let req = SearchBlocksReq {
                query,
                limit: 20,
                failures_only,
            };
            tokio::spawn(async move {
                let hits = client
                    .search_blocks(tarpc::context::current(), req)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let _ = tx.send(hits).await;
            });
        }

        // Drain search results channel.
        if let Some(ref mut rx) = state.search.rx {
            if let Ok(hits) = rx.try_recv() {
                state.search.results = hits;
                state.search.cursor = 0;
            }
        }

        apply_focus_request(&mut state).await;

        // Pane-meta poll — 1s, unconditional.  Feeds both the sidebar display
        // and pane border titles (via `render_pane`).  Running regardless of
        // sidebar visibility ensures names are available even when the sidebar
        // is closed.
        if state.sidebar_last_poll.elapsed() >= Duration::from_secs(1) {
            state.sidebar_last_poll = Instant::now();
            if let Ok(Ok(mut panes)) = state
                .control
                .list_all_panes(tarpc::context::current())
                .await
            {
                panes.truncate(50);
                panes.sort_by(|a, b| {
                    session_name_for(&state, a.session)
                        .cmp(&session_name_for(&state, b.session))
                        .then_with(|| a.id.0.cmp(&b.id.0))
                });
                state.sidebar_data = panes;
                state.sidebar_cursor = state
                    .sidebar_cursor
                    .min(state.sidebar_data.len().saturating_sub(1));
            }
        }

        // Session-list sync — 1s poll to discover sessions created by other clients
        // (e.g. pyre_mcp::session_spawn) or to prune sessions removed elsewhere.
        if state.session_list_last_poll.elapsed() >= Duration::from_secs(1) {
            state.session_list_last_poll = Instant::now();
            if let Ok(Ok(daemon_sessions)) =
                state.control.list_sessions(tarpc::context::current()).await
            {
                // Preserve the active session across all list mutations below.
                // Insertions and removals shift indices; we restore by ID
                // after every mutation batch so the user's view never jumps.
                let prev_active_id = state.sessions.get(state.active_session).map(|sv| sv.id);
                // Add sessions that appeared in the daemon but are unknown to TUI.
                let known_ids: Vec<SessionId> = state.sessions.iter().map(|s| s.id).collect();
                for info in &daemon_sessions {
                    if !known_ids.contains(&info.id) {
                        // Attach to the first pane of the new session (if any).
                        match state
                            .control
                            .list_panes(tarpc::context::current(), info.id)
                            .await
                        {
                            Ok(Ok(panes)) if !panes.is_empty() => {
                                let pane_id = panes[0].id;
                                let (sc, sr) = {
                                    let (tc, tr) = term_size();
                                    compute_pane_inner_size(tc, tr)
                                };
                                match attach_pane(&state.socket, info.id, pane_id, sc, sr).await {
                                    Ok(slot) => {
                                        state.slots.push(Some(slot));
                                        state.sessions.push(SessionView {
                                            id: info.id,
                                            name: info.name.clone(),
                                            tabs: vec![Tab {
                                                root: LayoutNode::Leaf(pane_id),
                                                focus_pane: pane_id,
                                                zoomed: None,
                                                boundaries: Vec::new(),
                                                drag: None,
                                            }],
                                            active_tab: 0,
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "session-sync: attach_pane for session {} failed: {e}",
                                            info.id
                                        );
                                    }
                                }
                            }
                            _ => {
                                // Session has no panes yet; skip until it does.
                            }
                        }
                    }
                }

                // Sync panes within sessions that already exist in both TUI and
                // daemon — handles panes added by external clients (e.g. MCP
                // pane_open) after the session was first attached.
                for info in &daemon_sessions {
                    // Only sessions the TUI already knows about.
                    let sv_idx = match state.sessions.iter().position(|s| s.id == info.id) {
                        Some(i) => i,
                        None => continue,
                    };

                    // Collect all pane IDs currently tracked in this SessionView.
                    let local_pane_ids: Vec<PaneId> = {
                        let sv = &state.sessions[sv_idx];
                        sv.tabs
                            .iter()
                            .flat_map(|tab| pane_leaves_in_order(&tab.root))
                            .collect()
                    };

                    // Ask daemon which panes belong to this session.
                    let daemon_panes = match state
                        .control
                        .list_panes(tarpc::context::current(), info.id)
                        .await
                    {
                        Ok(Ok(p)) => p,
                        _ => continue,
                    };

                    // Attach panes the daemon knows about but TUI does not.
                    // Determine whether any new panes need to be attached.
                    let new_panes: Vec<_> = daemon_panes
                        .iter()
                        .filter(|p| !local_pane_ids.contains(&p.id))
                        .collect();

                    if !new_panes.is_empty() {
                        // Fetch the authoritative layout from the daemon FIRST.
                        // The daemon is the single source of truth for the split
                        // tree; creating a new Tab here would be wrong when the
                        // pane was added via open_pane_split (it belongs inside
                        // the existing tab's split, not as a separate tab).
                        let fresh_layout = state
                            .control
                            .get_session_layout(tarpc::context::current(), info.id)
                            .await
                            .ok()
                            .and_then(|r| r.ok());

                        // Attach a slot for each new pane (I/O streams).
                        for pane_info in &new_panes {
                            let (pc, pr) = {
                                let (tc, tr) = term_size();
                                compute_pane_inner_size(tc, tr)
                            };
                            match attach_pane(&state.socket, info.id, pane_info.id, pc, pr).await {
                                Ok(slot) => {
                                    state.slots.push(Some(slot));
                                    tracing::info!(
                                        "pane-sync: attached slot for pane {} in session {}",
                                        pane_info.id,
                                        info.id,
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "pane-sync: attach_pane for pane {} in session {} \
                                         failed: {e}",
                                        pane_info.id,
                                        info.id,
                                    );
                                }
                            }
                        }

                        // Reconcile the layout tree: if the daemon returned a
                        // fresh layout, apply it to the active tab so splits are
                        // reflected correctly. If get_session_layout failed, fall
                        // back to inserting a new tab per new pane (old behaviour)
                        // so we don't silently lose panes.
                        let sv = &mut state.sessions[sv_idx];
                        if let Some(layout) = fresh_layout {
                            // Replace the active tab's root with the daemon layout.
                            // Focus stays on whatever pane was focused before (or
                            // falls back to the first leaf in the new tree).
                            let at = sv.active_tab;
                            let old_focus = sv.tabs[at].focus_pane;
                            let new_leaves = pane_leaves_in_order(&layout);
                            let focus = if new_leaves.contains(&old_focus) {
                                old_focus
                            } else {
                                new_leaves.into_iter().next().unwrap_or(old_focus)
                            };
                            sv.tabs[at].root = layout;
                            sv.tabs[at].focus_pane = focus;
                            tracing::info!(
                                "pane-sync: applied daemon layout to active tab of session {}",
                                info.id,
                            );
                        } else {
                            // Fallback: add each new pane as a separate tab.
                            // This preserves the previous behaviour when the RPC
                            // fails, at the cost of incorrect tab/split mapping.
                            for pane_info in &new_panes {
                                // Only add a tab if the slot was successfully attached.
                                if pane_to_slot_idx(&state.slots, pane_info.id).is_some() {
                                    let tab_n = sv.tabs.len() + 1;
                                    sv.tabs.push(Tab {
                                        root: LayoutNode::Leaf(pane_info.id),
                                        focus_pane: pane_info.id,
                                        zoomed: None,
                                        boundaries: Vec::new(),
                                        drag: None,
                                    });
                                    tracing::warn!(
                                        "pane-sync: fallback — new pane {} added as tab-{} \
                                         (get_session_layout failed)",
                                        pane_info.id,
                                        tab_n,
                                    );
                                }
                            }
                        }
                    }

                    // Prune panes the daemon no longer reports (they closed externally).
                    let daemon_ids_for_session: Vec<PaneId> =
                        daemon_panes.iter().map(|p| p.id).collect();
                    // Collect slot indices to null-out for panes that vanished.
                    let slots_to_drop: Vec<usize> = {
                        let sv = &state.sessions[sv_idx];
                        let mut to_drop = Vec::new();
                        for tab in &sv.tabs {
                            for pid in pane_leaves_in_order(&tab.root) {
                                if !daemon_ids_for_session.contains(&pid) {
                                    if let Some(idx) = pane_to_slot_idx(&state.slots, pid) {
                                        to_drop.push(idx);
                                    }
                                }
                            }
                        }
                        to_drop
                    };
                    for slot_idx in slots_to_drop {
                        state.slots[slot_idx] = None;
                    }
                }

                // Prune sessions that disappeared from the daemon.
                let daemon_ids: Vec<SessionId> = daemon_sessions.iter().map(|s| s.id).collect();
                // Collect indices to remove (in reverse order so removal is safe).
                let to_remove: Vec<usize> = state
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, sv)| !daemon_ids.contains(&sv.id))
                    .map(|(i, _)| i)
                    .collect();
                for &idx in to_remove.iter().rev() {
                    state.sessions.remove(idx);
                }

                // Restore active_session to the index of the session the user
                // was viewing before this sync cycle. Removals at indices lower
                // than the previous active_session shift every subsequent index
                // down, so the naïve numeric index is no longer reliable.
                // Only change active_session if the previously-active session
                // itself was pruned (it disappeared from the daemon), in which
                // case fall back to the last remaining session.
                if let Some(id) = prev_active_id {
                    if let Some(new_idx) = state.sessions.iter().position(|sv| sv.id == id) {
                        state.active_session = new_idx;
                    } else if !state.sessions.is_empty() {
                        // The session the user was on no longer exists — pick last.
                        state.active_session = state.sessions.len() - 1;
                    }
                } else if state.active_session >= state.sessions.len() && !state.sessions.is_empty()
                {
                    state.active_session = state.sessions.len() - 1;
                }
            }
        }

        // Guard: exit immediately if every session was removed by any code path.
        if state.sessions.is_empty() {
            break;
        }

        // Periodic layout resync — safety net for missed LayoutChanged events.
        // Every 5 s, re-fetch the daemon's authoritative LayoutNode for the
        // active session and reconcile the active tab's root tree. This catches
        // any splits that arrived while the TUI was not polling (e.g. rapid MCP
        // calls) without waiting for the next 1 s session-sync cycle.
        if state.layout_resync_last_poll.elapsed() >= Duration::from_secs(5) {
            state.layout_resync_last_poll = Instant::now();
            let active_session_id = state.sessions[state.active_session].id;
            if let Ok(Ok(fresh_layout)) = state
                .control
                .get_session_layout(tarpc::context::current(), active_session_id)
                .await
            {
                let si = state.active_session;
                let at = state.sessions[si].active_tab;
                let daemon_leaves = pane_leaves_in_order(&fresh_layout);
                // Attach slots for any pane in the daemon layout we don't know yet.
                let mut new_ids: Vec<PaneId> = Vec::new();
                for &pid in &daemon_leaves {
                    if pane_to_slot_idx(&state.slots, pid).is_none() {
                        new_ids.push(pid);
                    }
                }
                for pid in new_ids {
                    let (pc, pr) = {
                        let (tc, tr) = term_size();
                        compute_pane_inner_size(tc, tr)
                    };
                    match attach_pane(&state.socket, active_session_id, pid, pc, pr).await {
                        Ok(slot) => {
                            state.slots.push(Some(slot));
                            tracing::info!(
                                "layout-resync: attached missing slot for pane {} in session {}",
                                pid,
                                active_session_id,
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "layout-resync: attach_pane for pane {} failed: {e}",
                                pid,
                            );
                        }
                    }
                }
                // Replace the active tab's root only if the tree changed.
                let old_leaves = pane_leaves_in_order(&state.sessions[si].tabs[at].root);
                let new_leaves = pane_leaves_in_order(&fresh_layout);
                if old_leaves != new_leaves {
                    let old_focus = state.sessions[si].tabs[at].focus_pane;
                    let focus = if new_leaves.contains(&old_focus) {
                        old_focus
                    } else {
                        new_leaves.into_iter().next().unwrap_or(old_focus)
                    };
                    state.sessions[si].tabs[at].root = fresh_layout;
                    state.sessions[si].tabs[at].focus_pane = focus;
                    tracing::info!(
                        "layout-resync: updated tab layout for session {}",
                        active_session_id,
                    );
                }
            }
        }

        // Session-lost detection: check whether the active session's active tab
        // has any live (Some) pane slot. If all slots are None the daemon evicted
        // all panes from this session (e.g. last pane closed via close_pane RPC).
        // Auto-switch to the next available session when one exists; otherwise
        // set session_lost so the overlay is shown.
        {
            let si = state.active_session;
            let ti = state.sessions[si].active_tab;
            let all_dead = pane_leaves_in_order(&state.sessions[si].tabs[ti].root)
                .iter()
                .all(|pid| pane_to_slot_idx(&state.slots, *pid).is_none());

            if all_dead && !state.session_lost {
                // Try to find another session that still has at least one live slot.
                let alt = (0..state.sessions.len()).find(|&other_si| {
                    if other_si == si {
                        return false;
                    }
                    let other_ti = state.sessions[other_si].active_tab;
                    pane_leaves_in_order(&state.sessions[other_si].tabs[other_ti].root)
                        .iter()
                        .any(|pid| pane_to_slot_idx(&state.slots, *pid).is_some())
                });
                if let Some(next_si) = alt {
                    state.active_session = next_si;
                    state.session_lost = false;
                } else {
                    state.session_lost = true;
                }
            } else if !all_dead {
                // Panes came back (e.g. new session opened) — clear the overlay.
                state.session_lost = false;
            }
        }

        state.anim.tick();
        // Draw — pass state as mut so render_pane can store last_screen_rect.
        draw_frame(&mut terminal, &mut state, prefix_active)?;
        loop_frames_drawn += 1;

        // 1 s periodic stats log.
        if loop_stats_at.elapsed() >= Duration::from_secs(1) {
            tracing::debug!(
                frames_drawn = loop_frames_drawn,
                bytes_processed = loop_bytes_processed,
                "event-loop: 1s stats"
            );
            loop_frames_drawn = 0;
            loop_bytes_processed = 0;
            loop_stats_at = Instant::now();
        }

        // Drain pending resize RPCs collected by render_pane (fire-and-forget).
        let resizes = std::mem::take(&mut state.pending_resizes);
        if !resizes.is_empty() {
            let client = state.control.clone();
            tokio::spawn(async move {
                for (pane_id, size) in resizes {
                    let req = pyre_proto::ResizePaneReq { pane_id, size };
                    let _ = client.resize_pane(tarpc::context::current(), req).await;
                }
            });
        }

        // Poll crossterm events (~16 ms = 60 fps)
        if !crossterm::event::poll(Duration::from_millis(16))? {
            continue;
        }

        // Compute body_area for mouse hit-tests (mirrors draw_frame layout).
        // Now: row0=sessions, row1=tabs, rows2..N-1=body, rowN=status.
        let term_size_rect = terminal.size()?;
        let outer_rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(term_size_rect.into());
        let body_area = outer_rects[2];

        match crossterm::event::read()? {
            Event::Mouse(me) => {
                handle_mouse(&mut state, me, body_area);
                // Drain deferred async actions queued by the sync mouse handler.
                if let Some(action) = state.pending_menu_action.take() {
                    match action {
                        PendingMenuAction::SplitH => {
                            if let Err(e) = split_active(&mut state, true).await {
                                tracing::warn!("context menu HSplit: {e}");
                            }
                        }
                        PendingMenuAction::SplitV => {
                            if let Err(e) = split_active(&mut state, false).await {
                                tracing::warn!("context menu VSplit: {e}");
                            }
                        }
                        PendingMenuAction::RenameSession => {
                            let sv = &state.sessions[state.active_session];
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::RenameSession(sv.id),
                                input: sv.name.clone(),
                            });
                        }
                        PendingMenuAction::SearchJump(idx) => {
                            // Mirror the Enter key handler for search.
                            if idx < state.search.results.len() {
                                let hit = &state.search.results[idx];
                                let target_pane = hit.block.pane;
                                let target_block = hit.block.id;
                                type JumpTarget = (usize, usize, PaneId, usize);
                                let mut jump: Option<JumpTarget> = None;
                                'search_jump: for (si, sv) in state.sessions.iter().enumerate() {
                                    for (ti, tab) in sv.tabs.iter().enumerate() {
                                        for pid in pane_leaves_in_order(&tab.root) {
                                            if pid == target_pane {
                                                if let Some(slot_idx) =
                                                    pane_to_slot_idx(&state.slots, pid)
                                                {
                                                    jump = Some((si, ti, pid, slot_idx));
                                                    if si == state.active_session {
                                                        break 'search_jump;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some((si, ti, pane_id, slot_idx)) = jump {
                                    state.active_session = si;
                                    state.sessions[si].active_tab = ti;
                                    state.sessions[si].tabs[ti].focus_pane = pane_id;
                                    if let Some(c) = state.slots[slot_idx].as_ref().and_then(|s| {
                                        s.recent_blocks.iter().position(|b| b.id == target_block)
                                    }) {
                                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                                            slot.ribbon_cursor = Some(c);
                                        }
                                    }
                                } else {
                                    state.status_msg =
                                        Some("search: result pane not loaded".to_owned());
                                }
                            }
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        PendingMenuAction::ContextMenuActivate(item_idx) => {
                            // Mouse-left on a context menu item row: activate the item at
                            // item_idx, identical logic to the KeyCode::Enter handler.
                            if let Some(menu) = state.context_menu.take() {
                                let idx = item_idx.min(MENU_ITEMS.len().saturating_sub(1));
                                let item = MENU_ITEMS[idx];
                                let target = menu.target_slot;
                                match item {
                                    MenuItem::Copy => {
                                        if let Some(ref sel) = state.selection.clone() {
                                            let pane_idx = sel.pane_idx;
                                            let ((r0, c0), (r1, c1)) = sel.normalized();
                                            if let Some(slot) = state.slots[pane_idx].as_ref() {
                                                let grid = slot.term.grid();
                                                let num_cols = grid.columns();
                                                let mut text = String::new();
                                                for gr in r0..=r1 {
                                                    if gr > r0 {
                                                        text.push('\n');
                                                    }
                                                    let cs = if gr == r0 { c0 as usize } else { 0 };
                                                    let ce = if gr == r1 {
                                                        c1 as usize
                                                    } else {
                                                        num_cols.saturating_sub(1)
                                                    };
                                                    for c in cs..=ce {
                                                        let pt = TermPoint::new(
                                                            TermLine(gr as i32),
                                                            TermColumn(c),
                                                        );
                                                        let ch = grid[pt].c;
                                                        text.push(if ch == '\0' {
                                                            ' '
                                                        } else {
                                                            ch
                                                        });
                                                    }
                                                }
                                                let trimmed: String = text
                                                    .lines()
                                                    .map(|l| l.trim_end())
                                                    .collect::<Vec<_>>()
                                                    .join("\n");
                                                if !trimmed.is_empty() {
                                                    let _ = crate::clipboard::copy_to_clipboard(
                                                        &trimmed,
                                                    );
                                                    state.status_msg = Some("copied".to_owned());
                                                }
                                            }
                                        }
                                    }
                                    MenuItem::KillPane => {
                                        close_pane_by_slot_idx(&mut state, target);
                                        if state.sessions.is_empty() {
                                            break;
                                        }
                                    }
                                    MenuItem::SplitH => {
                                        if let Err(e) = split_active(&mut state, true).await {
                                            tracing::warn!("context menu mouse HSplit: {e}");
                                        }
                                    }
                                    MenuItem::SplitV => {
                                        if let Err(e) = split_active(&mut state, false).await {
                                            tracing::warn!("context menu mouse VSplit: {e}");
                                        }
                                    }
                                    MenuItem::ZoomToggle => {
                                        let sv = state.active_session_view_mut();
                                        let tab = &mut sv.tabs[sv.active_tab];
                                        if tab.zoomed.is_some() {
                                            tab.zoomed = None;
                                        } else {
                                            tab.zoomed = Some(tab.focus_pane);
                                        }
                                    }
                                    MenuItem::InspectPid => {
                                        if let Some(slot) = state.slots[target].as_ref() {
                                            let pane_id = slot.pane_id;
                                            match state
                                                .control
                                                .inspect_pid(tarpc::context::current(), pane_id)
                                                .await
                                            {
                                                Ok(Ok(info)) => {
                                                    state.pid_inspect = Some(info);
                                                }
                                                Ok(Err(e)) => {
                                                    state.status_msg =
                                                        Some(format!("inspect_pid: {e}"));
                                                }
                                                Err(e) => {
                                                    state.status_msg =
                                                        Some(format!("rpc transport: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        PendingMenuAction::ContextMenuCommit => {
                            // Used for double-click on ribbon chip: open pager for the
                            // currently focused block cursor. Mirrors the Enter-in-ribbon
                            // handler in the key path.
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                                if let Some(slot) = state.slots[slot_idx].as_ref() {
                                    if let Some(cursor) = slot.ribbon_cursor {
                                        if let Some(block) = slot.recent_blocks.get(cursor) {
                                            let block_id_str = block.id.0.to_string();
                                            let exit_code = block.exit_code;
                                            let block_id = block.id;
                                            match state
                                                .control
                                                .get_block_stdout(
                                                    tarpc::context::current(),
                                                    block_id,
                                                )
                                                .await
                                            {
                                                Ok(Ok(raw)) => {
                                                    state.pager = Some(PagerState::new(
                                                        block_id_str,
                                                        exit_code,
                                                        &raw,
                                                    ));
                                                }
                                                Ok(Err(e)) => {
                                                    state.status_msg =
                                                        Some(format!("get_block_stdout: {e}"));
                                                }
                                                Err(e) => {
                                                    state.status_msg =
                                                        Some(format!("rpc transport: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Exit immediately if mouse action removed all sessions.
                if state.sessions.is_empty() {
                    break;
                }
            }

            Event::Key(key_event) => {
                let code = key_event.code;
                let mods = key_event.modifiers;

                // Session-lost overlay intercepts all keys when active.
                // q / Esc / Ctrl-C all exit the TUI cleanly.
                if state.session_lost {
                    match (code, mods) {
                        (KeyCode::Char('q'), _)
                        | (KeyCode::Esc, _)
                        | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                        _ => {}
                    }
                    continue;
                }

                // Name-prompt intercepts all keys when open.
                if state.prompt.is_some() {
                    match code {
                        KeyCode::Esc => {
                            state.prompt = None;
                        }
                        KeyCode::Backspace => {
                            if let Some(ref mut p) = state.prompt {
                                p.input.pop();
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(p) = state.prompt.take() {
                                let input = if p.input.is_empty() {
                                    None
                                } else {
                                    Some(p.input)
                                };
                                match p.kind {
                                    PromptKind::NewSession => {
                                        if let Err(e) = open_new_session(&mut state, input).await {
                                            tracing::warn!("open_new_session failed: {e}");
                                        }
                                    }
                                    PromptKind::NewTab => {
                                        if let Err(e) = open_new_tab(&mut state, input).await {
                                            tracing::warn!("open_new_tab failed: {e}");
                                        }
                                    }
                                    PromptKind::RenameSession(session_id) => {
                                        if let Some(new_name) = input {
                                            match state
                                                .control
                                                .rename_session(
                                                    tarpc::context::current(),
                                                    session_id,
                                                    new_name.clone(),
                                                )
                                                .await
                                            {
                                                Ok(Ok(())) => {
                                                    // Update local view immediately.
                                                    if let Some(sv) = state
                                                        .sessions
                                                        .iter_mut()
                                                        .find(|s| s.id == session_id)
                                                    {
                                                        sv.name = new_name;
                                                    }
                                                }
                                                Ok(Err(e)) => {
                                                    tracing::warn!("rename_session rpc error: {e}");
                                                    state.status_msg =
                                                        Some(format!("rename failed: {e}"));
                                                }
                                                Err(e) => {
                                                    tracing::warn!("rename_session transport: {e}");
                                                    state.status_msg =
                                                        Some(format!("rename rpc: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(ref mut p) = state.prompt {
                                p.input.push(c);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Context menu key handling — intercepts all keys while open.
                if state.context_menu.is_some() {
                    match code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            state.context_menu = None;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(ref mut m) = state.context_menu {
                                m.cursor = m.cursor.saturating_sub(1);
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(ref mut m) = state.context_menu {
                                let max = MENU_ITEMS.len().saturating_sub(1);
                                m.cursor = (m.cursor + 1).min(max);
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(menu) = state.context_menu.take() {
                                let item = MENU_ITEMS[menu.cursor];
                                let target = menu.target_slot;
                                match item {
                                    MenuItem::Copy => {
                                        // Copy the current text selection (if any) or last block.
                                        if let Some(ref sel) = state.selection.clone() {
                                            let pane_idx = sel.pane_idx;
                                            let ((r0, c0), (r1, c1)) = sel.normalized();
                                            if let Some(slot) = state.slots[pane_idx].as_ref() {
                                                let grid = slot.term.grid();
                                                let num_cols = grid.columns();
                                                let mut text = String::new();
                                                for gr in r0..=r1 {
                                                    if gr > r0 {
                                                        text.push('\n');
                                                    }
                                                    let cs = if gr == r0 { c0 as usize } else { 0 };
                                                    let ce = if gr == r1 {
                                                        c1 as usize
                                                    } else {
                                                        num_cols.saturating_sub(1)
                                                    };
                                                    for c in cs..=ce {
                                                        let pt = TermPoint::new(
                                                            TermLine(gr as i32),
                                                            TermColumn(c),
                                                        );
                                                        let ch = grid[pt].c;
                                                        text.push(if ch == '\0' {
                                                            ' '
                                                        } else {
                                                            ch
                                                        });
                                                    }
                                                }
                                                let trimmed: String = text
                                                    .lines()
                                                    .map(|l| l.trim_end())
                                                    .collect::<Vec<_>>()
                                                    .join("\n");
                                                if !trimmed.is_empty() {
                                                    let _ = crate::clipboard::copy_to_clipboard(
                                                        &trimmed,
                                                    );
                                                    state.status_msg = Some("copied".to_owned());
                                                }
                                            }
                                        }
                                    }
                                    MenuItem::KillPane => {
                                        close_pane_by_slot_idx(&mut state, target);
                                        if state.sessions.is_empty() {
                                            break;
                                        }
                                    }
                                    MenuItem::SplitH => {
                                        if let Err(e) = split_active(&mut state, true).await {
                                            tracing::warn!("context menu HSplit: {e}");
                                        }
                                    }
                                    MenuItem::SplitV => {
                                        if let Err(e) = split_active(&mut state, false).await {
                                            tracing::warn!("context menu VSplit: {e}");
                                        }
                                    }
                                    MenuItem::ZoomToggle => {
                                        let sv = state.active_session_view_mut();
                                        let tab = &mut sv.tabs[sv.active_tab];
                                        if tab.zoomed.is_some() {
                                            tab.zoomed = None;
                                        } else {
                                            tab.zoomed = Some(tab.focus_pane);
                                        }
                                    }
                                    MenuItem::InspectPid => {
                                        if let Some(slot) = state.slots[target].as_ref() {
                                            let pane_id = slot.pane_id;
                                            match state
                                                .control
                                                .inspect_pid(tarpc::context::current(), pane_id)
                                                .await
                                            {
                                                Ok(Ok(info)) => {
                                                    state.pid_inspect = Some(info);
                                                }
                                                Ok(Err(e)) => {
                                                    state.status_msg =
                                                        Some(format!("inspect_pid: {e}"));
                                                }
                                                Err(e) => {
                                                    state.status_msg =
                                                        Some(format!("rpc transport: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Theme picker key handling — intercepts all keys while open.
                if state.theme_picker.is_some() {
                    match code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            // Restore original theme before closing.
                            if let Some(p) = state.theme_picker.take() {
                                state.theme = p.original_theme;
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(ref mut p) = state.theme_picker {
                                p.cursor = p.cursor.saturating_sub(1);
                                // Live preview: apply the hovered theme immediately.
                                let reg = Registry::builtin();
                                if let Some(t) = reg.get(p.names[p.cursor]) {
                                    state.theme = t.clone();
                                }
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(ref mut p) = state.theme_picker {
                                let max = p.names.len().saturating_sub(1);
                                p.cursor = (p.cursor + 1).min(max);
                                // Live preview: apply the hovered theme immediately.
                                let reg = Registry::builtin();
                                if let Some(t) = reg.get(p.names[p.cursor]) {
                                    state.theme = t.clone();
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(p) = state.theme_picker.take() {
                                let name = p.names[p.cursor];
                                let reg = Registry::builtin();
                                if let Some(t) = reg.get(name) {
                                    // Theme already applied via live preview; persist to config.
                                    state.theme = t.clone();
                                    if let Err(e) = pyre_themes::config::save_theme_name(name) {
                                        tracing::warn!("save theme failed: {e}");
                                        state.status_msg = Some(format!("theme saved (warn: {e})"));
                                    } else {
                                        state.status_msg =
                                            Some(format!("theme: {}", t.display_name));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Block stdout pager key handling — intercepts all keys while open.
                if state.pager.is_some() {
                    let visible_rows = body_area.height.saturating_sub(2) as usize; // inner - footer
                    match code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            state.pager = None;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(ref mut p) = state.pager {
                                p.scroll_up(1);
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(ref mut p) = state.pager {
                                p.scroll_down(1, visible_rows);
                            }
                        }
                        KeyCode::PageUp => {
                            let n = visible_rows.max(1);
                            if let Some(ref mut p) = state.pager {
                                p.scroll_up(n);
                            }
                        }
                        KeyCode::PageDown => {
                            let n = visible_rows.max(1);
                            if let Some(ref mut p) = state.pager {
                                p.scroll_down(n, visible_rows);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Detect Ctrl-B prefix
                if !prefix_active
                    && mods.contains(KeyModifiers::CONTROL)
                    && matches!(code, KeyCode::Char('b'))
                {
                    prefix_active = true;
                    continue;
                }

                if prefix_active {
                    prefix_active = false;
                    match code {
                        KeyCode::Char('q') => break,

                        KeyCode::Char('c') => {
                            if let Err(e) = open_new_tab(&mut state, None).await {
                                tracing::warn!("open_new_tab failed: {e}");
                            }
                        }

                        KeyCode::Char('n') => {
                            let sv = state.active_session_view_mut();
                            sv.active_tab = (sv.active_tab + 1) % sv.tabs.len();
                        }

                        KeyCode::Char('p') => {
                            let sv = state.active_session_view_mut();
                            sv.active_tab = (sv.active_tab + sv.tabs.len() - 1) % sv.tabs.len();
                        }

                        KeyCode::Char('"') => {
                            if let Err(e) = split_active(&mut state, true).await {
                                tracing::warn!("HSplit failed: {e}");
                            }
                        }

                        KeyCode::Char('%') => {
                            if let Err(e) = split_active(&mut state, false).await {
                                tracing::warn!("VSplit failed: {e}");
                            }
                        }

                        KeyCode::Right | KeyCode::Down => {
                            let si = state.active_session;
                            let ti = state.sessions[si].active_tab;
                            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, true);
                        }

                        KeyCode::Left | KeyCode::Up => {
                            let si = state.active_session;
                            let ti = state.sessions[si].active_tab;
                            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, false);
                        }

                        // Enter scrollback mode for focused pane (block ribbon)
                        KeyCode::Char('[') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                                if let Some(slot) = state.slots[slot_idx].as_mut() {
                                    let last = slot.recent_blocks.len().saturating_sub(1);
                                    slot.ribbon_cursor = Some(last);
                                }
                            }
                        }

                        // Exit scrollback mode for focused pane (block ribbon)
                        KeyCode::Char(']') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                                if let Some(slot) = state.slots[slot_idx].as_mut() {
                                    slot.ribbon_cursor = None;
                                }
                            }
                        }

                        // Open search overlay
                        KeyCode::Char('/') => {
                            state.search.open = true;
                            state.search.input.clear();
                            state.search.cursor = 0;
                            state.search.results.clear();
                            state.search.pending_query = None;
                            state.search.rx = None;
                            state.status_msg = None;
                        }

                        // Zoom toggle (Ctrl-B z)
                        KeyCode::Char('z') => {
                            let sv = state.active_session_view_mut();
                            let tab = &mut sv.tabs[sv.active_tab];
                            if tab.zoomed.is_some() {
                                tab.zoomed = None;
                            } else {
                                tab.zoomed = Some(tab.focus_pane);
                            }
                        }

                        // Copy last block stdout to clipboard (Ctrl-B y)
                        KeyCode::Char('y') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                                if let Some(slot) = state.slots[slot_idx].as_ref() {
                                    if let Some(last_block) = slot.recent_blocks.last() {
                                        let block_id = last_block.id;
                                        match state
                                            .control
                                            .get_block_stdout(tarpc::context::current(), block_id)
                                            .await
                                        {
                                            Ok(Ok(bytes)) => {
                                                let text = String::from_utf8_lossy(&bytes);
                                                match clipboard::copy_to_clipboard(&text) {
                                                    Ok(()) => {
                                                        state.status_msg =
                                                            Some("copied to clipboard".to_owned());
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!("clipboard: {e}");
                                                        state.status_msg =
                                                            Some(format!("clipboard error: {e}"));
                                                    }
                                                }
                                            }
                                            Ok(Err(e)) => {
                                                state.status_msg =
                                                    Some(format!("get_block_stdout rpc: {e}"));
                                            }
                                            Err(e) => {
                                                state.status_msg =
                                                    Some(format!("rpc transport: {e}"));
                                            }
                                        }
                                    } else {
                                        state.status_msg = Some("no blocks".to_owned());
                                    }
                                } // if let Some(slot)
                            }
                        }

                        // Close focused pane (Ctrl-B x)
                        KeyCode::Char('x') => {
                            close_focused_pane(&mut state);
                            // If all sessions are gone, exit the TUI loop.
                            if state.sessions.is_empty() {
                                break;
                            }
                        }

                        // Toggle sidebar (Ctrl-B s)
                        KeyCode::Char('s') => {
                            state.sidebar_open = !state.sidebar_open;
                            if state.sidebar_open {
                                state.sidebar_focused = true;
                                state.sidebar_last_poll = Instant::now() - Duration::from_secs(10);
                            } else {
                                state.sidebar_focused = false;
                            }
                        }

                        // New session (Ctrl-B S — uppercase to avoid collision with Ctrl-B s sidebar)
                        KeyCode::Char('S') => {
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::NewSession,
                                input: String::new(),
                            });
                        }

                        // Theme picker (Ctrl-B T — uppercase to avoid collision with lower-t)
                        KeyCode::Char('T') => {
                            let reg = Registry::builtin();
                            let names: Vec<&'static str> =
                                reg.list().iter().map(|t| t.name).collect();
                            // Pre-select the currently active theme.
                            let cursor = names
                                .iter()
                                .position(|&n| n == state.theme.name)
                                .unwrap_or(0);
                            // Snapshot the current theme so Esc can restore it.
                            let original_theme = state.theme.clone();
                            state.theme_picker = Some(ThemePickerState {
                                cursor,
                                names,
                                original_theme,
                            });
                        }

                        // Rename active session (Ctrl-B ,  — mirrors tmux rename-session)
                        KeyCode::Char(',') => {
                            let sv = &state.sessions[state.active_session];
                            let current_name = sv.name.clone();
                            let session_id = sv.id;
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::RenameSession(session_id),
                                input: current_name,
                            });
                        }

                        // Toggle toast notifications (Ctrl-B N)
                        KeyCode::Char('N') => {
                            state.toast_deck.enabled = !state.toast_deck.enabled;
                            let label = if state.toast_deck.enabled {
                                "notifications on"
                            } else {
                                "notifications off"
                            };
                            state.status_msg = Some(label.to_owned());
                        }

                        // All other prefix keys consumed silently
                        _ => {}
                    }
                    continue;
                }

                // Search overlay key handling — intercepts all keys while open.
                if state.search.open {
                    match (code, mods) {
                        (KeyCode::Esc, _) => {
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        (KeyCode::Enter, _) => {
                            if !state.search.results.is_empty() {
                                let hit = &state.search.results[state.search.cursor];
                                let target_pane = hit.block.pane;
                                let target_block = hit.block.id;

                                // Search all sessions + tabs for a loaded pane matching
                                // target_pane. Prefer the current session; fall back to others.
                                type JumpTarget = (usize, usize, PaneId, usize);
                                let mut jump: Option<JumpTarget> = None;
                                'outer: for (si, sv) in state.sessions.iter().enumerate() {
                                    for (ti, tab) in sv.tabs.iter().enumerate() {
                                        for pid in pane_leaves_in_order(&tab.root) {
                                            if pid == target_pane {
                                                if let Some(slot_idx) =
                                                    pane_to_slot_idx(&state.slots, pid)
                                                {
                                                    jump = Some((si, ti, pid, slot_idx));
                                                    if si == state.active_session {
                                                        break 'outer;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                if let Some((si, ti, pane_id, slot_idx)) = jump {
                                    state.active_session = si;
                                    state.sessions[si].active_tab = ti;
                                    state.sessions[si].tabs[ti].focus_pane = pane_id;
                                    let maybe_cursor =
                                        state.slots[slot_idx].as_ref().and_then(|s| {
                                            s.recent_blocks
                                                .iter()
                                                .position(|b| b.id == target_block)
                                        });
                                    if let Some(c) = maybe_cursor {
                                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                                            slot.ribbon_cursor = Some(c);
                                        }
                                    }
                                } else {
                                    state.status_msg =
                                        Some("search: result pane not loaded".to_owned());
                                }
                            }
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            state.search.cursor = state.search.cursor.saturating_sub(1);
                        }
                        (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            let max = state.search.results.len().saturating_sub(1);
                            state.search.cursor = (state.search.cursor + 1).min(max);
                        }
                        (KeyCode::Backspace, _) => {
                            state.search.input.pop();
                            state.search.pending_query = Some(state.search.input.clone());
                            state.search.last_query_at = Instant::now();
                        }
                        (KeyCode::Char(c), _) => {
                            state.search.input.push(c);
                            state.search.pending_query = Some(state.search.input.clone());
                            state.search.last_query_at = Instant::now();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Sidebar navigation when sidebar is focused.
                if state.sidebar_open && state.sidebar_focused {
                    match code {
                        KeyCode::Up => {
                            state.sidebar_cursor = state.sidebar_cursor.saturating_sub(1);
                            continue;
                        }
                        KeyCode::Down => {
                            let max = state.sidebar_data.len().saturating_sub(1);
                            state.sidebar_cursor = (state.sidebar_cursor + 1).min(max);
                            continue;
                        }
                        KeyCode::Enter => {
                            if let Some(info) = state.sidebar_data.get(state.sidebar_cursor) {
                                let target = info.id;
                                let in_tab = {
                                    let sv = &state.sessions[state.active_session];
                                    let tab = &sv.tabs[sv.active_tab];
                                    pane_leaves_in_order(&tab.root).contains(&target)
                                };
                                if in_tab {
                                    let sv = &mut state.sessions[state.active_session];
                                    sv.tabs[sv.active_tab].focus_pane = target;
                                    state.sidebar_focused = false;
                                    let _ = state
                                        .control
                                        .mark_pane_seen(tarpc::context::current(), target)
                                        .await;
                                } else {
                                    state.status_msg =
                                        Some("open this pane first in a tab to focus".to_owned());
                                }
                            }
                            continue;
                        }
                        KeyCode::Esc => {
                            state.sidebar_focused = false;
                            continue;
                        }
                        _ => {}
                    }
                }

                // Block ribbon scrollback navigation (Ctrl-B [ mode).
                {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                        if let Some(slot) = state.slots[slot_idx].as_ref() {
                            if slot.ribbon_cursor.is_some() {
                                match code {
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        let s = state.slots[slot_idx].as_mut().expect("checked");
                                        s.ribbon_cursor =
                                            s.ribbon_cursor.map(|c| c.saturating_sub(1));
                                        continue;
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        let s = state.slots[slot_idx].as_mut().expect("checked");
                                        let max = s.recent_blocks.len().saturating_sub(1);
                                        s.ribbon_cursor = s.ribbon_cursor.map(|c| (c + 1).min(max));
                                        continue;
                                    }
                                    KeyCode::Esc => {
                                        let s = state.slots[slot_idx].as_mut().expect("checked");
                                        s.ribbon_cursor = None;
                                        continue;
                                    }
                                    KeyCode::Enter => {
                                        // Open modal pager for the focused block's stdout.
                                        if let Some(cursor) = slot.ribbon_cursor {
                                            if let Some(block) = slot.recent_blocks.get(cursor) {
                                                let block_id_str = block.id.0.to_string();
                                                let exit_code = block.exit_code;
                                                let block_id = block.id;
                                                match state
                                                    .control
                                                    .get_block_stdout(
                                                        tarpc::context::current(),
                                                        block_id,
                                                    )
                                                    .await
                                                {
                                                    Ok(Ok(bytes)) => {
                                                        state.pager = Some(PagerState::new(
                                                            block_id_str,
                                                            exit_code,
                                                            &bytes,
                                                        ));
                                                    }
                                                    Ok(Err(e)) => {
                                                        state.status_msg =
                                                            Some(format!("pager: rpc error: {e}"));
                                                    }
                                                    Err(e) => {
                                                        state.status_msg = Some(format!(
                                                            "pager: transport error: {e}"
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    _ => {
                                        continue;
                                    }
                                }
                            }
                        } // if let Some(slot)
                    }
                }

                // PgUp / PgDn for scrollback buffer (unmodified only).
                if mods == KeyModifiers::NONE {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                        let half_page = (body_area.height / 2).max(1) as usize;
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            match code {
                                KeyCode::PageUp => {
                                    slot.scroll_offset = (slot.scroll_offset + half_page)
                                        .min(slot.scrollback_capacity);
                                    continue;
                                }
                                KeyCode::PageDown => {
                                    slot.scroll_offset =
                                        slot.scroll_offset.saturating_sub(half_page);
                                    continue;
                                }
                                _ => {}
                            }
                        } // if let Some(slot)
                    }
                }

                // Forward key to focused pane.
                if let Some(bytes) = key_to_bytes(code, mods) {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset = 0;
                            let t0 = Instant::now();
                            let send_result = slot.input_tx.send(bytes.clone()).await;
                            let elapsed_us = t0.elapsed().as_micros();
                            tracing::debug!(
                                slot_idx,
                                key_bytes = bytes.len(),
                                elapsed_us,
                                send_ok = send_result.is_ok(),
                                "send_keys: input_tx.send (inline await)"
                            );
                        }
                    }
                }
            }

            Event::Paste(s) => {
                let mut buf = Vec::with_capacity(s.len() + 12);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\x1b[201~");
                let bytes = bytes::Bytes::from(buf);
                let sv = &state.sessions[state.active_session];
                let tab = &sv.tabs[sv.active_tab];
                if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                    if let Some(slot) = state.slots[slot_idx].as_mut() {
                        slot.scroll_offset = 0;
                        let send_result = slot.input_tx.send(bytes.clone()).await;
                        tracing::debug!(
                            slot_idx,
                            paste_bytes = bytes.len(),
                            send_ok = send_result.is_ok(),
                            "send_keys: bracketed paste input_tx.send"
                        );
                    }
                }
            }

            Event::Resize(new_cols, new_rows) => {
                // render_pane handles per-slot set_size with split-correct dims.
                // Just clear stale cells and let next draw repaint.
                terminal.clear()?;
                tracing::debug!("terminal resized to {new_cols}x{new_rows}");
            }

            _ => {}
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Route tracing to a file so logs survive ratatui's alternate-screen mode.
    // Fall back to stderr if the file cannot be opened.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/pyre-tui.log");
    match log_file {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::sync::Mutex::new(f))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::io::stderr)
                .init();
        }
    }

    let cli = Cli::parse();
    // Load the theme config before the splash so the flame uses the user's palette.
    // Non-fatal: falls back to built-in ember palette on any error.
    let splash_colors = {
        let reg = pyre_themes::Registry::builtin();
        let name = pyre_themes::config::load_theme_name()
            .unwrap_or(None)
            .unwrap_or_else(|| pyre_themes::Registry::default_theme().to_owned());
        let theme = reg
            .get(&name)
            .or_else(|| reg.get(pyre_themes::Registry::default_theme()))
            .expect("ember always present");
        splash::SplashColors::from_palette(&theme.palette)
    };
    splash::play_splash(cli.no_splash, Some(splash_colors));
    let socket = cli.socket.unwrap_or_else(default_socket);
    let shell = resolve_shell(cli.shell);

    match cli.command {
        None => {
            let client = control_client(&socket).await?;
            // Check for existing sessions first; attach to first if present.
            // All PTY spawning is deferred into run_tui() so that it happens
            // after the terminal enters alternate-screen and terminal.size()
            // returns true dimensions — not an 80×24 pre-alt-screen placeholder.
            let existing = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            // Iterate sessions and pick the first one that has a live pane.
            // list_sessions already computes pane_count per session via an
            // internal list_panes RPC; use that to skip sessions with zero panes
            // without issuing an additional list_panes RPC per stale session.
            // With many persisted stale sessions (e.g. 37 out of 38), this avoids
            // 37 extra sequential RPCs on every startup.
            let mut init = PaneInit::Spawn;
            for sess in existing {
                if sess.pane_count == 0 {
                    // Fast path: daemon already knows this session has no panes.
                    continue;
                }
                match first_pane(&client, sess.id).await {
                    Ok(pane) => {
                        init = PaneInit::Existing {
                            session: sess.id,
                            session_name: sess.name,
                            pane,
                        };
                        break;
                    }
                    Err(_) => {
                        // Session has no live pane — skip it.
                        continue;
                    }
                }
            }

            run_tui(socket, init, client, shell).await
        }
        Some(Sub::Attach {
            session: session_prefix,
            pane: pane_prefix,
        }) => {
            let client = control_client(&socket).await?;
            let sessions = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            let session_id = resolve_session(&client, &session_prefix).await?;
            let session_name = sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| {
                    let short8: String = session_id.0.to_string().chars().take(8).collect();
                    format!("session-{short8}")
                });

            let pane = match pane_prefix {
                Some(ref prefix) => resolve_pane(&client, session_id, prefix).await?,
                None => first_pane(&client, session_id).await?,
            };
            run_tui(
                socket,
                PaneInit::Existing {
                    session: session_id,
                    session_name,
                    pane,
                },
                client,
                shell,
            )
            .await
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── ToastDeck::push trims to max_visible ────────────────────────────────

    #[test]
    fn deck_push_trims_to_max_visible() {
        let mut deck = ToastDeck::new(true, 4000, 3);
        deck.push("a".into(), "body".into(), ToastKind::Info);
        deck.push("b".into(), "body".into(), ToastKind::Info);
        deck.push("c".into(), "body".into(), ToastKind::Info);
        // At capacity.
        assert_eq!(deck.toasts.len(), 3);
        // Push a fourth — oldest should be evicted.
        deck.push("d".into(), "body".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 3);
        // "a" (pushed first) should have been dropped; "d" should be at back.
        assert_eq!(deck.toasts.back().unwrap().title, "d");
        assert_eq!(deck.toasts.front().unwrap().title, "b");
    }

    // ── ToastDeck::tick drops expired toasts ────────────────────────────────

    #[test]
    fn deck_tick_drops_expired() {
        let mut deck = ToastDeck::new(true, 4000, 5);

        // Inject a toast that was born 5 s ago with a 4 s TTL (already expired).
        let expired = Toast {
            title: "old".into(),
            body: "body".into(),
            kind: ToastKind::Warn,
            born_at: Instant::now() - Duration::from_secs(5),
            ttl: Duration::from_secs(4),
        };
        deck.toasts.push_back(expired);

        // Inject a fresh toast with a 4 s TTL (not yet expired).
        deck.push("fresh".into(), "body".into(), ToastKind::Success);

        assert_eq!(deck.toasts.len(), 2);
        deck.tick();
        // Expired one should have been removed; fresh one remains.
        assert_eq!(deck.toasts.len(), 1);
        assert_eq!(deck.toasts.front().unwrap().title, "fresh");
    }

    // ── pane_event_to_toast: mapping coverage ───────────────────────────────

    fn make_event(
        kind: pyre_proto::PaneEventKind,
        state: Option<pyre_proto::PaneStateKind>,
    ) -> pyre_proto::PaneEvent {
        pyre_proto::PaneEvent {
            seq: 1,
            pane_id: "aabbccdd-0000-0000-0000-000000000000".into(),
            kind,
            state,
            agent: None,
        }
    }

    #[test]
    fn pane_event_to_toast_mapping() {
        use pyre_proto::{PaneEventKind, PaneStateKind};

        let ttl = Duration::from_millis(4000);

        // Spawned → Info
        let ev = make_event(PaneEventKind::Spawned, None);
        let t = pane_event_to_toast(&ev, ttl).expect("Spawned must produce a toast");
        assert_eq!(t.kind, ToastKind::Info);
        assert_eq!(t.body, "Spawned");

        // Closed → Info
        let ev = make_event(PaneEventKind::Closed, None);
        let t = pane_event_to_toast(&ev, ttl).expect("Closed must produce a toast");
        assert_eq!(t.kind, ToastKind::Info);
        assert_eq!(t.body, "Closed");

        // StateChanged(WaitingInput) → Warn
        let ev = make_event(
            PaneEventKind::StateChanged,
            Some(PaneStateKind::WaitingInput),
        );
        let t = pane_event_to_toast(&ev, ttl).expect("WaitingInput must produce a toast");
        assert_eq!(t.kind, ToastKind::Warn);

        // StateChanged(Done) → Success
        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Done));
        let t = pane_event_to_toast(&ev, ttl).expect("Done must produce a toast");
        assert_eq!(t.kind, ToastKind::Success);

        // StateChanged(Crashed) → Error
        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Crashed));
        let t = pane_event_to_toast(&ev, ttl).expect("Crashed must produce a toast");
        assert_eq!(t.kind, ToastKind::Error);

        // StateChanged(Idle) → suppressed (None)
        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Idle));
        assert!(
            pane_event_to_toast(&ev, ttl).is_none(),
            "Idle must be suppressed"
        );

        // StateChanged(Running) → suppressed (None)
        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Running));
        assert!(
            pane_event_to_toast(&ev, ttl).is_none(),
            "Running must be suppressed"
        );
    }
}
