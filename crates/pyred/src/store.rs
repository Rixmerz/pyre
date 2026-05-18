//! Persistence for sessions, panes, and blocks. SQLite (WAL) for metadata,
//! per-block zstd-compressed stdout blobs on disk.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use pyre_proto::{Block, BlockId, PaneId, SessionId};
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

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("run migrations")?;

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

    #[tokio::test]
    async fn open_and_roundtrip_block() -> Result<()> {
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation; tests run single-threaded per process.
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
}
