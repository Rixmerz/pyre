use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use pyre_proto::{Block, PaneId};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
use alacritty_terminal::Term;

// ─────────────────────────────────────────────────────────────────────────────
// EventProxy — bridges alacritty_terminal to pyre I/O
// ─────────────────────────────────────────────────────────────────────────────

/// Bridges alacritty_terminal's EventListener with pyre's async I/O.
/// Queues PtyWrite responses (DSR/CPR) so `render_pane` can drain them.
#[derive(Clone)]
pub struct EventProxy {
    /// Queued PtyWrite responses; drained by PaneSlot::drain_pty_responses.
    queue: Arc<Mutex<Vec<String>>>,
}

impl EventProxy {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drain any accumulated response bytes into a flat Vec<u8>.
    /// Call this after `process_output` and send the collected bytes back to
    /// the daemon input channel so child programs receive their CPR/DSR replies.
    pub fn drain(&self) -> Vec<u8> {
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
// PaneEvent — per-pane stream events
// ─────────────────────────────────────────────────────────────────────────────

/// Events produced by the daemon output stream for a single pane.
pub enum PaneEvent {
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

// ─────────────────────────────────────────────────────────────────────────────
// SplitBoundary / DragState — drag-resize hit-testing
// ─────────────────────────────────────────────────────────────────────────────

/// A boundary between two split children — used for drag-resize hit-testing.
#[derive(Clone)]
pub struct SplitBoundary {
    /// Screen coordinate (column for VSplit, row for HSplit) of the boundary.
    pub coord: u16,
    /// Axis: true = horizontal split (drag row), false = vertical split (drag col).
    pub is_hsplit: bool,
    /// Path to the parent split node.
    pub parent_path: Vec<usize>,
    /// Index of the LEFT/TOP child (the boundary is between child_idx and child_idx+1).
    pub child_idx: usize,
    /// Total size of the parent in the split axis (height for HSplit, width for VSplit).
    pub parent_size: u16,
}

/// Active drag state.
pub struct DragState {
    pub boundary: SplitBoundary,
    /// Terminal coordinate (col or row depending on axis) where drag began.
    pub start_coord: u16,
    /// Weights of all children in the parent split at drag start.
    pub start_weights: Vec<u16>,
}

// ─────────────────────────────────────────────────────────────────────────────
// PaneSlot — one attached PTY pane
// ─────────────────────────────────────────────────────────────────────────────

/// One attached PTY pane with its I/O channels and VT parser.
pub struct PaneSlot {
    pub pane_id: PaneId,
    /// alacritty_terminal state machine — handles alt-screen, DSR/CPR, mouse.
    pub term: Term<EventProxy>,
    /// VTE ANSI byte-stream processor that feeds bytes into `term`.
    pub processor: AnsiProcessor,
    /// Event proxy shared with `term`; drained after each process_output call
    /// to forward CPR/DSR replies back to the child PTY.
    pub event_proxy: EventProxy,
    /// Bytes to send to this pane (written by the key handler).
    pub input_tx: mpsc::Sender<Bytes>,
    /// Events from daemon for this pane (drained each UI tick).
    pub output_rx: mpsc::Receiver<PaneEvent>,
    /// Last polled block list for the ribbon (up to 20 entries, newest last).
    pub recent_blocks: Vec<Block>,
    /// `None` = live (rightmost highlighted); `Some(i)` = scrollback cursor.
    pub ribbon_cursor: Option<usize>,

    /// Last PTY size successfully sent to the daemon, to avoid spamming per frame.
    pub last_sent_size: (u16, u16),

    /// Number of OutputFrame messages received from the daemon on this stream.
    /// Used to distinguish a connection-level failure (zero frames → do not fire
    /// close_pane RPC) from a legitimate pane exit (≥1 frames → fire close_pane).
    pub frames_received: u64,

    /// 0 = live view; N = N lines scrolled back via vt100 native scrollback.
    pub scroll_offset: usize,
    /// Total scrollback lines available as of the last render (cached via peek/restore).
    /// vt100::Screen::scrollback() returns the *current offset*, not the capacity;
    /// we peek by setting MAX and reading the clamped value, then restore.
    pub scrollback_capacity: usize,
    /// The screen rect captured during the last render, used for mouse hit-test.
    pub last_screen_rect: Rect,
    /// Ribbon chip rects captured during last render: (block_idx, rect).
    pub ribbon_chip_rects: Vec<(usize, Rect)>,
    /// Output bytes received before the first render (parser not yet sized to
    /// the real pane area). Drained into the parser on the first render frame.
    pub pending_output: Vec<u8>,
    /// True once the parser has been sized to the actual pane area and
    /// `pending_output` has been flushed. Set on the first `render_pane` call.
    pub parser_sized: bool,
    /// Timestamp of the last `process_output` debug log emission (50 ms throttle).
    pub last_output_log: Option<Instant>,
}

impl PaneSlot {
    /// Feed raw bytes into the alacritty Term processor.
    /// If the terminal has not yet been sized to the real pane area (before the
    /// first render frame), bytes are buffered in `pending_output` instead of
    /// being processed at the wrong terminal dimensions. `render_pane` drains
    /// the buffer once it knows the correct area size.
    pub fn process_output(&mut self, data: &[u8]) {
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
    pub fn drain_pty_responses(&self) -> Vec<u8> {
        self.event_proxy.drain()
    }
}
