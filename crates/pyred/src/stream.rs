//! Stream-mode connection handler: bidirectional PTY I/O over a UDS.
//!
//! After the caller writes MODE_STREAM (0x02), it writes 16 bytes SessionId
//! followed by 16 bytes PaneId (32 bytes total).  The connection then becomes
//! a length-delimited bincode channel:
//!   daemon -> client: `OutputFrame`
//!   client -> daemon: `InputFrame`
//!
//! Phase 1 stub: the registry is keyed by the PtySession id which was
//! returned as the pane id in SpawnResp.  We look up by pane id.

use std::sync::Arc;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use pyre_proto::{InputFrame, OutputFrame, PaneId, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use uuid::Uuid;

use crate::session::SessionRegistry;

pub async fn handle_stream(mut sock: UnixStream, registry: Arc<SessionRegistry>) -> Result<()> {
    // Read session id (16 bytes) then pane id (16 bytes).
    let mut session_buf = [0u8; 16];
    sock.read_exact(&mut session_buf).await?;
    let session_id = SessionId(Uuid::from_bytes(session_buf));

    let mut pane_buf = [0u8; 16];
    sock.read_exact(&mut pane_buf).await?;
    let pane_id = PaneId(Uuid::from_bytes(pane_buf));

    let pty = match registry.get_pane(pane_id).await {
        Some((sess, p)) if sess.id == session_id => p,
        Some(_) => {
            tracing::warn!("stream pane {pane_id} does not belong to session {session_id}");
            let _ = sock.shutdown().await;
            return Ok(());
        }
        None => {
            tracing::warn!("stream for unknown pane {pane_id}");
            let _ = sock.shutdown().await;
            return Ok(());
        }
    };
    // Reuse the session id from the wire for frames.
    let session = session_id;

    // Snapshot the ringbuf before subscribing so we don't miss bytes that
    // arrived between the subscribe call and the first recv().
    let snap = {
        let rb = pty.ringbuf.lock().expect("ringbuf poisoned");
        rb.snapshot()
    };

    let (rd, wr) = sock.into_split();

    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // Send snapshot as seq=0 frame (always, even if empty — uniform client path).
    output_frames
        .send(OutputFrame {
            session,
            seq: 0,
            data: snap,
        })
        .await?;

    // Subscribe *after* the snapshot write.  Any live bytes that arrive after
    // the snapshot was taken will be delivered by the broadcast receiver below.
    let mut sub = pty.subscribe();
    let close_notify = pty.close_notify.clone();

    let input_tx = pty.input_tx.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(frame) = input_frames.next().await {
            match frame {
                Ok(f) => {
                    if input_tx.send(f.data).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("input frame error: {e}");
                    break;
                }
            }
        }
    });

    let send_task = tokio::spawn(async move {
        let mut seq: u64 = 0; // seq 0 was the snapshot frame; live frames start at 1
        loop {
            tokio::select! {
                recv_res = sub.recv() => match recv_res {
                    Ok(data) => {
                        seq = seq.wrapping_add(1);
                        let frame = OutputFrame { session, seq, data };
                        if output_frames.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("output broadcast lagged {n} messages");
                    }
                    Err(_) => break,
                },
                _ = close_notify.notified() => {
                    tracing::info!("pane close_notify fired; dropping stream socket");
                    break;
                }
            }
        }
        // Dropping output_frames here closes the write half of the socket,
        // giving the client an EOF on its read path.
    });

    let _ = tokio::join!(recv_task, send_task);
    Ok(())
}
