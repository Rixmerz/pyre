//! Minimal alacritty_terminal wrapper for the GPU viewer (single pane).

#![allow(dead_code)]

use std::sync::mpsc;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::{cell::Flags as CellFlags, Config as TermConfig};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor as AnsiProcessor};
use alacritty_terminal::Term;
use bytes::Bytes;

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

pub(crate) struct EventProxy {
    tx: mpsc::Sender<Bytes>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        if let TermEvent::PtyWrite(s) = event {
            let _ = self.tx.send(Bytes::from(s));
        }
    }
}

pub struct TermView {
    pub term: Term<EventProxy>,
    pub processor: AnsiProcessor,
    pub proxy_rx: mpsc::Receiver<Bytes>,
    pending_output: Vec<u8>,
    pub parser_sized: bool,
}

impl TermView {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (tx, rx) = mpsc::channel();
        let proxy = EventProxy { tx };
        let term_config = TermConfig::default();
        let term = Term::new(term_config, &TermSize::new(cols, rows), proxy);
        Self {
            term,
            processor: AnsiProcessor::new(),
            proxy_rx: rx,
            pending_output: Vec::new(),
            parser_sized: false,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(TermSize::new(cols, rows));
    }

    pub fn push_bytes(&mut self, data: &[u8]) {
        if self.parser_sized {
            self.processor.advance(&mut self.term, data);
        } else {
            self.pending_output.extend_from_slice(data);
        }
    }

    pub fn flush_pending(&mut self) {
        if !self.parser_sized {
            self.parser_sized = true;
            if !self.pending_output.is_empty() {
                let buf = std::mem::take(&mut self.pending_output);
                self.processor.advance(&mut self.term, &buf);
            }
        }
    }

    pub fn drain_pty_replies(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        while let Ok(b) = self.proxy_rx.try_recv() {
            out.push(b);
        }
        out
    }

    pub fn scroll_display_bottom(&mut self) {
        self.term.grid_mut().scroll_display(Scroll::Bottom);
    }
}

#[derive(Clone, Copy)]
pub struct CellRgb {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
}

/// Resolve an ANSI [`AnsiColor`] to `[r, g, b]`.
///
/// The 16 named colours (indices 0-15) are drawn from `palette.ansi` so they
/// match the active theme.  256-colour cube (16-231) and greyscale ramp
/// (232-255) paths are computed arithmetically and are not theme-driven, as
/// they represent explicit colour requests from the application.  Truecolor
/// (`Spec`) passes through verbatim.
pub fn ansi_to_rgb(color: AnsiColor, palette: &pyre_themes::Palette) -> [u8; 3] {
    match color {
        AnsiColor::Named(named) => {
            let idx: usize = match named {
                NamedColor::Black => 0,
                NamedColor::Red => 1,
                NamedColor::Green => 2,
                NamedColor::Yellow => 3,
                NamedColor::Blue => 4,
                NamedColor::Magenta => 5,
                NamedColor::Cyan => 6,
                NamedColor::White => 7,
                NamedColor::BrightBlack => 8,
                NamedColor::BrightRed => 9,
                NamedColor::BrightGreen => 10,
                NamedColor::BrightYellow => 11,
                NamedColor::BrightBlue => 12,
                NamedColor::BrightMagenta => 13,
                NamedColor::BrightCyan => 14,
                NamedColor::BrightWhite => 15,
                // Non-ANSI named colours (cursor, foreground, background, etc.)
                // fall back to the palette fg/bg as appropriate.
                NamedColor::Foreground => {
                    let c = palette.fg;
                    return [c.0, c.1, c.2];
                }
                NamedColor::Background => {
                    let c = palette.bg;
                    return [c.0, c.1, c.2];
                }
                _ => {
                    let c = palette.fg;
                    return [c.0, c.1, c.2];
                }
            };
            let c = palette.ansi[idx];
            [c.0, c.1, c.2]
        }
        AnsiColor::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        AnsiColor::Indexed(i) => {
            // Re-map indices 0-15 through the palette ANSI table.
            if i < 16 {
                let c = palette.ansi[i as usize];
                [c.0, c.1, c.2]
            } else if (16..=231).contains(&i) {
                // xterm 256-colour cube approximation.
                let i = i - 16;
                let r = (i / 36) * 51;
                let g = ((i / 6) % 6) * 51;
                let b = (i % 6) * 51;
                [r, g, b]
            } else if (232..=255).contains(&i) {
                // Greyscale ramp.
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            } else {
                let c = palette.fg;
                [c.0, c.1, c.2]
            }
        }
    }
}

pub fn collect_grid(
    view: &TermView,
    cols: usize,
    rows: usize,
    palette: &pyre_themes::Palette,
) -> Vec<CellRgb> {
    let grid = view.term.grid();
    let num_rows = grid.screen_lines();
    let num_cols = grid.columns();
    let mut cells = Vec::with_capacity(cols * rows);
    let default_bg = {
        let c = palette.bg;
        [c.0, c.1, c.2]
    };
    let default_fg = {
        let c = palette.fg;
        [c.0, c.1, c.2]
    };

    for row in 0..rows {
        let display_line = TermLine(row as i32 - grid.display_offset() as i32);
        for col in 0..cols {
            let (ch, fg, bg, bold) = if row < num_rows && col < num_cols {
                let cell = &grid[TermPoint::new(display_line, TermColumn(col))];
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                (
                    ch,
                    ansi_to_rgb(cell.fg, palette),
                    ansi_to_rgb(cell.bg, palette),
                    cell.flags.contains(CellFlags::BOLD),
                )
            } else {
                (' ', default_fg, default_bg, false)
            };
            cells.push(CellRgb { ch, fg, bg, bold });
        }
    }
    cells
}
