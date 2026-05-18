//! pyre-tui — ratatui-based terminal UI for pyre.
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
//!   All other keys forwarded to the focused PTY.

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::SinkExt;
use futures::StreamExt;
use pyre_proto::{
    blocks::{BlockHit, SearchBlocksReq},
    Block, InputFrame, OpenPaneReq, OutputFrame, PaneId, PyreDaemonClient, SessionId, SpawnReq,
    SpawnResp, MODE_CONTROL, MODE_STREAM,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Terminal;
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

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
        crossterm::execute!(stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
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
}

/// Recursive layout tree. Indices reference `AppState::slots`.
enum LayoutNode {
    Leaf(usize),
    HSplit(Vec<LayoutNode>),
    VSplit(Vec<LayoutNode>),
}

/// One tab, owning a layout tree and a cursor into the focused leaf.
struct Tab {
    root: LayoutNode,
    /// Path of child indices from `root` down to the active `Leaf`.
    focus_path: Vec<usize>,
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Layout helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk the tree depth-first and collect every (focus_path, slot_index) leaf.
fn leaves_in_order(node: &LayoutNode, path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    match node {
        LayoutNode::Leaf(_) => out.push(path.clone()),
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
            for (i, child) in children.iter().enumerate() {
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
                node = children.get(idx)?;
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
            replace_at(&mut children[path[0]], &path[1..], new_node);
        }
        LayoutNode::Leaf(_) => {}
    }
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
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Rendering
// ─────────────────────────────────────────────────────────────────────────────

fn render_pane(frame: &mut ratatui::Frame, area: Rect, slot: &PaneSlot, focused: bool) {
    let border_style = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = format!("{:.8}", slot.pane_id.0.to_string());
    let border_block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let inner = border_block.inner(area);
    frame.render_widget(border_block, area);

    // Split inner area: vt100 area (Min 1) on top, ribbon (1 line) at bottom.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let vt_area = split[0];
    let ribbon_area = split[1];

    // ── vt100 render ──
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

    // ── ribbon render ──
    render_ribbon(frame, ribbon_area, slot);
}

/// Render the one-line block ribbon inside `area`.
fn render_ribbon(frame: &mut ratatui::Frame, area: Rect, slot: &PaneSlot) {
    if slot.recent_blocks.is_empty() {
        let p = Paragraph::new(" (no blocks)").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(p, area);
        return;
    }

    // Determine the highlighted index.
    let highlight_idx = match slot.ribbon_cursor {
        Some(i) => i,
        None => slot.recent_blocks.len().saturating_sub(1),
    };

    let mut spans: Vec<Span> = Vec::new();
    for (i, b) in slot.recent_blocks.iter().enumerate() {
        let short_id = &b.id.0.to_string()[..4];
        let cmd_short: String = b.command.chars().take(12).collect();
        let exit_str = match b.exit_code {
            Some(c) => format!("{c}"),
            None => "…".to_owned(),
        };
        let entry = format!("b{short_id}:{cmd_short}[{exit_str}]");

        let style = if i == highlight_idx {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        if i > 0 {
            spans.push(Span::raw(","));
        }
        spans.push(Span::styled(entry, style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_layout(
    frame: &mut ratatui::Frame,
    area: Rect,
    node: &LayoutNode,
    slots: &[PaneSlot],
    focus_path: &[usize],
    current_path: &mut Vec<usize>,
) {
    match node {
        LayoutNode::Leaf(slot_idx) => {
            let focused = current_path == focus_path;
            render_pane(frame, area, &slots[*slot_idx], focused);
        }
        LayoutNode::HSplit(children) => {
            let n = children.len() as u32;
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![Constraint::Ratio(1, n); children.len()])
                .split(area);
            for (i, (child, rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(frame, *rect, child, slots, focus_path, current_path);
                current_path.pop();
            }
        }
        LayoutNode::VSplit(children) => {
            let n = children.len() as u32;
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(vec![Constraint::Ratio(1, n); children.len()])
                .split(area);
            for (i, (child, rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(frame, *rect, child, slots, focus_path, current_path);
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
        .title(" Search (Esc to close) ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Split inner: 3-line input box + remainder for results.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let input_area = split[0];
    let results_area = split[1];

    // Input box — append `_` as cursor indicator.
    let input_display = format!("{}_", app.search.input);
    let input_block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .title(" Query ")
        .border_style(Style::default().fg(Color::Yellow));
    let input_para = Paragraph::new(input_display).block(input_block);
    frame.render_widget(input_para, input_area);

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
        })
        .collect();

    let list = List::new(items)
        .block(
            RatatuiBlock::default()
                .borders(Borders::ALL)
                .title(format!(" {} results ", app.search.results.len()))
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    // Use a stateful list so we can highlight the cursor item.
    let mut list_state = ratatui::widgets::ListState::default();
    if !app.search.results.is_empty() {
        list_state.select(Some(app.search.cursor));
    }
    frame.render_stateful_widget(list, results_area, &mut list_state);
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &AppState,
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

        // Tab bar
        let tab_spans: Vec<Span> = state
            .tabs
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let label = if i == state.active_tab {
                    format!(" [{}*] ", i + 1)
                } else {
                    format!(" [{}] ", i + 1)
                };
                let style = if i == state.active_tab {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                Span::styled(label, style)
            })
            .collect();
        let tab_line = Line::from(tab_spans);
        frame.render_widget(Paragraph::new(tab_line), tab_area);

        // Body — render active tab's layout
        let tab = &state.tabs[state.active_tab];
        let mut current_path: Vec<usize> = Vec::new();
        render_layout(
            frame,
            body_area,
            &tab.root,
            &state.slots,
            &tab.focus_path,
            &mut current_path,
        );

        // Status bar
        let status_text = if state.search.open {
            format!(
                " search: {} ({} results)",
                state.search.input,
                state.search.results.len()
            )
        } else if let Some(ref msg) = state.status_msg {
            format!(" {msg}")
        } else {
            let focused_slot = slot_at(&tab.root, &tab.focus_path);
            if let Some(slot_idx) = focused_slot {
                let slot = &state.slots[slot_idx];
                let session_short = &state.session.0.to_string()[..8];
                let pane_short = &slot.pane_id.0.to_string()[..8];
                let base = format!(" session {session_short} pane {pane_short}");
                if let Some(cursor) = slot.ribbon_cursor {
                    if let Some(b) = slot.recent_blocks.get(cursor) {
                        let bid = &b.id.0.to_string()[..8];
                        let exit_str = match b.exit_code {
                            Some(c) => format!("{c}"),
                            None => "?".to_owned(),
                        };
                        format!("{base} | scroll b{bid} {} exit={exit_str}", b.command)
                    } else {
                        base
                    }
                } else {
                    base
                }
            } else {
                format!(" session {:.8}", state.session.0.to_string())
            }
        };
        frame.render_widget(
            Paragraph::new(status_text).style(Style::default().fg(Color::Gray)),
            status_area,
        );

        // Search overlay — drawn on top of everything else.
        if state.search.open {
            render_search_overlay(frame, state);
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
    let old_path = tab.focus_path.clone();

    // Find the existing leaf slot index at the current focus path.
    let old_slot_idx = match slot_at(&tab.root, &old_path) {
        Some(idx) => idx,
        None => return Ok(()), // nothing to split
    };

    let new_node = if horizontal {
        LayoutNode::HSplit(vec![
            LayoutNode::Leaf(old_slot_idx),
            LayoutNode::Leaf(new_slot_idx),
        ])
    } else {
        LayoutNode::VSplit(vec![
            LayoutNode::Leaf(old_slot_idx),
            LayoutNode::Leaf(new_slot_idx),
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
    });
    state.active_tab = state.tabs.len() - 1;

    Ok(())
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
        }],
        active_tab: 0,
        control,
        socket,
        shell,
        search: SearchState::default(),
        status_msg: None,
    };

    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut prefix_active = false;

    loop {
        // Drain all pane output into their parsers
        for slot in &mut state.slots {
            while let Ok(data) = slot.output_rx.try_recv() {
                slot.parser.process(&data);
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

        // Draw
        draw_frame(&mut terminal, &state)?;

        // Poll crossterm events (~16 ms = 60 fps)
        if !crossterm::event::poll(Duration::from_millis(16))? {
            continue;
        }

        match crossterm::event::read()? {
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

                        // Enter scrollback mode for focused pane
                        KeyCode::Char('[') => {
                            let tab = &state.tabs[state.active_tab];
                            if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
                                let slot = &mut state.slots[slot_idx];
                                let last = slot.recent_blocks.len().saturating_sub(1);
                                slot.ribbon_cursor = Some(last);
                            }
                        }

                        // Exit scrollback mode for focused pane
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

                // Scrollback navigation (no prefix required when in scrollback mode)
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
                                    // In scrollback mode other keys are swallowed.
                                    continue;
                                }
                            }
                        }
                    }
                }

                // Forward key to focused pane
                if let Some(bytes) = key_to_bytes(code, mods) {
                    let tab = &state.tabs[state.active_tab];
                    if let Some(slot_idx) = slot_at(&tab.root, &tab.focus_path) {
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
