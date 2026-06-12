//! Tantivy full-text index for blocks.
//!
//! Indexed fields: `command` (TEXT), `stdout` (TEXT, not stored),
//! `exit_code` (i64, INDEXED+FAST), `pane_id` (STRING+STORED, term-indexed
//! via STRING semantics), `session_id` (STRING+STORED, same).
//! Stored fields: `block_id`, `started_at`.
//!
//! `BlockIndex::open` creates or reopens the index at a versioned sub-directory
//! of the caller-supplied `dir`:
//!
//!   `<dir>/v3/`  — schema version 3 (this version).
//!
//! Schema migration behaviour: each schema version lives in its own
//! sub-directory. When the daemon starts with a newer schema, it creates the
//! new sub-directory and starts indexing there. The old directory is left on
//! disk and orphaned; users can delete it manually or let a future GC pass
//! remove it. Search starts empty on a fresh directory — blocks are
//! re-indexed as they are re-executed.
//!
//! Schema version 3 changes vs v2:
//! - The index directory is versioned (`v3/` sub-dir) so old schema is
//!   abandoned cleanly rather than causing `open_or_create` schema-mismatch
//!   errors on upgrade.
//! - `pane_id` / `session_id` remain `STRING | STORED`; STRING already
//!   implies term indexing, enabling `TermQuery` filters by pane or session.
//!
//! Schema version 2: adds `exit_code` field. Blocks with no exit code are
//! stored as `i64::MIN` sentinel. The `search` function accepts a
//! `failures_only` flag that appends a `RangeQuery` on `exit_code >= 1`,
//! excluding both successes (0) and the sentinel (`i64::MIN`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use pyre_proto::{Block, BlockId, PaneId, SessionId};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, OwnedValue, Schema, FAST, INDEXED, STORED, STRING, TEXT,
};
use tantivy::Term;
use tantivy::{Index, IndexWriter, TantivyDocument};

const WRITER_HEAP_BYTES: usize = 50 * 1024 * 1024; // 50 MB

