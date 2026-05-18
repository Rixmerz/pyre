//! Integration smoke test: spawn pyred, open control + stream connections,
//! run `echo pyre-smoke` in a PTY session, assert the output arrives.

use std::sync::{Arc, Mutex};
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
async fn smoke_echo_pyre_smoke() {
    let result = tokio::time::timeout(Duration::from_secs(15), run_smoke()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("smoke test failed: {e:#}"),
        Err(_) => panic!("smoke test timed out after 15s"),
    }
}

async fn run_smoke() -> anyhow::Result<()> {
    // --- 1. Temporary XDG_RUNTIME_DIR and spawn pyred ---
    let tmpdir = tempfile::TempDir::new()?;
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", tmpdir.path())
        .env("PYRE_DATA_DIR", tmpdir.path())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // --- 2. Poll up to 3s for socket to appear ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if sock_path.exists() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("socket {} never appeared", sock_path.display());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // --- 3. Control connection ---
    let mut ctrl_sock = UnixStream::connect(&sock_path).await?;
    ctrl_sock.write_all(&[MODE_CONTROL]).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(ctrl_sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    let rpc_client = PyreDaemonClient::new(client::Config::default(), transport).spawn();

    let SpawnResp { session, pane } = rpc_client
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

    // --- 5. Reader task accumulating output ---
    let accumulated: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let acc_clone = Arc::clone(&accumulated);
    let reader_task = tokio::spawn(async move {
        let read_fut = async {
            while let Some(frame) = output_frames.next().await {
                match frame {
                    Ok(f) => acc_clone.lock().unwrap().extend_from_slice(&f.data),
                    Err(_) => break,
                }
            }
        };
        // Give the shell up to 5s to produce the echo output.
        let _ = tokio::time::timeout(Duration::from_secs(5), read_fut).await;
    });

    // --- 6. Send echo command then exit ---
    input_frames
        .send(InputFrame {
            session,
            data: Bytes::from_static(b"echo pyre-smoke\nexit\n"),
        })
        .await
        .map_err(|e| anyhow::anyhow!("send InputFrame: {e}"))?;

    // Drop the writer so the daemon sees EOF on input side.
    drop(input_frames);

    // Wait for reader to finish (it has its own 5s timeout internally).
    let _ = reader_task.await;

    // --- 7. Assert output contains the marker ---
    let output = accumulated.lock().unwrap().clone();
    let found = output
        .windows(b"pyre-smoke".len())
        .any(|w| w == b"pyre-smoke");
    if !found {
        eprintln!(
            "=== pyred stderr ===\n{}",
            // Read whatever stderr was piped (best-effort).
            String::from_utf8_lossy(&[])
        );
        eprintln!(
            "=== PTY output ({} bytes) ===\n{}",
            output.len(),
            String::from_utf8_lossy(&output)
        );
        anyhow::bail!("expected `pyre-smoke` in PTY output but did not find it");
    }

    // Drop the tarpc client to release its background task.
    drop(rpc_client);

    // --- 8. SIGTERM pyred and wait ---
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

    assert!(
        !sock_path.exists(),
        "socket should have been removed after pyred exited"
    );

    Ok(())
}
