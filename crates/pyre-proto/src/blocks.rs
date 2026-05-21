//! Block model — event stream + list/search RPC request/response types for S2.

use std::path::PathBuf;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{Block, BlockId, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BlockEvent {
    PromptStart {
        session: SessionId,
    },
    CommandStart {
        session: SessionId,
        block: BlockId,
        command: String,
        cwd: Option<PathBuf>,
    },
    OutputChunk {
        block: BlockId,
        #[serde(with = "bytes_serde")]
        data: Bytes,
    },
    BlockEnd {
        block: BlockId,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListBlocksReq {
    pub session: Option<SessionId>,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchBlocksReq {
    pub query: String,
    pub limit: u32,
    /// When true, only blocks with a non-zero exit code are returned.
    #[serde(default)]
    pub failures_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHit {
    pub block: Block,
    pub snippet: String,
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
