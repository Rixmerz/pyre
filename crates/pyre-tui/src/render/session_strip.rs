//! Session strip — the top row showing all sessions as scrollable pills.
//!
//! Extracted from `draw_frame` in main.rs (Wave 1D refactor).

use crate::fire_motion;
use crate::theme;
use crate::AppState;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::render::sidebar::{agent_ui_label, session_worst_pane};
use pyre_proto::PaneStateKind;

/// Render the session strip (row 0 of the outer layout) into `area`.
///
/// Mutates `state` to update scroll, hit-test rects, and arrow positions.
pub fn render_session_strip(
    frame: &mut Frame,
    area: Rect,
    state: &mut AppState,
    t: &theme::LegacyTheme,
) {
    // Arrow indicator width: 1 column each.
    const ARROW_W: u16 = 1;
    let viewport_w = area.width as usize;

    // Build all pill labels and compute their natural (unscrolled) widths.
    // pill_items: (session_index, label, width, style)
    struct PillItem {
        sess_idx: usize,
        label: String,
        width: usize,
        style: Style,
    }
    let mut pill_items: Vec<PillItem> = Vec::new();
    let anim_f = state.anim.frame();
    for (i, sv) in state.sessions.iter().enumerate() {
        let rollup = session_worst_pane(&state.sidebar_data, sv.id);
        let rollup_tag = rollup
            .map(|p| format!(":{}", agent_ui_label(p.state, p.seen)))
            .unwrap_or_default();
        let label = format!(" {} {}{} ", i + 1, sv.name, rollup_tag);
        let len = label.chars().count();
        let needs_attention =
            rollup.is_some_and(|p| p.state == PaneStateKind::WaitingInput && !p.seen);
        let style = if i == state.active_session {
            t.tab_active()
        } else if needs_attention {
            fire_motion::ember_fg_style(anim_f, sv.id.0.as_u128() as u32, t.spark, t.primary, 1.0)
                .bg(t.bg)
        } else {
            t.tab_inactive()
        };
        pill_items.push(PillItem {
            sess_idx: i,
            label,
            width: len,
            style,
        });
    }

    // Compute cumulative column offsets (virtual, unscrolled).
    // Each pill is followed by a 1-column separator space (except the last).
    // Then " [+]" (1 space + 3 chars = 4 cols) at the end.
    let mut offsets: Vec<usize> = Vec::with_capacity(pill_items.len());
    let mut col_cur: usize = 0;
    for (idx, item) in pill_items.iter().enumerate() {
        offsets.push(col_cur);
        col_cur += item.width;
        if idx + 1 < pill_items.len() {
            col_cur += 1; // separator space
        }
    }
    // [+] button: 1 space separator + 3 chars = 4 wide.
    let plus_virtual_x = col_cur + 1; // +1 space before [+]
    let total_virtual_w = plus_virtual_x + 3; // "[+]"

    // Auto-scroll: bring the active session pill into view.
    // Available viewport columns after reserving space for arrows.
    let needs_left_arrow = state.session_strip_scroll > 0;
    let needs_right_arrow = total_virtual_w > viewport_w + state.session_strip_scroll;
    // Reserve arrow slots when they will be shown.
    let left_reserved: usize = if needs_left_arrow {
        ARROW_W as usize
    } else {
        0
    };
    let right_reserved: usize = if needs_right_arrow {
        ARROW_W as usize
    } else {
        0
    };
    let visible_w = viewport_w.saturating_sub(left_reserved + right_reserved);

    if !pill_items.is_empty() {
        let active = state.active_session.min(pill_items.len() - 1);
        let pill_start = offsets[active];
        let pill_end = pill_start + pill_items[active].width;
        // Scroll left if pill start is behind the left viewport edge.
        if pill_start < state.session_strip_scroll + left_reserved {
            state.session_strip_scroll = pill_start.saturating_sub(left_reserved);
        }
        // Scroll right if pill end is beyond the right viewport edge.
        let view_end = state.session_strip_scroll + left_reserved + visible_w;
        if pill_end > view_end {
            state.session_strip_scroll = pill_end
                .saturating_sub(visible_w)
                .saturating_sub(left_reserved);
        }
    }
    // Clamp scroll so we don't over-scroll past content.
    let max_scroll = total_virtual_w.saturating_sub(viewport_w);
    state.session_strip_scroll = state.session_strip_scroll.min(max_scroll);

    // Recompute arrow visibility after potential scroll adjustment.
    let needs_left_arrow = state.session_strip_scroll > 0;
    let needs_right_arrow = total_virtual_w > viewport_w + state.session_strip_scroll;
    let left_reserved: usize = if needs_left_arrow {
        ARROW_W as usize
    } else {
        0
    };
    let right_reserved: usize = if needs_right_arrow {
        ARROW_W as usize
    } else {
        0
    };
    let content_start_col = area.x + left_reserved as u16;
    let content_viewport_w = viewport_w.saturating_sub(left_reserved + right_reserved);

    // Render left arrow.
    let left_arrow_rect = if needs_left_arrow && area.height > 0 {
        Some(Rect::new(area.x, area.y, ARROW_W, 1))
    } else {
        None
    };
    if let Some(r) = left_arrow_rect {
        frame.render_widget(
            Paragraph::new("◄").style(Style::default().fg(t.text_dim).bg(t.bg)),
            r,
        );
    }

    // Render right arrow.
    let right_arrow_x = area.x + area.width - ARROW_W;
    let right_arrow_rect = if needs_right_arrow && area.height > 0 {
        Some(Rect::new(right_arrow_x, area.y, ARROW_W, 1))
    } else {
        None
    };
    if let Some(r) = right_arrow_rect {
        frame.render_widget(
            Paragraph::new("►").style(Style::default().fg(t.text_dim).bg(t.bg)),
            r,
        );
    }

    // Build visible spans within [scroll, scroll + content_viewport_w).
    let scroll = state.session_strip_scroll;
    let mut new_session_rects: Vec<(usize, Rect)> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    // Virtual x position in content space (relative to scroll origin).
    let mut vx: usize = 0;

    for (idx, item) in pill_items.iter().enumerate() {
        let pill_vstart = offsets[idx];
        let pill_vend = pill_vstart + item.width;

        // Skip pills entirely to the left of the viewport.
        if pill_vend <= scroll {
            vx = pill_vend;
            if idx + 1 < pill_items.len() {
                vx += 1;
            }
            continue;
        }
        // Stop when the pill starts past the right edge.
        if pill_vstart >= scroll + content_viewport_w {
            break;
        }

        // Add separator space between pills if needed.
        if idx > 0 && vx > scroll {
            let sep_screen_col = content_start_col + (vx - scroll) as u16;
            let _ = sep_screen_col; // drawn via span below
            spans.push(Span::styled(" ", Style::default().bg(t.bg)));
            vx += 1;
        } else if idx > 0 {
            // The separator was scrolled off; move vx forward.
            vx += 1;
        }

        // Clip the label to the visible window.
        let label_chars: Vec<char> = item.label.chars().collect();
        let clip_start = scroll.saturating_sub(vx);
        let clip_end = (scroll + content_viewport_w).saturating_sub(vx);
        let clip_end = clip_end.min(label_chars.len());
        let visible_label: String = label_chars[clip_start..clip_end].iter().collect();
        let visible_len = visible_label.chars().count() as u16;

        // Compute screen rect for hit-test (maps to full pill, even if clipped).
        // We store the screen rect for the visible portion so clicks land correctly.
        let screen_x = content_start_col + (vx + clip_start).saturating_sub(scroll) as u16;
        if area.height > 0 && visible_len > 0 {
            new_session_rects.push((item.sess_idx, Rect::new(screen_x, area.y, visible_len, 1)));
        }

        spans.push(Span::styled(visible_label, item.style));
        vx = pill_vend;
    }

    // [+] button — show only if it fits in the viewport.
    let plus_visible_start = plus_virtual_x.saturating_sub(scroll);
    let plus_visible_end = plus_virtual_x + 3;
    let plus_rect = if area.height > 0
        && plus_visible_end > scroll
        && plus_virtual_x < scroll + content_viewport_w
    {
        let plus_screen_x = content_start_col + plus_visible_start as u16;
        // Add separator space before [+] when it fits.
        if plus_virtual_x > scroll && vx <= scroll + content_viewport_w {
            spans.push(Span::styled(" ", Style::default().bg(t.bg)));
        }
        let clip_s = scroll.saturating_sub(plus_virtual_x);
        let clip_e = (scroll + content_viewport_w)
            .saturating_sub(plus_virtual_x)
            .min(3);
        let plus_chars: Vec<char> = "[+]".chars().collect();
        let plus_visible: String = plus_chars[clip_s..clip_e].iter().collect();
        let plus_w = plus_visible.chars().count() as u16;
        spans.push(Span::styled(plus_visible, t.tab_inactive()));
        if plus_w > 0 {
            Some(Rect::new(plus_screen_x, area.y, plus_w, 1))
        } else {
            None
        }
    } else {
        None
    };

    state.session_strip_rects = new_session_rects;
    state.session_strip_left_arrow = left_arrow_rect;
    state.session_strip_right_arrow = right_arrow_rect;
    state.session_plus_rect = plus_rect;

    // Render visible content into the content sub-area.
    let content_area = Rect::new(
        content_start_col,
        area.y,
        content_viewport_w as u16,
        area.height,
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg)),
        content_area,
    );
}
