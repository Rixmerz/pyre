//! pyrec — Pyre client. Connects to pyred over UDS, spawns a PTY session,
//! puts the local TTY in raw mode, and bridges stdio.
//!
//! Without a subcommand: spawn new session+pane, then attach.
//! `sessions`        — list active sessions.
//! `panes`           — list panes in a session.
//! `attach`          — attach to an existing session/pane.
//! `new-pane`        — open a new pane in a session (no attach).
//! `list`            — list recent blocks.
//! `search`          — linear-scan search across stdout blobs.
//! `capture-pane`    — capture last N lines of a pane's ring buffer.
//!
//! tmux-compat aliases (mapped to pyre RPCs):
//! `list-sessions`   → sessions
//! `list-panes`      → panes
//! `list-windows`    → panes (tmux windows ≈ pyre panes for MVP)
//! `new-session`     → spawn (optionally detached)
//! `kill-session`    → close_session RPC
//! `send-keys`       → write bytes to pane via stream connection
//! `split-window`    → open_pane (layout managed by TUI)
//! `select-pane`     → stub (TUI-only)
//! `display-message` → print to stderr

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use pyre_proto::{
    BlockHit, InputFrame, ListBlocksReq, OpenPaneReq, OutputFrame, PaneId, PaneStateKind,
    PyreDaemonClient, SearchBlocksReq, SessionId, SpawnReq, SpawnResp, MODE_CONTROL, MODE_STREAM,
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

    /// Capture the last N lines of a pane's ring buffer (CSI stripped)
    CapturePane {
        /// Pane id or ≥8-char prefix
        pane: String,
        /// Session id or ≥8-char prefix (required to resolve pane prefix)
        #[arg(long)]
        session: Option<String>,
        /// Number of lines to capture (default 50)
        #[arg(short = 'N', long, default_value_t = 50)]
        lines: u32,
        /// Print to stdout (default: print to stdout)
        #[arg(short, long)]
        pipe: bool,
    },

    /// Show pane state summary (colored table)
    Status {
        /// Show only panes in WaitingInput state
        #[arg(long)]
        waiting: bool,
        /// Emit JSON array of PaneInfo instead of a table
        #[arg(long)]
        json: bool,
    },

    // ── tmux-compat aliases ────────────────────────────────────────────────────
    /// [tmux compat] List active sessions
    #[command(name = "list-sessions")]
    ListSessions,

    /// [tmux compat] List panes in a session
    #[command(name = "list-panes")]
    ListPanes {
        /// Target session: id or ≥8-char prefix (-t <session>)
        #[arg(short = 't')]
        target: String,
    },

    /// [tmux compat] List windows — mapped to panes for MVP
    #[command(name = "list-windows")]
    ListWindows {
        /// Target session: id or ≥8-char prefix (-t <session>)
        #[arg(short = 't')]
        target: String,
    },

    /// [tmux compat] Create a new session (optionally detached)
    #[command(name = "new-session")]
    NewSession {
        /// Session name (informational; stored in session metadata)
        #[arg(short = 's')]
        name: Option<String>,
        /// Detach immediately after spawn (do not attach)
        #[arg(short = 'd')]
        detach: bool,
    },

    /// [tmux compat] Kill a session
    #[command(name = "kill-session")]
    KillSession {
        /// Target session: id or ≥8-char prefix (-t <session>)
        #[arg(short = 't')]
        target: String,
    },

    /// [tmux compat] Send keys to a pane
    #[command(name = "send-keys")]
    SendKeys {
        /// Target pane id or ≥8-char prefix (-t <pane>)
        #[arg(short = 't')]
        target: String,
        /// Text to send (multiple args joined by space)
        keys: Vec<String>,
        /// Append a newline (Enter) after the keys
        #[arg(long)]
        enter: bool,
    },

    /// [tmux compat] Split a pane (opens a new pane; TUI manages layout)
    #[command(name = "split-window")]
    SplitWindow {
        /// Target session: id or ≥8-char prefix (-t <session>)
        #[arg(short = 't')]
        target: String,
        /// Horizontal split (ignored for spawn, layout is TUI-managed)
        #[arg(short = 'h')]
        horizontal: bool,
        /// Vertical split (ignored for spawn, layout is TUI-managed)
        #[arg(short = 'v')]
        vertical: bool,
    },

    /// [tmux compat] Select (focus) a pane — stub; focus is TUI-managed
    #[command(name = "select-pane")]
    SelectPane {
        /// Target pane id or ≥8-char prefix (-t <pane>)
        #[arg(short = 't')]
        target: String,
    },

    /// [tmux compat] Display a message on stderr
    #[command(name = "display-message")]
    DisplayMessage {
        /// Message to print
        message: String,
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
        name: None,
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

async fn run_capture_pane(
    socket: PathBuf,
    pane_prefix: String,
    session_prefix: Option<String>,
    lines: u32,
    _pipe: bool,
) -> Result<()> {
    let client = control_client(&socket).await?;

    // Resolve the pane id. If a session prefix is given we search that session's
    // panes; otherwise we scan all sessions for a matching pane.
    let pane_id = if let Some(ref sp) = session_prefix {
        let session = resolve_session(&client, sp).await?;
        resolve_pane(&client, session, &pane_prefix).await?
    } else {
        // Enumerate all sessions and find a matching pane.
        let sessions = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc transport")?
            .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

        let mut found: Option<PaneId> = None;
        'outer: for s in sessions {
            let panes = client
                .list_panes(tarpc::context::current(), s.id)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_panes: {e}"))?;
            for p in panes {
                if p.id.0.to_string().starts_with(&pane_prefix) {
                    found = Some(p.id);
                    break 'outer;
                }
            }
        }
        found.ok_or_else(|| anyhow!("no pane matches prefix '{pane_prefix}'"))?
    };

    let bytes = client
        .capture_pane(tarpc::context::current(), pane_id, lines)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon capture_pane: {e}"))?;

    let text = String::from_utf8_lossy(&bytes);
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn run_status(socket: PathBuf, waiting: bool, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    let client = control_client(&socket).await?;
    let panes = client
        .list_all_panes(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_all_panes: {e}"))?;

    let panes: Vec<_> = if waiting {
        panes
            .into_iter()
            .filter(|p| p.state == PaneStateKind::WaitingInput)
            .collect()
    } else {
        panes
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&panes)?);
        return Ok(());
    }

    let is_tty = std::io::stdout().is_terminal();

    // Color codes (only when stdout is a TTY).
    let reset = if is_tty { "\x1b[0m" } else { "" };
    let dim = if is_tty { "\x1b[2m" } else { "" };

    fn state_color(state: PaneStateKind, is_tty: bool) -> &'static str {
        if !is_tty {
            return "";
        }
        match state {
            PaneStateKind::Running => "\x1b[32m",      // green
            PaneStateKind::WaitingInput => "\x1b[33m", // yellow
            PaneStateKind::Idle => "\x1b[2m",          // dim
            PaneStateKind::Interactive => "\x1b[36m",  // cyan
            PaneStateKind::Crashed => "\x1b[31m",      // red
            PaneStateKind::Done => "\x1b[2m",          // dim
        }
    }

    fn state_dot(state: PaneStateKind) -> char {
        match state {
            PaneStateKind::Running => '●',
            PaneStateKind::WaitingInput => '◎',
            PaneStateKind::Idle => '○',
            PaneStateKind::Interactive => '◆',
            PaneStateKind::Crashed => '✗',
            PaneStateKind::Done => '◦',
        }
    }

    // Header
    println!(
        "{dim}{:<8}  {:<8}  {:<11}  {:<14}  {:<16}  PID{reset}",
        "SESSION", "PANE", "STATE", "LAST ACTIVITY", "FOREGROUND",
    );

    for p in &panes {
        let sess_short = &p.session.0.to_string()[..8];
        let pane_short = &p.id.0.to_string()[..8];
        let state_str = format!(
            "{}{} {}{}",
            state_color(p.state, is_tty),
            state_dot(p.state),
            p.state,
            reset
        );
        let elapsed = {
            let secs = (chrono::Utc::now() - p.last_activity).num_seconds().max(0);
            if secs < 60 {
                format!("{secs}s ago")
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else {
                format!("{}h ago", secs / 3600)
            }
        };
        let fg = p.foreground_cmd.as_deref().unwrap_or("-");
        println!(
            "{sess_short}  {pane_short}  {state_str:<20}  {elapsed:<14}  {fg:<16}  {}",
            p.root_pid
        );
    }

    if panes.is_empty() {
        println!("no panes");
    }

    Ok(())
}

async fn run_kill_session(socket: PathBuf, target: String) -> Result<()> {
    let client = control_client(&socket).await?;
    let session = resolve_session(&client, &target).await?;
    client
        .close_session(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon close_session: {e}"))
}

async fn run_send_keys(
    socket: PathBuf,
    pane_prefix: String,
    keys: Vec<String>,
    append_enter: bool,
) -> Result<()> {
    let client = control_client(&socket).await?;

    // Resolve pane by scanning all sessions.
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

    let mut found_pane: Option<PaneId> = None;
    'outer: for s in sessions {
        let panes = client
            .list_panes(tarpc::context::current(), s.id)
            .await
            .context("rpc transport")?
            .map_err(|e| anyhow!("daemon list_panes: {e}"))?;
        for p in panes {
            if p.id.0.to_string().starts_with(&pane_prefix) {
                found_pane = Some(p.id);
                break 'outer;
            }
        }
    }
    let pane_id = found_pane.ok_or_else(|| anyhow!("no pane matches prefix '{pane_prefix}'"))?;

    let mut text = keys.join(" ");
    if append_enter {
        text.push('\r');
    }

    client
        .send_keys(tarpc::context::current(), pane_id, text.into_bytes())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon send_keys: {e}"))
}

async fn run_split_window(socket: PathBuf, session_prefix: String) -> Result<()> {
    let client = control_client(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let (cols, rows) = term_size();
    let req = OpenPaneReq {
        session,
        shell: std::env::var("SHELL").ok(),
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
        Some(Sub::Sessions) | Some(Sub::ListSessions) => run_sessions(sock_path).await,
        Some(Sub::Panes { session })
        | Some(Sub::ListPanes { target: session })
        | Some(Sub::ListWindows { target: session }) => run_panes(sock_path, session).await,
        Some(Sub::Attach { session, pane }) => run_attach_cmd(sock_path, session, pane).await,
        Some(Sub::NewPane {
            session,
            shell,
            cols,
            rows,
        }) => run_new_pane(sock_path, session, shell, cols, rows).await,
        Some(Sub::Status { waiting, json }) => run_status(sock_path, waiting, json).await,
        Some(Sub::List { limit }) => run_list(sock_path, limit).await,
        Some(Sub::Search { query, limit }) => run_search(sock_path, query, limit).await,
        Some(Sub::CapturePane {
            pane,
            session,
            lines,
            pipe,
        }) => run_capture_pane(sock_path, pane, session, lines, pipe).await,
        Some(Sub::NewSession { name: _, detach }) => {
            let client = control_client(&sock_path).await?;
            let (cols, rows) = term_size();
            let shell = cli
                .shell
                .or_else(|| std::env::var("SHELL").ok())
                .or_else(|| {
                    for c in ["/bin/bash", "/bin/sh"] {
                        if std::path::Path::new(c).exists() {
                            return Some(c.to_owned());
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
                name: None,
            };
            let SpawnResp { session, pane } = client
                .spawn(tarpc::context::current(), req)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon spawn: {e}"))?;
            println!("{}", session.0);
            if !detach {
                run_attach(&sock_path, session, pane).await?;
            }
            Ok(())
        }
        Some(Sub::KillSession { target }) => run_kill_session(sock_path, target).await,
        Some(Sub::SendKeys {
            target,
            keys,
            enter,
        }) => run_send_keys(sock_path, target, keys, enter).await,
        Some(Sub::SplitWindow { target, .. }) => run_split_window(sock_path, target).await,
        Some(Sub::SelectPane { target }) => {
            eprintln!("select-pane: pane focus is TUI-managed; target={target}");
            Ok(())
        }
        Some(Sub::DisplayMessage { message }) => {
            eprintln!("{message}");
            Ok(())
        }
    }
}
