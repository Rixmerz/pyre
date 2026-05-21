//! pyre-tui — ratatui-based terminal UI for pyre.
//!
//! Theme: Ember palette (see `theme.rs`).
//!
//! Without a subcommand: spawn a new session+pane, then attach (full-screen).
//! `attach <session> [--pane <id>]`: attach to an existing session/pane.
//!
//! Key bindings (Ctrl-B prefix):
//!   Ctrl-B c  — new tab (opens a new pane)
//!   Ctrl-B n  — next tab
//!   Ctrl-B p  — previous tab
//!   Ctrl-B "  — horizontal split (HSplit active leaf)
//!   Ctrl-B %  — vertical split (VSplit active leaf)
//!   Ctrl-B ←/→/↑/↓ — cycle focus between panes (DFS order)
//!   Ctrl-B x  — close focused pane (removes dead or live pane from layout)
//!   Ctrl-B q  — quit
//!   Ctrl-B [  — enter block ribbon scrollback for focused pane
//!   Ctrl-B ]  — exit block ribbon scrollback
//!   Ctrl-B /  — open search overlay (Tantivy full-text search)
//!   In scrollback mode: Left/h = prev block, Right/l = next block, Enter/Esc = exit
//!   Search overlay: type to query, Up/Ctrl-P / Down/Ctrl-N to navigate, Enter to jump, Esc to close
//!   Mouse scroll: ScrollUp/ScrollDown over a pane to scroll its scrollback buffer
//!   Mouse click: left-click tab strip to switch tabs; left-click pane to focus it
//!   PgUp/PgDn: scroll focused pane's scrollback buffer (when NOT in prefix / search)
//!   All other keys forwarded to the focused PTY.

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::SinkExt;
use futures::StreamExt;
use pyre_proto::{
    blocks::{BlockHit, SearchBlocksReq},
    write_control_client, Block, InputFrame, OpenPaneReq, OutputFrame, PaneId, PidInspect,
    PyreDaemonClient, SessionId, SpawnReq, SpawnResp, MODE_STREAM,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as RatatuiBlock, BorderType, Borders, Clear, List, ListItem, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};
use ratatui::Terminal;
mod clipboard;
mod fire_motion;
mod splash;
mod theme;
use fire_motion::AnimClock;
use std::collections::HashMap;
use std::process::Stdio;
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use theme::EMBER;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio::sync::{mpsc, watch};
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::{cell::Flags as CellFlags, Config as TermConfig};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor as AnsiProcessor};
use alacritty_terminal::Term;

/// Minimal Dimensions impl for creating/resizing an alacritty Term.
struct TermSize {
    cols: usize,
    rows: usize,
}

impl TermSize {
    fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "pyre", version, about = "Pyre TUI — ratatui terminal frontend")]
struct Cli {
    /// Override socket path
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Shell to use when spawning (default: $SHELL)
    #[arg(long, global = true)]
    shell: Option<String>,

    /// Skip the startup flame animation
    #[arg(long, global = true)]
    no_splash: bool,

    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// Attach to an existing session (and optionally a specific pane)
    Attach {
        /// Session id or ≥8-char prefix
        session: String,
        /// Pane id or ≥8-char prefix (default: first pane)
        #[arg(long)]
        pane: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Compute the inner PTY content area for a single full-screen pane given
/// the raw terminal dimensions.
///
/// The ratatui layout removes:
///   - 1 row  sessions strip
///   - 1 row  tabs strip
///   - 1 row  status bar
///   - 2 rows pane border (top + bottom)
///   - 1 row  ribbon strip inside the pane
///   - 2 cols pane border (left + right)
///
/// Total overhead: 6 rows, 2 cols. For splits the first-frame resize RPC
/// corrects the exact size immediately; this gets the initial spawn close
/// enough to avoid shell / fastfetch layout corruption on startup.
fn compute_pane_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    let cols = term_cols.saturating_sub(2).max(1);
    let rows = term_rows.saturating_sub(6).max(1);
    (cols, rows)
}

fn resolve_shell(shell_arg: Option<String>) -> Option<String> {
    shell_arg
        .or_else(|| std::env::var("SHELL").ok())
        .or_else(|| {
            for candidate in ["/bin/bash", "/bin/sh"] {
                if std::path::Path::new(candidate).exists() {
                    return Some(candidate.to_owned());
                }
            }
            None
        })
}

async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    // Try connecting first. On ENOENT/ECONNREFUSED, spawn pyred and retry.
    if let Ok(sock) = try_connect_control(socket).await {
        return Ok(sock);
    }

    // Daemon not running — spawn it.
    let pyred_bin = std::env::var("PYRED_BIN").ok().unwrap_or_else(|| {
        // Sibling of current_exe() (e.g. target/release/pyred next to target/release/pyre-tui).
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.parent().map(|p| p.join("pyred"));
            if let Some(path) = sibling {
                if path.exists() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
        "pyred".to_owned()
    });

    tracing::info!(
        "pyred not reachable at {}; spawning {}",
        socket.display(),
        pyred_bin
    );
    let mut child = TokioCommand::new(&pyred_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Inherit stderr so startup errors from pyred are visible in the
        // user's terminal (e.g. Tantivy lock contention, bind failures).
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn pyred binary '{pyred_bin}'"))?;

    // Poll every 100 ms for up to 5 s (50 attempts).
    // If the child exits before the socket becomes ready, surface its exit
    // status immediately rather than waiting out the full timeout.
    let mut last_err = anyhow!("daemon did not come up after 5 s");
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check whether the child already exited (e.g. crashed on lock).
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(anyhow!(
                    "spawned pyred exited immediately with status {status}; \
                     check stderr for details (Tantivy lock? stale socket?)"
                ));
            }
            Ok(None) => {} // still running — keep polling
            Err(e) => tracing::warn!("try_wait on pyred child: {e}"),
        }

        match try_connect_control(socket).await {
            Ok(client) => return Ok(client),
            Err(e) => last_err = e,
        }
    }
    Err(last_err).with_context(|| {
        format!(
            "spawned pyred but socket {} never became ready; check pyred logs",
            socket.display()
        )
    })
}

/// Single non-retrying connect attempt; wraps the mode-byte handshake.
async fn try_connect_control(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    write_control_client(&mut sock).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

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

// ─────────────────────────────────────────────────────────────────────────────
// Terminal restore guard
// ─────────────────────────────────────────────────────────────────────────────

struct TermGuard;

impl TermGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Key serialization
// ─────────────────────────────────────────────────────────────────────────────

fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Bytes> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let byte = (c.to_ascii_lowercase() as u8) & 0x1f;
            return Some(Bytes::copy_from_slice(&[byte]));
        }
    }

    let bytes: &[u8] = match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            return Some(Bytes::copy_from_slice(s.as_bytes()));
        }
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        _ => return None,
    };
    Some(Bytes::copy_from_slice(bytes))
}

// ─────────────────────────────────────────────────────────────────────────────
// Color helper
// ─────────────────────────────────────────────────────────────────────────────

/// Convert an alacritty/vte AnsiColor to a ratatui Color.
/// Returns None for "default" colors so ratatui uses its own defaults.
fn ansi_color(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Named(nc) => match nc {
            NamedColor::Black => Some(Color::Black),
            NamedColor::Red => Some(Color::Red),
            NamedColor::Green => Some(Color::Green),
            NamedColor::Yellow => Some(Color::Yellow),
            NamedColor::Blue => Some(Color::Blue),
            NamedColor::Magenta => Some(Color::Magenta),
            NamedColor::Cyan => Some(Color::Cyan),
            NamedColor::White => Some(Color::Gray),
            NamedColor::BrightBlack => Some(Color::DarkGray),
            NamedColor::BrightRed => Some(Color::LightRed),
            NamedColor::BrightGreen => Some(Color::LightGreen),
            NamedColor::BrightYellow => Some(Color::LightYellow),
            NamedColor::BrightBlue => Some(Color::LightBlue),
            NamedColor::BrightMagenta => Some(Color::LightMagenta),
            NamedColor::BrightCyan => Some(Color::LightCyan),
            NamedColor::BrightWhite => Some(Color::White),
            // Foreground/Background are "default" — let ratatui use terminal defaults.
            NamedColor::Foreground | NamedColor::Background => None,
            // Dim variants: map to corresponding base color.
            NamedColor::DimBlack => Some(Color::Black),
            NamedColor::DimRed => Some(Color::Red),
            NamedColor::DimGreen => Some(Color::Green),
            NamedColor::DimYellow => Some(Color::Yellow),
            NamedColor::DimBlue => Some(Color::Blue),
            NamedColor::DimMagenta => Some(Color::Magenta),
            NamedColor::DimCyan => Some(Color::Cyan),
            NamedColor::DimWhite => Some(Color::Gray),
            // Cursor/DimForeground/etc — treat as default.
            _ => None,
        },
        AnsiColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Indexed(i) => Some(Color::Indexed(i)),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EventProxy — forwards PtyWrite responses from Term back to the PTY input.
// This is critical for DSR/CPR (cursor position reports) so TUIs that issue
// ?6n or ?1000h don't hang waiting for a reply.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct EventProxy {
    /// Queued PtyWrite responses; drained by PaneSlot::drain_pty_responses.
    queue: Arc<Mutex<Vec<String>>>,
}

