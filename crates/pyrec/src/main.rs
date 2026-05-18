//! pyrec — Pyre client. Connects to pyred over UDS, spawns a PTY session,
//! puts the local TTY in raw mode, and bridges stdio.
//!
//! Without a subcommand: interactive attach (original behaviour).
//! `list`   — list recent blocks.
//! `search` — linear-scan search across stdout blobs.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use pyre_proto::{
    BlockHit, InputFrame, ListBlocksReq, OutputFrame, PyreDaemonClient, SearchBlocksReq, SessionId,
    SpawnReq, MODE_CONTROL, MODE_STREAM,
};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing_subscriber::EnvFilter;

// ──────────────────────────────────────────────────────────────────────────────
// CLI definition
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "pyrec", version)]
struct Cli {
    /// Override socket path (applies to all subcommands)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Override shell (only used by the default interactive attach)
    #[arg(long, global = true)]
    shell: Option<String>,

    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// List recent blocks
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Linear-scan search across stdout blobs
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

fn term_size() -> (u16, u16) {
    use std::os::fd::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdin().as_raw_fd();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        (ws.ws_col, ws.ws_row.max(1))
    } else {
        (80, 24)
    }
}

/// Open a control connection and return a tarpc client.
async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    sock.write_all(&[MODE_CONTROL]).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

// ──────────────────────────────────────────────────────────────────────────────
// Raw-mode guard
// ──────────────────────────────────────────────────────────────────────────────

struct RawGuard {
    fd: std::os::fd::RawFd,
    saved: nix::sys::termios::Termios,
}

impl RawGuard {
    fn enter() -> Result<Option<Self>> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: fd is stdin (0), valid for the process lifetime.
        if unsafe { libc::isatty(fd) } == 0 {
            return Ok(None);
        }
        // SAFETY: fd == 0 (stdin), valid.
        let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let saved = tcgetattr(bfd)?;
        let mut raw = saved.clone();
        cfmakeraw(&mut raw);
        tcsetattr(bfd, SetArg::TCSANOW, &raw)?;
        Ok(Some(Self { fd, saved }))
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        // SAFETY: fd is stdin (0), valid for the process lifetime.
        let bfd = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) };
        let _ = tcsetattr(bfd, SetArg::TCSANOW, &self.saved);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Sub-command implementations
// ──────────────────────────────────────────────────────────────────────────────

async fn run_attach(socket: PathBuf, shell: Option<String>) -> Result<()> {
    let client_obj = control_client(&socket).await?;

    let (cols, rows) = term_size();
    let shell = shell.or_else(|| std::env::var("SHELL").ok()).or_else(|| {
        for candidate in ["/bin/bash", "/bin/sh"] {
            if std::path::Path::new(candidate).exists() {
                return Some(candidate.to_owned());
            }
        }
        None
    });
    let req = SpawnReq {
        shell,
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
    };
    let session: SessionId = client_obj
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    // === Stream connection ===
    let mut stream_sock = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();

    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());
    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    let _raw = RawGuard::enter()?;

    // stdin -> InputFrame -> daemon
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = InputFrame {
                        session,
                        data: Bytes::copy_from_slice(&buf[..n]),
                    };
                    if input_frames.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // daemon -> OutputFrame -> stdout
    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    if stdout.write_all(&f.data).await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                }
                Err(_) => break,
            }
        }
    });

    // ctrl-c -> kill session via control RPC
    let client_for_signal = client_obj.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = client_for_signal
                .kill(tarpc::context::current(), session)
                .await;
        }
    });

    // Exit when the PTY output stream ends (daemon closed the session).
    let _ = stdout_task.await;
    stdin_task.abort();
    signal_task.abort();

    Ok(())
}

async fn run_list(socket: PathBuf, limit: u32) -> Result<()> {
    let client = control_client(&socket).await?;
    let blocks = client
        .list_blocks(
            tarpc::context::current(),
            ListBlocksReq {
                session: None,
                limit,
            },
        )
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_blocks: {e}"))?;

    for block in &blocks {
        let short_id = &block.id.0.to_string()[..8];
        let time = block.started_at.format("%Y-%m-%d %H:%M:%S");
        let exit = block
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!("{short_id:<8}  {time}  {exit:<4}  {}", block.command);
    }

    Ok(())
}

async fn run_search(socket: PathBuf, query: String, limit: u32) -> Result<()> {
    let client = control_client(&socket).await?;
    let hits: Vec<BlockHit> = client
        .search_blocks(tarpc::context::current(), SearchBlocksReq { query, limit })
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon search_blocks: {e}"))?;

    for hit in &hits {
        let short_id = &hit.block.id.0.to_string()[..8];
        println!("{short_id:<8}  {}", hit.block.command);
        let snippet = hit.snippet.replace('\n', " ");
        println!("    {snippet}");
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Entry point
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let sock_path = cli.socket.unwrap_or_else(default_socket);

    match cli.command {
        None => run_attach(sock_path, cli.shell).await,
        Some(Sub::List { limit }) => run_list(sock_path, limit).await,
        Some(Sub::Search { query, limit }) => run_search(sock_path, query, limit).await,
    }
}
