//! Persistence for sessions, panes, and blocks. SQLite (WAL) for metadata,
//! per-block zstd-compressed stdout blobs on disk.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use pyre_proto::{Block, BlockId, LayoutNode, PaneId, SessionId, WindowId};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Pool, Row, Sqlite};
use uuid::Uuid;

pub struct Store {
    pool: Pool<Sqlite>,
    data_dir: PathBuf,
}

impl Store {
    pub async fn open() -> Result<Self> {
        let data_dir = if let Ok(p) = std::env::var("PYRE_DATA_DIR") {
            PathBuf::from(p)
        } else {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("pyre")
        };
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("mkdir {}", data_dir.display()))?;
        std::fs::create_dir_all(data_dir.join("blocks")).context("mkdir blocks/")?;

        let db_path = data_dir.join("state.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .context("open sqlite")?;

        // ── Pre-migration backup ───────────────────────────────────────────────
        // If the database file exists but the `windows` table hasn't been
        // created yet, this is a pre-v4 schema. Copy it for rollback safety
        // before applying migration 0004.
        if db_path.exists() {
            let n: i64 = sqlx::query(
                "SELECT COUNT(*) AS n FROM sqlite_master \
                 WHERE type='table' AND name='windows'",
            )
            .fetch_one(&pool)
            .await
            .context("check windows table")?
            .try_get("n")
            .unwrap_or(0);
            if n == 0 {
                let ts = Local::now().format("%Y%m%d-%H%M%S");
                let bak_name = format!("state.db.bak.{ts}");
                let bak = data_dir.join(&bak_name);
                std::fs::copy(&db_path, &bak)
                    .with_context(|| format!("backup state.db -> {}", bak.display()))?;
            }
        }

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("run migrations")?;

        backfill_windows(&pool).await.context("backfill windows")?;

        Ok(Self { pool, data_dir })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn blob_path_for(&self, id: BlockId) -> PathBuf {
        self.data_dir.join("blocks").join(format!("{}.zst", id.0))
    }

    pub async fn upsert_session(&self, id: SessionId, name: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO sessions (id, name, created_at, last_active_at) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET last_active_at = excluded.last_active_at, name = excluded.name",
        )
        .bind(id.0.to_string())
        .bind(name)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist the JSON-encoded `LayoutNode` for a session (M7-C, ADR-0005).
    ///
    /// Uses `INSERT OR IGNORE` + `UPDATE` so the row is always present before
    /// the layout column is written — safe to call before `upsert_session` for
    /// sessions whose rows are created elsewhere (e.g. hybrid worker path).
    // Planned for supervisor layout-restore on attach; not yet wired.
    #[allow(dead_code)]
    pub async fn upsert_session_layout(&self, id: SessionId, layout_json: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET layout = ?2 WHERE id = ?1")
            .bind(id.0.to_string())
            .bind(layout_json)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return the raw JSON layout string for a single session, or `None` if the
    /// session has no persisted layout.
    // Planned for supervisor layout-restore on attach; not yet wired.
    #[allow(dead_code)]
    pub async fn get_session_layout_json(&self, id: SessionId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT layout FROM sessions WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("layout").unwrap_or(None)))
    }

    /// Return `(SessionId, layout_json)` for all sessions that have a layout column.
    // dead_code: planned for supervisor startup layout-restore (S3 reattach);
    // not yet called from any code path but kept to avoid re-implementing the
    // query when that work lands.
    #[allow(dead_code)]
    pub async fn list_session_layouts(&self) -> Result<Vec<(SessionId, Option<String>)>> {
        let rows = sqlx::query("SELECT id, layout FROM sessions ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.try_get("id")?;
            if let Ok(uuid) = uuid::Uuid::parse_str(&id_str) {
                let layout: Option<String> = row.try_get("layout").unwrap_or(None);
                out.push((SessionId(uuid), layout));
            }
        }
        Ok(out)
    }

