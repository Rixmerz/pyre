//! pyre-gpu — S6.2 multi-pane tiling terminal viewer.
//!
//! Connects to `pyred` over UDS, manages a tiling layout of pane streams,
//! and renders via winit + softbuffer. The glyph atlas is shared across all
//! panes. Each leaf pane owns its own stream task and TermView.
//!
//! Keybindings:
//!   Ctrl+w then v   — vsplit (new pane to the right)
//!   Ctrl+w then s   — hsplit (new pane below)
//!   Ctrl+w then h/j/k/l — focus left/down/up/right
//!   Ctrl+w then x   — close focused pane
//!   Ctrl+Tab / Ctrl+Shift+Tab — cycle panes (legacy, kept for compat)
//!   Ctrl+/          — search overlay (scoped to focused pane by default)
//!
//! Mouse (M4):
//!   Left-click pane         — focus that pane
//!   Double-click            — select word under cursor
//!   Triple-click            — select line under cursor
//!   Left-drag split border  — resize split (drag-resize)
//!   Right-click pane        — context menu (Copy/KillPane/SplitH/SplitV/ZoomToggle/InspectPid)
//!   Scroll wheel            — scroll focused pane's scrollback
//!
//! Toast notifications:
//!   Bottom-right toast cards driven by next_pane_event push events.
//!   Toggleable via Ctrl+N.

mod atlas;
mod layout;
mod paint;
mod search;
mod term;
mod toast;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use anyhow::{anyhow, Context, Result};
use atlas::grid_dims_for_window;
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use layout::{Dir, LayoutNode, Orient, Rect};
use paint::Painter;
use pyre_proto::{
    write_control_client, InputFrame, OpenPaneReq, OutputFrame, PaneId, PaneSize, PyreDaemonClient,
    ResizePaneReq, SessionId, SpawnReq, SpawnResp, MODE_STREAM,
};
use softbuffer::{Context as SbContext, Surface};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use term::{collect_grid, TermView};
use toast::{ContextMenu, ToastDeck};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::ModifiersState;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

// ─── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "pyre-gpu", version, about = "GPU terminal viewer for pyred")]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(long, global = true)]
    shell: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    pane: Option<String>,
}

// ─── Stream handle ────────────────────────────────────────────────────────────

/// Per-pane live stream: owns the background I/O task and the channel ends.
struct StreamHandle {
    _pane_id: PaneId,
    output_rx: mpsc::UnboundedReceiver<Bytes>,
    input_tx: mpsc::UnboundedSender<Bytes>,
    /// Dropping the cancel sender stops the stream task.
    _cancel: watch::Sender<()>,
    /// Handle kept for clean shutdown diagnostics; not awaited at runtime.
    _task: JoinHandle<()>,
}

// ─── Click tracker ────────────────────────────────────────────────────────────

/// Tracks rapid successive clicks for double/triple-click detection.
#[derive(Debug, Clone)]
struct ClickTracker {
    last_at: Instant,
    last_pos: (u32, u32), // physical pixel (x, y)
    count: u8,
}

impl ClickTracker {
    fn new() -> Self {
        Self {
            last_at: Instant::now() - Duration::from_secs(10),
            last_pos: (0, 0),
            count: 0,
        }
    }

    /// Record a click at `pos`. Returns the click count (1/2/3).
    fn record(&mut self, pos: (u32, u32)) -> u8 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_at).as_millis() as u64;
        let count = if elapsed <= 300 && self.last_pos == pos {
            self.count.saturating_add(1).min(3)
        } else {
            1
        };
        self.last_at = now;
        self.last_pos = pos;
        self.count = count;
        count
    }
}

// ─── Drag state (split-border resize) ────────────────────────────────────────

/// Which axis a drag is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragAxis {
    /// Dragging a horizontal split boundary (row coord).
    Horizontal,
    /// Dragging a vertical split boundary (col coord).
    Vertical,
}

/// Active drag-resize state. Set when the mouse is held near a split border.
#[derive(Debug, Clone)]
struct DragResize {
    axis: DragAxis,
    /// Pixel coordinate (x for Vertical, y for Horizontal) where the drag started.
    start_px: f64,
    /// Full-window pixel dimension along the drag axis (width or height).
    viewport_dim: u32,
    /// The two pane IDs on either side of the boundary, left/top first.
    left_pane: PaneId,
    right_pane: PaneId,
    /// Starting weights of the two panes.
    start_weight_left: u16,
    #[allow(dead_code)] // kept for future symmetric resize clamping
    start_weight_right: u16,
}

// ─── Selection state ──────────────────────────────────────────────────────────

/// Text selection (for clipboard copy after double/triple-click or drag).
#[derive(Debug, Clone)]
struct Selection {
    /// Which pane this selection belongs to.
    pane_id: PaneId,
    /// Start cell (col, row) in terminal grid coordinates.
    #[allow(dead_code)] // used by future clipboard-copy path
    start: (usize, usize),
    /// End cell (col, row) in terminal grid coordinates (inclusive).
    end: (usize, usize),
    /// Whether the mouse button is still held.
    dragging: bool,
}

impl Selection {
    #[allow(dead_code)] // used by future clipboard-copy path
    fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

// ─── App ──────────────────────────────────────────────────────────────────────

struct App {
    /// Tiling layout tree.
    layout: LayoutNode,
    /// Currently focused pane.
    focused: PaneId,
    /// Per-pane stream handles.
    streams: HashMap<PaneId, StreamHandle>,
    /// Per-pane terminal state.
    terms: HashMap<PaneId, Arc<Mutex<TermView>>>,
    /// Glyph cache — shared across all panes.
    painter: Painter,
    /// Active theme palette used for all colour decisions.
    palette: pyre_themes::Palette,
    /// Control-plane RPC client.
    control: PyreDaemonClient,
    /// Tantivy search overlay.
    search: search::SearchUi,
    /// winit window (created in `resumed`).
    window: Option<Arc<Window>>,
    /// softbuffer surface.
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    /// Full-window grid dimensions in cells.
    cols: usize,
    rows: usize,
    needs_redraw: bool,
    session: SessionId,
    socket: PathBuf,
    shell: Option<String>,
    modifiers: ModifiersState,
    /// When `true`, the next keypress is interpreted as a Ctrl+w sub-command.
    awaiting_window_key: bool,

