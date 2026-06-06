//! Per-session sqlite shard paths and read-only pane introspection.
//!
//! In hybrid mode each session's live PTYs are persisted in a dedicated
//! sqlite "shard" under `$XDG_STATE_HOME/pyre/sessions/<session_id>/state.db`
//! (see [`crate::worker::WorkerShard`]). The shard's `panes` table is the
//! source of truth for whether a session has any PTY to restore on reattach —
//! the supervisor's main `state.db` `panes` table is NOT written in hybrid
//! mode, so a JOIN there would always report zero.
//!
//! These helpers let the supervisor decide, *without* spawning a worker,
//! whether a persisted session is live (≥1 pane) or stale (0 panes). A stale
//! shard must be skipped on reattach (invariants I-4 / I-5) and is a candidate
//! for startup GC.
//!
//! The path derivation mirrors `WorkerShard::open` and the `migration` module
//! so all three agree on shard locations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::Row;

/// `$XDG_STATE_HOME` (or `~/.local/state` fallback).
fn state_home() -> PathBuf {
    if let Ok(p) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("state")
}

/// Root directory holding every per-session shard dir:
/// `$XDG_STATE_HOME/pyre/sessions`.
pub fn sessions_dir() -> PathBuf {
    state_home().join("pyre").join("sessions")
}

/// Directory for a single session's shard: `…/pyre/sessions/<session_id>`.
pub fn shard_dir(session_id: &str) -> PathBuf {
    sessions_dir().join(session_id)
}

/// Path to a single session's shard database file.
pub fn shard_db_path(session_id: &str) -> PathBuf {
    shard_dir(session_id).join("state.db")
}

/// Count the persisted panes in a session's shard **without creating it**.
///
/// Returns `Ok(0)` (not an error) when the shard directory, the `state.db`
/// file, or the `panes` table is absent — a missing shard has, definitionally,
/// zero panes. This makes the function safe to call on arbitrary session ids
/// during reattach and GC, and guarantees it never resurrects a shard that a
/// prior clean exit deleted.
///
/// # Errors
/// Propagates only genuine sqlite errors on an existing, openable database
/// (e.g. corruption) — never "file not found".
pub async fn shard_pane_count(session_id: &str) -> Result<u64> {
    let db_path = shard_db_path(session_id);
    // Missing file ⇒ no panes. Do not open with create_if_missing here.
    if !db_path.exists() {
        return Ok(0);
    }

    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(false)
        .read_only(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);

    // A shard whose file vanished between the exists() check and connect, or
    // whose `panes` table was never created, is treated as 0 panes rather than
    // a hard error — the only thing the caller needs to know is "is it live?".
    let pool = match SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(
                session_id,
                "shard_pane_count: open failed, treating as 0 panes: {e:#}"
            );
            return Ok(0);
        }
    };

    let count = match sqlx::query("SELECT COUNT(*) AS n FROM panes")
        .fetch_one(&pool)
        .await
    {
        Ok(row) => row.try_get::<i64, _>("n").unwrap_or(0).max(0) as u64,
        Err(e) => {
            // Missing table ⇒ 0 panes. Surface anything else as debug + 0 so a
            // single weird shard can never wedge startup.
            tracing::debug!(
                session_id,
                "shard_pane_count: query failed, treating as 0 panes: {e:#}"
            );
            0
        }
    };
    pool.close().await;
    Ok(count)
}

/// Remove a session's shard directory (used by startup GC for stale shards).
///
/// Idempotent: a missing directory is a no-op success.
///
/// # Errors
/// Propagates filesystem errors other than "not found".
pub fn remove_shard_dir(session_id: &str) -> Result<()> {
    let dir = shard_dir(session_id);
    remove_dir_if_exists(&dir)
}

fn remove_dir_if_exists(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("rm -rf {}", dir.display())),
    }
}

/// Process-wide lock serializing every lib-test that mutates the global
/// `XDG_STATE_HOME` env var (shard tests, migration tests). All such tests
/// share THIS one lock so they never race across modules — two independent
/// per-module locks would not serialize against each other.
///
/// An async-aware `tokio::sync::Mutex` so the guard is safe to hold across the
/// `.await`s inside the tests (I-8 / clippy::await_holding_lock).
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    use crate::shard::ENV_TEST_LOCK as ENV_LOCK;

    /// Create a shard for `session_id` with `n` panes (n == 0 leaves the
    /// `panes` table empty). Mirrors `WorkerShard::open`'s schema.
    async fn make_shard(session_id: &str, n: u32) -> Result<()> {
        let dir = shard_dir(session_id);
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("state.db");
        let opts = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS panes (
                slot_idx INTEGER PRIMARY KEY,
                shell    TEXT NOT NULL,
                cwd      TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        for i in 0..n {
            sqlx::query("INSERT INTO panes (slot_idx, shell, cwd) VALUES (?1, ?2, ?3)")
                .bind(i as i64)
                .bind("/bin/sh")
                .bind("/tmp")
                .execute(&pool)
                .await?;
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn pane_count_zero_for_absent_shard() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation, serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        // Never created — must be 0, and must NOT create the file.
        let sid = uuid::Uuid::new_v4().to_string();
        assert_eq!(shard_pane_count(&sid).await?, 0);
        assert!(
            !shard_db_path(&sid).exists(),
            "shard_pane_count must not resurrect a missing shard db"
        );
        Ok(())
    }

    #[tokio::test]
    async fn pane_count_zero_for_empty_shard_and_nonzero_for_live() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation, serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let stale = uuid::Uuid::new_v4().to_string();
        let live = uuid::Uuid::new_v4().to_string();
        make_shard(&stale, 0).await?;
        make_shard(&live, 2).await?;

        assert_eq!(
            shard_pane_count(&stale).await?,
            0,
            "an existing shard with an empty panes table must report 0"
        );
        assert_eq!(
            shard_pane_count(&live).await?,
            2,
            "a shard with 2 persisted panes must report 2"
        );
        Ok(())
    }

    #[tokio::test]
    async fn remove_shard_dir_is_idempotent_and_real() -> Result<()> {
        let _g = ENV_LOCK.lock().await;
        let tmp = TempDir::new()?;
        // SAFETY: test-only env mutation, serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let sid = uuid::Uuid::new_v4().to_string();
        make_shard(&sid, 0).await?;
        assert!(shard_dir(&sid).exists());
        remove_shard_dir(&sid)?;
        assert!(!shard_dir(&sid).exists(), "shard dir must be gone after GC");
        // Second removal is a no-op success.
        remove_shard_dir(&sid)?;
        Ok(())
    }
}