impl EventProxy {
    fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drain any accumulated response bytes into `dest`. Call this after
    /// `process_output` and send the collected bytes back to the daemon input
    /// channel so child programs receive their CPR / DSR replies.
    fn drain(&self) -> Vec<u8> {
        let mut q = self.queue.lock().expect("event proxy lock");
        let mut out: Vec<u8> = Vec::new();
        for s in q.drain(..) {
            out.extend_from_slice(s.as_bytes());
        }
        out
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        if let TermEvent::PtyWrite(s) = event {
            self.queue.lock().expect("event proxy lock").push(s);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-pane layout data model
// ─────────────────────────────────────────────────────────────────────────────

/// Events flowing from the net→UI background task to the main loop.
enum PaneEvent {
    Output(Bytes),
    /// Stream ended. `frames_received` is the total number of `OutputFrame`
    /// messages successfully decoded before the stream closed. A value of 0
    /// means the connection was rejected at the handshake level (e.g. worker
    /// returned "pane not found") rather than a real pane exit; in that case
    /// the TUI should skip the `close_pane` RPC to avoid a respawn loop.
    Closed {
        frames_received: u64,
    },
}

/// One attached PTY pane with its I/O channels and VT parser.
struct PaneSlot {
    pane_id: PaneId,
    /// alacritty_terminal state machine — handles alt-screen, DSR/CPR, mouse.
    term: Term<EventProxy>,
    /// VTE ANSI byte-stream processor that feeds bytes into `term`.
    processor: AnsiProcessor,
    /// Event proxy shared with `term`; drained after each process_output call
    /// to forward CPR/DSR replies back to the child PTY.
    event_proxy: EventProxy,
    /// Bytes to send to this pane (written by the key handler).
    input_tx: mpsc::Sender<Bytes>,
    /// Events from daemon for this pane (drained each UI tick).
    output_rx: mpsc::Receiver<PaneEvent>,
    /// Last polled block list for the ribbon (up to 20 entries, newest last).
    recent_blocks: Vec<Block>,
    /// `None` = live (rightmost highlighted); `Some(i)` = scrollback cursor.
    ribbon_cursor: Option<usize>,

    /// Last PTY size successfully sent to the daemon, to avoid spamming per frame.
    last_sent_size: (u16, u16),

    /// Number of OutputFrame messages received from the daemon on this stream.
    /// Used to distinguish a connection-level failure (zero frames → do not fire
    /// close_pane RPC) from a legitimate pane exit (≥1 frames → fire close_pane).
    frames_received: u64,

    /// 0 = live view; N = N lines scrolled back via vt100 native scrollback.
    scroll_offset: usize,
    /// Total scrollback lines available as of the last render (cached via peek/restore).
    /// vt100::Screen::scrollback() returns the *current offset*, not the capacity;
    /// we peek by setting MAX and reading the clamped value, then restore.
    scrollback_capacity: usize,
    /// The screen rect captured during the last render, used for mouse hit-test.
    last_screen_rect: Rect,
    /// Ribbon chip rects captured during last render: (block_idx, rect).
    ribbon_chip_rects: Vec<(usize, Rect)>,
    /// Output bytes received before the first render (parser not yet sized to
    /// the real pane area). Drained into the parser on the first render frame.
    pending_output: Vec<u8>,
    /// True once the parser has been sized to the actual pane area and
    /// `pending_output` has been flushed. Set on the first `render_pane` call.
    parser_sized: bool,
    /// Timestamp of the last `process_output` debug log emission (50 ms throttle).
    last_output_log: Option<Instant>,
}

impl PaneSlot {
    /// Feed raw bytes into the alacritty Term processor.
    /// If the terminal has not yet been sized to the real pane area (before the
    /// first render frame), bytes are buffered in `pending_output` instead of
    /// being processed at the wrong terminal dimensions. `render_pane` drains
    /// the buffer once it knows the correct area size.
    fn process_output(&mut self, data: &[u8]) {
        // Throttled debug log: at most once per 50 ms to avoid flooding.
        let now = Instant::now();
        let emit = match self.last_output_log {
            None => true,
            Some(t) => now.duration_since(t) >= Duration::from_millis(50),
        };
        if emit {
            tracing::debug!(
                bytes = data.len(),
                parser_sized = self.parser_sized,
                pane_id = %self.pane_id.0,
                "process_output: chunk"
            );
            self.last_output_log = Some(now);
        }

        if self.parser_sized {
            self.processor.advance(&mut self.term, data);
        } else {
            self.pending_output.extend_from_slice(data);
        }
    }

    /// Drain any PtyWrite responses generated by the Term (CPR/DSR replies)
    /// and return them as raw bytes to be forwarded back to the child PTY.
    fn drain_pty_responses(&self) -> Vec<u8> {
        self.event_proxy.drain()
    }
}

/// Recursive layout tree. Indices reference `AppState::slots`.
///
/// HSplit and VSplit children carry a `u16` weight (percentage, summing to 100
/// across siblings). Initial splits are equal-weight. Drag-resize updates the
/// weights of neighboring children while clamping each to >=5.
enum LayoutNode {
    Leaf(usize),
    /// Horizontal split (children stacked top-to-bottom). Weights are percentages.
    HSplit(Vec<(LayoutNode, u16)>),
    /// Vertical split (children side-by-side). Weights are percentages.
    VSplit(Vec<(LayoutNode, u16)>),
}

/// A boundary between two split children — used for drag-resize hit-testing.
#[derive(Clone)]
struct SplitBoundary {
    /// Screen coordinate (column for VSplit, row for HSplit) of the boundary.
    coord: u16,
    /// Axis: true = horizontal split (drag row), false = vertical split (drag col).
    is_hsplit: bool,
    /// Path to the parent split node.
    parent_path: Vec<usize>,
    /// Index of the LEFT/TOP child (the boundary is between child_idx and child_idx+1).
    child_idx: usize,
    /// Total size of the parent in the split axis (height for HSplit, width for VSplit).
    parent_size: u16,
}

/// Active drag state.
struct DragState {
    boundary: SplitBoundary,
    /// Terminal coordinate (col or row depending on axis) where drag began.
    start_coord: u16,
    /// Weights of all children in the parent split at drag start.
    start_weights: Vec<u16>,
}

/// One tab, owning a layout tree and a cursor into the focused leaf.
struct Tab {
    root: LayoutNode,
    /// Path of child indices from `root` down to the active `Leaf`.
    focus_path: Vec<usize>,
    /// When Some, renders only the leaf at that focus_path filling the body rect.
    zoomed: Option<Vec<usize>>,
    /// Boundaries collected during the last render, used for drag-resize hit-test.
    boundaries: Vec<SplitBoundary>,
    /// Active drag state (set on mouse-down near a boundary).
    drag: Option<DragState>,
}

/// State for the full-text search overlay (Ctrl-B /).
struct SearchState {
    open: bool,
    input: String,
    /// Selected result index.
    cursor: usize,
    results: Vec<BlockHit>,
    last_query_at: Instant,
    pending_query: Option<String>,
    /// Prefix `!` in the search box sets this (non-zero exit only).
    failures_only: bool,
    rx: Option<mpsc::Receiver<Vec<BlockHit>>>,
}

/// Split `!query` failures filter from the tantivy query string.
fn parse_search_input(input: &str) -> (String, bool) {
    if let Some(rest) = input.strip_prefix('!') {
        (rest.trim_start().to_string(), true)
    } else {
        (input.to_string(), false)
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            cursor: 0,
            results: Vec::new(),
            last_query_at: Instant::now(),
            pending_query: None,
            failures_only: false,
            rx: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Drag-selection types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
enum SelectionBase {
    Live,
    #[allow(dead_code)]
    Scrollback(usize), // window_top line index into scrollback
}

#[derive(Clone)]
struct Selection {
    pane_idx: usize,
    /// (row, col) relative to the pane's vt100/content area, viewport-relative.
    start: (u16, u16),
    end: (u16, u16),
    dragging: bool,
    base: SelectionBase,
}

impl Selection {
    fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        let (sr, sc) = self.start;
        let (er, ec) = self.end;
        if (sr, sc) <= (er, ec) {
            ((sr, sc), (er, ec))
        } else {
            ((er, ec), (sr, sc))
        }
    }

    #[allow(dead_code)]
    fn contains(&self, row: u16, col: u16) -> bool {
        let ((r0, c0), (r1, c1)) = self.normalized();
        if row < r0 || row > r1 {
            return false;
        }
        if row == r0 && col < c0 {
            return false;
        }
        if row == r1 && col > c1 {
            return false;
        }
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Click tracker (for double/triple-click detection)
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
struct ClickTracker {
    last_at: Instant,
    last_pos: (u16, u16), // (col, row) in terminal coordinates
    count: u8,
    pane_idx: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Right-click context menu
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum MenuItem {
    Copy,
    KillPane,
    SplitH,
    SplitV,
    ZoomToggle,
    InspectPid,
}

#[allow(dead_code)]
impl MenuItem {
    fn label(self) -> &'static str {
        match self {
            Self::Copy => " Copy",
            Self::KillPane => " Kill pane",
            Self::SplitH => " Split horizontal",
            Self::SplitV => " Split vertical",
            Self::ZoomToggle => " Zoom toggle",
            Self::InspectPid => " Inspect PID",
        }
    }
}

#[allow(dead_code)]
const MENU_ITEMS: &[MenuItem] = &[
    MenuItem::Copy,
    MenuItem::KillPane,
    MenuItem::SplitH,
    MenuItem::SplitV,
    MenuItem::ZoomToggle,
    MenuItem::InspectPid,
];

#[allow(dead_code)]
struct ContextMenu {
    rect: Rect,
    cursor: usize,
    target_slot: usize,
}

/// Per-session view: tabs and panes for one daemon session.
struct SessionView {
    id: SessionId,
    name: String,
    tabs: Vec<Tab>,
    active_tab: usize,
}

/// Which kind of name-prompt overlay is open.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptKind {
    NewSession,
    NewTab,
    RenameSession(SessionId),
}

/// Name-prompt overlay state.
struct NamePrompt {
    kind: PromptKind,
    input: String,
}

#[allow(dead_code)]
struct AppState {
    /// All known sessions (may have tabs loaded lazily).
    sessions: Vec<SessionView>,
    /// Index into `sessions` that is currently displayed.
    active_session: usize,
    /// All attached pane slots (shared across all sessions). None = closed/removed.
    slots: Vec<Option<PaneSlot>>,
    control: PyreDaemonClient,
    socket: PathBuf,
    shell: Option<String>,
    search: SearchState,
    /// One-line status message shown when action feedback is needed.
    status_msg: Option<String>,
    /// Whether the sidebar is visible.
    sidebar_open: bool,
    /// Cached pane info for sidebar display.
    sidebar_data: Vec<pyre_proto::PaneInfo>,
    /// Last time sidebar data was fetched.
    sidebar_last_poll: Instant,
    /// Selected row index within the sidebar.
    sidebar_cursor: usize,
    /// Whether the sidebar panel has keyboard focus.
    sidebar_focused: bool,
    /// Active text selection (drag or click-to-select).
    selection: Option<Selection>,
    /// State for double/triple-click detection.
    last_click: Option<ClickTracker>,
    /// Right-click context menu state.
    context_menu: Option<ContextMenu>,
    /// PID inspect overlay data.
    pid_inspect: Option<PidInspect>,
    /// Name-prompt overlay (new session or new tab).
    prompt: Option<NamePrompt>,
    /// Session strip hit-test rects: (session_vec_index, rect).
    session_strip_rects: Vec<(usize, Rect)>,
    /// Rect of the [+] button in the session strip.
    session_plus_rect: Option<Rect>,
    /// Rect of the [+] button in the tabs strip.
    tab_plus_rect: Option<Rect>,
    /// Queued resize RPCs collected by render_pane (sync); drained after each draw.
    pending_resizes: Vec<(PaneId, pyre_proto::PaneSize)>,
    /// Last time the session list was refreshed from the daemon.
    session_list_last_poll: Instant,
    /// Latest block snapshot delivered by the background poll task.
    /// Key = PaneId, value = blocks for that pane (up to 20, newest last).
    blocks_rx: watch::Receiver<HashMap<PaneId, Vec<Block>>>,
    /// In-TUI ember motion (shared curves with startup splash).
    anim: AnimClock,
}

impl AppState {
    /// Convenience: active session's session id.
    fn active_session_id(&self) -> SessionId {
        self.sessions[self.active_session].id
    }

    /// Convenience: active session view (mutable).
    fn active_session_view_mut(&mut self) -> &mut SessionView {
        &mut self.sessions[self.active_session]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Layout helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk the tree depth-first and collect every focus_path for each leaf.
fn leaves_in_order(node: &LayoutNode, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    match node {
        LayoutNode::Leaf(_) => out.push(path.clone()),
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
            for (i, (child, _weight)) in children.iter().enumerate() {
                path.push(i);
                leaves_in_order(child, path, out);
                path.pop();
            }
        }
    }
}

/// Return the slot index at a given focus path.
fn slot_at(root: &LayoutNode, path: &[usize]) -> Option<usize> {
    let mut node = root;
    for &idx in path {
        match node {
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                node = &children.get(idx)?.0;
            }
            LayoutNode::Leaf(_) => return None,
        }
    }
    match node {
        LayoutNode::Leaf(slot) => Some(*slot),
        _ => None,
    }
}

/// Replace the node at `path` with `new_node`.
fn replace_at(root: &mut LayoutNode, path: &[usize], new_node: LayoutNode) {
    if path.is_empty() {
        *root = new_node;
        return;
    }
    match root {
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
            replace_at(&mut children[path[0]].0, &path[1..], new_node);
        }
        LayoutNode::Leaf(_) => {}
    }
}

/// Mutably access the children of the split node at `path`.
fn children_at_mut<'a>(
    root: &'a mut LayoutNode,
    path: &[usize],
) -> Option<&'a mut Vec<(LayoutNode, u16)>> {
    let mut node = root;
    for &idx in path {
        match node {
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                node = &mut children[idx].0;
            }
            LayoutNode::Leaf(_) => return None,
        }
    }
    match node {
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => Some(children),
        LayoutNode::Leaf(_) => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mouse hit-test helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk the layout tree and collect (slot_index, screen_rect) for each leaf,
/// computing rects the same way render_layout does (without actually rendering).
fn collect_leaf_rects(node: &LayoutNode, area: Rect, out: &mut Vec<(usize, Rect)>) {
    match node {
        LayoutNode::Leaf(slot_idx) => {
            out.push((*slot_idx, area));
        }
        LayoutNode::HSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
        LayoutNode::VSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
    }
}

/// Returns true if (col, row) is inside `rect`.
fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

// ─────────────────────────────────────────────────────────────────────────────
// Stream connection + background tasks for one pane
// ─────────────────────────────────────────────────────────────────────────────

async fn attach_pane(
    socket: &Path,
    session: SessionId,
    pane_id: PaneId,
    cols: u16,
    rows: u16,
) -> Result<PaneSlot> {
    tracing::debug!(cols, rows, pane_id = %pane_id.0, "attach_pane: entry");

    let mut stream_sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane_id.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // Bug B fix: bumped to 1024 to absorb output bursts without blocking the
    // net→UI task. The sender uses try_send so a full channel drops the chunk
    // (output loss) instead of hanging the UI loop (backpressure stall).
    let (net_tx, output_rx) = mpsc::channel::<PaneEvent>(1024);
    let (input_tx, mut key_rx) = mpsc::channel::<Bytes>(64);

    // net → UI
    tokio::spawn(async move {
        let mut frames: u64 = 0;
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    frames += 1;
                    if let Err(e) = net_tx.try_send(PaneEvent::Output(f.data)) {
                        match e {
                            mpsc::error::TrySendError::Full(_) => {
                                // Channel saturated during burst — drop chunk, keep running.
                                tracing::warn!("net→UI channel full; dropping output chunk");
                            }
                            mpsc::error::TrySendError::Closed(_) => break,
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Stream ended. Carry frame count so the UI can distinguish a
        // connection-level failure (0 frames) from a real pane exit (≥1 frames).
        let _ = net_tx.try_send(PaneEvent::Closed {
            frames_received: frames,
        });
    });

    // UI → net
    // Batch keystrokes: after the first byte arrives, drain all queued bytes
    // into a single concatenated buffer and send one InputFrame per tick.
    // This converts N sequential framed UDS writes down to 1 per render tick,
    // eliminating per-keystroke serialization latency for fast typists.
    tokio::spawn(async move {
        while let Some(first) = key_rx.recv().await {
            // Drain any additional bytes already queued in the channel.
            let mut buf: Vec<u8> = first.to_vec();
            while let Ok(more) = key_rx.try_recv() {
                buf.extend_from_slice(&more);
            }
            let batch_len = buf.len();
            let t0 = std::time::Instant::now();
            let send_result = input_frames
                .send(InputFrame {
                    session,
                    data: Bytes::from(buf),
                })
                .await;
            let elapsed_us = t0.elapsed().as_micros();
            tracing::debug!(
                batch_bytes = batch_len,
                elapsed_us,
                send_ok = send_result.is_ok(),
                "send_keys: input_frames RPC send"
            );
            if send_result.is_err() {
                break;
            }
        }
    });

    tracing::debug!(rows, cols, pane_id = %pane_id.0, "attach_pane: creating alacritty Term");
    let event_proxy = EventProxy::new();
    let term_config = TermConfig::default();
    // (cols, rows) implements Dimensions via the tuple impl in alacritty_terminal.
    let term = Term::new(
        term_config,
        &TermSize::new(cols as usize, rows as usize),
        event_proxy.clone(),
    );
    Ok(PaneSlot {
        pane_id,
        term,
        processor: AnsiProcessor::new(),
        event_proxy,
        input_tx,
        output_rx,
        recent_blocks: Vec::new(),
        ribbon_cursor: None,
        last_sent_size: (cols, rows),
        frames_received: 0,
        scroll_offset: 0,
        scrollback_capacity: 0,
        last_screen_rect: Rect::default(),
        ribbon_chip_rects: Vec::new(),
        pending_output: Vec::new(),
        parser_sized: false,
        last_output_log: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn pane_needs_attention(meta: &[pyre_proto::PaneInfo], pane_id: PaneId) -> bool {
    meta.iter()
        .any(|p| p.id == pane_id && p.state == pyre_proto::PaneStateKind::WaitingInput && !p.seen)
}

#[allow(clippy::too_many_arguments)]
fn render_pane(
    frame: &mut ratatui::Frame,
    area: Rect,
    slot: &mut PaneSlot,
    focused: bool,
    selection: Option<&Selection>,
    slot_idx: usize,
    pending_resizes: &mut Vec<(PaneId, pyre_proto::PaneSize)>,
    anim_frame: u64,
    attention: bool,
) {
    let short8: String = slot.pane_id.0.to_string().chars().take(8).collect();
    let seed = slot.pane_id.0.as_u128() as u32;
    let border_block = if focused {
        let border_style = if attention {
            fire_motion::ember_border_style(anim_frame, seed, EMBER.border_focus, EMBER.spark)
        } else {
            EMBER.border_focus()
        };
        let title_style = if attention {
            fire_motion::ember_title_style(anim_frame, seed, EMBER.primary, EMBER.spark)
        } else {
            EMBER.title(EMBER.primary)
        };
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick)
            .border_style(border_style)
            .title(Span::styled(format!(" pane {short8} "), title_style))
    } else if attention {
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fire_motion::ember_border_style(
                anim_frame,
                seed,
                EMBER.border,
                EMBER.primary,
            ))
            .title(Span::styled(
                format!(" pane {short8} "),
                fire_motion::ember_title_style(anim_frame, seed, EMBER.text_dim, EMBER.secondary),
            ))
    } else {
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(EMBER.border())
            .title(Span::styled(
                format!(" pane {short8} "),
                EMBER.title(EMBER.text_dim),
            ))
    };

    let inner = border_block.inner(area);
    frame.render_widget(border_block, area);

    // Split inner area: vt100/scrollback area (Min 1) on top, ribbon (1 line) at bottom.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let content_area = split[0];
    let ribbon_area = split[1];

    // Store the content rect for mouse hit-test (used by scroll wheel handler).
    slot.last_screen_rect = content_area;

    // ── Unified render: scroll_display shifts the alacritty view; 0 = live ──
    // Peek total scrollback capacity by temporarily jumping to Top, reading
    // history_size(), then restoring to our desired offset.
    slot.scrollback_capacity = slot.term.grid().history_size();
    // Clamp current offset in case old lines aged out of the ring buffer.
    slot.scroll_offset = slot.scroll_offset.min(slot.scrollback_capacity);
    // Set display_offset to our desired scrollback position.
    slot.term.grid_mut().scroll_display(Scroll::Bottom);
    if slot.scroll_offset > 0 {
        slot.term
            .grid_mut()
            .scroll_display(Scroll::Delta(slot.scroll_offset as i32));
    }

    // When scrolled back, reserve 1 column on the right for a scrollbar.
    let (sb_area, text_area) = if slot.scroll_offset > 0 && content_area.width > 1 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(content_area);
        (Some(split[1]), split[0])
    } else {
        (None, content_area)
    };

    // Bug A fix: sync terminal dimensions to the actual visible area each frame.
    // If the terminal was never resized (e.g. after a split), it still thinks it
    // is the original full-terminal size and positions output beyond the pane
    // bounds, producing invisible or overlapping lines.
    {
        let target_rows = text_area.height as usize;
        let target_cols = text_area.width as usize;
        let cur_rows = slot.term.grid().screen_lines();
        let cur_cols = slot.term.grid().columns();

        // Log on first call only (before parser_sized is set).
        if !slot.parser_sized {
            tracing::debug!(
                slot_idx,
                text_area.width,
                text_area.height,
                parser_rows = cur_rows,
                parser_cols = cur_cols,
                "render_pane: first call"
            );
        }

        if cur_rows != target_rows || cur_cols != target_cols {
            tracing::debug!(
                slot_idx,
                old_rows = cur_rows,
                old_cols = cur_cols,
                new_rows = target_rows,
                new_cols = target_cols,
                "render_pane: terminal resize"
            );
            slot.term.resize(TermSize::new(target_cols, target_rows));
        }
        // On the first render we now know the real pane area. Drain any bytes
        // that arrived before this frame (buffered in pending_output at wrong
        // size) through the correctly-sized terminal, then mark as sized so
        // subsequent bytes go directly to the terminal.
        if !slot.parser_sized {
            slot.parser_sized = true;
            if !slot.pending_output.is_empty() {
                let buffered = std::mem::take(&mut slot.pending_output);
                slot.processor.advance(&mut slot.term, &buffered);
            }
        }
        // Fire resize RPC when dims changed AND differ from last sent — avoid
        // spamming the daemon every frame. Collected into pending_resizes and
        // drained after draw() returns (async context).
        let (last_cols, last_rows) = slot.last_sent_size;
        let target_cols_u16 = target_cols as u16;
        let target_rows_u16 = target_rows as u16;
        if target_cols_u16 != last_cols || target_rows_u16 != last_rows {
            slot.last_sent_size = (target_cols_u16, target_rows_u16);
            pending_resizes.push((
                slot.pane_id,
                pyre_proto::PaneSize {
                    cols: target_cols_u16,
                    rows: target_rows_u16,
                },
            ));
        }
    }

    {
        let grid = slot.term.grid();
        let num_rows = grid.screen_lines();
        let num_cols = grid.columns();
        let mut lines: Vec<Line> = Vec::with_capacity(text_area.height as usize);

        for row in 0..text_area.height as usize {
            let mut spans: Vec<Span> = Vec::new();
            let mut current_text = String::new();
            let mut current_style = Style::default();

            // The viewport top line when scrolled: display_offset lines above Line(0).
            // display_iter visits rows from top of viewport downward; we index directly.
            let display_line = TermLine(row as i32 - grid.display_offset() as i32);

            for col in 0..text_area.width as usize {
                let (ch, fg, bg, flags) = if row < num_rows && col < num_cols {
                    let cell = &grid[TermPoint::new(display_line, TermColumn(col))];
                    let ch = if cell.c == '\0' { ' ' } else { cell.c };
                    (ch, ansi_color(cell.fg), ansi_color(cell.bg), cell.flags)
                } else {
                    (' ', None, None, CellFlags::empty())
                };

                let mut style = Style::default()
                    .fg(fg.unwrap_or(Color::Reset))
                    .bg(bg.unwrap_or(Color::Reset));

                if flags.contains(CellFlags::BOLD) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if flags.contains(CellFlags::DIM) {
                    style = style.add_modifier(Modifier::DIM);
                }
                if flags.contains(CellFlags::ITALIC) {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if flags.intersects(CellFlags::ALL_UNDERLINES) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if flags.contains(CellFlags::INVERSE) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if flags.contains(CellFlags::HIDDEN) {
                    style = style.add_modifier(Modifier::HIDDEN);
                }
                if flags.contains(CellFlags::STRIKEOUT) {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }

                if style == current_style {
                    current_text.push(ch);
                } else {
                    if !current_text.is_empty() {
                        spans.push(Span::styled(current_text.clone(), current_style));
                        current_text.clear();
                    }
                    current_text.push(ch);
                    current_style = style;
                }
            }
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text, current_style));
            }
            lines.push(Line::from(spans));
        }

        frame.render_widget(Paragraph::new(lines), text_area);
    }

    // Overlay selection highlight on live view.
    if slot.scroll_offset == 0 {
        if let Some(sel) = selection {
            if sel.pane_idx == slot_idx {
                if let SelectionBase::Live = sel.base {
                    let ((r0, c0), (r1, c1)) = sel.normalized();
                    for row in r0..=r1.min(text_area.height.saturating_sub(1)) {
                        let col_start = if row == r0 { c0 } else { 0 };
                        let col_end = if row == r1 {
                            c1.min(text_area.width.saturating_sub(1))
                        } else {
                            text_area.width.saturating_sub(1)
                        };
                        for col in col_start..=col_end {
                            let sx = text_area.x + col;
                            let sy = text_area.y + row;
                            if sx < text_area.x + text_area.width
                                && sy < text_area.y + text_area.height
                            {
                                if let Some(cell) = frame.buffer_mut().cell_mut((sx, sy)) {
                                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Scrollbar when scrolled back.
    if let Some(sb_rect) = sb_area {
        let total_scrollback = slot.scrollback_capacity;
        let virtual_total = total_scrollback.max(1);
        let position = virtual_total.saturating_sub(slot.scroll_offset);
        let mut sb_state = ScrollbarState::new(virtual_total).position(position);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(EMBER.border())
                .thumb_style(Style::default().fg(EMBER.primary))
                .track_symbol(Some("│"))
                .thumb_symbol("█"),
            sb_rect,
            &mut sb_state,
        );
    }

    // ── ribbon render ──
    render_ribbon(frame, ribbon_area, slot);
}

/// Render the one-line block ribbon inside `area`.
/// Captures chip rects into `slot.ribbon_chip_rects` for mouse hit-test.
fn render_ribbon(frame: &mut ratatui::Frame, area: Rect, slot: &mut PaneSlot) {
    // Clear chip rects from the previous frame.
    slot.ribbon_chip_rects.clear();

    if slot.recent_blocks.is_empty() {
        let p =
            Paragraph::new(" (no blocks)").style(Style::default().fg(EMBER.text_dim).bg(EMBER.bg));
        frame.render_widget(p, area);
        return;
    }

    // Determine the highlighted index. None = live (last block).
    let is_live = slot.ribbon_cursor.is_none();
    let latest_idx = slot.recent_blocks.len().saturating_sub(1);
    let highlight_idx = slot.ribbon_cursor.unwrap_or(latest_idx);

    let mut spans: Vec<Span> = Vec::new();
    // Track x offset for chip rect calculation.
    let mut x_offset: u16 = area.x;

    for (i, b) in slot.recent_blocks.iter().enumerate() {
        let short4: String = b.id.0.to_string().chars().take(4).collect();

        // Exit code badge colour and prefix.
        let (badge_fg, live_prefix) = match b.exit_code {
            Some(0) => (EMBER.ok, ""),
            Some(_) => (EMBER.err, ""),
            None => (EMBER.spark, "●"),
        };

        let sep = if i > 0 { "│" } else { "" };
        let chip_text = format!("{live_prefix}▎b{short4}");
        let sep_len = sep.chars().count() as u16;
        let chip_len = chip_text.chars().count() as u16;

        // Record rect for this chip (separator not included in clickable area).
        if area.height > 0 && x_offset + sep_len < area.x + area.width {
            slot.ribbon_chip_rects.push((
                i,
                Rect::new(
                    x_offset + sep_len,
                    area.y,
                    chip_len.min((area.x + area.width).saturating_sub(x_offset + sep_len)),
                    1,
                ),
            ));
        }
        x_offset += sep_len + chip_len;

        let chip_style = if i == highlight_idx && !is_live {
            EMBER.selection()
        } else if i == latest_idx && is_live {
            Style::default()
                .fg(EMBER.bg)
                .bg(EMBER.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(badge_fg).bg(EMBER.muted_bg)
        };

        if i > 0 {
            spans.push(Span::styled("│", Style::default().fg(EMBER.text_dim)));
        }
        spans.push(Span::styled(chip_text, chip_style));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(EMBER.bg)),
        area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_layout(
    frame: &mut ratatui::Frame,
    area: Rect,
    node: &LayoutNode,
    slots: &mut Vec<Option<PaneSlot>>,
    focus_path: &[usize],
    current_path: &mut Vec<usize>,
    boundaries: &mut Vec<SplitBoundary>,
    selection: Option<&Selection>,
    pending_resizes: &mut Vec<(PaneId, pyre_proto::PaneSize)>,
    anim_frame: u64,
    panes_meta: &[pyre_proto::PaneInfo],
) {
    match node {
        LayoutNode::Leaf(slot_idx) => {
            if let Some(slot) = slots[*slot_idx].as_mut() {
                let focused = current_path == focus_path;
                let attention = pane_needs_attention(panes_meta, slot.pane_id);
                render_pane(
                    frame,
                    area,
                    slot,
                    focused,
                    selection,
                    *slot_idx,
                    pending_resizes,
                    anim_frame,
                    attention,
                );
            }
        }
        LayoutNode::HSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            // Collect horizontal boundaries between children.
            for i in 0..children.len().saturating_sub(1) {
                let boundary_row = rects[i].y + rects[i].height;
                boundaries.push(SplitBoundary {
                    coord: boundary_row,
                    is_hsplit: true,
                    parent_path: current_path.clone(),
                    child_idx: i,
                    parent_size: area.height,
                });
            }
            for (i, ((child, _), rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(
                    frame,
                    *rect,
                    child,
                    slots,
                    focus_path,
                    current_path,
                    boundaries,
                    selection,
                    pending_resizes,
                    anim_frame,
                    panes_meta,
                );
                current_path.pop();
            }
        }
        LayoutNode::VSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            // Collect vertical boundaries between children.
            for i in 0..children.len().saturating_sub(1) {
                let boundary_col = rects[i].x + rects[i].width;
                boundaries.push(SplitBoundary {
                    coord: boundary_col,
                    is_hsplit: false,
                    parent_path: current_path.clone(),
                    child_idx: i,
                    parent_size: area.width,
                });
            }
            for (i, ((child, _), rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(
                    frame,
                    *rect,
                    child,
                    slots,
                    focus_path,
                    current_path,
                    boundaries,
                    selection,
                    pending_resizes,
                    anim_frame,
                    panes_meta,
                );
                current_path.pop();
            }
        }
    }
}

/// Render the search overlay centered on the terminal.
fn render_search_overlay(frame: &mut ratatui::Frame, app: &AppState) {
    let area = frame.area();

    // Centered rect: ~70% width, ~60% height.
    let w = (area.width as f32 * 0.70) as u16;
    let h = (area.height as f32 * 0.60) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(20), h.max(6));

    // Clear backing area so panes don't bleed through.
    frame.render_widget(Clear, overlay_rect);

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(EMBER.border_focus())
        .title(Span::styled(
            " search (! = failures) ",
            EMBER.title(EMBER.primary),
        ))
        .style(EMBER.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Split inner: 3-line input box + remainder for results.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let input_area = split[0];
    let results_area = split[1];

    // Input box — prompt prefix `> ` in primary, query in text, cursor █ in spark.
    let cursor_f = app.anim.frame();
    let input_spans = vec![
        Span::styled("> ", Style::default().fg(EMBER.primary)),
        Span::styled(app.search.input.as_str(), Style::default().fg(EMBER.text)),
        Span::styled(
            "█",
            fire_motion::ember_fg_style(cursor_f, 0x_a11ce, EMBER.spark, EMBER.secondary, 0.9),
        ),
    ];
    let input_block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(EMBER.border())
        .style(EMBER.overlay());
    let input_para = Paragraph::new(Line::from(input_spans))
        .block(input_block)
        .style(EMBER.bg_style());
    frame.render_widget(input_para, input_area);

    // Set host cursor at end of input text: inner input area x + 2 (prompt) + query len.
    // The input block has 1-cell border on each side, so inner starts at input_area.x + 1.
    let inner_x = input_area.x + 1;
    let inner_y = input_area.y + 1;
    // "> " prefix (2 chars) + query length, clamped to inner width.
    let inner_width = input_area.width.saturating_sub(2); // subtract left+right border
    let cursor_col = (2u16 + app.search.input.len() as u16).min(inner_width.saturating_sub(1));
    frame.set_cursor_position((inner_x + cursor_col, inner_y));

    // Results list.
    let items: Vec<ListItem> = app
        .search
        .results
        .iter()
        .map(|hit| {
            let b = &hit.block;
            let pane_short: String = b.pane.0.to_string().chars().take(8).collect();
            let ts_short = b.started_at.format("%H:%M:%S").to_string();
            let snippet: String = if hit.snippet.is_empty() {
                b.command.chars().take(80).collect()
            } else {
                hit.snippet.chars().take(80).collect()
            };
            ListItem::new(format!("[{pane_short}] {ts_short} {snippet}"))
                .style(Style::default().fg(EMBER.text))
        })
        .collect();

    let list = List::new(items)
        .block(
            RatatuiBlock::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(EMBER.border())
                .title(Span::styled(
                    format!(" {} results ", app.search.results.len()),
                    Style::default().fg(EMBER.text_dim),
                ))
                .style(EMBER.overlay()),
        )
        .highlight_style(EMBER.selection());

    // Use a stateful list so we can highlight the cursor item.
    let mut list_state = ratatui::widgets::ListState::default();
    if !app.search.results.is_empty() {
        list_state.select(Some(app.search.cursor));
    }
    frame.render_stateful_widget(list, results_area, &mut list_state);
}

fn state_dot_char(state: pyre_proto::PaneStateKind) -> char {
    use pyre_proto::PaneStateKind::*;
    match state {
        Running => '●',
        WaitingInput => '◎',
        Idle => '○',
        Interactive => '◆',
        Crashed => '✗',
        Done => '◦',
    }
}

fn state_dot_color(state: pyre_proto::PaneStateKind) -> Color {
    use pyre_proto::PaneStateKind::*;
    match state {
        Running => EMBER.ok,
        WaitingInput => EMBER.spark,
        Idle | Done => EMBER.text_dim,
        Interactive => EMBER.info,
        Crashed => EMBER.err,
    }
}

/// Agent-friendly label for sidebar (maps daemon state + seen flag).
fn agent_ui_label(state: pyre_proto::PaneStateKind, seen: bool) -> &'static str {
    use pyre_proto::PaneStateKind::*;
    match (state, seen) {
        (WaitingInput, _) => "blocked",
        (Running, _) => "working",
        (Interactive, _) => "interactive",
        (Crashed, _) => "crashed",
        (Done, false) => "done",
        (Done, true) => "idle",
        (Idle, _) => "idle",
    }
}

/// Worst pane in a session (for session-strip rollup).
fn session_worst_pane(
    sidebar: &[pyre_proto::PaneInfo],
    session_id: pyre_proto::SessionId,
) -> Option<&pyre_proto::PaneInfo> {
    use pyre_proto::PaneStateKind::*;
    let rank = |s: pyre_proto::PaneStateKind| -> u8 {
        match s {
            Crashed => 0,
            WaitingInput => 1,
            Running => 2,
            Interactive => 3,
            Idle => 4,
            Done => 5,
        }
    };
    sidebar
        .iter()
        .filter(|p| p.session == session_id)
        .min_by_key(|p| rank(p.state))
}

fn session_name_for(state: &AppState, session_id: pyre_proto::SessionId) -> String {
    state
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| {
            let s = session_id.0.to_string();
            s[..8.min(s.len())].to_string()
        })
}

fn render_sidebar(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let block = RatatuiBlock::default()
        .borders(Borders::RIGHT)
        .border_type(BorderType::Rounded)
        .style(EMBER.bg_style())
        .title(Span::styled(" agents ", EMBER.title(EMBER.primary)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = state
        .sidebar_data
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(i, info)| {
            let sess = session_name_for(state, info.session);
            let dot = state_dot_char(info.state);
            let anim_f = state.anim.frame();
            let seed = info.id.0.as_u128() as u32;
            let dot_color = if info.state == pyre_proto::PaneStateKind::WaitingInput && !info.seen {
                let p = fire_motion::pulse_phase(anim_f, seed, 9.0);
                fire_motion::lerp_rgb(
                    fire_motion::rgb_tuple(state_dot_color(info.state)),
                    fire_motion::rgb_tuple(EMBER.secondary),
                    p * 0.55,
                )
            } else {
                state_dot_color(info.state)
            };
            let label = agent_ui_label(info.state, info.seen);
            let agent = info.agent.label();
            let id_str = info.id.0.to_string();
            let pane_short = &id_str[..8.min(id_str.len())];
            let row_style = if i == state.sidebar_cursor && state.sidebar_focused {
                Style::default()
                    .fg(EMBER.bg)
                    .bg(EMBER.primary)
                    .add_modifier(Modifier::BOLD)
            } else if i == state.sidebar_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if info.state == pyre_proto::PaneStateKind::WaitingInput && !info.seen {
                fire_motion::ember_fg_style(anim_f, seed, EMBER.spark, EMBER.text, 0.45)
            } else {
                Style::default().fg(EMBER.text)
            };
            ListItem::new(Line::from(vec![
                Span::styled("  ", row_style),
                Span::styled(dot.to_string(), Style::default().fg(dot_color)),
                Span::styled(format!(" {sess} {label} {agent} {pane_short}"), row_style),
            ]))
        })
        .collect();

    let list = List::new(items).style(EMBER.bg_style());
    frame.render_widget(list, inner);
}

/// Render the name-prompt overlay and position the host cursor.
fn render_name_prompt(frame: &mut ratatui::Frame, prompt: &NamePrompt, anim_frame: u64) {
    let area = frame.area();
    let w = (area.width as f32 * 0.60) as u16;
    let h: u16 = 5;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(30), h);

    frame.render_widget(Clear, overlay_rect);

    let title = match prompt.kind {
        PromptKind::NewSession => " new session name ",
        PromptKind::NewTab => " new tab label ",
        PromptKind::RenameSession(_) => " rename session ",
    };

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(EMBER.border_focus())
        .title(Span::styled(title, EMBER.title(EMBER.primary)))
        .style(EMBER.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Input row (row 0 of inner) + hint row (row 1).
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let input_area = split[0];
    let hint_area = split[1];

    let input_spans = vec![
        Span::styled("> ", Style::default().fg(EMBER.primary)),
        Span::styled(prompt.input.as_str(), Style::default().fg(EMBER.text)),
        Span::styled(
            "█",
            fire_motion::ember_fg_style(anim_frame, 0xc0ffee, EMBER.spark, EMBER.secondary, 0.9),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(input_spans)), input_area);

    let hint = Paragraph::new(" Enter = create  |  Esc = cancel")
        .style(Style::default().fg(EMBER.text_dim));
    frame.render_widget(hint, hint_area);

    // Host cursor at end of input.
    let cursor_col = (2u16 + prompt.input.len() as u16).min(input_area.width.saturating_sub(1));
    frame.set_cursor_position((input_area.x + cursor_col, input_area.y));
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut AppState,
    prefix_active: bool,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Four rows: sessions strip (1) + tabs strip (1) + body (min 0) + status bar (1)
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let sessions_area = outer[0];
        let tabs_area = outer[1];
        let body_area = outer[2];
        let status_area = outer[3];

        // Frame clear — paint entire frame with bg_style so no bleed.
        frame.render_widget(
            RatatuiBlock::default().style(EMBER.bg_style()),
            frame.area(),
        );

        // ── Row 0: sessions strip ──
        {
            let mut new_session_rects: Vec<(usize, Rect)> = Vec::new();
            let mut spans: Vec<Span> = Vec::new();
            let mut x_cursor: u16 = sessions_area.x;

            for (i, sv) in state.sessions.iter().enumerate() {
                let rollup = session_worst_pane(&state.sidebar_data, sv.id);
                let rollup_tag = rollup
                    .map(|p| format!(":{}", agent_ui_label(p.state, p.seen)))
                    .unwrap_or_default();
                let label = format!(" {} {}{} ", i + 1, sv.name, rollup_tag);
                let len = label.chars().count() as u16;
                let needs_attention = rollup
                    .is_some_and(|p| p.state == pyre_proto::PaneStateKind::WaitingInput && !p.seen);
                let anim_f = state.anim.frame();
                let style = if i == state.active_session {
                    EMBER.tab_active()
                } else if needs_attention {
                    fire_motion::ember_fg_style(
                        anim_f,
                        sv.id.0.as_u128() as u32,
                        EMBER.spark,
                        EMBER.primary,
                        1.0,
                    )
                    .bg(EMBER.bg)
                } else {
                    EMBER.tab_inactive()
                };
                if sessions_area.height > 0 {
                    new_session_rects.push((i, Rect::new(x_cursor, sessions_area.y, len, 1)));
                }
                x_cursor += len;
                spans.push(Span::styled(label, style));
                if i + 1 < state.sessions.len() {
                    spans.push(Span::styled(" ", Style::default().bg(EMBER.bg)));
                    x_cursor += 1;
                }
            }

            // [+] button immediately after the last label (browser-style).
            let plus_label = "[+]";
            let plus_len = plus_label.len() as u16;
            // x_cursor now points to the cell right after the last session label.
            let plus_x = x_cursor;
            let plus_rect = if sessions_area.height > 0
                && plus_x + plus_len <= sessions_area.x + sessions_area.width
            {
                Some(Rect::new(plus_x, sessions_area.y, plus_len, 1))
            } else {
                None
            };
            if !spans.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(EMBER.bg)));
            }
            spans.push(Span::styled(plus_label, EMBER.tab_inactive()));

            state.session_strip_rects = new_session_rects;
            state.session_plus_rect = plus_rect;

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(EMBER.bg)),
                sessions_area,
            );
        }

        // ── Row 1: tabs strip of active session ──
        {
            let sv = &state.sessions[state.active_session];
            let total_tabs = sv.tabs.len();
            let mut spans: Vec<Span> = Vec::new();
            let mut x_cursor: u16 = tabs_area.x;

            for (i, _) in sv.tabs.iter().enumerate() {
                let label = format!(" {} ", i + 1);
                let len = label.chars().count() as u16;
                let style = if i == sv.active_tab {
                    EMBER.tab_active()
                } else {
                    EMBER.tab_inactive()
                };
                x_cursor += len;
                spans.push(Span::styled(label, style));
                if i + 1 < total_tabs {
                    spans.push(Span::styled(" ", Style::default().bg(EMBER.bg)));
                    x_cursor += 1;
                }
            }

            // [+] button immediately after the last tab label (browser-style).
            let plus_label = "[+]";
            let plus_len = plus_label.len() as u16;
            let plus_x = x_cursor;
            let plus_rect =
                if tabs_area.height > 0 && plus_x + plus_len <= tabs_area.x + tabs_area.width {
                    Some(Rect::new(plus_x, tabs_area.y, plus_len, 1))
                } else {
                    None
                };
            if !spans.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(EMBER.bg)));
            }
            spans.push(Span::styled(plus_label, EMBER.tab_inactive()));

            state.tab_plus_rect = plus_rect;

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(EMBER.bg)),
                tabs_area,
            );
        }

        // Body — optionally split horizontally for sidebar.
        let (sidebar_area_opt, pane_body_area) = if state.sidebar_open {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Min(0)])
                .split(body_area);
            (Some(cols[0]), cols[1])
        } else {
            (None, body_area)
        };

        if let Some(sbar_area) = sidebar_area_opt {
            render_sidebar(frame, sbar_area, state);
        }

        // Render active tab's layout in the remaining area.
        let active_tab_idx = state.sessions[state.active_session].active_tab;
        let focus_path = state.sessions[state.active_session].tabs[active_tab_idx]
            .focus_path
            .clone();
        let zoomed = state.sessions[state.active_session].tabs[active_tab_idx]
            .zoomed
            .clone();
        let mut new_boundaries: Vec<SplitBoundary> = Vec::new();

        // SAFETY: we only borrow root/zoomed via a raw pointer to avoid the
        // simultaneous mutable borrow of slots. render_layout only reads `root`
        // and mutates `slots` at disjoint indices; no mutation of `tabs` occurs.
        let root_ptr: *const LayoutNode =
            &state.sessions[state.active_session].tabs[active_tab_idx].root;

        let anim_frame = state.anim.frame();
        let panes_meta = state.sidebar_data.as_slice();

        if let Some(ref zoom_path) = zoomed {
            // Zoom mode: render only the zoomed leaf filling pane_body_area.
            if let Some(slot_idx) = slot_at(unsafe { &*root_ptr }, zoom_path) {
                if let Some(slot) = state.slots[slot_idx].as_mut() {
                    let attention = pane_needs_attention(panes_meta, slot.pane_id);
                    render_pane(
                        frame,
                        pane_body_area,
                        slot,
                        true,
                        state.selection.as_ref(),
                        slot_idx,
                        &mut state.pending_resizes,
                        anim_frame,
                        attention,
                    );
                }
            }
        } else {
            let mut current_path: Vec<usize> = Vec::new();
            render_layout(
                frame,
                pane_body_area,
                unsafe { &*root_ptr },
                &mut state.slots,
                &focus_path,
                &mut current_path,
                &mut new_boundaries,
                state.selection.as_ref(),
                &mut state.pending_resizes,
                anim_frame,
                panes_meta,
            );
        }
        state.sessions[state.active_session].tabs[active_tab_idx].boundaries = new_boundaries;

        // Status bar — two segments + optional middle message.
        {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot = slot_at(&tab.root, &tab.focus_path);
            let is_zoomed = tab.zoomed.is_some();

            // Determine mode label and mid message.
            let (mode_label, mid_msg) = if state.search.open {
                (
                    "SEARCH",
                    Some(format!(
                        " search: {} ({} results) ",
                        state.search.input,
                        state.search.results.len()
                    )),
                )
            } else if prefix_active {
                (
                    "PREFIX",
                    state.status_msg.as_ref().map(|m| format!(" {m} ")),
                )
            } else if let Some(slot_idx) = focused_slot {
                let in_ribbon = state.slots[slot_idx]
                    .as_ref()
                    .map(|s| s.ribbon_cursor.is_some())
                    .unwrap_or(false);
                if in_ribbon {
                    (
                        "SCROLL",
                        state.status_msg.as_ref().map(|m| format!(" {m} ")),
                    )
                } else {
                    ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
                }
            } else {
                ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
            };

            // Left: ` ● {session_name} ▸ {pane} `
            let left_text = if let Some(slot_idx) = focused_slot {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    let pane_short = &slot.pane_id.0.to_string()[..8];
                    format!(" ● {} ▸ {pane_short} ", sv.name)
                } else {
                    format!(" ● {} ", sv.name)
                }
            } else {
                format!(" ● {} ", sv.name)
            };

            // Right: mode indicator + optional ZOOM chip
            let right_text = format!(" {mode_label} ");

            let mut status_spans: Vec<Span> = vec![Span::styled(left_text, EMBER.status())];
            if let Some(msg) = mid_msg {
                status_spans.push(Span::styled(
                    msg,
                    Style::default().fg(EMBER.secondary).bg(EMBER.surface),
                ));
            }
            // Spacer to push mode to right — approximate with bg fill.
            status_spans.push(Span::styled(" ", Style::default().bg(EMBER.surface)));
            if is_zoomed {
                status_spans.push(Span::styled(
                    " ZOOM ",
                    Style::default()
                        .fg(EMBER.bg)
                        .bg(EMBER.primary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            status_spans.push(Span::styled(
                right_text,
                Style::default()
                    .fg(EMBER.bg)
                    .bg(EMBER.primary)
                    .add_modifier(Modifier::BOLD),
            ));

            frame.render_widget(
                Paragraph::new(Line::from(status_spans)).style(EMBER.status()),
                status_area,
            );
        }

        // Host-terminal cursor positioning.
        // Only one pane (the focused one, live view) owns the cursor.
        // Overlays or scrollback suppress it.
        if let Some(ref prompt) = state.prompt {
            render_name_prompt(frame, prompt, state.anim.frame());
        } else if state.search.open {
            // Search overlay — drawn on top of everything else and owns cursor.
            render_search_overlay(frame, state);
        } else if state.pid_inspect.is_none() {
            // No blocking overlay: propagate vt100 cursor from focused pane.
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot_idx = if let Some(ref zoom_path) = tab.zoomed {
                slot_at(&tab.root, zoom_path)
            } else {
                slot_at(&tab.root, &tab.focus_path)
            };
            if let Some(slot_idx) = focused_slot_idx {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    if slot.scroll_offset == 0 {
                        let vt_area = slot.last_screen_rect;
                        let cursor_pt = slot.term.grid().cursor.point;
                        let vt_row = cursor_pt.line.0.max(0) as u16;
                        let vt_col = cursor_pt.column.0 as u16;
                        let cursor_x = vt_area
                            .x
                            .saturating_add(vt_col)
                            .min(vt_area.x.saturating_add(vt_area.width).saturating_sub(1));
                        let cursor_y = vt_area
                            .y
                            .saturating_add(vt_row)
                            .min(vt_area.y.saturating_add(vt_area.height).saturating_sub(1));
                        frame.set_cursor_position((cursor_x, cursor_y));
                    }
                }
            }
        }
    })?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Ctrl-B prefix actions
