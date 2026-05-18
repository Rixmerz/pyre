//! Integration smoke tests for multi-pane, mirror, and reattach-replay (S3).
//!
//! Each test spawns a fresh pyred process with isolated TempDir state.
//! All tests have a 30-second outer timeout to prevent CI hangs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct PyredHandle {
    child: std::process::Child,
    sock_path: std::path::PathBuf,
    _xdg_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
}

impl Drop for PyredHandle {
    fn drop(&mut self) {
        let pid = Pid::from_raw(self.child.id() as i32);
        kill(pid, Signal::SIGTERM).ok();
        let _ = self.child.wait();
    }
}

async fn spawn_pyred() -> anyhow::Result<(PyredHandle, PyreDaemonClient)> {
    let xdg_dir = tempfile::TempDir::new()?;
    let data_dir = tempfile::TempDir::new()?;
    let sock_path = xdg_dir.path().join("pyre.sock");

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", xdg_dir.path())
        .env("PYRE_DATA_DIR", data_dir.path())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Poll up to 3 s for socket.
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

    let rpc_client = connect_control(&sock_path).await?;

    Ok((
        PyredHandle {
            child,
            sock_path,
            _xdg_dir: xdg_dir,
            _data_dir: data_dir,
        },
        rpc_client,
    ))
}

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut ctrl_sock = UnixStream::connect(sock_path).await?;
    ctrl_sock.write_all(&[MODE_CONTROL]).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(ctrl_sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

/// Open a stream connection for (session, pane) and return split framed halves.
async fn open_stream(
    sock_path: &std::path::Path,
    session: pyre_proto::SessionId,
    pane: pyre_proto::PaneId,
) -> anyhow::Result<(
    tokio_serde::SymmetricallyFramed<
        FramedRead<tokio::net::unix::OwnedReadHalf, LengthDelimitedCodec>,
        OutputFrame,
        SymmetricalBincode<OutputFrame>,
    >,
    tokio_serde::SymmetricallyFramed<
        FramedWrite<tokio::net::unix::OwnedWriteHalf, LengthDelimitedCodec>,
        InputFrame,
        SymmetricalBincode<InputFrame>,
    >,
)> {
    let mut stream_sock = UnixStream::connect(sock_path).await?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let output_frames = tokio_serde::SymmetricallyFramed::new(
        FramedRead::new(rd, LengthDelimitedCodec::new()),
        SymmetricalBincode::<OutputFrame>::default(),
    );
    let input_frames = tokio_serde::SymmetricallyFramed::new(
        FramedWrite::new(wr, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputFrame>::default(),
    );
    Ok((output_frames, input_frames))
}

/// Drain output frames into an `Arc<Mutex<Vec<u8>>>` until timeout elapses or
/// the stream closes. Returns the accumulated buffer.
async fn drain_with_timeout(
    mut output_frames: tokio_serde::SymmetricallyFramed<
        FramedRead<tokio::net::unix::OwnedReadHalf, LengthDelimitedCodec>,
        OutputFrame,
        SymmetricalBincode<OutputFrame>,
    >,
    timeout: Duration,
) -> Vec<u8> {
    let acc: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let acc_clone = Arc::clone(&acc);
    let read_fut = async move {
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => acc_clone.lock().unwrap().extend_from_slice(&f.data),
                Err(_) => break,
            }
        }
    };
    let _ = tokio::time::timeout(timeout, read_fut).await;
    Arc::try_unwrap(acc).unwrap().into_inner().unwrap()
}

fn contains_marker(buf: &[u8], marker: &[u8]) -> bool {
    buf.windows(marker.len()).any(|w| w == marker)
}

// ---------------------------------------------------------------------------
// Test 1 — multi-pane in one session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_pane_two_panes_in_one_session() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_multi_pane_test()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("multi_pane test failed: {e:#}"),
        Err(_) => panic!("multi_pane test timed out after 30s"),
    }
}

