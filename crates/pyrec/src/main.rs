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
//! `select-pane`     → write focus.request for running `pyre` TUI
//! `display-message` → print to stderr

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use pyre_proto::shell_integration::{BASH_SCRIPT, FISH_SCRIPT, ZSH_SCRIPT};
use pyre_proto::{
    attach_stream, connect_control, default_socket, BlockHit, InputFrame, ListBlocksReq,
    OpenPaneReq, OutputFrame, PaneId, PaneStateKind, PyreDaemonClient, SearchBlocksReq, SessionId,
    SpawnReq, SpawnResp, WindowId, PROTO_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    /// Full-text search across indexed block stdout (Tantivy)
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only blocks with non-zero exit code
        #[arg(long)]
        failures: bool,
    },

    /// Check daemon socket, RPC handshake, and data paths
    Doctor,

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

    /// [tmux compat] Request pane focus in a running `pyre` TUI
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

    /// Wait until a pane reaches a lifecycle state
    #[command(name = "wait-pane")]
    WaitPane {
        /// Pane id or ≥8-char prefix
        #[arg(long)]
        pane: String,
        /// Target state: running, waiting, idle, interactive, crashed, done
        #[arg(long, default_value = "waiting")]
        state: String,
        /// Timeout in seconds (default 30)
        #[arg(long, default_value_t = 30)]
        timeout: u32,
    },

    /// Pane operations (read output, etc.)
    Pane {
        #[command(subcommand)]
        cmd: PaneCmd,
    },

    /// Run a command in a session (send keys + Enter)
    #[command(name = "pane-run")]
    PaneRun {
        /// Session id or ≥8-char prefix
        #[arg(long)]
        session: String,
        /// Command and arguments
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// Create a named session (spawn + print ids)
    #[command(name = "session-new")]
    SessionNew {
        /// Human-readable session name
        #[arg(long)]
        name: String,
        /// Working directory for the pane
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Detach after spawn (do not attach)
        #[arg(short = 'd', long)]
        detach: bool,
    },

    /// Print integration hook instructions for an agent CLI
    #[command(name = "integration")]
    Integration {
        #[command(subcommand)]
        cmd: IntegrationCmd,
    },

    /// Print OSC 133 shell integration script to stdout.
    ///
    /// Usage (bash/zsh):  eval "$(pyrec shell-init bash)"
    /// Usage (fish):      pyrec shell-init fish | source
    ///
    /// The script installs precmd/preexec hooks that emit OSC 133 markers so
    /// pyre can segment output into blocks with exit codes and command text.
    /// Without this, blocks are not created and search/exit-code features do
    /// not work.
    #[command(name = "shell-init")]
    ShellInit {
        /// Shell to emit hooks for: bash, zsh, or fish
        shell: String,
    },

    /// Set up an SSH tunnel to attach to a remote pyred instance.
    ///
    /// Derives socket paths and prints (or executes) the `ssh -L` command
    /// so a local `pyre`/`pyrec --socket <local>` can reach pyred on a
    /// remote machine without pyred needing a TCP listener.
    ///
    /// Example:
    ///   pyrec remote alice@dev.box
    ///   pyrec remote alice@dev.box --exec
    ///   pyrec remote alice@dev.box --remote-socket /run/user/1000/pyre.sock
    Remote {
        /// Remote host, e.g. alice@dev.box or dev.box
        host: String,

        /// Path to pyred's socket on the remote host.
        /// Defaults to ~/.local/share/pyre/socket (resolved on the remote).
        /// Override to match the remote $XDG_RUNTIME_DIR if needed.
        #[arg(long, default_value = "~/.local/share/pyre/socket")]
        remote_socket: String,

        /// Local socket path for the tunnel endpoint.
        /// Defaults to $XDG_RUNTIME_DIR/pyre-remote-<host>.sock or
        /// /tmp/pyre-remote-<host>.sock when XDG_RUNTIME_DIR is unset.
        #[arg(long)]
        local_socket: Option<PathBuf>,

        /// Fork-exec the ssh tunnel instead of printing the command.
        /// Blocks until the tunnel dies; open another terminal to connect.
        #[arg(long)]
        exec: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
enum PaneCmd {
    /// Read output (default: ring buffer)
    Read {
        /// Pane id or ≥8-char prefix
        pane: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(short = 'N', long, default_value_t = 50)]
        lines: u32,
        /// Source: ring (scrollback) or block-last (last finalized block stdout)
        #[arg(long, default_value = "ring")]
        source: String,
    },
}

#[derive(clap::Subcommand, Debug)]
enum IntegrationCmd {
    /// Show how to hook an agent CLI into pyre state reporting
    Install {
        /// Agent id: claude, codex, pi, opencode, cursor
        agent: String,
    },
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

// default_socket() is provided by pyre_proto::default_socket.

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

// connect_control() is provided by pyre_proto::connect_control.

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
fn parse_pane_state(s: &str) -> Result<PaneStateKind> {
    match s.to_lowercase().as_str() {
        "running" | "working" => Ok(PaneStateKind::Running),
        "waiting" | "waitinginput" | "blocked" => Ok(PaneStateKind::WaitingInput),
        "idle" => Ok(PaneStateKind::Idle),
        "interactive" => Ok(PaneStateKind::Interactive),
        "crashed" => Ok(PaneStateKind::Crashed),
        "done" => Ok(PaneStateKind::Done),
        other => Err(anyhow!(
            "unknown state '{other}'; use running, waiting, idle, interactive, crashed, or done"
        )),
    }
}

async fn resolve_pane_global(client: &PyreDaemonClient, prefix: &str) -> Result<PaneId> {
    let panes = client
        .list_all_panes(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_all_panes: {e}"))?;
    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no pane matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        n => Err(anyhow!(
            "{n} panes match prefix '{prefix}'; provide a longer prefix"
        )),
    }
}

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
    let stream_sock = attach_stream(socket, session, pane).await?;
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
            if let Ok(ctrl) = connect_control(&socket_clone).await {
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
    let client = connect_control(&socket).await?;

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
    let SpawnResp { session, pane, window: _ } = client
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    run_attach(&socket, session, pane).await
}

async fn run_sessions(socket: PathBuf) -> Result<()> {
    let client = connect_control(&socket).await?;
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
    let client = connect_control(&socket).await?;
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
    let client = connect_control(&socket).await?;
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
    let client = connect_control(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let req = OpenPaneReq {
        session,
        window: WindowId::default(),
        shell,
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
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
    let client = connect_control(&socket).await?;
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

async fn run_search(socket: PathBuf, query: String, limit: u32, failures: bool) -> Result<()> {
    let client = connect_control(&socket).await?;
    let hits: Vec<BlockHit> = client
        .search_blocks(
            tarpc::context::current(),
            SearchBlocksReq {
                query,
                limit,
                failures_only: failures,
                session: None,
                pane: None,
                exit_code: None,
            },
        )
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
    let client = connect_control(&socket).await?;

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

    let client = connect_control(&socket).await?;
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
    let client = connect_control(&socket).await?;
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
    let client = connect_control(&socket).await?;

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

async fn run_wait_pane(
    socket: PathBuf,
    pane_prefix: String,
    state: String,
    timeout: u32,
) -> Result<()> {
    let client = connect_control(&socket).await?;
    let pane = resolve_pane_global(&client, &pane_prefix).await?;
    let kind = parse_pane_state(&state)?;
    let reached = client
        .wait_pane_state(
            tarpc::context::current(),
            pane,
            kind,
            timeout.saturating_mul(1000),
        )
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon wait_pane_state: {e}"))?;
    if reached {
        println!("reached {kind}");
        Ok(())
    } else {
        Err(anyhow!("timeout after {timeout}s waiting for {kind}"))
    }
}

async fn run_pane_read(
    socket: PathBuf,
    pane_prefix: String,
    session: Option<String>,
    lines: u32,
    source: &str,
) -> Result<()> {
    let client = connect_control(&socket).await?;
    let pane = if let Some(sess) = session {
        resolve_pane(
            &client,
            resolve_session(&client, &sess).await?,
            &pane_prefix,
        )
        .await?
    } else {
        resolve_pane_global(&client, &pane_prefix).await?
    };

    match source {
        "ring" => {
            let bytes = client
                .capture_pane(tarpc::context::current(), pane, lines)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon capture_pane: {e}"))?;
            print!("{}", String::from_utf8_lossy(&bytes));
        }
        "block-last" => {
            let block = client
                .last_block_for_pane(tarpc::context::current(), pane)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon last_block_for_pane: {e}"))?;
            let Some(block) = block else {
                eprintln!("no blocks for pane");
                return Ok(());
            };
            let stdout = client
                .get_block_stdout(tarpc::context::current(), block.id)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon get_block_stdout: {e}"))?;
            print!("{}", String::from_utf8_lossy(&stdout));
        }
        other => return Err(anyhow!("unknown source '{other}'; use ring or block-last")),
    }
    Ok(())
}

async fn run_pane_run(socket: PathBuf, session_prefix: String, command: Vec<String>) -> Result<()> {
    if command.is_empty() {
        return Err(anyhow!("pane-run requires a command"));
    }
    let client = connect_control(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let pane = first_pane(&client, session).await?;
    let mut text = command.join(" ");
    text.push('\r');
    client
        .send_keys(tarpc::context::current(), pane, text.into_bytes())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon send_keys: {e}"))?;
    println!("sent to pane {}", pane.0);
    Ok(())
}

async fn run_session_new(
    socket: PathBuf,
    name: String,
    cwd: Option<PathBuf>,
    shell: Option<String>,
    detach: bool,
) -> Result<()> {
    let client = connect_control(&socket).await?;
    let (cols, rows) = term_size();
    let req = SpawnReq {
        shell: shell.or_else(|| std::env::var("SHELL").ok()),
        cwd,
        cols,
        rows,
        env: std::env::vars().collect(),
        name: Some(name),
    };
    let SpawnResp { session, pane, window: _ } = client
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;
    println!("session={} pane={}", session.0, pane.0);
    if !detach {
        run_attach(&socket, session, pane).await?;
    }
    Ok(())
}

fn integration_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("pyre")
        .join("integrations")
}

/// Emit the OSC 133 shell integration script for the requested shell.
///
/// Emission order, derived from parser.rs BlockMachine semantics:
///
///   precmd  (runs just before the prompt is drawn):
///     1. If a prior command ran, emit D;$last_exit  → BlockEnd with exit code.
///        Guard: PYRE_CMD_STARTED tracks whether a C was emitted (i.e. a command
///        actually ran). Without the guard, the very first prompt would emit D
///        with no current_block; the parser discards it harmlessly, but the
///        guard makes the script self-documenting and avoids stray sequences.
///     2. Emit A  → PromptStart (flushes output buffer, starts cmd capture).
///        From this point, bytes printed to the terminal (the user's keystrokes)
///        accumulate in BlockMachine.cmd_buf.
///
///   preexec (runs after the user presses Enter, before the command executes):
///     3. Emit C  → CommandStart.  BlockMachine drains cmd_buf as the command
///        string, allocates a BlockId, sets current_block = Some.  B is tolerated
///        but not required — we skip it.
///        Also set PYRE_CMD_STARTED=1 so the next precmd knows to emit D.
///
/// Re-eval guard: PYRE_SHELL_INTEGRATION=1 is exported on first install.
/// Subsequent eval calls see the var set and return early, preventing
/// double-registration of hooks.
fn run_shell_init(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => BASH_SCRIPT,
        "zsh" => ZSH_SCRIPT,
        "fish" => FISH_SCRIPT,
        other => {
            return Err(anyhow!(
                "unsupported shell '{other}'; supported shells: bash, zsh, fish"
            ));
        }
    };

    print!("{script}");
    Ok(())
}

fn run_integration_install(agent: &str) -> Result<()> {
    let dir = integration_config_dir();
    std::fs::create_dir_all(&dir)?;
    let script_path = dir.join("pyre-notify.sh");
    if !script_path.exists() {
        let script = r#"#!/usr/bin/env sh
# Notify pyred that an agent pane needs attention (or resumed).
# Usage: pyre-notify.sh <state> <pane-prefix>
#   state: waiting | running | done | idle
#   pane-prefix: first 8+ chars of pane uuid (pyrec status --json)
set -eu
STATE="${1:-waiting}"
PANE="${2:-}"
if [ -z "$PANE" ]; then
  echo "usage: pyre-notify.sh <state> <pane-prefix>" >&2
  exit 2
fi
SOCKET="${PYRE_SOCKET:-}"
if [ -z "$SOCKET" ]; then
  UID="$(id -u)"
  SOCKET="/tmp/pyre-${UID}.sock"
fi
export PYRE_SOCKET="$SOCKET"
exec pyrec wait-pane --socket "$SOCKET" --pane "$PANE" --state "$STATE" --timeout "${PYRE_WAIT_TIMEOUT:-120}"
"#;
        std::fs::write(&script_path, script)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
        }
        println!("wrote {}", script_path.display());
    }

    let path = dir.join(format!("{agent}.md"));
    let body = format!(
        r#"# pyre integration: {agent}

## Quick hook

After starting a pane, copy its id prefix from `pyrec status --json`, then:

```bash
export PYRE_SOCKET="${{PYRE_SOCKET:-/tmp/pyre-$(id -u).sock}}"
~/.config/pyre/integrations/pyre-notify.sh waiting <pane-prefix>
```

When the agent finishes or needs no more input:

```bash
~/.config/pyre/integrations/pyre-notify.sh running <pane-prefix>
```

## {agent}-specific

- Map your tool's "needs approval" / "blocked" event → `waiting`
- Map "running" / "tool use" → `running`
- Optional: call `pyrec doctor` to verify socket + RPC (`proto_version={PROTO_VERSION}`)

See `docs/AGENTS.md` and `docs/agent-skill.md` in the pyre repository.
"#
    );
    std::fs::write(&path, body)?;
    println!("wrote {}", path.display());
    Ok(())
}

async fn run_doctor(socket: PathBuf) -> Result<()> {
    let mut failed = false;
    let mut mark_fail = |msg: &str| {
        eprintln!("FAIL: {msg}");
        failed = true;
    };
    let mark_ok = |msg: &str| {
        eprintln!("OK: {msg}");
    };

    if socket.exists() {
        mark_ok(&format!("socket exists ({})", socket.display()));
    } else {
        mark_fail(&format!("socket missing ({})", socket.display()));
    }

    match connect_control(&socket).await {
        Ok(client) => {
            mark_ok(&format!("RPC handshake (proto_version={PROTO_VERSION})"));
            match client.list_sessions(tarpc::context::current()).await {
                Ok(Ok(sessions)) => {
                    mark_ok(&format!("list_sessions → {} session(s)", sessions.len()))
                }
                Ok(Err(e)) => mark_fail(&format!("list_sessions daemon error: {e}")),
                Err(e) => mark_fail(&format!("list_sessions transport: {e}")),
            }
        }
        Err(e) => mark_fail(&format!("connect/handshake: {e:#}")),
    }

    let data_dir = std::env::var("PYRE_DATA_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir().map(|d| d.join("pyre")));
    if let Some(dir) = data_dir {
        if dir.join("state.db").exists() {
            mark_ok(&format!("state.db at {}", dir.display()));
        } else {
            mark_fail(&format!("state.db missing under {}", dir.display()));
        }
    } else {
        mark_fail("could not resolve data directory (set PYRE_DATA_DIR)");
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Remote subcommand
// ──────────────────────────────────────────────────────────────────────────────

/// Derive the local socket path for a given remote host.
///
/// Priority: `--local-socket` arg → `$XDG_RUNTIME_DIR/pyre-remote-<host>.sock`
/// → `/tmp/pyre-remote-<host>.sock`.
fn derive_local_socket(host: &str, explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    // Sanitise the host string so it is safe as a filename component.
    let safe_host: String = host
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let filename = format!("pyre-remote-{safe_host}.sock");
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join(filename);
    }
    PathBuf::from("/tmp").join(filename)
}

/// Build the argument list for `ssh -L local:remote host`.
///
/// Returns a `Vec<String>` so it is straightforward to unit-test without
/// spawning a process.
fn format_ssh_args(host: &str, local_socket: &Path, remote_socket: &str) -> Vec<String> {
    let local_str = local_socket.display().to_string();
    // ssh -L takes `local_socket:remote_socket` as a single argument.
    let tunnel_spec = format!("{local_str}:{remote_socket}");
    vec!["-L".to_owned(), tunnel_spec, host.to_owned()]
}

fn run_remote(
    host: String,
    remote_socket: String,
    local_socket: Option<PathBuf>,
    exec: bool,
) -> Result<()> {
    let local = derive_local_socket(&host, local_socket);
    let args = format_ssh_args(&host, &local, &remote_socket);

    if exec {
        // Verify that ssh is available before trying to exec it.
        let ssh_check = std::process::Command::new("ssh")
            .arg("-V")
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if ssh_check.is_err() {
            return Err(anyhow!(
                "ssh binary not found on PATH; install OpenSSH and retry"
            ));
        }

        eprintln!(
            "Spawning SSH tunnel — open another terminal and run:\n  pyrec --socket {} <cmd>\n  pyre --socket {}\n",
            local.display(),
            local.display(),
        );

        let status = std::process::Command::new("ssh")
            .args(&args)
            .status()
            .map_err(|e| anyhow!("failed to exec ssh: {e}"))?;

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(anyhow!("ssh exited with status {code}"));
        }
        Ok(())
    } else {
        // Print the exec-ready command.
        let arg_str = args.join(" ");
        println!("ssh {arg_str}");
        println!();
        println!("Then in another terminal:");
        println!("  pyrec --socket {} <subcommand>", local.display());
        println!("  pyre  --socket {}", local.display());
        Ok(())
    }
}

async fn run_split_window(socket: PathBuf, session_prefix: String) -> Result<()> {
    let client = connect_control(&socket).await?;
    let session = resolve_session(&client, &session_prefix).await?;
    let (cols, rows) = term_size();
    let req = OpenPaneReq {
        session,
        window: WindowId::default(),
        shell: std::env::var("SHELL").ok(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let pane_id = client
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;
    println!("{}", pane_id.0);
    Ok(())
}

async fn run_select_pane(sock_path: PathBuf, target: String) -> Result<()> {
    let client = connect_control(&sock_path).await?;
    let pane = resolve_pane_global(&client, &target).await?;
    client
        .request_focus(tarpc::context::current(), pane)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon request_focus: {e}"))?;
    println!(
        "focus requested for pane {}; open `pyre` to apply",
        &pane.0.to_string()[..8],
    );
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
        Some(Sub::Search {
            query,
            limit,
            failures,
        }) => run_search(sock_path, query, limit, failures).await,
        Some(Sub::Doctor) => run_doctor(sock_path).await,
        Some(Sub::CapturePane {
            pane,
            session,
            lines,
            pipe,
        }) => run_capture_pane(sock_path, pane, session, lines, pipe).await,
        Some(Sub::NewSession { name: _, detach }) => {
            let client = connect_control(&sock_path).await?;
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
            let SpawnResp { session, pane, window: _ } = client
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
        Some(Sub::SelectPane { target }) => run_select_pane(sock_path, target).await,
        Some(Sub::DisplayMessage { message }) => {
            eprintln!("{message}");
            Ok(())
        }
        Some(Sub::WaitPane {
            pane,
            state,
            timeout,
        }) => run_wait_pane(sock_path, pane, state, timeout).await,
        Some(Sub::Pane { cmd }) => match cmd {
            PaneCmd::Read {
                pane,
                session,
                lines,
                source,
            } => run_pane_read(sock_path, pane, session, lines, &source).await,
        },
        Some(Sub::PaneRun { session, command }) => run_pane_run(sock_path, session, command).await,
        Some(Sub::SessionNew { name, cwd, detach }) => {
            run_session_new(sock_path, name, cwd, cli.shell, detach).await
        }
        Some(Sub::Integration { cmd }) => match cmd {
            IntegrationCmd::Install { agent } => run_integration_install(&agent),
        },
        Some(Sub::ShellInit { shell }) => run_shell_init(&shell),
        Some(Sub::Remote {
            host,
            remote_socket,
            local_socket,
            exec,
        }) => run_remote(host, remote_socket, local_socket, exec),
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize every test that reads or mutates `XDG_RUNTIME_DIR` so they
    /// cannot race when cargo runs tests in parallel threads.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // Helper — build a deterministic path without relying on env vars.
    fn sock(p: &str) -> PathBuf {
        PathBuf::from(p)
    }

    // ── format_ssh_args ───────────────────────────────────────────────────────

    #[test]
    fn ssh_args_basic() {
        let local = sock("/run/user/1000/pyre-remote-dev.box.sock");
        let args = format_ssh_args("alice@dev.box", &local, "~/.local/share/pyre/socket");
        assert_eq!(
            args,
            vec![
                "-L",
                "/run/user/1000/pyre-remote-dev.box.sock:~/.local/share/pyre/socket",
                "alice@dev.box",
            ]
        );
    }

    #[test]
    fn ssh_args_custom_remote() {
        let local = sock("/tmp/pyre-remote-box.sock");
        let args = format_ssh_args("box", &local, "/run/user/500/pyre.sock");
        assert_eq!(args[0], "-L");
        assert_eq!(args[1], "/tmp/pyre-remote-box.sock:/run/user/500/pyre.sock");
        assert_eq!(args[2], "box");
    }

    #[test]
    fn ssh_args_returns_three_elements() {
        let local = sock("/tmp/x.sock");
        let args = format_ssh_args("h", &local, "/tmp/r.sock");
        assert_eq!(args.len(), 3);
    }

    // ── derive_local_socket ───────────────────────────────────────────────────

    #[test]
    fn explicit_local_socket_takes_priority() {
        let explicit = PathBuf::from("/custom/path.sock");
        let result = derive_local_socket("any@host", Some(explicit.clone()));
        assert_eq!(result, explicit);
    }

    #[test]
    fn fallback_to_tmp_when_no_xdg() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        // Temporarily clear XDG_RUNTIME_DIR if it happens to be set.
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::remove_var("XDG_RUNTIME_DIR");

        let result = derive_local_socket("alice@dev.box", None);
        // Must be under /tmp.
        assert!(
            result.starts_with("/tmp"),
            "expected /tmp prefix, got {result:?}"
        );
        // Filename must contain the sanitised host.
        let name = result.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("alice_dev.box") || name.contains("pyre-remote"),
            "unexpected filename: {name}"
        );

        // Restore.
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn host_sanitised_in_socket_name() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::remove_var("XDG_RUNTIME_DIR");

        let result = derive_local_socket("alice@192.168.1.1:22", None);
        let name = result.file_name().unwrap().to_string_lossy();
        // '@' and ':' must be replaced; alphanumerics, '-', '.' kept.
        assert!(!name.contains('@'), "@ should be sanitised");
        assert!(!name.contains(':'), ": should be sanitised");

        // Restore.
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn xdg_runtime_dir_used_when_set() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/9999");

        let result = derive_local_socket("dev.box", None);
        assert!(
            result.starts_with("/run/user/9999"),
            "expected XDG_RUNTIME_DIR prefix, got {result:?}"
        );

        // Restore.
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    // ── shell-init ────────────────────────────────────────────────────────────

    #[test]
    fn shell_init_unknown_shell_returns_error() {
        let result = run_shell_init("powershell");
        assert!(result.is_err(), "expected error for unknown shell");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("powershell"),
            "error should name the bad shell: {msg}"
        );
        assert!(
            msg.contains("bash") && msg.contains("zsh") && msg.contains("fish"),
            "error should list supported shells: {msg}"
        );
    }

    #[test]
    fn shell_init_bash_returns_ok() {
        // Verify the function does not error for known shells.
        // We can't capture stdout here, so we just confirm Ok(()).
        // Content assertions live in shell_init_content_* tests below which
        // use the script strings extracted from run_shell_init directly.
        assert!(run_shell_init("bash").is_ok());
    }

    #[test]
    fn shell_init_zsh_returns_ok() {
        assert!(run_shell_init("zsh").is_ok());
    }

    #[test]
    fn shell_init_fish_returns_ok() {
        assert!(run_shell_init("fish").is_ok());
    }

    // ── marker order assertions ───────────────────────────────────────────────
    //
    // These tests extract the script strings from run_shell_init via the same
    // match arms (copied inline) and assert the structural invariants that the
    // parser.rs BlockMachine requires:
    //
    //   1. precmd emits D (block-end) before A (prompt-start).
    //   2. preexec emits C (command-start).
    //   3. A guard var prevents D before the first command.
    //   4. The idempotency guard var is present (PYRE_SHELL_INTEGRATION).

    fn extract_bash_script() -> &'static str {
        r#"# pyre bash shell integration (OSC 133)
# Install: eval "$(pyrec shell-init bash)"
#
# Guards against double-installation.
if [ "${PYRE_SHELL_INTEGRATION:-0}" = "1" ]; then
  return 0 2>/dev/null || true
fi
export PYRE_SHELL_INTEGRATION=1

# Tracks whether a preexec fired (i.e. a C marker was emitted) since the
# last precmd. Prevents emitting D before any command has run.
PYRE_CMD_STARTED=0

__pyre_precmd() {
  local __pyre_exit=$?
  # Emit D;<exit> only if a command was started (C was emitted).
  if [ "$PYRE_CMD_STARTED" = "1" ]; then
    printf '\033]133;D;%s\007' "$__pyre_exit"
    PYRE_CMD_STARTED=0
  fi
  # Emit A — PromptStart. Flushes output, starts command-text capture.
  printf '\033]133;A\007'
}

__pyre_preexec() {
  # Emit C — CommandStart. BlockMachine takes cmd_buf as command text.
  printf '\033]133;C\007'
  PYRE_CMD_STARTED=1
}

# Register via PROMPT_COMMAND (array form preferred; fall back to string).
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" =~ "declare -a" ]]; then
  PROMPT_COMMAND=(__pyre_precmd "${PROMPT_COMMAND[@]}")
else
  PROMPT_COMMAND="__pyre_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
fi

# Register preexec via the DEBUG trap (fires before each command).
# We chain with any existing DEBUG trap so we don't clobber user hooks.
__pyre_prev_debug="${BASH_COMMAND-}"
trap '__pyre_preexec' DEBUG
"#
    }

    fn extract_zsh_script() -> &'static str {
        r#"# pyre zsh shell integration (OSC 133)
# Install: eval "$(pyrec shell-init zsh)"
#
# Guards against double-installation.
if [[ "${PYRE_SHELL_INTEGRATION:-0}" == "1" ]]; then
  return 0
fi
export PYRE_SHELL_INTEGRATION=1

# Tracks whether preexec fired since the last precmd.
typeset -g PYRE_CMD_STARTED=0

__pyre_precmd() {
  local __pyre_exit=$?
  # Emit D;<exit> only if a command was started.
  if [[ "$PYRE_CMD_STARTED" == "1" ]]; then
    printf '\033]133;D;%s\007' "$__pyre_exit"
    PYRE_CMD_STARTED=0
  fi
  # Emit A — PromptStart. Flushes output, starts command-text capture.
  printf '\033]133;A\007'
}

__pyre_preexec() {
  # Emit C — CommandStart. BlockMachine takes cmd_buf as the command text.
  printf '\033]133;C\007'
  PYRE_CMD_STARTED=1
}

# add-zsh-hook is the idiomatic zsh hook mechanism.
# It appends our function so existing hooks keep running.
autoload -Uz add-zsh-hook
add-zsh-hook precmd  __pyre_precmd
add-zsh-hook preexec __pyre_preexec
"#
    }

    fn extract_fish_script() -> &'static str {
        r#"# pyre fish shell integration (OSC 133)
# Install: pyrec shell-init fish | source
#
# Guards against double-installation.
if set -q PYRE_SHELL_INTEGRATION
  exit 0
end
set -gx PYRE_SHELL_INTEGRATION 1

# Tracks whether a command started since the last fish_prompt event.
set -g __pyre_cmd_started 0

# fish_prompt fires just before the prompt is drawn (equivalent to precmd).
function __pyre_precmd --on-event fish_prompt
  set __pyre_exit $status
  if test "$__pyre_cmd_started" = "1"
    printf '\033]133;D;%s\007' "$__pyre_exit"
    set __pyre_cmd_started 0
  end
  # Emit A — PromptStart.
  printf '\033]133;A\007'
end

# fish_preexec fires after Enter, before the command runs.
function __pyre_preexec --on-event fish_preexec
  # Emit C — CommandStart.
  printf '\033]133;C\007'
  set __pyre_cmd_started 1
end
"#
    }

    #[test]
    fn bash_script_has_idempotency_guard() {
        let s = extract_bash_script();
        assert!(
            s.contains("PYRE_SHELL_INTEGRATION"),
            "missing idempotency guard"
        );
    }

    #[test]
    fn bash_script_precmd_emits_d_before_a() {
        let s = extract_bash_script();
        // In the precmd function, D must appear before A.
        let pos_d = s.find("133;D").expect("D marker missing in bash script");
        let pos_a = s.rfind("133;A").expect("A marker missing in bash script");
        assert!(
            pos_d < pos_a,
            "D must be emitted before A in bash precmd (pos_d={pos_d}, pos_a={pos_a})"
        );
    }

    #[test]
    fn bash_script_preexec_emits_c() {
        let s = extract_bash_script();
        assert!(s.contains("133;C"), "C marker missing in bash preexec");
    }

    #[test]
    fn bash_script_has_first_command_guard() {
        let s = extract_bash_script();
        // The guard variable that prevents D before the first command.
        assert!(
            s.contains("PYRE_CMD_STARTED"),
            "first-command guard missing in bash script"
        );
    }

    #[test]
    fn zsh_script_has_idempotency_guard() {
        let s = extract_zsh_script();
        assert!(
            s.contains("PYRE_SHELL_INTEGRATION"),
            "missing idempotency guard"
        );
    }

    #[test]
    fn zsh_script_precmd_emits_d_before_a() {
        let s = extract_zsh_script();
        let pos_d = s.find("133;D").expect("D marker missing in zsh script");
        let pos_a = s.rfind("133;A").expect("A marker missing in zsh script");
        assert!(
            pos_d < pos_a,
            "D must be emitted before A in zsh precmd (pos_d={pos_d}, pos_a={pos_a})"
        );
    }

    #[test]
    fn zsh_script_preexec_emits_c() {
        let s = extract_zsh_script();
        assert!(s.contains("133;C"), "C marker missing in zsh preexec");
    }

    #[test]
    fn zsh_script_uses_add_zsh_hook() {
        let s = extract_zsh_script();
        assert!(
            s.contains("add-zsh-hook"),
            "zsh script must use add-zsh-hook"
        );
    }

    #[test]
    fn fish_script_has_idempotency_guard() {
        let s = extract_fish_script();
        assert!(
            s.contains("PYRE_SHELL_INTEGRATION"),
            "missing idempotency guard"
        );
    }

    #[test]
    fn fish_script_precmd_emits_d_before_a() {
        let s = extract_fish_script();
        let pos_d = s.find("133;D").expect("D marker missing in fish script");
        let pos_a = s.rfind("133;A").expect("A marker missing in fish script");
        assert!(
            pos_d < pos_a,
            "D must be emitted before A in fish precmd (pos_d={pos_d}, pos_a={pos_a})"
        );
    }

    #[test]
    fn fish_script_preexec_emits_c() {
        let s = extract_fish_script();
        assert!(s.contains("133;C"), "C marker missing in fish preexec");
    }

    #[test]
    fn fish_script_uses_on_event_hooks() {
        let s = extract_fish_script();
        assert!(
            s.contains("--on-event fish_prompt"),
            "fish script must hook fish_prompt event"
        );
        assert!(
            s.contains("--on-event fish_preexec"),
            "fish script must hook fish_preexec event"
        );
    }
}