// ─────────────────────────────────────────────────────────────────────────────

/// Cycle focus to the next leaf (DFS order), wrapping around.
/// `slots` is passed so dead/None leaves can be skipped.
fn focus_next(tab: &mut Tab, slots: &[Option<PaneSlot>], forward: bool) {
    let mut all_paths: Vec<Vec<usize>> = Vec::new();
    let mut tmp: Vec<usize> = Vec::new();
    leaves_in_order(&tab.root, &mut tmp, &mut all_paths);

    // Filter to only live (non-None) leaves.
    let live_paths: Vec<Vec<usize>> = all_paths
        .into_iter()
        .filter(|p| {
            slot_at(&tab.root, p)
                .and_then(|idx| slots.get(idx))
                .and_then(|s| s.as_ref())
                .is_some()
        })
        .collect();

    if live_paths.is_empty() {
        return;
    }

    let current_pos = live_paths
        .iter()
        .position(|p| p == &tab.focus_path)
        .unwrap_or(0);

    let next_pos = if forward {
        (current_pos + 1) % live_paths.len()
    } else {
        (current_pos + live_paths.len() - 1) % live_paths.len()
    };

    tab.focus_path = live_paths[next_pos].clone();
}

/// Split the active leaf. `horizontal` = true means HSplit (top/bottom).
async fn split_active(state: &mut AppState, horizontal: bool) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();
    let req = OpenPaneReq {
        session: session_id,
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
    };
    let new_pane_id = state
        .control
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;

    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    let new_slot_idx = state.slots.len();
    state.slots.push(Some(slot));

    let sv = state.active_session_view_mut();
    let tab = &mut sv.tabs[sv.active_tab];
    // Clear zoom before splitting.
    tab.zoomed = None;
    let old_path = tab.focus_path.clone();

    // Find the existing leaf slot index at the current focus path.
    let old_slot_idx = match slot_at(&tab.root, &old_path) {
        Some(idx) => idx,
        None => return Ok(()), // nothing to split
    };

    // Equal 50/50 weights for a two-child split.
    let new_node = if horizontal {
        LayoutNode::HSplit(vec![
            (LayoutNode::Leaf(old_slot_idx), 50),
            (LayoutNode::Leaf(new_slot_idx), 50),
        ])
    } else {
        LayoutNode::VSplit(vec![
            (LayoutNode::Leaf(old_slot_idx), 50),
            (LayoutNode::Leaf(new_slot_idx), 50),
        ])
    };

    replace_at(&mut tab.root, &old_path, new_node);

    // New focus: append child index 1 to old path to point at the new leaf.
    let mut new_focus = old_path;
    new_focus.push(1);
    tab.focus_path = new_focus;

    Ok(())
}

