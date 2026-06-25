//! pyre shared protocol types: sessions, panes, blocks, IPC commands.

pub mod blocks;
pub mod handshake;
pub mod layout;
pub mod paths;
pub mod service;
pub mod sessions;
pub mod shell_integration;
pub mod socket;
pub mod supervisor;
pub use blocks::{BlockEvent, BlockHit, ListBlocksReq, SearchBlocksReq};
pub use handshake::{
    read_control_server, read_control_version_after_tag, write_control_client, PROTO_VERSION,
};
pub use layout::{Dir, LayoutNode, Orient, Rect};
pub use paths::runtime_pyre_dir;
pub use service::{
    AttachAck, InputFrame, OpenPaneSplitReq, OutputFrame, PaneEvent, PaneEventKind, PidInspect,
    PyreDaemon, PyreDaemonClient, PyreError, SpawnReq, MODE_CONTROL, MODE_STREAM,
};
pub use sessions::{
    AgentKind, OpenPaneReq, PaneInfo, PaneStateKind, ReplayBlocks, ResizePaneReq, ResizePaneRes,
    SessionInfo, SpawnResp, WindowInfo,
};
pub use socket::{attach_stream, connect_control, default_socket};
pub use supervisor::{
    BlockEvent as SupervisorBlockEvent, BlockKind, RegisterAck, RpcError as SupervisorRpcError,
    SupervisorWorkerClient, WorkerControlClient,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_newtype!(SessionId);
id_newtype!(PaneId);
id_newtype!(BlockId);
id_newtype!(WindowId);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub panes: Vec<PaneId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub session: SessionId,
    pub cwd: String,
    pub shell: String,
    pub size: PaneSize,
    pub block_ids: Vec<BlockId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub pane: PaneId,
    pub session: SessionId,
    pub command: String,
    pub cwd: Option<std::path::PathBuf>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub stdout_len: u64,
}

impl Block {
    /// Wall-clock duration of the block in milliseconds.
    ///
    /// Returns `None` when the block has not yet received a `BlockEnd` event
    /// (i.e. `ended_at` is `None`).
    pub fn duration_ms(&self) -> Option<u64> {
        let ended = self.ended_at?;
        let delta = ended - self.started_at;
        // to_std() fails only when the duration is negative (clock skew / test
        // fixture artefact); treat that as None rather than panicking.
        delta.to_std().ok().map(|d| d.as_millis() as u64)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        session: Option<SessionId>,
        shell: Option<String>,
        cwd: Option<String>,
        size: PaneSize,
    },
    Attach {
        pane: PaneId,
    },
    Detach {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        size: PaneSize,
    },
    Kill {
        pane: PaneId,
    },
    ListSessions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Spawned { pane: PaneId, session: SessionId },
    Attached { pane: PaneId },
    Detached { pane: PaneId },
    Resized { pane: PaneId },
    Killed { pane: PaneId },
    Sessions { sessions: Vec<Session> },
    Error { message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_unique() {
        assert_ne!(SessionId::new(), SessionId::new());
    }

    #[test]
    fn request_roundtrip() {
        let r = Request::Resize {
            pane: PaneId::new(),
            size: PaneSize { cols: 80, rows: 24 },
        };
        let s = serde_json::to_string(&r).unwrap();
        let _back: Request = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn duration_ms_none_when_not_ended() {
        let b = Block {
            id: BlockId::new(),
            pane: PaneId::new(),
            session: SessionId::new(),
            command: "ls".to_string(),
            cwd: None,
            started_at: DateTime::from_timestamp_millis(0).unwrap(),
            ended_at: None,
            exit_code: None,
            stdout_len: 0,
        };
        assert_eq!(
            b.duration_ms(),
            None,
            "duration_ms must be None when ended_at is absent"
        );
    }

    #[test]
    fn duration_ms_correct_when_ended() {
        let start = DateTime::from_timestamp_millis(1_000_000).unwrap();
        let end = DateTime::from_timestamp_millis(1_001_500).unwrap(); // 1500 ms later
        let b = Block {
            id: BlockId::new(),
            pane: PaneId::new(),
            session: SessionId::new(),
            command: "sleep 1".to_string(),
            cwd: None,
            started_at: start,
            ended_at: Some(end),
            exit_code: Some(0),
            stdout_len: 0,
        };
        assert_eq!(
            b.duration_ms(),
            Some(1500),
            "duration_ms must equal ended_at - started_at in milliseconds"
        );
    }
}