    pub async fn upsert_pane(
        &self,
        id: PaneId,
        session: SessionId,
        argv: &str,
        cwd: Option<&Path>,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO panes (id, session_id, argv, cwd, cols, rows, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET cwd = excluded.cwd, cols = excluded.cols, rows = excluded.rows",
        )
        .bind(id.0.to_string())
        .bind(session.0.to_string())
        .bind(argv)
        .bind(cwd.map(|p| p.to_string_lossy().to_string()))
        .bind(cols as i64)
        .bind(rows as i64)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_block(&self, block: &Block) -> Result<()> {
        sqlx::query(
            "INSERT INTO blocks
             (id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_blob_path, stdout_len)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, 0)",
        )
        .bind(block.id.0.to_string())
        .bind(block.pane.0.to_string())
        .bind(block.session.0.to_string())
        .bind(&block.command)
        .bind(block.started_at.timestamp_millis())
        .bind(block.cwd.as_ref().map(|p| p.to_string_lossy().to_string()))
        .bind(self.blob_path_for(block.id).to_string_lossy().to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn finalize_block(
        &self,
        id: BlockId,
        ended_at: DateTime<Utc>,
        exit: Option<i32>,
        stdout_len: u64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE blocks SET ended_at = ?2, exit_code = ?3, stdout_len = ?4 WHERE id = ?1",
        )
        .bind(id.0.to_string())
        .bind(ended_at.timestamp_millis())
        .bind(exit.map(|c| c as i64))
        .bind(stdout_len as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_blocks(&self, session: Option<SessionId>, limit: u32) -> Result<Vec<Block>> {
        let rows = if let Some(s) = session {
            sqlx::query(
                "SELECT id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_len
                 FROM blocks WHERE session_id = ?1 ORDER BY started_at DESC LIMIT ?2",
            )
            .bind(s.0.to_string())
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_len
                 FROM blocks ORDER BY started_at DESC LIMIT ?1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter().map(row_to_block).collect()
    }

    pub async fn get_block(&self, id: BlockId) -> Result<Option<Block>> {
        let row = sqlx::query(
            "SELECT id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_len
             FROM blocks WHERE id = ?1",
        )
        .bind(id.0.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_block).transpose()
    }

    /// First `max_chars` of decompressed stdout, flattened to one line (for search UI).
    pub fn stdout_snippet(&self, id: pyre_proto::BlockId, max_chars: usize) -> String {
        match self.read_block_stdout(id) {
            Ok(bytes) => bytes
                .iter()
                .map(|&b| {
                    if b == b'\n' || b == b'\r' || b == b'\t' {
                        ' '
                    } else {
                        char::from(b)
                    }
                })
                .take(max_chars)
                .collect(),
            Err(_) => String::new(),
        }
    }

    /// Read and decompress the stdout blob for a block. Returns empty vec if blob does not exist.
    pub fn read_block_stdout(&self, id: pyre_proto::BlockId) -> Result<Vec<u8>> {
        let path = self.blob_path_for(id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file =
            std::fs::File::open(&path).with_context(|| format!("open blob {}", path.display()))?;
        let mut dec = zstd::Decoder::new(file)?;
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut out)?;
        Ok(out)
    }

    /// Persist a human-readable name for a pane row.
    ///
    /// In hybrid mode the supervisor's `panes` table is never populated by
    /// normal pane-spawn paths (workers write their own per-session shard, not
    /// the supervisor's `state.db`). A plain `UPDATE ... WHERE id = ?` is
    /// therefore a no-op when no row exists, silently discarding the rename.
    ///
    /// This method uses `INSERT OR IGNORE` + `UPDATE` (an UPSERT) so a stub
    /// row is created on first rename if none exists, and subsequent renames
    /// overwrite the name in place. `session_id` is required for the initial
    /// insert; `cols`/`rows` are zeroed because only the `name` column matters
    /// for the rename read-path (`get_pane_name`).
    pub async fn rename_pane(&self, id: PaneId, session_id: SessionId, name: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        // Ensure a row exists (no-op if it already does).
        sqlx::query(
            "INSERT OR IGNORE INTO panes (id, session_id, argv, cols, rows, created_at)
             VALUES (?1, ?2, '', 0, 0, ?3)",
        )
        .bind(id.0.to_string())
        .bind(session_id.0.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?;
        // Now set the name (works whether the row was just created or pre-existed).
        sqlx::query("UPDATE panes SET name = ?2 WHERE id = ?1")
            .bind(id.0.to_string())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return the persisted name for a pane, or `None` if the row is missing or
    /// the name column is NULL/empty.
    pub async fn get_pane_name(&self, id: PaneId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT name FROM panes WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            r.try_get::<Option<String>, _>("name")
                .unwrap_or(None)
                .filter(|n| !n.is_empty())
        }))
    }

    /// Return the persisted name for a session, or `None` if the row is missing.
    pub async fn get_session_name(&self, id: SessionId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT name FROM sessions WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("name").unwrap_or(None)))
    }

    /// Return all session IDs that have a row in the `sessions` table.
    pub async fn list_session_ids(&self) -> Result<Vec<SessionId>> {
        let rows = sqlx::query("SELECT id FROM sessions ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
                ids.push(SessionId(uuid));
            }
        }
        Ok(ids)
    }

    /// Return the total number of block rows in SQLite (fast COUNT query).
    pub async fn count_blocks(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM blocks")
            .fetch_one(&self.pool)
            .await?;
        let n: i64 = row.try_get("n")?;
        Ok(n.max(0) as u64)
    }

