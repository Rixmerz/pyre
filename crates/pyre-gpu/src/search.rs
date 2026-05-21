//! Block search overlay (Tantivy via `search_blocks` RPC).

use std::time::Instant;

use pyre_proto::{BlockHit, PyreDaemonClient, SearchBlocksReq};
use tokio::sync::mpsc;

use crate::atlas::GlyphAtlas;

const PANEL_MARGIN: usize = 2;
const MAX_VISIBLE_RESULTS: usize = 10;

pub struct SearchUi {
    pub open: bool,
    pub input: String,
    pub cursor: usize,
    pub results: Vec<BlockHit>,
    pub status: String,
    pub pending_query: Option<String>,
    pub last_query_at: Instant,
    pub failures_only: bool,
    rx: Option<mpsc::UnboundedReceiver<Vec<BlockHit>>>,
}

impl Default for SearchUi {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            cursor: 0,
            results: Vec::new(),
            status: String::new(),
            pending_query: None,
            last_query_at: Instant::now(),
            failures_only: false,
            rx: None,
        }
    }
}

pub fn parse_search_input(input: &str) -> (String, bool) {
    if let Some(rest) = input.strip_prefix('!') {
        (rest.trim_start().to_string(), true)
    } else {
        (input.to_string(), false)
    }
}

impl SearchUi {
    pub fn open_overlay(&mut self) {
        self.open = true;
        self.input.clear();
        self.cursor = 0;
        self.results.clear();
        self.status = "search (! = failures only) — Enter run, Esc close".into();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.rx = None;
        self.pending_query = None;
    }

    pub fn poll_results(&mut self) -> bool {
        let Some(rx) = self.rx.as_mut() else {
            return false;
        };
        let mut changed = false;
        while let Ok(hits) = rx.try_recv() {
            self.results = hits;
            self.cursor = 0;
            self.status = format!("{} result(s)", self.results.len());
            changed = true;
        }
        changed
    }

    pub fn tick_debounce(&mut self, client: PyreDaemonClient) -> bool {
        if !self.open {
            return false;
        }
        let Some(raw) = self.pending_query.take() else {
            return false;
        };
        if self.last_query_at.elapsed() < std::time::Duration::from_millis(150) {
            self.pending_query = Some(raw);
            return false;
        }
        let (query, failures_only) = parse_search_input(&raw);
        self.failures_only = failures_only;
        if query.is_empty() {
            self.results.clear();
            self.status = "empty query".into();
            return true;
        }
        self.status = "searching…".into();
        let (tx, rx) = mpsc::unbounded_channel();
        self.rx = Some(rx);
        tokio::spawn(async move {
            let req = SearchBlocksReq {
                query,
                limit: 20,
                failures_only,
            };
            let hits = client
                .search_blocks(tarpc::context::current(), req)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let _ = tx.send(hits);
        });
        true
    }

    pub fn queue_query(&mut self, raw: String) {
        self.pending_query = Some(raw);
        self.last_query_at = Instant::now();
    }

    pub fn run_query_now(&mut self, client: PyreDaemonClient) {
        let raw = self.input.clone();
        self.queue_query(raw);
        self.pending_query = None;
        self.last_query_at = Instant::now() - std::time::Duration::from_millis(200);
        let _ = self.tick_debounce(client);
    }

