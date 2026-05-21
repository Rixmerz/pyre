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

pub fn ansi_to_rgb(color: AnsiColor) -> [u8; 3] {
    match color {
        AnsiColor::Named(NamedColor::Black) => [0x1a, 0x1a, 0x1a],
        AnsiColor::Named(NamedColor::Red) => [0xff, 0x55, 0x55],
        AnsiColor::Named(NamedColor::Green) => [0x50, 0xfa, 0x7b],
        AnsiColor::Named(NamedColor::Yellow) => [0xff, 0xd7, 0x00],
        AnsiColor::Named(NamedColor::Blue) => [0x6b, 0x9e, 0xff],
        AnsiColor::Named(NamedColor::Magenta) => [0xff, 0x79, 0xc6],
        AnsiColor::Named(NamedColor::Cyan) => [0x8b, 0xe9, 0xfd],
        AnsiColor::Named(NamedColor::White) => [0xf8, 0xf8, 0xf2],
        AnsiColor::Named(NamedColor::BrightBlack) => [0x62, 0x62, 0x62],
        AnsiColor::Named(NamedColor::BrightRed) => [0xff, 0x6e, 0x6e],
        AnsiColor::Named(NamedColor::BrightGreen) => [0x69, 0xff, 0x94],
        AnsiColor::Named(NamedColor::BrightYellow) => [0xff, 0xff, 0xa5],
        AnsiColor::Named(NamedColor::BrightBlue) => [0x9c, 0xbd, 0xff],
        AnsiColor::Named(NamedColor::BrightMagenta) => [0xff, 0x92, 0xdf],
        AnsiColor::Named(NamedColor::BrightCyan) => [0xa4, 0xff, 0xff],
        AnsiColor::Named(NamedColor::BrightWhite) => [0xff, 0xff, 0xff],
        AnsiColor::Spec(rgb) => [rgb.r, rgb.g, rgb.b],
        AnsiColor::Indexed(i) => {
            // xterm 256-color cube approximation for indices 16..231
            if (16..=231).contains(&i) {
                let i = i - 16;
                let r = (i / 36) * 51;
                let g = ((i / 6) % 6) * 51;
                let b = (i % 6) * 51;
                [r, g, b]
            } else if (232..=255).contains(&i) {
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            } else {
                [0xc8, 0xc8, 0xc8]
            }
        }
        _ => [0xc8, 0xc8, 0xc8],
    }
}

pub fn collect_grid(view: &TermView, cols: usize, rows: usize) -> Vec<CellRgb> {
    let grid = view.term.grid();
    let num_rows = grid.screen_lines();
    let num_cols = grid.columns();
    let mut cells = Vec::with_capacity(cols * rows);
    let default_bg = [0x0d, 0x0d, 0x0d];
    let default_fg = [0xc8, 0xc8, 0xc8];

    for row in 0..rows {
        let display_line = TermLine(row as i32 - grid.display_offset() as i32);
        for col in 0..cols {
            let (ch, fg, bg, bold) = if row < num_rows && col < num_cols {
                let cell = &grid[TermPoint::new(display_line, TermColumn(col))];
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                (
                    ch,
                    ansi_to_rgb(cell.fg),
                    ansi_to_rgb(cell.bg),
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
