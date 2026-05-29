// ─────────────────────────────────────────────────────────────────────────────
// Drag-selection types
// ─────────────────────────────────────────────────────────────────────────────

/// Whether the selection anchor is in the live view or the scrollback buffer.
#[derive(Clone)]
pub enum SelectionBase {
    Live,
    /// window_top: how many lines past the live viewport the drag started,
    /// matching `slot.scroll_offset` at the time drag began.
    Scrollback(usize),
}

/// A text selection spanning a range of (row, col) within a pane.
#[derive(Clone)]
pub struct Selection {
    pub pane_idx: usize,
    /// (row, col) relative to the pane's vt100/content area, viewport-relative.
    pub start: (u16, u16),
    pub end: (u16, u16),
    pub dragging: bool,
    pub base: SelectionBase,
}

impl Selection {
    /// Clamp `start` and `end` so that neither row exceeds `max_row` and
    /// neither col exceeds `max_col`.  Useful after a resize that shrinks
    /// the content area.
    #[allow(dead_code)]
    pub fn clamp_to(&mut self, max_row: u16, max_col: u16) {
        let clamp_point = |(r, c): (u16, u16)| (r.min(max_row), c.min(max_col));
        self.start = clamp_point(self.start);
        self.end = clamp_point(self.end);
    }

    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        let (sr, sc) = self.start;
        let (er, ec) = self.end;
        if (sr, sc) <= (er, ec) {
            ((sr, sc), (er, ec))
        } else {
            ((er, ec), (sr, sc))
        }
    }

    #[allow(dead_code)]
    pub fn contains(&self, row: u16, col: u16) -> bool {
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

use std::time::Instant;

pub struct ClickTracker {
    pub last_at: Instant,
    pub last_pos: (u16, u16), // (col, row) in terminal coordinates
    pub count: u8,
    #[allow(dead_code)]
    pub pane_idx: usize,
}

impl ClickTracker {
    /// Given a new click at `now` / `pos`, return the resulting click count
    /// (1 = single, 2 = double, 3 = triple). Resets to 1 when more than
    /// `window_ms` have passed or the cell position changed.
    pub fn click_count(
        now: Instant,
        last_at: Instant,
        last_pos: (u16, u16),
        new_pos: (u16, u16),
        prev_count: u8,
        window_ms: u64,
    ) -> u8 {
        let elapsed = now.duration_since(last_at).as_millis() as u64;
        if elapsed <= window_ms && last_pos == new_pos {
            prev_count.saturating_add(1).min(3)
        } else {
            1
        }
    }
}