/// Open a new pane in a new tab within the active session.
/// `label` is stored client-side only; pass `None` to auto-number.
async fn open_new_tab(state: &mut AppState, label: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();
    let req = OpenPaneReq {
        session: session_id,
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
    };
    let new_pane_id = state
        .control
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;

    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    let slot_idx = state.slots.len();
    state.slots.push(Some(slot));

    let sv = state.active_session_view_mut();
    let tab_n = sv.tabs.len() + 1;
    let _label = label
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| format!("tab-{tab_n}"));
    sv.tabs.push(Tab {
        root: LayoutNode::Leaf(slot_idx),
        focus_path: vec![],
        zoomed: None,
        boundaries: Vec::new(),
        drag: None,
    });
    sv.active_tab = sv.tabs.len() - 1;

    Ok(())
}

/// Spawn a brand-new daemon session and push a SessionView.
async fn open_new_session(state: &mut AppState, name: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let resolved_name = name.filter(|n| !n.is_empty());
    let req = SpawnReq {
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: resolved_name.clone(),
    };
    let SpawnResp { session, pane } = state
        .control
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    let slot = attach_pane(&state.socket, session, pane, cols, rows).await?;
    let slot_idx = state.slots.len();
    state.slots.push(Some(slot));

    // Derive display name: use provided or fall back to session-<short8>.
    let short8: String = session.0.to_string().chars().take(8).collect();
    let display_name = resolved_name.unwrap_or_else(|| format!("session-{short8}"));

    state.sessions.push(SessionView {
        id: session,
        name: display_name,
        tabs: vec![Tab {
            root: LayoutNode::Leaf(slot_idx),
            focus_path: vec![],
            zoomed: None,
            boundaries: Vec::new(),
            drag: None,
        }],
        active_tab: 0,
    });
    state.active_session = state.sessions.len() - 1;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Mouse event handler
// ─────────────────────────────────────────────────────────────────────────────

/// Handle a mouse event. Returns true if the event was consumed.
fn handle_mouse(state: &mut AppState, me: crossterm::event::MouseEvent, body_area: Rect) -> bool {
    let col = me.column;
    let row = me.row;

    match me.kind {
        MouseEventKind::ScrollUp => {
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    if let Some(slot) = state.slots[*slot_idx].as_mut() {
                        slot.scroll_offset = (slot.scroll_offset + 3).min(slot.scrollback_capacity);
                    }
                    return true;
                }
            }
            false
        }
        MouseEventKind::ScrollDown => {
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    if let Some(slot) = state.slots[*slot_idx].as_mut() {
                        slot.scroll_offset = slot.scroll_offset.saturating_sub(3);
                    }
                    return true;
                }
            }
            false
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // row 0 = sessions strip; row 1 = tabs strip; body starts at row 2.
            if row == 0 {
                // Check [+] session button first.
                if let Some(plus_rect) = state.session_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewSession,
                            input: String::new(),
                        });
                        return true;
                    }
                }
                // Check session tab rects (cloned to avoid borrow issues).
                let session_rects = state.session_strip_rects.clone();
                for (sess_idx, rect) in &session_rects {
                    if rect_contains(*rect, col, row) {
                        state.active_session = *sess_idx;
                        return true;
                    }
                }
                return false;
            }

            if row == 1 {
                // Check [+] tab button.
                if let Some(plus_rect) = state.tab_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewTab,
                            input: String::new(),
                        });
                        return true;
                    }
                }
                // Check individual tab chips by computing widths inline.
                let sv = &state.sessions[state.active_session];
                let mut x: u16 = 0;
                for (i, _) in sv.tabs.iter().enumerate() {
                    let label_len = format!(" {} ", i + 1).len() as u16;
                    if col >= x && col < x + label_len {
                        state.sessions[state.active_session].active_tab = i;
                        return true;
                    }
                    x += label_len + 1; // +1 for separator space
                }
                return false;
            }

            // Check if clicking near a split boundary to start a drag.
            {
                let sv = &mut state.sessions[state.active_session];
                let tab = &mut sv.tabs[sv.active_tab];
                for boundary in tab.boundaries.clone() {
                    let hit = if boundary.is_hsplit {
                        row.abs_diff(boundary.coord) <= 1
                    } else {
                        col.abs_diff(boundary.coord) <= 1
                    };
                    if hit {
                        let start_coord = if boundary.is_hsplit { row } else { col };
                        let start_weights: Vec<u16> = if let Some(children) =
                            children_at_mut(&mut tab.root, &boundary.parent_path)
                        {
                            children.iter().map(|(_, w)| *w).collect()
                        } else {
                            continue;
                        };
                        tab.drag = Some(DragState {
                            boundary,
                            start_coord,
                            start_weights,
                        });
                        return true;
                    }
                }
            }

            // Check if clicking inside a leaf pane — also start text selection.
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    // Compute (row, col) relative to the slot's content area.
                    // The content area inner rect is stored in last_screen_rect.
                    if let Some(slot) = state.slots[*slot_idx].as_ref() {
                        let content = slot.last_screen_rect;
                        if rect_contains(content, col, row) {
                            let sel_row = row.saturating_sub(content.y);
                            let sel_col = col.saturating_sub(content.x);
                            state.selection = Some(Selection {
                                pane_idx: *slot_idx,
                                start: (sel_row, sel_col),
                                end: (sel_row, sel_col),
                                dragging: true,
                                base: SelectionBase::Live,
                            });
                        }
                    }
                    return true;
                }
            }
            false
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            // Split-resize drag takes priority.
            if let Some(ref drag) = tab.drag {
                let cur_coord = if drag.boundary.is_hsplit { row } else { col };
                let delta = cur_coord as i32 - drag.start_coord as i32;
                let parent_size = drag.boundary.parent_size.max(1) as i32;
                let delta_pct = (delta * 100) / parent_size;
                let idx = drag.boundary.child_idx;
                let mut new_weights = drag.start_weights.clone();

                if idx + 1 < new_weights.len() {
                    let left = new_weights[idx] as i32 + delta_pct;
                    let right = new_weights[idx + 1] as i32 - delta_pct;
                    let left = left.clamp(5, (left + right - 5).max(5)) as u16;
                    let right = (new_weights[idx] as i32 + new_weights[idx + 1] as i32
                        - left as i32)
                        .clamp(5, i32::MAX) as u16;
                    new_weights[idx] = left;
                    new_weights[idx + 1] = right;

                    let parent_path = drag.boundary.parent_path.clone();
                    if let Some(children) = children_at_mut(&mut tab.root, &parent_path) {
                        for (i, w) in new_weights.iter().enumerate() {
                            if i < children.len() {
                                children[i].1 = *w;
                            }
                        }
                    }
                }
                return true;
            }
            // Text selection drag: update end point if selection is active.
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    if let Some(slot) = state.slots[sel.pane_idx].as_ref() {
                        let content = slot.last_screen_rect;
                        if rect_contains(content, col, row) {
                            sel.end =
                                (row.saturating_sub(content.y), col.saturating_sub(content.x));
                            return true;
                        }
                    }
                }
            }
            false
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            if tab.drag.is_some() {
                tab.drag = None;
                return true;
            }
            // Finish text selection: copy to clipboard.
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    sel.dragging = false;
                    // Extract selected text from the alacritty grid via per-cell iteration.
                    let pane_idx = sel.pane_idx;
                    let ((r0, c0), (r1, c1)) = sel.normalized();
                    if let Some(slot) = state.slots[pane_idx].as_ref() {
                        let grid = slot.term.grid();
                        let num_cols = grid.columns();
                        let mut text = String::new();
                        for row in r0..=r1 {
                            if row > r0 {
                                text.push('\n');
                            }
                            let col_start = if row == r0 { c0 as usize } else { 0usize };
                            let col_end = if row == r1 { c1 as usize } else { num_cols };
                            for col in col_start..=col_end {
                                let pt = TermPoint::new(TermLine(row as i32), TermColumn(col));
                                let ch = grid[pt].c;
                                if ch == '\0' {
                                    text.push(' ');
                                } else {
                                    text.push(ch);
                                }
                            }
                        }
                        // Trim trailing whitespace from each line.
                        let trimmed: String = text
                            .lines()
                            .map(|l| l.trim_end())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !trimmed.is_empty() {
                            if let Err(e) = crate::clipboard::copy_to_clipboard(&trimmed) {
                                tracing::warn!("clipboard copy failed: {e}");
                            }
                        }
                    }
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Poll the daemon for a pending `pyrec select-pane` focus request and apply it.
///
/// Replaces the former dropfile (`focus.request`) approach with an RPC call so
/// that concurrent `pyrec select-pane` invocations are handled atomically.
async fn apply_focus_request(state: &mut AppState) {
    let Ok(Ok(Some(pane_str))) = state
        .control
        .take_focus_request(tarpc::context::current())
        .await
    else {
        return;
    };
    let Ok(pane_uuid) = uuid::Uuid::parse_str(&pane_str) else {
        return;
    };
    let pane_id = PaneId(pane_uuid);
    if let Some(slot_idx) = state
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|slot| slot.pane_id == pane_id))
    {
        focus_slot(state, slot_idx);
        let short: String = pane_str.chars().take(8).collect();
        state.status_msg = Some(format!("focused pane {short} (select-pane)"));
    } else {
        state.status_msg = Some(format!(
            "select-pane: pane {pane_str} not open in this TUI — attach it first"
        ));
    }
}

