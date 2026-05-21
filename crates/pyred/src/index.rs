//! Tantivy full-text index for blocks.
//!
//! Indexed fields: `command` (TEXT), `stdout` (TEXT, not stored),
//! `exit_code` (i64, INDEXED+FAST).
//! Stored fields: `block_id`, `pane_id`, `session_id`, `started_at`.
//!
//! `BlockIndex::open` creates or reopens the index at
//! `$XDG_DATA_HOME/pyre/index/` (or `$PYRE_DATA_DIR/index/`).
//!
//! Schema version 2: adds `exit_code` field. Blocks with no exit code are
//! stored as `i64::MIN` sentinel. The `search` function accepts a
//! `failures_only` flag that appends a `RangeQuery` on `exit_code >= 1`,
//! excluding both successes (0) and the sentinel (`i64::MIN`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use pyre_proto::{Block, BlockId};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, QueryParser, RangeQuery};
use tantivy::schema::{Field, OwnedValue, Schema, FAST, INDEXED, STORED, STRING, TEXT};
use tantivy::Term;
use tantivy::{Index, IndexWriter, TantivyDocument};

const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024; // 50 MB

struct BlockSchema {
    block_id: Field,
    pane_id: Field,
    session_id: Field,
    started_at: Field,
    command: Field,
    stdout: Field,
    exit_code: Field,
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
        let exit_code = sb.add_i64_field("exit_code", INDEXED | FAST);
        let schema = sb.build();

        let block_schema = BlockSchema {
            block_id,
            pane_id,
            session_id,
            started_at,
            command,
            stdout,
            exit_code,
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
    ///
    /// Blocks with no exit code are stored as `i64::MIN` sentinel so the
    /// `exit_code >= 1` range query used by `failures_only` excludes them.
    pub fn add_block(&self, block: &Block, stdout: &str) -> Result<()> {
        let mut doc = TantivyDocument::new();
        doc.add_text(self.schema.block_id, block.id.0.to_string());
        doc.add_text(self.schema.pane_id, block.pane.0.to_string());
        doc.add_text(self.schema.session_id, block.session.0.to_string());
        doc.add_i64(self.schema.started_at, block.started_at.timestamp_millis());
        doc.add_text(self.schema.command, &block.command);
        doc.add_text(self.schema.stdout, stdout);
        doc.add_i64(
            self.schema.exit_code,
            block.exit_code.map(i64::from).unwrap_or(i64::MIN),
        );

        let mut writer = self.writer.lock().expect("tantivy writer poisoned");
        writer.add_document(doc).context("tantivy add_document")?;
        writer.commit().context("tantivy commit")?;
        Ok(())
    }

    /// Search `query` across `command` and `stdout`, return up to `limit`
    /// `BlockId`s ordered by relevance score.
    ///
    /// When `failures_only` is `true`, only blocks whose `exit_code` is
    /// `>= 1` are returned (excludes exit_code 0 = success and `i64::MIN`
    /// sentinel = no exit code recorded).
    pub fn search(&self, query: &str, limit: u32, failures_only: bool) -> Result<Vec<BlockId>> {
        let reader = self.index.reader().context("tantivy reader")?;
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(&self.index, vec![self.schema.command, self.schema.stdout]);
        let text_query = qp.parse_query(query).context("tantivy parse_query")?;

        let final_query: Box<dyn tantivy::query::Query> = if failures_only {
            let exit_range = RangeQuery::new(
                std::ops::Bound::Included(Term::from_field_i64(self.schema.exit_code, 1)),
                std::ops::Bound::Included(Term::from_field_i64(self.schema.exit_code, i64::MAX)),
            );
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, text_query),
                (Occur::Must, Box::new(exit_range)),
            ]))
        } else {
            text_query
        };

        let top = searcher
            .search(
                final_query.as_ref(),
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

    fn make_block_with_exit(command: &str, exit_code: Option<i32>) -> Block {
        Block {
            exit_code,
            ..make_block(command)
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

        let cargo_results = idx.search("cargo", 10, false)?;
        assert_eq!(cargo_results.len(), 1, "cargo search should return one hit");
        assert_eq!(cargo_results[0], cargo_block.id);

        let git_results = idx.search("git", 10, false)?;
        assert_eq!(git_results.len(), 1, "git search should return one hit");
        assert_eq!(git_results[0], git_block.id);

        Ok(())
    }

    #[test]
    fn failures_only_returns_only_nonzero_exit() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        // success block (exit 0)
        let ok_block = make_block_with_exit("make test", Some(0));
        // failure block (exit 1)
        let fail_block = make_block_with_exit("make test", Some(1));
        // no-exit block (still running / no exit recorded)
        let pending_block = make_block_with_exit("make test", None);

        idx.add_block(&ok_block, "make: all tests passed")?;
        idx.add_block(&fail_block, "make: 3 tests FAILED")?;
        idx.add_block(&pending_block, "make: running tests")?;

        // failures_only=true must return only the block with exit_code=1
        let failures = idx.search("make", 20, true)?;
        assert_eq!(
            failures.len(),
            1,
            "failures_only=true should return exactly one block, got {}: {:?}",
            failures.len(),
            failures
        );
        assert_eq!(
            failures[0], fail_block.id,
            "the returned block must be the one with exit_code=1"
        );

        // failures_only=false must return all three blocks
        let all = idx.search("make", 20, false)?;
        assert_eq!(
            all.len(),
            3,
            "failures_only=false should return all 3 blocks, got {}",
            all.len()
        );

        Ok(())
    }
}
