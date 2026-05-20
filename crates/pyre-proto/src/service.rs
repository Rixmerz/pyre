//! Control-plane RPC + sidechannel frame types for pyred <-> pyrec.
//!
//! S1/S3 multiplexes over a single UDS using a one-byte mode tag the
//! client writes after `connect()`:
//!
//!   * `0x01` — control: tarpc bincode transport for the `PyreDaemon`
//!     service trait below.
//!   * `0x02` — stream: after the tag the client writes 16 bytes
//!     `SessionId` (Uuid as_bytes) followed by 16 bytes `PaneId`
//!     (Uuid as_bytes), totalling 32 bytes.  The server replies with
//!     one synthetic `OutputFrame { seq: 0, data: <ring-buffer snapshot> }`
//!     before live frames.  The connection then becomes a bidirectional
//!     length-delimited bincode channel carrying `OutputFrame`
//!     (daemon -> client) and `InputFrame` (client -> daemon).
//!
//! Keeping the streams off the tarpc service avoids modelling
//! streaming-RPCs in tarpc 0.34 and gives us byte-clean raw passthrough
//! for the PTY.

use std::path::PathBuf;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{PaneId, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnReq {
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
    /// Optional human-readable name for the new session.
    /// Defaults to `session-<short8>` on the daemon side when absent.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachAck {
    pub session: SessionId,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum PyreError {
    #[error("no such session: {0}")]
    NoSuchSession(SessionId),
    #[error("no such pane: {0}")]
    NoSuchPane(PaneId),
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
    #[error("io: {0}")]
    Io(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFrame {
    pub session: SessionId,
    pub seq: u64,
    #[serde(with = "bytes_serde")]
    pub data: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFrame {
    pub session: SessionId,
    #[serde(with = "bytes_serde")]
    pub data: Bytes,
}

mod bytes_serde {
    use bytes::Bytes;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        serde_bytes::serialize(b.as_ref(), s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let v: Vec<u8> = serde_bytes::deserialize(d)?;
        Ok(Bytes::from(v))
    }
}

pub const MODE_CONTROL: u8 = 0x01;
pub const MODE_STREAM: u8 = 0x02;

#[tarpc::service]
pub trait PyreDaemon {
    async fn spawn(req: SpawnReq) -> Result<crate::SpawnResp, PyreError>;
    async fn attach(session: SessionId) -> Result<AttachAck, PyreError>;
    async fn detach(session: SessionId) -> Result<(), PyreError>;
    async fn kill(session: SessionId) -> Result<(), PyreError>;
    async fn list_blocks(req: crate::blocks::ListBlocksReq)
        -> Result<Vec<crate::Block>, PyreError>;
    async fn search_blocks(
        req: crate::blocks::SearchBlocksReq,
    ) -> Result<Vec<crate::blocks::BlockHit>, PyreError>;
    async fn list_sessions() -> Result<Vec<crate::SessionInfo>, PyreError>;
    async fn list_panes(session: SessionId) -> Result<Vec<crate::PaneInfo>, PyreError>;
    async fn open_pane(req: crate::OpenPaneReq) -> Result<PaneId, PyreError>;
    async fn close_pane(pane: PaneId) -> Result<(), PyreError>;
    async fn replay(pane: PaneId, recent_blocks: u32) -> Result<crate::ReplayBlocks, PyreError>;
    /// Return the decompressed stdout bytes of the last block for a given block id.
    async fn get_block_stdout(block_id: crate::BlockId) -> Result<Vec<u8>, PyreError>;
    /// Return the last `lines` lines of a pane's ring buffer with CSI stripped.
    async fn capture_pane(pane: PaneId, lines: u32) -> Result<Vec<u8>, PyreError>;
    /// Close all panes in a session and remove it from the registry.
    async fn close_session(session: SessionId) -> Result<(), PyreError>;
    /// Override the state of a pane for up to `PYRE_OVERRIDE_WINDOW_SECS` seconds.
    /// After the window expires the heuristic engine resumes.
    async fn set_pane_state(
        pane: PaneId,
        state: crate::PaneStateKind,
        reason: String,
    ) -> Result<(), PyreError>;
    /// List all panes across all sessions (convenience for clients that iterate sessions).
    async fn list_all_panes() -> Result<Vec<crate::PaneInfo>, PyreError>;
    /// Return process metadata for the foreground PID of a pane (Linux-only).
    async fn inspect_pid(pane: PaneId) -> Result<PidInspect, PyreError>;
    /// Deliver raw bytes directly to a pane's PTY input channel.
    /// Bypasses the stream protocol to avoid the race where the socket closes
    /// before the async stream task forwards the InputFrame to pane.input_tx.
    async fn send_keys(pane: PaneId, bytes: Vec<u8>) -> Result<(), PyreError>;
    /// Resize the PTY of a pane to the given dimensions.
    async fn resize_pane(req: crate::ResizePaneReq) -> Result<crate::ResizePaneRes, PyreError>;
    /// Rename an existing session. Persists to SQLite immediately.
    async fn rename_session(session: SessionId, name: String) -> Result<(), PyreError>;
}

/// Process metadata returned by `inspect_pid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidInspect {
    pub pid: u32,
    pub comm: String,
    /// First ≤50 environment variables; values truncated to 80 chars.
    pub env: Vec<(String, String)>,
    /// Resolved symlinks from /proc/{pid}/fd (≤50 entries).
    pub fds: Vec<String>,
    /// Direct child PIDs.
    pub children: Vec<u32>,
}