/// Update active session's active tab focus_path to point at the given slot index.
fn focus_slot(state: &mut AppState, target_slot_idx: usize) {
    // Collect candidate paths first to avoid simultaneous borrow of sessions + slots.
    let all_paths = {
        let sv = &state.sessions[state.active_session];
        let tab = &sv.tabs[sv.active_tab];
        let mut paths: Vec<Vec<usize>> = Vec::new();
        let mut tmp: Vec<usize> = Vec::new();
        leaves_in_order(&tab.root, &mut tmp, &mut paths);
        paths
    };
    let chosen = {
        let sv = &state.sessions[state.active_session];
        let tab = &sv.tabs[sv.active_tab];
        all_paths.into_iter().find(|path| {
            slot_at(&tab.root, path)
                .map(|idx| {
                    idx == target_slot_idx
                        && state.slots.get(idx).and_then(|s| s.as_ref()).is_some()
                })
                .unwrap_or(false)
        })
    };
    if let Some(path) = chosen {
        let sv = &mut state.sessions[state.active_session];
        let tab = &mut sv.tabs[sv.active_tab];
        tab.focus_path = path;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane close / layout collapse
// ─────────────────────────────────────────────────────────────────────────────

/// Remove the leaf at `leaf_path` from the layout tree, collapsing the parent
/// split if it becomes a single child.  Returns the slot index that was removed.
fn remove_leaf(root: &mut LayoutNode, leaf_path: &[usize]) -> Option<usize> {
    if leaf_path.is_empty() {
        // Root is a Leaf — replace with a sentinel; caller handles.
        if let LayoutNode::Leaf(idx) = root {
            return Some(*idx);
        }
        return None;
    }

    let parent_path = &leaf_path[..leaf_path.len() - 1];
    let child_idx = leaf_path[leaf_path.len() - 1];

    // Navigate to parent, remove child, collapse if single child remains.
    fn remove_in(node: &mut LayoutNode, path: &[usize], child_idx: usize) -> Option<usize> {
        if path.is_empty() {
            let removed = match node {
                LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                    if child_idx >= children.len() {
                        return None;
                    }
                    let removed_slot = match &children[child_idx].0 {
                        LayoutNode::Leaf(idx) => *idx,
                        _ => return None, // only remove leaves
                    };
                    children.remove(child_idx);
                    // Re-normalize weights to sum to 100.
                    let n = children.len() as u16;
                    if let Some(each) = 100u16.checked_div(n) {
                        let remainder = 100 - each * n;
                        for (i, (_, w)) in children.iter_mut().enumerate() {
                            *w = each + if i == 0 { remainder } else { 0 };
                        }
                    }
                    Some(removed_slot)
                }
                _ => None,
            };
            // Collapse single-child split into its child.
            let collapse = match node {
                LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => children.len() == 1,
                _ => false,
            };
            if collapse {
                let child = match node {
                    LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                        children.remove(0).0
                    }
                    _ => unreachable!(),
                };
                *node = child;
            }
            return removed;
        }
        match node {
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                if path[0] >= children.len() {
                    return None;
                }
                remove_in(&mut children[path[0]].0, &path[1..], child_idx)
            }
            _ => None,
        }
    }

    remove_in(root, parent_path, child_idx)
}

