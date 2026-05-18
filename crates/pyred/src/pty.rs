//! PTY session lifecycle for pyred. Linux-first; Windows stubbed.
//!
//! Public surface (`spawn_pty`, `PtySession`, `SessionRegistry`) is consumed
//! by the UDS transport layer (server.rs, stream.rs).

#[cfg(unix)]
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use pyre_proto::{SessionId, SpawnReq};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

const OUT_CHANNEL_CAP: usize = 1024;
const IN_CHANNEL_CAP: usize = 256;

pub struct PtySession {
    pub id: SessionId,
    pub cols: u16,
    pub rows: u16,
    /// Hold the master so we can resize; wrapped in Mutex for &self resize.
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    /// Broadcast lets multiple stream-mode connections subscribe (future).
    pub output_tx: broadcast::Sender<Bytes>,
    /// Single producer of input (the active stream connection).
    pub input_tx: mpsc::Sender<Bytes>,
    /// Hold the child so it doesn't drop. Mutex for kill(). Used in phase 4 kill().
    #[allow(dead_code)]
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl PtySession {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.output_tx.subscribe()
    }

    #[expect(dead_code, reason = "PTY resize RPC lands in S1+")]
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().await;
        master
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("resize: {e}"))
    }

    pub async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        let _ = child.kill();
        Ok(())
    }
}

#[cfg(unix)]
pub fn spawn_pty(req: SpawnReq) -> Result<PtySession> {
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

    let id = SessionId::new();
    let (output_tx, _) = broadcast::channel::<Bytes>(OUT_CHANNEL_CAP);
    let (input_tx, mut input_rx) = mpsc::channel::<Bytes>(IN_CHANNEL_CAP);

    // Reader: blocking std::io::Read on the master in a blocking thread,
    // bridging Bytes back to the async broadcast channel.
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow!("clone reader: {e}"))?;
    let out_tx = output_tx.clone();
    std::thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; 4096];
            loop {
                match std::io::Read::read(&mut reader, &mut buf) {
                    Ok(0) => {
                        tracing::info!("pty eof");
                        break;
                    }
                    Ok(n) => {
                        let _ = out_tx.send(Bytes::copy_from_slice(&buf[..n]));
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
        .name(format!("pty-writer-{id}"))
        .spawn(move || {
            while let Some(chunk) = input_rx.blocking_recv() {
                if let Err(e) = std::io::Write::write_all(&mut writer, &chunk) {
                    tracing::warn!("pty write: {e}");
                    break;
                }
                let _ = std::io::Write::flush(&mut writer);
            }
        })
        .context("spawn pty writer thread")?;

    Ok(PtySession {
        id,
        cols: req.cols,
        rows: req.rows,
        master: Arc::new(Mutex::new(pair.master)),
        output_tx,
        input_tx,
        child: Arc::new(Mutex::new(child)),
    })
}

#[cfg(not(unix))]
pub fn spawn_pty(_req: SpawnReq) -> Result<PtySession> {
    anyhow::bail!("pyred PTY only supported on unix in S1")
}

#[derive(Default)]
pub struct SessionRegistry {
    inner: Mutex<HashMap<SessionId, Arc<PtySession>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, sess: PtySession) -> Arc<PtySession> {
        let arc = Arc::new(sess);
        self.inner.lock().await.insert(arc.id, arc.clone());
        arc
    }

    pub async fn get(&self, id: SessionId) -> Option<Arc<PtySession>> {
        self.inner.lock().await.get(&id).cloned()
    }

    pub async fn remove(&self, id: SessionId) -> Option<Arc<PtySession>> {
        self.inner.lock().await.remove(&id)
    }

    pub async fn all(&self) -> Vec<Arc<PtySession>> {
        self.inner.lock().await.values().cloned().collect()
    }
}
