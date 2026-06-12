//! Integration smoke test: tantivy-backed search indexes two distinct blocks
//! and retrieves each by a unique synthetic token.
//!
//! Uses the same OSC 133 shell-script injection pattern as blocks.rs: a script
//! writes OSC 133 markers directly to its stdout (PTY master read side),
//! bypassing the line-discipline echo path that would strip raw escape bytes.

use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{
    write_control_client, InputFrame, ListBlocksReq, OutputFrame, PyreDaemonClient,
    SearchBlocksReq, SpawnReq, SpawnResp, MODE_STREAM,
};
use std::sync::{Arc, Mutex};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tantivy_search_indexes_two_distinct_blocks() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_tantivy_test()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("tantivy test failed: {e:#}"),
        Err(_) => panic!("tantivy test timed out after 30s"),
    }
}

async fn run_tantivy_test() -> anyhow::Result<()> {
    // --- 1. Isolated dirs for socket and data ---
    let xdg_dir = tempfile::TempDir::new()?;
    let data_dir = tempfile::TempDir::new()?;
    let sock_path = xdg_dir.path().join("pyre.sock");

    // --- 2. Build two shell scripts, each emitting OSC 133 markers + a unique token ---
    // Token A: tantivytokenalpha  Token B: tantivytokenbeta
    // Synthetic tokens are unlikely to appear in shell prompt noise.
    let script_a = xdg_dir.path().join("block_a.sh");
    let script_b = xdg_dir.path().join("block_b.sh");

    for (path, token) in [
        (&script_a, "tantivytokenalpha"),
        (&script_b, "tantivytokenbeta"),
    ] {
        // Script:
        //   printf '<OSC133;A><token><OSC133;C>'
        //   printf '<OSC133;D;0>'
        let mut script: Vec<u8> = b"#!/bin/sh\nprintf '".to_vec();
        script.push(0x1b);
        script.extend_from_slice(b"]133;A");
        script.push(0x07);
        script.extend_from_slice(token.as_bytes());
        script.push(0x1b);
        script.extend_from_slice(b"]133;C");
        script.push(0x07);
        script.extend_from_slice(b"'\nprintf '");
        script.push(0x1b);
        script.extend_from_slice(b"]133;D;0");
        script.push(0x07);
        script.extend_from_slice(b"'\n");
        std::fs::write(path, &script)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }

    // --- 3. Spawn pyred ---
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", xdg_dir.path())
        .env("PYRE_DATA_DIR", data_dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // --- 4. Poll up to 3 s for socket ---
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

    // --- 5. Control connection ---
    let mut ctrl_sock = UnixStream::connect(&sock_path).await?;
    write_control_client(&mut ctrl_sock).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(ctrl_sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    let rpc_client = PyreDaemonClient::new(client::Config::default(), transport).spawn();

    // --- 6. Spawn a shell session ---
    let SpawnResp { session, pane } = rpc_client
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

    // --- 7. Stream connection ---
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

    // --- 8. Reader task ---
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
        let _ = tokio::time::timeout(Duration::from_secs(10), read_fut).await;
    });

    // --- 9. Run both scripts then exit ---
    let cmd = format!(
        "sh {}\nsh {}\nexit\n",
        script_a.display(),
        script_b.display()
    );
    input_frames
        .send(InputFrame {
            session,
            data: Bytes::from(cmd.into_bytes()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("send InputFrame: {e}"))?;

    drop(input_frames);

    // --- 10. Wait for reader to settle then give parser time to flush ---
    let _ = reader_task.await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // --- 11. Poll list_blocks until both blocks land (max 20 × 200 ms = 4 s) ---
    let mut blocks = Vec::new();
    for _ in 0..20 {
        blocks = rpc_client
            .list_blocks(
                tarpc::context::current(),
                ListBlocksReq {
                    session: None,
                    limit: 10,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("tarpc list_blocks: {e}"))?
            .map_err(|e| anyhow::anyhow!("list_blocks: {e}"))?;

        if blocks.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        blocks.len() >= 2,
        "expected at least 2 blocks in list_blocks, got {}: {:?}",
        blocks.len(),
        blocks.iter().map(|b| &b.command).collect::<Vec<_>>()
    );

    // Extra 200 ms to let tantivy commit both blocks.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --- 12. search_blocks for token A → exactly one hit ---
    // Retry loop: tantivy commit may lag the list_blocks insert by one cycle.
    let hits_a = {
        let mut hits = Vec::new();
        for _ in 0..20 {
            hits = rpc_client
                .search_blocks(
                    tarpc::context::current(),
                    SearchBlocksReq {
                        query: "tantivytokenalpha".into(),
                        limit: 10,
                        failures_only: false,
                        session: None,
                        pane: None,
                        exit_code: None,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("tarpc search_blocks alpha: {e}"))?
                .map_err(|e| anyhow::anyhow!("search_blocks alpha: {e}"))?;
            if !hits.is_empty() {
                break;
            }
            // Race between block insert and tantivy segment commit: bounded wait.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        hits
    };

    assert_eq!(
        hits_a.len(),
        1,
        "search_blocks(\"tantivytokenalpha\") expected 1 hit, got {}: {:?}",
        hits_a.len(),
        hits_a.iter().map(|h| &h.block.command).collect::<Vec<_>>()
    );
    assert!(
        hits_a[0].block.command.contains("tantivytokenalpha"),
        "hit for alpha has unexpected command: {:?}",
        hits_a[0].block.command
    );

    // --- 13. search_blocks for token B → exactly one hit ---
    let hits_b = {
        let mut hits = Vec::new();
        for _ in 0..20 {
            hits = rpc_client
                .search_blocks(
                    tarpc::context::current(),
                    SearchBlocksReq {
                        query: "tantivytokenbeta".into(),
                        limit: 10,
                        failures_only: false,
                        session: None,
                        pane: None,
                        exit_code: None,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("tarpc search_blocks beta: {e}"))?
                .map_err(|e| anyhow::anyhow!("search_blocks beta: {e}"))?;
            if !hits.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        hits
    };

    assert_eq!(
        hits_b.len(),
        1,
        "search_blocks(\"tantivytokenbeta\") expected 1 hit, got {}: {:?}",
        hits_b.len(),
        hits_b.iter().map(|h| &h.block.command).collect::<Vec<_>>()
    );
    assert!(
        hits_b[0].block.command.contains("tantivytokenbeta"),
        "hit for beta has unexpected command: {:?}",
        hits_b[0].block.command
    );

    // --- 14. The two hits must be different blocks ---
    assert_ne!(
        hits_a[0].block.id, hits_b[0].block.id,
        "alpha and beta searches returned the same block id — indexing collision"
    );

    // --- 15. Teardown ---
    drop(rpc_client);

    let pid = Pid::from_raw(child.id() as i32);
    kill(pid, Signal::SIGTERM).ok();

    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
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
