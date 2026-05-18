//! Integration smoke test: capture_pane RPC returns expected text.
//!
//! Spawns pyred, runs a command in a PTY session via the stream connection,
//! then calls `capture_pane` over the control connection and asserts the
//! expected marker text appears in the returned ring-buffer snapshot.

use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{
    InputFrame, OutputFrame, PyreDaemonClient, SpawnReq, SpawnResp, MODE_CONTROL, MODE_STREAM,
};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_pane_returns_marker() {
    let result = tokio::time::timeout(Duration::from_secs(20), run_capture()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("capture test failed: {e:#}"),
        Err(_) => panic!("capture test timed out after 20s"),
    }
}

async fn run_capture() -> anyhow::Result<()> {
    // --- 1. Temporary dir + spawn pyred ---
    let tmpdir = tempfile::TempDir::new()?;
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", tmpdir.path())
        .env("PYRE_DATA_DIR", tmpdir.path())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // --- 2. Wait for socket to appear and accept connections ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let client = loop {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "pyred socket never became connectable at {}",
                sock_path.display()
            );
        }
        if sock_path.exists() {
            if let Ok(c) = connect_control(&sock_path).await {
                break c;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let SpawnResp { session, pane } = client
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "".into())],
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("daemon spawn: {e}"))?;

    // --- 4. Stream connection ---
    let mut stream_sock = UnixStream::connect(&sock_path).await?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // --- 5. Drain the initial snapshot frame ---
    let _ = tokio::time::timeout(Duration::from_secs(2), output_frames.next()).await;

    // --- 6. Send marker command ---
    const MARKER: &str = "pyre-capture-marker-42";
    input_frames
        .send(InputFrame {
            session,
            data: Bytes::from(format!("echo {MARKER}\n")),
        })
        .await
        .map_err(|e| anyhow::anyhow!("send InputFrame: {e}"))?;

    // --- 7. Wait for output to settle ---
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(input_frames);

    // --- 8. capture_pane via second control connection ---
    let client2 = connect_control(&sock_path).await?;
    let capture_bytes = client2
        .capture_pane(tarpc::context::current(), pane, 100)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("daemon capture_pane: {e}"))?;

    let capture_text = String::from_utf8_lossy(&capture_bytes);
    assert!(
        capture_text.contains(MARKER),
        "expected '{MARKER}' in capture_pane output, got: {capture_text:?}"
    );

    // --- 9. Cleanup ---
    drop(client);
    drop(client2);

    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGTERM).ok();
    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }
        if tokio::time::Instant::now() >= wait_deadline {
            child.kill().ok();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(())
}

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(sock_path).await?;
    sock.write_all(&[MODE_CONTROL]).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}
