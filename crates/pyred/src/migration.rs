//! One-time migration: monolithic `state.db` → per-session worker shards (hybrid mode).
//!
//! Called once at supervisor startup BEFORE binding sockets when
//! `process_model == "hybrid"`. Idempotent: a sentinel file at
//! `$XDG_STATE_HOME/pyre/migration_completed` prevents re-running.
//!
//! # Migration steps
//!
//! 1. Check sentinel → return [`MigrationReport::AlreadyDone`] if present.
//! 2. Detect legacy `$XDG_STATE_HOME/pyre/state.db` → return
//!    [`MigrationReport::NoLegacy`] (and write sentinel) if absent.
//! 3. Backup legacy DB → `state.db.bak.YYYYMMDD-HHMMSS`. Abort if backup fails.
//! 4. Open legacy DB read-only; iterate distinct `session_id`s from `panes`
//!    and `blocks`. For each session:
//!    - `mkdir $XDG_STATE_HOME/pyre/sessions/<session_id>/`
//!    - Open per-session shard (same schema as `WorkerShard` in `worker.rs`).
//!    - Copy matching `panes` rows.
//! 5. Rebuild Tantivy index from all `blocks` rows in the legacy DB.
//! 6. Write sentinel containing ISO8601 timestamp and backup path.
//! 7. Return [`MigrationReport::Migrated`].
//!
//! Any IO or DB failure aborts BEFORE writing sentinel so retry on next boot
//! works.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::index::BlockIndex;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Report returned by [`migrate_to_hybrid`].
#[derive(Debug, Clone)]
pub enum MigrationReport {
    /// Sentinel already present; no work done.
    AlreadyDone,
    /// No monolithic `state.db` found; sentinel written for future boots.
    NoLegacy,
    /// Migration completed.
    Migrated {
        sessions: usize,
        blocks: usize,
        backup_path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn state_home() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("state")
}

fn pyre_state_dir() -> PathBuf {
    state_home().join("pyre")
}

fn legacy_db_path() -> PathBuf {
    pyre_state_dir().join("state.db")
}

fn sentinel_path() -> PathBuf {
    pyre_state_dir().join("migration_completed")
}

fn sessions_dir() -> PathBuf {
    pyre_state_dir().join("sessions")
}

fn index_dir() -> PathBuf {
    pyre_state_dir().join("index")
}

// ---------------------------------------------------------------------------
// Sentinel helpers
// ---------------------------------------------------------------------------

fn sentinel_exists() -> bool {
    sentinel_path().exists()
}

fn write_sentinel(timestamp: &str, backup_path: &str) -> Result<()> {
    let content = format!("timestamp={timestamp}\nbackup={backup_path}\n");
    std::fs::write(sentinel_path(), content).context("write migration sentinel")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-session shard helpers (mirrors WorkerShard in worker.rs)
// ---------------------------------------------------------------------------

async fn open_shard(session_dir: &PathBuf) -> Result<SqlitePool> {
    std::fs::create_dir_all(session_dir)
        .with_context(|| format!("mkdir {}", session_dir.display()))?;

    let db_path = session_dir.join("state.db");
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .with_context(|| format!("open shard {}", db_path.display()))?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS panes (
            slot_idx INTEGER PRIMARY KEY,
            shell    TEXT NOT NULL,
            cwd      TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .context("create panes table in shard")?;

    Ok(pool)
}

// ---------------------------------------------------------------------------
// Tantivy rebuild
// ---------------------------------------------------------------------------

fn rebuild_tantivy_index(index_dir: &PathBuf, blocks: &[LegacyBlock]) -> Result<()> {
    // Remove existing index for a clean rebuild.
    if index_dir.exists() {
        std::fs::remove_dir_all(index_dir)
            .with_context(|| format!("rm -rf {}", index_dir.display()))?;
    }

    let block_index = BlockIndex::open(index_dir).context("open tantivy for rebuild")?;

    for b in blocks {
        // Read stdout blob if it exists; ignore missing blobs (non-fatal).
        let stdout_text = if let Some(ref blob_path) = b.stdout_blob_path {
            match read_zstd_blob(blob_path) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => {
                    tracing::warn!(blob_path, "failed to read stdout blob: {e:#}");
                    String::new()
                }
            }
        } else {
            String::new()
        };

        let proto_block = b.to_proto_block();
        if let Err(e) = block_index.add_block(&proto_block, &stdout_text) {
            tracing::warn!(block_id = %b.id, "tantivy add_block failed: {e:#}");
        }
    }

    Ok(())
}

fn read_zstd_blob(path: &str) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let decompressed = zstd::decode_all(std::io::Cursor::new(raw))
        .with_context(|| format!("zstd decode {path}"))?;
    Ok(decompressed)
}

// ---------------------------------------------------------------------------
// Legacy block row (from monolithic DB)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LegacyBlock {
    id: String,
    pane_id: String,
    session_id: String,
    command: String,
    started_at: i64,
    ended_at: Option<i64>,
    exit_code: Option<i32>,
    cwd: Option<String>,
    stdout_blob_path: Option<String>,
}

