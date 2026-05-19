//! Headless typing harness: isolates where chars are lost when typing "fastfetch".
//!
//! Sends 'f','a','s','t','f','e','t','c','h','\n' one byte at a time via the
//! `send_keys` RPC (100 ms apart), collects all OutputFrame bytes for 5 s, then:
//!   - Hex-dumps the first 500 bytes.
//!   - Searches for "fastfetch" in raw bytes.
//!   - Feeds the same bytes into vt100::Parser and searches for "fastfetch" in
//!     the rendered screen grid.
//!
//! Run with:
//!   cargo test --test headless_typing -- --nocapture

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pyre_proto::{OutputFrame, PyreDaemonClient, SpawnReq, SpawnResp, MODE_CONTROL, MODE_STREAM};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, LengthDelimitedCodec};

// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn headless_typing_fastfetch() {
    let result = tokio::time::timeout(Duration::from_secs(60), run_headless_typing()).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("headless_typing failed: {e:#}"),
        Err(_) => panic!("headless_typing timed out after 60s"),
    }
}

async fn run_headless_typing() -> anyhow::Result<()> {
    // ── 1. Temporary dir + spawn pyred ──────────────────────────────────────
    let tmpdir = tempfile::TempDir::new()?;
    let sock_path = tmpdir.path().join("pyre.sock");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_pyred"))
        .env("XDG_RUNTIME_DIR", tmpdir.path())
        .env("PYRE_DATA_DIR", tmpdir.path())
        .env("RUST_LOG", "pyred=debug,tarpc=warn")
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // ── 2. Wait for socket ──────────────────────────────────────────────────
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let ctrl = loop {
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

    // ── 3. Spawn session + pane (bash, 102×44) ──────────────────────────────
    let SpawnResp { session, pane } = ctrl
        .spawn(
            tarpc::context::current(),
            SpawnReq {
                shell: Some("/bin/bash".into()),
                cwd: Some("/tmp".into()),
                cols: 102,
                rows: 44,
                // Minimal PS1 so we don't drown in prompt escape sequences
                env: vec![
                    ("PS1".into(), "$ ".into()),
                    ("TERM".into(), "xterm-256color".into()),
                ],
                name: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("tarpc transport: {e}"))?
        .map_err(|e| anyhow::anyhow!("daemon spawn: {e}"))?;

    eprintln!("[harness] session={session} pane={pane}");

    // ── 4. Open stream connection ────────────────────────────────────────────
    let mut stream_sock = UnixStream::connect(&sock_path).await?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;

    let (rd, _wr) = stream_sock.into_split();
    // We only need the read half; we use send_keys RPC for input.
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());

    // ── 5. Spawn reader task ─────────────────────────────────────────────────
    let accumulated: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let acc_clone = Arc::clone(&accumulated);
    let reader_task = tokio::spawn(async move {
        // Collect for up to 5 s after the caller signals "done sending".
        // We drive it for longer here and abort from outside.
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    let mut buf = acc_clone.lock().unwrap();
                    buf.extend_from_slice(&f.data);
                }
                Err(e) => {
                    eprintln!("[reader] frame error: {e}");
                    break;
                }
            }
        }
    });

    // Let bash initialise (prompt, rc scripts).
    tokio::time::sleep(Duration::from_millis(800)).await;
    // Drain whatever the shell already sent (prompt bytes).
    {
        let _ = accumulated.lock().unwrap().len();
        eprintln!(
            "[harness] bytes received after shell init: {}",
            accumulated.lock().unwrap().len()
        );
    }

    // ── 6. Send "fastfetch\n" one byte at a time, 100 ms apart ──────────────
    let target = b"fastfetch\n";
    for (i, &byte) in target.iter().enumerate() {
        let before = accumulated.lock().unwrap().len();

        ctrl.send_keys(tarpc::context::current(), pane, vec![byte])
            .await
            .map_err(|e| anyhow::anyhow!("tarpc transport byte {i}: {e}"))?
            .map_err(|e| anyhow::anyhow!("daemon send_keys byte {i}: {e}"))?;

        eprintln!(
            "[harness] sent byte {i}: 0x{byte:02x} ({:?})  bytes_before={before}",
            char::from(byte)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ── 7. Collect output for 5 more seconds ────────────────────────────────
    eprintln!("[harness] all bytes sent; collecting output for 5 s …");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Abort the reader; we have all we need.
    reader_task.abort();

    let raw: Vec<u8> = accumulated.lock().unwrap().clone();

    // ── 8. Report ────────────────────────────────────────────────────────────
    let total = raw.len();
    eprintln!("\n========= HEADLESS TYPING REPORT =========");
    eprintln!("Total bytes received: {total}");

    // Hex+ASCII dump — first 500 bytes
    let dump_len = total.min(500);
    eprintln!("\n--- first {dump_len} bytes (hex | ascii) ---");
    for (i, chunk) in raw[..dump_len].chunks(16).enumerate() {
        let hex: String = chunk
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        eprintln!("{:04x}  {:47}  |{}|", i * 16, hex, ascii);
    }

    // Search for "fastfetch" in raw bytes
    let needle = b"fastfetch";
    let raw_found = raw.windows(needle.len()).any(|w| w == needle);
    eprintln!("\n'fastfetch' in raw bytes: {raw_found}");

    // Search for "bash:" in raw bytes (error message)
    let bash_err_found = raw.windows(b"bash:".len()).any(|w| w == b"bash:");
    eprintln!("'bash:' in raw bytes:     {bash_err_found}");

    // ── 9. Feed raw bytes into vt100::Parser and inspect screen ─────────────
    let mut parser = vt100::Parser::new(44, 102, 10_000);
    parser.process(&raw);
    let screen_text = parser.screen().contents();

    eprintln!("\n--- vt100 screen contents ---");
    eprintln!("{screen_text}");

    let screen_found = screen_text.contains("fastfetch");
    eprintln!("\n'fastfetch' in vt100 screen: {screen_found}");

    // ── 10. Diagnostics — count how many echo'd chars arrived ────────────────
    // The shell echoes each char; count how many of f,a,s,t,f,e,t,c,h appear
    // in the raw bytes between the first sent byte and 5 s later.
    eprintln!("\n--- Individual char presence in raw output ---");
    for ch in b"fastfetch".iter() {
        let count = raw.iter().filter(|&&b| b == *ch).count();
        eprintln!("  '{}' (0x{:02x}): {count} occurrences", *ch as char, ch);
    }

    // ── 11. Assertions ───────────────────────────────────────────────────────
    assert!(
        total > 0,
        "no output bytes received at all — stream or PTY is completely broken"
    );

    // The primary assertion: "fastfetch" must appear somewhere in raw bytes OR
    // on the parsed screen. If it appears in raw but not screen, the bug is in
    // the vt100 parser / renderer. If neither, the bug is upstream (input path
    // or PTY echo).
    if !raw_found && !screen_found {
        // Print whatever capture_pane returns for additional evidence.
        if let Ok(cap) = ctrl.capture_pane(tarpc::context::current(), pane, 50).await {
            if let Ok(bytes) = cap {
                eprintln!(
                    "\n--- capture_pane output ---\n{}",
                    String::from_utf8_lossy(&bytes)
                );
            }
        }
        panic!(
            "'fastfetch' not found in raw bytes ({raw_found}) \
             nor in vt100 screen ({screen_found}). \
             Total bytes received: {total}. \
             bash_err_found: {bash_err_found}. \
             See hex dump above for details."
        );
    }

    // Soft warning if raw has it but screen doesn't (parser/render bug).
    if raw_found && !screen_found {
        eprintln!(
            "\nWARNING: 'fastfetch' present in raw bytes but missing from \
             vt100 screen — likely a parser/render bug, not an input-loss bug."
        );
    }

    // ── 12. Cleanup ──────────────────────────────────────────────────────────
    drop(ctrl);
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

async fn connect_control(sock_path: &std::path::Path) -> anyhow::Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(sock_path).await?;
    sock.write_all(&[MODE_CONTROL]).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}
