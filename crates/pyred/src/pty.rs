//! PTY spawning for pyred. Linux-first; Windows stubbed.
//!
//! `spawn_pty` is the only public entry point. Callers (session.rs) wrap the
//! returned `PaneState` in an Arc and insert it into a `SessionState`.

#[cfg(unix)]
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use chrono::Utc;
use pyre_proto::{BlockEvent, PaneId, SessionId, SpawnReq};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::session::{PaneState, SessionRegistry};

const OUT_CHANNEL_CAP: usize = 1024;
const IN_CHANNEL_CAP: usize = 256;

/// Spawn a PTY for the given session. The caller is responsible for inserting
/// the returned `PaneState` into `SessionState::panes`.
///
/// `pane_name` is the optional human-readable label for the new pane (distinct
/// from `req.name` which labels the session).
#[cfg(unix)]
pub async fn spawn_pty(
    req: SpawnReq,
    session_id: SessionId,
    pane_name: Option<String>,
    session_name: Option<&str>,
    store: Arc<crate::store::Store>,
    block_index: Arc<crate::index::BlockIndex>,
    registry: Arc<SessionRegistry>,
) -> Result<PaneState> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            cols: req.cols,
            rows: req.rows,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow!("openpty: {e}"))?;

    let shell = req
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/bash".to_string());

    let mut cmd = CommandBuilder::new(&shell);
    if let Some(cwd) = &req.cwd {
        cmd.cwd(cwd);
    }
    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawn shell {shell}"))?;

    let pane_id = PaneId::new();
    let (output_tx, _) = broadcast::channel::<Bytes>(OUT_CHANNEL_CAP);
    let (events_tx, _) = broadcast::channel::<BlockEvent>(OUT_CHANNEL_CAP);
    let (parse_tx, mut parse_rx) = mpsc::unbounded_channel::<Bytes>();
    let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(IN_CHANNEL_CAP);

    // State tracker — root PID obtained after spawn.
    // We initialize with 0 and update it once the child pid is known.
    let (state_tracker_inner, _state_rx) = crate::state::PaneStateTracker::new(0);
    let state_tracker_arc = Arc::new(std::sync::Mutex::new(state_tracker_inner));

    // Reader: blocking std::io::Read on the master in a blocking thread,
    // bridging Bytes back to the async broadcast channel.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow!("clone reader: {e}"))?;
    let out_tx = output_tx.clone();
    let ringbuf_arc = std::sync::Arc::new(std::sync::Mutex::new(crate::ringbuf::RingBuf::new(
        64 * 1024,
    )));
    let ringbuf_thread = ringbuf_arc.clone();
    let state_tracker_reader = state_tracker_arc.clone();
    std::thread::Builder::new()
        .name(format!("pty-reader-{pane_id}"))
        .spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) => {
                        tracing::info!("pty eof");
                        break;
                    }
                    Ok(n) => {
                        {
                            let mut rb = ringbuf_thread
                                .lock()
                                .unwrap_or_else(|e| {
                                    tracing::error!(
                                        "ringbuf lock poisoned in pty reader thread; recovering guard: {e}"
                                    );
                                    e.into_inner()
                                });
                            rb.push(&buf[..n]);
                        }
                        // Update last_output_at on the state tracker.
                        if let Ok(mut t) = state_tracker_reader.lock() {
                            t.touch_output();
                        }
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        // IGNORED: broadcast::send error means no active subscribers;
                        // the ring buffer above already captured the bytes for replay.
                        let _ = out_tx.send(chunk.clone());
                        // IGNORED: unbounded mpsc::send only fails if the receiver is
                        // dropped (parser task exited), which means parsing is already
                        // shutting down — safe to drop remaining chunks.
                        let _ = parse_tx.send(chunk);
                    }
                    Err(e) => {
                        tracing::warn!("pty read: {e}");
                        break;
                    }
                }
            }
        })
        .context("spawn pty reader thread")?;

    // Writer: take master writer once, drive from input_rx in a blocking thread.
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow!("take writer: {e}"))?;
    std::thread::Builder::new()
        .name(format!("pty-writer-{pane_id}"))
        .spawn(move || {
            while let Some(chunk) = input_rx.blocking_recv() {
                if let Err(e) = std::io::Write::write_all(&mut writer, &chunk) {
                    tracing::warn!("pty write: {e}");
                    break;
                }
                // IGNORED: flush error on a PTY is non-fatal; the kernel will
                // drain the write buffer on the next write or on close.
                let _ = std::io::Write::flush(&mut writer);
            }
        })
        .context("spawn pty writer thread")?;

    // Persist session and pane rows before returning.
    // Use the caller-supplied session_name so we don't overwrite the name that
    // new_session() already persisted.  Fall back to "" only when unknown.
    store
        .upsert_session(session_id, session_name.unwrap_or(""))
        .await?;
    store
        .upsert_pane(
            pane_id,
            session_id,
            &shell,
            req.cwd.as_deref(),
            req.cols,
            req.rows,
        )
        .await?;

    // Extract the child PID before wrapping in Arc<Mutex> so we can store it
    // on PaneState without ever needing to re-lock the mutex later.  This PID
    // is used by PaneState::kill() to send SIGTERM without acquiring the child
    // lock (which is held by the child-wait task for the life of the process).
    let child_pid: u32 = child.process_id().unwrap_or(0);

    // Wrap child in Arc<Mutex<>> for the child-wait task.
    let child = Arc::new(Mutex::new(child));

    // close_token: cancelled when the child process exits so stream handlers
    // can break their recv loop and drop the socket, giving clients EOF.
    // CancellationToken is sticky — cancelled() resolves immediately if the
    // token was already cancelled when a task reaches the select arm.
    let close_token = CancellationToken::new();

    // Stash the child PID into the state tracker.
    {
        if child_pid > 0 {
            if let Ok(mut t) = state_tracker_arc.lock() {
                t.root_pid = child_pid;
            }
        }
    }

    // Child-wait task: blocks until the shell exits, cancels close_token so
    // all stream handlers drop their sockets (clients receive EOF), then
    // removes the pane (and its session if empty) from the registry so
    // subsequent list_sessions / attach calls do not see dead state.
    {
        let child_wait = child.clone();
        let token = close_token.clone();
        let registry_wait = Arc::clone(&registry);
        tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let mut guard = child_wait.blocking_lock();
                match guard.wait() {
                    Ok(status) => tracing::info!("child exited: {status:?}"),
                    Err(e) => tracing::warn!("child.wait(): {e}"),
                }
                token.cancel();
                tracing::info!("pane {pane_id} close_token cancelled");
            })
            .await
            .ok();
            registry_wait.remove_pane(pane_id).await;
            tracing::info!("pane {pane_id} removed from registry after exit");
        });
    }

    // Parser task: feed raw PTY bytes through BlockParser, broadcast BlockEvents,
    // and persist blocks/output into the store.
    let events_tx_clone = events_tx.clone();
    let store_clone = store.clone();
    let block_index_clone = block_index.clone();
    let state_tracker_parser = state_tracker_arc.clone();
    tokio::spawn(async move {
        let mut parser = crate::parser::BlockParser::new(session_id);
        let mut writers: HashMap<pyre_proto::BlockId, crate::store::BlobWriter> = HashMap::new();
        // In-memory stdout accumulator per block for indexing (capped at 256 KiB).
        let mut stdout_bufs: HashMap<pyre_proto::BlockId, Vec<u8>> = HashMap::new();
        // Block metadata keyed by id, needed at BlockEnd for indexing.
        let mut block_meta: HashMap<pyre_proto::BlockId, pyre_proto::Block> = HashMap::new();
        let mut events = Vec::new();
        while let Some(chunk) = parse_rx.recv().await {
            events.clear();
            parser.feed(&chunk, &mut events);
            for ev in events.drain(..) {
                // IGNORED: broadcast::send error means no active block-event
                // subscribers; the store persist path below is independent of
                // whether any subscriber receives the event.
                let _ = events_tx_clone.send(ev.clone());

                // Push OSC 133 markers into the state tracker.
                match &ev {
                    BlockEvent::PromptStart { .. } => {
                        if let Ok(mut t) = state_tracker_parser.lock() {
                            t.push_marker(crate::state::Osc133Marker::A);
                        }
                    }
                    BlockEvent::CommandStart { .. } => {
                        if let Ok(mut t) = state_tracker_parser.lock() {
                            t.push_marker(crate::state::Osc133Marker::C);
                        }
                    }
                    BlockEvent::BlockEnd { exit_code, .. } => {
                        if let Ok(mut t) = state_tracker_parser.lock() {
                            t.push_marker(crate::state::Osc133Marker::D {
                                exit_code: *exit_code,
                            });
                        }
                    }
                    BlockEvent::OutputChunk { .. } => {}
                }

                match ev {
                    BlockEvent::PromptStart { .. } => {}
                    BlockEvent::CommandStart {
                        block,
                        ref command,
                        ref cwd,
                        ..
                    } => {
                        // Resolve the foreground process cwd from /proc/<pid>/cwd.
                        // This gives the actual working directory at command-start
                        // time rather than relying on the shell to emit it via OSC.
                        // Falls back to the cwd carried in the event (always None
                        // from the current parser), then to None on any error.
                        #[cfg(unix)]
                        let resolved_cwd: Option<std::path::PathBuf> =
                            { std::fs::read_link(format!("/proc/{child_pid}/cwd")).ok() };
                        #[cfg(not(unix))]
                        let resolved_cwd: Option<std::path::PathBuf> =
                            cwd.as_ref().map(std::path::PathBuf::from);
                        #[cfg(unix)]
                        let resolved_cwd =
                            resolved_cwd.or_else(|| cwd.as_ref().map(std::path::PathBuf::from));

                        let proto_block = pyre_proto::Block {
                            id: block,
                            pane: pane_id,
                            session: session_id,
                            command: command.clone(),
                            cwd: resolved_cwd,
                            started_at: chrono::Utc::now(),
                            ended_at: None,
                            exit_code: None,
                            stdout_len: 0,
                        };
                        if let Err(e) = store_clone.create_block(&proto_block).await {
                            tracing::warn!("store.create_block: {e:#}");
                            continue;
                        }
                        let blob_path = store_clone.blob_path_for(block);
                        match tokio::task::spawn_blocking(move || {
                            crate::store::BlobWriter::open(&blob_path)
                        })
                        .await
                        {
                            Ok(Ok(bw)) => {
                                writers.insert(block, bw);
                            }
                            Ok(Err(e)) => tracing::warn!("BlobWriter::open: {e:#}"),
                            Err(e) => tracing::warn!("spawn_blocking BlobWriter::open: {e}"),
                        }
                        stdout_bufs.insert(block, Vec::new());
                        block_meta.insert(block, proto_block);
                    }
                    BlockEvent::OutputChunk { block, data } => {
                        // Accumulate stdout for indexing (cap at 256 KiB).
                        if let Some(buf) = stdout_bufs.get_mut(&block) {
                            const INDEX_CAP: usize = 256 * 1024;
                            let remaining = INDEX_CAP.saturating_sub(buf.len());
                            if remaining > 0 {
                                let slice = &data[..data.len().min(remaining)];
                                buf.extend_from_slice(slice);
                            }
                        }
                        if let Some(mut bw) = writers.remove(&block) {
                            let bytes_vec = data.to_vec();
                            let result = tokio::task::spawn_blocking(move || {
                                // IGNORED: BlobWriter::write error is logged by
                                // the caller if spawn_blocking itself fails; a
                                // write error here means the block output is
                                // truncated but the daemon stays live.
                                let _ = bw.write(&bytes_vec);
                                bw
                            })
                            .await;
                            if let Ok(bw) = result {
                                writers.insert(block, bw);
                            }
                        }
                    }
                    BlockEvent::BlockEnd { block, exit_code } => {
                        let bw = writers.remove(&block);
                        let stdout_len = if let Some(bw) = bw {
                            tokio::task::spawn_blocking(move || bw.close().unwrap_or(0))
                                .await
                                .unwrap_or(0)
                        } else {
                            0
                        };
                        if let Err(e) = store_clone
                            .finalize_block(block, chrono::Utc::now(), exit_code, stdout_len)
                            .await
                        {
                            tracing::warn!("store.finalize_block: {e:#}");
                        }
                        // Index the block.
                        let stdout_text = stdout_bufs
                            .remove(&block)
                            .and_then(|b| String::from_utf8(b).ok())
                            .unwrap_or_default();
                        if let Some(meta) = block_meta.remove(&block) {
                            let idx = block_index_clone.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Err(e) = idx.add_block(&meta, &stdout_text) {
                                    tracing::warn!("block_index.add_block: {e:#}");
                                }
                            });
                        }
                    }
                }
            }
        }
        // Drain on shutdown — finalize any still-open blocks (best-effort).
        for (block, bw) in writers {
            let stdout_len = tokio::task::spawn_blocking(move || bw.close().unwrap_or(0))
                .await
                .unwrap_or(0);
            // IGNORED: finalize_block error on shutdown drain is best-effort;
            // a failure here only means the block's end timestamp / exit code
            // won't be stored, which is acceptable during process teardown.
            let _ = store_clone
                .finalize_block(block, Utc::now(), None, stdout_len)
                .await;
        }
    });

    Ok(PaneState::new(
        pane_id,
        session_id,
        pane_name,
        req.cols,
        req.rows,
        shell,
        Utc::now(),
        Arc::new(Mutex::new(pair.master)),
        output_tx,
        events_tx,
        input_tx,
        child,
        child_pid,
        ringbuf_arc,
        state_tracker_arc,
        close_token,
    ))
}

#[cfg(not(unix))]
pub async fn spawn_pty(
    _req: SpawnReq,
    _session_id: SessionId,
    _pane_name: Option<String>,
    _session_name: Option<&str>,
    _store: Arc<crate::store::Store>,
    _block_index: Arc<crate::index::BlockIndex>,
    _registry: Arc<SessionRegistry>,
) -> Result<PaneState> {
    anyhow::bail!("pyred PTY only supported on unix in S1")
}
