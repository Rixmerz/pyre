//! pyre shared protocol types: sessions, panes, blocks, IPC commands.

pub mod service;
pub use service::{
    AttachAck, InputFrame, OutputFrame, PyreDaemon, PyreDaemonClient, PyreError, SpawnReq,
    MODE_CONTROL, MODE_STREAM,
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
    pub command: String,
    pub cwd: String,
    pub stdout: String,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
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
}
