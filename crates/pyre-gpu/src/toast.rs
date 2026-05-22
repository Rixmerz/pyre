//! Toast notification subsystem for pyre-gpu.
//!
//! Mirrors the TUI's `ToastDeck` / `Toast` / `ToastKind` types.
//! Rendering is done via softbuffer into the RGBA framebuffer rather than
//! ratatui widgets.

use std::time::{Duration, Instant};

use crate::atlas::{GlyphAtlas, CELL_H, CELL_W};

/// Visual severity of a toast; drives accent colour selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

/// One ephemeral notification card.
#[derive(Debug, Clone)]
pub struct Toast {
    /// Bold title, e.g. "a1b2c3d4".
    pub title: String,
    /// Body message, e.g. "Waiting for input".
    pub body: String,
    pub kind: ToastKind,
    pub born_at: Instant,
    pub ttl: Duration,
}

impl Toast {
    /// Fraction of TTL remaining `[0.0, 1.0]`.
    pub fn remaining_fraction(&self) -> f32 {
        let elapsed = self.born_at.elapsed();
        if elapsed >= self.ttl {
            0.0
        } else {
            1.0 - (elapsed.as_secs_f32() / self.ttl.as_secs_f32())
        }
    }

    pub fn is_expired(&self) -> bool {
        self.born_at.elapsed() >= self.ttl
    }
}

/// Stack of live toasts rendered bottom-right of the framebuffer.
pub struct ToastDeck {
    pub toasts: std::collections::VecDeque<Toast>,
    pub max_visible: usize,
    pub enabled: bool,
    pub ttl: Duration,
}

impl ToastDeck {
    pub fn new(enabled: bool, ttl_ms: u64, max_visible: usize) -> Self {
        Self {
            toasts: std::collections::VecDeque::new(),
            max_visible,
            enabled,
            ttl: Duration::from_millis(ttl_ms),
        }
    }

    /// Push a new toast; trims oldest when over `max_visible`.
    pub fn push(&mut self, title: String, body: String, kind: ToastKind) {
        if !self.enabled {
            return;
        }
        let toast = Toast {
            title,
            body,
            kind,
            born_at: Instant::now(),
            ttl: self.ttl,
        };
        self.toasts.push_back(toast);
        while self.toasts.len() > self.max_visible {
            self.toasts.pop_front();
        }
    }

    /// Drop expired toasts. Call once per frame.
    pub fn tick(&mut self) {
        self.toasts.retain(|t| !t.is_expired());
    }

    /// Returns `true` if there are any live toasts.
    pub fn has_live(&self) -> bool {
        self.enabled && !self.toasts.is_empty()
    }
}

/// Map a `pyre_proto::PaneEvent` to an optional `Toast`.
/// Returns `None` for Idle/Running (spam suppression).
pub fn pane_event_to_toast(event: &pyre_proto::PaneEvent, ttl: Duration) -> Option<Toast> {
    use pyre_proto::{PaneEventKind, PaneStateKind};

    let short: String = event.pane_id.chars().take(8).collect();
    let agent_label = event
        .agent
        .map(|a| format!(" ({})", a.label()))
        .unwrap_or_default();
    let title = format!("{short}{agent_label}");

    let (body, kind) = match event.kind {
        PaneEventKind::Spawned => ("Spawned".to_owned(), ToastKind::Info),
        PaneEventKind::Closed => ("Closed".to_owned(), ToastKind::Info),
        PaneEventKind::StateChanged => match event.state {
            Some(PaneStateKind::WaitingInput) => ("Waiting for input".to_owned(), ToastKind::Warn),
            Some(PaneStateKind::Done) => ("Done".to_owned(), ToastKind::Success),
            Some(PaneStateKind::Crashed) => ("Failed".to_owned(), ToastKind::Error),
            // Idle and Running are high-frequency — suppress.
            Some(PaneStateKind::Idle) | Some(PaneStateKind::Running) => return None,
            _ => return None,
        },
        // Layout topology changes do not produce a toast — clients re-fetch
        // via get_session_layout on this event.
        PaneEventKind::LayoutChanged => return None,
    };

    Some(Toast {
        title,
        body,
        kind,
        born_at: Instant::now(),
        ttl,
    })
}

