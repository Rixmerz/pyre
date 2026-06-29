//! Production smoke test: full session lifecycle driven against a live pyred.
//!
//! Steps:
//!   1. Spawn pyred in a temp dir.
//!   2. Call spawn() → get session + pane.
//!   3. Open a second pane via open_pane().
//!   4. Send `echo hello` via stream connection into pane 1.
//!   5. capture_pane(pane1, 10) → assert output contains "hello".
//!   6. list_all_panes() → assert 2 entries with valid states.
//!   7. close_session(session_id) → assert Ok.
//!   8. list_sessions() → assert empty.
//!
//! Marked #[ignore] because it requires the pyred binary to be built and
//! a writable XDG_RUNTIME_DIR-like temp directory.  Run with:
//!   cargo test --test prod_smoke -- --ignored --nocapture

#[allow(dead_code)]
#[path = "common.rs"]
mod common;

use std::time::Duration;

use bytes::Bytes;
use futures::SinkExt;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{
    write_control_client, InputFrame, OpenPaneReq, OutputFrame, PyreDaemonClient, SpawnReq,
    SpawnResp, MODE_STREAM,
};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

async fn spawn_daemon(
    tmpdir: &tempfile::TempDir,
) -> anyhow::Result<(common::ChildGuard, std::path::PathBuf, PyreDaemonClient)> {
    let sock_path = tmpdir.path().join("pyre.sock");
    let child = common::ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
            .env("XDG_RUNTIME_DIR", tmpdir.path())
            .env("PYRE_DATA_DIR", tmpdir.path())
            .stderr(std::process::Stdio::piped())
            .spawn()?,
    );
    wait_for_socket(&sock_path, Duration::from_secs(5)).await?;
    let rpc = connect_control(&sock_path).await?;
    Ok((child, sock_path, rpc))
}

fn shutdown_daemon(mut child: common::ChildGuard) {
    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGTERM).ok();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // child guard drops here; already reaped above.
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(sock_path).await?;
    write_control_client(&mut sock).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

async fn wait_for_socket(path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("socket {} never appeared", path.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ── test ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires built pyred binary; run with --ignored"]
async fn prod_smoke_full_lifecycle() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_prod_smoke()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("prod_smoke failed: {e:#}"),
        Err(_) => panic!("prod_smoke timed out after 30s"),
    }
}

async fn run_prod_smoke() -> anyhow::Result<()> {
    // ── 1. Spawn pyred ────────────────────────────────────────────────────────
    let tmpdir = tempfile::TempDir::new()?;
    let (child, sock_path, rpc) = spawn_daemon(&tmpdir).await?;

    // ── 2. spawn() → session + pane1 ─────────────────────────────────────────
    let SpawnResp {
        session,
        pane: pane1,
        window,
    } = rpc
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "$ ".into())],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e:?}"))?;

    // ── 3. open_pane() → pane2 ───────────────────────────────────────────────
    let pane2 = rpc
        .open_pane(
            tarpc::context::current(),
            OpenPaneReq {
                session,
                window,
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "$ ".into())],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("open_pane: {e:?}"))?;

    assert_ne!(pane1, pane2, "pane1 and pane2 should be distinct");

    // ── 4. send-keys via stream connection into pane1 ─────────────────────────
    {
        let mut stream_sock = UnixStream::connect(&sock_path).await?;
        stream_sock.write_all(&[MODE_STREAM]).await?;
        stream_sock.write_all(session.0.as_bytes()).await?;
        stream_sock.write_all(pane1.0.as_bytes()).await?;

        let (rd, wr) = stream_sock.into_split();
        let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
        let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

        let mut _output: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
            tokio_serde::SymmetricallyFramed::new(
                frame_read,
                SymmetricalBincode::<OutputFrame>::default(),
            );
        let mut input: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
            tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

        input
            .send(InputFrame {
                session,
                data: Bytes::from_static(b"echo hello\n"),
            })
            .await
            .map_err(|e| anyhow::anyhow!("send InputFrame: {e}"))?;

        // Give the shell time to process the command before we capture.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // ── 5. capture_pane(pane1, 10) → assert contains "hello" ─────────────────
    let captured = rpc
        .capture_pane(tarpc::context::current(), pane1, 10)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("capture_pane: {e:?}"))?;

    let output_str = String::from_utf8_lossy(&captured);
    assert!(
        output_str.contains("hello"),
        "expected 'hello' in capture_pane output, got: {output_str:?}"
    );

    // ── 6. list_all_panes() → assert 2 entries ────────────────────────────────
    let all_panes = rpc
        .list_all_panes(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_all_panes: {e:?}"))?;

    assert_eq!(
        all_panes.len(),
        2,
        "expected 2 panes, got {}: {all_panes:?}",
        all_panes.len()
    );

    let pane_ids: Vec<_> = all_panes.iter().map(|p| p.id).collect();
    assert!(
        pane_ids.contains(&pane1),
        "pane1 missing from list_all_panes"
    );
    assert!(
        pane_ids.contains(&pane2),
        "pane2 missing from list_all_panes"
    );

    // ── 7. close_session() → assert Ok ───────────────────────────────────────
    rpc.close_session(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_session: {e:?}"))?;

    // Brief wait for the daemon to remove internal state.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── 8. list_sessions() → assert empty ────────────────────────────────────
    let sessions = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e:?}"))?;

    assert!(
        sessions.is_empty(),
        "expected empty session list after close_session, got: {sessions:?}"
    );

    // ── 9. Shut down pyred ───────────────────────────────────────────────────
    drop(rpc);
    shutdown_daemon(child);

    Ok(())
}

