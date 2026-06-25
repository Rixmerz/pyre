//! Integration test: full block pipeline end-to-end.
//! Spawns pyred, injects a shell script that emits OSC 133 markers to the PTY,
//! then verifies that list_blocks, search_blocks, and the blob file all reflect
//! the captured output.
//!
//! Why a script file instead of inline `printf '\x1b]...'`:
//! When bytes are written to the PTY's input side, the terminal's line discipline
//! echoes them back — and during that echo pass the terminal strips raw escape
//! bytes.  A shell *script* (executed non-interactively via `sh <path>`) writes
//! directly to its stdout, which is the PTY master's read side.  Those bytes are
//! never processed by the echo path, so the OSC 133 sequences arrive intact at
//! pyred's parser.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{
    write_control_client, InputFrame, ListBlocksReq, OutputFrame, PyreDaemonClient,
    SearchBlocksReq, SpawnReq, SpawnResp, MODE_STREAM,
};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocks_pipeline_end_to_end() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_blocks_test()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("blocks test failed: {e:#}"),
        Err(_) => panic!("blocks test timed out after 30s"),
    }
}

async fn run_blocks_test() -> anyhow::Result<()> {
    // --- 1. Two TempDirs: one for socket, one for data ---
    let xdg_dir = tempfile::TempDir::new()?;
    let data_dir = tempfile::TempDir::new()?;
    let sock_path = xdg_dir.path().join("pyre.sock");

    // Write a shell script that emits the OSC 133 markers directly to its own
    // stdout (not through the terminal echo path).  The script:
    //   1. Emits OSC 133;A  (prompt start)
    //   2. Emits "pwd"       (the command name captured by the parser)
    //   3. Emits OSC 133;C  (output start — triggers CommandStart event)
    //   4. Runs actual `pwd` so its output is captured in the blob
    //   5. Emits OSC 133;D;0 (block end, exit 0)
    let script_path = xdg_dir.path().join("run_block.sh");
    // Build bytes: ESC ] 133 ; A BEL  pwd  ESC ] 133 ; C BEL
    //              <pwd output>
    //              ESC ] 133 ; D ; 0 BEL
    let mut script: Vec<u8> = b"#!/bin/sh\nprintf '".to_vec();
    script.push(0x1b);
    script.extend_from_slice(b"]133;A");
    script.push(0x07);
    script.extend_from_slice(b"pwd");
    script.push(0x1b);
    script.extend_from_slice(b"]133;C");
    script.push(0x07);
    script.extend_from_slice(b"'\npwd\nprintf '");
    script.push(0x1b);
    script.extend_from_slice(b"]133;D;0");
    script.push(0x07);
    script.extend_from_slice(b"'\n");
    std::fs::write(&script_path, &script)?;
    // Make it executable.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    // --- 2. Spawn pyred with both env vars set ---
    // XDG_CONFIG_HOME is pointed at the temp dir so pyred uses default (single)
    // process model regardless of the user's ~/.config/pyre/config.toml setting.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", xdg_dir.path())
        .env("PYRE_DATA_DIR", data_dir.path())
        .env("XDG_CONFIG_HOME", xdg_dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // --- 3. Poll up to 3s for socket ---
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

    // --- 4. Control connection ---
    let mut ctrl_sock = UnixStream::connect(&sock_path).await?;
    write_control_client(&mut ctrl_sock).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(ctrl_sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    let rpc_client = PyreDaemonClient::new(client::Config::default(), transport).spawn();

    let SpawnResp { session, pane, .. } = rpc_client
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

    // --- 5. Stream connection ---
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

    // --- 6. Reader task accumulating output ---
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
        let _ = tokio::time::timeout(Duration::from_secs(8), read_fut).await;
    });

    // --- 7. Send the script path as the command, then exit ---
    // The shell executes `sh <script>` which writes OSC 133 bytes directly
    // to its stdout (the PTY master read side), bypassing the echo path.
    let cmd = format!("sh {}\nexit\n", script_path.display());
    input_frames
        .send(InputFrame {
            session,
            data: Bytes::from(cmd.into_bytes()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("send InputFrame: {e}"))?;

    drop(input_frames);

    // --- 8. Wait for reader to settle, then give parser task time to flush ---
    let _ = reader_task.await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // --- 9. list_blocks: assert at least one block with command == "pwd" ---
    let blocks = rpc_client
        .list_blocks(
            tarpc::context::current(),
            ListBlocksReq {
                session: None,
                limit: 10,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc list_blocks transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_blocks: {e}"))?;

    let pwd_block = blocks
        .iter()
        .find(|b| b.command.trim() == "pwd")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "expected a block with command=\"pwd\", got: {:?}",
                blocks.iter().map(|b| &b.command).collect::<Vec<_>>()
            )
        })?;

    assert_eq!(
        pwd_block.exit_code,
        Some(0),
        "expected exit_code Some(0), got {:?}",
        pwd_block.exit_code
    );

    let bid = pwd_block.id;

    // --- 10. search_blocks: query by the command name "pwd" which is indexed ---
    // The tantivy tokenizer splits on non-alphanumeric characters, so "/" does
    // not produce a token. The command field contains "pwd" as a whole token.
    let hits = rpc_client
        .search_blocks(
            tarpc::context::current(),
            SearchBlocksReq {
                query: "pwd".into(),
                limit: 10,
                failures_only: false,
                session: None,
                pane: None,
                exit_code: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc search_blocks transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("search_blocks: {e}"))?;

    assert!(
        !hits.is_empty(),
        "search_blocks(\"pwd\") returned 0 hits — block was not indexed"
    );

    // --- 11. Blob file exists and is non-empty ---
    // Store::open uses PYRE_DATA_DIR directly as data_dir (no "pyre" join).
    // blob_path_for = data_dir / "blocks" / "<id>.zst"
    let blob_path = data_dir
        .path()
        .join("blocks")
        .join(format!("{}.zst", bid.0));
    assert!(
        blob_path.exists(),
        "blob file not found at {}",
        blob_path.display()
    );
    assert!(
        std::fs::metadata(&blob_path)?.len() > 0,
        "blob file is empty at {}",
        blob_path.display()
    );

    // Drop the tarpc client.
    drop(rpc_client);

    // --- 12. SIGTERM pyred and wait ---
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

    assert!(
        !sock_path.exists(),
        "socket should have been removed after pyred exited"
    );

    Ok(())
}