    // ── M4: mouse fields ──────────────────────────────────────────────────────
    /// Last known cursor position in physical pixels.
    cursor_pos: (f64, f64),
    /// Left button currently held down.
    left_held: bool,
    /// Click-count tracker for double/triple-click detection.
    click_tracker: ClickTracker,
    /// Active drag-resize state (Some while dragging a split border).
    drag_resize: Option<DragResize>,
    /// Active text selection.
    selection: Option<Selection>,
    /// Right-click context menu.
    context_menu: Option<ContextMenu>,

    // ── M4: toast fields ──────────────────────────────────────────────────────
    /// Ephemeral toast notification stack rendered bottom-right.
    toast_deck: ToastDeck,
    /// Toasts produced by the background push-event task.
    toast_rx: mpsc::UnboundedReceiver<toast::Toast>,
}

// ─── ApplicationHandler impl ──────────────────────────────────────────────────

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let title = window_title(self.session, self.focused);
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(1200u32, 800u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let ctx = SbContext::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&ctx, window.clone()).expect("softbuffer surface");
        self.window = Some(window);
        self.surface = Some(surface);
        self.needs_redraw = true;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }

            WindowEvent::Resized(size) => {
                let (c, r) = grid_dims_for_window(size.width, size.height);
                self.cols = c;
                self.rows = r;
                // Resize every pane's TermView proportionally.
                let vp = self.viewport_rect();
                let leaves = self.layout.leaves(vp);
                for (pane_id, rect) in &leaves {
                    if let Some(tv) = self.terms.get(pane_id) {
                        let (pc, pr) = rect_to_cells(*rect);
                        if let Ok(mut tv) = tv.lock() {
                            tv.resize(pc, pr);
                            tv.flush_pending();
                        }
                    }
                    // Propagate to daemon (S6.2-8): N sequential RPCs, fine for v0.1.
                    let (pc, pr) = rect_to_cells(*rect);
                    let client = self.control.clone();
                    let pane_id = *pane_id;
                    tokio::spawn(async move {
                        let req = ResizePaneReq {
                            pane_id,
                            size: PaneSize {
                                cols: pc as u16,
                                rows: pr as u16,
                            },
                        };
                        if let Err(e) = client.resize_pane(tarpc::context::current(), req).await {
                            tracing::warn!("resize_pane rpc: {e:#}");
                        }
                    });
                }
                self.needs_redraw = true;
            }

            WindowEvent::RedrawRequested => {
                // Drain output for all panes.
                for (pane_id, handle) in self.streams.iter_mut() {
                    if let Some(tv) = self.terms.get(pane_id) {
                        while let Ok(chunk) = handle.output_rx.try_recv() {
                            if let Ok(mut tv) = tv.lock() {
                                tv.push_bytes(&chunk);
                            }
                        }
                        if let Ok(mut tv) = tv.lock() {
                            tv.flush_pending();
                            for reply in tv.drain_pty_replies() {
                                let _ = handle.input_tx.send(reply);
                            }
                        }
                    }
                }
                self.draw_frame();
                self.needs_redraw = false;
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_keyboard(event, event_loop);
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                if self.left_held {
                    self.handle_mouse_drag();
                }
                self.needs_redraw = true;
            }

            WindowEvent::MouseInput { state, button, .. } => {
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        self.handle_left_down();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        self.handle_left_up();
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        self.handle_right_down(event_loop);
                    }
                    _ => {}
                }
                self.needs_redraw = true;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Dismiss context menu on scroll.
                self.context_menu = None;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_x, y) => y,
                    MouseScrollDelta::PixelDelta(pos) => (pos.y / 20.0) as f32,
                };
                if lines != 0.0 {
                    self.handle_scroll(lines);
                }
                self.needs_redraw = true;
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Check if any pane has pending output.
        for handle in self.streams.values_mut() {
            if handle.output_rx.try_recv().is_ok() {
                self.needs_redraw = true;
                break;
            }
        }
        if self.search.poll_results() {
            self.needs_redraw = true;
        }
        if self.search.tick_debounce(self.control.clone()) {
            self.needs_redraw = true;
        }
        // Drain incoming toasts from the background push-event task.
        while let Ok(t) = self.toast_rx.try_recv() {
            self.toast_deck.push(t.title, t.body, t.kind);
            self.needs_redraw = true;
        }
        // Expire stale toasts; schedule redraw if any remain live.
        self.toast_deck.tick();
        if self.toast_deck.has_live() {
            self.needs_redraw = true;
        }
        if self.needs_redraw {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

// ─── App impl ─────────────────────────────────────────────────────────────────

impl App {
    fn viewport_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: (self.cols * atlas::CELL_W) as u32,
            h: (self.rows * atlas::CELL_H) as u32,
        }
    }

    fn handle_keyboard(&mut self, event: KeyEvent, event_loop: &ActiveEventLoop) {
        use winit::keyboard::{KeyCode, PhysicalKey};

        if event.state != ElementState::Pressed {
            return;
        }

        // Context menu absorbs arrow keys + Enter + Esc when open.
        if self.context_menu.is_some() {
            match event.physical_key {
                PhysicalKey::Code(KeyCode::ArrowDown) | PhysicalKey::Code(KeyCode::KeyJ) => {
                    if let Some(ref mut m) = self.context_menu {
                        m.cursor = (m.cursor + 1).min(toast::MENU_ITEMS.len().saturating_sub(1));
                    }
                    self.needs_redraw = true;
                    return;
                }
                PhysicalKey::Code(KeyCode::ArrowUp) | PhysicalKey::Code(KeyCode::KeyK) => {
                    if let Some(ref mut m) = self.context_menu {
                        m.cursor = m.cursor.saturating_sub(1);
                    }
                    self.needs_redraw = true;
                    return;
                }
                PhysicalKey::Code(KeyCode::Enter) => {
                    self.commit_context_menu(event_loop);
                    self.needs_redraw = true;
                    return;
                }
                PhysicalKey::Code(KeyCode::Escape) => {
                    self.context_menu = None;
                    self.needs_redraw = true;
                    return;
                }
                _ => {
                    // Other keys dismiss the menu.
                    self.context_menu = None;
                }
            }
        }

        // Search overlay absorbs all keys when open.
        if search_toggle_key(&event, self.modifiers) {
            if self.search.open {
                self.search.close();
            } else {
                self.search.open_overlay_scoped(&self.focused);
            }
            self.needs_redraw = true;
            return;
        }
        if self.search.open {
            let client = self.control.clone();
            if self.search.handle_key(&event, client) {
                let _ = self.search.tick_debounce(self.control.clone());
                self.needs_redraw = true;
            }
            return;
        }

        // Ctrl+N — toggle toast notifications.
        if self.modifiers.contains(ModifiersState::CONTROL)
            && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyN))
        {
            self.toast_deck.enabled = !self.toast_deck.enabled;
            if !self.toast_deck.enabled {
                self.toast_deck.toasts.clear();
            }
            self.needs_redraw = true;
            return;
        }

        // Ctrl+w prefix mode (S6.2-5).
        if self.awaiting_window_key {
            self.awaiting_window_key = false;
            self.handle_window_key(&event, event_loop);
            return;
        }
        if window_prefix_key(&event, self.modifiers) {
            self.awaiting_window_key = true;
            return;
        }

        // Legacy Ctrl+Tab cycle.
        if cycle_pane_key(&event, self.modifiers) {
            self.cycle_pane(1);
            return;
        }
        if cycle_pane_key_back(&event, self.modifiers) {
            self.cycle_pane(-1);
            return;
        }

        // Forward to focused pane's input.
        if let Some(text) = key_to_bytes(&event) {
            if let Some(handle) = self.streams.get(&self.focused) {
                let _ = handle.input_tx.send(Bytes::from(text));
            }
        }
    }

    /// Handle the key that follows a Ctrl+w prefix.
    fn handle_window_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        use winit::keyboard::{KeyCode, PhysicalKey};
        match event.physical_key {
            PhysicalKey::Code(KeyCode::KeyV) => {
                self.spawn_pane(Orient::Vertical);
            }
            PhysicalKey::Code(KeyCode::KeyS) => {
                self.spawn_pane(Orient::Horizontal);
            }
            PhysicalKey::Code(KeyCode::KeyH) => {
                self.move_focus(Dir::Left);
            }
            PhysicalKey::Code(KeyCode::KeyJ) => {
                self.move_focus(Dir::Down);
            }
            PhysicalKey::Code(KeyCode::KeyK) => {
                self.move_focus(Dir::Up);
            }
            PhysicalKey::Code(KeyCode::KeyL) => {
                self.move_focus(Dir::Right);
            }
            PhysicalKey::Code(KeyCode::KeyX) => {
                self.close_focused(event_loop);
            }
            _ => {
                // Unrecognised sub-key: cancel mode silently.
            }
        }
        self.needs_redraw = true;
    }

    fn move_focus(&mut self, dir: Dir) {
        if let Some(next) = self.layout.focus_dir(&self.focused, dir) {
            self.focused = next;
            self.update_title();
        }
    }

    /// Cycle through leaves (legacy Ctrl+Tab).
    fn cycle_pane(&mut self, delta: isize) {
        let vp = self.viewport_rect();
        let leaves: Vec<PaneId> = self
            .layout
            .leaves(vp)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if leaves.len() <= 1 {
            return;
        }
        let pos = leaves
            .iter()
            .position(|id| *id == self.focused)
            .unwrap_or(0);
        let n = leaves.len() as isize;
        let next = (pos as isize + delta).rem_euclid(n) as usize;
        self.focused = leaves[next];
        self.update_title();
        self.needs_redraw = true;
    }

    /// Open a new pane adjacent to `self.focused` via the daemon's `open_pane` RPC.
    fn spawn_pane(&mut self, orient: Orient) {
        let vp = self.viewport_rect();
        let focused = self.focused;
        // Compute the size the new pane will get (half of focused rect).
        let focused_rect = self.layout.rect_for(vp, &focused).unwrap_or(vp);
        let (new_cols, new_rows) = match orient {
            Orient::Vertical => {
                let c = ((focused_rect.w as usize / 2) / atlas::CELL_W).max(20);
                let r = (focused_rect.h as usize / atlas::CELL_H).max(8);
                (c, r)
            }
            Orient::Horizontal => {
                let c = (focused_rect.w as usize / atlas::CELL_W).max(20);
                let r = ((focused_rect.h as usize / 2) / atlas::CELL_H).max(8);
                (c, r)
            }
        };

        let req = OpenPaneReq {
            session: self.session,
            shell: self.shell.clone().or_else(|| std::env::var("SHELL").ok()),
            cwd: std::env::current_dir().ok(),
            cols: new_cols as u16,
            rows: new_rows as u16,
            env: std::env::vars().collect(),
        };
        let client = self.control.clone();
        let session = self.session;

        // We need the new PaneId synchronously to update the layout; use a
        // blocking call via a one-shot channel so the event handler returns quickly.
        let (tx, rx) = mpsc::unbounded_channel::<PaneId>();
        tokio::spawn(async move {
            match client.open_pane(tarpc::context::current(), req).await {
                Ok(Ok(pane_id)) => {
                    let _ = tx.send(pane_id);
                }
                Ok(Err(e)) => tracing::error!("open_pane rpc error: {e}"),
                Err(e) => tracing::error!("open_pane transport: {e:#}"),
            }
            // Keep session alive inside the async block.
            let _ = session;
        });

        // Poll for the result without blocking the event loop. Because winit
        // event handlers cannot await, we'll pick it up in about_to_wait via a
        // pending spawn using a thread-local queue.
        PENDING_PANES.with(|p| {
            p.borrow_mut().push(PendingPane {
                rx,
                orient,
                focused_at_spawn: focused,
            });
        });
    }

    /// Drain the pending-pane queue: for each resolved PaneId, wire up the
    /// stream handle + TermView and insert into the layout.
    fn drain_pending_panes(&mut self) {
        PENDING_PANES.with(|p| {
            let mut queue = p.borrow_mut();
            let mut still_pending: Vec<PendingPane> = Vec::new();
            for mut item in queue.drain(..) {
                if let Ok(pane_id) = item.rx.try_recv() {
                    self.insert_pane(pane_id, item.orient, item.focused_at_spawn);
                } else {
                    still_pending.push(item);
                }
            }
            *queue = still_pending;
        });
    }

    fn insert_pane(&mut self, pane_id: PaneId, orient: Orient, focused_id: PaneId) {
        // Build TermView for this pane.
        let vp = self.viewport_rect();
        let approx_cols = (self.cols / 2).max(20);
        let approx_rows = (self.rows / 2).max(8);
        let tv = Arc::new(Mutex::new(TermView::new(approx_cols, approx_rows)));
        self.terms.insert(pane_id, tv);

        // Spawn stream task.
        let handle = open_stream(self.socket.clone(), self.session, pane_id);
        self.streams.insert(pane_id, handle);

        // Update layout.
        self.layout.split_focused(&focused_id, pane_id, orient);

        // Recompute this pane's TermView size from its new rect.
        if let Some(rect) = self.layout.rect_for(vp, &pane_id) {
            let (pc, pr) = rect_to_cells(rect);
            if let Some(tv) = self.terms.get(&pane_id) {
                if let Ok(mut tv) = tv.lock() {
                    tv.resize(pc, pr);
                }
            }
        }

        // Focus the new pane.
        self.focused = pane_id;
        self.update_title();
        self.needs_redraw = true;
    }

    fn close_focused(&mut self, event_loop: &ActiveEventLoop) {
        // Close via daemon RPC (fire-and-forget).
        let client = self.control.clone();
        let pane_id = self.focused;
        tokio::spawn(async move {
            if let Err(e) = client.close_pane(tarpc::context::current(), pane_id).await {
                tracing::warn!("close_pane rpc: {e:#}");
            }
        });

        // Remove from layout; get new focus candidate.
        let new_focus = self.layout.close(&self.focused);
        // Drop stream handle (stream task stops when _cancel is dropped).
        self.streams.remove(&pane_id);
        self.terms.remove(&pane_id);

        match new_focus {
            Some(f) => {
                self.focused = f;
                self.update_title();
            }
            None => {
                // No panes left — exit.
                event_loop.exit();
                return;
            }
        }
        self.needs_redraw = true;
    }

    fn update_title(&self) {
        if let Some(w) = &self.window {
            w.set_title(&window_title(self.session, self.focused));
        }
    }

    fn draw_frame(&mut self) {
        if self.surface.is_none() {
            return;
        }

        // Compute geometry before taking the mutable surface borrow.
        let vp = self.viewport_rect();
        let buf_w = vp.w as usize;
        let buf_h = vp.h as usize;
        let mut buffer = vec![0u32; buf_w * buf_h];

        // Derive palette-sourced colours once per frame.
        let bg_fill = self.palette.bg.to_rgba8();
        let border_colors = paint::BorderColors {
            focused: self.palette.border_focus.to_rgba8(),
            unfocused: self.palette.border.to_rgba8(),
        };

        // Paint each pane.
        let leaves: Vec<(PaneId, Rect)> = self.layout.leaves(vp);
        for (pane_id, rect) in &leaves {
            let (cell_cols, cell_rows) = rect_to_cells(*rect);
            let cells = self
                .terms
                .get(pane_id)
                .and_then(|tv| {
                    tv.lock()
                        .ok()
                        .map(|tv| collect_grid(&tv, cell_cols, cell_rows, &self.palette))
                })
                .unwrap_or_default();
            self.painter.paint_pane_at(
                &mut buffer,
                buf_w,
                buf_h,
                *rect,
                &cells,
                cell_cols,
                cell_rows,
                bg_fill,
            );

            // Draw border (S6.2-6).
            let focused = *pane_id == self.focused;
            self.painter
                .paint_border(&mut buffer, buf_w, buf_h, *rect, focused, &border_colors);
        }

        // Paint search overlay on top.
        self.search.paint_overlay(
            &mut self.painter.atlas,
            &mut buffer,
            buf_w,
            buf_h,
            self.cols,
            self.rows,
        );

        // Paint context menu (above search overlay).
        if let Some(ref menu) = self.context_menu {
            let menu = menu.clone();
            toast::paint_context_menu(
                &menu,
                &mut self.painter.atlas,
                &mut buffer,
                buf_w,
                buf_h,
                &self.palette,
            );
        }

        // Paint toast deck bottom-right.
        toast::paint_toast_deck(
            &self.toast_deck,
            &mut self.painter.atlas,
            &mut buffer,
            buf_w,
            buf_h,
            &self.palette,
        );

        // Blit to softbuffer.
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let Ok(mut sb) = surface.buffer_mut() else {
            return;
        };
        let sw = sb.width().get() as usize;
        let sh = sb.height().get() as usize;
        for y in 0..sh.min(buf_h) {
            for x in 0..sw.min(buf_w) {
                sb[y * sw + x] = buffer[y * buf_w + x];
            }
        }
        let _ = sb.present();
    }

    // ── M4: mouse handlers ────────────────────────────────────────────────────

    /// Pixel position converted to cell (col, row) in the full grid.
    #[allow(dead_code)]
    fn px_to_cell(&self, px: f64, py: f64) -> (usize, usize) {
        let col = (px as usize) / atlas::CELL_W;
        let row = (py as usize) / atlas::CELL_H;
        (col, row)
    }

    /// Return the `PaneId` whose pixel rect contains `(px, py)`, if any.
    fn pane_at_px(&self, px: f64, py: f64) -> Option<PaneId> {
        let vp = self.viewport_rect();
        let leaves = self.layout.leaves(vp);
        for (pane_id, rect) in leaves {
            if rect.contains_pt(px as u32, py as u32) {
                return Some(pane_id);
            }
        }
        None
    }

    /// Detect whether `(px, py)` is within `BORDER_HIT_PX` pixels of a split
    /// boundary. Returns the drag state if so.
    fn try_hit_boundary(&self, px: f64, py: f64) -> Option<DragResize> {
        const BORDER_HIT_PX: f64 = 6.0;
        let vp = self.viewport_rect();
        let leaves = self.layout.leaves(vp);

        // Check vertical boundaries (VSplit → right edge of left pane).
        for i in 0..leaves.len() {
            for j in i + 1..leaves.len() {
                let (lid, lr) = &leaves[i];
                let (rid, rr) = &leaves[j];

                // Vertical split: left pane right edge ≈ right pane left edge.
                if lr.y == rr.y && lr.h == rr.h {
                    let boundary_x = (lr.x + lr.w) as f64;
                    if (px - boundary_x).abs() <= BORDER_HIT_PX
                        && py >= lr.y as f64
                        && py < (lr.y + lr.h) as f64
                    {
                        let total = lr.w + rr.w;
                        let wl = ((lr.w as u64 * 100) / total as u64) as u16;
                        let wr = 100u16.saturating_sub(wl);
                        return Some(DragResize {
                            axis: DragAxis::Vertical,
                            start_px: px,
                            viewport_dim: vp.w,
                            left_pane: *lid,
                            right_pane: *rid,
                            start_weight_left: wl,
                            start_weight_right: wr,
                        });
                    }
                }

                // Horizontal split: top pane bottom edge ≈ bottom pane top edge.
                if lr.x == rr.x && lr.w == rr.w {
                    let boundary_y = (lr.y + lr.h) as f64;
                    if (py - boundary_y).abs() <= BORDER_HIT_PX
                        && px >= lr.x as f64
                        && px < (lr.x + lr.w) as f64
                    {
                        let total = lr.h + rr.h;
                        let wt = ((lr.h as u64 * 100) / total as u64) as u16;
                        let wb = 100u16.saturating_sub(wt);
                        return Some(DragResize {
                            axis: DragAxis::Horizontal,
                            start_px: py,
                            viewport_dim: vp.h,
                            left_pane: *lid,
                            right_pane: *rid,
                            start_weight_left: wt,
                            start_weight_right: wb,
                        });
                    }
                }
            }
        }
        None
    }

    /// Apply drag-resize weight update to the layout tree.
    fn apply_drag_weights(&mut self, drag: &DragResize, current_px: f64) {
        let delta_px = current_px - drag.start_px;
        let total_px = drag.viewport_dim as f64;
        let delta_pct = (delta_px / total_px * 100.0) as i32;

        let new_left = (drag.start_weight_left as i32 + delta_pct).clamp(5, 95) as u16;
        let new_right = 100u16.saturating_sub(new_left);

        apply_weights_for_pair(
            &mut self.layout,
            &drag.left_pane,
            &drag.right_pane,
            new_left,
            new_right,
        );
    }

    fn handle_left_down(&mut self) {
        let (px, py) = self.cursor_pos;
        self.left_held = true;

        // Dismiss context menu on any left-click.
        self.context_menu = None;

        // Try to hit a split boundary first.
        if let Some(drag) = self.try_hit_boundary(px, py) {
            self.drag_resize = Some(drag);
            return;
        }

        // Click-to-focus pane.
        let click_count = self.click_tracker.record((px as u32, py as u32));
        if let Some(pane_id) = self.pane_at_px(px, py) {
            self.focused = pane_id;
            self.update_title();

            match click_count {
                2 => {
                    // Double-click: select word under cursor.
                    self.select_word_at(pane_id, px, py);
                }
                3 => {
                    // Triple-click: select full line.
                    self.select_line_at(pane_id, px, py);
                }
                _ => {
                    // Single click: start a potential drag selection.
                    let (col, row) = self.px_to_cell_in_pane(pane_id, px, py);
                    self.selection = Some(Selection {
                        pane_id,
                        start: (col, row),
                        end: (col, row),
                        dragging: true,
                    });
                }
            }
        }
    }

    fn handle_left_up(&mut self) {
        self.left_held = false;
        self.drag_resize = None;
        if let Some(ref mut sel) = self.selection {
            sel.dragging = false;
        }
    }

    fn handle_mouse_drag(&mut self) {
        let (px, py) = self.cursor_pos;

        // Drag-resize takes priority.
        if let Some(drag) = self.drag_resize.clone() {
            let current = match drag.axis {
                DragAxis::Vertical => px,
                DragAxis::Horizontal => py,
            };
            self.apply_drag_weights(&drag, current);
            return;
        }

        // Extend text selection — resolve coords before taking the mutable borrow.
        let extend = self.selection.as_ref().and_then(|sel| {
            if sel.dragging {
                Some((sel.pane_id, self.px_to_cell_in_pane(sel.pane_id, px, py)))
            } else {
                None
            }
        });
        if let Some((_pane_id, (col, row))) = extend {
            if let Some(ref mut sel) = self.selection {
                sel.end = (col, row);
            }
        }
    }

    fn handle_right_down(&mut self, event_loop: &ActiveEventLoop) {
        let (px, py) = self.cursor_pos;

        // Dismiss any existing menu first.
        self.context_menu = None;

        if let Some(pane_id) = self.pane_at_px(px, py) {
            self.focused = pane_id;
            self.update_title();

            let vp = self.viewport_rect();
            let menu = ContextMenu::new(
                px as usize,
                py as usize,
                pane_id,
                vp.w as usize,
                vp.h as usize,
            );
            self.context_menu = Some(menu);
        }
        // Suppress "unused" warning — event_loop is not needed here but kept
        // for API symmetry with other handlers that may need it.
        let _ = event_loop;
    }

    /// Execute the currently highlighted context menu item.
    fn commit_context_menu(&mut self, event_loop: &ActiveEventLoop) {
        let item = self.context_menu.as_ref().and_then(|m| m.item_at_cursor());
        self.context_menu = None;

        let Some(item) = item else { return };

        use toast::MenuItem;
        match item {
            MenuItem::Copy => {
                // Clipboard copy: if there is an active selection, extract and copy.
                // Full clipboard integration is deferred until a platform clipboard
                // crate is added; log intention for now.
                tracing::debug!("context menu: Copy (clipboard integration deferred)");
            }
            MenuItem::KillPane => {
                self.close_focused(event_loop);
            }
            MenuItem::SplitH => {
                self.spawn_pane(Orient::Horizontal);
            }
            MenuItem::SplitV => {
                self.spawn_pane(Orient::Vertical);
            }
            MenuItem::ZoomToggle => {
                // Zoom toggle: pyre-gpu has no zoom yet (deferred post-M4).
                tracing::debug!("context menu: ZoomToggle (deferred)");
            }
            MenuItem::InspectPid => {
                // PID inspect: deferred until PidInspect overlay is ported.
                tracing::debug!("context menu: InspectPid (deferred)");
            }
        }
    }

    fn handle_scroll(&mut self, lines: f32) {
        // Route scroll into the focused pane's terminal scrollback.
        if let Some(tv) = self.terms.get(&self.focused) {
            if let Ok(mut tv) = tv.lock() {
                use alacritty_terminal::grid::Scroll;
                let delta = if lines > 0.0 {
                    Scroll::Delta(lines.ceil() as i32)
                } else {
                    Scroll::Delta(lines.floor() as i32)
                };
                tv.term.grid_mut().scroll_display(delta);
            }
        }
    }

    /// Convert a physical pixel `(px, py)` to a cell `(col, row)` relative to
    /// the top-left of the given pane's content rect. Clamps to the pane bounds.
    fn px_to_cell_in_pane(&self, pane_id: PaneId, px: f64, py: f64) -> (usize, usize) {
        let vp = self.viewport_rect();
        let rect = self.layout.rect_for(vp, &pane_id).unwrap_or(vp);
        let rel_x = (px as u32).saturating_sub(rect.x) as usize;
        let rel_y = (py as u32).saturating_sub(rect.y) as usize;
        let col = rel_x / atlas::CELL_W;
        let row = rel_y / atlas::CELL_H;
        let (max_cols, max_rows) = rect_to_cells(rect);
        (
            col.min(max_cols.saturating_sub(1)),
            row.min(max_rows.saturating_sub(1)),
        )
    }

    /// Double-click: select the word (alphanumeric run) at the given cell.
    fn select_word_at(&mut self, pane_id: PaneId, px: f64, py: f64) {
        let (col, row) = self.px_to_cell_in_pane(pane_id, px, py);
        let Some(tv_arc) = self.terms.get(&pane_id) else {
            return;
        };
        let Ok(tv) = tv_arc.lock() else { return };

        let grid = tv.term.grid();
        let num_cols = grid.columns();
        let num_rows = grid.screen_lines();
        if row >= num_rows || col >= num_cols {
            return;
        }

        use alacritty_terminal::index::{
            Column as TermColumn, Line as TermLine, Point as TermPoint,
        };
        let line = TermLine(row as i32 - grid.display_offset() as i32);

        let is_word_char = |c: char| c.is_alphanumeric() || c == '_';

        // Walk left.
        let mut start_col = col;
        loop {
            if start_col == 0 {
                break;
            }
            let c = grid[TermPoint::new(line, TermColumn(start_col - 1))].c;
            if !is_word_char(c) {
                break;
            }
            start_col -= 1;
        }
        // Walk right.
        let mut end_col = col;
        loop {
            if end_col + 1 >= num_cols {
                break;
            }
            let c = grid[TermPoint::new(line, TermColumn(end_col + 1))].c;
            if !is_word_char(c) {
                break;
            }
            end_col += 1;
        }

        self.selection = Some(Selection {
            pane_id,
            start: (start_col, row),
            end: (end_col, row),
            dragging: false,
        });
    }

    /// Triple-click: select the full line at the given cell.
    fn select_line_at(&mut self, pane_id: PaneId, px: f64, py: f64) {
        let (_, row) = self.px_to_cell_in_pane(pane_id, px, py);
        let Some(tv_arc) = self.terms.get(&pane_id) else {
            return;
        };
        let Ok(tv) = tv_arc.lock() else { return };

        let num_cols = tv.term.grid().columns();
        if num_cols == 0 {
            return;
        }

        self.selection = Some(Selection {
            pane_id,
            start: (0, row),
            end: (num_cols.saturating_sub(1), row),
            dragging: false,
        });
    }
}