async fn run_multi_pane_test() -> anyhow::Result<()> {
    let (handle, rpc) = spawn_pyred().await?;

    // 1. Spawn session + first pane.
    let SpawnResp { session, pane: p1 } = rpc
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
        .map_err(|e| anyhow::anyhow!("tarpc spawn: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

    // 2. Open a second pane in the same session.
    let p2 = rpc
        .open_pane(
            tarpc::context::current(),
            OpenPaneReq {
                session,
                shell: Some("/bin/sh".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                env: vec![("PS1".into(), "".into())],
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc open_pane: {e}"))?
        .map_err(|e| anyhow::anyhow!("open_pane: {e}"))?;

    // 3. list_panes must return exactly two panes.
    let panes = rpc
        .list_panes(tarpc::context::current(), session)
        .await
        .map_err(|e| anyhow::anyhow!("tarpc list_panes: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_panes: {e}"))?;

    assert_eq!(
        panes.len(),
        2,
        "expected 2 panes, got {}: {:?}",
        panes.len(),
        panes.iter().map(|p| p.id).collect::<Vec<_>>()
    );
    let pane_ids: Vec<_> = panes.iter().map(|p| p.id).collect();
    assert!(
        pane_ids.contains(&p1),
        "p1 {p1} not found in list_panes: {pane_ids:?}"
    );
    assert!(
        pane_ids.contains(&p2),
        "p2 {p2} not found in list_panes: {pane_ids:?}"
    );

    // 4. Stream p1: send echo + exit, assert output contains marker.
    let (out1, mut in1) = open_stream(&handle.sock_path, session, p1).await?;
    in1.send(InputFrame {
        session,
        data: Bytes::from_static(b"echo p1_marker\nexit\n"),
    })
    .await
    .map_err(|e| anyhow::anyhow!("send p1: {e}"))?;
    drop(in1);
    let buf1 = drain_with_timeout(out1, Duration::from_secs(5)).await;
    assert!(
        contains_marker(&buf1, b"p1_marker"),
        "p1 output did not contain 'p1_marker' ({} bytes): {}",
        buf1.len(),
        String::from_utf8_lossy(&buf1)
    );

    // 5. Stream p2: send echo + exit, assert output contains marker.
    let (out2, mut in2) = open_stream(&handle.sock_path, session, p2).await?;
    in2.send(InputFrame {
        session,
        data: Bytes::from_static(b"echo p2_marker\nexit\n"),
    })
    .await
    .map_err(|e| anyhow::anyhow!("send p2: {e}"))?;
    drop(in2);
    let buf2 = drain_with_timeout(out2, Duration::from_secs(5)).await;
    assert!(
        contains_marker(&buf2, b"p2_marker"),
        "p2 output did not contain 'p2_marker' ({} bytes): {}",
        buf2.len(),
        String::from_utf8_lossy(&buf2)
    );

    // 6. Both panes have exited; the daemon auto-removes the session once its
    //    last pane exits. list_sessions must now be empty.
    // Give the async remove_pane task a moment to run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let sessions = rpc
        .list_sessions(tarpc::context::current())
        .await
        .map_err(|e| anyhow::anyhow!("tarpc list_sessions: {e}"))?
        .map_err(|e| anyhow::anyhow!("list_sessions: {e}"))?;
    assert_eq!(
        sessions.len(),
        0,
        "expected 0 sessions after all panes exited, got {}: {:?}",
        sessions.len(),
        sessions.iter().map(|s| s.id).collect::<Vec<_>>()
    );

    drop(rpc);
    drop(handle);
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2 — mirror (two clients on the same pane)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mirror_two_clients_receive_same_output() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_mirror_test()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("mirror test failed: {e:#}"),
        Err(_) => panic!("mirror test timed out after 30s"),
    }
}

async fn run_mirror_test() -> anyhow::Result<()> {
    let (handle, rpc) = spawn_pyred().await?;

    let SpawnResp { session, pane } = rpc
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
        .map_err(|e| anyhow::anyhow!("tarpc spawn: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

    // 2. Open two stream connections to the same pane.
    let (out_a, mut in_a) = open_stream(&handle.sock_path, session, pane).await?;
    let (out_b, _in_b) = open_stream(&handle.sock_path, session, pane).await?;

    // Start draining both before sending input.
    let acc_a: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let acc_b: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let acc_a_clone = Arc::clone(&acc_a);
    let acc_b_clone = Arc::clone(&acc_b);

    let task_a = tokio::spawn(async move {
        let read_fut = async move {
            let mut out_a = out_a;
            while let Some(frame) = out_a.next().await {
                match frame {
                    Ok(f) => {
                        acc_a_clone.lock().unwrap().extend_from_slice(&f.data);
                        if contains_marker(&acc_a_clone.lock().unwrap(), b"mirror_test") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), read_fut).await;
    });

    let task_b = tokio::spawn(async move {
        let read_fut = async move {
            let mut out_b = out_b;
            while let Some(frame) = out_b.next().await {
                match frame {
                    Ok(f) => {
                        acc_b_clone.lock().unwrap().extend_from_slice(&f.data);
                        if contains_marker(&acc_b_clone.lock().unwrap(), b"mirror_test") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), read_fut).await;
    });

    // Give readers time to register before sending input.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Send echo from stream A.
    in_a.send(InputFrame {
        session,
        data: Bytes::from_static(b"echo mirror_test\n"),
    })
    .await
    .map_err(|e| anyhow::anyhow!("send mirror_test: {e}"))?;

    task_a.await.ok();
    task_b.await.ok();

    // 5. Assert both buffers contain the marker.
    let buf_a = acc_a.lock().unwrap().clone();
    let buf_b = acc_b.lock().unwrap().clone();

    assert!(
        contains_marker(&buf_a, b"mirror_test"),
        "stream A did not receive 'mirror_test' ({} bytes): {}",
        buf_a.len(),
        String::from_utf8_lossy(&buf_a)
    );
    assert!(
        contains_marker(&buf_b, b"mirror_test"),
        "stream B did not receive 'mirror_test' ({} bytes): {}",
        buf_b.len(),
        String::from_utf8_lossy(&buf_b)
    );

    // 6. Exit the shell.
    in_a.send(InputFrame {
        session,
        data: Bytes::from_static(b"exit\n"),
    })
    .await
    .map_err(|e| anyhow::anyhow!("send exit: {e}"))?;
    drop(in_a);

    drop(rpc);
    drop(handle);
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3 — reattach replay (ring-buffer replays on second connection)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reattach_first_output_is_ringbuf_replay() {
    let result = tokio::time::timeout(Duration::from_secs(30), run_reattach_test()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("reattach test failed: {e:#}"),
        Err(_) => panic!("reattach test timed out after 30s"),
    }
}

async fn run_reattach_test() -> anyhow::Result<()> {
    let (handle, rpc) = spawn_pyred().await?;

    let SpawnResp { session, pane } = rpc
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
        .map_err(|e| anyhow::anyhow!("tarpc spawn: {e}"))?
        .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

    // 2. First stream connection: send command, wait for output, then close.
    {
        let (out1, mut in1) = open_stream(&handle.sock_path, session, pane).await?;
        in1.send(InputFrame {
            session,
            data: Bytes::from_static(b"echo first_run\n"),
        })
        .await
        .map_err(|e| anyhow::anyhow!("send first_run: {e}"))?;

        let buf = drain_with_timeout(out1, Duration::from_secs(4)).await;
        assert!(
            contains_marker(&buf, b"first_run"),
            "first connection did not receive 'first_run' ({} bytes): {}",
            buf.len(),
            String::from_utf8_lossy(&buf)
        );
        // Drop in1 / out1 to close the socket.
        drop(in1);
    }

    // 3. Brief pause to let pyred notice the disconnect.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 4. Second stream connection: the first OutputFrame is the ring-buffer
    //    replay and must contain `first_run`.
    let (mut out2, mut in2) = open_stream(&handle.sock_path, session, pane).await?;

    let replay_frame = tokio::time::timeout(Duration::from_secs(3), out2.next())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for replay frame"))?
        .ok_or_else(|| anyhow::anyhow!("stream closed before replay frame"))?
        .map_err(|e| anyhow::anyhow!("read replay frame: {e}"))?;

    assert!(
        contains_marker(&replay_frame.data, b"first_run"),
        "replay frame did not contain 'first_run' ({} bytes): {}",
        replay_frame.data.len(),
        String::from_utf8_lossy(&replay_frame.data)
    );

    // 5. Clean exit.
    in2.send(InputFrame {
        session,
        data: Bytes::from_static(b"exit\n"),
    })
    .await
    .map_err(|e| anyhow::anyhow!("send exit: {e}"))?;
    drop(in2);
    let _ = drain_with_timeout(out2, Duration::from_secs(2)).await;

    drop(rpc);
    drop(handle);
    Ok(())
}