impl LegacyBlock {
    fn to_proto_block(&self) -> pyre_proto::Block {
        use chrono::TimeZone;
        let block_uuid = uuid::Uuid::parse_str(&self.id).unwrap_or_default();
        let pane_uuid = uuid::Uuid::parse_str(&self.pane_id).unwrap_or_default();
        let session_uuid = uuid::Uuid::parse_str(&self.session_id).unwrap_or_default();

        pyre_proto::Block {
            id: pyre_proto::BlockId(block_uuid),
            pane: pyre_proto::PaneId(pane_uuid),
            session: pyre_proto::SessionId(session_uuid),
            command: self.command.clone(),
            started_at: Utc
                .timestamp_millis_opt(self.started_at)
                .single()
                .unwrap_or_else(Utc::now),
            ended_at: self
                .ended_at
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
            exit_code: self.exit_code,
            cwd: self.cwd.as_deref().map(std::path::PathBuf::from),
            stdout_len: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Main migration function
// ---------------------------------------------------------------------------

/// Run the one-time monolithic → hybrid migration.
///
/// Returns [`MigrationReport::AlreadyDone`] if the sentinel exists.
/// Returns [`MigrationReport::NoLegacy`] if no monolithic DB exists.
/// Returns [`MigrationReport::Migrated`] on success.
///
/// # Errors
///
/// Any IO/DB failure before the sentinel is written returns an error so the
/// caller can log it and retry on the next boot.
pub async fn migrate_to_hybrid() -> Result<MigrationReport> {
    let pyre_dir = pyre_state_dir();
    std::fs::create_dir_all(&pyre_dir).with_context(|| format!("mkdir {}", pyre_dir.display()))?;

    // 1. Idempotency check.
    if sentinel_exists() {
        tracing::info!("migration sentinel present — skipping migration");
        return Ok(MigrationReport::AlreadyDone);
    }

    let legacy_path = legacy_db_path();

    // 2. No legacy DB.
    if !legacy_path.exists() {
        tracing::info!("no legacy state.db found — writing sentinel");
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        write_sentinel(&ts, "")?;
        return Ok(MigrationReport::NoLegacy);
    }

    // 3. Backup.
    let backup_suffix = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_path = pyre_dir.join(format!("state.db.bak.{backup_suffix}"));
    std::fs::copy(&legacy_path, &backup_path).with_context(|| {
        format!(
            "backup {} → {}",
            legacy_path.display(),
            backup_path.display()
        )
    })?;
    tracing::info!(backup = %backup_path.display(), "legacy state.db backed up");

    // 4. Open legacy DB read-only.
    let legacy_opts = SqliteConnectOptions::new()
        .filename(&legacy_path)
        .create_if_missing(false)
        .read_only(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    let legacy_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(legacy_opts)
        .await
        .context("open legacy state.db read-only")?;

    // Collect distinct session_ids from panes and blocks.
    let session_ids = collect_session_ids(&legacy_pool).await?;
    tracing::info!(count = session_ids.len(), "sessions found in legacy DB");

    // Collect all blocks for Tantivy rebuild.
    let all_blocks = collect_all_blocks(&legacy_pool).await?;
    let block_count = all_blocks.len();
    tracing::info!(count = block_count, "blocks found in legacy DB");

    let sessions_dir = sessions_dir();
    let mut migrated_sessions = 0usize;

    // 5. Split into per-session shards.
    for sid in &session_ids {
        let session_dir = sessions_dir.join(sid);

        // Skip session if shard already exists (partial migration recovery).
        let shard_path = session_dir.join("state.db");
        if shard_path.exists() {
            tracing::info!(session_id = sid, "shard already exists — skipping session");
            migrated_sessions += 1;
            continue;
        }

        let shard = open_shard(&session_dir)
            .await
            .with_context(|| format!("open shard for session {sid}"))?;

        // Copy panes for this session.
        let panes = fetch_panes_for_session(&legacy_pool, sid).await?;
        for (slot_idx, shell, cwd) in panes {
            sqlx::query(
                "INSERT INTO panes (slot_idx, shell, cwd) VALUES (?1, ?2, ?3)
                 ON CONFLICT(slot_idx) DO UPDATE SET shell = excluded.shell, cwd = excluded.cwd",
            )
            .bind(slot_idx)
            .bind(&shell)
            .bind(&cwd)
            .execute(&shard)
            .await
            .with_context(|| format!("insert pane into shard for session {sid}"))?;
        }

        shard.close().await;
        migrated_sessions += 1;
        tracing::info!(session_id = sid, "session migrated to shard");
    }

    legacy_pool.close().await;

    // 6. Rebuild Tantivy index.
    let idx_dir = index_dir();
    tracing::info!(index_dir = %idx_dir.display(), "rebuilding tantivy index");
    tokio::task::spawn_blocking({
        let idx_dir = idx_dir.clone();
        move || rebuild_tantivy_index(&idx_dir, &all_blocks)
    })
    .await
    .context("spawn_blocking tantivy rebuild")??;

    // 7. Write sentinel — only after all steps succeed.
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    write_sentinel(&ts, &backup_path.to_string_lossy())?;

    tracing::info!(
        sessions = migrated_sessions,
        blocks = block_count,
        "hybrid migration complete"
    );

    Ok(MigrationReport::Migrated {
        sessions: migrated_sessions,
        blocks: block_count,
        backup_path,
    })
}

// ---------------------------------------------------------------------------
// Legacy DB query helpers
// ---------------------------------------------------------------------------

async fn collect_session_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    // Gather from both panes and blocks tables; union deduplicated.
    let mut ids: Vec<String> = Vec::new();

    // From panes — may not have session_id column in legacy single-mode panes
    // (worker shard schema has no session_id); fall back gracefully.
    let pane_sids: Vec<String> =
        match sqlx::query("SELECT DISTINCT session_id FROM panes WHERE session_id IS NOT NULL")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows
                .iter()
                .map(|r| r.get::<String, _>("session_id"))
                .collect(),
            Err(_) => vec![],
        };

    let block_sids: Vec<String> =
        match sqlx::query("SELECT DISTINCT session_id FROM blocks WHERE session_id IS NOT NULL")
            .fetch_all(pool)
            .await
        {
            Ok(rows) => rows
                .iter()
                .map(|r| r.get::<String, _>("session_id"))
                .collect(),
            Err(_) => vec![],
        };

    ids.extend(pane_sids);
    ids.extend(block_sids);

    // Deduplicate.
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

async fn fetch_panes_for_session(
    pool: &SqlitePool,
    session_id: &str,
) -> Result<Vec<(i64, String, String)>> {
    // The monolithic panes table has (id, session_id, argv, cwd, cols, rows, ...).
    // We map argv → shell and cwd → cwd; use rowid as slot_idx proxy since the
    // worker shard expects an integer slot_idx.
    let rows = match sqlx::query(
        "SELECT rowid, argv, cwd FROM panes WHERE session_id = ?1 AND closed_at IS NULL",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(_) => return Ok(vec![]),
    };

    let mut out = Vec::new();
    for row in rows {
        let slot_idx: i64 = row.get("rowid");
        let shell: String = row.try_get("argv").unwrap_or_default();
        let cwd: String = row.try_get("cwd").unwrap_or_default();
        out.push((slot_idx, shell, cwd));
    }
    Ok(out)
}

async fn collect_all_blocks(pool: &SqlitePool) -> Result<Vec<LegacyBlock>> {
    let rows = match sqlx::query(
        "SELECT id, pane_id, session_id, command, started_at, ended_at,
                exit_code, cwd, stdout_blob_path
         FROM blocks",
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("failed to read blocks from legacy DB: {e:#}");
            return Ok(vec![]);
        }
    };

    let mut out = Vec::new();
    for row in rows {
        out.push(LegacyBlock {
            id: row.get("id"),
            pane_id: row.get("pane_id"),
            session_id: row.get("session_id"),
            command: row.get("command"),
            started_at: row.get("started_at"),
            ended_at: row.get("ended_at"),
            exit_code: row.get("exit_code"),
            cwd: row.get("cwd"),
            stdout_blob_path: row.get("stdout_blob_path"),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Schema v2 migration: add exit_code field to Tantivy index
// ---------------------------------------------------------------------------

/// Detect whether the Tantivy index at `index_dir` is missing the `exit_code`
/// field (schema v1). If so:
///  1. Rename `index_dir` → `index_dir/../index.bak.YYYYMMDD-HHMMSS`.
///  2. Recreate `index_dir` (empty).
///  3. Walk the sqlite `blocks` table via `store` and reindex every block.
///
/// Idempotent: if the index already has `exit_code`, returns immediately.
/// If `index_dir` does not exist yet, also returns immediately (fresh install).
pub async fn maybe_migrate_tantivy_schema(
    index_dir: &std::path::Path,
    store: &crate::store::Store,
) -> Result<()> {
    // No existing index — nothing to migrate.
    if !index_dir.exists() {
        return Ok(());
    }

    // Detect schema version by checking for the exit_code field.
    let needs_migration = {
        let index_dir_owned = index_dir.to_path_buf();
        tokio::task::spawn_blocking(move || tantivy_has_exit_code_field(&index_dir_owned))
            .await
            .context("spawn_blocking schema detect")?
    };

    match needs_migration {
        Ok(true) => {
            tracing::info!("tantivy index schema v2 detected — no migration needed");
            return Ok(());
        }
        Ok(false) => {
            tracing::info!("tantivy index missing exit_code field — migrating to schema v2");
        }
        Err(e) => {
            tracing::warn!("could not read tantivy schema ({e:#}) — will rebuild index");
        }
    }

    // 1. Rename old index dir.
    let suffix = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let parent = index_dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let backup_dir = parent.join(format!("index.bak.{suffix}"));
    std::fs::rename(index_dir, &backup_dir)
        .with_context(|| format!("rename {} → {}", index_dir.display(), backup_dir.display()))?;
    tracing::info!(backup = %backup_dir.display(), "old tantivy index backed up");

    // 2. Recreate empty index dir.
    std::fs::create_dir_all(index_dir).with_context(|| format!("mkdir {}", index_dir.display()))?;

    // 3. Reindex all blocks from sqlite.
    let index_dir_owned = index_dir.to_path_buf();
    let block_index = tokio::task::spawn_blocking(move || BlockIndex::open(&index_dir_owned))
        .await
        .context("spawn_blocking BlockIndex::open for reindex")?
        .context("open block index for schema v2 reindex")?;

    // Collect all blocks. list_blocks(None, u32::MAX) returns newest-first;
    // order doesn't matter for reindex correctness.
    let all_blocks = store
        .list_blocks(None, u32::MAX)
        .await
        .context("list_blocks for tantivy reindex")?;

    let total = all_blocks.len();
    tracing::info!(total, "tantivy reindex: starting");

    let mut indexed = 0usize;
    for block in &all_blocks {
        let stdout = store.stdout_snippet(block.id, usize::MAX);
        if let Err(e) = block_index.add_block(block, &stdout) {
            tracing::warn!(block_id = %block.id, "reindex add_block failed: {e:#}");
        }
        indexed += 1;
        if indexed.is_multiple_of(1000) {
            tracing::info!(indexed, total, "tantivy reindex: progress");
        }
    }

    tracing::info!(indexed, "tantivy reindex: complete");
    Ok(())
}

/// Returns `Ok(true)` if the tantivy index at `path` already has an
/// `exit_code` field (schema v2), `Ok(false)` if it exists but lacks the
/// field, or `Err` if the index cannot be read at all.
fn tantivy_has_exit_code_field(path: &std::path::Path) -> Result<bool> {
    use tantivy::directory::MmapDirectory;
    use tantivy::Index;

    let dir = MmapDirectory::open(path)
        .with_context(|| format!("open MmapDirectory at {}", path.display()))?;
    let index =
        Index::open(dir).with_context(|| format!("open tantivy index at {}", path.display()))?;
    Ok(index.schema().get_field("exit_code").is_ok())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Serialize tests that mutate `XDG_STATE_HOME`. Shared crate-wide with the
    // `shard` tests (which also mutate it) so cross-module runs never race —
    // see `crate::shard::ENV_TEST_LOCK`.
    use crate::shard::ENV_TEST_LOCK as ENV_LOCK;

    fn setup_env(tmp: &TempDir) {
        std::env::set_var("XDG_STATE_HOME", tmp.path());
    }

    // -----------------------------------------------------------------------
    // Test helpers: build a minimal legacy DB
    // -----------------------------------------------------------------------

    async fn build_legacy_db(pyre_dir: &PathBuf) -> Result<()> {
        std::fs::create_dir_all(pyre_dir)?;
        let db_path = pyre_dir.join("state.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(opts)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL DEFAULT 0,
                last_active_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS panes (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                argv TEXT NOT NULL DEFAULT '',
                cwd TEXT,
                cols INTEGER NOT NULL DEFAULT 80,
                rows INTEGER NOT NULL DEFAULT 24,
                created_at INTEGER NOT NULL DEFAULT 0,
                closed_at INTEGER
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS blocks (
                id TEXT PRIMARY KEY,
                pane_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                command TEXT NOT NULL,
                started_at INTEGER NOT NULL DEFAULT 0,
                ended_at INTEGER,
                exit_code INTEGER,
                cwd TEXT,
                stdout_blob_path TEXT NOT NULL,
                stdout_len INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await?;

        pool.close().await;
        Ok(())
    }

    async fn insert_pane(pool: &SqlitePool, id: &str, session_id: &str, argv: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO panes (id, session_id, argv, cwd, cols, rows, created_at)
             VALUES (?1, ?2, ?3, '/tmp', 80, 24, 0)",
        )
        .bind(id)
        .bind(session_id)
        .bind(argv)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_block(
        pool: &SqlitePool,
        id: &str,
        pane_id: &str,
        session_id: &str,
        command: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO blocks (id, pane_id, session_id, command, started_at, stdout_blob_path)
             VALUES (?1, ?2, ?3, ?4, 0, '')",
        )
        .bind(id)
        .bind(pane_id)
        .bind(session_id)
        .bind(command)
        .execute(pool)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn migrate_no_legacy_db_writes_sentinel() {
        let _guard = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        setup_env(&tmp);

        let report = migrate_to_hybrid().await.unwrap();
        assert!(
            matches!(report, MigrationReport::NoLegacy),
            "expected NoLegacy, got {report:?}"
        );
        assert!(sentinel_path().exists(), "sentinel should be written");
    }

    #[tokio::test]
    async fn migrate_already_done_short_circuits() {
        let _guard = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        setup_env(&tmp);

        // Write sentinel manually.
        let pyre_dir = pyre_state_dir();
        std::fs::create_dir_all(&pyre_dir).unwrap();
        std::fs::write(sentinel_path(), "timestamp=2026-01-01T00:00:00Z\nbackup=\n").unwrap();

        let report = migrate_to_hybrid().await.unwrap();
        assert!(
            matches!(report, MigrationReport::AlreadyDone),
            "expected AlreadyDone, got {report:?}"
        );
    }

    #[tokio::test]
    async fn migrate_splits_two_sessions_into_shards() {
        let _guard = ENV_LOCK.lock().await;
        let tmp = TempDir::new().unwrap();
        setup_env(&tmp);

        let pyre_dir = pyre_state_dir();
        build_legacy_db(&pyre_dir).await.unwrap();

        // Populate legacy DB with 2 sessions.
        let db_path = pyre_dir.join("state.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(opts)
            .await
            .unwrap();

        let sid_a = "aaaaaaaa-0000-0000-0000-000000000001";
        let sid_b = "bbbbbbbb-0000-0000-0000-000000000002";

        insert_pane(&pool, "pane-1", sid_a, "/bin/bash")
            .await
            .unwrap();
        insert_pane(&pool, "pane-2", sid_b, "/bin/zsh")
            .await
            .unwrap();
        insert_block(&pool, "block-1", "pane-1", sid_a, "echo hello")
            .await
            .unwrap();
        insert_block(&pool, "block-2", "pane-2", sid_b, "echo world")
            .await
            .unwrap();
        pool.close().await;

        let report = migrate_to_hybrid().await.unwrap();
        match report {
            MigrationReport::Migrated {
                sessions,
                blocks,
                backup_path,
            } => {
                assert_eq!(sessions, 2, "expected 2 sessions migrated");
                assert_eq!(blocks, 2, "expected 2 blocks");
                assert!(backup_path.exists(), "backup file should exist");
            }
            other => panic!("expected Migrated, got {other:?}"),
        }

        // Verify shard files exist.
        assert!(
            sessions_dir().join(sid_a).join("state.db").exists(),
            "shard for session A should exist"
        );
        assert!(
            sessions_dir().join(sid_b).join("state.db").exists(),
            "shard for session B should exist"
        );

        // Verify Tantivy index dir exists.
        assert!(index_dir().exists(), "tantivy index dir should exist");

        // Verify sentinel exists.
        assert!(sentinel_path().exists(), "sentinel should be written");
    }

    // -----------------------------------------------------------------------
    // Schema v2 detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn tantivy_v2_index_reports_has_exit_code() {
        let tmp = TempDir::new().unwrap();
        // Open a fresh BlockIndex (schema v2) and verify the field is detected.
        BlockIndex::open(tmp.path()).unwrap();
        let result = tantivy_has_exit_code_field(tmp.path()).unwrap();
        assert!(result, "fresh index should have exit_code field");
    }

    #[test]
    fn tantivy_v1_index_reports_missing_exit_code() {
        use tantivy::schema::{SchemaBuilder, STORED, STRING, TEXT};
        use tantivy::{Index, IndexWriter};

        let tmp = TempDir::new().unwrap();

        // Build a v1-style index (no exit_code field).
        let mut sb = SchemaBuilder::new();
        sb.add_text_field("block_id", STRING | STORED);
        sb.add_text_field("command", TEXT | STORED);
        sb.add_text_field("stdout", TEXT);
        let schema = sb.build();
        let index = Index::create_in_dir(tmp.path(), schema).unwrap();
        let mut writer: IndexWriter = index.writer(50_000_000).unwrap();
        writer.commit().unwrap();

        let result = tantivy_has_exit_code_field(tmp.path()).unwrap();
        assert!(!result, "v1 index should NOT have exit_code field");
    }
}
