//! pyred — Pyre daemon.
//!
//! Binds a UDS at `$XDG_RUNTIME_DIR/pyre.sock` (mode 0700) and
//! multiplexes two connection kinds via a 1-byte mode tag:
//!   * `0x01` control — tarpc `PyreDaemon` (bincode, length-delimited)
//!   * `0x02` stream  — 16-byte SessionId then bidirectional
//!     length-delimited bincode `OutputFrame`/`InputFrame`

mod index;
mod parser;
mod pty;
mod ringbuf;
mod server;
mod session;
mod store;
mod stream;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use pyre_proto::service::PyreDaemon as _;
use pyre_proto::{MODE_CONTROL, MODE_STREAM};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing_subscriber::EnvFilter;

use crate::index::BlockIndex;
use crate::server::DaemonImpl;
use crate::session::SessionRegistry;
use crate::store::Store;

fn socket_path() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // Fallback: use uid-namespaced path under /tmp.
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

async fn handle_conn(
    sock: UnixStream,
    registry: Arc<SessionRegistry>,
    store: Arc<Store>,
    block_index: Arc<BlockIndex>,
) -> Result<()> {
    let mut sock = sock;
    let mut tag = [0u8; 1];
    sock.read_exact(&mut tag).await.context("read mode tag")?;

    match tag[0] {
        MODE_CONTROL => {
            let daemon = DaemonImpl {
                registry: registry.clone(),
                store: store.clone(),
                block_index: block_index.clone(),
            };
            let transport = tarpc::serde_transport::new(
                Framed::new(sock, LengthDelimitedCodec::new()),
                Bincode::default(),
            );
            BaseChannel::with_defaults(transport)
                .execute(daemon.serve())
                .for_each(|f| async move {
                    tokio::spawn(f);
                })
                .await;
            Ok(())
        }
        MODE_STREAM => stream::handle_stream(sock, registry).await,
        other => anyhow::bail!("unknown mode tag {other:#04x}"),
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = socket_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    tracing::info!("pyred listening on {}", path.display());

    let store = Arc::new(Store::open().await.context("open store")?);
    tracing::info!("store opened at {}", store.data_dir().display());

    let index_dir = store.data_dir().join("index");
    let block_index = Arc::new(
        tokio::task::spawn_blocking(move || BlockIndex::open(&index_dir))
            .await
            .context("spawn BlockIndex::open")?
            .context("open block index")?,
    );

    let registry = Arc::new(SessionRegistry::new());

    let shutdown_path = path.clone();
    let shutdown_registry = registry.clone();
    let shutdown = async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("register SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("register SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            _ = sigint.recv()  => tracing::info!("received SIGINT"),
        }
        for s in shutdown_registry.all_sessions().await {
            let panes: Vec<_> = s.panes.lock().await.values().cloned().collect();
            for p in panes {
                let _ = p.kill().await;
            }
        }
        let _ = std::fs::remove_file(&shutdown_path);
        tracing::info!("pyred shut down cleanly");
    };

    let accept_loop = async {
        loop {
            match listener.accept().await {
                Ok((sock, _addr)) => {
                    let reg = registry.clone();
                    let st = store.clone();
                    let bi = block_index.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conn(sock, reg, st, bi).await {
                            tracing::warn!("connection error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("accept error: {e}");
                    break;
                }
            }
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = shutdown => {},
    }
    Ok(())
}