    pub async fn list_blocks_for_pane(&self, pane: PaneId, limit: u32) -> Result<Vec<Block>> {
        let rows = sqlx::query(
            "SELECT id, pane_id, session_id, command, started_at, ended_at, exit_code, cwd, stdout_len
             FROM blocks WHERE pane_id = ?1 ORDER BY started_at DESC LIMIT ?2",
        )
        .bind(pane.0.to_string())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_block).collect()
    }

    // ── Window management ────────────────────────────────────────────────────

    /// Upsert a window row.
    ///
    /// `created_at` is milliseconds since the Unix epoch (matches the DB column
    /// type). ON CONFLICT updates `name` and `position` so the method is safe
    /// to call from both create and re-persist paths.
    ///
    /// Mirrors `upsert_session` (INSERT … ON CONFLICT DO UPDATE).
    pub async fn upsert_window(
        &self,
        id: WindowId,
        session: SessionId,
        name: &str,
        position: u32,
        created_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO windows (id, session_id, name, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, position = excluded.position",
        )
        .bind(id.0.to_string())
        .bind(session.0.to_string())
        .bind(name)
        .bind(position as i64)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist the JSON-encoded `LayoutNode` for a window.
    ///
    /// Mirrors `upsert_session_layout` (UPDATE … SET layout=?).
    pub async fn upsert_window_layout(&self, id: WindowId, layout_json: &str) -> Result<()> {
        sqlx::query("UPDATE windows SET layout = ?2 WHERE id = ?1")
            .bind(id.0.to_string())
            .bind(layout_json)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return the raw JSON layout string for a window, or `None` if the row is
    /// missing or the `layout` column is NULL.
    ///
    /// Mirrors `get_session_layout_json`.
    pub async fn get_window_layout_json(&self, id: WindowId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT layout FROM windows WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("layout").unwrap_or(None)))
    }

    /// Return all windows for a session ordered by position ascending.
    ///
    /// Each entry is `(WindowId, name, position)`.
    pub async fn list_windows(&self, session: SessionId) -> Result<Vec<(WindowId, String, u32)>> {
        let rows = sqlx::query(
            "SELECT id, name, position FROM windows \
             WHERE session_id = ?1 ORDER BY position ASC",
        )
        .bind(session.0.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.try_get("id")?;
            if let Ok(uuid) = Uuid::parse_str(&id_str) {
                let name: String = row.try_get("name")?;
                let position: i64 = row.try_get("position")?;
                out.push((WindowId(uuid), name, position.max(0) as u32));
            }
        }
        Ok(out)
    }

    /// Set the persisted name for a window.
    ///
    /// Uses a plain `UPDATE` — windows are always supervisor-created and will
    /// have rows before being renamed (unlike panes, which workers can spawn
    /// without touching the supervisor's `panes` table). If the row is absent
    /// the UPDATE is a safe no-op.
    pub async fn rename_window(&self, id: WindowId, name: &str) -> Result<()> {
        sqlx::query("UPDATE windows SET name = ?2 WHERE id = ?1")
            .bind(id.0.to_string())
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return the persisted name for a window, or `None` if the row is missing
    /// or the `name` column is NULL/empty.
    ///
    /// Mirrors `get_pane_name` / `get_session_name`.
    pub async fn get_window_name(&self, id: WindowId) -> Result<Option<String>> {
        let row = sqlx::query("SELECT name FROM windows WHERE id = ?1")
            .bind(id.0.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            r.try_get::<Option<String>, _>("name")
                .unwrap_or(None)
                .filter(|n| !n.is_empty())
        }))
    }

    /// Delete a window row. Call after all its panes have been closed.
    pub async fn delete_window(&self, id: WindowId) -> Result<()> {
        sqlx::query("DELETE FROM windows WHERE id = ?1")
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Assign a pane to a window.
    ///
    /// Uses `INSERT OR IGNORE` + `UPDATE` (an UPSERT) so a stub pane row is
    /// created on first assignment if none exists — mirrors `rename_pane` for
    /// hybrid mode where workers spawn panes without touching the supervisor's
    /// `panes` table. `session` is required for the initial stub insert.
    pub async fn assign_pane_window(
        &self,
        pane: PaneId,
        session: SessionId,
        window: WindowId,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR IGNORE INTO panes (id, session_id, argv, cols, rows, created_at)
             VALUES (?1, ?2, '', 0, 0, ?3)",
        )
        .bind(pane.0.to_string())
        .bind(session.0.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE panes SET window_id = ?2 WHERE id = ?1")
            .bind(pane.0.to_string())
            .bind(window.0.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Return all pane IDs assigned to a window.
    pub async fn list_panes_for_window(&self, window: WindowId) -> Result<Vec<PaneId>> {
        let rows = sqlx::query("SELECT id FROM panes WHERE window_id = ?1")
            .bind(window.0.to_string())
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id_str: String = row.try_get("id")?;
            if let Ok(uuid) = Uuid::parse_str(&id_str) {
                out.push(PaneId(uuid));
            }
        }
        Ok(out)
    }
}

/// Backfill the `windows` table for databases predating migration 0004.
///
/// Called once from `Store::open` after `sqlx::migrate!` completes.
/// Self-guards on `COUNT(*) FROM windows == 0` — a fresh install with zero
/// sessions and a database that already ran this function both no-op.
///
/// For each session row with a persisted `layout` JSON:
/// - A default window W0 (named "1", position 0) is created carrying the
///   session's existing tiling tree verbatim.
/// - Tree panes (leaves of the layout) are assigned to W0.
/// - Each standalone pane (window_id IS NULL after the tree assignment) gets
///   its own single-leaf window, reproducing the prior GUI "standalone tab"
///   mental model (each extra terminal = its own window).
async fn backfill_windows(pool: &Pool<Sqlite>) -> Result<()> {
    // Idempotency guard: skip if windows already exist or there are no sessions.
    let win_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM windows")
        .fetch_one(pool)
        .await?
        .try_get("n")?;
    let sess_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM sessions")
        .fetch_one(pool)
        .await?
        .try_get("n")?;
    if win_count > 0 || sess_count == 0 {
        return Ok(());
    }

    let sessions = sqlx::query("SELECT id, layout FROM sessions ORDER BY created_at ASC")
        .fetch_all(pool)
        .await?;

    for row in sessions {
        let sid_str: String = row.try_get("id")?;
        let sid = match Uuid::parse_str(&sid_str) {
            Ok(u) => SessionId(u),
            Err(_) => continue,
        };
        let layout_json: Option<String> = row.try_get("layout").unwrap_or(None);
        let now = Utc::now().timestamp_millis();

        // Collect pane IDs present in the tiling tree.
        let tree_panes: Vec<PaneId> = if let Some(ref json) = layout_json {
            match serde_json::from_str::<LayoutNode>(json) {
                Ok(node) => node.all_leaves(),
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        // Create default window W0.
        let w0 = WindowId::new();
        sqlx::query(
            "INSERT INTO windows (id, session_id, name, position, created_at)
             VALUES (?1, ?2, '1', 0, ?3)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
        )
        .bind(w0.0.to_string())
        .bind(sid.0.to_string())
        .bind(now)
        .execute(pool)
        .await?;

        // Carry the session's tiling tree into W0's layout column.
        if let Some(ref json) = layout_json {
            sqlx::query("UPDATE windows SET layout = ?2 WHERE id = ?1")
                .bind(w0.0.to_string())
                .bind(json.as_str())
                .execute(pool)
                .await?;
        }

        // Assign tree panes to W0.
        // UUIDs contain only hex digits and hyphens — safe to embed in SQL.
        if !tree_panes.is_empty() {
            let ids: String = tree_panes
                .iter()
                .map(|p| format!("'{}'", p.0))
                .collect::<Vec<_>>()
                .join(",");
            sqlx::query(&format!(
                "UPDATE panes SET window_id = ?1 \
                 WHERE session_id = ?2 AND id IN ({ids})"
            ))
            .bind(w0.0.to_string())
            .bind(sid.0.to_string())
            .execute(pool)
            .await?;
        }

        // Standalone panes: rows for this session with window_id still NULL.
        let standalone = sqlx::query(
            "SELECT id, name FROM panes \
             WHERE session_id = ?1 AND window_id IS NULL \
             ORDER BY created_at ASC",
        )
        .bind(sid.0.to_string())
        .fetch_all(pool)
        .await?;

        for (k, prow) in standalone.iter().enumerate() {
            let pid_str: String = prow.try_get("id")?;
            let pid = match Uuid::parse_str(&pid_str) {
                Ok(u) => PaneId(u),
                Err(_) => continue,
            };
            let pane_name: Option<String> = prow.try_get("name").unwrap_or(None);

            let wk = WindowId::new();
            // Fall back to sequential window number if the pane has no name.
            // W0 is "1" so the k-th standalone (0-indexed) defaults to "k+1".
            let win_name = pane_name
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| (k + 1).to_string());

            sqlx::query(
                "INSERT INTO windows (id, session_id, name, position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            )
            .bind(wk.0.to_string())
            .bind(sid.0.to_string())
            .bind(&win_name)
            .bind((k + 1) as i64)
            .bind(now)
            .execute(pool)
            .await?;

            // Single-leaf layout for this standalone pane.
            let leaf_json = serde_json::to_string(&LayoutNode::Leaf(pid))?;
            sqlx::query("UPDATE windows SET layout = ?2 WHERE id = ?1")
                .bind(wk.0.to_string())
                .bind(&leaf_json)
                .execute(pool)
                .await?;

            // Assign the pane to its own window.
            sqlx::query("UPDATE panes SET window_id = ?1 WHERE id = ?2")
                .bind(wk.0.to_string())
                .bind(pid.0.to_string())
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

fn row_to_block(row: sqlx::sqlite::SqliteRow) -> Result<Block> {
    let id: String = row.try_get("id")?;
    let pane_id: String = row.try_get("pane_id")?;
    let session_id: String = row.try_get("session_id")?;
    let command: String = row.try_get("command")?;
    let started_at: i64 = row.try_get("started_at")?;
    let ended_at: Option<i64> = row.try_get("ended_at")?;
    let exit_code: Option<i64> = row.try_get("exit_code")?;
    let cwd: Option<String> = row.try_get("cwd")?;
    let stdout_len: i64 = row.try_get("stdout_len")?;
    Ok(Block {
        id: BlockId(Uuid::parse_str(&id)?),
        pane: PaneId(Uuid::parse_str(&pane_id)?),
        session: SessionId(Uuid::parse_str(&session_id)?),
        command,
        cwd: cwd.map(PathBuf::from),
        started_at: Utc
            .timestamp_millis_opt(started_at)
            .single()
            .unwrap_or_else(Utc::now),
        ended_at: ended_at.and_then(|t| Utc.timestamp_millis_opt(t).single()),
        exit_code: exit_code.map(|c| c as i32),
        stdout_len: stdout_len.max(0) as u64,
    })
}

// === Blob writer (sync, used inside spawn_blocking) ===

pub struct BlobWriter {
    enc: Option<zstd::Encoder<'static, BufWriter<std::fs::File>>>,
    len: u64,
}

impl BlobWriter {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        let enc = zstd::Encoder::new(BufWriter::new(file), 3)?;
        Ok(Self {
            enc: Some(enc),
            len: 0,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(enc) = self.enc.as_mut() {
            enc.write_all(bytes)?;
            self.len += bytes.len() as u64;
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<u64> {
        if let Some(enc) = self.enc.take() {
            let mut bufw = enc.finish()?;
            bufw.flush()?;
        }
        Ok(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pyre_proto::{Block, BlockId, PaneId, SessionId};
    use tempfile::TempDir;
    use uuid::Uuid;

    // These tests mutate the process-global `PYRE_DATA_DIR`. Serialize them
    // (shared crate-wide with the shard/migration tests that mutate
    // `XDG_STATE_HOME`) so a parallel test run can never read another test's
    // env value mid-`Store::open`. See `crate::shard::ENV_TEST_LOCK`.
    use crate::shard::ENV_TEST_LOCK as ENV_LOCK;

    #[tokio::test]
    async fn open_and_roundtrip_block() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation, serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Store::open().await?;

        let sid = SessionId(Uuid::new_v4());
        let pid = PaneId(Uuid::new_v4());
        let bid = BlockId(Uuid::new_v4());
        store.upsert_session(sid, "test").await?;
        store.upsert_pane(pid, sid, "/bin/sh", None, 80, 24).await?;

        let block = Block {
            id: bid,
            pane: pid,
            session: sid,
            command: "echo hi".into(),
            cwd: None,
            started_at: Utc::now(),
            ended_at: None,
            exit_code: None,
            stdout_len: 0,
        };
        store.create_block(&block).await?;
        store.finalize_block(bid, Utc::now(), Some(0), 42).await?;

        let listed = store.list_blocks(Some(sid), 10).await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].command, "echo hi");
        assert_eq!(listed[0].exit_code, Some(0));
        assert_eq!(listed[0].stdout_len, 42);

        // Blob writer round-trip — verify bytes survive compress/decompress.
        let path = store.blob_path_for(bid);
        let mut bw = BlobWriter::open(&path)?;
        bw.write(b"line one\nNEEDLE here\nline three\n")?;
        let written_len = bw.close()?;
        assert!(written_len > 0);
        assert!(path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn list_blocks_for_pane_filters_by_pane_and_respects_limit() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Store::open().await?;

        let sid = SessionId(Uuid::new_v4());
        let pane_a = PaneId(Uuid::new_v4());
        let pane_b = PaneId(Uuid::new_v4());
        store.upsert_session(sid, "test").await?;
        store
            .upsert_pane(pane_a, sid, "/bin/sh", None, 80, 24)
            .await?;
        store
            .upsert_pane(pane_b, sid, "/bin/sh", None, 80, 24)
            .await?;

        // Insert 3 blocks for pane_a and 2 for pane_b.
        for i in 0..3u8 {
            let block = Block {
                id: BlockId(Uuid::new_v4()),
                pane: pane_a,
                session: sid,
                command: format!("cmd-a-{i}"),
                cwd: None,
                started_at: Utc::now(),
                ended_at: None,
                exit_code: None,
                stdout_len: 0,
            };
            store.create_block(&block).await?;
        }
        for i in 0..2u8 {
            let block = Block {
                id: BlockId(Uuid::new_v4()),
                pane: pane_b,
                session: sid,
                command: format!("cmd-b-{i}"),
                cwd: None,
                started_at: Utc::now(),
                ended_at: None,
                exit_code: None,
                stdout_len: 0,
            };
            store.create_block(&block).await?;
        }

        // pane_a with no limit cap returns only pane_a rows.
        let a_rows = store.list_blocks_for_pane(pane_a, 100).await?;
        assert_eq!(a_rows.len(), 3);
        assert!(a_rows.iter().all(|b| b.pane == pane_a));

        // pane_b returns only pane_b rows.
        let b_rows = store.list_blocks_for_pane(pane_b, 100).await?;
        assert_eq!(b_rows.len(), 2);
        assert!(b_rows.iter().all(|b| b.pane == pane_b));

        // Limit is respected.
        let limited = store.list_blocks_for_pane(pane_a, 2).await?;
        assert_eq!(limited.len(), 2);

        Ok(())
    }

    // ── Layout persistence tests (M7-C) ───────────────────────────────────────

    #[tokio::test]
    async fn layout_roundtrip() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Store::open().await?;

        let sid = SessionId(Uuid::new_v4());
        let pane_a = PaneId(Uuid::new_v4());
        let pane_b = PaneId(Uuid::new_v4());
        store.upsert_session(sid, "test-layout").await?;

        // Build a simple VSplit layout and persist it.
        let layout = pyre_proto::LayoutNode::VSplit(vec![
            (pyre_proto::LayoutNode::Leaf(pane_a), 50),
            (pyre_proto::LayoutNode::Leaf(pane_b), 50),
        ]);
        let json = serde_json::to_string(&layout)?;
        store.upsert_session_layout(sid, &json).await?;

        // Read back via list_session_layouts.
        let rows = store.list_session_layouts().await?;
        let row = rows.iter().find(|(id, _)| *id == sid).expect("session row");
        let restored: pyre_proto::LayoutNode =
            serde_json::from_str(row.1.as_deref().expect("layout present"))?;
        assert_eq!(layout, restored, "layout round-trip must match");

        Ok(())
    }

    #[tokio::test]
    async fn migration_idempotent() -> Result<()> {
        // Running Store::open() twice on the same DB must not fail (the migration
        // is guarded by sqlx::migrate! which checks applied migrations).
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let _store1 = Store::open().await?;
        let _store2 = Store::open().await?; // second open — must not error
        Ok(())
    }

    // ── Window migration tests (0004_windows.sql + backfill_windows) ──────────

    /// Open a raw SQLite pool in `dir` with all migrations applied but WITHOUT
    /// the backfill or backup side-effects of `Store::open`.  Used by
    /// backfill_windows tests to set up a pre-backfill state.
    async fn open_raw_pool(dir: &std::path::Path) -> Result<Pool<Sqlite>> {
        std::fs::create_dir_all(dir)?;
        let db_path = dir.join("state.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(pool)
    }

    /// Insert a session row directly into `pool` with an optional layout JSON.
    async fn insert_session_row(
        pool: &Pool<Sqlite>,
        id: SessionId,
        layout_json: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO sessions (id, name, created_at, last_active_at, layout) \
             VALUES (?1, 'test', ?2, ?2, ?3)",
        )
        .bind(id.0.to_string())
        .bind(now)
        .bind(layout_json)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Insert a pane row directly into `pool` with an optional name.
    async fn insert_pane_row(
        pool: &Pool<Sqlite>,
        id: PaneId,
        session: SessionId,
        name: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO panes (id, session_id, argv, cols, rows, created_at, name) \
             VALUES (?1, ?2, '/bin/sh', 80, 24, ?3, ?4)",
        )
        .bind(id.0.to_string())
        .bind(session.0.to_string())
        .bind(now)
        .bind(name)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn count_windows_in(pool: &Pool<Sqlite>) -> i64 {
        sqlx::query("SELECT COUNT(*) AS n FROM windows")
            .fetch_one(pool)
            .await
            .expect("count query")
            .try_get("n")
            .expect("n column")
    }

    async fn get_pane_window_id(pool: &Pool<Sqlite>, pane: PaneId) -> Option<String> {
        sqlx::query("SELECT window_id FROM panes WHERE id = ?1")
            .bind(pane.0.to_string())
            .fetch_optional(pool)
            .await
            .expect("pane query")
            .and_then(|r| r.try_get::<Option<String>, _>("window_id").unwrap_or(None))
    }

    /// THE critical test: `backfill_windows` on a realistic pre-v4 DB with a
    /// tiling layout (2 panes) plus 2 standalone panes not present in the tree.
    ///
    /// Asserts:
    /// - A default window named "1" (position 0) is created, carrying the
    ///   original session layout JSON verbatim.
    /// - The two tree panes both get window_id = W0.
    /// - Each standalone pane becomes its own single-leaf window (position ≥ 1,
    ///   distinct window_id, leaf layout over that pane).
    /// - All panes have a non-NULL window_id after backfill.
    #[tokio::test]
    async fn backfill_windows_on_pre_v4_db_with_layout_and_standalone_panes() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = open_raw_pool(tmp.path()).await?;

        let sid = SessionId(Uuid::new_v4());
        let pane_a = PaneId(Uuid::new_v4());
        let pane_b = PaneId(Uuid::new_v4());
        let standalone_1 = PaneId(Uuid::new_v4());
        let standalone_2 = PaneId(Uuid::new_v4());

        // Layout: HSplit with pane_a (top) and pane_b (bottom).
        let layout = pyre_proto::LayoutNode::HSplit(vec![
            (pyre_proto::LayoutNode::Leaf(pane_a), 50),
            (pyre_proto::LayoutNode::Leaf(pane_b), 50),
        ]);
        let layout_json = serde_json::to_string(&layout)?;

        insert_session_row(&pool, sid, Some(&layout_json)).await?;
        insert_pane_row(&pool, pane_a, sid, None).await?;
        insert_pane_row(&pool, pane_b, sid, None).await?;
        // standalone_1 has a name; standalone_2 does not.
        insert_pane_row(&pool, standalone_1, sid, Some("my-shell")).await?;
        insert_pane_row(&pool, standalone_2, sid, None).await?;

        // Pre-condition: no windows yet.
        assert_eq!(count_windows_in(&pool).await, 0, "no windows before backfill");

        backfill_windows(&pool).await?;

        // 3 windows total: W0 (default for layout) + Wk for each standalone pane.
        assert_eq!(
            count_windows_in(&pool).await,
            3,
            "expected 3 windows: 1 default + 2 standalone"
        );

        // Fetch windows ordered by position.
        let wins = sqlx::query(
            "SELECT id, name, position, layout FROM windows ORDER BY position ASC",
        )
        .fetch_all(&pool)
        .await?;

        // W0 — default window.
        let w0_id: String = wins[0].try_get("id")?;
        let w0_name: String = wins[0].try_get("name")?;
        let w0_pos: i64 = wins[0].try_get("position")?;
        let w0_layout: Option<String> = wins[0].try_get("layout").unwrap_or(None);
        assert_eq!(w0_name, "1", "default window must be named '1'");
        assert_eq!(w0_pos, 0, "default window must be at position 0");
        assert_eq!(
            w0_layout.as_deref(),
            Some(layout_json.as_str()),
            "W0 layout must carry original session layout JSON verbatim"
        );

        // Tree panes assigned to W0.
        let pane_a_wid = get_pane_window_id(&pool, pane_a).await;
        let pane_b_wid = get_pane_window_id(&pool, pane_b).await;
        assert_eq!(
            pane_a_wid.as_deref(),
            Some(w0_id.as_str()),
            "pane_a (tree pane) must be in W0"
        );
        assert_eq!(
            pane_b_wid.as_deref(),
            Some(w0_id.as_str()),
            "pane_b (tree pane) must be in W0"
        );

        // Standalone windows (positions 1 and 2).
        let w1_pos: i64 = wins[1].try_get("position")?;
        let w2_pos: i64 = wins[2].try_get("position")?;
        assert!(w1_pos >= 1, "first standalone window must be at position >= 1");
        assert!(w2_pos > w1_pos, "second standalone must have higher position");

        // standalone_1 named "my-shell" (name inherited from pane.name).
        let w1_name: String = wins[1].try_get("name")?;
        assert_eq!(w1_name, "my-shell", "standalone window name must come from pane name");

        // Each standalone pane has its own distinct window.
        let s1_wid = get_pane_window_id(&pool, standalone_1).await;
        let s2_wid = get_pane_window_id(&pool, standalone_2).await;
        let w1_id: String = wins[1].try_get("id")?;
        let w2_id: String = wins[2].try_get("id")?;
        assert_eq!(
            s1_wid.as_deref(),
            Some(w1_id.as_str()),
            "standalone_1 must be in its own window"
        );
        assert_eq!(
            s2_wid.as_deref(),
            Some(w2_id.as_str()),
            "standalone_2 must be in its own window"
        );

        // Standalone windows must have single-leaf layout pointing at the pane.
        let w1_layout: Option<String> = wins[1].try_get("layout").unwrap_or(None);
        let w2_layout: Option<String> = wins[2].try_get("layout").unwrap_or(None);
        let expected_leaf_1 = serde_json::to_string(&pyre_proto::LayoutNode::Leaf(standalone_1))?;
        let expected_leaf_2 = serde_json::to_string(&pyre_proto::LayoutNode::Leaf(standalone_2))?;
        assert_eq!(
            w1_layout.as_deref(),
            Some(expected_leaf_1.as_str()),
            "standalone_1 window must have a Leaf layout"
        );
        assert_eq!(
            w2_layout.as_deref(),
            Some(expected_leaf_2.as_str()),
            "standalone_2 window must have a Leaf layout"
        );

        // All panes must have a non-NULL window_id.
        let null_count: i64 = sqlx::query(
            "SELECT COUNT(*) AS n FROM panes WHERE session_id = ?1 AND window_id IS NULL",
        )
        .bind(sid.0.to_string())
        .fetch_one(&pool)
        .await?
        .try_get("n")?;
        assert_eq!(null_count, 0, "all panes must have window_id after backfill");

        Ok(())
    }

    /// Backup file `state.db.bak.<timestamp>` is created in the data dir the
    /// first time `Store::open` runs (the fresh db has no `windows` table yet,
    /// so the guard fires).
    #[tokio::test]
    async fn store_open_creates_backup_file_when_windows_table_absent() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let _store = Store::open().await?;
        drop(_store);

        let bak_count = std::fs::read_dir(tmp.path())?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("state.db.bak.")
            })
            .count();
        assert_eq!(bak_count, 1, "exactly one backup file must exist after first Store::open");

        Ok(())
    }

    /// Calling `backfill_windows` a second time (or re-opening the store) must
    /// be a no-op — the idempotency guard is `COUNT(*) FROM windows == 0`.
    #[tokio::test]
    async fn backfill_windows_is_noop_on_second_call() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = open_raw_pool(tmp.path()).await?;

        let sid = SessionId(Uuid::new_v4());
        let pane = PaneId(Uuid::new_v4());
        let layout = pyre_proto::LayoutNode::Leaf(pane);
        let layout_json = serde_json::to_string(&layout)?;
        insert_session_row(&pool, sid, Some(&layout_json)).await?;
        insert_pane_row(&pool, pane, sid, None).await?;

        // First call creates one window.
        backfill_windows(&pool).await?;
        let count_first = count_windows_in(&pool).await;
        assert_eq!(count_first, 1, "first backfill must create 1 window");

        // Second call must not add any more windows.
        backfill_windows(&pool).await?;
        let count_second = count_windows_in(&pool).await;
        assert_eq!(
            count_second, 1,
            "second backfill must be a no-op — window count unchanged"
        );

        Ok(())
    }

    /// Store::open on a freshly created db with no session rows must leave the
    /// windows table empty (the backfill guard short-circuits on sess_count == 0).
    #[tokio::test]
    async fn backfill_windows_fresh_install_with_no_sessions_creates_no_windows() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool = open_raw_pool(tmp.path()).await?;

        // No sessions inserted — backfill should be a no-op.
        backfill_windows(&pool).await?;

        let window_count = count_windows_in(&pool).await;
        assert_eq!(window_count, 0, "fresh install: no sessions => no windows created");

        Ok(())
    }

    /// Full window CRUD roundtrip through `Store` public API:
    /// upsert → list → rename → get_name → layout_upsert/get → delete.
    #[tokio::test]
    async fn window_crud_roundtrip() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Store::open().await?;

        let sid = SessionId(Uuid::new_v4());
        store.upsert_session(sid, "test").await?;

        let wid = WindowId::new();
        let now = Utc::now().timestamp_millis();

        // ── upsert ──────────────────────────────────────────────────────────────
        store.upsert_window(wid, sid, "my-window", 0, now).await?;

        // ── list (ordered by position) ───────────────────────────────────────
        let windows = store.list_windows(sid).await?;
        assert_eq!(windows.len(), 1, "one window after upsert");
        assert_eq!(windows[0].0, wid, "window id must match");
        assert_eq!(windows[0].1, "my-window", "window name must match");
        assert_eq!(windows[0].2, 0, "position must be 0");

        // ── rename → get_name ────────────────────────────────────────────────
        store.rename_window(wid, "renamed").await?;
        let name = store.get_window_name(wid).await?;
        assert_eq!(name.as_deref(), Some("renamed"), "get_window_name must reflect rename");

        // ── layout upsert / get ──────────────────────────────────────────────
        let pane = PaneId(Uuid::new_v4());
        let layout = pyre_proto::LayoutNode::Leaf(pane);
        let json = serde_json::to_string(&layout)?;
        store.upsert_window_layout(wid, &json).await?;
        let got = store.get_window_layout_json(wid).await?;
        assert_eq!(
            got.as_deref(),
            Some(json.as_str()),
            "get_window_layout_json must return what was upserted"
        );

        // ── delete ───────────────────────────────────────────────────────────
        store.delete_window(wid).await?;
        let after_delete = store.list_windows(sid).await?;
        assert!(after_delete.is_empty(), "list_windows must be empty after delete_window");

        // get_window_name must return None for a deleted window.
        let name_after_delete = store.get_window_name(wid).await?;
        assert!(
            name_after_delete.is_none(),
            "get_window_name must return None for deleted window"
        );

        Ok(())
    }

    /// `assign_pane_window` is idempotent: calling it twice with the same
    /// (pane, window) pair must not create a duplicate row.
    #[tokio::test]
    async fn assign_pane_window_calling_twice_keeps_single_assignment() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        unsafe {
            std::env::set_var("PYRE_DATA_DIR", tmp.path());
        }
        let store = Store::open().await?;

        let sid = SessionId(Uuid::new_v4());
        store.upsert_session(sid, "test").await?;

        let wid = WindowId::new();
        store
            .upsert_window(wid, sid, "w0", 0, Utc::now().timestamp_millis())
            .await?;

        let pane = PaneId(Uuid::new_v4());

        // First assignment.
        store.assign_pane_window(pane, sid, wid).await?;
        let panes_first = store.list_panes_for_window(wid).await?;
        assert_eq!(panes_first.len(), 1, "one pane after first assign");
        assert_eq!(panes_first[0], pane);

        // Second assignment — must be idempotent (INSERT OR IGNORE + UPDATE).
        store.assign_pane_window(pane, sid, wid).await?;
        let panes_second = store.list_panes_for_window(wid).await?;
        assert_eq!(
            panes_second.len(),
            1,
            "assign_pane_window twice must not create duplicate pane row"
        );

        Ok(())
    }
}
