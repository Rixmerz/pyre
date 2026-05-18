//! Tantivy full-text index for blocks.
//!
//! Indexed fields: `command` (TEXT), `stdout` (TEXT, not stored).
//! Stored fields: `block_id`, `pane_id`, `session_id`, `started_at`.
//!
//! `BlockIndex::open` creates or reopens the index at
//! `$XDG_DATA_HOME/pyre/index/` (or `$PYRE_DATA_DIR/index/`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use pyre_proto::{Block, BlockId};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, OwnedValue, Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument};

const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024; // 50 MB

struct BlockSchema {
    block_id: Field,
    pane_id: Field,
    session_id: Field,
    started_at: Field,
    command: Field,
    stdout: Field,
}

pub struct BlockIndex {
    index: Index,
    writer: Arc<Mutex<IndexWriter>>,
    schema: BlockSchema,
}

impl BlockIndex {
    /// Open or create the tantivy index at `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;

        let mut sb = Schema::builder();
        let block_id = sb.add_text_field("block_id", STRING | STORED);
        let pane_id = sb.add_text_field("pane_id", STRING | STORED);
        let session_id = sb.add_text_field("session_id", STRING | STORED);
        let started_at = sb.add_i64_field("started_at", INDEXED | STORED | FAST);
        let command = sb.add_text_field("command", TEXT | STORED);
        let stdout = sb.add_text_field("stdout", TEXT);
        let schema = sb.build();

        let block_schema = BlockSchema {
            block_id,
            pane_id,
            session_id,
            started_at,
            command,
            stdout,
        };

        let mmap_dir =
            MmapDirectory::open(dir).with_context(|| format!("mmap dir {}", dir.display()))?;
        let index = Index::open_or_create(mmap_dir, schema).context("open_or_create tantivy")?;
        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .context("create tantivy writer")?;

        Ok(Self {
            index,
            writer: Arc::new(Mutex::new(writer)),
            schema: block_schema,
        })
    }

    /// Add a block and its decoded stdout to the index, then commit.
    pub fn add_block(&self, block: &Block, stdout: &str) -> Result<()> {
        let mut doc = TantivyDocument::new();
        doc.add_text(self.schema.block_id, block.id.0.to_string());
        doc.add_text(self.schema.pane_id, block.pane.0.to_string());
        doc.add_text(self.schema.session_id, block.session.0.to_string());
        doc.add_i64(self.schema.started_at, block.started_at.timestamp_millis());
        doc.add_text(self.schema.command, &block.command);
        doc.add_text(self.schema.stdout, stdout);

        let mut writer = self.writer.lock().expect("tantivy writer poisoned");
        writer.add_document(doc).context("tantivy add_document")?;
        writer.commit().context("tantivy commit")?;
        Ok(())
    }

    /// Search `query` across `command` and `stdout`, return up to `limit`
    /// `BlockId`s ordered by relevance score.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<BlockId>> {
        let reader = self.index.reader().context("tantivy reader")?;
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(&self.index, vec![self.schema.command, self.schema.stdout]);
        let parsed = qp.parse_query(query).context("tantivy parse_query")?;
        let top = searcher
            .search(
                &parsed,
                &TopDocs::with_limit(limit as usize).order_by_score(),
            )
            .context("tantivy search")?;

        let mut ids = Vec::with_capacity(top.len());
        for (_score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).context("tantivy fetch doc")?;
            if let Some(compact) = doc.get_first(self.schema.block_id) {
                if let OwnedValue::Str(s) = OwnedValue::from(compact) {
                    if let Ok(uuid) = uuid::Uuid::parse_str(&s) {
                        ids.push(BlockId(uuid));
                    }
                }
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pyre_proto::{PaneId, SessionId};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn make_block(command: &str) -> Block {
        Block {
            id: BlockId(Uuid::new_v4()),
            pane: PaneId(Uuid::new_v4()),
            session: SessionId(Uuid::new_v4()),
            command: command.to_string(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            stdout_len: 0,
        }
    }

    #[test]
    fn index_and_search_two_blocks() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let cargo_block = make_block("cargo build");
        let git_block = make_block("git status");

        idx.add_block(&cargo_block, "compiling pyre v0.1.0")?;
        idx.add_block(&git_block, "on branch main nothing to commit")?;

        let cargo_results = idx.search("cargo", 10)?;
        assert_eq!(cargo_results.len(), 1, "cargo search should return one hit");
        assert_eq!(cargo_results[0], cargo_block.id);

        let git_results = idx.search("git", 10)?;
        assert_eq!(git_results.len(), 1, "git search should return one hit");
        assert_eq!(git_results[0], git_block.id);

        Ok(())
    }
}
