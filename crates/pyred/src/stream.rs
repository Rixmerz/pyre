//! Stream-mode connection handler: bidirectional PTY I/O over a UDS.
//!
//! After the caller writes MODE_STREAM (0x02), it writes a 16-byte
//! SessionId (Uuid as_bytes), then the connection becomes a
//! length-delimited bincode channel:
//!   daemon -> client: `OutputFrame`
//!   client -> daemon: `InputFrame`

use std::sync::Arc;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use pyre_proto::{InputFrame, OutputFrame, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use uuid::Uuid;

use crate::pty::SessionRegistry;

pub async fn handle_stream(mut sock: UnixStream, registry: Arc<SessionRegistry>) -> Result<()> {
    let mut id_buf = [0u8; 16];
    sock.read_exact(&mut id_buf).await?;
    let session = SessionId(Uuid::from_bytes(id_buf));

    let pty = match registry.get(session).await {
        Some(p) => p,
        None => {
            tracing::warn!("stream for unknown session {session}");
            let _ = sock.shutdown().await;
            return Ok(());
        }
    };

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