    /// Handle a key while the overlay is open. Returns true if the event was consumed.
    pub fn handle_key(&mut self, key: &winit::event::KeyEvent, client: PyreDaemonClient) -> bool {
        use winit::event::ElementState;
        use winit::keyboard::{KeyCode, NamedKey, PhysicalKey};
        if key.state != ElementState::Pressed {
            return true;
        }
        match key.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => {
                self.close();
                return true;
            }
            PhysicalKey::Code(KeyCode::Enter) => {
                self.run_query_now(client);
                return true;
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                self.input.pop();
                self.queue_query(self.input.clone());
                return true;
            }
            PhysicalKey::Code(KeyCode::ArrowUp) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                return true;
            }
            PhysicalKey::Code(KeyCode::ArrowDown) => {
                if !self.results.is_empty() && self.cursor + 1 < self.results.len() {
                    self.cursor += 1;
                }
                return true;
            }
            _ => {}
        }
        match &key.logical_key {
            winit::keyboard::Key::Character(s) => {
                for ch in s.chars() {
                    if !ch.is_control() {
                        self.input.push(ch);
                    }
                }
                self.queue_query(self.input.clone());
            }
            winit::keyboard::Key::Named(NamedKey::Backspace) => {
                self.input.pop();
                self.queue_query(self.input.clone());
            }
            _ => {}
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn paint_overlay(
        &self,
        atlas: &mut GlyphAtlas,
        buffer: &mut [u32],
        buf_w: usize,
        buf_h: usize,
        term_cols: usize,
        term_rows: usize,
    ) {
        if !self.open {
            return;
        }
        let panel_x0 = PANEL_MARGIN * crate::atlas::CELL_W;
        let panel_y0 = PANEL_MARGIN * crate::atlas::CELL_H;
        let panel_x1 = term_cols.saturating_sub(PANEL_MARGIN) * crate::atlas::CELL_W;
        let panel_y1 = term_rows.saturating_sub(PANEL_MARGIN) * crate::atlas::CELL_H;
        let bg = [0x18, 0x18, 0x22];
        let border = [0xff, 0x6b, 0x35];
        let fg = [0xe8, 0xe8, 0xe8];
        let dim = [0x88, 0x88, 0x99];

        fill_rect(
            atlas, buffer, buf_w, buf_h, panel_x0, panel_y0, panel_x1, panel_y1, bg,
        );
        draw_hline(
            atlas, buffer, buf_w, buf_h, panel_x0, panel_y0, panel_x1, border,
        );
        draw_hline(
            atlas,
            buffer,
            buf_w,
            buf_h,
            panel_x0,
            panel_y1.saturating_sub(crate::atlas::CELL_H),
            panel_x1,
            border,
        );

        let mut row = panel_y0 / crate::atlas::CELL_H + 1;
        let col = panel_x0 / crate::atlas::CELL_W + 1;
        draw_line(
            atlas,
            buffer,
            buf_w,
            buf_h,
            col,
            row,
            " search (!prefix = failures) ",
            border,
            bg,
        );
        row += 1;
        draw_line(
            atlas,
            buffer,
            buf_w,
            buf_h,
            col,
            row,
            &format!("> {}", self.input),
            fg,
            bg,
        );
        row += 1;
        draw_line(atlas, buffer, buf_w, buf_h, col, row, &self.status, dim, bg);
        row += 1;

        for (i, hit) in self.results.iter().take(MAX_VISIBLE_RESULTS).enumerate() {
            let sel = i == self.cursor;
            let line_bg = if sel { [0x2a, 0x22, 0x18] } else { bg };
            let cmd: String = hit.block.command.chars().take(32).collect();
            let exit = hit
                .block
                .exit_code
                .map(|c| format!(" exit={c}"))
                .unwrap_or_default();
            let snip: String = hit.snippet.chars().take(48).collect();
            let line = format!(" {cmd}{exit} — {snip}");
            draw_line(atlas, buffer, buf_w, buf_h, col, row, &line, fg, line_bg);
            row += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    bg: [u8; 3],
) {
    let cols = (x1 - x0) / crate::atlas::CELL_W;
    let rows = (y1 - y0) / crate::atlas::CELL_H;
    let col0 = x0 / crate::atlas::CELL_W;
    let row0 = y0 / crate::atlas::CELL_H;
    for r in 0..rows {
        for c in 0..cols {
            atlas.paint_cell(
                buffer,
                buf_w,
                buf_h,
                col0 + c,
                row0 + r,
                ' ',
                [0, 0, 0],
                bg,
                false,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_hline(
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x0: usize,
    _y: usize,
    x1: usize,
    color: [u8; 3],
) {
    let row = _y / crate::atlas::CELL_H;
    let col0 = x0 / crate::atlas::CELL_W;
    let cols = (x1 - x0) / crate::atlas::CELL_W;
    for c in 0..cols {
        atlas.paint_cell(
            buffer,
            buf_w,
            buf_h,
            col0 + c,
            row,
            '─',
            color,
            color,
            false,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_line(
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    col: usize,
    row: usize,
    text: &str,
    fg: [u8; 3],
    bg: [u8; 3],
) {
    for (i, ch) in text.chars().enumerate() {
        if col + i >= buf_w / crate::atlas::CELL_W {
            break;
        }
        atlas.paint_cell(buffer, buf_w, buf_h, col + i, row, ch, fg, bg, false);
    }
}
