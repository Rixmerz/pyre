//! pyrec — Pyre client. Connects to pyred over UDS, spawns a PTY session,
//! puts the local TTY in raw mode, and bridges stdio.
//!
//! Without a subcommand: spawn new session+pane, then attach.
//! `sessions`  — list active sessions.
//! `panes`     — list panes in a session.
//! `attach`    — attach to an existing session/pane.
//! `new-pane`  — open a new pane in a session (no attach).
//! `list`      — list recent blocks.
//! `search`    — linear-scan search across stdout blobs.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use pyre_proto::{
    BlockHit, InputFrame, ListBlocksReq, OpenPaneReq, OutputFrame, PaneId, PyreDaemonClient,
    SearchBlocksReq, SessionId, SpawnReq, SpawnResp, MODE_CONTROL, MODE_STREAM,
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
    /// List active sessions
    Sessions,

    /// List panes in a session
    Panes {
        /// Session id or ≥8-char prefix
        session: String,
    },

    /// Attach to an existing session (and optionally a specific pane)
    Attach {
        /// Session id or ≥8-char prefix
        session: String,
        /// Pane id or ≥8-char prefix (default: first pane)
        #[arg(long)]
        pane: Option<String>,
    },

    /// Open a new pane in a session and print its id (no attach)
    NewPane {
        /// Session id or ≥8-char prefix
        session: String,
        /// Shell to launch in the new pane
        #[arg(long)]
        shell: Option<String>,
        /// Terminal width
        #[arg(long, default_value_t = 80)]
        cols: u16,
        /// Terminal height
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },

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

/// Resolve a session id from a full uuid string or a ≥8-char prefix.
async fn resolve_session(client: &PyreDaemonClient, prefix: &str) -> Result<SessionId> {
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no session matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} sessions match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

/// Resolve a pane id from a full uuid string or a ≥8-char prefix within a session.
async fn resolve_pane(
    client: &PyreDaemonClient,
    session: SessionId,
    prefix: &str,
) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no pane matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} panes match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

/// Pick the first pane in a session.
async fn first_pane(client: &PyreDaemonClient, session: SessionId) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    panes
        .into_iter()
        .next()
        .map(|p| p.id)
        .ok_or_else(|| anyhow!("session has no panes"))
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

/// Open a stream connection and bridge PTY I/O between the terminal and daemon.
async fn run_attach(socket: &Path, session: SessionId, pane: PaneId) -> Result<()> {
    let mut stream_sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
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

    // daemon -> OutputFrame -> stdout (first frame is snapshot replay)
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
    let socket_clone = socket.to_owned();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            if let Ok(ctrl) = control_client(&socket_clone).await {
                let _ = ctrl.kill(tarpc::context::current(), session).await;
            }
        }
    });

    // Exit when the PTY output stream ends (daemon closed the session).
    let _ = stdout_task.await;
    stdin_task.abort();
    signal_task.abort();

    Ok(())
}

async fn run_default(socket: PathBuf, shell: Option<String>) -> Result<()> {
    let client = control_client(&socket).await?;

    let (cols, rows) = term_size();
    let shell_resolved = shell.or_else(|| std::env::var("SHELL").ok()).or_else(|| {
        for candidate in ["/bin/bash", "/bin/sh"] {
            if std::path::Path::new(candidate).exists() {
                return Some(candidate.to_owned());
            }
        }
        None
    });
    let req = SpawnReq {
        shell: shell_resolved,
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
    };
    let SpawnResp { session, pane } = client
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    run_attach(&socket, session, pane).await
}

async fn run_sessions(socket: PathBuf) -> Result<()> {
    let client = control_client(&socket).await?;
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

    for s in &sessions {
        let short_id = &s.id.0.to_string()[..8];
        let created = s.created_at.format("%Y-%m-%d %H:%M:%S");
        println!(
            "{short_id}  {:>3} panes  {created}  {}",
            s.pane_count, s.name
        );
    }

    Ok(())
}

async fn run_panes(socket: PathBuf, session_prefix: String) -> Result<()> {
    let client = control_client(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    for p in &panes {
        let short_id = &p.id.0.to_string()[..8];
        let created = p.created_at.format("%Y-%m-%d %H:%M:%S");
        let status = if p.closed_at.is_some() {
            "closed"
        } else {
            "open  "
        };
        println!(
            "{short_id}  {status}  {}x{}  {created}  {}",
            p.cols, p.rows, p.shell
        );
    }

    Ok(())
}

async fn run_attach_cmd(
    socket: PathBuf,
    session_prefix: String,
    pane_prefix: Option<String>,
) -> Result<()> {
    let client = control_client(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let pane = match pane_prefix {
        Some(ref prefix) => resolve_pane(&client, session, prefix).await?,
        None => first_pane(&client, session).await?,
    };
    run_attach(&socket, session, pane).await
}

async fn run_new_pane(
    socket: PathBuf,
    session_prefix: String,
    shell: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let client = control_client(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let req = OpenPaneReq {
        session,
        shell,
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
    };
    let pane_id = client
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;

    println!("{}", pane_id.0);
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
        None => run_default(sock_path, cli.shell).await,
        Some(Sub::Sessions) => run_sessions(sock_path).await,
        Some(Sub::Panes { session }) => run_panes(sock_path, session).await,
        Some(Sub::Attach { session, pane }) => run_attach_cmd(sock_path, session, pane).await,
        Some(Sub::NewPane {
            session,
            shell,
            cols,
            rows,
        }) => run_new_pane(sock_path, session, shell, cols, rows).await,
        Some(Sub::List { limit }) => run_list(sock_path, limit).await,
        Some(Sub::Search { query, limit }) => run_search(sock_path, query, limit).await,
    }
}