// ─── Internal pixel helpers ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    px_x: usize,
    px_y: usize,
    w: usize,
    h: usize,
    pixel: u32,
) {
    for row in 0..h {
        let y = px_y + row;
        if y >= buf_h {
            break;
        }
        for col in 0..w {
            let x = px_x + col;
            if x < buf_w {
                buffer[y * buf_w + x] = pixel;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_border_rect(
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    px_x: usize,
    px_y: usize,
    w: usize,
    h: usize,
    pixel: u32,
) {
    for x in 0..w {
        let bx = px_x + x;
        if bx < buf_w {
            if px_y < buf_h {
                buffer[px_y * buf_w + bx] = pixel;
            }
            let bot = px_y + h.saturating_sub(1);
            if bot < buf_h {
                buffer[bot * buf_w + bx] = pixel;
            }
        }
    }
    for y in 0..h {
        let by = px_y + y;
        if by < buf_h {
            if px_x < buf_w {
                buffer[by * buf_w + px_x] = pixel;
            }
            let rx = px_x + w.saturating_sub(1);
            if rx < buf_w {
                buffer[by * buf_w + rx] = pixel;
            }
        }
    }
}

fn rgb_u32(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Paint a text string using the glyph atlas.
/// `cell_col` and `cell_row` are in cell (grid) coordinates, not pixels.
/// `fg` and `bg` are RGB triples.
#[allow(clippy::too_many_arguments)]
fn paint_text_row(
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    text: &str,
    cell_col: usize,
    cell_row: usize,
    fg: [u8; 3],
    bg: [u8; 3],
) {
    for (i, ch) in text.chars().enumerate() {
        atlas.paint_cell(
            buffer,
            buf_w,
            buf_h,
            cell_col + i,
            cell_row,
            ch,
            fg,
            bg,
            false,
        );
    }
}

// ─── Card dimensions ──────────────────────────────────────────────────────────

/// Card width in cell units.
const CARD_W_CELLS: usize = 42;
/// Card height in cell units (1 border + 1 text + 1 border).
const CARD_H_CELLS: usize = 3;
const CARD_GAP_CELLS: usize = 1;
const MARGIN_RIGHT_CELLS: usize = 1;
const MARGIN_BOTTOM_CELLS: usize = 1;

/// Paint the toast deck into the RGBA framebuffer.
///
/// Cards are placed bottom-right, stacking upward. Each card is `CARD_W_CELLS`
/// × `CARD_H_CELLS` cells, with a 1-pixel border. The active theme palette
/// selects background and per-severity accent colours.
pub fn paint_toast_deck(
    deck: &ToastDeck,
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    palette: &pyre_themes::Palette,
) {
    if !deck.enabled || deck.toasts.is_empty() {
        return;
    }

    let card_w_px = CARD_W_CELLS * CELL_W;
    let card_h_px = CARD_H_CELLS * CELL_H;
    let gap_px = CARD_GAP_CELLS * CELL_H;
    let margin_right_px = MARGIN_RIGHT_CELLS * CELL_W;
    let margin_bottom_px = MARGIN_BOTTOM_CELLS * CELL_H;

    let bg_rgb = [palette.bg.0, palette.bg.1, palette.bg.2];
    let bg_u32 = rgb_u32(bg_rgb[0], bg_rgb[1], bg_rgb[2]);

    let card_x_px = buf_w.saturating_sub(card_w_px + margin_right_px);

    let visible: Vec<&Toast> = deck.toasts.iter().rev().take(deck.max_visible).collect();

    for (i, toast) in visible.iter().enumerate() {
        let card_bottom_px = buf_h.saturating_sub(margin_bottom_px + i * (card_h_px + gap_px));
        if card_bottom_px < card_h_px {
            break;
        }
        let card_y_px = card_bottom_px - card_h_px;

        let accent = match toast.kind {
            ToastKind::Info => palette.accent,
            ToastKind::Success => palette.ok,
            ToastKind::Warn => palette.warn,
            ToastKind::Error => palette.error,
        };
        let accent_rgb = [accent.0, accent.1, accent.2];
        let accent_u32 = rgb_u32(accent.0, accent.1, accent.2);

        // Fill card background.
        fill_rect(
            buffer, buf_w, buf_h, card_x_px, card_y_px, card_w_px, card_h_px, bg_u32,
        );
        // Draw 1-pixel border.
        draw_border_rect(
            buffer, buf_w, buf_h, card_x_px, card_y_px, card_w_px, card_h_px, accent_u32,
        );

        // Text content row: cell row 1 (inside top border), column 1 (inside left border).
        // Atlas.paint_cell works in integer cell coordinates.
        let base_cell_col = (card_x_px + CELL_W) / CELL_W; // skip left border column
        let text_cell_row = (card_y_px / CELL_H) + 1; // skip top border row

        let label = format!("{} | {}", toast.title, toast.body);
        let max_chars = CARD_W_CELLS.saturating_sub(2);
        let text: String = label.chars().take(max_chars).collect();

        paint_text_row(
            atlas,
            buffer,
            buf_w,
            buf_h,
            &text,
            base_cell_col,
            text_cell_row,
            accent_rgb,
            bg_rgb,
        );

        // Progress bar: overwrite the bottom border row with a filled fraction.
        let frac = toast.remaining_fraction();
        let bar_px_w = ((card_w_px as f32) * frac) as usize;
        let bar_y = card_y_px + card_h_px - 1;
        if bar_y < buf_h {
            for bx in 0..bar_px_w.min(card_w_px) {
                let x = card_x_px + bx;
                if x < buf_w {
                    buffer[bar_y * buf_w + x] = accent_u32;
                }
            }
        }
    }
}

// ─── Context menu ─────────────────────────────────────────────────────────────

/// Items mirroring the TUI's `MenuItem` enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuItem {
    Copy,
    KillPane,
    SplitH,
    SplitV,
    ZoomToggle,
    InspectPid,
}

impl MenuItem {
    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => " Copy selection",
            Self::KillPane => " Kill pane",
            Self::SplitH => " Split horizontal",
            Self::SplitV => " Split vertical",
            Self::ZoomToggle => " Zoom toggle",
            Self::InspectPid => " Inspect PID",
        }
    }
}

pub const MENU_ITEMS: &[MenuItem] = &[
    MenuItem::Copy,
    MenuItem::KillPane,
    MenuItem::SplitH,
    MenuItem::SplitV,
    MenuItem::ZoomToggle,
    MenuItem::InspectPid,
];

/// Pixel-space context menu state.
#[derive(Debug, Clone)]
pub struct ContextMenu {
    /// Top-left pixel position.
    pub px_x: usize,
    pub px_y: usize,
    /// Width/height in pixels (pre-computed).
    pub width: usize,
    pub height: usize,
    /// Currently highlighted row (0-based).
    pub cursor: usize,
    /// The `PaneId` that was right-clicked.
    #[allow(dead_code)] // used by future commit_context_menu clipboard path
    pub target_pane: pyre_proto::PaneId,
}

impl ContextMenu {
    /// Compute menu dimensions and clamp position to the framebuffer.
    pub fn new(
        cursor_px_x: usize,
        cursor_px_y: usize,
        target_pane: pyre_proto::PaneId,
        buf_w: usize,
        buf_h: usize,
    ) -> Self {
        let max_label_chars = MENU_ITEMS
            .iter()
            .map(|i| i.label().chars().count())
            .max()
            .unwrap_or(10);
        let w = (max_label_chars + 2) * CELL_W;
        let h = (MENU_ITEMS.len() + 2) * CELL_H; // +2 border rows
        let px_x = cursor_px_x.min(buf_w.saturating_sub(w));
        let px_y = cursor_px_y.min(buf_h.saturating_sub(h));
        Self {
            px_x,
            px_y,
            width: w,
            height: h,
            cursor: 0,
            target_pane,
        }
    }

    /// Returns `true` if the pixel `(px, py)` is inside the menu rect.
    #[allow(dead_code)]
    pub fn contains(&self, px: usize, py: usize) -> bool {
        px >= self.px_x
            && px < self.px_x + self.width
            && py >= self.px_y
            && py < self.px_y + self.height
    }

    /// Returns the item index hovered at pixel `(px, py)`, if any.
    #[allow(dead_code)]
    pub fn item_at(&self, px: usize, py: usize) -> Option<usize> {
        if !self.contains(px, py) {
            return None;
        }
        // Skip top-border cell row.
        let rel_y = py.saturating_sub(self.px_y + CELL_H);
        let row = rel_y / CELL_H;
        if row < MENU_ITEMS.len() {
            Some(row)
        } else {
            None
        }
    }

    pub fn item_at_cursor(&self) -> Option<MenuItem> {
        MENU_ITEMS.get(self.cursor).copied()
    }
}

/// Paint the context menu into `buffer`.
pub fn paint_context_menu(
    menu: &ContextMenu,
    atlas: &mut GlyphAtlas,
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    palette: &pyre_themes::Palette,
) {
    let bg_rgb = [palette.bg.0, palette.bg.1, palette.bg.2];
    let bg_u32 = rgb_u32(bg_rgb[0], bg_rgb[1], bg_rgb[2]);
    let border_u32 = rgb_u32(
        palette.border_focus.0,
        palette.border_focus.1,
        palette.border_focus.2,
    );
    let accent_rgb = [palette.accent.0, palette.accent.1, palette.accent.2];
    let accent_u32 = rgb_u32(palette.accent.0, palette.accent.1, palette.accent.2);

    // Background + border.
    fill_rect(
        buffer,
        buf_w,
        buf_h,
        menu.px_x,
        menu.px_y,
        menu.width,
        menu.height,
        bg_u32,
    );
    draw_border_rect(
        buffer,
        buf_w,
        buf_h,
        menu.px_x,
        menu.px_y,
        menu.width,
        menu.height,
        border_u32,
    );

    let base_cell_col = (menu.px_x + CELL_W) / CELL_W;
    let base_cell_row = menu.px_y / CELL_H + 1; // skip top border row

    for (i, item) in MENU_ITEMS.iter().enumerate() {
        let row = base_cell_row + i;
        let is_selected = i == menu.cursor;

        let (fg, row_bg_u32) = if is_selected {
            (bg_rgb, accent_u32)
        } else {
            ([palette.fg.0, palette.fg.1, palette.fg.2], bg_u32)
        };
        let row_bg_rgb = [
            ((row_bg_u32 >> 16) & 0xff) as u8,
            ((row_bg_u32 >> 8) & 0xff) as u8,
            (row_bg_u32 & 0xff) as u8,
        ];

        // Fill row background.
        let row_px_y = row * CELL_H;
        fill_rect(
            buffer,
            buf_w,
            buf_h,
            menu.px_x + 1,
            row_px_y,
            menu.width.saturating_sub(2),
            CELL_H,
            row_bg_u32,
        );

        let label: String = item.label().chars().take(menu.width / CELL_W - 1).collect();
        paint_text_row(
            atlas,
            buffer,
            buf_w,
            buf_h,
            &label,
            base_cell_col,
            row,
            fg,
            row_bg_rgb,
        );
    }

    // Suppress unused warning for accent_rgb when no selected row.
    let _ = accent_rgb;
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_deck_push_and_tick() {
        let mut deck = ToastDeck::new(true, 500, 3);
        deck.push("title".into(), "body".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 1);
        deck.tick(); // not expired yet
        assert_eq!(deck.toasts.len(), 1);
    }

    #[test]
    fn toast_deck_disabled_suppresses() {
        let mut deck = ToastDeck::new(false, 500, 3);
        deck.push("title".into(), "body".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 0);
    }

    #[test]
    fn toast_deck_max_visible_trim() {
        let mut deck = ToastDeck::new(true, 5000, 2);
        deck.push("a".into(), "a".into(), ToastKind::Info);
        deck.push("b".into(), "b".into(), ToastKind::Info);
        deck.push("c".into(), "c".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 2);
        assert_eq!(deck.toasts.back().unwrap().title, "c");
    }

    #[test]
    fn toast_remaining_fraction_full_at_start() {
        let t = Toast {
            title: String::new(),
            body: String::new(),
            kind: ToastKind::Info,
            born_at: Instant::now(),
            ttl: Duration::from_secs(10),
        };
        let frac = t.remaining_fraction();
        assert!(
            frac > 0.99,
            "brand-new toast should be near 1.0, got {frac}"
        );
    }

    #[test]
    fn context_menu_item_at() {
        let pane_id = pyre_proto::PaneId::new();
        let menu = ContextMenu::new(100, 100, pane_id, 1200, 800);
        // Inside the first item row.
        let item = menu.item_at(menu.px_x + 5, menu.px_y + CELL_H + 2);
        assert_eq!(item, Some(0));
        // Outside entirely.
        assert!(menu.item_at(0, 0).is_none());
    }
}