// ─── Drag-resize weight update ────────────────────────────────────────────────

/// Walk the layout tree and update the weights of the two siblings identified
/// by `left_pane` and `right_pane` (which must be adjacent leaves inside the
/// same split node).
fn apply_weights_for_pair(
    node: &mut LayoutNode,
    left_pane: &PaneId,
    right_pane: &PaneId,
    new_left: u16,
    new_right: u16,
) -> bool {
    match node {
        LayoutNode::Leaf(_) => false,
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
            // Check if this node directly contains the two panes as adjacent leaves.
            let mut found_l = None;
            let mut found_r = None;
            for (i, (child, _)) in children.iter().enumerate() {
                if let LayoutNode::Leaf(id) = child {
                    if id == left_pane {
                        found_l = Some(i);
                    }
                    if id == right_pane {
                        found_r = Some(i);
                    }
                }
            }
            if let (Some(li), Some(ri)) = (found_l, found_r) {
                // Update weights.
                children[li].1 = new_left;
                children[ri].1 = new_right;
                return true;
            }
            // Recurse.
            for (child, _) in children.iter_mut() {
                if apply_weights_for_pair(child, left_pane, right_pane, new_left, new_right) {
                    return true;
                }
            }
            false
        }
    }
}

// ─── Thread-local pending pane queue (for async pane spawn) ──────────────────