// ── close_pane eviction test ──────────────────────────────────────────────────
//
// Regression for: closing the last pane of a session via close_pane() RPC
// (i.e. the TUI's Ctrl-B x path) should evict the session from the registry.
// If it does not, session_list() returns a stale entry after the pane is gone.
//
// Run with: cargo test --test prod_smoke -- --ignored --nocapture

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires built pyred binary; run with --ignored"]
async fn close_pane_evicts_empty_session() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_close_pane_eviction()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("close_pane_evicts_empty_session failed: {e:#}"),
        Err(_) => panic!("close_pane_evicts_empty_session timed out after 30s"),
    }
}

async fn run_close_pane_eviction() -> anyhow::Result<()> {
    // ── 1. Spawn daemon ───────────────────────────────────────────────────────
    let tmpdir = tempfile::TempDir::new()?;
    let (child, _sock, rpc) = spawn_daemon(&tmpdir).await?;

    // ── 2. Spawn session with one pane ────────────────────────────────────────
    let SpawnResp { session, pane, .. } = rpc
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e:?}"))?;

    // ── 3. Confirm session exists ──────────────────────────────────────────────
    let sessions = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e:?}"))?;
    assert_eq!(
        sessions.len(),
        1,
        "expected 1 session before close, got {sessions:?}"
    );

    // ── 4. Close the only pane via close_pane() RPC (TUI path) ────────────────
    rpc.close_pane(tarpc::context::current(), pane)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_pane: {e:?}"))?;

    // Brief wait for daemon to process eviction.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── 5. Session must be gone ────────────────────────────────────────────────
    let sessions_after = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions after close_pane: {e:?}"))?;

    assert!(
        sessions_after.is_empty(),
        "BUG: session {session} still present after closing its last pane via close_pane RPC; \
         got: {sessions_after:?}"
    );

    // ── 6. Shut down ──────────────────────────────────────────────────────────
    drop(rpc);
    shutdown_daemon(child);

    Ok(())
}

// ── close_session regression: windows cleared + idempotent on evicted session ─

/// After `close_session`, `list_sessions` AND `list_windows` must both be empty.
/// Regression for the window-model migration where close_session did not remove
/// window rows from SQLite or from the in-memory window_store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires built pyred binary; run with --ignored"]
async fn close_session_clears_windows() {
    let result =
        tokio::time::timeout(Duration::from_secs(30), run_close_session_clears_windows()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("close_session_clears_windows failed: {e:#}"),
        Err(_) => panic!("close_session_clears_windows timed out after 30s"),
    }
}

async fn run_close_session_clears_windows() -> anyhow::Result<()> {
    let tmpdir = tempfile::TempDir::new()?;
    let (child, _sock, rpc) = spawn_daemon(&tmpdir).await?;

    let SpawnResp { session, .. } = rpc
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e:?}"))?;

    // Confirm 1 window exists before close.
    let windows_before = rpc
        .list_windows(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_windows before: {e:?}"))?;
    assert_eq!(
        windows_before.len(),
        1,
        "expected 1 window before close_session, got: {windows_before:?}"
    );

    // Close the session.
    rpc.close_session(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_session: {e:?}"))?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Session must be gone.
    let sessions = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e:?}"))?;
    assert!(
        sessions.is_empty(),
        "expected empty session list after close_session, got: {sessions:?}"
    );

    // Windows must be gone too.
    let windows_after = rpc
        .list_windows(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_windows after: {e:?}"))?;
    assert!(
        windows_after.is_empty(),
        "BUG: windows still present after close_session; got: {windows_after:?}"
    );

    drop(rpc);
    shutdown_daemon(child);
    Ok(())
}

/// `close_session` on a session whose pane already exited (triggering
/// `evict_session_if_empty`) must return `Ok(())`, not `NoSuchSession`.
/// Regression for the single-mode path where kill_session failed with an error
/// when the session was already gone, causing the GUI to show a toast and
/// skip the UI reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires built pyred binary; run with --ignored"]
async fn close_session_idempotent_after_pane_exit() {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run_close_session_idempotent(),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("close_session_idempotent_after_pane_exit failed: {e:#}"),
        Err(_) => panic!("close_session_idempotent_after_pane_exit timed out after 30s"),
    }
}

