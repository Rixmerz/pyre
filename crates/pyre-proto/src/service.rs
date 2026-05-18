//! Control-plane RPC + sidechannel frame types for pyred <-> pyrec.
//!
//! S1 multiplexes over a single UDS using a one-byte mode tag the
//! client writes after `connect()`:
//!
//!   * `0x01` — control: tarpc bincode transport for the `PyreDaemon`
//!     service trait below.
//!   * `0x02` — stream: after the tag the client writes a 16-byte
//!     `SessionId` (Uuid as_bytes), then the connection becomes a
//!     bidirectional length-delimited bincode channel carrying
//!     `OutputFrame` (daemon -> client) and `InputFrame`
//!     (client -> daemon).
//!
//! Keeping the streams off the tarpc service avoids modelling
//! streaming-RPCs in tarpc 0.34 and gives us byte-clean raw passthrough
//! for the PTY.

use std::path::PathBuf;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnReq {
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
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
    async fn spawn(req: SpawnReq) -> Result<SessionId, PyreError>;
    async fn attach(session: SessionId) -> Result<AttachAck, PyreError>;
    async fn detach(session: SessionId) -> Result<(), PyreError>;
    async fn kill(session: SessionId) -> Result<(), PyreError>;
}
