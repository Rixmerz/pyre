//! Integration test: send_keys control RPC delivers bytes to the PTY.
//!
//! Spawns pyred, creates a session+pane via control RPC, injects a command
//! via the new `send_keys` RPC, then asserts `capture_pane` shows the
//! expected output. This verifies the fix for the race where the stream
//! connection closed before the daemon forwarded the InputFrame.

#[allow(dead_code)]
#[path = "common.rs"]
mod common;

use std::os::unix::process::CommandExt as _;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{write_control_client, PyreDaemonClient, SpawnReq, SpawnResp};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::net::UnixStream;
use tokio_util::codec::LengthDelimitedCodec;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_keys_rpc_delivers_bytes() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_send_keys()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("send_keys test failed: {e:#}"),
        Err(_) => panic!("send_keys test timed out after 30s"),
    }
}

async fn run_send_keys() -> anyhow::Result<()> {
    // --- 1. Temporary dir + spawn pyred ---
    let tmpdir = tempfile::TempDir::new()?;
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = common::ChildGuard(
        std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
            .env("XDG_RUNTIME_DIR", tmpdir.path())
            .env("PYRE_DATA_DIR", tmpdir.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()?,
    );

    // --- 2. Wait for socket ---
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

    // --- 3. Spawn session+pane ---
    let SpawnResp {
        session: _, pane, ..
    } = client
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "".into())],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("daemon spawn: {e}"))?;

    // Give the shell a moment to initialize.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- 4. Inject command via send_keys RPC ---
    const MARKER: &str = "MCPFIX_OK";
    client
        .send_keys(
            tarpc::context::current(),
            pane,
            format!("echo {MARKER}\n").into_bytes(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("daemon send_keys: {e}"))?;

    // --- 5. Poll capture_pane until marker appears (20 × 100 ms) ---
    let client2 = connect_control(&sock_path).await?;
    let mut found = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let bytes = client2
            .capture_pane(tarpc::context::current(), pane, 50)
            .await
            .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
            .map_err(|e| anyhow::anyhow!("daemon capture_pane: {e}"))?;
        if String::from_utf8_lossy(&bytes).contains(MARKER) {
            found = true;
            break;
        }
    }

    assert!(
        found,
        "expected '{MARKER}' in capture_pane output after send_keys RPC"
    );

    // --- 6. Cleanup ---
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
            child.kill();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Ok(())
    // `child` guard drops here; process already reaped above on the happy path.
}

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(sock_path).await?;
    write_control_client(&mut sock).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}
