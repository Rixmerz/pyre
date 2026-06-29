//! Supervisor ↔ Worker RPC surface (tarpc services).
//!
//! Two separate service traits are defined:
//!
//! * `SupervisorWorker` — calls **from** the worker **to** the supervisor.
//!   Workers register themselves, stream `BlockEvent`s, and send lifecycle
//!   signals over this interface.
//!
//! * `WorkerControl` — calls **from** the supervisor **to** the worker.
//!   Supervisor drives PTY operations and graceful shutdown through this
//!   interface.
//!
//! Both services run over the per-session private UDS
//! `$XDG_RUNTIME_DIR/pyre/session-<session_id>.sock` (mode 0700).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Classification of the bytes carried in a [`BlockEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Raw stdout bytes from the PTY.
    Stdout,
    /// Raw stderr bytes (when the shell separates stderr; otherwise folded into `Stdout`).
    Stderr,
    /// Shell prompt detected by the VTE parser.
    Prompt,
    /// Arbitrary user-defined marker injected by the client.
    Marker,
}

/// A single output event streamed from a worker to the supervisor.
///
/// Workers emit these fire-and-forget; the supervisor batches them internally
/// before committing to the Tantivy index (every ~50 ms).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEvent {
    /// UUID string of the session that owns this event.
    pub session_id: String,
    /// Zero-based slot index of the pane within the session.
    pub slot_idx: u32,
    /// Classification of the payload bytes.
    pub kind: BlockKind,
    /// Raw bytes from the PTY (may be empty for `Prompt` / `Marker`).
    pub bytes: Vec<u8>,
    /// Unix epoch milliseconds at the time of capture.
    pub ts_ms: u64,
}

/// Acknowledgement returned to the worker after a successful `register_worker` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAck {
    /// `true` when the supervisor's aggregated Tantivy index is already
    /// warm and ready to serve queries for this session.
    pub aggregated_index_ready: bool,
}

/// Unified RPC error type for both service traits.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum RpcError {
    #[error("unknown session: {0}")]
    UnknownSession(String),
    #[error("unknown pane slot: {0}")]
    UnknownSlot(u32),
    #[error("internal: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Worker → Supervisor service
// ---------------------------------------------------------------------------

/// Calls made by a worker process to the supervisor.
///
/// The worker connects to the supervisor's private UDS and calls these
/// methods to register itself and send lifecycle signals.
#[tarpc::service]
pub trait SupervisorWorker {
    /// Announce a new worker process to the supervisor.
    ///
    /// `session_id`       — UUID string of the session this worker owns.
    /// `pid`              — OS PID of the worker process.
    /// `sock_path`        — Absolute path to the worker's own `WorkerControl` UDS.
    /// `stream_sock_path` — Absolute path to the worker's raw-stream UDS for
    ///                      bidirectional PTY byte proxying.
    async fn register_worker(
        session_id: String,
        pid: u32,
        sock_path: String,
        stream_sock_path: String,
    ) -> Result<RegisterAck, RpcError>;

    /// Deliver a single output event to the supervisor (fire-and-forget style).
    ///
    /// The supervisor acknowledges receipt but the worker does not wait for
    /// the Tantivy commit — batching and durability are supervisor concerns.
    async fn block_event(event: BlockEvent) -> Result<(), RpcError>;

    /// Notify the supervisor that a pane slot has been closed.
    ///
    /// The supervisor uses this to evict the shard entry and update its
    /// in-memory registry.
    async fn pane_closed(session_id: String, slot_idx: u32) -> Result<(), RpcError>;

    /// Notify the supervisor that a pane's state has changed.
    ///
    /// The supervisor resolves the stable PaneId and emits a `StateChanged`
    /// event into its `PaneEventBus` so MCP clients on the hybrid daemon
    /// receive the same live state events as single-mode clients.
    async fn pane_state_changed(
        session_id: String,
        slot_idx: u32,
        state: crate::PaneStateKind,
    ) -> Result<(), RpcError>;

    /// Liveness signal sent by the worker every 5 s.
    ///
    /// Supervisor times out at 15 s and triggers a forced respawn if no
    /// heartbeat arrives within the window.
    async fn heartbeat(session_id: String) -> Result<(), RpcError>;
}

// ---------------------------------------------------------------------------
// Supervisor → Worker service
// ---------------------------------------------------------------------------

/// Calls made by the supervisor to an individual worker process.
///
/// The supervisor connects to the worker's UDS (advertised in
/// `register_worker`) and calls these methods to drive PTY operations.
#[tarpc::service]
pub trait WorkerControl {
    /// Ask the worker to shut down gracefully within `grace_secs` seconds.
    ///
    /// After the deadline the supervisor sends SIGKILL.
    async fn shutdown(grace_secs: u32) -> Result<(), RpcError>;

    /// Tell the worker that `client_id` is now attached to pane `slot_idx`.
    ///
    /// The worker starts forwarding PTY output for that pane to the client.
    async fn attach_pane(slot_idx: u32, client_id: String) -> Result<(), RpcError>;

    /// Resize the PTY for pane `slot_idx` to `cols` × `rows`.
    async fn resize_pane(slot_idx: u32, cols: u16, rows: u16) -> Result<(), RpcError>;

    /// Write `bytes` directly into the PTY input of pane `slot_idx`.
    async fn send_keys(slot_idx: u32, bytes: Vec<u8>) -> Result<(), RpcError>;

    /// Spawn a new PTY for `slot_idx` with the given shell and working directory.
    ///
    /// `cols` and `rows` set the initial PTY dimensions; pass 0 to fall back to
    /// the 80×24 default (logged as a warning).
    ///
    /// The worker registers the pane internally and begins streaming output to
    /// the supervisor via `SupervisorWorkerClient::block_event`.
    async fn open_pane(
        slot_idx: u32,
        shell: String,
        cwd: String,
        cols: u16,
        rows: u16,
    ) -> Result<(), RpcError>;

    /// Kill the PTY for `slot_idx` and remove it from the worker's pane map.
    ///
    /// If no panes remain after removal the worker exits cleanly.
    async fn close_pane(slot_idx: u32) -> Result<(), RpcError>;

    /// Return the last `lines` lines of a pane's ring buffer (CSI stripped).
    async fn capture_pane(slot_idx: u32, lines: u32) -> Result<Vec<u8>, RpcError>;

    /// List all pane slot indices currently open in this worker.
    async fn list_panes() -> Result<Vec<u32>, RpcError>;

    /// Live pane metadata for agent UX (state engine runs in the worker).
    async fn get_pane_info(slot_idx: u32) -> Result<crate::PaneInfo, RpcError>;

    /// Override pane state (self-report / integration hooks).
    async fn set_pane_state(
        slot_idx: u32,
        state: crate::PaneStateKind,
        reason: String,
    ) -> Result<(), RpcError>;

    /// Mark pane as seen by the user.
    async fn mark_pane_seen(slot_idx: u32) -> Result<(), RpcError>;

    /// Return the live working directory of the shell child for pane `slot_idx`.
    ///
    /// Resolves via `/proc/<child_pid>/cwd`, which follows `cd` in real time.
    /// Returns `None` when the pane is absent or the procfs symlink cannot be
    /// read (e.g. the child has already exited).
    ///
    /// This is intentionally an internal supervisor↔worker RPC and does NOT
    /// affect the user-facing `PROTO_VERSION` (which guards the GUI↔daemon
    /// `PyreDaemon` handshake in `handshake.rs`).
    async fn pane_cwd(slot_idx: u32) -> Result<Option<String>, RpcError>;
}