async fn run_close_session_idempotent() -> anyhow::Result<()> {
    let tmpdir = tempfile::TempDir::new()?;
    let (child, _sock, rpc) = spawn_daemon(&tmpdir).await?;

    let SpawnResp { session, pane, .. } = rpc
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e:?}"))?;

    // Close the pane first — triggers evict_session_if_empty in single mode,
    // removing the session from the in-memory registry before close_session arrives.
    rpc.close_pane(tarpc::context::current(), pane)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_pane: {e:?}"))?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Session already evicted — close_session must return Ok (idempotent), not error.
    rpc.close_session(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_session on evicted session returned error: {e:?}"))?;

    let sessions = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e:?}"))?;
    assert!(
        sessions.is_empty(),
        "expected empty session list; got: {sessions:?}"
    );

    drop(rpc);
    shutdown_daemon(child);
    Ok(())
}

// ── M7 regression: close_pane with layout persistence doesn't deadlock ────────
//
// Regression for: 7a8e7b8 extended close_pane with layout persistence (Store
// arg) but the async SQLite write (upsert_session_layout) was awaited while
// holding session.layout tokio::Mutex, stalling any concurrent caller that
// also needed the lock (get_layout, open_pane_split, set_pane_weight).
//
// This test: spawn → split (so a LayoutNode exists in SQLite) → fire a
// concurrent get_session_layout RPC alongside close_pane → assert neither
// times out and the session evicts cleanly.
//
// Run with: cargo test --test prod_smoke -- --ignored --nocapture

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires built pyred binary; run with --ignored"]
async fn close_pane_with_layout_no_deadlock() {
    let result =
        tokio::time::timeout(Duration::from_secs(30), run_close_pane_layout_regression()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("close_pane_with_layout_no_deadlock failed: {e:#}"),
        Err(_) => {
            panic!("close_pane_with_layout_no_deadlock timed out — likely deadlock in layout mutex")
        }
    }
}

async fn run_close_pane_layout_regression() -> anyhow::Result<()> {
    // ── 1. Spawn daemon ───────────────────────────────────────────────────────
    let tmpdir = tempfile::TempDir::new()?;
    let (child, _sock, rpc) = spawn_daemon(&tmpdir).await?;

    // ── 2. Spawn session → pane1 ──────────────────────────────────────────────
    let SpawnResp {
        session,
        pane: pane1,
        window,
    } = rpc
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e:?}"))?;

    // ── 3. Open a second pane so LayoutNode is a Split (not just Leaf) ────────
    // This ensures a non-trivial tree exists in both memory and SQLite,
    // exercising the close path that must collapse a Split node.
    let pane2 = rpc
        .open_pane(
            tarpc::context::current(),
            OpenPaneReq {
                session,
                window,
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("open_pane: {e:?}"))?;

    assert_ne!(pane1, pane2, "panes must be distinct");

    // ── 4. Fire close_pane(pane2) and a concurrent get_session_layout ─────────
    // Before the fix, close_pane held session.layout mutex across the SQLite
    // await, so the concurrent get_session_layout would block until the write
    // finished. Under test-timeout pressure this manifests as a hang.
    let rpc2 = rpc.clone();
    let layout_task = tokio::spawn(async move {
        rpc2.get_session_layout(tarpc::context::current(), session)
            .await
    });

    rpc.close_pane(tarpc::context::current(), pane2)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_pane(pane2): {e:?}"))?;

    // Both must complete within the 5 s sub-deadline.
    // get_session_layout returns Ok(LayoutNode) on success or a PyreError if
    // the session was already evicted (race is acceptable — what matters is
    // that it does NOT hang).
    let layout_result = tokio::time::timeout(Duration::from_secs(5), layout_task)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "get_session_layout timed out — layout mutex likely held across close_pane await"
            )
        })?
        .map_err(|e| anyhow::anyhow!("layout_task join: {e}"))?;

    // Either a valid layout or a NoSuchSession error are both acceptable;
    // a timeout (caught above) is the failure mode we're guarding against.
    eprintln!("get_session_layout result: {layout_result:?}");

    // ── 5. Close the remaining pane — session must evict ─────────────────────
    rpc.close_pane(tarpc::context::current(), pane1)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("close_pane(pane1): {e:?}"))?;

    tokio::time::sleep(Duration::from_millis(200)).await;

    let sessions_after = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e:?}"))?;

    assert!(
        sessions_after.is_empty(),
        "BUG: session {session} still present after closing all panes; got: {sessions_after:?}"
    );

    // ── 6. Shut down ──────────────────────────────────────────────────────────
    drop(rpc);
    shutdown_daemon(child);

    Ok(())
}
