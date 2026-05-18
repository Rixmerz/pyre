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
    let _session = SessionId(Uuid::from_bytes(session_buf));

    let mut pane_buf = [0u8; 16];
    sock.read_exact(&mut pane_buf).await?;
    let pane = PaneId(Uuid::from_bytes(pane_buf));

    // TODO(s3-phase4-stream-target): replace with registry.get_pane(pane) once
    // the client sends a real PaneId. For now look up by pane id directly.
    let pty = match registry.get_pane(pane).await {
        Some((_sess, p)) => p,
        None => {
            tracing::warn!("stream for unknown pane {pane}");
            let _ = sock.shutdown().await;
            return Ok(());
        }
    };
    // Use pane id as the session field in frames (matches what the client sent).
    let session = SessionId(pane.0);

    let (rd, wr) = sock.into_split();

    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

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

    let mut sub = pty.subscribe();
    let send_task = tokio::spawn(async move {
        let mut seq: u64 = 0;
        loop {
            match sub.recv().await {
                Ok(data) => {
                    seq = seq.wrapping_add(1);
                    let frame = OutputFrame { session, seq, data };
                    if output_frames.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("output broadcast lagged {n} messages");
                    continue;
                }
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(recv_task, send_task);
    Ok(())
}
