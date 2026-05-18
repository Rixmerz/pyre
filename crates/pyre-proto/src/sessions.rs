//! Session/pane info + open-pane + replay types for S3.

use std::path::PathBuf;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Block, PaneId, SessionId};

/// Coarse lifecycle state of a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PaneStateKind {
    #[default]
    /// Shell/process is actively producing output.
    Running,
    /// Shell prompt visible; waiting for user/agent input.
    WaitingInput,
    /// Process is alive but idle (no input, no output).
    Idle,
    /// Foreground is a full-screen interactive program (vim, less, top, …).
    Interactive,
    /// Process exited with a non-zero code and has not been respawned.
    Crashed,
    /// Process exited cleanly.
    Done,
}

impl std::fmt::Display for PaneStateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::WaitingInput => write!(f, "waiting"),
            Self::Idle => write!(f, "idle"),
            Self::Interactive => write!(f, "interactive"),
            Self::Crashed => write!(f, "crashed"),
            Self::Done => write!(f, "done"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub pane_count: u32,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub session: SessionId,
    pub cols: u16,
    pub rows: u16,
    pub shell: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Current lifecycle state.
    pub state: PaneStateKind,
    /// Human-readable reason for the current state.
    pub state_reason: String,
    /// Wall-clock time of the last byte received from this pane.
    pub last_activity: DateTime<Utc>,
    /// Basename of the current foreground process (if known).
    pub foreground_cmd: Option<String>,
    /// Root PID of the PTY child process.
    pub root_pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPaneReq {
    pub session: SessionId,
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnResp {
    pub session: SessionId,
    pub pane: PaneId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayBlocks {
    pub recent: Vec<Block>,
    #[serde(with = "bytes_serde")]
    pub snapshot: Bytes,
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
