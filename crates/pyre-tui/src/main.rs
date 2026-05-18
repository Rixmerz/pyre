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

use std::collections::VecDeque;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::SinkExt;
use futures::StreamExt;
use pyre_proto::{
    blocks::{BlockHit, SearchBlocksReq},
    Block, InputFrame, OpenPaneReq, OutputFrame, PaneId, PidInspect, PyreDaemonClient, SessionId,
    SpawnReq, SpawnResp, MODE_CONTROL, MODE_STREAM,
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
use regex::Regex;

mod clipboard;
mod theme;
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use theme::EMBER;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

// ─────────────────────────────────────────────────────────────────────────────
// ANSI strip helper
//
// Conservative regex: matches ESC [ followed by optional parameter/intermediate
// bytes (0x20–0x3F) and a final byte (0x40–0x7E). This covers SGR, cursor
// movement, and most CSI sequences without risking eating printable text.
// OSC and other non-CSI sequences are left in place; the line splitter will
// simply include the raw bytes as non-printable, which is harmless for the
// plain-text scrollback view.
// ─────────────────────────────────────────────────────────────────────────────

static ANSI_RE: OnceLock<Regex> = OnceLock::new();

fn ansi_regex() -> &'static Regex {
    ANSI_RE.get_or_init(|| {
        Regex::new(r"\x1b\[[\x20-\x3f]*[\x40-\x7e]").expect("static regex is valid")
    })
}

/// Strip CSI ANSI escape sequences from a byte slice and return a UTF-8 string.
/// Non-UTF-8 bytes are lossily replaced.
fn strip_ansi(raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw);
    ansi_regex().replace_all(&lossy, "").into_owned()
}

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "pyre-tui",
    version,
    about = "Pyre TUI — ratatui terminal frontend"
)]
struct Cli {
    /// Override socket path
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Shell to use when spawning (default: $SHELL)
    #[arg(long, global = true)]
    shell: Option<String>,

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
        crossterm::execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
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

fn vt100_color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Multi-pane layout data model
// ─────────────────────────────────────────────────────────────────────────────

/// One attached PTY pane with its I/O channels and VT parser.
struct PaneSlot {
    pane_id: PaneId,
    parser: vt100::Parser,
    /// Bytes to send to this pane (written by the key handler).
    input_tx: mpsc::Sender<Bytes>,
    /// Bytes from daemon for this pane (drained each UI tick).
    output_rx: mpsc::Receiver<Bytes>,
    /// Last polled block list for the ribbon (up to 20 entries, newest last).
    recent_blocks: Vec<Block>,
    /// `None` = live (rightmost highlighted); `Some(i)` = scrollback cursor.
    ribbon_cursor: Option<usize>,
    /// Wall-clock instant of the last block-poll so we throttle to ~500 ms.
    last_block_poll: std::time::Instant,

    // ── Scrollback line buffer ──
    /// Accumulated output lines, oldest at front. Capacity: scrollback_cap.
    scrollback: VecDeque<String>,
    /// Maximum number of lines retained in the scrollback buffer.
    scrollback_cap: usize,
    /// 0 = live view; N = N lines back from the bottom of scrollback.
    scroll_offset: usize,
    /// Incomplete line accumulator for the line splitter.
    current_line: String,
    /// The screen rect captured during the last render, used for mouse hit-test.
    last_screen_rect: Rect,
    /// Ribbon chip rects captured during last render: (block_idx, rect).
    ribbon_chip_rects: Vec<(usize, Rect)>,
}

impl PaneSlot {
    /// Feed raw bytes into both the vt100 parser and the side-channel line buffer.
    fn process_output(&mut self, data: &[u8]) {
        self.parser.process(data);
        self.feed_line_buffer(data);
    }

