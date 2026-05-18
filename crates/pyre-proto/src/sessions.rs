//! Session/pane info + open-pane + replay types for S3.

use std::path::PathBuf;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Block, PaneId, SessionId};

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