/// Locate the (session_idx, tab_idx, leaf_path) for a given slot index.
fn locate_slot(state: &AppState, target: usize) -> Option<(usize, usize, Vec<usize>)> {
    for (si, sess) in state.sessions.iter().enumerate() {
        for (ti, tab) in sess.tabs.iter().enumerate() {
            let mut paths: Vec<Vec<usize>> = Vec::new();
            let mut tmp: Vec<usize> = Vec::new();
            leaves_in_order(&tab.root, &mut tmp, &mut paths);
            for path in paths {
                if slot_at(&tab.root, &path) == Some(target) {
                    return Some((si, ti, path));
                }
            }
        }
    }
    None
}

/// Close a pane by its slot index.
/// Removes the leaf from the layout tree, drops the slot, cascades tab/session removal.
fn close_pane_by_slot_idx(state: &mut AppState, slot_idx: usize) {
    let (sess_idx, tab_idx, focus_path) = match locate_slot(state, slot_idx) {
        Some(loc) => loc,
        None => return,
    };

    // Extract the pane_id before we drop the slot so we can fire the close RPC.
    let pane_id = state
        .slots
        .get(slot_idx)
        .and_then(|s| s.as_ref())
        .map(|s| s.pane_id);

    // Fire close_pane RPC fire-and-forget so the daemon evicts the pane.
    if let Some(pid) = pane_id {
        let client = state.control.clone();
        tokio::runtime::Handle::current().spawn(async move {
            let _ = client.close_pane(tarpc::context::current(), pid).await;
        });
    }

    // Special case: root itself is the only leaf (no splits).  remove_leaf
    // returns the slot index but cannot mutate root in this case, so
    // leaves_in_order would still find the leaf and the tab-removal branch
    // would never fire.  Bypass remove_leaf and go straight to tab removal.
    let root_is_leaf = focus_path.is_empty()
        && matches!(
            &state.sessions[sess_idx].tabs[tab_idx].root,
            LayoutNode::Leaf(_)
        );

    if root_is_leaf {
        if slot_idx < state.slots.len() {
            state.slots[slot_idx] = None;
        }
        state.sessions[sess_idx].tabs.remove(tab_idx);
        if state.sessions[sess_idx].tabs.is_empty() {
            state.sessions.remove(sess_idx);
            if state.sessions.is_empty() {
                return;
            }
            state.active_session = state.active_session.min(state.sessions.len() - 1);
        } else {
            state.sessions[sess_idx].active_tab =
                tab_idx.min(state.sessions[sess_idx].tabs.len() - 1);
            let new_tab_idx = state.sessions[sess_idx].active_tab;
            let mut paths: Vec<Vec<usize>> = Vec::new();
            let mut t: Vec<usize> = Vec::new();
            leaves_in_order(
                &state.sessions[sess_idx].tabs[new_tab_idx].root,
                &mut t,
                &mut paths,
            );
            state.sessions[sess_idx].tabs[new_tab_idx].focus_path =
                paths.into_iter().next().unwrap_or_default();
        }
        return;
    }

    // Remove the leaf from the layout tree.
    let removed = remove_leaf(
        &mut state.sessions[sess_idx].tabs[tab_idx].root,
        &focus_path,
    );
    if removed.is_none() {
        return;
    }

    // Drop the slot.
    if slot_idx < state.slots.len() {
        state.slots[slot_idx] = None;
    }

    // Check if the tab now has no leaves.
    let mut remaining_paths: Vec<Vec<usize>> = Vec::new();
    let mut tmp: Vec<usize> = Vec::new();
    leaves_in_order(
        &state.sessions[sess_idx].tabs[tab_idx].root,
        &mut tmp,
        &mut remaining_paths,
    );

    if remaining_paths.is_empty() {
        // Tab is empty — remove it.
        state.sessions[sess_idx].tabs.remove(tab_idx);
        if state.sessions[sess_idx].tabs.is_empty() {
            // Session has no tabs — remove session view.
            state.sessions.remove(sess_idx);
            if state.sessions.is_empty() {
                // No sessions left — nothing to do; caller should exit.
                return;
            }
            state.active_session = state.active_session.min(state.sessions.len() - 1);
        } else {
            state.sessions[sess_idx].active_tab =
                tab_idx.min(state.sessions[sess_idx].tabs.len() - 1);
            // Reset focus to first leaf of new active tab.
            let new_tab_idx = state.sessions[sess_idx].active_tab;
            let mut paths: Vec<Vec<usize>> = Vec::new();
            let mut t: Vec<usize> = Vec::new();
            leaves_in_order(
                &state.sessions[sess_idx].tabs[new_tab_idx].root,
                &mut t,
                &mut paths,
            );
            state.sessions[sess_idx].tabs[new_tab_idx].focus_path =
                paths.into_iter().next().unwrap_or_default();
        }
    } else {
        // Tab still has leaves — point focus at first remaining leaf.
        let new_focus = remaining_paths.into_iter().next().unwrap_or_default();
        state.sessions[sess_idx].tabs[tab_idx].focus_path = new_focus;
        state.sessions[sess_idx].tabs[tab_idx].zoomed = None;
    }
}