/// Sub-directory name for the current schema version.
/// Bump this string (e.g. "v4") whenever the Tantivy schema changes in a
/// backwards-incompatible way.  The previous directory is left on disk and
/// abandoned.
pub const SCHEMA_VERSION_DIR: &str = "v3";

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
    /// Open or create the tantivy index at `dir/v3/`.
    pub fn open(dir: &Path) -> Result<Self> {
        let versioned = dir.join(SCHEMA_VERSION_DIR);
        std::fs::create_dir_all(&versioned)
            .with_context(|| format!("mkdir {}", versioned.display()))?;

        let mut sb = Schema::builder();
        let block_id = sb.add_text_field("block_id", STRING | STORED);
        // pane_id and session_id use STRING (not TEXT) so they are stored as a
        // single exact-match term — TermQuery can filter on them without
        // tokenisation.  STRING already implies indexing for term lookup; the
        // separate INDEXED flag applies only to numeric/date fields and cannot
        // be applied to TextOptions.
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

        let mmap_dir = MmapDirectory::open(&versioned)
            .with_context(|| format!("mmap dir {}", versioned.display()))?;
        let index = Index::open_or_create(mmap_dir, schema).context("open_or_create tantivy v3")?;
        let writer = index
            .writer(WRITER_HEAP_BYTES)
            .context("create tantivy writer")?;

        Ok(Self {
            index,
            writer: Arc::new(Mutex::new(writer)),
            schema: block_schema,
        })
    }

    /// Return the number of documents currently committed to the index.
    ///
    /// Used by the startup backfill guard: if doc count is 0 but SQLite has
    /// blocks, the v3 index is empty and needs to be backfilled.
    pub fn doc_count(&self) -> Result<u64> {
        let reader = self
            .index
            .reader()
            .context("tantivy reader for doc_count")?;
        Ok(reader.searcher().num_docs())
    }

    /// Backfill the v3 index from `blocks` (already fetched from SQLite).
    ///
    /// Each block's stdout blob is read via `read_blob` (a closure so the
    /// sync blob reading can be tested without a real `Store`).  A missing or
    /// corrupt blob results in an empty-stdout document (warn, not abort).
    ///
    /// Documents are added in batches of `BACKFILL_BATCH_SIZE` and committed
    /// together at the end; a mid-batch commit fires every `BACKFILL_BATCH_SIZE`
    /// docs to bound memory on very large histories.
    ///
    /// Returns the number of documents successfully added.
    pub fn backfill<F>(&self, blocks: &[Block], mut read_blob: F) -> Result<usize>
    where
        F: FnMut(BlockId) -> Result<Vec<u8>>,
    {
        const BACKFILL_BATCH_SIZE: usize = 500;

        let mut writer = self.writer.lock().expect("tantivy writer poisoned");
        let mut added = 0usize;

        for (i, block) in blocks.iter().enumerate() {
            let stdout_bytes = match read_blob(block.id) {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(block_id = %block.id, "backfill: failed to read stdout blob: {e:#}");
                    Vec::new()
                }
            };
            let stdout = String::from_utf8_lossy(&stdout_bytes);

            let mut doc = tantivy::TantivyDocument::new();
            doc.add_text(self.schema.block_id, block.id.0.to_string());
            doc.add_text(self.schema.pane_id, block.pane.0.to_string());
            doc.add_text(self.schema.session_id, block.session.0.to_string());
            doc.add_i64(self.schema.started_at, block.started_at.timestamp_millis());
            doc.add_text(self.schema.command, &block.command);
            doc.add_text(self.schema.stdout, stdout.as_ref());
            doc.add_i64(
                self.schema.exit_code,
                block.exit_code.map(i64::from).unwrap_or(i64::MIN),
            );

            if let Err(e) = writer.add_document(doc) {
                tracing::warn!(block_id = %block.id, "backfill: add_document failed: {e:#}");
                continue;
            }
            added += 1;

            // Chunked commit every BACKFILL_BATCH_SIZE docs to bound memory.
            if (i + 1) % BACKFILL_BATCH_SIZE == 0 {
                if let Err(e) = writer.commit() {
                    tracing::warn!("backfill: mid-batch commit failed at doc {i}: {e:#}");
                }
            }
        }

        // Final commit for any remaining docs.
        writer.commit().context("backfill: final commit")?;
        Ok(added)
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
    /// Filters applied (all `Must` clauses combined with `BooleanQuery`):
    ///
    /// - `failures_only`: restrict to `exit_code >= 1`. Ignored when
    ///   `exit_code` is `Some`.
    /// - `exit_code`: exact match on the stored i64 exit code.  Supersedes
    ///   `failures_only` when set.
    /// - `session`: restrict to blocks with this `session_id`.
    /// - `pane`: restrict to blocks with this `pane_id`.
    pub fn search(
        &self,
        query: &str,
        limit: u32,
        failures_only: bool,
        session: Option<SessionId>,
        pane: Option<PaneId>,
        exit_code: Option<i32>,
    ) -> Result<Vec<BlockId>> {
        let reader = self.index.reader().context("tantivy reader")?;
        let searcher = reader.searcher();
        let qp = QueryParser::for_index(&self.index, vec![self.schema.command, self.schema.stdout]);
        let text_query = qp.parse_query(query).context("tantivy parse_query")?;

        // Build the list of Must clauses starting with the text query.
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            vec![(Occur::Must, text_query)];

        // Exit-code filter: exact match supersedes failures_only.
        if let Some(code) = exit_code {
            let term = Term::from_field_i64(self.schema.exit_code, i64::from(code));
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        } else if failures_only {
            let exit_range = RangeQuery::new(
                std::ops::Bound::Included(Term::from_field_i64(self.schema.exit_code, 1)),
                std::ops::Bound::Included(Term::from_field_i64(self.schema.exit_code, i64::MAX)),
            );
            clauses.push((Occur::Must, Box::new(exit_range)));
        }

        // Session filter.
        if let Some(sid) = session {
            let term = Term::from_field_text(self.schema.session_id, &sid.0.to_string());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        // Pane filter.
        if let Some(pid) = pane {
            let term = Term::from_field_text(self.schema.pane_id, &pid.0.to_string());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        let final_query: Box<dyn tantivy::query::Query> = if clauses.len() == 1 {
            // Only the text query — skip BooleanQuery wrapper for efficiency.
            clauses.remove(0).1
        } else {
            Box::new(BooleanQuery::new(clauses))
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

    /// Convenience: search with no filters beyond `failures_only`.
    fn search_basic(
        idx: &BlockIndex,
        query: &str,
        limit: u32,
        failures_only: bool,
    ) -> Result<Vec<BlockId>> {
        idx.search(query, limit, failures_only, None, None, None)
    }

    #[test]
    fn index_and_search_two_blocks() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let cargo_block = make_block("cargo build");
        let git_block = make_block("git status");

        idx.add_block(&cargo_block, "compiling pyre v0.1.0")?;
        idx.add_block(&git_block, "on branch main nothing to commit")?;

        let cargo_results = search_basic(&idx, "cargo", 10, false)?;
        assert_eq!(cargo_results.len(), 1, "cargo search should return one hit");
        assert_eq!(cargo_results[0], cargo_block.id);

        let git_results = search_basic(&idx, "git", 10, false)?;
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
        let failures = search_basic(&idx, "make", 20, true)?;
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
        let all = search_basic(&idx, "make", 20, false)?;
        assert_eq!(
            all.len(),
            3,
            "failures_only=false should return all 3 blocks, got {}",
            all.len()
        );

        Ok(())
    }

    #[test]
    fn pane_filter_returns_only_matching_pane() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let pane_a = PaneId(Uuid::new_v4());
        let pane_b = PaneId(Uuid::new_v4());
        let session = SessionId(Uuid::new_v4());

        let block_a = Block {
            id: BlockId(Uuid::new_v4()),
            pane: pane_a,
            session,
            command: "rustfmt src".to_string(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: Some(0),
            stdout_len: 0,
        };
        let block_b = Block {
            id: BlockId(Uuid::new_v4()),
            pane: pane_b,
            session,
            command: "rustfmt src".to_string(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: Some(0),
            stdout_len: 0,
        };

        idx.add_block(&block_a, "formatted 3 files")?;
        idx.add_block(&block_b, "formatted 1 file")?;

        let results = idx.search("rustfmt", 20, false, None, Some(pane_a), None)?;
        assert_eq!(
            results.len(),
            1,
            "pane filter must return only blocks from pane_a, got {}: {:?}",
            results.len(),
            results
        );
        assert_eq!(
            results[0], block_a.id,
            "returned block must belong to pane_a"
        );

        Ok(())
    }

    #[test]
    fn session_filter_returns_only_matching_session() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let session_a = SessionId(Uuid::new_v4());
        let session_b = SessionId(Uuid::new_v4());

        let block_a = Block {
            id: BlockId(Uuid::new_v4()),
            pane: PaneId(Uuid::new_v4()),
            session: session_a,
            command: "clippy check".to_string(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: Some(0),
            stdout_len: 0,
        };
        let block_b = Block {
            id: BlockId(Uuid::new_v4()),
            pane: PaneId(Uuid::new_v4()),
            session: session_b,
            command: "clippy check".to_string(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: Some(0),
            stdout_len: 0,
        };

        idx.add_block(&block_a, "no warnings")?;
        idx.add_block(&block_b, "no warnings")?;

        let results = idx.search("clippy", 20, false, Some(session_a), None, None)?;
        assert_eq!(
            results.len(),
            1,
            "session filter must return only blocks from session_a, got {}: {:?}",
            results.len(),
            results
        );
        assert_eq!(
            results[0], block_a.id,
            "returned block must belong to session_a"
        );

        Ok(())
    }

    #[test]
    fn exit_code_filter_exact_match() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let exit0 = make_block_with_exit("pytest suite", Some(0));
        let exit1 = make_block_with_exit("pytest suite", Some(1));
        let exit2 = make_block_with_exit("pytest suite", Some(2));

        idx.add_block(&exit0, "all passed")?;
        idx.add_block(&exit1, "1 failed")?;
        idx.add_block(&exit2, "2 failed")?;

        // exact exit_code=1 filter supersedes failures_only
        let results = idx.search("pytest", 20, false, None, None, Some(1))?;
        assert_eq!(
            results.len(),
            1,
            "exit_code=1 filter must return exactly one block, got {}: {:?}",
            results.len(),
            results
        );
        assert_eq!(results[0], exit1.id, "returned block must have exit_code=1");

        // exit_code=0 filter must return only the success block
        let ok_results = idx.search("pytest", 20, false, None, None, Some(0))?;
        assert_eq!(ok_results.len(), 1, "exit_code=0 must return one block");
        assert_eq!(ok_results[0], exit0.id);

        Ok(())
    }

    #[test]
    fn exit_code_filter_supersedes_failures_only() -> Result<()> {
        let tmp = TempDir::new()?;
        let idx = BlockIndex::open(tmp.path())?;

        let exit0 = make_block_with_exit("cargo nextest", Some(0));
        let exit1 = make_block_with_exit("cargo nextest", Some(1));

        idx.add_block(&exit0, "all green")?;
        idx.add_block(&exit1, "1 test failed")?;

        // exit_code=Some(0) with failures_only=true must return the success block,
        // not the failure — exit_code filter wins.
        let results = idx.search("nextest", 20, true, None, None, Some(0))?;
        assert_eq!(
            results.len(),
            1,
            "exit_code=0 with failures_only=true must return the exit-0 block"
        );
        assert_eq!(results[0], exit0.id);

        Ok(())
    }
}