    /// Append bytes to the plain-text line buffer, stripping ANSI sequences.
    fn feed_line_buffer(&mut self, raw: &[u8]) {
        let text = strip_ansi(raw);
        for ch in text.chars() {
            match ch {
                '\n' => {
                    let line = std::mem::take(&mut self.current_line);
                    self.scrollback.push_back(line);
                    while self.scrollback.len() > self.scrollback_cap {
                        self.scrollback.pop_front();
                        // If user is scrolled back, keep their view stable.
                        if self.scroll_offset > 0 {
                            self.scroll_offset = self.scroll_offset.saturating_sub(1);
                        }
                    }
                }
                '\r' => {
                    // Carriage return: terminal overwrites the line. For
                    // line-buffer purposes we drop the current partial line so
                    // the next content replaces it rather than appending.
                    self.current_line.clear();
                }
                c if c.is_control() => {
                    // Skip other control characters (bells, form feeds, etc.).
                }
                c => {
                    self.current_line.push(c);
                }
            }
        }
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
    rx: Option<mpsc::Receiver<Vec<BlockHit>>>,
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
            rx: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Drag-selection types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
#[allow(dead_code)]
enum SelectionBase {
    Live,
    Scrollback(usize), // window_top line index into scrollback
}

#[derive(Clone)]
#[allow(dead_code)]
struct Selection {
    pane_idx: usize,
    /// (row, col) relative to the pane's vt100/content area, viewport-relative.
    start: (u16, u16),
    end: (u16, u16),
    dragging: bool,
    base: SelectionBase,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
struct AppState {
    session: SessionId,
    slots: Vec<PaneSlot>,
    tabs: Vec<Tab>,
    active_tab: usize,
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

async fn attach_pane(socket: &Path, session: SessionId, pane_id: PaneId) -> Result<PaneSlot> {
    let (cols, rows) = term_size();

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

    let (net_tx, output_rx) = mpsc::channel::<Bytes>(256);
    let (input_tx, mut key_rx) = mpsc::channel::<Bytes>(64);

    // net → UI
    tokio::spawn(async move {
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    if net_tx.send(f.data).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // UI → net
    tokio::spawn(async move {
        while let Some(data) = key_rx.recv().await {
            let frame = InputFrame { session, data };
            if input_frames.send(frame).await.is_err() {
                break;
            }
        }
    });

    Ok(PaneSlot {
        pane_id,
        parser: vt100::Parser::new(rows, cols, 0),
        input_tx,
        output_rx,
        recent_blocks: Vec::new(),
        ribbon_cursor: None,
        last_block_poll: std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .unwrap_or_else(std::time::Instant::now),
        scrollback: VecDeque::new(),
        scrollback_cap: 10_000,
        scroll_offset: 0,
        current_line: String::new(),
        last_screen_rect: Rect::default(),
        ribbon_chip_rects: Vec::new(),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_pane(
    frame: &mut ratatui::Frame,
    area: Rect,
    slot: &mut PaneSlot,
    focused: bool,
    selection: Option<&Selection>,
    slot_idx: usize,
) {
    let short8: String = slot.pane_id.0.to_string().chars().take(8).collect();
    let border_block = if focused {
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick)
            .border_style(EMBER.border_focus())
            .title(Span::styled(
                format!(" pane {short8} "),
                EMBER.title(EMBER.primary),
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

    if slot.scroll_offset == 0 {
        // ── Live vt100 render ──
        let vt_area = content_area;
        let screen = slot.parser.screen();
        let mut lines: Vec<Line> = Vec::with_capacity(vt_area.height as usize);

        for row in 0..vt_area.height {
            let mut spans: Vec<Span> = Vec::new();
            let mut current_text = String::new();
            let mut current_style = Style::default();

            for col in 0..vt_area.width {
                let cell = screen.cell(row, col);
                let (ch, fg, bg) = match cell {
                    Some(c) => {
                        let ch = if c.contents().is_empty() {
                            ' '
                        } else {
                            c.contents().chars().next().unwrap_or(' ')
                        };
                        let fg = vt100_color(c.fgcolor());
                        let bg = vt100_color(c.bgcolor());
                        (ch, fg, bg)
                    }
                    None => (' ', None, None),
                };

                let style = Style::default()
                    .fg(fg.unwrap_or(Color::Reset))
                    .bg(bg.unwrap_or(Color::Reset));

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

        frame.render_widget(Paragraph::new(lines), vt_area);

        // Overlay selection highlight on live view.
        if let Some(sel) = selection {
            if sel.pane_idx == slot_idx {
                if let SelectionBase::Live = sel.base {
                    let ((r0, c0), (r1, c1)) = sel.normalized();
                    for row in r0..=r1.min(vt_area.height.saturating_sub(1)) {
                        let col_start = if row == r0 { c0 } else { 0 };
                        let col_end = if row == r1 {
                            c1.min(vt_area.width.saturating_sub(1))
                        } else {
                            vt_area.width.saturating_sub(1)
                        };
                        for col in col_start..=col_end {
                            let sx = vt_area.x + col;
                            let sy = vt_area.y + row;
                            if sx < vt_area.x + vt_area.width && sy < vt_area.y + vt_area.height {
                                if let Some(cell) = frame.buffer_mut().cell_mut((sx, sy)) {
                                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        // ── Scrollback render ──
        // Reserve 1 column on the right for the scrollbar.
        let (sb_area, text_area) = if content_area.width > 1 {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(content_area);
            (Some(split[1]), split[0])
        } else {
            (None, content_area)
        };

        let height = text_area.height as usize;
        let total = slot.scrollback.len();
        // The bottom of the visible window is `total - scroll_offset` lines from top.
        let window_end = total.saturating_sub(slot.scroll_offset);
        let window_start = window_end.saturating_sub(height);

        let lines: Vec<Line> = slot
            .scrollback
            .iter()
            .skip(window_start)
            .take(height)
            .map(|l| Line::from(l.as_str()))
            .collect();

        frame.render_widget(Paragraph::new(lines), text_area);

        // Overlay selection highlight on scrollback view.
        if let Some(sel) = selection {
            if sel.pane_idx == slot_idx {
                if let SelectionBase::Scrollback(_) = sel.base {
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

        if let Some(sb_rect) = sb_area {
            let mut sb_state = ScrollbarState::new(total).position(window_end);
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
    slots: &mut Vec<PaneSlot>,
    focus_path: &[usize],
    current_path: &mut Vec<usize>,
    boundaries: &mut Vec<SplitBoundary>,
    selection: Option<&Selection>,
) {
    match node {
        LayoutNode::Leaf(slot_idx) => {
            let focused = current_path == focus_path;
            render_pane(
                frame,
                area,
                &mut slots[*slot_idx],
                focused,
                selection,
                *slot_idx,
            );
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
        .title(Span::styled(" search ", EMBER.title(EMBER.primary)))
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
    let input_spans = vec![
        Span::styled("> ", Style::default().fg(EMBER.primary)),
        Span::styled(app.search.input.as_str(), Style::default().fg(EMBER.text)),
        Span::styled("█", Style::default().fg(EMBER.spark)),
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
            // TODO: real stdout snippet once daemon returns it
            let snippet: String = b.command.chars().take(80).collect();
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

fn render_sidebar(frame: &mut ratatui::Frame, area: Rect, state: &AppState) {
    let block = RatatuiBlock::default()
        .borders(Borders::RIGHT)
        .border_type(BorderType::Rounded)
        .style(EMBER.bg_style())
        .title(Span::styled(" panes ", EMBER.title(EMBER.primary)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = state
        .sidebar_data
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(i, info)| {
            let dot = state_dot_char(info.state);
            let dot_color = state_dot_color(info.state);
            let id_str = info.id.0.to_string();
            let pane_short = &id_str[..8.min(id_str.len())];
            let fg = info.foreground_cmd.as_deref().unwrap_or("-");
            let row_style = if i == state.sidebar_cursor && state.sidebar_focused {
                Style::default()
                    .fg(EMBER.bg)
                    .bg(EMBER.primary)
                    .add_modifier(Modifier::BOLD)
            } else if i == state.sidebar_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(EMBER.text)
            };
            ListItem::new(Line::from(vec![
                Span::styled("  ", row_style),
                Span::styled(dot.to_string(), Style::default().fg(dot_color)),
                Span::styled(format!(" {pane_short} {fg}"), row_style),
            ]))
        })
        .collect();

    let list = List::new(items).style(EMBER.bg_style());
    frame.render_widget(list, inner);
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut AppState,
    prefix_active: bool,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();

        // Three rows: tab bar (1) + body (min 0) + status bar (1)
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let tab_area = outer[0];
        let body_area = outer[1];
        let status_area = outer[2];

        // Frame clear — paint entire frame with bg_style so no bleed.
        frame.render_widget(
            RatatuiBlock::default().style(EMBER.bg_style()),
            frame.area(),
        );

        // Tab bar
        let total_tabs = state.tabs.len();
        let mut tab_spans: Vec<Span> = Vec::new();
        for (i, _) in state.tabs.iter().enumerate() {
            let label = format!(" {} ", i + 1);
            let style = if i == state.active_tab {
                EMBER.tab_active()
            } else {
                EMBER.tab_inactive()
            };
            tab_spans.push(Span::styled(label, style));
            // Single-space separator in bg color between tabs.
            if i + 1 < total_tabs {
                tab_spans.push(Span::styled(" ", Style::default().bg(EMBER.bg)));
            }
        }
        // Tab count indicator on right side.
        let count_label = format!(" {total_tabs} tabs ");
        tab_spans.push(Span::styled(
            count_label,
            Style::default().fg(EMBER.text_dim).bg(EMBER.bg),
        ));
        let tab_line = Line::from(tab_spans);
        frame.render_widget(
            Paragraph::new(tab_line).style(Style::default().bg(EMBER.bg)),
            tab_area,
        );

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
        let active_tab_idx = state.active_tab;
        let focus_path = state.tabs[active_tab_idx].focus_path.clone();
        let zoomed = state.tabs[active_tab_idx].zoomed.clone();
        let mut new_boundaries: Vec<SplitBoundary> = Vec::new();

        // SAFETY: we only borrow root/zoomed via a raw pointer to avoid the
        // simultaneous mutable borrow of slots. render_layout only reads `root`
        // and mutates `slots` at disjoint indices; no mutation of `tabs` occurs.
        let root_ptr: *const LayoutNode = &state.tabs[active_tab_idx].root;

        if let Some(ref zoom_path) = zoomed {
            // Zoom mode: render only the zoomed leaf filling pane_body_area.
            if let Some(slot_idx) = slot_at(unsafe { &*root_ptr }, zoom_path) {
                let focused = true;
                render_pane(
                    frame,
                    pane_body_area,
                    &mut state.slots[slot_idx],
                    focused,
                    state.selection.as_ref(),
                    slot_idx,
                );
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
            );
        }
        state.tabs[active_tab_idx].boundaries = new_boundaries;

        // Status bar — two segments + optional middle message.
        {
            let tab = &state.tabs[state.active_tab];
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
                let slot = &state.slots[slot_idx];
                if slot.ribbon_cursor.is_some() {
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

            // Left: ` ● {session} ▸ {pane} `
            let left_text = if let Some(slot_idx) = focused_slot {
                let slot = &state.slots[slot_idx];
                let session_short = &state.session.0.to_string()[..8];
                let pane_short = &slot.pane_id.0.to_string()[..8];
                format!(" ● {session_short} ▸ {pane_short} ")
            } else {
                format!(" ● {:.8} ", state.session.0.to_string())
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
        if state.search.open {
            // Search overlay — drawn on top of everything else and owns cursor.
            render_search_overlay(frame, state);
        } else if state.pid_inspect.is_none() {
            // No blocking overlay: propagate vt100 cursor from focused pane.
            let tab = &state.tabs[state.active_tab];
            let focused_slot_idx = if let Some(ref zoom_path) = tab.zoomed {
                slot_at(&tab.root, zoom_path)
            } else {
                slot_at(&tab.root, &tab.focus_path)
            };
            if let Some(slot_idx) = focused_slot_idx {
                let slot = &state.slots[slot_idx];
                if slot.scroll_offset == 0 {
                    let vt_area = slot.last_screen_rect;
                    let (vt_row, vt_col) = slot.parser.screen().cursor_position();
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
    })?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Ctrl-B prefix actions
// ─────────────────────────────────────────────────────────────────────────────

/// Cycle focus to the next leaf (DFS order), wrapping around.
fn focus_next(tab: &mut Tab, forward: bool) {
    let mut all_paths: Vec<Vec<usize>> = Vec::new();
    let mut tmp: Vec<usize> = Vec::new();
    leaves_in_order(&tab.root, &mut tmp, &mut all_paths);

    if all_paths.is_empty() {
        return;
    }

    let current_pos = all_paths
        .iter()
        .position(|p| p == &tab.focus_path)
        .unwrap_or(0);

    let next_pos = if forward {
        (current_pos + 1) % all_paths.len()
    } else {
        (current_pos + all_paths.len() - 1) % all_paths.len()
    };

    tab.focus_path = all_paths[next_pos].clone();
}

/// Split the active leaf. `horizontal` = true means HSplit (top/bottom).
async fn split_active(state: &mut AppState, horizontal: bool) -> Result<()> {
    let (cols, rows) = term_size();
    let req = OpenPaneReq {
        session: state.session,
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

    let slot = attach_pane(&state.socket, state.session, new_pane_id).await?;
    let new_slot_idx = state.slots.len();
    state.slots.push(slot);

    let tab = &mut state.tabs[state.active_tab];
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

/// Open a new pane in a new tab.
async fn open_new_tab(state: &mut AppState) -> Result<()> {
    let (cols, rows) = term_size();
    let req = OpenPaneReq {
        session: state.session,
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

    let slot = attach_pane(&state.socket, state.session, new_pane_id).await?;
    let slot_idx = state.slots.len();
    state.slots.push(slot);

    state.tabs.push(Tab {
        root: LayoutNode::Leaf(slot_idx),
        focus_path: vec![],
        zoomed: None,
        boundaries: Vec::new(),
        drag: None,
    });
    state.active_tab = state.tabs.len() - 1;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Mouse event handler
// ─────────────────────────────────────────────────────────────────────────────

/// Compute tab strip layout widths and return the tab index clicked, if any.
/// `col` is the x coordinate of the click (0-based).
fn tab_idx_at_col(tabs: &[Tab], col: u16) -> Option<usize> {
    let mut x: u16 = 0;
    for (i, _) in tabs.iter().enumerate() {
        let label_len = if i == 0 {
            // label format: " [N*] " or " [N] "
            format!(" [{}*] ", i + 1).len() as u16
        } else {
            format!(" [{}] ", i + 1).len() as u16
        };
        if col >= x && col < x + label_len {
            return Some(i);
        }
        x += label_len;
    }
    None
}

/// Handle a mouse event. Returns true if the event was consumed.
fn handle_mouse(state: &mut AppState, me: crossterm::event::MouseEvent, body_area: Rect) -> bool {
    let col = me.column;
    let row = me.row;

    match me.kind {
        MouseEventKind::ScrollUp => {
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(
                &state.tabs[state.active_tab].root,
                body_area,
                &mut leaf_rects,
            );
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    let slot = &mut state.slots[*slot_idx];
                    let max_offset = slot.scrollback.len();
                    slot.scroll_offset = (slot.scroll_offset + 3).min(max_offset);
                    return true;
                }
            }
            false
        }
        MouseEventKind::ScrollDown => {
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(
                &state.tabs[state.active_tab].root,
                body_area,
                &mut leaf_rects,
            );
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    let slot = &mut state.slots[*slot_idx];
                    slot.scroll_offset = slot.scroll_offset.saturating_sub(3);
                    return true;
                }
            }
            false
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // row == 0 is the tab strip (body_area starts at row 1).
            if row == 0 {
                if let Some(tab_idx) = tab_idx_at_col(&state.tabs, col) {
                    state.active_tab = tab_idx;
                    return true;
                }
            }

            // Check if clicking near a split boundary to start a drag.
            let tab = &mut state.tabs[state.active_tab];
            for boundary in tab.boundaries.clone() {
                let hit = if boundary.is_hsplit {
                    row.abs_diff(boundary.coord) <= 1
                } else {
                    col.abs_diff(boundary.coord) <= 1
                };
                if hit {
                    let start_coord = if boundary.is_hsplit { row } else { col };
                    // Capture current weights from the parent node.
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

            // Check if clicking inside a leaf pane.
            let mut leaf_rects: Vec<(usize, Rect)> = Vec::new();
            collect_leaf_rects(
                &state.tabs[state.active_tab].root,
                body_area,
                &mut leaf_rects,
            );
            for (slot_idx, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    focus_slot(state, *slot_idx);
                    return true;
                }
            }
            false
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let tab = &mut state.tabs[state.active_tab];
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
                    // Clamp: each child must keep at least 5%.
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
            false
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let tab = &mut state.tabs[state.active_tab];
            if tab.drag.is_some() {
                tab.drag = None;
                return true;
            }
            false
        }
        _ => false,
    }
}

/// Update active tab's focus_path to point at the given slot index.
fn focus_slot(state: &mut AppState, target_slot_idx: usize) {
    let tab = &mut state.tabs[state.active_tab];
    let mut all_paths: Vec<Vec<usize>> = Vec::new();
    let mut tmp: Vec<usize> = Vec::new();
    leaves_in_order(&tab.root, &mut tmp, &mut all_paths);
    for path in &all_paths {
        if slot_at(&tab.root, path) == Some(target_slot_idx) {
            tab.focus_path = path.clone();
            return;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main TUI loop
// ─────────────────────────────────────────────────────────────────────────────

async fn run_tui(
    socket: PathBuf,
    session: SessionId,
    pane: PaneId,
    control: PyreDaemonClient,
    shell: Option<String>,
) -> Result<()> {
    let initial_slot = attach_pane(&socket, session, pane).await?;

    let mut state = AppState {
        session,
        slots: vec![initial_slot],
        tabs: vec![Tab {
            root: LayoutNode::Leaf(0),
            focus_path: vec![],
            zoomed: None,
            boundaries: Vec::new(),
            drag: None,
        }],
        active_tab: 0,
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
    };

    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut prefix_active = false;

    loop {
        // Drain all pane output into their parsers and scrollback buffers.
        for slot in &mut state.slots {
            while let Ok(data) = slot.output_rx.try_recv() {
                slot.process_output(&data);
            }
        }

        // Poll block lists inline (~500 ms throttle per pane)
        let poll_interval = std::time::Duration::from_millis(500);
        for slot in &mut state.slots {
            if slot.last_block_poll.elapsed() >= poll_interval {
                slot.last_block_poll = std::time::Instant::now();
                let req = pyre_proto::blocks::ListBlocksReq {
                    session: None,
                    limit: 20,
                };
                if let Ok(Ok(blocks)) = state
                    .control
                    .list_blocks(tarpc::context::current(), req)
                    .await
                {
                    // Filter to this pane's blocks only.
                    let pane_id = slot.pane_id;
                    slot.recent_blocks = blocks.into_iter().filter(|b| b.pane == pane_id).collect();
                    // Clamp ribbon cursor if blocks shrank.
                    if let Some(cursor) = slot.ribbon_cursor {
                        if !slot.recent_blocks.is_empty() {
                            slot.ribbon_cursor = Some(cursor.min(slot.recent_blocks.len() - 1));
                        } else {
                            slot.ribbon_cursor = None;
                        }
                    }
                }
            }
        }

        // Search debounce: fire query 150 ms after last keystroke.
        if state.search.open
            && state.search.pending_query.is_some()
            && state.search.last_query_at.elapsed() >= Duration::from_millis(150)
        {
            let query = state.search.pending_query.take().expect("checked Some");
            let (tx, rx) = mpsc::channel::<Vec<BlockHit>>(1);
            state.search.rx = Some(rx);
            let client = state.control.clone();
            let req = SearchBlocksReq { query, limit: 20 };
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

        // Sidebar poll — 1s when open, up to 50 panes.
        if state.sidebar_open && state.sidebar_last_poll.elapsed() >= Duration::from_secs(1) {
            state.sidebar_last_poll = Instant::now();
            if let Ok(Ok(mut panes)) = state
                .control
                .list_all_panes(tarpc::context::current())
                .await
            {
                panes.truncate(50);
                state.sidebar_data = panes;
                state.sidebar_cursor = state
                    .sidebar_cursor
                    .min(state.sidebar_data.len().saturating_sub(1));
            }
        }

        // Draw — pass state as mut so render_pane can store last_screen_rect.
        draw_frame(&mut terminal, &mut state, prefix_active)?;

        // Poll crossterm events (~16 ms = 60 fps)
        if !crossterm::event::poll(Duration::from_millis(16))? {
            continue;
        }

        // Compute body_area for mouse hit-tests (mirrors draw_frame layout).
        let term_size_rect = terminal.size()?;
        let outer_rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(term_size_rect.into());
        let body_area = outer_rects[1];

        match crossterm::event::read()? {
            Event::Mouse(me) => {
                handle_mouse(&mut state, me, body_area);
            }

            Event::Key(key_event) => {
                let code = key_event.code;
                let mods = key_event.modifiers;

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
                            if let Err(e) = open_new_tab(&mut state).await {
                                tracing::warn!("open_new_tab failed: {e}");
                            }
                        }

                        KeyCode::Char('n') => {
                            state.active_tab = (state.active_tab + 1) % state.tabs.len();
                        }

                        KeyCode::Char('p') => {
                            state.active_tab =
                                (state.active_tab + state.tabs.len() - 1) % state.tabs.len();
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
                            focus_next(&mut state.tabs[state.active_tab], true);
                        }

                        KeyCode::Left | KeyCode::Up => {
                            focus_next(&mut state.tabs[state.active_tab], false);
                        }

                        // Enter scrollback mode for focused pane (block ribbon)
                        KeyCode::Char('[') => {
                            let tab = &state.tabs[state.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                let slot = &mut state.slots[slot_idx];
                                let last = slot.recent_blocks.len().saturating_sub(1);
                                slot.ribbon_cursor = Some(last);
                            }
                        }

                        // Exit scrollback mode for focused pane (block ribbon)
                        KeyCode::Char(']') => {
                            let tab = &state.tabs[state.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                state.slots[slot_idx].ribbon_cursor = None;
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
                            let tab = &mut state.tabs[state.active_tab];
                            if tab.zoomed.is_some() {
                                tab.zoomed = None;
                            } else {
                                tab.zoomed = Some(tab.focus_path.clone());
                            }
                        }

                        // Copy last block stdout to clipboard (Ctrl-B y)
                        KeyCode::Char('y') => {
                            let tab = &state.tabs[state.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                let slot = &state.slots[slot_idx];
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
                                            state.status_msg = Some(format!("rpc transport: {e}"));
                                        }
                                    }
                                } else {
                                    state.status_msg = Some("no blocks".to_owned());
                                }
                            }
                        }

                        // Toggle sidebar (Ctrl-B s)
                        KeyCode::Char('s') => {
                            state.sidebar_open = !state.sidebar_open;
                            if state.sidebar_open {
                                state.sidebar_focused = true;
                                // Force immediate poll.
                                state.sidebar_last_poll = Instant::now() - Duration::from_secs(10);
                            } else {
                                state.sidebar_focused = false;
                            }
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
                                let tab = &mut state.tabs[state.active_tab];
                                // Find if the target pane is a leaf in the active tab.
                                let mut all_paths: Vec<Vec<usize>> = Vec::new();
                                let mut tmp: Vec<usize> = Vec::new();
                                leaves_in_order(&tab.root, &mut tmp, &mut all_paths);
                                let found_path = all_paths.iter().find(|p| {
                                    slot_at(&tab.root, p)
                                        .map(|idx| state.slots[idx].pane_id == target_pane)
                                        .unwrap_or(false)
                                });
                                if let Some(path) = found_path {
                                    let path = path.clone();
                                    let slot_idx = slot_at(&tab.root, &path).expect("just found");
                                    tab.focus_path = path;
                                    // Find block index in recent_blocks by id.
                                    let block_id = hit.block.id;
                                    let maybe_cursor = state.slots[slot_idx]
                                        .recent_blocks
                                        .iter()
                                        .position(|b| b.id == block_id);
                                    if let Some(c) = maybe_cursor {
                                        state.slots[slot_idx].ribbon_cursor = Some(c);
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
                            // Focus pane if it is a leaf in the active tab.
                            if let Some(info) = state.sidebar_data.get(state.sidebar_cursor) {
                                let target = info.id;
                                let tab = &mut state.tabs[state.active_tab];
                                let mut all_paths: Vec<Vec<usize>> = Vec::new();
                                let mut tmp: Vec<usize> = Vec::new();
                                leaves_in_order(&tab.root, &mut tmp, &mut all_paths);
                                let found = all_paths.iter().find(|p| {
                                    slot_at(&tab.root, p)
                                        .map(|i| state.slots[i].pane_id == target)
                                        .unwrap_or(false)
                                });
                                if let Some(path) = found {
                                    tab.focus_path = path.clone();
                                    state.sidebar_focused = false;
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
                    let tab = &state.tabs[state.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        let slot = &mut state.slots[slot_idx];
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
                                    // In block scrollback mode other keys are swallowed.
                                    continue;
                                }
                            }
                        }
                    }
                }

                // PgUp / PgDn for scrollback buffer (unmodified only).
                // Shift/Ctrl-modified PgUp/PgDn fall through to PTY.
                if mods == KeyModifiers::NONE {
                    let tab = &state.tabs[state.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        let half_page = (body_area.height / 2).max(1) as usize;
                        let slot = &mut state.slots[slot_idx];
                        match code {
                            KeyCode::PageUp => {
                                let max_offset = slot.scrollback.len();
                                slot.scroll_offset =
                                    (slot.scroll_offset + half_page).min(max_offset);
                                continue;
                            }
                            KeyCode::PageDown => {
                                slot.scroll_offset = slot.scroll_offset.saturating_sub(half_page);
                                continue;
                            }
                            _ => {}
                        }
                    }
                }

                // Forward key to focused pane — reset scroll_offset first so
                // the user sees the live terminal after typing.
                if let Some(bytes) = key_to_bytes(code, mods) {
                    let tab = &state.tabs[state.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                        // Resume live view when sending input.
                        state.slots[slot_idx].scroll_offset = 0;
                        let _ = state.slots[slot_idx].input_tx.send(bytes).await;
                    }
                }
            }

            Event::Resize(new_cols, new_rows) => {
                // Update all parsers; a Resize RPC doesn't exist yet (S3 TODO).
                for slot in &mut state.slots {
                    slot.parser.set_size(new_rows, new_cols);
                }
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket);
    let shell = resolve_shell(cli.shell);

    match cli.command {
        None => {
            let client = control_client(&socket).await?;
            let (cols, rows) = term_size();
            let req = SpawnReq {
                shell: shell.clone(),
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

            // We need a second control client; the spawn one can be reused.
            run_tui(socket, session, pane, client, shell).await
        }
        Some(Sub::Attach {
            session: session_prefix,
            pane: pane_prefix,
        }) => {
            let client = control_client(&socket).await?;
            let session = resolve_session(&client, &session_prefix).await?;
            let pane = match pane_prefix {
                Some(ref prefix) => resolve_pane(&client, session, prefix).await?,
                None => first_pane(&client, session).await?,
            };
            run_tui(socket, session, pane, client, shell).await
        }
    }
}