struct PendingPane {
    rx: mpsc::UnboundedReceiver<PaneId>,
    orient: Orient,
    focused_at_spawn: PaneId,
}

std::thread_local! {
    static PENDING_PANES: std::cell::RefCell<Vec<PendingPane>> = const { std::cell::RefCell::new(Vec::new()) };
}

// ─── Stream helpers ───────────────────────────────────────────────────────────

fn open_stream(socket: PathBuf, session: SessionId, pane_id: PaneId) -> StreamHandle {
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Bytes>();
    let (cancel_tx, cancel_rx) = watch::channel(());

    let input_tx_for_handle = input_tx.clone();
    let task = tokio::spawn(async move {
        loop {
            let res = stream_bridge(
                socket.clone(),
                session,
                pane_id,
                output_tx.clone(),
                &mut input_rx,
                cancel_rx.clone(),
            )
            .await;
            if let Err(e) = res {
                tracing::error!("stream [{pane_id}]: {e:#}");
            }
            // If the cancel was signalled, stop.
            if cancel_rx.has_changed().unwrap_or(true) {
                break;
            }
        }
    });

    StreamHandle {
        _pane_id: pane_id,
        output_rx,
        input_tx: input_tx_for_handle,
        _cancel: cancel_tx,
        _task: task,
    }
}

