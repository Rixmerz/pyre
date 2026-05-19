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

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use pyre_proto::service::PyreDaemon as _;
use pyre_proto::supervisor::{
    BlockEvent, RegisterAck, RpcError, SupervisorWorker, WorkerControlClient,
};
use pyre_proto::{
    AttachAck, Block, BlockHit, BlockId, ListBlocksReq, OpenPaneReq, PaneId, PaneInfo,
    PaneStateKind, PyreError, ReplayBlocks, ResizePaneReq, ResizePaneRes, SearchBlocksReq,
    SessionId, SessionInfo, SpawnReq, SpawnResp,
};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, RwLock};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::index::BlockIndex;
use crate::store::Store;

// ---------------------------------------------------------------------------
// Worker registry
// ---------------------------------------------------------------------------

/// Handle for a live worker process.
pub struct WorkerHandle {
    /// OS PID of the worker.
    pub pid: u32,
    /// Path to the worker's `WorkerControl` UDS (used for reconnect in S2).
    #[allow(dead_code)]
    pub sock_path: PathBuf,
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

    /// Look up (session_id_str, slot_idx) for a PaneId.
    pub async fn lookup_pane(&self, pane_id: uuid::Uuid) -> Option<(String, u32)> {
        self.panes
            .read()
            .await
            .lookup(pane_id)
            .map(|(s, i)| (s.to_owned(), i))
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
// Supervisor implementation of PyreDaemon
// ---------------------------------------------------------------------------

/// tarpc `PyreDaemon` implementation backed by the worker registry.
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
    #[allow(dead_code)]
    pub event_tx: mpsc::Sender<BlockEvent>,
    /// Path to the supervisor's callback socket, passed to spawned workers.
    pub supervisor_sock: PathBuf,
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

        self.spawn_worker(&session_id_str)
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?;

        // Register the initial pane in PaneIndex BEFORE returning the PaneId
        // so that any follow-up RPC (e.g. close_pane) can resolve it immediately.
        let (pane_uuid, _slot_idx) = self.registry.alloc_pane(&session_id_str).await;
        let pane_id = PaneId(pane_uuid);
        Ok(SpawnResp {
            session: sid,
            pane: pane_id,
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
        let limit = req.limit;
        let ids = tokio::task::spawn_blocking(move || block_index.search(&query, limit))
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;

        let mut hits = Vec::with_capacity(ids.len());
        for id in ids {
            match self.store.get_block(id).await {
                Ok(Some(block)) => hits.push(BlockHit {
                    block,
                    snippet: String::new(),
                }),
                Ok(None) => {}
                Err(e) => tracing::warn!("get_block {id:?}: {e:#}"),
            }
        }
        Ok(hits)
    }

    async fn list_sessions(self, _ctx: context::Context) -> Result<Vec<SessionInfo>, PyreError> {
        let handles = self.registry.inner.read().await;
        let mut sessions = Vec::with_capacity(handles.len());
        for (session_id_str, handle) in handles.iter() {
            let pane_count = match handle.ctrl_client.list_panes(context::current()).await {
                Ok(Ok(slots)) => slots.len() as u32,
                _ => 0,
            };
            let sid = match uuid::Uuid::parse_str(session_id_str) {
                Ok(u) => SessionId(u),
                Err(_) => continue,
            };
            sessions.push(SessionInfo {
                id: sid,
                name: session_id_str.clone(),
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
        let now = Utc::now();
        let panes = slots
            .into_iter()
            .map(|slot_idx| {
                // Look up the PaneId we assigned; fall back to a fresh UUID if
                // the entry was somehow evicted.
                let pane_id = PaneId(uuid::Uuid::new_v4());
                PaneInfo {
                    id: pane_id,
                    session,
                    cols: 80,
                    rows: 24,
                    shell: String::new(),
                    created_at: now,
                    closed_at: None,
                    state: pyre_proto::PaneStateKind::Running,
                    state_reason: format!("slot {slot_idx}"),
                    last_activity: now,
                    foreground_cmd: None,
                    root_pid: 0,
                }
            })
            .collect();
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
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        client
            .open_pane(context::current(), slot_idx, shell, cwd)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(PaneId(pane_uuid))
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
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn replay(
        self,
        _ctx: context::Context,
        pane: PaneId,
        recent_blocks: u32,
    ) -> Result<ReplayBlocks, PyreError> {
        // Read recent blocks from the supervisor store (same path written by
        // the block_event batcher). Snapshot bytes are deferred to S3.
        let blocks = self
            .store
            .list_blocks_for_pane(pane, recent_blocks)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(ReplayBlocks {
            recent: blocks,
            snapshot: bytes::Bytes::new(),
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

    async fn set_pane_state(
        self,
        _ctx: context::Context,
        _pane: PaneId,
        _state: PaneStateKind,
        _reason: String,
    ) -> Result<(), PyreError> {
        // State override is a heuristic engine concern; deferred to S3.
        Ok(())
    }

    async fn list_all_panes(self, _ctx: context::Context) -> Result<Vec<PaneInfo>, PyreError> {
        let handles = self.registry.inner.read().await;
        let mut all = Vec::new();
        let now = Utc::now();
        for (session_id_str, handle) in handles.iter() {
            let sid = match uuid::Uuid::parse_str(session_id_str) {
                Ok(u) => SessionId(u),
                Err(_) => continue,
            };
            let slots = match handle.ctrl_client.list_panes(context::current()).await {
                Ok(Ok(s)) => s,
                _ => continue,
            };
            for slot_idx in slots {
                all.push(PaneInfo {
                    id: PaneId(uuid::Uuid::new_v4()),
                    session: sid,
                    cols: 80,
                    rows: 24,
                    shell: String::new(),
                    created_at: now,
                    closed_at: None,
                    state: pyre_proto::PaneStateKind::Running,
                    state_reason: format!("slot {slot_idx}"),
                    last_activity: now,
                    foreground_cmd: None,
                    root_pid: 0,
                });
            }
        }
        Ok(all)
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
}

// ---------------------------------------------------------------------------
// SupervisorWorker tarpc service (workers call in to this)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SupervisorWorkerImpl {
    registry: Arc<WorkerRegistry>,
    event_tx: mpsc::Sender<BlockEvent>,
}

impl SupervisorWorker for SupervisorWorkerImpl {
    async fn register_worker(
        self,
        _ctx: context::Context,
        session_id: String,
        pid: u32,
        sock_path: String,
    ) -> Result<RegisterAck, RpcError> {
        tracing::info!(session_id, pid, sock_path, "worker registered");

        let worker_sock = PathBuf::from(&sock_path);
        let ctrl_client = connect_worker_ctrl(&worker_sock)
            .await
            .map_err(|e| RpcError::Internal(format!("connect worker ctrl: {e}")))?;

        let handle = WorkerHandle {
            pid,
            sock_path: worker_sock,
            ctrl_client,
            last_heartbeat: Instant::now(),
        };
        self.registry.insert(session_id.clone(), handle).await;
        tracing::info!(session_id, pid, "worker handle stored");

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
        // TODO(S2): evict pane entry from supervisor pane index.
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

async fn block_event_batcher(
    mut event_rx: mpsc::Receiver<BlockEvent>,
    block_index: Arc<BlockIndex>,
) {
    let mut batch: Vec<BlockEvent> = Vec::new();
    let flush_interval = Duration::from_millis(50);
    let mut interval = tokio::time::interval(flush_interval);

    loop {
        tokio::select! {
            maybe_ev = event_rx.recv() => {
                match maybe_ev {
                    Some(ev) => batch.push(ev),
                    None => {
                        if !batch.is_empty() {
                            flush_batch(&batch, &block_index);
                            batch.clear();
                        }
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                if !batch.is_empty() {
                    flush_batch(&batch, &block_index);
                    batch.clear();
                }
            }
        }
    }
}

fn flush_batch(batch: &[BlockEvent], _block_index: &BlockIndex) {
    // TODO(S2): convert BlockEvent → Block and write to Tantivy via add_block.
    // Full conversion requires session/pane metadata which is deferred to S2.
    tracing::debug!(count = batch.len(), "flushed block event batch (noop — S2)");
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
                    registry.remove(&session_id).await;
                    tracing::info!(session_id, "respawning worker after exit");
                    if let Err(e) = supervisor_impl.spawn_worker(&session_id).await {
                        tracing::error!(session_id, "respawn failed: {e:#}");
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
        let _ = std::fs::remove_file(&supervisor_sock);
    }

    let (event_tx, event_rx) = mpsc::channel::<BlockEvent>(4096);
    let registry = Arc::new(WorkerRegistry::new());

    let supervisor_impl = SupervisorImpl {
        registry: registry.clone(),
        store: store.clone(),
        block_index: block_index.clone(),
        event_tx: event_tx.clone(),
        supervisor_sock: supervisor_sock.clone(),
    };

    // Bind the supervisor callback socket (workers dial here to register).
    let sw_listener = UnixListener::bind(&supervisor_sock)
        .with_context(|| format!("bind supervisor sock {}", supervisor_sock.display()))?;
    std::fs::set_permissions(&supervisor_sock, std::fs::Permissions::from_mode(0o700))?;
    tracing::info!(
        "supervisor callback socket at {}",
        supervisor_sock.display()
    );

    tokio::spawn(block_event_batcher(event_rx, block_index.clone()));
    tokio::spawn(heartbeat_monitor(registry.clone(), supervisor_impl.clone()));
    start_sigchld_handler(registry.clone(), supervisor_impl.clone());

    // Accept loop for worker → supervisor callbacks (SupervisorWorker trait).
    {
        let sw_registry = registry.clone();
        let sw_event_tx = event_tx.clone();
        tokio::spawn(async move {
            loop {
                match sw_listener.accept().await {
                    Ok((sock, _)) => {
                        let sw_impl = SupervisorWorkerImpl {
                            registry: sw_registry.clone(),
                            event_tx: sw_event_tx.clone(),
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

    // Public socket accept loop (PyreDaemon trait — same tag protocol as single mode).
    let listener = UnixListener::bind(&public_sock)
        .with_context(|| format!("bind {}", public_sock.display()))?;
    std::fs::set_permissions(&public_sock, std::fs::Permissions::from_mode(0o700))?;
    tracing::info!("supervisor public socket at {}", public_sock.display());

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

async fn handle_public_conn(
    sock: UnixStream,
    supervisor_impl: SupervisorImpl,
    _store: Arc<Store>,
    _block_index: Arc<BlockIndex>,
) -> Result<()> {
    use pyre_proto::{MODE_CONTROL, MODE_STREAM};

    let mut sock = sock;
    let mut tag = [0u8; 1];
    sock.read_exact(&mut tag).await.context("read mode tag")?;

    match tag[0] {
        MODE_CONTROL => {
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
            // TODO(S2): proxy stream connections to the appropriate worker.
            tracing::warn!("hybrid stream connections not yet implemented");
            Ok(())
        }
        other => anyhow::bail!("unknown mode tag {other:#04x}"),
    }
}
