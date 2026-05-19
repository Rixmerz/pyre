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

use std::time::Duration;

use bytes::Bytes;
use futures::SinkExt;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{
    InputFrame, OpenPaneReq, OutputFrame, PyreDaemonClient, SpawnReq, SpawnResp, MODE_CONTROL,
    MODE_STREAM,
};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

// ── helpers ──────────────────────────────────────────────────────────────────

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(sock_path).await?;
    sock.write_all(&[MODE_CONTROL]).await?;
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
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", tmpdir.path())
        .env("PYRE_DATA_DIR", tmpdir.path())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    wait_for_socket(&sock_path, Duration::from_secs(5)).await?;

    let rpc = connect_control(&sock_path).await?;

    // ── 2. spawn() → session + pane1 ─────────────────────────────────────────
    let SpawnResp {
        session,
        pane: pane1,
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
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "$ ".into())],
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

    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGTERM).ok();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            child.kill().ok();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

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
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", tmpdir.path())
        .env("PYRE_DATA_DIR", tmpdir.path())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    wait_for_socket(&sock_path, Duration::from_secs(5)).await?;
    let rpc = connect_control(&sock_path).await?;

    // ── 2. Spawn session with one pane ────────────────────────────────────────
    let SpawnResp { session, pane } = rpc
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
    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGTERM).ok();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            child.kill().ok();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(())
}