async fn stream_bridge(
    socket: PathBuf,
    session: SessionId,
    pane: PaneId,
    output_tx: mpsc::UnboundedSender<Bytes>,
    input_rx: &mut mpsc::UnboundedReceiver<Bytes>,
    mut cancel: watch::Receiver<()>,
) -> Result<()> {
    let mut stream_sock = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let mut output_frames = tokio_serde::SymmetricallyFramed::new(
        FramedRead::new(rd, LengthDelimitedCodec::new()),
        SymmetricalBincode::<OutputFrame>::default(),
    );
    let mut input_frames = tokio_serde::SymmetricallyFramed::new(
        FramedWrite::new(wr, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputFrame>::default(),
    );

    loop {
        tokio::select! {
            _ = cancel.changed() => return Ok(()),
            frame = output_frames.next() => {
                match frame {
                    Some(Ok(f)) => {
                        if !f.data.is_empty() {
                            let _ = output_tx.send(f.data);
                        }
                    }
                    Some(Err(e)) => return Err(anyhow!("output transport: {e}")),
                    None => break,
                }
            }
            inp = input_rx.recv() => {
                match inp {
                    Some(data) => {
                        input_frames
                            .send(InputFrame { session, data })
                            .await?;
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

// ─── Key helpers ─────────────────────────────────────────────────────────────

fn window_prefix_key(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::KeyW))
        && mods.contains(ModifiersState::CONTROL)
}

fn cycle_pane_key(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
        && mods.contains(ModifiersState::CONTROL)
        && !mods.contains(ModifiersState::SHIFT)
}

fn cycle_pane_key_back(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
        && mods.contains(ModifiersState::CONTROL | ModifiersState::SHIFT)
}

fn search_toggle_key(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    if event.state != ElementState::Pressed || !mods.contains(ModifiersState::CONTROL) {
        return false;
    }
    matches!(event.physical_key, PhysicalKey::Code(KeyCode::Slash))
        || matches!(&event.logical_key, Key::Character(s) if s == "/")
}

fn key_to_bytes(event: &KeyEvent) -> Option<Vec<u8>> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    match event.physical_key {
        PhysicalKey::Code(KeyCode::Enter) => return Some(b"\r".to_vec()),
        PhysicalKey::Code(KeyCode::Backspace) => return Some(vec![0x7f]),
        PhysicalKey::Code(KeyCode::Tab) => return Some(b"\t".to_vec()),
        PhysicalKey::Code(KeyCode::Escape) => return Some(b"\x1b".to_vec()),
        _ => {}
    }
    match &event.logical_key {
        Key::Character(s) => Some(s.as_bytes().to_vec()),
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
        _ => None,
    }
}

// ─── Misc helpers ─────────────────────────────────────────────────────────────

fn window_title(session: SessionId, pane: PaneId) -> String {
    let sess: String = session.0.to_string().chars().take(8).collect();
    let pane_s: String = pane.0.to_string().chars().take(8).collect();
    format!("pyre-gpu [{sess}/{pane_s}] — Ctrl+w v/s split, h/j/k/l focus, x close, Ctrl+/ search")
}

/// Convert a pixel `Rect` to `(cols, rows)` in cell units.
fn rect_to_cells(rect: Rect) -> (usize, usize) {
    let cols = (rect.w as usize / atlas::CELL_W).max(1);
    let rows = (rect.h as usize / atlas::CELL_H).max(1);
    (cols, rows)
}

fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("PYRE_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
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
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.0.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no session matches '{prefix}'")),
        1 => Ok(matches[0].id),
        n => Err(anyhow!("{n} sessions match '{prefix}'")),
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
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no pane matches '{prefix}'")),
        1 => Ok(matches[0].id),
        n => Err(anyhow!("{n} panes match '{prefix}'")),
    }
}

async fn first_pane(client: &PyreDaemonClient, session: SessionId) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    panes
        .into_iter()
        .next()
        .map(|p| p.id)
        .ok_or_else(|| anyhow!("session has no panes"))
}

fn term_size() -> (u16, u16) {
    (120, 40)
}

async fn spawn_default(
    client: &PyreDaemonClient,
    shell: Option<String>,
) -> Result<(SessionId, PaneId)> {
    let (cols, rows) = term_size();
    let req = SpawnReq {
        shell: shell.or_else(|| std::env::var("SHELL").ok()),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let SpawnResp { session, pane } = client
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    Ok((session, pane))
}

// ─── main ─────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket);
    let painter = Painter::from_system().context("init painter")?;

    // Load active theme from config; fall back to built-in default.
    let registry = pyre_themes::Registry::builtin();
    let theme_name = pyre_themes::config::load_theme_name()
        .unwrap_or(None)
        .unwrap_or_else(|| pyre_themes::Registry::default_theme().to_string());
    let palette = registry
        .get(&theme_name)
        .map(|t| t.palette.clone())
        .unwrap_or_else(|| {
            registry
                .get(pyre_themes::Registry::default_theme())
                .expect("ember is always present")
                .palette
                .clone()
        });

    let client = control_client(&socket).await?;
    let (session, pane) = if let Some(ref sess_prefix) = cli.session {
        let session = resolve_session(&client, sess_prefix).await?;
        let pane = match &cli.pane {
            Some(p) => resolve_pane(&client, session, p).await?,
            None => first_pane(&client, session).await?,
        };
        (session, pane)
    } else {
        let existing = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(sess) = existing.into_iter().next() {
            if let Ok(p) = first_pane(&client, sess.id).await {
                (sess.id, p)
            } else {
                spawn_default(&client, cli.shell.clone()).await?
            }
        } else {
            spawn_default(&client, cli.shell.clone()).await?
        }
    };

    let (cols, rows) = (80usize, 24usize);
    let term = Arc::new(Mutex::new(TermView::new(cols, rows)));

    // Build initial single-pane layout.
    let layout = LayoutNode::Leaf(pane);
    let mut terms: HashMap<PaneId, Arc<Mutex<TermView>>> = HashMap::new();
    terms.insert(pane, term);

    let stream_handle = open_stream(socket.clone(), session, pane);
    let mut streams: HashMap<PaneId, StreamHandle> = HashMap::new();
    streams.insert(pane, stream_handle);

    // ── Toast background task ─────────────────────────────────────────────────
    // Mirrors the TUI's push-event task: long-polls `next_pane_event` and
    // sends Toast structs into an unbounded channel drained each frame.
    let notif_cfg = pyre_themes::config::load_notifications_config().unwrap_or_default();
    let (toast_tx, toast_rx) = mpsc::unbounded_channel::<toast::Toast>();
    {
        let push_socket = socket.clone();
        let ttl = Duration::from_millis(notif_cfg.ttl_ms);
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            let mut backoff = Duration::from_millis(200);
            loop {
                // Each iteration needs a fresh control connection; reuse the
                // same helper used by the main path.
                let client = match async {
                    let mut sock = UnixStream::connect(&push_socket).await?;
                    write_control_client(&mut sock).await?;
                    let transport = tarpc::serde_transport::new(
                        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
                        Bincode::default(),
                    );
                    anyhow::Ok(
                        PyreDaemonClient::new(tarpc::client::Config::default(), transport).spawn(),
                    )
                }
                .await
                {
                    Ok(c) => c,
                    Err(_) => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                        continue;
                    }
                };
                backoff = Duration::from_millis(200);

                match client
                    .next_pane_event(tarpc::context::current(), seq, 30_000)
                    .await
                {
                    Ok(Ok(events)) if !events.is_empty() => {
                        if let Some(last) = events.last() {
                            seq = last.seq;
                        }
                        for ev in &events {
                            if let Some(t) = toast::pane_event_to_toast(ev, ttl) {
                                let _ = toast_tx.send(t);
                            }
                        }
                    }
                    Ok(Ok(_)) => {}
                    _ => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(5));
                    }
                }
            }
        });
    }

    let toast_deck = ToastDeck::new(notif_cfg.enabled, notif_cfg.ttl_ms, notif_cfg.max_visible);

    let app = App {
        layout,
        focused: pane,
        streams,
        terms,
        painter,
        palette,
        control: client,
        search: search::SearchUi::default(),
        window: None,
        surface: None,
        cols,
        rows,
        needs_redraw: true,
        session,
        socket,
        shell: cli.shell,
        modifiers: ModifiersState::default(),
        awaiting_window_key: false,
        cursor_pos: (0.0, 0.0),
        left_held: false,
        click_tracker: ClickTracker::new(),
        drag_resize: None,
        selection: None,
        context_menu: None,
        toast_deck,
        toast_rx,
    };

    let event_loop = EventLoop::new().context("event loop")?;

    // Patch about_to_wait to also drain pending panes.
    // We achieve this by overriding the trait via a wrapper type that delegates
    // to App but adds the drain call.
    struct AppWrapper(App);

    impl ApplicationHandler for AppWrapper {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            self.0.resumed(el);
        }
        fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
            self.0.window_event(el, id, event);
        }
        fn about_to_wait(&mut self, el: &ActiveEventLoop) {
            self.0.drain_pending_panes();
            self.0.about_to_wait(el);
        }
    }

    event_loop
        .run_app(&mut AppWrapper(app))
        .context("run_app")?;
    Ok(())
}
