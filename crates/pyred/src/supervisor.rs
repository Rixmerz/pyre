//! Supervisor process logic (ADR-002 Option C, hybrid mode).
//!
//! The supervisor:
//! * Binds the public UDS `$XDG_RUNTIME_DIR/pyre.sock` serving `PyreDaemon`.
//! * Binds a private UDS `$XDG_RUNTIME_DIR/pyre/supervisor.sock` for
//!   `SupervisorWorker` callbacks (workers connect here to register).
//! * Maintains a [`WorkerRegistry`] of live worker processes.
//! * Forks `pyred --mode worker` on `spawn` RPC; receives `register_worker`
//!   callback; stores the [`WorkerHandle`].
//! * Monitors heartbeats every 5 s; respawns workers that miss the 15 s window.
//! * Listens for SIGCHLD; removes exited workers and respawns them.
//! * Batches [`BlockEvent`]s from workers and writes to its Tantivy index.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use pyre_proto::service::PyreDaemon as _;
use pyre_proto::supervisor::{
    BlockEvent, RegisterAck, RpcError, SupervisorWorker, WorkerControlClient,
};
use pyre_proto::{
    layout, AttachAck, Block, BlockHit, BlockId, GitInfo, InputFrame, LayoutNode, ListBlocksReq,
    OpenPaneReq, OpenPaneSplitReq, OutputFrame, PaneEvent, PaneEventKind, PaneId, PaneInfo,
    PaneStateKind, PyreError, ReplayBlocks, ResizePaneReq, ResizePaneRes, SearchBlocksReq,
    SessionId, SessionInfo, SpawnReq, SpawnResp, WindowId, WindowInfo,
};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{Framed, FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::index::BlockIndex;
use crate::parser::BlockParser;
use crate::store::{BlobWriter, Store};

// ---------------------------------------------------------------------------
// Pane event bus (supervisor-side broadcaster)
// ---------------------------------------------------------------------------

/// Ring-buffer capacity for pane lifecycle events in hybrid mode.
const SUPERVISOR_EVENT_RING_CAP: usize = 256;

/// Shared broadcaster + ring buffer for `PaneEvent`s emitted by the supervisor.
///
/// Mirrors the design in `SessionRegistry` (single mode) so `next_pane_event`
/// has the same TOCTOU-safe subscribe-then-drain-history semantics.
pub struct PaneEventBus {
    tx: broadcast::Sender<PaneEvent>,
    ring: std::sync::Mutex<VecDeque<PaneEvent>>,
    seq: AtomicU64,
}

impl PaneEventBus {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(SUPERVISOR_EVENT_RING_CAP);
        Arc::new(Self {
            tx,
            ring: std::sync::Mutex::new(VecDeque::with_capacity(SUPERVISOR_EVENT_RING_CAP)),
            seq: AtomicU64::new(0),
        })
    }

    /// Assign the next seq, push into ring, and broadcast.
    pub fn emit(&self, pane_id: PaneId, kind: PaneEventKind, state: Option<PaneStateKind>) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ev = PaneEvent {
            seq,
            pane_id,
            kind,
            state,
            agent: None,
        };
        {
            let mut ring = self.ring.lock().unwrap_or_else(|e| {
                tracing::error!("PaneEventBus ring lock poisoned; recovering guard: {e}");
                e.into_inner()
            });
            if ring.len() >= SUPERVISOR_EVENT_RING_CAP {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        // IGNORED: broadcast::send error means no active receivers; slow subscribers
        // catch up via the ring buffer on their next call to events_after.
        let _ = self.tx.send(ev);
    }

    /// Subscribe and return buffered history with seq > `after_seq` atomically.
    pub fn events_after(&self, after_seq: u64) -> (Vec<PaneEvent>, broadcast::Receiver<PaneEvent>) {
        // Subscribe before reading history so no live event can slip through.
        let rx = self.tx.subscribe();
        let history: Vec<PaneEvent> = self
            .ring
            .lock()
            .unwrap_or_else(|e| {
                tracing::error!(
                    "PaneEventBus ring lock poisoned in events_after; recovering guard: {e}"
                );
                e.into_inner()
            })
            .iter()
            .filter(|e| e.seq > after_seq)
            .cloned()
            .collect();
        (history, rx)
    }
}

// ---------------------------------------------------------------------------
// Worker registry
// ---------------------------------------------------------------------------

/// Handle for a live worker process.
pub struct WorkerHandle {
    /// OS PID of the worker.
    pub pid: u32,
    /// Path to the worker's `WorkerControl` UDS (used for reconnect in S2).
    // dead_code: sock_path is stored so the supervisor can reconnect to a
    // worker after a crash (S2 reconnect feature). No reconnect logic exists
    // yet; keep the field so the registration wire format doesn't change when
    // that work lands.
    #[allow(dead_code)]
    pub sock_path: PathBuf,
    /// Path to the worker's raw-stream UDS for bidirectional PTY byte proxying.
    pub stream_sock: PathBuf,
    /// tarpc client connected to the worker's `WorkerControl` UDS.
    pub ctrl_client: WorkerControlClient,
    /// Last time a heartbeat was received from this worker.
    pub last_heartbeat: Instant,
}

/// Mapping from PaneId → (session_id_str, slot_idx).
/// Maintained by the supervisor so pane-scoped RPCs can be routed to the
/// correct worker without round-tripping through the worker.
#[derive(Default)]
struct PaneIndex {
    pane_to_slot: HashMap<uuid::Uuid, (String, u32)>,
    /// Next slot index to assign per session.
    next_slot: HashMap<String, u32>,
    /// Slots that have been explicitly closed via `pane_closed` RPC.
    /// `get_or_alloc_pane_by_slot` refuses to re-register an entry here,
    /// preventing late BlockEvents from a dying pane from resurrecting its
    /// slot in the index and inflating the remaining-pane count.
    dead_slots: HashSet<(String, u32)>,
}

impl PaneIndex {
    fn register(&mut self, pane_id: uuid::Uuid, session_id: String, slot_idx: u32) {
        self.pane_to_slot.insert(pane_id, (session_id, slot_idx));
    }

    fn lookup(&self, pane_id: uuid::Uuid) -> Option<(&str, u32)> {
        self.pane_to_slot
            .get(&pane_id)
            .map(|(s, i)| (s.as_str(), *i))
    }

    /// Reverse lookup: given (session_id, slot_idx) return the stable PaneId.
    /// Returns `None` if the slot is not tracked OR has been marked dead.
    fn pane_id_by_slot(&self, session_id: &str, slot_idx: u32) -> Option<uuid::Uuid> {
        self.pane_to_slot
            .iter()
            .find(|(_, (sid, s))| sid.as_str() == session_id && *s == slot_idx)
            .map(|(pane_id, _)| *pane_id)
    }

    /// Return true if this (session_id, slot_idx) has been explicitly closed.
    fn is_dead(&self, session_id: &str, slot_idx: u32) -> bool {
        self.dead_slots.contains(&(session_id.to_owned(), slot_idx))
    }

    fn next_slot(&mut self, session_id: &str) -> u32 {
        let entry = self.next_slot.entry(session_id.to_owned()).or_insert(0);
        let slot = *entry;
        *entry += 1;
        slot
    }

    fn remove_session(&mut self, session_id: &str) {
        self.pane_to_slot
            .retain(|_, (sid, _)| sid.as_str() != session_id);
        self.next_slot.remove(session_id);
        self.dead_slots
            .retain(|(sid, _)| sid.as_str() != session_id);
    }

    /// Remove one pane slot mapping, mark it dead, and return the number of
    /// panes still registered for `session_id` after the removal.
    fn remove_pane_slot(&mut self, session_id: &str, slot_idx: u32) -> usize {
        self.pane_to_slot
            .retain(|_, (sid, s)| !(sid.as_str() == session_id && *s == slot_idx));
        self.dead_slots.insert((session_id.to_owned(), slot_idx));
        self.pane_to_slot
            .values()
            .filter(|(sid, _)| sid.as_str() == session_id)
            .count()
    }
}

/// In-memory registry of live worker processes, keyed by session UUID string.
#[derive(Default)]
pub struct WorkerRegistry {
    inner: RwLock<HashMap<String, WorkerHandle>>,
    panes: RwLock<PaneIndex>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, session_id: String, handle: WorkerHandle) {
        self.inner.write().await.insert(session_id, handle);
    }

    pub async fn remove(&self, session_id: &str) -> Option<WorkerHandle> {
        self.panes.write().await.remove_session(session_id);
        self.inner.write().await.remove(session_id)
    }

    pub async fn touch_heartbeat(&self, session_id: &str) {
        if let Some(h) = self.inner.write().await.get_mut(session_id) {
            h.last_heartbeat = Instant::now();
        }
    }

    /// Allocate the next slot_idx for `session_id` and register a PaneId→slot mapping.
    /// Returns `(pane_id, slot_idx)`.
    pub async fn alloc_pane(&self, session_id: &str) -> (uuid::Uuid, u32) {
        let mut panes = self.panes.write().await;
        let slot_idx = panes.next_slot(session_id);
        let pane_id = uuid::Uuid::new_v4();
        panes.register(pane_id, session_id.to_owned(), slot_idx);
        (pane_id, slot_idx)
    }

    /// Return the stable PaneId for (session_id, slot_idx), allocating a new one
    /// if the slot was opened directly by the worker without going through the supervisor.
    ///
    /// Returns `None` if the slot has been explicitly closed via `pane_closed` RPC.
    /// Callers that only need best-effort identity (list_all_panes, process_raw_event)
    /// can safely ignore a `None` result.
    pub async fn get_or_alloc_pane_by_slot(
        &self,
        session_id: &str,
        slot_idx: u32,
    ) -> Option<uuid::Uuid> {
        {
            let panes = self.panes.read().await;
            // Return existing mapping if present.
            if let Some(pane_id) = panes.pane_id_by_slot(session_id, slot_idx) {
                return Some(pane_id);
            }
            // Refuse to resurrect a slot that was explicitly closed by pane_closed RPC.
            // This prevents late BlockEvents from a dying pane re-inflating the PaneIndex
            // and causing the remaining-count check in pane_closed to never reach zero.
            if panes.is_dead(session_id, slot_idx) {
                tracing::debug!(
                    session_id,
                    slot_idx,
                    "get_or_alloc_pane_by_slot: slot is dead, skipping re-allocation"
                );
                return None;
            }
        }
        // Slot not tracked yet — register a stable mapping now.
        let pane_id = uuid::Uuid::new_v4();
        self.panes
            .write()
            .await
            .register(pane_id, session_id.to_owned(), slot_idx);
        Some(pane_id)
    }

    /// Look up (session_id_str, slot_idx) for a PaneId.
    pub async fn lookup_pane(&self, pane_id: uuid::Uuid) -> Option<(String, u32)> {
        self.panes
            .read()
            .await
            .lookup(pane_id)
            .map(|(s, i)| (s.to_owned(), i))
    }

    /// Return the stream UDS path for a session.
    pub async fn get_stream_sock(&self, session_id: &str) -> Option<PathBuf> {
        self.inner
            .read()
            .await
            .get(session_id)
            .map(|h| h.stream_sock.clone())
    }

    /// Get the WorkerControlClient for a session (read-only borrow scope).
    pub async fn get_ctrl_client(
        &self,
        session_id: &str,
    ) -> Option<pyre_proto::supervisor::WorkerControlClient> {
        self.inner
            .read()
            .await
            .get(session_id)
            .map(|h| h.ctrl_client.clone())
    }

    /// Return session_ids where the last heartbeat is older than `timeout`.
    pub async fn stale_sessions(&self, timeout: Duration) -> Vec<String> {
        self.inner
            .read()
            .await
            .iter()
            .filter(|(_, h)| h.last_heartbeat.elapsed() > timeout)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Remove a single pane-slot mapping for a session and return the number of
    /// panes still registered for that session.  Used by `pane_closed` to decide
    /// whether to evict the session immediately.
    pub async fn remove_pane_slot(&self, session_id: &str, slot_idx: u32) -> usize {
        let remaining = self
            .panes
            .write()
            .await
            .remove_pane_slot(session_id, slot_idx);
        tracing::debug!(
            session_id,
            slot_idx,
            remaining,
            "remove_pane_slot: post-removal pane count"
        );
        remaining
    }

    /// Return the number of panes currently tracked for `session_id`.
    pub async fn pane_count(&self, session_id: &str) -> usize {
        self.panes
            .read()
            .await
            .pane_to_slot
            .values()
            .filter(|(sid, _)| sid.as_str() == session_id)
            .count()
    }

    /// Return all live PaneIds currently tracked for `session_id`.
    /// Dead slots (marked via `pane_closed`) are excluded automatically because
    /// `remove_pane_slot` erases them from `pane_to_slot`.
    pub async fn live_pane_ids(&self, session_id: &str) -> Vec<PaneId> {
        self.panes
            .read()
            .await
            .pane_to_slot
            .iter()
            .filter(|(_, (sid, _))| sid.as_str() == session_id)
            .map(|(uuid, _)| PaneId(*uuid))
            .collect()
    }

    /// Look up the session_id for a given worker PID; used by SIGCHLD handler.
    pub async fn session_for_pid(&self, pid: u32) -> Option<String> {
        self.inner
            .read()
            .await
            .iter()
            .find(|(_, h)| h.pid == pid)
            .map(|(id, _)| id.clone())
    }
}

// ---------------------------------------------------------------------------
// Per-pane mirror hub — multi-client output broadcast + serialised input
// ---------------------------------------------------------------------------

/// Shared state for all TUI clients attached to one pane.
///
/// A single connection is held open to the worker's stream UDS. Output bytes
/// from the worker are broadcast to every subscribed TUI client. Input from
/// any TUI client is sent to a shared `mpsc` channel whose single drainer
/// forwards them to the worker in order, preventing interleaving.
///
/// The ring-buffer snapshot (seq=0 frame from the worker) is saved in
/// `last_snapshot` so that late-arriving TUI clients receive it as a
/// synthetic seq=0 frame before subscribing to live output.
struct PaneMirrorHub {
    /// Broadcast sender: worker → all TUI clients.
    output_tx: broadcast::Sender<Bytes>,
    /// Serialised input queue: any TUI client → single worker connection.
    input_tx: mpsc::Sender<Bytes>,
    /// Most recent ring-buffer snapshot received from the worker (seq=0 frame).
    /// Replayed to each new TUI client so reattach shows prior terminal state.
    last_snapshot: Arc<Mutex<Bytes>>,
}

/// Registry of live per-pane mirror hubs, keyed by pane UUID.
///
/// An entry is created on the first TUI client attach for a pane and removed
/// when the worker-side connection closes (pane exited or worker respawned).
#[derive(Default, Clone)]
pub struct PaneMirrorRegistry {
    hubs: Arc<RwLock<HashMap<uuid::Uuid, Arc<PaneMirrorHub>>>>,
}

impl PaneMirrorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the existing hub for `pane_uuid`, or `None` if not yet created.
    async fn get(&self, pane_uuid: uuid::Uuid) -> Option<Arc<PaneMirrorHub>> {
        self.hubs.read().await.get(&pane_uuid).cloned()
    }

    /// Insert a new hub. Replaces any stale entry for the same pane.
    async fn insert(&self, pane_uuid: uuid::Uuid, hub: Arc<PaneMirrorHub>) {
        self.hubs.write().await.insert(pane_uuid, hub);
    }

    /// Remove the hub for `pane_uuid` (called when the worker side closes).
    async fn remove(&self, pane_uuid: uuid::Uuid) {
        self.hubs.write().await.remove(&pane_uuid);
    }

    /// Ring-buffer snapshot last received from the worker for this pane (hybrid reattach).
    pub async fn last_snapshot_for(&self, pane_uuid: uuid::Uuid) -> bytes::Bytes {
        if let Some(hub) = self.get(pane_uuid).await {
            hub.last_snapshot.lock().await.clone()
        } else {
            bytes::Bytes::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor implementation of PyreDaemon
// ---------------------------------------------------------------------------

/// tarpc `PyreDaemon` implementation backed by the worker registry.
/// In-memory metadata for a single window held by the supervisor.
///
/// Windows are a supervisor-only grouping over panes — workers never learn
/// about them, identical to how layout was supervisor-only before this change.
/// `layout` is `None` until the first pane is added (no placeholder needed;
/// `get_window_layout` falls through to the DB and then to the single-leaf
/// fallback when the in-memory entry is absent or has no layout).
#[derive(Clone)]
pub(crate) struct WindowMeta {
    session_id: String,
    name: String,
    position: u32,
    layout: Option<LayoutNode>,
}

///
/// All RPC methods that require PTY access are forwarded to the appropriate
/// worker via its `WorkerControl` client. Aggregated operations (list_sessions,
/// search_blocks) are handled locally.
#[derive(Clone)]
pub struct SupervisorImpl {
    pub registry: Arc<WorkerRegistry>,
    pub store: Arc<Store>,
    pub block_index: Arc<BlockIndex>,
    /// Sender for raw BlockEvents coming from workers (batched → Tantivy).
    /// Kept on the struct so `Clone` propagates it; read path active in S2.
    // dead_code: event_tx is used in SupervisorWorker::block_event (the
    // receive side drives the block-event batcher task). The field must stay
    // on the struct so Clone propagates the sender to every tarpc handler
    // clone; the lint fires because the field is never read back from the
    // struct after construction.
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<BlockEvent>,
    /// Path to the supervisor's callback socket, passed to spawned workers.
    pub supervisor_sock: PathBuf,
    /// Per-session oneshot channels awaited by `spawn` until `register_worker` fires.
    /// Key: session_id string. Entry removed once fired or timed out.
    pub pending_registrations: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    /// Per-pane broadcast hubs: one worker connection shared by N TUI clients.
    pub mirror_registry: PaneMirrorRegistry,
    /// Pending focus requests enqueued by `request_focus`, dequeued by `take_focus_request`.
    pub focus_queue: Arc<std::sync::Mutex<std::collections::VecDeque<PaneId>>>,
    /// Pane lifecycle event bus: broadcast ring for `next_pane_event` long-poll.
    pub pane_event_bus: Arc<PaneEventBus>,
    /// Per-window in-memory state: layout tree and metadata.
    ///
    /// Replaces the old `layout_store` (session-keyed) with a window-keyed map.
    /// Windows are supervisor-only; workers never learn about them.
    /// Layout is persisted to `self.store` (windows table) and mirrored here for
    /// fast reads without a DB round-trip.
    pub window_store: Arc<Mutex<HashMap<WindowId, WindowMeta>>>,
    /// Ordered window list per session. Key: session_id UUID string.
    /// Index entry is the source of truth for display order.
    pub session_windows: Arc<Mutex<HashMap<String, Vec<WindowId>>>>,
    /// Fast pane→window reverse index. Key: PaneId.
    ///
    /// Maintained alongside `window_store`: set on pane open/spawn, removed on
    /// pane close. Shared with `SupervisorWorkerImpl` so `pane_closed` can prune
    /// ghost leaves without re-querying the window store.
    pub pane_window_map: Arc<Mutex<HashMap<PaneId, WindowId>>>,
    /// Spawn cwd per session — recorded at `spawn()` time so `git_status` can
    /// fall back to it in hybrid mode where the worker PIDs are not available.
    ///
    /// ponytail: in hybrid mode we cannot follow `cd` within the pane; the
    /// chip reflects the directory the session was started in until we add
    /// live-cwd forwarding from the worker over WorkerControl.
    pub spawn_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
}

impl SupervisorImpl {
    /// Spawn a new worker process for `session_id`.
    ///
    /// The worker receives env vars:
    /// * `PYRE_SESSION_ID` — UUID string of the session.
    /// * `PYRE_WORKER_SOCK` — where it should bind its `WorkerControl` UDS.
    /// * `PYRE_SUPERVISOR_SOCK` — where it should dial to register.
    async fn spawn_worker(&self, session_id: &str) -> Result<()> {
        let rt_dir = runtime_dir();
        let worker_sock = rt_dir
            .join("pyre")
            .join(format!("session-{session_id}.sock"));

        let exe = std::env::current_exe().context("current_exe")?;
        let _child = std::process::Command::new(exe)
            .arg("--mode")
            .arg("worker")
            .env("PYRE_SESSION_ID", session_id)
            .env("PYRE_WORKER_SOCK", &worker_sock)
            .env("PYRE_SUPERVISOR_SOCK", &self.supervisor_sock)
            .spawn()
            .with_context(|| format!("spawn worker for session {session_id}"))?;

        tracing::info!(session_id, ?worker_sock, "spawned worker");
        Ok(())
    }
}

impl pyre_proto::service::PyreDaemon for SupervisorImpl {
    async fn spawn(self, _ctx: context::Context, req: SpawnReq) -> Result<SpawnResp, PyreError> {
        let session_uuid = uuid::Uuid::new_v4();
        let session_id_str = session_uuid.to_string();
        let sid = SessionId(session_uuid);

        if let Err(e) = self
            .store
            .upsert_session(sid, req.name.as_deref().unwrap_or(&session_id_str))
            .await
        {
            tracing::warn!("upsert_session {session_id_str}: {e:#}");
        }

        // Register the initial pane in PaneIndex BEFORE spawning so that any
        // follow-up RPC (e.g. close_pane) can resolve the PaneId immediately.
        let (pane_uuid, slot_idx) = self.registry.alloc_pane(&session_id_str).await;

        // Insert a oneshot BEFORE spawning so register_worker can never race
        // against us — it always finds the sender in the map.
        let (reg_tx, reg_rx) = oneshot::channel::<()>();
        self.pending_registrations
            .lock()
            .await
            .insert(session_id_str.clone(), reg_tx);

        self.spawn_worker(&session_id_str).await.map_err(|e| {
            // Clean up the pending entry so it doesn't leak.
            let pend = self.pending_registrations.clone();
            let sid_str = session_id_str.clone();
            tokio::spawn(async move {
                pend.lock().await.remove(&sid_str);
            });
            PyreError::SpawnFailed(e.to_string())
        })?;

        // Await worker registration with a 5 s timeout.
        match tokio::time::timeout(Duration::from_secs(5), reg_rx).await {
            Ok(Ok(())) => {
                tracing::debug!(session_id = session_id_str, "worker registration confirmed");
            }
            Ok(Err(_)) => {
                // Sender was dropped — treat as failure.
                return Err(PyreError::SpawnFailed(
                    "worker registration channel closed".into(),
                ));
            }
            Err(_) => {
                // Timeout — clean up and return error.
                self.pending_registrations
                    .lock()
                    .await
                    .remove(&session_id_str);
                return Err(PyreError::SpawnFailed(format!(
                    "worker for session {session_id_str} did not register within 5 s"
                )));
            }
        }

        // Actually create the PTY on the worker for slot_idx.  alloc_pane above
        // only reserves the PaneId↔slot mapping in the supervisor's PaneIndex;
        // without this call the worker has no entry in its panes map, so the
        // first stream connection returns "pane not found slot_idx=0" and the
        // worker immediately exits (all panes closed), causing a respawn loop.
        let shell = req.shell.clone().unwrap_or_default();
        // Capture spawn cwd before consuming req.cwd — used as the git_status
        // fallback in hybrid mode (worker PIDs are not visible to the supervisor).
        let spawn_cwd_pb: Option<PathBuf> = req.cwd.clone();
        let cwd = req
            .cwd
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cols = req.cols;
        let rows = req.rows;
        let ctrl_client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or_else(|| {
                PyreError::SpawnFailed("worker deregistered immediately after registration".into())
            })?;
        ctrl_client
            .open_pane(context::current(), slot_idx, shell, cwd, cols, rows)
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?;

        // Store the spawn cwd so git_status can fall back to it in hybrid mode.
        if let Some(pb) = spawn_cwd_pb {
            self.spawn_cwds
                .lock()
                .await
                .insert(session_id_str.clone(), pb);
        }

        let pane_id = PaneId(pane_uuid);

        // Create the default window (position 0, name "1") for this session.
        let default_window = WindowId::new();
        let now_ms = Utc::now().timestamp_millis();
        if let Err(e) = self
            .store
            .upsert_window(default_window, sid, "1", 0, now_ms)
            .await
        {
            tracing::warn!("spawn: upsert_window default {session_id_str}: {e:#}");
        }
        // Assign the initial pane to the default window in the store.
        if let Err(e) = self
            .store
            .assign_pane_window(pane_id, sid, default_window)
            .await
        {
            tracing::warn!("spawn: assign_pane_window {session_id_str}: {e:#}");
        }
        // Warm in-memory window state.
        {
            let mut ws = self.window_store.lock().await;
            ws.insert(
                default_window,
                WindowMeta {
                    session_id: session_id_str.clone(),
                    name: "1".into(),
                    position: 0,
                    layout: Some(LayoutNode::Leaf(pane_id)),
                },
            );
        }
        {
            let mut sw = self.session_windows.lock().await;
            sw.entry(session_id_str.clone())
                .or_default()
                .push(default_window);
        }
        {
            let mut pwm = self.pane_window_map.lock().await;
            pwm.insert(pane_id, default_window);
        }

        self.pane_event_bus.emit(
            pane_id,
            PaneEventKind::Spawned,
            Some(PaneStateKind::Running),
        );
        Ok(SpawnResp {
            session: sid,
            pane: pane_id,
            window: default_window,
        })
    }

    async fn attach(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<AttachAck, PyreError> {
        let id = session.0.to_string();
        let client = self
            .registry
            .get_ctrl_client(&id)
            .await
            .ok_or(PyreError::NoSuchSession(session))?;
        // Attach the first pane (slot 0) with a generated client_id.
        let client_id = uuid::Uuid::new_v4().to_string();
        client
            .attach_pane(context::current(), 0, client_id)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(AttachAck {
            session,
            cols: 80,
            rows: 24,
        })
    }

    async fn detach(self, _ctx: context::Context, _session: SessionId) -> Result<(), PyreError> {
        Ok(())
    }

    async fn kill(self, _ctx: context::Context, session: SessionId) -> Result<(), PyreError> {
        let id = session.0.to_string();
        if let Some(handle) = self.registry.remove(&id).await {
            let res = handle
                .ctrl_client
                .shutdown(context::current(), 5)
                .await
                .map_err(|e| PyreError::Io(e.to_string()))?;
            res.map_err(|e| PyreError::Io(e.to_string()))?;
        }
        Ok(())
    }

    async fn list_blocks(
        self,
        _ctx: context::Context,
        req: ListBlocksReq,
    ) -> Result<Vec<Block>, PyreError> {
        self.store
            .list_blocks(req.session, req.limit)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn search_blocks(
        self,
        _ctx: context::Context,
        req: SearchBlocksReq,
    ) -> Result<Vec<BlockHit>, PyreError> {
        let block_index = self.block_index.clone();
        let query = req.query.clone();
        let failures_only = req.failures_only;
        let limit = req.limit;
        let session = req.session;
        let pane = req.pane;
        let exit_code = req.exit_code;
        let ids = tokio::task::spawn_blocking(move || {
            block_index.search(&query, limit, failures_only, session, pane, exit_code)
        })
        .await
        .map_err(|e| PyreError::Io(e.to_string()))?
        .map_err(|e| PyreError::Io(e.to_string()))?;

        let mut blocks = Vec::with_capacity(ids.len());
        for id in ids {
            match self.store.get_block(id).await {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => {}
                Err(e) => tracing::warn!("get_block {id:?}: {e:#}"),
            }
        }
        let store = self.store.clone();
        let hits = tokio::task::spawn_blocking(move || {
            crate::search_filter::hits_with_snippets(&store, blocks, 160)
        })
        .await
        .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(hits)
    }

    async fn list_sessions(self, _ctx: context::Context) -> Result<Vec<SessionInfo>, PyreError> {
        // Collect (session_id_str, uuid, ctrl_client) while holding the read
        // lock for the minimum possible time — no async I/O inside the lock.
        // Holding a tokio RwLock read guard across await points blocks any
        // concurrent writer (e.g. register_worker), which causes the 5 s
        // registration timeout inside spawn() to fire.
        let snapshot: Vec<(
            String,
            uuid::Uuid,
            pyre_proto::supervisor::WorkerControlClient,
        )> = {
            let handles = self.registry.inner.read().await;
            handles
                .iter()
                .filter_map(|(id_str, handle)| {
                    uuid::Uuid::parse_str(id_str)
                        .ok()
                        .map(|u| (id_str.clone(), u, handle.ctrl_client.clone()))
                })
                .collect()
        }; // read lock dropped here

        let mut sessions = Vec::with_capacity(snapshot.len());
        for (session_id_str, uuid, ctrl_client) in snapshot {
            let sid = SessionId(uuid);
            let pane_count = match ctrl_client.list_panes(context::current()).await {
                Ok(Ok(slots)) => slots.len() as u32,
                _ => 0,
            };
            // Look up the human-readable name persisted in the supervisor store.
            // Fall back to the UUID string only when the row is absent (e.g.
            // after a DB reset), so that names set via SpawnReq or rename_session
            // are always surfaced to clients instead of the raw UUID.
            let name = match self.store.get_session_name(sid).await {
                Ok(Some(n)) if !n.is_empty() => n,
                _ => session_id_str.clone(),
            };
            sessions.push(SessionInfo {
                id: sid,
                name,
                pane_count,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
            });
        }
        Ok(sessions)
    }

    async fn list_panes(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<Vec<PaneInfo>, PyreError> {
        let id = session.0.to_string();
        let client = self
            .registry
            .get_ctrl_client(&id)
            .await
            .ok_or(PyreError::NoSuchSession(session))?;
        let slots = client
            .list_panes(context::current())
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        let mut panes = Vec::with_capacity(slots.len());
        for slot_idx in slots {
            let Some(pane_uuid) = self.registry.get_or_alloc_pane_by_slot(&id, slot_idx).await
            else {
                continue;
            };
            let mut info = match client.get_pane_info(context::current(), slot_idx).await {
                Ok(Ok(pi)) => pi,
                _ => continue,
            };
            info.id = PaneId(pane_uuid);
            info.session = session;
            // Overlay the persisted name from the supervisor store.  The worker
            // process has no name field (PaneHandle carries only PTY state); the
            // source of truth for the human-readable label is the supervisor's
            // SQLite row written by rename_pane.  Mirrors how list_sessions
            // overlays session names via get_session_name.
            if let Ok(Some(stored_name)) = self.store.get_pane_name(info.id).await {
                tracing::debug!(
                    pane = ?info.id,
                    name = %stored_name,
                    "[pyre-rename] list_panes: overlaying stored pane name"
                );
                info.name = Some(stored_name);
            }
            // Window overlay: look up this pane's window from the in-memory
            // pane_window_map.  If not present (fresh worker pane, or after daemon
            // restart before the map was warm), lazily assign it to the session's
            // default (position-0) window — creating one if none exists yet.
            let assigned_window = {
                let pwm = self.pane_window_map.lock().await;
                pwm.get(&info.id).copied()
            };
            let window_id = if let Some(wid) = assigned_window {
                wid
            } else {
                // ponytail: lazy-assign to default window; per-window active hint
                // is a follow-up if multi-window spawn-without-target becomes common
                let default_wid = self.get_or_create_default_window(&id, session).await;
                if let Some(wid) = default_wid {
                    if let Err(e) = self.store.assign_pane_window(info.id, session, wid).await {
                        tracing::warn!(
                            pane = ?info.id,
                            "list_panes: assign_pane_window failed: {e:#}"
                        );
                    }
                    let mut pwm = self.pane_window_map.lock().await;
                    pwm.insert(info.id, wid);
                    wid
                } else {
                    WindowId::default()
                }
            };
            info.window = window_id;
            panes.push(info);
        }
        Ok(panes)
    }

    async fn open_pane(
        self,
        _ctx: context::Context,
        req: OpenPaneReq,
    ) -> Result<PaneId, PyreError> {
        let session_id_str = req.session.0.to_string();
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchSession(req.session))?;
        let (pane_uuid, slot_idx) = self.registry.alloc_pane(&session_id_str).await;
        let shell = req.shell.unwrap_or_default();
        let cwd = req
            .cwd
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cols = req.cols;
        let rows = req.rows;
        client
            .open_pane(context::current(), slot_idx, shell, cwd, cols, rows)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        let pane_id = PaneId(pane_uuid);

        // Resolve target window: use req.window if non-nil, else default.
        let target_window = if req.window != WindowId::default() {
            req.window
        } else {
            self.get_or_create_default_window(&session_id_str, req.session)
                .await
                .unwrap_or_else(WindowId::new)
        };
        // Assign pane → window in store and in-memory map.
        if let Err(e) = self
            .store
            .assign_pane_window(pane_id, req.session, target_window)
            .await
        {
            tracing::warn!("open_pane: assign_pane_window: {e:#}");
        }
        {
            let mut pwm = self.pane_window_map.lock().await;
            pwm.insert(pane_id, target_window);
        }
        // Ensure the window exists in session_windows (create if the caller
        // passed a window we haven't seen yet — possible when the GUI creates a
        // window then immediately opens a pane into it).
        {
            let mut sw = self.session_windows.lock().await;
            let entry = sw.entry(session_id_str.clone()).or_default();
            if !entry.contains(&target_window) {
                let position = entry.len() as u32;
                entry.push(target_window);
                let mut ws = self.window_store.lock().await;
                ws.entry(target_window).or_insert_with(|| WindowMeta {
                    session_id: session_id_str.clone(),
                    name: (position + 1).to_string(),
                    position,
                    layout: Some(LayoutNode::Leaf(pane_id)),
                });
            }
        }

        self.pane_event_bus.emit(
            pane_id,
            PaneEventKind::Spawned,
            Some(PaneStateKind::Running),
        );
        Ok(pane_id)
    }

    async fn close_pane(self, _ctx: context::Context, pane: PaneId) -> Result<(), PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        client
            .close_pane(context::current(), slot_idx)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;

        // Look up this pane's window, then collapse the layout in window_store.
        // Collect json under the lock, then release before the async persist
        // (avoids holding window_store across an await — the prior close_pane
        // deadlock was caused by this exact pattern with layout_store).
        let window_id = {
            let pwm = self.pane_window_map.lock().await;
            pwm.get(&pane).copied()
        };
        let layout_persist: Option<(WindowId, String)> = if let Some(wid) = window_id {
            let mut ws = self.window_store.lock().await;
            if let Some(meta) = ws.get_mut(&wid) {
                if let Some(ref mut tree) = meta.layout {
                    tree.close(&pane);
                    let json = serde_json::to_string(tree).unwrap_or_default();
                    Some((wid, json))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        // Remove from reverse index.
        {
            let mut pwm = self.pane_window_map.lock().await;
            pwm.remove(&pane);
        }
        if let Some((wid, json)) = layout_persist {
            if let Err(e) = self.store.upsert_window_layout(wid, &json).await {
                tracing::warn!("supervisor: upsert_window_layout on close_pane: {e:#}");
            }
            self.pane_event_bus
                .emit(pane, PaneEventKind::LayoutChanged, None);
        }
        Ok(())
    }

    async fn replay(
        self,
        _ctx: context::Context,
        pane: PaneId,
        recent_blocks: u32,
    ) -> Result<ReplayBlocks, PyreError> {
        // Recent blocks from the supervisor store; grid snapshot from mirror hub.
        let blocks = self
            .store
            .list_blocks_for_pane(pane, recent_blocks)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        let snapshot = self.mirror_registry.last_snapshot_for(pane.0).await;
        Ok(ReplayBlocks {
            recent: blocks,
            snapshot,
        })
    }

    async fn get_block_stdout(
        self,
        _ctx: context::Context,
        block_id: BlockId,
    ) -> Result<Vec<u8>, PyreError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.read_block_stdout(block_id))
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn capture_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
        lines: u32,
    ) -> Result<Vec<u8>, PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        client
            .capture_pane(context::current(), slot_idx, lines)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn close_session(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<(), PyreError> {
        let id = session.0.to_string();

        // Snapshot window IDs before removing the session from the registry.
        let window_ids: Vec<WindowId> = {
            let sw = self.session_windows.lock().await;
            sw.get(&id).cloned().unwrap_or_default()
        };

        // Remove the session from the worker registry (no-op if already gone).
        let handle_opt = self.registry.remove(&id).await;

        // Best-effort worker shutdown — if the worker is already dead the
        // transport call fails, which is fine: we still clean local state below.
        if let Some(handle) = handle_opt {
            if let Err(e) = handle.ctrl_client.shutdown(context::current(), 5).await {
                tracing::warn!("close_session {id}: worker shutdown transport error: {e}");
            }
        }

        // Tear down in-memory window + pane state for every window of this session.
        for window in &window_ids {
            // Remove pane → window mappings for all panes in this window.
            let pane_ids = self
                .store
                .list_panes_for_window(*window)
                .await
                .unwrap_or_default();
            {
                let mut pwm = self.pane_window_map.lock().await;
                for pane in &pane_ids {
                    pwm.remove(pane);
                }
            }
            // Remove window from in-memory store.
            {
                let mut ws = self.window_store.lock().await;
                ws.remove(window);
            }
            // Delete from SQLite (best-effort).
            if let Err(e) = self.store.delete_window(*window).await {
                tracing::warn!("close_session: delete_window {window}: {e:#}");
            }
        }

        // Remove session → windows mapping.
        {
            let mut sw = self.session_windows.lock().await;
            sw.remove(&id);
        }

        Ok(())
    }

    async fn set_pane_state(
        self,
        _ctx: context::Context,
        pane: PaneId,
        state: PaneStateKind,
        reason: String,
    ) -> Result<(), PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        client
            .set_pane_state(context::current(), slot_idx, state, reason)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn list_all_panes(self, _ctx: context::Context) -> Result<Vec<PaneInfo>, PyreError> {
        // Snapshot the registry without holding the read lock across async I/O
        // (same pattern as list_sessions — avoids blocking register_worker).
        let snapshot: Vec<(
            String,
            uuid::Uuid,
            pyre_proto::supervisor::WorkerControlClient,
        )> = {
            let handles = self.registry.inner.read().await;
            handles
                .iter()
                .filter_map(|(id_str, handle)| {
                    uuid::Uuid::parse_str(id_str)
                        .ok()
                        .map(|u| (id_str.clone(), u, handle.ctrl_client.clone()))
                })
                .collect()
        }; // read lock dropped here

        let mut all = Vec::new();
        for (session_id_str, uuid, ctrl_client) in snapshot {
            let sid = SessionId(uuid);
            let slots = match ctrl_client.list_panes(context::current()).await {
                Ok(Ok(s)) => s,
                _ => continue,
            };
            for slot_idx in slots {
                let Some(pane_uuid) = self
                    .registry
                    .get_or_alloc_pane_by_slot(&session_id_str, slot_idx)
                    .await
                else {
                    continue;
                };
                let mut info = match ctrl_client
                    .get_pane_info(context::current(), slot_idx)
                    .await
                {
                    Ok(Ok(pi)) => pi,
                    _ => continue,
                };
                info.id = PaneId(pane_uuid);
                info.session = sid;
                // Overlay the persisted name from the supervisor store.  The
                // worker PaneHandle has no name field; the supervisor's SQLite
                // row is the single source of truth for user-assigned pane
                // names.  Without this overlay, rename_pane writes to SQLite
                // but list_all_panes returns name:None from the worker, so
                // the GUI polls always see the old (None) name and revert.
                // Mirrors how list_sessions overlays session names.
                if let Ok(Some(stored_name)) = self.store.get_pane_name(info.id).await {
                    tracing::debug!(
                        pane = ?info.id,
                        name = %stored_name,
                        "[pyre-rename] list_all_panes: overlaying stored pane name"
                    );
                    info.name = Some(stored_name);
                }
                all.push(info);
            }
        }
        Ok(all)
    }

    async fn wait_pane_state(
        self,
        ctx: context::Context,
        pane: PaneId,
        state: PaneStateKind,
        timeout_ms: u32,
    ) -> Result<bool, PyreError> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(1) as u64);
        let this = self.clone();
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            let panes = this.clone().list_all_panes(ctx).await?;
            if let Some(p) = panes.iter().find(|p| p.id == pane) {
                if p.state == state {
                    return Ok(true);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    async fn mark_pane_seen(self, _ctx: context::Context, pane: PaneId) -> Result<(), PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        client
            .mark_pane_seen(context::current(), slot_idx)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn last_block_for_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
    ) -> Result<Option<pyre_proto::Block>, PyreError> {
        let blocks = self
            .store
            .list_blocks_for_pane(pane, 1)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(blocks.into_iter().next())
    }

    async fn send_keys(
        self,
        _ctx: context::Context,
        pane: PaneId,
        bytes: Vec<u8>,
    ) -> Result<(), PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        client
            .send_keys(context::current(), slot_idx, bytes)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn inspect_pid(
        self,
        _ctx: context::Context,
        pane: PaneId,
    ) -> Result<pyre_proto::PidInspect, PyreError> {
        // Read the child PID from the worker's pane map is not available over
        // WorkerControl yet. Fall back to inspect the slot's child by reading
        // the worker handle's pid as a proxy, which at least shows the worker
        // process. Full per-pane PID routing is deferred to S3.
        let (session_id_str, _slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let pid = {
            let handles = self.registry.inner.read().await;
            handles.get(&session_id_str).map(|h| h.pid).unwrap_or(0)
        };
        Ok(crate::inspect::inspect_pid(pid))
    }

    async fn rename_session(
        self,
        _ctx: context::Context,
        session: SessionId,
        name: String,
    ) -> Result<(), PyreError> {
        // In hybrid mode the session name lives in the supervisor store.
        // The worker registry keyed by UUID string does not carry a mutable
        // name field, so we persist via the supervisor's store directly.
        self.store
            .upsert_session(session, &name)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn rename_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
        name: String,
    ) -> Result<(), PyreError> {
        // In hybrid/supervisor mode the pane name is persisted directly to
        // SQLite — the same approach rename_session uses for session names.
        // Verify the pane exists in the registry before writing; also capture
        // the session_id so rename_pane can upsert a row when the supervisor's
        // panes table has no entry (hybrid mode never inserts pane rows on
        // spawn — workers use their own per-session shard instead).
        let (session_id_str, _slot_idx) = self
            .registry
            .lookup_pane(pane.0)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let session_id = uuid::Uuid::parse_str(&session_id_str)
            .map(pyre_proto::SessionId)
            .map_err(|e| PyreError::Io(format!("invalid session uuid {session_id_str}: {e}")))?;
        tracing::debug!(
            pane = ?pane,
            session = %session_id_str,
            name = %name,
            "[pyre-rename] supervisor: persisting pane name via upsert"
        );
        self.store
            .rename_pane(pane, session_id, &name)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn resize_pane(
        self,
        _ctx: context::Context,
        req: ResizePaneReq,
    ) -> Result<ResizePaneRes, PyreError> {
        let (session_id_str, slot_idx) = self
            .registry
            .lookup_pane(req.pane_id.0)
            .await
            .ok_or(PyreError::NoSuchPane(req.pane_id))?;
        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(req.pane_id))?;
        client
            .resize_pane(context::current(), slot_idx, req.size.cols, req.size.rows)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(ResizePaneRes { ok: true })
    }

    async fn request_focus(
        self,
        _ctx: context::Context,
        pane_id: PaneId,
    ) -> Result<bool, PyreError> {
        self.focus_queue
            .lock()
            .map_err(|_| PyreError::Io("focus_queue lock poisoned".into()))?
            .push_back(pane_id);
        Ok(true)
    }

    async fn take_focus_request(self, _ctx: context::Context) -> Result<Option<PaneId>, PyreError> {
        Ok(self
            .focus_queue
            .lock()
            .map_err(|_| PyreError::Io("focus_queue lock poisoned".into()))?
            .pop_front())
    }

    async fn next_pane_event(
        self,
        _ctx: context::Context,
        after_seq: u64,
        timeout_ms: u32,
    ) -> Result<Vec<PaneEvent>, PyreError> {
        // Drain buffered history and subscribe atomically so no event slips through.
        let (history, mut rx) = self.pane_event_bus.events_after(after_seq);
        if !history.is_empty() {
            return Ok(history);
        }

        let deadline = tokio::time::Duration::from_millis(timeout_ms.max(1) as u64);
        let mut collected: Vec<PaneEvent> = Vec::new();

        // Wait for the first qualifying event, handling lagged receiver by
        // falling back to the ring buffer.
        let got_first = tokio::time::timeout(deadline, async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.seq > after_seq => {
                        collected.push(ev);
                        break;
                    }
                    Ok(_) => continue, // stale event behind our cursor
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let (missed, new_rx) = self.pane_event_bus.events_after(after_seq);
                        rx = new_rx;
                        if !missed.is_empty() {
                            collected.extend(missed);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;

        if got_first.is_err() {
            // Normal timeout — client loops with the same seq.
            return Ok(vec![]);
        }

        // Coalesce: drain any additional events that arrive within 1 ms.
        let coalesce = tokio::time::Duration::from_millis(1);
        loop {
            match tokio::time::timeout(coalesce, rx.recv()).await {
                Ok(Ok(ev)) if ev.seq > after_seq => collected.push(ev),
                _ => break,
            }
        }

        Ok(collected)
    }

    async fn gc_stale_sessions(self, _ctx: context::Context) -> Result<Vec<String>, PyreError> {
        // Build the list of zero-pane sessions first (avoids holding the registry
        // read-lock while sending RPCs).
        let stale: Vec<String> = {
            let handles = self.registry.inner.read().await;
            let mut out = Vec::new();
            for (session_id_str, handle) in handles.iter() {
                let pane_count = match handle.ctrl_client.list_panes(context::current()).await {
                    Ok(Ok(slots)) => slots.len(),
                    _ => continue, // can't determine — skip
                };
                if pane_count == 0 {
                    out.push(session_id_str.clone());
                }
            }
            out
        };

        let mut evicted = Vec::new();
        for id in stale {
            if let Some(handle) = self.registry.remove(&id).await {
                match handle.ctrl_client.shutdown(context::current(), 5).await {
                    Ok(_) => evicted.push(id),
                    Err(e) => tracing::warn!("gc_stale_sessions: shutdown {id}: {e}"),
                }
            }
        }
        tracing::info!("gc_stale_sessions: evicted {} session(s)", evicted.len());
        Ok(evicted)
    }

    // ── Window RPCs (window-model-plan §6.3) ──────────────────────────────

    async fn list_windows(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<Vec<WindowInfo>, PyreError> {
        let id = session.0.to_string();
        // Snapshot ordered window IDs for this session from the in-memory index.
        let window_ids: Vec<WindowId> = {
            let sw = self.session_windows.lock().await;
            sw.get(&id).cloned().unwrap_or_default()
        };
        // If empty, load from DB (cold start / after restart).
        let window_ids = if window_ids.is_empty() {
            let rows = self
                .store
                .list_windows(session)
                .await
                .map_err(|e| PyreError::Io(e.to_string()))?;
            let ids: Vec<WindowId> = rows.iter().map(|(wid, _, _)| *wid).collect();
            // Warm the in-memory index.
            {
                let mut ws = self.window_store.lock().await;
                let mut sw = self.session_windows.lock().await;
                let entry = sw.entry(id.clone()).or_default();
                for (wid, name, position) in &rows {
                    if !entry.contains(wid) {
                        entry.push(*wid);
                    }
                    ws.entry(*wid).or_insert_with(|| WindowMeta {
                        session_id: id.clone(),
                        name: name.clone(),
                        position: *position,
                        layout: None,
                    });
                }
            }
            ids
        } else {
            window_ids
        };

        let mut result = Vec::with_capacity(window_ids.len());
        for wid in window_ids {
            // Get name and position from in-memory store first.
            let (name, position) = {
                let ws = self.window_store.lock().await;
                if let Some(meta) = ws.get(&wid) {
                    (meta.name.clone(), meta.position)
                } else {
                    continue;
                }
            };
            // Overlay name from DB (same pattern as list_sessions overlays
            // session names via get_session_name).
            let name = match self.store.get_window_name(wid).await {
                Ok(Some(n)) if !n.is_empty() => n,
                _ => name,
            };
            // Count panes assigned to this window via the pane_window_map.
            let pane_count = {
                let pwm = self.pane_window_map.lock().await;
                pwm.values().filter(|&&w| w == wid).count() as u32
            };
            result.push(WindowInfo {
                id: wid,
                session,
                name,
                position,
                pane_count,
                created_at: Utc::now(),
            });
        }
        Ok(result)
    }

    async fn new_window(
        self,
        _ctx: context::Context,
        session: SessionId,
        name: Option<String>,
    ) -> Result<WindowId, PyreError> {
        let id = session.0.to_string();
        let window_id = WindowId::new();
        let position = {
            let sw = self.session_windows.lock().await;
            sw.get(&id).map(|v| v.len() as u32).unwrap_or(0)
        };
        let resolved_name = name.unwrap_or_else(|| (position + 1).to_string());
        let now_ms = Utc::now().timestamp_millis();
        self.store
            .upsert_window(window_id, session, &resolved_name, position, now_ms)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        {
            let mut ws = self.window_store.lock().await;
            ws.insert(
                window_id,
                WindowMeta {
                    session_id: id.clone(),
                    name: resolved_name,
                    position,
                    layout: None,
                },
            );
        }
        {
            let mut sw = self.session_windows.lock().await;
            sw.entry(id).or_default().push(window_id);
        }
        Ok(window_id)
    }

    async fn rename_window(
        self,
        _ctx: context::Context,
        window: WindowId,
        name: String,
    ) -> Result<(), PyreError> {
        // Update in-memory store first.
        {
            let mut ws = self.window_store.lock().await;
            if let Some(meta) = ws.get_mut(&window) {
                meta.name = name.clone();
            }
        }
        // Persist — mirrors rename_session (supervisor.rs:1136-1139).
        self.store
            .rename_window(window, &name)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn close_window(self, _ctx: context::Context, window: WindowId) -> Result<(), PyreError> {
        // Collect panes assigned to this window from the store.
        let pane_ids = self
            .store
            .list_panes_for_window(window)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;

        // Close each pane through the worker (mirrors the existing close_pane
        // path without re-running the window-layout collapse — we drop the whole
        // window below).
        for pane in &pane_ids {
            if let Some((session_id_str, slot_idx)) = self.registry.lookup_pane(pane.0).await {
                if let Some(client) = self.registry.get_ctrl_client(&session_id_str).await {
                    let _ = client.close_pane(context::current(), slot_idx).await;
                }
            }
            let mut pwm = self.pane_window_map.lock().await;
            pwm.remove(pane);
        }

        // Get session_id BEFORE removing the window from window_store.
        let session_id_opt = {
            let ws = self.window_store.lock().await;
            ws.get(&window).map(|m| m.session_id.clone())
        };
        // Remove from in-memory state.
        {
            let mut ws = self.window_store.lock().await;
            ws.remove(&window);
        }
        if let Some(sid) = session_id_opt {
            let mut sw = self.session_windows.lock().await;
            if let Some(v) = sw.get_mut(&sid) {
                v.retain(|&w| w != window);
            }
        }
        // Persist deletion.
        self.store
            .delete_window(window)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(())
    }

    async fn get_window_layout(
        self,
        _ctx: context::Context,
        window: WindowId,
    ) -> Result<layout::LayoutNode, PyreError> {
        // Find the session_id for this window (needed for live-pane lookup and
        // the single-leaf fallback).
        let session_id_str_opt = {
            let ws = self.window_store.lock().await;
            ws.get(&window).map(|m| m.session_id.clone())
        };
        // If not in memory, try loading from DB.
        let session_id_str = if let Some(s) = session_id_str_opt {
            s
        } else {
            // Attempt to find by querying DB — load all windows and warm the cache.
            // This handles the cold-start case after a daemon restart.
            return Err(PyreError::NoSuchSession(SessionId(uuid::Uuid::nil())));
        };
        let session_id = uuid::Uuid::parse_str(&session_id_str)
            .map(SessionId)
            .map_err(|_| {
                PyreError::Io(format!(
                    "invalid session uuid in window_store: {session_id_str}"
                ))
            })?;

        // ── Step 1: obtain the layout tree ──────────────────────────────────
        let tree_opt: Option<LayoutNode> = {
            let ws = self.window_store.lock().await;
            ws.get(&window).and_then(|m| m.layout.clone())
        };
        let mut tree = if let Some(t) = tree_opt {
            t
        } else {
            // Try to restore from the database (mirrors get_session_layout pattern).
            match self.store.get_window_layout_json(window).await {
                Ok(Some(json)) => match serde_json::from_str::<LayoutNode>(&json) {
                    Ok(t) => {
                        // Warm the in-memory window_store layout.
                        let mut ws = self.window_store.lock().await;
                        if let Some(meta) = ws.get_mut(&window) {
                            meta.layout = Some(t.clone());
                        }
                        t
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?window,
                            "get_window_layout: could not deserialize persisted layout: {e:#}"
                        );
                        return self
                            .window_single_leaf_fallback(session_id, &session_id_str)
                            .await;
                    }
                },
                _ => {
                    return self
                        .window_single_leaf_fallback(session_id, &session_id_str)
                        .await;
                }
            }
        };

        // ── Step 2: lazy ghost-leaf reconcile (mirrors get_session_layout) ───
        // Filter the live pane set to only panes belonging to this window.
        let all_live: std::collections::HashSet<PaneId> = self
            .registry
            .live_pane_ids(&session_id_str)
            .await
            .into_iter()
            .collect();
        let window_panes: std::collections::HashSet<PaneId> = {
            let pwm = self.pane_window_map.lock().await;
            pwm.iter()
                .filter(|(_, &w)| w == window)
                .map(|(p, _)| *p)
                .collect()
        };
        let live: std::collections::HashSet<PaneId> =
            all_live.intersection(&window_panes).copied().collect();

        if !live.is_empty() {
            let all_leaves = tree.all_leaves();
            let ghost_ids: Vec<PaneId> = all_leaves
                .into_iter()
                .filter(|id| !live.contains(id))
                .collect();
            if !ghost_ids.is_empty() {
                tracing::warn!(
                    ?window,
                    ghosts = ghost_ids.len(),
                    "get_window_layout: pruning ghost leaves"
                );
                for ghost in &ghost_ids {
                    tree.close(ghost);
                }
                let json = serde_json::to_string(&tree).unwrap_or_default();
                {
                    let mut ws = self.window_store.lock().await;
                    if let Some(meta) = ws.get_mut(&window) {
                        meta.layout = Some(tree.clone());
                    }
                }
                if let Err(e) = self.store.upsert_window_layout(window, &json).await {
                    tracing::warn!(
                        ?window,
                        "get_window_layout: upsert after ghost prune failed: {e:#}"
                    );
                }
            }
        }
        Ok(tree)
    }

    // ── Layout RPCs (M7-C, ADR-0005) ──────────────────────────────────────

    async fn open_pane_split(
        self,
        _ctx: context::Context,
        req: OpenPaneSplitReq,
    ) -> Result<PaneId, PyreError> {
        let parent = req.parent_pane;

        // Resolve which session and worker own the parent pane.
        let (session_id_str, _slot_idx) = self
            .registry
            .lookup_pane(parent.0)
            .await
            .ok_or(PyreError::NoSuchPane(parent))?;

        let sid = uuid::Uuid::parse_str(&session_id_str)
            .map(SessionId)
            .map_err(|_| PyreError::NoSuchPane(parent))?;

        let client = self
            .registry
            .get_ctrl_client(&session_id_str)
            .await
            .ok_or(PyreError::NoSuchPane(parent))?;

        // Allocate a new PaneId/slot in the supervisor's registry.
        let (pane_uuid, slot_idx) = self.registry.alloc_pane(&session_id_str).await;

        // Tell the worker to spawn the PTY for this slot.
        let shell = req.cmd.unwrap_or_default();
        let cwd = req
            .cwd
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Reuse parent pane's cols/rows — query the worker for them.
        let cols: u16 = 80;
        let rows: u16 = 24;

        client
            .open_pane(context::current(), slot_idx, shell, cwd, cols, rows)
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?;

        let new_pane_id = PaneId(pane_uuid);

        // Key the layout mutation by the PARENT pane's window — not session_id.
        // This mirrors what get_window_layout reads, keeping layout scoped per-window.
        let parent_window = {
            let pwm = self.pane_window_map.lock().await;
            pwm.get(&parent).copied()
        };
        let target_window = if let Some(wid) = parent_window {
            wid
        } else {
            // Fallback: assign to the session's default window.
            self.get_or_create_default_window(&session_id_str, sid)
                .await
                .unwrap_or_else(WindowId::new)
        };

        // Update the window's in-memory layout and persist.
        let layout_json: String = {
            let mut ws = self.window_store.lock().await;
            let meta = ws.entry(target_window).or_insert_with(|| WindowMeta {
                session_id: session_id_str.clone(),
                name: "1".into(),
                position: 0,
                layout: Some(LayoutNode::Leaf(parent)),
            });
            let tree = meta.layout.get_or_insert(LayoutNode::Leaf(parent));
            tree.split_focused(&parent, new_pane_id, req.orient);
            serde_json::to_string(tree).unwrap_or_default()
        }; // lock dropped here — no layout_store across await
        if let Err(e) = self
            .store
            .upsert_window_layout(target_window, &layout_json)
            .await
        {
            tracing::warn!("supervisor: upsert_window_layout after split: {e:#}");
        }

        // Assign the new pane to the same window as its parent.
        if let Err(e) = self
            .store
            .assign_pane_window(new_pane_id, sid, target_window)
            .await
        {
            tracing::warn!("open_pane_split: assign_pane_window: {e:#}");
        }
        {
            let mut pwm = self.pane_window_map.lock().await;
            pwm.insert(new_pane_id, target_window);
        }

        // Persist → emit (ADR-0005 invariant).
        self.pane_event_bus.emit(
            new_pane_id,
            PaneEventKind::Spawned,
            Some(PaneStateKind::Running),
        );
        self.pane_event_bus
            .emit(new_pane_id, PaneEventKind::LayoutChanged, None);

        Ok(new_pane_id)
    }

    async fn set_pane_weight(
        self,
        _ctx: context::Context,
        pane: PaneId,
        weight: u16,
    ) -> Result<(), PyreError> {
        // Resolve the pane's window — same keying strategy as open_pane_split.
        let window_id = {
            let pwm = self.pane_window_map.lock().await;
            pwm.get(&pane).copied()
        }
        .ok_or(PyreError::NoSuchPane(pane))?;

        let layout_json: String = {
            let mut ws = self.window_store.lock().await;
            let meta = ws.get_mut(&window_id).ok_or(PyreError::NoSuchPane(pane))?;
            let tree = meta.layout.as_mut().ok_or(PyreError::NoSuchPane(pane))?;
            tree.set_weight(&pane, weight);
            serde_json::to_string(tree).unwrap_or_default()
        }; // lock dropped before async call
        self.store
            .upsert_window_layout(window_id, &layout_json)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;

        self.pane_event_bus
            .emit(pane, PaneEventKind::LayoutChanged, None);
        Ok(())
    }

    /// Compat shim: return the first (position-0) window's layout for this
    /// session. New clients should call `get_window_layout` directly.
    ///
    /// **Deprecated** — kept for one release per window-model-plan §5.4.
    async fn get_session_layout(
        self,
        _ctx: context::Context,
        session_id: SessionId,
    ) -> Result<layout::LayoutNode, PyreError> {
        let id_str = session_id.0.to_string();
        // Find the first window in the session.
        let first_window: Option<WindowId> = {
            let sw = self.session_windows.lock().await;
            sw.get(&id_str).and_then(|v| v.first().copied())
        };
        if let Some(wid) = first_window {
            return self.get_window_layout(_ctx, wid).await;
        }
        // session_windows is cold — load from DB.
        let rows = self
            .store
            .list_windows(session_id)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        if let Some((wid, name, position)) = rows.into_iter().next() {
            // Warm the in-memory index.
            {
                let mut ws = self.window_store.lock().await;
                ws.entry(wid).or_insert_with(|| WindowMeta {
                    session_id: id_str.clone(),
                    name,
                    position,
                    layout: None,
                });
            }
            {
                let mut sw = self.session_windows.lock().await;
                let entry = sw.entry(id_str).or_default();
                if !entry.contains(&wid) {
                    entry.push(wid);
                }
            }
            return self.get_window_layout(_ctx, wid).await;
        }
        // No windows at all — fall back to the single-leaf builder.
        self.window_single_leaf_fallback(session_id, &id_str).await
    }

    /// Resolve git status for a session in hybrid mode.
    ///
    /// Worker pane child PIDs are not forwarded to the supervisor over the
    /// WorkerControl protocol, so `/proc/<pid>/cwd` is unavailable here.
    /// Fall back to the session's spawn cwd recorded at `spawn()` time.
    ///
    /// ponytail: the spawn-cwd fallback does not track `cd` within the pane;
    /// the chip reflects the session's starting directory until live-cwd
    /// forwarding from the worker is implemented (future work).
    async fn git_status(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<Option<GitInfo>, PyreError> {
        let session_id_str = session.0.to_string();
        let cwd = self
            .spawn_cwds
            .lock()
            .await
            .get(&session_id_str)
            .cloned();
        let Some(cwd) = cwd else {
            return Ok(None);
        };
        Ok(crate::git::git_info(&cwd).await)
    }
}

impl SupervisorImpl {
    /// Build a single-Leaf layout from the first live pane the worker reports.
    /// Used as the last-resort fallback in `get_window_layout` / `get_session_layout`.
    async fn window_single_leaf_fallback(
        &self,
        session_id: SessionId,
        id_str: &str,
    ) -> Result<layout::LayoutNode, PyreError> {
        let client = self
            .registry
            .get_ctrl_client(id_str)
            .await
            .ok_or(PyreError::NoSuchSession(session_id))?;
        let slots = client
            .list_panes(context::current())
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        let first_slot = slots.into_iter().next();
        if let Some(slot) = first_slot {
            if let Some(pane_uuid) = self.registry.get_or_alloc_pane_by_slot(id_str, slot).await {
                return Ok(LayoutNode::Leaf(PaneId(pane_uuid)));
            }
        }
        Err(PyreError::NoSuchSession(session_id))
    }

    /// Return the position-0 window for a session, creating one if none exists.
    ///
    /// Used by `list_panes` (lazy assignment) and `open_pane` (nil window
    /// target) to ensure every pane lands in a valid window.
    async fn get_or_create_default_window(
        &self,
        session_id_str: &str,
        session_id: SessionId,
    ) -> Option<WindowId> {
        // Fast path: already in memory.
        {
            let sw = self.session_windows.lock().await;
            if let Some(v) = sw.get(session_id_str) {
                if let Some(&wid) = v.first() {
                    return Some(wid);
                }
            }
        }
        // Load from DB (cold start after restart).
        if let Ok(rows) = self.store.list_windows(session_id).await {
            if let Some((wid, name, position)) = rows.into_iter().next() {
                // Warm in-memory caches.
                {
                    let mut ws = self.window_store.lock().await;
                    ws.entry(wid).or_insert_with(|| WindowMeta {
                        session_id: session_id_str.to_owned(),
                        name,
                        position,
                        layout: None,
                    });
                }
                {
                    let mut sw = self.session_windows.lock().await;
                    let entry = sw.entry(session_id_str.to_owned()).or_default();
                    if !entry.contains(&wid) {
                        entry.insert(0, wid);
                    }
                }
                return Some(wid);
            }
        }
        // No windows in DB either — create a default window now.
        let wid = WindowId::new();
        let now_ms = Utc::now().timestamp_millis();
        if let Err(e) = self
            .store
            .upsert_window(wid, session_id, "1", 0, now_ms)
            .await
        {
            tracing::warn!(
                session_id = session_id_str,
                "get_or_create_default_window: upsert_window failed: {e:#}"
            );
            return None;
        }
        {
            let mut ws = self.window_store.lock().await;
            ws.insert(
                wid,
                WindowMeta {
                    session_id: session_id_str.to_owned(),
                    name: "1".into(),
                    position: 0,
                    layout: None,
                },
            );
        }
        {
            let mut sw = self.session_windows.lock().await;
            sw.entry(session_id_str.to_owned()).or_default().push(wid);
        }
        Some(wid)
    }
}

// ---------------------------------------------------------------------------
// SupervisorWorker tarpc service (workers call in to this)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SupervisorWorkerImpl {
    registry: Arc<WorkerRegistry>,
    event_tx: mpsc::Sender<BlockEvent>,
    pending_registrations: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    pane_event_bus: Arc<PaneEventBus>,
    /// Shared with `SupervisorImpl` so `pane_closed` can prune ghost leaves
    /// from the correct window's layout (was layout_store keyed by session).
    window_store: Arc<Mutex<HashMap<WindowId, WindowMeta>>>,
    /// Shared reverse index so `pane_closed` can resolve pane → window and
    /// remove the entry without holding `window_store` across an await.
    pane_window_map: Arc<Mutex<HashMap<PaneId, WindowId>>>,
    store: Arc<crate::store::Store>,
}

impl SupervisorWorker for SupervisorWorkerImpl {
    async fn register_worker(
        self,
        _ctx: context::Context,
        session_id: String,
        pid: u32,
        sock_path: String,
        stream_sock_path: String,
    ) -> Result<RegisterAck, RpcError> {
        tracing::info!(
            session_id,
            pid,
            sock_path,
            stream_sock_path,
            "worker registered"
        );

        let worker_sock = PathBuf::from(&sock_path);
        let ctrl_client = connect_worker_ctrl(&worker_sock)
            .await
            .map_err(|e| RpcError::Internal(format!("connect worker ctrl: {e}")))?;

        let handle = WorkerHandle {
            pid,
            sock_path: worker_sock,
            stream_sock: PathBuf::from(&stream_sock_path),
            ctrl_client,
            last_heartbeat: Instant::now(),
        };
        self.registry.insert(session_id.clone(), handle).await;
        tracing::info!(session_id, pid, "worker handle stored");

        // Unblock any spawn() RPC that is waiting for this worker to register.
        if let Some(tx) = self.pending_registrations.lock().await.remove(&session_id) {
            // IGNORED: oneshot::send error means the receiver in spawn() already
            // timed out and dropped its half; the registration still succeeds —
            // the session is now live even if the original spawn() returned an error.
            let _ = tx.send(());
        }

        Ok(RegisterAck {
            aggregated_index_ready: true,
        })
    }

    async fn block_event(self, _ctx: context::Context, event: BlockEvent) -> Result<(), RpcError> {
        self.event_tx
            .send(event)
            .await
            .map_err(|_| RpcError::Internal("event_tx closed".into()))
    }

    async fn pane_closed(
        self,
        _ctx: context::Context,
        session_id: String,
        slot_idx: u32,
    ) -> Result<(), RpcError> {
        tracing::info!(session_id, slot_idx, "worker reports pane closed");
        // Resolve PaneId before removing the slot so we can prune the layout
        // and emit the event with a stable identity.
        let pane_uuid = self
            .registry
            .get_or_alloc_pane_by_slot(&session_id, slot_idx)
            .await;
        // Remove this pane from the supervisor's index and check whether any
        // panes remain for the session.  If none remain, evict the session from
        // the registry *now* — before the worker process actually exits — so
        // that the SIGCHLD handler finds no registry entry and does not respawn.
        let remaining = self.registry.remove_pane_slot(&session_id, slot_idx).await;
        if remaining == 0 {
            tracing::info!(
                session_id,
                "all panes closed — evicting session from registry (no respawn)"
            );
            self.registry.remove(&session_id).await;
        }

        // Prune the dead pane from the supervisor's window layout tree so that
        // get_window_layout never returns ghost leaves.  This mirrors what
        // SupervisorImpl::close_pane does for the RPC-initiated close path;
        // here we handle the case where the pane exits on its own (e.g. the
        // shell finishes, or Ctrl-B x inside the worker).
        if let Some(uuid) = pane_uuid {
            let pane_id = PaneId(uuid);

            // Resolve window from the reverse index; remove the entry.
            // Hold the lock only for the lookup + removal — not across awaits.
            let window_id = {
                let mut pwm = self.pane_window_map.lock().await;
                pwm.remove(&pane_id)
            };

            let persist: Option<(WindowId, String)> = if let Some(wid) = window_id {
                let mut ws = self.window_store.lock().await;
                if let Some(meta) = ws.get_mut(&wid) {
                    if let Some(ref mut tree) = meta.layout {
                        tree.close(&pane_id);
                        let json = serde_json::to_string(tree).unwrap_or_default();
                        Some((wid, json))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((wid, json)) = persist {
                if let Err(e) = self.store.upsert_window_layout(wid, &json).await {
                    tracing::warn!(
                        session_id,
                        "pane_closed: upsert_window_layout failed: {e:#}"
                    );
                }
                self.pane_event_bus
                    .emit(pane_id, PaneEventKind::LayoutChanged, None);
            }
            self.pane_event_bus
                .emit(pane_id, PaneEventKind::Closed, None);
        }
        Ok(())
    }

    async fn pane_state_changed(
        self,
        _ctx: context::Context,
        session_id: String,
        slot_idx: u32,
        state: PaneStateKind,
    ) -> Result<(), RpcError> {
        // Resolve the stable PaneId for this (session_id, slot_idx) pair.
        // If the slot is already dead (pane_closed fired first), discard silently.
        let Some(pane_uuid) = self
            .registry
            .get_or_alloc_pane_by_slot(&session_id, slot_idx)
            .await
        else {
            tracing::debug!(
                session_id,
                slot_idx,
                "pane_state_changed: slot dead, skipping"
            );
            return Ok(());
        };
        self.pane_event_bus
            .emit(PaneId(pane_uuid), PaneEventKind::StateChanged, Some(state));
        Ok(())
    }

    async fn heartbeat(self, _ctx: context::Context, session_id: String) -> Result<(), RpcError> {
        self.registry.touch_heartbeat(&session_id).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Block event batcher → Tantivy
// ---------------------------------------------------------------------------

/// Per-(session_id, slot_idx) parser + in-progress block state.
struct PaneParserState {
    parser: BlockParser,
    /// Writers keyed by in-progress BlockId.
    writers: HashMap<pyre_proto::BlockId, BlobWriter>,
    /// Stdout accumulator for Tantivy (capped at 256 KiB per block).
    stdout_bufs: HashMap<pyre_proto::BlockId, Vec<u8>>,
    /// Block metadata needed at BlockEnd.
    block_meta: HashMap<pyre_proto::BlockId, pyre_proto::Block>,
    /// Stable PaneId for this slot (resolved once on first event).
    pane_id: Option<pyre_proto::PaneId>,
}

impl PaneParserState {
    fn new(session_id: pyre_proto::SessionId) -> Self {
        Self {
            parser: BlockParser::new(session_id),
            writers: HashMap::new(),
            stdout_bufs: HashMap::new(),
            block_meta: HashMap::new(),
            pane_id: None,
        }
    }
}

async fn block_event_batcher(
    mut event_rx: mpsc::Receiver<BlockEvent>,
    block_index: Arc<BlockIndex>,
    store: Arc<Store>,
    registry: Arc<WorkerRegistry>,
) {
    // Parser state keyed by (session_id_str, slot_idx).
    let mut pane_parsers: HashMap<(String, u32), PaneParserState> = HashMap::new();
    let flush_interval = Duration::from_millis(50);
    let mut interval = tokio::time::interval(flush_interval);
    // Pending raw events collected between ticks.
    let mut pending: Vec<BlockEvent> = Vec::new();

    loop {
        tokio::select! {
            maybe_ev = event_rx.recv() => {
                match maybe_ev {
                    Some(ev) => pending.push(ev),
                    None => {
                        // Channel closed — flush remaining and finalize open blocks.
                        let evs = std::mem::take(&mut pending);
                        for ev in evs {
                            process_raw_event(ev, &mut pane_parsers, &store, &block_index, &registry).await;
                        }
                        finalize_open_blocks(&mut pane_parsers, &store, &block_index).await;
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                let evs = std::mem::take(&mut pending);
                if !evs.is_empty() {
                    for ev in evs {
                        process_raw_event(ev, &mut pane_parsers, &store, &block_index, &registry).await;
                    }
                }
            }
        }
    }
}

/// Feed one raw supervisor `BlockEvent` (PTY bytes) through the per-pane parser
/// and persist any finalized blocks to the store and Tantivy index.
async fn process_raw_event(
    raw: BlockEvent,
    pane_parsers: &mut HashMap<(String, u32), PaneParserState>,
    store: &Arc<Store>,
    block_index: &Arc<BlockIndex>,
    registry: &Arc<WorkerRegistry>,
) {
    use pyre_proto::blocks::BlockEvent as ParsedEvent;

    let key = (raw.session_id.clone(), raw.slot_idx);

    // Resolve session UUID — skip event on invalid UUID.
    let session_uuid = match uuid::Uuid::parse_str(&raw.session_id) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(
                session_id = raw.session_id,
                "block_event_batcher: invalid session uuid: {e}"
            );
            return;
        }
    };
    let session_id = pyre_proto::SessionId(session_uuid);

    // Get-or-create parser state for this pane slot.
    let state = pane_parsers
        .entry(key)
        .or_insert_with(|| PaneParserState::new(session_id));

    // Resolve PaneId once per slot.  If the slot has already been closed
    // (marked dead in the PaneIndex), skip this event entirely — the pane
    // is gone and indexing its output would produce orphaned blocks.
    if state.pane_id.is_none() {
        match registry
            .get_or_alloc_pane_by_slot(&raw.session_id, raw.slot_idx)
            .await
        {
            Some(pane_uuid) => state.pane_id = Some(pyre_proto::PaneId(pane_uuid)),
            None => return, // slot is dead — discard event
        }
    }
    let Some(pane_id) = state.pane_id else {
        tracing::error!(
            session_id = raw.session_id,
            slot_idx = raw.slot_idx,
            "process_raw_event: pane_id unexpectedly None after allocation; skipping event"
        );
        return;
    };

    // Feed bytes through the VTE parser.
    let mut parsed_events: Vec<ParsedEvent> = Vec::new();
    if !raw.bytes.is_empty() {
        state.parser.feed(&raw.bytes, &mut parsed_events);
    }

    for ev in parsed_events {
        match ev {
            ParsedEvent::PromptStart { .. } => {}
            ParsedEvent::CommandStart {
                block,
                ref command,
                ref cwd,
                ..
            } => {
                let proto_block = pyre_proto::Block {
                    id: block,
                    pane: pane_id,
                    session: session_id,
                    command: command.clone(),
                    cwd: cwd.clone(),
                    started_at: Utc::now(),
                    ended_at: None,
                    exit_code: None,
                    stdout_len: 0,
                };
                if let Err(e) = store.create_block(&proto_block).await {
                    tracing::warn!(?block, "block_event_batcher: create_block: {e:#}");
                    continue;
                }
                let blob_path = store.blob_path_for(block);
                match tokio::task::spawn_blocking(move || BlobWriter::open(&blob_path)).await {
                    Ok(Ok(bw)) => {
                        state.writers.insert(block, bw);
                    }
                    Ok(Err(e)) => tracing::warn!(?block, "BlobWriter::open: {e:#}"),
                    Err(e) => tracing::warn!(?block, "spawn_blocking BlobWriter::open: {e}"),
                }
                state.stdout_bufs.insert(block, Vec::new());
                state.block_meta.insert(block, proto_block);
            }
            ParsedEvent::OutputChunk { block, data } => {
                if let Some(buf) = state.stdout_bufs.get_mut(&block) {
                    const INDEX_CAP: usize = 256 * 1024;
                    let remaining = INDEX_CAP.saturating_sub(buf.len());
                    if remaining > 0 {
                        let take = data.len().min(remaining);
                        buf.extend_from_slice(&data[..take]);
                    }
                }
                if let Some(mut bw) = state.writers.remove(&block) {
                    let bytes_vec = data.to_vec();
                    let result =
                        tokio::task::spawn_blocking(move || bw.write(&bytes_vec).map(|_| bw)).await;
                    match result {
                        Ok(Ok(bw)) => {
                            state.writers.insert(block, bw);
                        }
                        Ok(Err(e)) => tracing::warn!(?block, "BlobWriter::write: {e:#}"),
                        Err(e) => tracing::warn!(?block, "spawn_blocking write: {e}"),
                    }
                }
            }
            ParsedEvent::BlockEnd { block, exit_code } => {
                let bw = state.writers.remove(&block);
                let stdout_len = if let Some(bw) = bw {
                    tokio::task::spawn_blocking(move || bw.close().unwrap_or(0))
                        .await
                        .unwrap_or(0)
                } else {
                    0
                };
                if let Err(e) = store
                    .finalize_block(block, Utc::now(), exit_code, stdout_len)
                    .await
                {
                    tracing::warn!(?block, "finalize_block: {e:#}");
                }
                let stdout_text = state
                    .stdout_bufs
                    .remove(&block)
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_default();
                if let Some(meta) = state.block_meta.remove(&block) {
                    let idx = block_index.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = idx.add_block(&meta, &stdout_text) {
                            tracing::warn!("block_index.add_block: {e:#}");
                        }
                    });
                }
                tracing::debug!(?block, ?exit_code, "block finalized");
            }
        }
    }
}

/// Finalize any in-progress blocks (best-effort on shutdown).
async fn finalize_open_blocks(
    pane_parsers: &mut HashMap<(String, u32), PaneParserState>,
    store: &Arc<Store>,
    block_index: &Arc<BlockIndex>,
) {
    for state in pane_parsers.values_mut() {
        // `block_meta` is the authoritative set of in-progress blocks: every
        // CommandStart inserts here. `writers` is a SUBSET — a block whose
        // `BlobWriter::open` failed has meta but no writer. Drain from
        // `block_meta` so those writer-less blocks are still finalized;
        // otherwise they stay `ended_at IS NULL` forever (ghost "running"
        // block). See `finalize_open_blocks_finalizes_writerless_block`.
        let open_blocks: Vec<pyre_proto::BlockId> = state.block_meta.keys().cloned().collect();
        for block in open_blocks {
            let stdout_len = if let Some(bw) = state.writers.remove(&block) {
                tokio::task::spawn_blocking(move || bw.close().unwrap_or(0))
                    .await
                    .unwrap_or(0)
            } else {
                0
            };
            // IGNORED: finalize_block error on shutdown drain is best-effort;
            // acceptable data loss during process teardown.
            let _ = store
                .finalize_block(block, Utc::now(), None, stdout_len)
                .await;
            let stdout_text = state
                .stdout_bufs
                .remove(&block)
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or_default();
            if let Some(meta) = state.block_meta.remove(&block) {
                let idx = block_index.clone();
                tokio::task::spawn_blocking(move || {
                    // IGNORED: Tantivy index error on shutdown drain is best-effort;
                    // the block is already persisted in SQLite.
                    let _ = idx.add_block(&meta, &stdout_text);
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Heartbeat monitor + SIGCHLD handler
// ---------------------------------------------------------------------------

async fn heartbeat_monitor(registry: Arc<WorkerRegistry>, supervisor_impl: SupervisorImpl) {
    let timeout = Duration::from_secs(15);
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        let stale = registry.stale_sessions(timeout).await;
        for session_id in stale {
            tracing::warn!(session_id, "worker heartbeat timeout — respawning");
            registry.remove(&session_id).await;
            if let Err(e) = supervisor_impl.spawn_worker(&session_id).await {
                tracing::error!(session_id, "respawn failed: {e:#}");
            }
        }
    }
}

fn start_sigchld_handler(registry: Arc<WorkerRegistry>, supervisor_impl: SupervisorImpl) {
    tokio::spawn(async move {
        let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .expect("register SIGCHLD handler");
        loop {
            sigchld.recv().await;
            loop {
                // SAFETY: waitpid(-1, NULL, WNOHANG) is async-signal-safe and
                // only reaps children we spawned. No other thread calls waitpid.
                let res = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
                if res <= 0 {
                    break;
                }
                let pid = res as u32;
                tracing::info!(pid, "worker exited (SIGCHLD)");
                if let Some(session_id) = registry.session_for_pid(pid).await {
                    // Check pane count before removing from registry.
                    // If all panes have already been closed via pane_closed
                    // RPC, the pane index will be empty — this was a voluntary
                    // clean exit, not a crash. Do not respawn.
                    let panes_remaining = registry.pane_count(&session_id).await;
                    registry.remove(&session_id).await;
                    if panes_remaining == 0 {
                        tracing::info!(
                            session_id,
                            "worker exited cleanly (no panes left) — not respawning"
                        );
                    } else {
                        tracing::info!(session_id, "respawning worker after exit");
                        if let Err(e) = supervisor_impl.spawn_worker(&session_id).await {
                            tracing::error!(session_id, "respawn failed: {e:#}");
                        }
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

async fn connect_worker_ctrl(sock_path: &PathBuf) -> Result<WorkerControlClient> {
    // Retry with backoff to give the worker time to bind its socket.
    for attempt in 0..10u32 {
        match UnixStream::connect(sock_path).await {
            Ok(stream) => {
                let transport = tarpc::serde_transport::new(
                    Framed::new(stream, LengthDelimitedCodec::new()),
                    Bincode::default(),
                );
                return Ok(WorkerControlClient::new(client::Config::default(), transport).spawn());
            }
            Err(_) if attempt < 9 => {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt + 1))).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

fn runtime_dir() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt);
    }
    // SAFETY: getuid() is always safe.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}"))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the supervisor: bind sockets, start background tasks, accept loop.
pub async fn run(
    public_sock: PathBuf,
    store: Arc<Store>,
    block_index: Arc<BlockIndex>,
) -> Result<()> {
    let rt_pyre = runtime_dir().join("pyre");
    std::fs::create_dir_all(&rt_pyre).with_context(|| format!("mkdir {}", rt_pyre.display()))?;

    let supervisor_sock = rt_pyre.join("supervisor.sock");
    if supervisor_sock.exists() {
        // IGNORED: stale socket cleanup failure is non-fatal; UnixListener::bind
        // will fail with a clear error if the file truly cannot be replaced.
        let _ = std::fs::remove_file(&supervisor_sock);
    }

    let (event_tx, event_rx) = mpsc::channel::<BlockEvent>(4096);
    let registry = Arc::new(WorkerRegistry::new());
    let mirror_registry = PaneMirrorRegistry::new();

    let pending_registrations: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let focus_queue: Arc<std::sync::Mutex<std::collections::VecDeque<PaneId>>> =
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

    let pane_event_bus = PaneEventBus::new();

    let window_store: Arc<Mutex<HashMap<WindowId, WindowMeta>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let session_windows: Arc<Mutex<HashMap<String, Vec<WindowId>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pane_window_map: Arc<Mutex<HashMap<PaneId, WindowId>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let spawn_cwds: Arc<Mutex<HashMap<String, PathBuf>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let supervisor_impl = SupervisorImpl {
        registry: registry.clone(),
        store: store.clone(),
        block_index: block_index.clone(),
        event_tx: event_tx.clone(),
        supervisor_sock: supervisor_sock.clone(),
        pending_registrations: pending_registrations.clone(),
        mirror_registry: mirror_registry.clone(),
        focus_queue: focus_queue.clone(),
        pane_event_bus: pane_event_bus.clone(),
        window_store: window_store.clone(),
        session_windows: session_windows.clone(),
        pane_window_map: pane_window_map.clone(),
        spawn_cwds: spawn_cwds.clone(),
    };

    // Bind the supervisor callback socket (workers dial here to register).
    let sw_listener = UnixListener::bind(&supervisor_sock)
        .with_context(|| format!("bind supervisor sock {}", supervisor_sock.display()))?;
    std::fs::set_permissions(&supervisor_sock, std::fs::Permissions::from_mode(0o700))?;
    tracing::info!(
        "supervisor callback socket at {}",
        supervisor_sock.display()
    );

    tokio::spawn(block_event_batcher(
        event_rx,
        block_index.clone(),
        store.clone(),
        registry.clone(),
    ));
    tokio::spawn(heartbeat_monitor(registry.clone(), supervisor_impl.clone()));
    start_sigchld_handler(registry.clone(), supervisor_impl.clone());

    // Accept loop for worker → supervisor callbacks (SupervisorWorker trait).
    {
        let sw_registry = registry.clone();
        let sw_event_tx = event_tx.clone();
        let sw_pending = pending_registrations.clone();
        let sw_pane_event_bus = pane_event_bus.clone();
        let sw_window_store = window_store.clone();
        let sw_pane_window_map = pane_window_map.clone();
        let sw_store = store.clone();
        tokio::spawn(async move {
            loop {
                match sw_listener.accept().await {
                    Ok((sock, _)) => {
                        let sw_impl = SupervisorWorkerImpl {
                            registry: sw_registry.clone(),
                            event_tx: sw_event_tx.clone(),
                            pending_registrations: sw_pending.clone(),
                            pane_event_bus: sw_pane_event_bus.clone(),
                            window_store: sw_window_store.clone(),
                            pane_window_map: sw_pane_window_map.clone(),
                            store: sw_store.clone(),
                        };
                        let transport = tarpc::serde_transport::new(
                            Framed::new(sock, LengthDelimitedCodec::new()),
                            Bincode::default(),
                        );
                        tokio::spawn(
                            BaseChannel::with_defaults(transport)
                                .execute(sw_impl.serve())
                                .for_each(|f| async move {
                                    tokio::spawn(f);
                                }),
                        );
                    }
                    Err(e) => {
                        tracing::error!("supervisor callback accept error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // Bind the public socket BEFORE reattach so clients can connect immediately
    // and a `spawn` RPC never starves while reattach runs. Reattach is then
    // spawned as a background task; the accept loop below can serve requests
    // while persisted sessions are still being restored (the desired behavior —
    // a fresh `spawn` no longer waits behind hundreds of reattaching shards).
    let listener = UnixListener::bind(&public_sock)
        .with_context(|| format!("bind {}", public_sock.display()))?;
    std::fs::set_permissions(&public_sock, std::fs::Permissions::from_mode(0o700))?;
    tracing::info!("supervisor public socket at {}", public_sock.display());

    // Re-attach persisted sessions: query SQLite for all known sessions, then
    // spawn a worker only for the LIVE ones (≥1 pane in their shard). Workers
    // can't restore the old PTY processes, but a session whose shard still has
    // panes will re-open shells via WorkerShard::load_panes and appear in
    // list_sessions. A session whose shard has 0 panes is STALE (invariants
    // I-4 / I-5): reattaching it produces a 0-PTY ghost that the TUI then
    // treats as a lost session. Such shards are skipped and GC'd here.
    {
        let reattach_store = store.clone();
        let reattach_impl = supervisor_impl.clone();
        let reattach_pending = pending_registrations.clone();
        tokio::spawn(async move {
            let persisted = reattach_store.list_session_ids().await.unwrap_or_else(|e| {
                tracing::warn!("reattach: list_session_ids: {e:#}");
                vec![]
            });

            // Partition persisted sessions into live (≥1 pane) and stale (0).
            // The shard's `panes` table — not the supervisor store, which is
            // empty in hybrid mode — is the source of truth for liveness.
            let mut live: Vec<SessionId> = Vec::new();
            let mut stale: Vec<SessionId> = Vec::new();
            for sid in persisted {
                let session_id_str = sid.0.to_string();
                match crate::shard::shard_pane_count(&session_id_str).await {
                    Ok(0) => stale.push(sid),
                    Ok(_) => live.push(sid),
                    Err(e) => {
                        // A shard we cannot introspect is left alone (neither
                        // reattached nor deleted) — never GC on uncertainty.
                        tracing::warn!(
                            session_id = session_id_str,
                            "reattach: shard pane-count failed, skipping (no GC): {e:#}"
                        );
                    }
                }
            }

            // Startup GC: prune the stale shard dirs. Defensive — only dirs we
            // just confirmed have 0 panes, and never a session that is already
            // live/registered (stale sessions have no worker yet by definition).
            let mut pruned = 0usize;
            for sid in &stale {
                let session_id_str = sid.0.to_string();
                if reattach_impl
                    .registry
                    .get_ctrl_client(&session_id_str)
                    .await
                    .is_some()
                {
                    // Race guard: a client spawned into this id between the
                    // pane-count read and now. Leave it.
                    continue;
                }
                match crate::shard::remove_shard_dir(&session_id_str) {
                    Ok(()) => pruned += 1,
                    Err(e) => tracing::warn!(
                        session_id = session_id_str,
                        "reattach GC: remove shard dir failed: {e:#}"
                    ),
                }
            }
            if pruned > 0 {
                tracing::info!(pruned, stale = stale.len(), "pruned stale (0-pane) shards");
            }

            let count = live.len();
            if count > 0 {
                tracing::info!(count, "reattaching persisted sessions");
            }
            for sid in live {
                let session_id_str = sid.0.to_string();
                // Insert a oneshot so spawn_worker can await registration.
                let (reg_tx, reg_rx) = oneshot::channel::<()>();
                reattach_pending
                    .lock()
                    .await
                    .insert(session_id_str.clone(), reg_tx);

                if let Err(e) = reattach_impl.spawn_worker(&session_id_str).await {
                    tracing::warn!(session_id = session_id_str, "reattach: spawn_worker: {e:#}");
                    reattach_pending.lock().await.remove(&session_id_str);
                    continue;
                }

                // Await registration with a 5 s timeout — same window as spawn RPC.
                match tokio::time::timeout(Duration::from_secs(5), reg_rx).await {
                    Ok(Ok(())) => {
                        tracing::info!(session_id = session_id_str, "reattached persisted session");
                    }
                    Ok(Err(_)) => {
                        tracing::warn!(
                            session_id = session_id_str,
                            "reattach: registration channel closed"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            session_id = session_id_str,
                            "reattach: worker did not register within 5 s"
                        );
                        reattach_pending.lock().await.remove(&session_id_str);
                    }
                }
            }
        });
    }

    let shutdown_public = public_sock.clone();
    let shutdown_sv = supervisor_sock.clone();
    let shutdown = async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("SIGINT");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("supervisor: SIGTERM"),
            _ = sigint.recv()  => tracing::info!("supervisor: SIGINT"),
        }
        // IGNORED: socket cleanup on shutdown; if removal fails the next
        // startup will retry removal before binding (see supervisor_sock cleanup above).
        let _ = std::fs::remove_file(&shutdown_public);
        let _ = std::fs::remove_file(&shutdown_sv);
    };

    let accept_loop = async {
        loop {
            match listener.accept().await {
                Ok((sock, _)) => {
                    let si = supervisor_impl.clone();
                    let st = store.clone();
                    let bi = block_index.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_public_conn(sock, si, st, bi).await {
                            tracing::warn!("supervisor conn error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("accept error: {e}");
                    break;
                }
            }
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = shutdown => {},
    }
    Ok(())
}

/// Proxy a MODE_STREAM connection from a TUI client to the appropriate pane.
///
/// Wire format after the MODE_STREAM tag byte:
///   16 bytes — SessionId (UUID bytes)
///   16 bytes — PaneId   (UUID bytes)
///
/// Multi-client architecture:
///   * A `PaneMirrorHub` is created (or reused) per pane, holding a single
///     connection to the worker's stream UDS.
///   * Output from the worker is broadcast to all subscribed TUI clients.
///   * Input from each TUI client is forwarded to a shared `mpsc` channel
///     whose single drainer writes to the worker in order, preventing
///     keystroke interleaving across concurrent clients.
async fn proxy_stream_to_worker(
    mut client_sock: UnixStream,
    registry: Arc<WorkerRegistry>,
    mirror_registry: PaneMirrorRegistry,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    // Read session id (16 bytes) + pane id (16 bytes) from the client.
    let mut session_buf = [0u8; 16];
    client_sock
        .read_exact(&mut session_buf)
        .await
        .context("read session id")?;
    let session_id = uuid::Uuid::from_bytes(session_buf).to_string();

    let mut pane_buf = [0u8; 16];
    client_sock
        .read_exact(&mut pane_buf)
        .await
        .context("read pane id")?;
    let pane_uuid = uuid::Uuid::from_bytes(pane_buf);

    // Resolve slot_idx before potentially creating the hub.
    let slot_idx = match registry.lookup_pane(pane_uuid).await {
        Some((_, s)) => s,
        None => {
            tracing::warn!(%pane_uuid, "MODE_STREAM: unknown pane uuid");
            // IGNORED: shutdown error on an already-closing connection is harmless.
            let _ = client_sock.shutdown().await;
            return Ok(());
        }
    };

    // Look up the worker's stream socket path.
    let stream_sock_path = match registry.get_stream_sock(&session_id).await {
        Some(p) => p,
        None => {
            tracing::warn!(session_id, "MODE_STREAM: no worker registered for session");
            // IGNORED: shutdown error on an already-closing connection is harmless.
            let _ = client_sock.shutdown().await;
            return Ok(());
        }
    };

    // Resolve or create the PaneMirrorHub for this pane.
    let hub: Arc<PaneMirrorHub> = if let Some(existing) = mirror_registry.get(pane_uuid).await {
        existing
    } else {
        // First client for this pane — open a single worker connection and start
        // the background output-reader + input-drainer tasks.
        let mut worker_sock = match UnixStream::connect(&stream_sock_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%pane_uuid, "MODE_STREAM: connect worker stream sock: {e}");
                // IGNORED: shutdown error on an already-closing connection is harmless.
                let _ = client_sock.shutdown().await;
                return Ok(());
            }
        };
        worker_sock
            .write_all(&slot_idx.to_le_bytes())
            .await
            .context("write slot_idx to worker")?;

        // Output broadcast channel: capacity 256 (matches worker's broadcast cap).
        let (out_tx, _) = broadcast::channel::<Bytes>(256);
        // Input serialisation channel.
        let (in_tx, mut in_rx) = mpsc::channel::<Bytes>(256);

        let last_snapshot: Arc<Mutex<Bytes>> = Arc::new(Mutex::new(Bytes::new()));

        let hub = Arc::new(PaneMirrorHub {
            output_tx: out_tx.clone(),
            input_tx: in_tx,
            last_snapshot: last_snapshot.clone(),
        });
        mirror_registry.insert(pane_uuid, hub.clone()).await;

        let (worker_rd, worker_wr) = worker_sock.into_split();
        let frame_read = FramedRead::new(worker_rd, LengthDelimitedCodec::new());
        let frame_write = FramedWrite::new(worker_wr, LengthDelimitedCodec::new());
        let mut worker_out: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
            tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
        let mut worker_in: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
            tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

        // Worker → broadcast: read OutputFrames from worker, fan out raw bytes.
        // All output bytes are appended to last_snapshot (capped at 256 KiB) so
        // that late-arriving TUI clients receive a synthetic seq=0 frame with the
        // accumulated terminal state before subscribing to live output.
        let mirror_reg_cleanup = mirror_registry.clone();
        const SNAPSHOT_CAP: usize = 256 * 1024;
        tokio::spawn(async move {
            while let Some(frame) = worker_out.next().await {
                match frame {
                    Ok(f) => {
                        // Append to accumulated snapshot, dropping the oldest bytes
                        // when we exceed the cap to bound memory.
                        {
                            let mut snap = last_snapshot.lock().await;
                            let new_len = snap.len() + f.data.len();
                            if new_len <= SNAPSHOT_CAP {
                                let mut v = snap.to_vec();
                                v.extend_from_slice(&f.data);
                                *snap = Bytes::from(v);
                            } else {
                                // Keep only the most recent SNAPSHOT_CAP bytes.
                                let mut v = snap.to_vec();
                                v.extend_from_slice(&f.data);
                                let drop_n = v.len().saturating_sub(SNAPSHOT_CAP);
                                *snap = Bytes::from(v[drop_n..].to_vec());
                            }
                        }
                        // IGNORED: broadcast::send error means all subscribers have
                        // temporarily disconnected; the hub stays alive and new
                        // subscribers will receive output from the ring snapshot.
                        let _ = out_tx.send(f.data);
                    }
                    Err(e) => {
                        tracing::debug!(%pane_uuid, "worker output stream ended: {e}");
                        break;
                    }
                }
            }
            // Worker closed — remove the hub so the next client gets a fresh one.
            mirror_reg_cleanup.remove(pane_uuid).await;
            tracing::debug!(%pane_uuid, "mirror hub removed (worker disconnected)");
        });

        // Input serialiser: drain the shared mpsc channel, write InputFrames to worker.
        let session_uuid = match uuid::Uuid::parse_str(&session_id) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("MODE_STREAM: invalid session uuid: {e}");
                return Ok(());
            }
        };
        let session = SessionId(session_uuid);
        tokio::spawn(async move {
            while let Some(data) = in_rx.recv().await {
                if worker_in.send(InputFrame { session, data }).await.is_err() {
                    break;
                }
            }
        });

        hub
    };

    // Snapshot the current ring-buffer state and subscribe to live output
    // atomically: subscribe first (so we don't miss bytes), then read snapshot.
    // This mirrors the order used in worker.rs handle_stream_conn.
    let mut out_sub = hub.output_tx.subscribe();
    let snap = hub.last_snapshot.lock().await.clone();
    let input_tx = hub.input_tx.clone();

    // Resolve a SessionId for wrapping OutputFrames sent to this client.
    let session_uuid = match uuid::Uuid::parse_str(&session_id) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("MODE_STREAM: invalid session uuid in client path: {e}");
            return Ok(());
        }
    };
    let session = SessionId(session_uuid);

    // Split the client socket into framed halves.
    let (client_rd, client_wr) = client_sock.into_split();
    let frame_read = FramedRead::new(client_rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(client_wr, LengthDelimitedCodec::new());
    let mut client_in: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut client_out: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // Client output task: replay snapshot as seq=0, then forward live broadcast frames.
    let out_task = tokio::spawn(async move {
        // Always send the snapshot first (seq=0), even if empty — uniform client path.
        if client_out
            .send(OutputFrame {
                session,
                seq: 0,
                data: snap,
            })
            .await
            .is_err()
        {
            return;
        }
        let mut seq: u64 = 0;
        loop {
            match out_sub.recv().await {
                Ok(data) => {
                    seq = seq.wrapping_add(1);
                    if client_out
                        .send(OutputFrame { session, seq, data })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%pane_uuid, n, "client output broadcast lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Client input task: this client's socket → shared serialised mpsc channel.
    let in_task = tokio::spawn(async move {
        while let Some(frame) = client_in.next().await {
            match frame {
                Ok(f) => {
                    if input_tx.send(f.data).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(%pane_uuid, "client input frame error: {e}");
                    break;
                }
            }
        }
    });

    // IGNORED: JoinHandle results from out_task/in_task; both tasks log their
    // own errors and a task panic here is unrecoverable per-connection only —
    // the supervisor process itself continues serving other connections.
    let _ = tokio::join!(out_task, in_task);
    Ok(())
}

async fn handle_public_conn(
    sock: UnixStream,
    supervisor_impl: SupervisorImpl,
    _store: Arc<Store>,
    _block_index: Arc<BlockIndex>,
) -> Result<()> {
    use pyre_proto::{read_control_version_after_tag, MODE_CONTROL, MODE_STREAM, PROTO_VERSION};

    let mut sock = sock;
    let mut tag = [0u8; 1];
    sock.read_exact(&mut tag).await.context("read mode tag")?;

    match tag[0] {
        MODE_CONTROL => {
            read_control_version_after_tag(&mut sock)
                .await
                .with_context(|| format!("control handshake (proto_version={PROTO_VERSION})"))?;
            let transport = tarpc::serde_transport::new(
                Framed::new(sock, LengthDelimitedCodec::new()),
                Bincode::default(),
            );
            BaseChannel::with_defaults(transport)
                .execute(supervisor_impl.serve())
                .for_each(|f| async move {
                    tokio::spawn(f);
                })
                .await;
            Ok(())
        }
        MODE_STREAM => {
            proxy_stream_to_worker(
                sock,
                supervisor_impl.registry,
                supervisor_impl.mirror_registry,
            )
            .await
        }
        other => anyhow::bail!("unknown mode tag {other:#04x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::ENV_TEST_LOCK as ENV_LOCK;
    use tempfile::TempDir;
    use uuid::Uuid;

    /// Teardown invariant: on the shutdown drain, EVERY in-progress block is
    /// finalized (ended_at set) — including a block whose `BlobWriter::open`
    /// failed at `CommandStart`. Such a block lives in `block_meta`/`stdout_bufs`
    /// but NOT in `writers`. If the drain only walks `writers`, that block stays
    /// `ended_at IS NULL` forever — a ghost "running" block. The authoritative
    /// set of open blocks is `block_meta`, not `writers`.
    #[tokio::test]
    async fn finalize_open_blocks_finalizes_writerless_block() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation, serialized by ENV_LOCK (shared with
        // the store/shard/migration tests that mutate process-global env).
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Arc::new(Store::open().await?);
        let block_index = Arc::new(BlockIndex::open(&tmp.path().join("index"))?);

        let sid = SessionId(Uuid::new_v4());
        let pid = PaneId(Uuid::new_v4());
        let bid = BlockId(Uuid::new_v4());
        store.upsert_session(sid, "test").await?;
        store.upsert_pane(pid, sid, "/bin/sh", None, 80, 24).await?;

        // CommandStart succeeded in creating the block (ended_at = NULL) but the
        // blob-open failed, so no writer was inserted for it.
        let block = Block {
            id: bid,
            pane: pid,
            session: sid,
            command: "long-running".into(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            stdout_len: 0,
        };
        store.create_block(&block).await?;

        // Precondition: the block is a ghost (in-progress, ended_at NULL).
        let before = store.list_blocks(Some(sid), 10).await?;
        assert_eq!(before.len(), 1);
        assert!(
            before[0].ended_at.is_none(),
            "precondition: block must start in-progress"
        );

        // Parser state shaped like a blob-open failure: present in block_meta +
        // stdout_bufs, absent from writers.
        let mut state = PaneParserState::new(sid);
        state.block_meta.insert(bid, block.clone());
        state.stdout_bufs.insert(bid, Vec::new());
        let mut pane_parsers: HashMap<(String, u32), PaneParserState> = HashMap::new();
        pane_parsers.insert((sid.0.to_string(), 0), state);

        // Drain (the shutdown path that runs when the event channel closes).
        finalize_open_blocks(&mut pane_parsers, &store, &block_index).await;

        // Invariant: no ghost block survives the drain.
        let after = store.list_blocks(Some(sid), 10).await?;
        assert_eq!(after.len(), 1);
        assert!(
            after[0].ended_at.is_some(),
            "block must be finalized on drain even without a BlobWriter — no ghost running block"
        );
        Ok(())
    }
}
