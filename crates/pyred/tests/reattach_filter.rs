//! Regression test for the hybrid-mode reattach stale-session filter.
//!
//! Bug: on startup the supervisor reattached EVERY persisted session id from
//! its `state.db`, including sessions whose per-session shard had 0 panes
//! (accumulated stale shards from prior clean exits). Each stale reattach
//! produced a 0-PTY ghost worker; the TUI then attached, got an immediate EOF,
//! and flipped into the "session lost / exit only" state. With ~874 stale
//! shards this also delayed the public socket bind for minutes.
//!
//! Fix: the reattach path partitions persisted sessions by *shard* pane count
//! (`pyred::shard::shard_pane_count`) — the shard `panes` table is the source
//! of truth in hybrid mode, since the supervisor `state.db` `panes` table is
//! never written there. Only sessions with ≥1 pane are reattached; 0-pane
//! shards are skipped (invariants I-4 / I-5) and GC'd.
//!
//! This test reconstructs the exact decision the reattach loop makes against
//! real on-disk shards built the same way `WorkerShard` builds them, and
//! asserts the partition + GC behaviour — no daemon spawn, no tautology.

use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use tokio::sync::Mutex;

// `XDG_STATE_HOME` is process-global; serialize the two tests in this file so
// they never race on it. An async-aware `tokio::sync::Mutex` is used so the
// guard may be safely held across the `.await`s inside each test (I-8 /
// clippy::await_holding_lock).
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Build a shard at `$XDG_STATE_HOME/pyre/sessions/<id>/state.db` with `n`
/// persisted panes, mirroring `WorkerShard::open`'s schema exactly.
async fn make_shard(session_id: &str, n: u32) -> Result<()> {
    let dir = pyred::shard::shard_dir(session_id);
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
            .bind("/bin/bash")
            .bind("/home/user")
            .execute(&pool)
            .await?;
    }
    pool.close().await;
    Ok(())
}

/// The pure decision the supervisor reattach loop makes: partition persisted
/// session ids into (live, stale) by shard pane count.
async fn partition_reattach(persisted: &[String]) -> (Vec<String>, Vec<String>) {
    let mut live = Vec::new();
    let mut stale = Vec::new();
    for sid in persisted {
        match pyred::shard::shard_pane_count(sid).await {
            Ok(0) => stale.push(sid.clone()),
            Ok(_) => live.push(sid.clone()),
            // Mirror the supervisor: an un-introspectable shard is left alone
            // (neither reattached nor GC'd).
            Err(_) => {}
        }
    }
    (live, stale)
}

#[tokio::test]
async fn stale_zero_pane_session_is_not_reattached() -> Result<()> {
    let _g = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    // SAFETY: test-only env mutation, guarded by ENV_LOCK for the file's tests.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", tmp.path());
    }

    // Two live sessions (1 and 3 panes) and three stale (0-pane) shards — the
    // accumulated-empty-dir scenario, just smaller.
    let live_a = uuid::Uuid::new_v4().to_string();
    let live_b = uuid::Uuid::new_v4().to_string();
    let stale: Vec<String> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();

    make_shard(&live_a, 1).await?;
    make_shard(&live_b, 3).await?;
    for s in &stale {
        make_shard(s, 0).await?;
    }

    // Persisted order interleaves live and stale, as the supervisor store would
    // return them (ORDER BY created_at).
    let persisted: Vec<String> = vec![
        stale[0].clone(),
        live_a.clone(),
        stale[1].clone(),
        live_b.clone(),
        stale[2].clone(),
    ];

    let (live, would_gc) = partition_reattach(&persisted).await;

    // Only the two live sessions are reattached.
    assert_eq!(live.len(), 2, "exactly the 2 live sessions reattach");
    assert!(live.contains(&live_a) && live.contains(&live_b));
    for s in &stale {
        assert!(
            !live.contains(s),
            "stale 0-pane session {s} must NOT be reattached (I-4/I-5)"
        );
    }

    // All three stale shards are GC candidates; neither live shard is.
    assert_eq!(would_gc.len(), 3, "all 3 stale shards are GC candidates");
    assert!(!would_gc.contains(&live_a) && !would_gc.contains(&live_b));

    // Perform the GC exactly as the supervisor does and assert only the stale
    // dirs vanish — never a live one.
    for s in &would_gc {
        pyred::shard::remove_shard_dir(s)?;
    }
    for s in &stale {
        assert!(
            !pyred::shard::shard_dir(s).exists(),
            "stale shard dir {s} must be pruned"
        );
    }
    assert!(
        pyred::shard::shard_dir(&live_a).exists(),
        "live shard must survive GC"
    );
    assert!(
        pyred::shard::shard_dir(&live_b).exists(),
        "live shard must survive GC"
    );

    Ok(())
}

#[tokio::test]
async fn absent_shard_counts_as_zero_without_creating_it() -> Result<()> {
    let _g = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir()?;
    // SAFETY: test-only env mutation, guarded by ENV_LOCK for the file's tests.
    unsafe {
        std::env::set_var("XDG_STATE_HOME", tmp.path());
    }

    // A session id the store knows about but whose shard dir was already
    // removed: must classify stale and must not resurrect the db file.
    let ghost = uuid::Uuid::new_v4().to_string();
    let (live, stale) = partition_reattach(std::slice::from_ref(&ghost)).await;
    assert!(live.is_empty(), "a missing shard is never reattached");
    assert_eq!(stale, vec![ghost.clone()]);
    let db: PathBuf = pyred::shard::shard_db_path(&ghost);
    assert!(
        !db.exists(),
        "introspecting a missing shard must not create it"
    );
    Ok(())
}