/// Close the focused pane in the active tab.
fn close_focused_pane(state: &mut AppState) {
    let sess_idx = state.active_session;
    let tab_idx = state.sessions[sess_idx].active_tab;
    let focus_path = state.sessions[sess_idx].tabs[tab_idx].focus_path.clone();
    if let Some(slot_idx) = slot_at(&state.sessions[sess_idx].tabs[tab_idx].root, &focus_path) {
        close_pane_by_slot_idx(state, slot_idx);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main TUI loop
// ─────────────────────────────────────────────────────────────────────────────

/// Build an `AppState` from one already-attached initial session/pane.
fn initial_app_state(
    session: SessionId,
    session_name: String,
    initial_slot: PaneSlot,
    control: PyreDaemonClient,
    socket: PathBuf,
    shell: Option<String>,
    blocks_rx: watch::Receiver<HashMap<PaneId, Vec<Block>>>,
) -> AppState {
    AppState {
        sessions: vec![SessionView {
            id: session,
            name: session_name,
            tabs: vec![Tab {
                root: LayoutNode::Leaf(0),
                focus_path: vec![],
                zoomed: None,
                boundaries: Vec::new(),
                drag: None,
            }],
            active_tab: 0,
        }],
        active_session: 0,
        slots: vec![Some(initial_slot)],
        control,
        socket,
        shell,
        search: SearchState::default(),
        status_msg: None,
        sidebar_open: false,
        sidebar_data: Vec::new(),
        sidebar_last_poll: Instant::now() - Duration::from_secs(10),
        sidebar_cursor: 0,
        sidebar_focused: false,
        selection: None,
        last_click: None,
        context_menu: None,
        pid_inspect: None,
        prompt: None,
        session_strip_rects: Vec::new(),
        session_plus_rect: None,
        tab_plus_rect: None,
        pending_resizes: Vec::new(),
        // Force an immediate session-list sync on the first loop iteration.
        session_list_last_poll: Instant::now() - Duration::from_secs(10),
        blocks_rx,
        anim: AnimClock::new(),
    }
}

/// Describes how `run_tui` should acquire the initial session and pane.
///
/// Spawning the PTY must happen AFTER the terminal enters alternate-screen so
/// that `terminal.size()` returns true dimensions.  Passing intent here
/// (instead of pre-spawning in `main`) prevents the shell from starting at
/// the 80×24 placeholder that `crossterm::terminal::size()` returns before
/// alt-screen is entered.
enum PaneInit {
    /// Session and pane already exist (e.g. `pyre attach`).
    Existing {
        session: SessionId,
        session_name: String,
        pane: PaneId,
    },
    /// No sessions exist (or all existing sessions are stale); spawn a fresh session+pane at real terminal size.
    Spawn,
}

async fn run_tui(
    socket: PathBuf,
    init: PaneInit,
    control: PyreDaemonClient,
    shell: Option<String>,
) -> Result<()> {
    // Enter alternate screen FIRST so that crossterm::terminal::size() returns
    // the full terminal dimensions when we compute the initial pane size below.
    // Any size query before this point reflects the pre-alt-screen scroll-region
    // height (typically 25 rows on most terminals), not the real window height.
    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // Now that we are in alternate-screen mode, ratatui's terminal.size() gives
    // the true frame dimensions. Use those to compute the initial inner pane area
    // so the PTY is spawned at the right size from the very first frame.
    let term_rect = terminal.size()?;
    let (init_cols, init_rows) = compute_pane_inner_size(term_rect.width, term_rect.height);

    // Resolve session/pane — spawning here (post-alt-screen) ensures the shell
    // is started at real terminal dimensions, not an 80×24 placeholder.
    let (session, session_name, pane) = match init {
        PaneInit::Existing {
            session,
            session_name,
            pane,
        } => (session, session_name, pane),
        PaneInit::Spawn => {
            let req = SpawnReq {
                shell: shell.clone(),
                cwd: std::env::current_dir().ok(),
                cols: init_cols,
                rows: init_rows,
                env: std::env::vars().collect(),
                name: None,
            };
            let SpawnResp { session, pane } = control
                .spawn(tarpc::context::current(), req)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon spawn: {e}"))?;
            let short8: String = session.0.to_string().chars().take(8).collect();
            (session, format!("session-{short8}"), pane)
        }
    };

    let mut initial_slot = attach_pane(&socket, session, pane, init_cols, init_rows).await?;

    // Pre-populate the block ribbon for the initial pane so that Ctrl-B [
    // shows previous command history immediately on reattach (S3).
    match tokio::time::timeout(
        Duration::from_secs(2),
        control.replay(tarpc::context::current(), pane, 20),
    )
    .await
    {
        Ok(Ok(Ok(replay))) => {
            if !replay.recent.is_empty() {
                tracing::debug!(
                    pane_id = %pane.0,
                    blocks = replay.recent.len(),
                    "reattach: pre-populated block ribbon"
                );
                initial_slot.recent_blocks = replay.recent;
            }
        }
        Ok(Ok(Err(e))) => tracing::debug!(pane_id = %pane.0, "replay rpc error (non-fatal): {e}"),
        Ok(Err(_)) => tracing::debug!(pane_id = %pane.0, "replay transport error (non-fatal)"),
        Err(_) => tracing::debug!(pane_id = %pane.0, "replay rpc timeout (non-fatal)"),
    }

    // ── Background block-poll task (Bug 2 fix) ──────────────────────────────
    // list_blocks is moved off the hot event loop into its own task. A
    // watch channel carries the latest snapshot; the event loop does a
    // non-blocking borrow_and_update read and never awaits the RPC directly.
    let (blocks_tx, blocks_rx) = watch::channel(HashMap::<PaneId, Vec<Block>>::new());
    {
        let poll_client = control.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let req = pyre_proto::blocks::ListBlocksReq {
                    session: None,
                    limit: 20,
                };
                // Apply a 2 s hard timeout so a stuck daemon cannot pin this task.
                let result = tokio::time::timeout(
                    Duration::from_secs(2),
                    poll_client.list_blocks(tarpc::context::current(), req),
                )
                .await;
                if let Ok(Ok(Ok(blocks))) = result {
                    // Group by pane_id so the event loop can index directly.
                    let mut map: HashMap<PaneId, Vec<Block>> = HashMap::new();
                    for b in blocks {
                        map.entry(b.pane).or_default().push(b);
                    }
                    // send only fails when all receivers are dropped (TUI exited).
                    if blocks_tx.send(map).is_err() {
                        break;
                    }
                }
            }
        });
    }

    let mut state = initial_app_state(
        session,
        session_name,
        initial_slot,
        control,
        socket,
        shell,
        blocks_rx,
    );

    // Eagerly discover all other sessions the daemon already knows about so
    // the top bar is populated before the first draw, not 1 s later.
    // Re-read terminal size here — we are already in alt-screen so the value is authoritative.
    if let Ok(Ok(daemon_sessions)) = state.control.list_sessions(tarpc::context::current()).await {
        let eager_rect = terminal.size().unwrap_or(term_rect);
        let (ec, er) = compute_pane_inner_size(eager_rect.width, eager_rect.height);
        for info in daemon_sessions {
            if info.id == session {
                continue; // already the active session
            }
            if let Ok(Ok(panes)) = state
                .control
                .list_panes(tarpc::context::current(), info.id)
                .await
            {
                if let Some(p) = panes.into_iter().next() {
                    if let Ok(mut slot) = attach_pane(&state.socket, info.id, p.id, ec, er).await {
                        // Pre-populate block ribbon for eagerly-attached panes.
                        if let Ok(Ok(Ok(replay))) = tokio::time::timeout(
                            Duration::from_secs(2),
                            state.control.replay(tarpc::context::current(), p.id, 20),
                        )
                        .await
                        {
                            if !replay.recent.is_empty() {
                                slot.recent_blocks = replay.recent;
                            }
                        }
                        let slot_idx = state.slots.len();
                        state.slots.push(Some(slot));
                        state.sessions.push(SessionView {
                            id: info.id,
                            name: info.name,
                            tabs: vec![Tab {
                                root: LayoutNode::Leaf(slot_idx),
                                focus_path: vec![],
                                zoomed: None,
                                boundaries: Vec::new(),
                                drag: None,
                            }],
                            active_tab: 0,
                        });
                    }
                }
            }
        }
        // Mark poll time so the in-loop poll won't fire again for 1 s.
        state.session_list_last_poll = Instant::now();
    }

    let mut prefix_active = false;

    // Observability counters for the 1 s periodic debug log.
    let mut loop_frames_drawn: u64 = 0;
    let mut loop_bytes_processed: u64 = 0;
    let mut loop_stats_at = Instant::now();

    loop {
        // Drain all pane output into their parsers and scrollback buffers.
        // Collect (slot_idx, frames_received) for Closed events so we can
        // decide whether to fire close_pane RPC after the borrow ends.
        let mut closed_slots: Vec<(usize, u64)> = Vec::new();
        for (slot_idx, slot_opt) in state.slots.iter_mut().enumerate() {
            if let Some(slot) = slot_opt {
                while let Ok(event) = slot.output_rx.try_recv() {
                    match event {
                        PaneEvent::Output(data) => {
                            slot.frames_received += 1;
                            loop_bytes_processed += data.len() as u64;
                            slot.process_output(&data);
                            // Forward any CPR/DSR responses generated by the
                            // terminal emulator (e.g. ?1000h, cursor position
                            // reports) back to the child PTY so nested TUIs
                            // don't hang waiting for a reply.
                            let responses = slot.drain_pty_responses();
                            if !responses.is_empty() {
                                let _ = slot.input_tx.try_send(Bytes::from(responses));
                            }
                        }
                        PaneEvent::Closed { frames_received } => {
                            closed_slots.push((slot_idx, frames_received));
                            // Stop draining this pane; it will be removed below.
                            break;
                        }
                    }
                }
            }
        }
        for (slot_idx, frames_received) in closed_slots {
            if frames_received == 0 {
                // The stream was rejected before any output arrived — the
                // worker did not have this pane (e.g. "pane not found").
                // Firing close_pane here would tell the worker to close a
                // slot it never opened, causing it to exit and triggering a
                // supervisor respawn loop.  Just remove the TUI slot; the
                // session-sync loop will reconcile daemon state on the next tick.
                tracing::warn!(
                    slot_idx,
                    "stream closed with 0 frames; skipping close_pane RPC"
                );
                if slot_idx < state.slots.len() {
                    state.slots[slot_idx] = None;
                }
            } else {
                close_pane_by_slot_idx(&mut state, slot_idx);
            }
        }

        // Drain latest block snapshot from the background poll task (non-blocking).
        // borrow_and_update marks the value as seen; subsequent calls return
        // Err(RecvError) until the background task publishes a new snapshot.
        if state.blocks_rx.has_changed().unwrap_or(false) {
            let map = state.blocks_rx.borrow_and_update().clone();
            for slot in state.slots.iter_mut().flatten() {
                let pane_id = slot.pane_id;
                if let Some(blocks) = map.get(&pane_id) {
                    slot.recent_blocks = blocks.clone();
                    if let Some(cursor) = slot.ribbon_cursor {
                        if !slot.recent_blocks.is_empty() {
                            slot.ribbon_cursor = Some(cursor.min(slot.recent_blocks.len() - 1));
                        } else {
                            slot.ribbon_cursor = None;
                        }
                    }
                } else {
                    // Pane not present in latest snapshot — clear stale data.
                    slot.recent_blocks.clear();
                    slot.ribbon_cursor = None;
                }
            }
        }

        // Search debounce: fire query 150 ms after last keystroke.
        if state.search.open
            && state.search.pending_query.is_some()
            && state.search.last_query_at.elapsed() >= Duration::from_millis(150)
        {
            let raw = state.search.pending_query.take().expect("checked Some");
            let (query, failures_only) = parse_search_input(&raw);
            state.search.failures_only = failures_only;
            let (tx, rx) = mpsc::channel::<Vec<BlockHit>>(1);
            state.search.rx = Some(rx);
            let client = state.control.clone();
            let req = SearchBlocksReq {
                query,
                limit: 20,
                failures_only,
            };
            tokio::spawn(async move {
                let hits = client
                    .search_blocks(tarpc::context::current(), req)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let _ = tx.send(hits).await;
            });
        }

        // Drain search results channel.
        if let Some(ref mut rx) = state.search.rx {
            if let Ok(hits) = rx.try_recv() {
                state.search.results = hits;
                state.search.cursor = 0;
            }
        }

        apply_focus_request(&mut state).await;

        // Sidebar poll — 1s when open, up to 50 panes.
        if state.sidebar_open && state.sidebar_last_poll.elapsed() >= Duration::from_secs(1) {
            state.sidebar_last_poll = Instant::now();
            if let Ok(Ok(mut panes)) = state
                .control
                .list_all_panes(tarpc::context::current())
                .await
            {
                panes.truncate(50);
                panes.sort_by(|a, b| {
                    session_name_for(&state, a.session)
                        .cmp(&session_name_for(&state, b.session))
                        .then_with(|| a.id.0.cmp(&b.id.0))
                });
                state.sidebar_data = panes;
                state.sidebar_cursor = state
                    .sidebar_cursor
                    .min(state.sidebar_data.len().saturating_sub(1));
            }
        }

        // Session-list sync — 1s poll to discover sessions created by other clients
        // (e.g. pyre_mcp::session_spawn) or to prune sessions removed elsewhere.
        if state.session_list_last_poll.elapsed() >= Duration::from_secs(1) {
            state.session_list_last_poll = Instant::now();
            if let Ok(Ok(daemon_sessions)) =
                state.control.list_sessions(tarpc::context::current()).await
            {
                // Add sessions that appeared in the daemon but are unknown to TUI.
                let known_ids: Vec<SessionId> = state.sessions.iter().map(|s| s.id).collect();
                for info in &daemon_sessions {
                    if !known_ids.contains(&info.id) {
                        // Attach to the first pane of the new session (if any).
                        match state
                            .control
                            .list_panes(tarpc::context::current(), info.id)
                            .await
                        {
                            Ok(Ok(panes)) if !panes.is_empty() => {
                                let pane_id = panes[0].id;
                                let (sc, sr) = {
                                    let (tc, tr) = term_size();
                                    compute_pane_inner_size(tc, tr)
                                };
                                match attach_pane(&state.socket, info.id, pane_id, sc, sr).await {
                                    Ok(slot) => {
                                        let slot_idx = state.slots.len();
                                        state.slots.push(Some(slot));
                                        state.sessions.push(SessionView {
                                            id: info.id,
                                            name: info.name.clone(),
                                            tabs: vec![Tab {
                                                root: LayoutNode::Leaf(slot_idx),
                                                focus_path: vec![],
                                                zoomed: None,
                                                boundaries: Vec::new(),
                                                drag: None,
                                            }],
                                            active_tab: 0,
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "session-sync: attach_pane for session {} failed: {e}",
                                            info.id
                                        );
                                    }
                                }
                            }
                            _ => {
                                // Session has no panes yet; skip until it does.
                            }
                        }
                    }
                }

                // Sync panes within sessions that already exist in both TUI and
                // daemon — handles panes added by external clients (e.g. MCP
                // pane_open) after the session was first attached.
                for info in &daemon_sessions {
                    // Only sessions the TUI already knows about.
                    let sv_idx = match state.sessions.iter().position(|s| s.id == info.id) {
                        Some(i) => i,
                        None => continue,
                    };

                    // Collect all pane IDs currently tracked in this SessionView.
                    let local_pane_ids: Vec<PaneId> = {
                        let sv = &state.sessions[sv_idx];
                        let mut ids = Vec::new();
                        for tab in &sv.tabs {
                            let mut tmp = Vec::new();
                            let mut paths: Vec<Vec<usize>> = Vec::new();
                            leaves_in_order(&tab.root, &mut tmp, &mut paths);
                            for path in &paths {
                                if let Some(slot_idx) = slot_at(&tab.root, path) {
                                    if let Some(Some(slot)) = state.slots.get(slot_idx) {
                                        ids.push(slot.pane_id);
                                    }
                                }
                            }
                        }
                        ids
                    };

                    // Ask daemon which panes belong to this session.
                    let daemon_panes = match state
                        .control
                        .list_panes(tarpc::context::current(), info.id)
                        .await
                    {
                        Ok(Ok(p)) => p,
                        _ => continue,
                    };

                    // Attach panes the daemon knows about but TUI does not.
                    for pane_info in &daemon_panes {
                        if local_pane_ids.contains(&pane_info.id) {
                            continue;
                        }
                        let (pc, pr) = {
                            let (tc, tr) = term_size();
                            compute_pane_inner_size(tc, tr)
                        };
                        match attach_pane(&state.socket, info.id, pane_info.id, pc, pr).await {
                            Ok(slot) => {
                                let slot_idx = state.slots.len();
                                state.slots.push(Some(slot));
                                // Add as a new tab in the existing session, mirroring
                                // open_new_tab's plumbing (new leaf, no split).
                                let sv = &mut state.sessions[sv_idx];
                                let tab_n = sv.tabs.len() + 1;
                                sv.tabs.push(Tab {
                                    root: LayoutNode::Leaf(slot_idx),
                                    focus_path: vec![],
                                    zoomed: None,
                                    boundaries: Vec::new(),
                                    drag: None,
                                });
                                tracing::info!(
                                    "pane-sync: attached new pane {} to session {} as tab-{}",
                                    pane_info.id,
                                    info.id,
                                    tab_n,
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "pane-sync: attach_pane for pane {} in session {} failed: {e}",
                                    pane_info.id,
                                    info.id,
                                );
                            }
                        }
                    }

                    // Prune panes the daemon no longer reports (they closed externally).
                    let daemon_ids_for_session: Vec<PaneId> =
                        daemon_panes.iter().map(|p| p.id).collect();
                    // Collect slot indices to null-out for panes that vanished.
                    let slots_to_drop: Vec<usize> = {
                        let sv = &state.sessions[sv_idx];
                        let mut to_drop = Vec::new();
                        for tab in &sv.tabs {
                            let mut tmp = Vec::new();
                            let mut paths: Vec<Vec<usize>> = Vec::new();
                            leaves_in_order(&tab.root, &mut tmp, &mut paths);
                            for path in &paths {
                                if let Some(slot_idx) = slot_at(&tab.root, path) {
                                    if let Some(Some(slot)) = state.slots.get(slot_idx) {
                                        if !daemon_ids_for_session.contains(&slot.pane_id) {
                                            to_drop.push(slot_idx);
                                        }
                                    }
                                }
                            }
                        }
                        to_drop
                    };
                    for slot_idx in slots_to_drop {
                        state.slots[slot_idx] = None;
                    }
                }

                // Prune sessions that disappeared from the daemon.
                let daemon_ids: Vec<SessionId> = daemon_sessions.iter().map(|s| s.id).collect();
                // Collect indices to remove (in reverse order so removal is safe).
                let to_remove: Vec<usize> = state
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, sv)| !daemon_ids.contains(&sv.id))
                    .map(|(i, _)| i)
                    .collect();
                for &idx in to_remove.iter().rev() {
                    state.sessions.remove(idx);
                    if state.active_session >= state.sessions.len() && !state.sessions.is_empty() {
                        state.active_session = state.sessions.len() - 1;
                    }
                }
            }
        }

        // Guard: exit immediately if every session was removed by any code path.
        if state.sessions.is_empty() {
            break;
        }

        state.anim.tick();
        // Draw — pass state as mut so render_pane can store last_screen_rect.
        draw_frame(&mut terminal, &mut state, prefix_active)?;
        loop_frames_drawn += 1;

        // 1 s periodic stats log.
        if loop_stats_at.elapsed() >= Duration::from_secs(1) {
            tracing::debug!(
                frames_drawn = loop_frames_drawn,
                bytes_processed = loop_bytes_processed,
                "event-loop: 1s stats"
            );
            loop_frames_drawn = 0;
            loop_bytes_processed = 0;
            loop_stats_at = Instant::now();
        }

        // Drain pending resize RPCs collected by render_pane (fire-and-forget).
        let resizes = std::mem::take(&mut state.pending_resizes);
        if !resizes.is_empty() {
            let client = state.control.clone();
            tokio::spawn(async move {
                for (pane_id, size) in resizes {
                    let req = pyre_proto::ResizePaneReq { pane_id, size };
                    let _ = client.resize_pane(tarpc::context::current(), req).await;
                }
            });
        }

        // Poll crossterm events (~16 ms = 60 fps)
        if !crossterm::event::poll(Duration::from_millis(16))? {
            continue;
        }

        // Compute body_area for mouse hit-tests (mirrors draw_frame layout).
        // Now: row0=sessions, row1=tabs, rows2..N-1=body, rowN=status.
        let term_size_rect = terminal.size()?;
        let outer_rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(term_size_rect.into());
        let body_area = outer_rects[2];

        match crossterm::event::read()? {
            Event::Mouse(me) => {
                handle_mouse(&mut state, me, body_area);
            }

            Event::Key(key_event) => {
                let code = key_event.code;
                let mods = key_event.modifiers;

                // Name-prompt intercepts all keys when open.
                if state.prompt.is_some() {
                    match code {
                        KeyCode::Esc => {
                            state.prompt = None;
                        }
                        KeyCode::Backspace => {
                            if let Some(ref mut p) = state.prompt {
                                p.input.pop();
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(p) = state.prompt.take() {
                                let input = if p.input.is_empty() {
                                    None
                                } else {
                                    Some(p.input)
                                };
                                match p.kind {
                                    PromptKind::NewSession => {
                                        if let Err(e) = open_new_session(&mut state, input).await {
                                            tracing::warn!("open_new_session failed: {e}");
                                        }
                                    }
                                    PromptKind::NewTab => {
                                        if let Err(e) = open_new_tab(&mut state, input).await {
                                            tracing::warn!("open_new_tab failed: {e}");
                                        }
                                    }
                                    PromptKind::RenameSession(session_id) => {
                                        if let Some(new_name) = input {
                                            match state
                                                .control
                                                .rename_session(
                                                    tarpc::context::current(),
                                                    session_id,
                                                    new_name.clone(),
                                                )
                                                .await
                                            {
                                                Ok(Ok(())) => {
                                                    // Update local view immediately.
                                                    if let Some(sv) = state
                                                        .sessions
                                                        .iter_mut()
                                                        .find(|s| s.id == session_id)
                                                    {
                                                        sv.name = new_name;
                                                    }
                                                }
                                                Ok(Err(e)) => {
                                                    tracing::warn!("rename_session rpc error: {e}");
                                                    state.status_msg =
                                                        Some(format!("rename failed: {e}"));
                                                }
                                                Err(e) => {
                                                    tracing::warn!("rename_session transport: {e}");
                                                    state.status_msg =
                                                        Some(format!("rename rpc: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(ref mut p) = state.prompt {
                                p.input.push(c);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // Detect Ctrl-B prefix
                if !prefix_active
                    && mods.contains(KeyModifiers::CONTROL)
                    && matches!(code, KeyCode::Char('b'))
                {
                    prefix_active = true;
                    continue;
                }

                if prefix_active {
                    prefix_active = false;
                    match code {
                        KeyCode::Char('q') => break,

                        KeyCode::Char('c') => {
                            if let Err(e) = open_new_tab(&mut state, None).await {
                                tracing::warn!("open_new_tab failed: {e}");
                            }
                        }

                        KeyCode::Char('n') => {
                            let sv = state.active_session_view_mut();
                            sv.active_tab = (sv.active_tab + 1) % sv.tabs.len();
                        }

                        KeyCode::Char('p') => {
                            let sv = state.active_session_view_mut();
                            sv.active_tab = (sv.active_tab + sv.tabs.len() - 1) % sv.tabs.len();
                        }

                        KeyCode::Char('"') => {
                            if let Err(e) = split_active(&mut state, true).await {
                                tracing::warn!("HSplit failed: {e}");
                            }
                        }

                        KeyCode::Char('%') => {
                            if let Err(e) = split_active(&mut state, false).await {
                                tracing::warn!("VSplit failed: {e}");
                            }
                        }

                        KeyCode::Right | KeyCode::Down => {
                            let si = state.active_session;
                            let ti = state.sessions[si].active_tab;
                            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, true);
                        }

                        KeyCode::Left | KeyCode::Up => {
                            let si = state.active_session;
                            let ti = state.sessions[si].active_tab;
                            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, false);
                        }

                        // Enter scrollback mode for focused pane (block ribbon)
                        KeyCode::Char('[') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                if let Some(slot) = state.slots[slot_idx].as_mut() {
                                    let last = slot.recent_blocks.len().saturating_sub(1);
                                    slot.ribbon_cursor = Some(last);
                                }
                            }
                        }

                        // Exit scrollback mode for focused pane (block ribbon)
                        KeyCode::Char(']') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                if let Some(slot) = state.slots[slot_idx].as_mut() {
                                    slot.ribbon_cursor = None;
                                }
                            }
                        }

                        // Open search overlay
                        KeyCode::Char('/') => {
                            state.search.open = true;
                            state.search.input.clear();
                            state.search.cursor = 0;
                            state.search.results.clear();
                            state.search.pending_query = None;
                            state.search.rx = None;
                            state.status_msg = None;
                        }

                        // Zoom toggle (Ctrl-B z)
                        KeyCode::Char('z') => {
                            let sv = state.active_session_view_mut();
                            let tab = &mut sv.tabs[sv.active_tab];
                            if tab.zoomed.is_some() {
                                tab.zoomed = None;
                            } else {
                                tab.zoomed = Some(tab.focus_path.clone());
                            }
                        }

                        // Copy last block stdout to clipboard (Ctrl-B y)
                        KeyCode::Char('y') => {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                if let Some(slot) = state.slots[slot_idx].as_ref() {
                                    if let Some(last_block) = slot.recent_blocks.last() {
                                        let block_id = last_block.id;
                                        match state
                                            .control
                                            .get_block_stdout(tarpc::context::current(), block_id)
                                            .await
                                        {
                                            Ok(Ok(bytes)) => {
                                                let text = String::from_utf8_lossy(&bytes);
                                                match clipboard::copy_to_clipboard(&text) {
                                                    Ok(()) => {
                                                        state.status_msg =
                                                            Some("copied to clipboard".to_owned());
                                                    }
                                                    Err(e) => {
                                                        tracing::warn!("clipboard: {e}");
                                                        state.status_msg =
                                                            Some(format!("clipboard error: {e}"));
                                                    }
                                                }
                                            }
                                            Ok(Err(e)) => {
                                                state.status_msg =
                                                    Some(format!("get_block_stdout rpc: {e}"));
                                            }
                                            Err(e) => {
                                                state.status_msg =
                                                    Some(format!("rpc transport: {e}"));
                                            }
                                        }
                                    } else {
                                        state.status_msg = Some("no blocks".to_owned());
                                    }
                                } // if let Some(slot)
                            }
                        }

                        // Close focused pane (Ctrl-B x)
                        KeyCode::Char('x') => {
                            close_focused_pane(&mut state);
                            // If all sessions are gone, exit the TUI loop.
                            if state.sessions.is_empty() {
                                break;
                            }
                        }

                        // Toggle sidebar (Ctrl-B s)
                        KeyCode::Char('s') => {
                            state.sidebar_open = !state.sidebar_open;
                            if state.sidebar_open {
                                state.sidebar_focused = true;
                                state.sidebar_last_poll = Instant::now() - Duration::from_secs(10);
                            } else {
                                state.sidebar_focused = false;
                            }
                        }

                        // New session (Ctrl-B S — uppercase to avoid collision with Ctrl-B s sidebar)
                        KeyCode::Char('S') => {
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::NewSession,
                                input: String::new(),
                            });
                        }

                        // Rename active session (Ctrl-B ,  — mirrors tmux rename-session)
                        KeyCode::Char(',') => {
                            let sv = &state.sessions[state.active_session];
                            let current_name = sv.name.clone();
                            let session_id = sv.id;
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::RenameSession(session_id),
                                input: current_name,
                            });
                        }

                        // All other prefix keys consumed silently
                        _ => {}
                    }
                    continue;
                }

                // Search overlay key handling — intercepts all keys while open.
                if state.search.open {
                    match (code, mods) {
                        (KeyCode::Esc, _) => {
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        (KeyCode::Enter, _) => {
                            if !state.search.results.is_empty() {
                                let hit = &state.search.results[state.search.cursor];
                                let target_pane = hit.block.pane;
                                let sv = &mut state.sessions[state.active_session];
                                let tab = &mut sv.tabs[sv.active_tab];
                                let mut all_paths: Vec<Vec<usize>> = Vec::new();
                                let mut tmp: Vec<usize> = Vec::new();
                                leaves_in_order(&tab.root, &mut tmp, &mut all_paths);
                                let found_path = all_paths.iter().find(|p| {
                                    slot_at(&tab.root, p)
                                        .and_then(|idx| state.slots[idx].as_ref())
                                        .map(|s| s.pane_id == target_pane)
                                        .unwrap_or(false)
                                });
                                if let Some(path) = found_path {
                                    let path = path.clone();
                                    let slot_idx = slot_at(&tab.root, &path).expect("just found");
                                    tab.focus_path = path;
                                    let block_id = hit.block.id;
                                    let maybe_cursor =
                                        state.slots[slot_idx].as_ref().and_then(|s| {
                                            s.recent_blocks.iter().position(|b| b.id == block_id)
                                        });
                                    if let Some(c) = maybe_cursor {
                                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                                            slot.ribbon_cursor = Some(c);
                                        }
                                    }
                                } else {
                                    state.status_msg =
                                        Some("search: result pane not loaded".to_owned());
                                }
                            }
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            state.search.cursor = state.search.cursor.saturating_sub(1);
                        }
                        (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            let max = state.search.results.len().saturating_sub(1);
                            state.search.cursor = (state.search.cursor + 1).min(max);
                        }
                        (KeyCode::Backspace, _) => {
                            state.search.input.pop();
                            state.search.pending_query = Some(state.search.input.clone());
                            state.search.last_query_at = Instant::now();
                        }
                        (KeyCode::Char(c), _) => {
                            state.search.input.push(c);
                            state.search.pending_query = Some(state.search.input.clone());
                            state.search.last_query_at = Instant::now();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Sidebar navigation when sidebar is focused.
                if state.sidebar_open && state.sidebar_focused {
                    match code {
                        KeyCode::Up => {
                            state.sidebar_cursor = state.sidebar_cursor.saturating_sub(1);
                            continue;
                        }
                        KeyCode::Down => {
                            let max = state.sidebar_data.len().saturating_sub(1);
                            state.sidebar_cursor = (state.sidebar_cursor + 1).min(max);
                            continue;
                        }
                        KeyCode::Enter => {
                            if let Some(info) = state.sidebar_data.get(state.sidebar_cursor) {
                                let target = info.id;
                                let sv = &mut state.sessions[state.active_session];
                                let tab = &mut sv.tabs[sv.active_tab];
                                let mut all_paths: Vec<Vec<usize>> = Vec::new();
                                let mut tmp: Vec<usize> = Vec::new();
                                leaves_in_order(&tab.root, &mut tmp, &mut all_paths);
                                let found = all_paths.iter().find(|p| {
                                    slot_at(&tab.root, p)
                                        .and_then(|i| state.slots[i].as_ref())
                                        .map(|s| s.pane_id == target)
                                        .unwrap_or(false)
                                });
                                if let Some(path) = found {
                                    tab.focus_path = path.clone();
                                    state.sidebar_focused = false;
                                    let _ = state
                                        .control
                                        .mark_pane_seen(tarpc::context::current(), target)
                                        .await;
                                } else {
                                    state.status_msg =
                                        Some("open this pane first in a tab to focus".to_owned());
                                }
                            }
                            continue;
                        }
                        KeyCode::Esc => {
                            state.sidebar_focused = false;
                            continue;
                        }
                        _ => {}
                    }
                }

                // Block ribbon scrollback navigation (Ctrl-B [ mode).
                {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            if slot.ribbon_cursor.is_some() {
                                match code {
                                    KeyCode::Left | KeyCode::Char('h') => {
                                        slot.ribbon_cursor =
                                            slot.ribbon_cursor.map(|c| c.saturating_sub(1));
                                        continue;
                                    }
                                    KeyCode::Right | KeyCode::Char('l') => {
                                        let max = slot.recent_blocks.len().saturating_sub(1);
                                        slot.ribbon_cursor =
                                            slot.ribbon_cursor.map(|c| (c + 1).min(max));
                                        continue;
                                    }
                                    KeyCode::Enter | KeyCode::Esc => {
                                        slot.ribbon_cursor = None;
                                        continue;
                                    }
                                    _ => {
                                        continue;
                                    }
                                }
                            }
                        } // if let Some(slot)
                    }
                }

                // PgUp / PgDn for scrollback buffer (unmodified only).
                if mods == KeyModifiers::NONE {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        let half_page = (body_area.height / 2).max(1) as usize;
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            match code {
                                KeyCode::PageUp => {
                                    slot.scroll_offset = (slot.scroll_offset + half_page)
                                        .min(slot.scrollback_capacity);
                                    continue;
                                }
                                KeyCode::PageDown => {
                                    slot.scroll_offset =
                                        slot.scroll_offset.saturating_sub(half_page);
                                    continue;
                                }
                                _ => {}
                            }
                        } // if let Some(slot)
                    }
                }

                // Forward key to focused pane.
                if let Some(bytes) = key_to_bytes(code, mods) {
                    let sv = &state.sessions[state.active_session];
                    let tab = &sv.tabs[sv.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset = 0;
                            let t0 = Instant::now();
                            let send_result = slot.input_tx.send(bytes.clone()).await;
                            let elapsed_us = t0.elapsed().as_micros();
                            tracing::debug!(
                                slot_idx,
                                key_bytes = bytes.len(),
                                elapsed_us,
                                send_ok = send_result.is_ok(),
                                "send_keys: input_tx.send (inline await)"
                            );
                        }
                    }
                }
            }

            Event::Paste(s) => {
                let mut buf = Vec::with_capacity(s.len() + 12);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\x1b[201~");
                let bytes = bytes::Bytes::from(buf);
                let sv = &state.sessions[state.active_session];
                let tab = &sv.tabs[sv.active_tab];
                if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                    if let Some(slot) = state.slots[slot_idx].as_mut() {
                        slot.scroll_offset = 0;
                        let send_result = slot.input_tx.send(bytes.clone()).await;
                        tracing::debug!(
                            slot_idx,
                            paste_bytes = bytes.len(),
                            send_ok = send_result.is_ok(),
                            "send_keys: bracketed paste input_tx.send"
                        );
                    }
                }
            }

            Event::Resize(new_cols, new_rows) => {
                // render_pane handles per-slot set_size with split-correct dims.
                // Just clear stale cells and let next draw repaint.
                terminal.clear()?;
                tracing::debug!("terminal resized to {new_cols}x{new_rows}");
            }

            _ => {}
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Route tracing to a file so logs survive ratatui's alternate-screen mode.
    // Fall back to stderr if the file cannot be opened.
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/pyre-tui.log");
    match log_file {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::sync::Mutex::new(f))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::io::stderr)
                .init();
        }
    }

    let cli = Cli::parse();
    splash::play_splash(cli.no_splash);
    let socket = cli.socket.unwrap_or_else(default_socket);
    let shell = resolve_shell(cli.shell);

    match cli.command {
        None => {
            let client = control_client(&socket).await?;
            // Check for existing sessions first; attach to first if present.
            // All PTY spawning is deferred into run_tui() so that it happens
            // after the terminal enters alternate-screen and terminal.size()
            // returns true dimensions — not an 80×24 pre-alt-screen placeholder.
            let existing = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            // Iterate sessions and pick the first one that has a live pane.
            // Stale sessions (worker evicted, zero panes) will fail first_pane;
            // skip them rather than trying to open a pane on a dead worker.
            let mut init = PaneInit::Spawn;
            for sess in existing {
                match first_pane(&client, sess.id).await {
                    Ok(pane) => {
                        init = PaneInit::Existing {
                            session: sess.id,
                            session_name: sess.name,
                            pane,
                        };
                        break;
                    }
                    Err(_) => {
                        // Session has no live pane — skip it.
                        continue;
                    }
                }
            }

            run_tui(socket, init, client, shell).await
        }
        Some(Sub::Attach {
            session: session_prefix,
            pane: pane_prefix,
        }) => {
            let client = control_client(&socket).await?;
            let sessions = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            let session_id = resolve_session(&client, &session_prefix).await?;
            let session_name = sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| {
                    let short8: String = session_id.0.to_string().chars().take(8).collect();
                    format!("session-{short8}")
                });

            let pane = match pane_prefix {
                Some(ref prefix) => resolve_pane(&client, session_id, prefix).await?,
                None => first_pane(&client, session_id).await?,
            };
            run_tui(
                socket,
                PaneInit::Existing {
                    session: session_id,
                    session_name,
                    pane,
                },
                client,
                shell,
            )
            .await
        }
    }
}
