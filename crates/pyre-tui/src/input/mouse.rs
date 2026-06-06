//! Mouse event handler — extracted from run_tui event loop (Wave 1E).
//!
//! Contains `handle_mouse` and helpers exclusive to mouse event dispatch
//! (word-bounds selection, resize weights). Layout rect helpers
//! (`collect_leaf_rects`, `rect_contains`) remain in the crate root (main.rs)
//! and are referenced via `crate::`.

use std::time::Instant;

use alacritty_terminal::grid::Dimensions;
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};

use crate::model::pane::DragState;
use crate::model::selection::{ClickTracker, Selection, SelectionBase};
use crate::{
    children_at_mut, close_pane_by_slot_idx, collect_leaf_rects, focus_slot, focused_slot_idx,
    pane_leaves_in_order, pane_to_slot_idx, rect_contains, AppState, ContextMenu, NamePrompt,
    PendingMenuAction, PromptKind, MENU_ITEMS,
};

/// Double/triple-click window in milliseconds.
const CLICK_WINDOW_MS: u64 = 500;

// ─────────────────────────────────────────────────────────────────────────────
// Selection helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Walk a word boundary outward from `(row, col)` in the alacritty grid.
/// Returns `(start_col, end_col)` on the same row, clamped to `[0, num_cols)`.
fn word_bounds(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    row: u16,
    col: u16,
) -> (u16, u16) {
    let num_cols = grid.columns();
    let r = row as i32;

    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';

    // Walk left.
    let mut c0 = col as usize;
    while c0 > 0 {
        let pt = TermPoint::new(TermLine(r), TermColumn(c0 - 1));
        let ch = grid[pt].c;
        if ch == '\0' || is_word_char(ch) {
            c0 -= 1;
        } else {
            break;
        }
    }

    // Walk right.
    let mut c1 = col as usize;
    while c1 + 1 < num_cols {
        let pt = TermPoint::new(TermLine(r), TermColumn(c1 + 1));
        let ch = grid[pt].c;
        if ch == '\0' || is_word_char(ch) {
            c1 += 1;
        } else {
            break;
        }
    }

    (c0 as u16, c1 as u16)
}

/// Extract the text under a NORMALIZED selection span from an alacritty grid.
///
/// `(r0, c0)` and `(r1, c1)` are viewport-relative, normalized so that
/// `(r0, c0) <= (r1, c1)` (callers pass `Selection::normalized()`).
/// `scroll_offset` is the LIVE display offset on screen — the row→grid-line
/// mapping `line_idx = row - scroll_offset` mirrors the render formula in
/// `render/pane.rs` (`display_line = row - display_offset`, where
/// `display_offset == scroll_offset`). Cells holding the null char render as a
/// space; each line is right-trimmed and joined with `\n`.
///
/// This is the single source of truth for selection extraction: the MouseUp
/// copy path calls it, and the regression test drives a real grid through it so
/// forward/reverse selections are compared against production logic — not a
/// restated copy of it.
fn extract_selection_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    (r0, c0): (u16, u16),
    (r1, c1): (u16, u16),
    scroll_offset: usize,
) -> String {
    let num_cols = grid.columns();
    let mut text = String::new();
    for grid_row in r0..=r1 {
        if grid_row > r0 {
            text.push('\n');
        }
        let line_idx = grid_row as i32 - scroll_offset as i32;
        let col_start = if grid_row == r0 { c0 as usize } else { 0usize };
        let col_end = if grid_row == r1 {
            c1 as usize
        } else {
            num_cols.saturating_sub(1)
        };
        for c in col_start..=col_end {
            let pt = TermPoint::new(TermLine(line_idx), TermColumn(c));
            let ch = grid[pt].c;
            if ch == '\0' {
                text.push(' ');
            } else {
                text.push(ch);
            }
        }
    }
    text.lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

// ─────────────────────────────────────────────────────────────────────────────
// Resize helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Apply a delta-percentage resize to two adjacent split children at `[idx]`
/// and `[idx+1]` within `weights`. Each child is clamped to `min_pct` (default
/// 5). The pair's total is preserved. Returns the updated weights vec.
pub(crate) fn apply_resize_weights(
    weights: &[u16],
    idx: usize,
    delta_pct: i32,
    min_pct: u16,
) -> Vec<u16> {
    let mut out = weights.to_vec();
    if idx + 1 >= out.len() {
        return out;
    }
    let total = out[idx] as i32 + out[idx + 1] as i32;
    let left = (out[idx] as i32 + delta_pct).clamp(min_pct as i32, total - min_pct as i32);
    let right = total - left;
    out[idx] = left as u16;
    out[idx + 1] = right as u16;
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Main entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Handle a mouse event. Returns `true` if the event was consumed.
pub(crate) fn handle_mouse(
    state: &mut AppState,
    me: crossterm::event::MouseEvent,
    body_area: Rect,
) -> bool {
    let col = me.column;
    let row = me.row;

    // Any click dismisses an open context menu (unless it is on the menu itself).
    let menu_rect = state.context_menu.as_ref().map(|m| m.rect);
    if let MouseEventKind::Down(_) = me.kind {
        if let Some(mr) = menu_rect {
            if !rect_contains(mr, col, row) {
                state.context_menu = None;
            }
        }
    }

    // Context menu mouse-left: hit-test item_rects written by the last render frame.
    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
        if state.context_menu.is_some() {
            let item_rects = state
                .context_menu
                .as_ref()
                .map(|m| m.item_rects.clone())
                .unwrap_or_default();
            for (idx, rect) in item_rects.iter().enumerate() {
                if rect_contains(*rect, col, row) {
                    if let Some(ref mut m) = state.context_menu {
                        m.cursor = idx;
                    }
                    state.pending_menu_action = Some(PendingMenuAction::ContextMenuActivate(idx));
                    return true;
                }
            }
        }
    }

    // Search overlay click — intercept left-down inside the result list.
    if state.search.open {
        if let MouseEventKind::Down(MouseButton::Left) = me.kind {
            let rects = state.search.result_rects.clone();
            for (result_idx, rect) in &rects {
                if rect_contains(*rect, col, row) {
                    state.search.cursor = *result_idx;
                    state.pending_menu_action = Some(PendingMenuAction::SearchJump(*result_idx));
                    return true;
                }
            }
        }
        // Scroll-wheel events pass through to the pane when search is open.
    }

    match me.kind {
        MouseEventKind::ScrollUp => {
            // Mouse-wheel up over the session strip scrolls the strip left.
            if row == 0 {
                state.session_strip_scroll = state.session_strip_scroll.saturating_sub(1);
                return true;
            }
            // Route to pager when it is open and click is inside pager area.
            if let Some(pr) = state.pager_rect {
                if rect_contains(pr, col, row) {
                    if let Some(ref mut pager) = state.pager {
                        pager.scroll_up(3);
                    }
                    return true;
                }
            }
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(pyre_proto::PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (pane_id, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                        focus_slot(state, slot_idx);
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset =
                                (slot.scroll_offset + 3).min(slot.scrollback_capacity);
                        }
                    }
                    return true;
                }
            }
            false
        }

        MouseEventKind::ScrollDown => {
            // Mouse-wheel down over the session strip scrolls the strip right.
            if row == 0 {
                state.session_strip_scroll = state.session_strip_scroll.saturating_add(1);
                return true;
            }
            // Route to pager when it is open and click is inside pager area.
            if let Some(pr) = state.pager_rect {
                if rect_contains(pr, col, row) {
                    let visible = pr.height.saturating_sub(3) as usize;
                    if let Some(ref mut pager) = state.pager {
                        pager.scroll_down(3, visible.max(1));
                    }
                    return true;
                }
            }
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(pyre_proto::PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            for (pane_id, rect) in &leaf_rects {
                if rect_contains(*rect, col, row) {
                    if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                        focus_slot(state, slot_idx);
                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                            slot.scroll_offset = slot.scroll_offset.saturating_sub(3);
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Right-click: open context menu ───────────────────────────────────
        MouseEventKind::Down(MouseButton::Right) => {
            state.context_menu = None;
            if row >= 2 {
                let sv = &state.sessions[state.active_session];
                let mut leaf_rects: Vec<(pyre_proto::PaneId, Rect)> = Vec::new();
                collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
                for (pane_id, rect) in &leaf_rects {
                    if rect_contains(*rect, col, row) {
                        if let Some(slot_idx) = pane_to_slot_idx(&state.slots, *pane_id) {
                            focus_slot(state, slot_idx);
                            let max_label = MENU_ITEMS
                                .iter()
                                .map(|i| i.label().len())
                                .max()
                                .unwrap_or(10) as u16;
                            let w = max_label + 4;
                            let h = MENU_ITEMS.len() as u16 + 2;
                            state.context_menu = Some(ContextMenu {
                                rect: Rect::new(col, row, w, h),
                                cursor: 0,
                                target_slot: slot_idx,
                                item_rects: Vec::new(),
                            });
                        }
                        return true;
                    }
                }
            }
            false
        }

        // ── Middle-click: paste clipboard to focused PTY ──────────────────────
        // read_from_clipboard is not yet implemented; arm reserved for M4.
        MouseEventKind::Down(MouseButton::Middle) => false,

        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();

            // ── Row 0: sessions strip ─────────────────────────────────────────
            if row == 0 {
                state.context_menu = None;
                if let Some(left_rect) = state.session_strip_left_arrow {
                    if rect_contains(left_rect, col, row) {
                        state.session_strip_scroll = state.session_strip_scroll.saturating_sub(1);
                        return true;
                    }
                }
                if let Some(right_rect) = state.session_strip_right_arrow {
                    if rect_contains(right_rect, col, row) {
                        state.session_strip_scroll = state.session_strip_scroll.saturating_add(1);
                        return true;
                    }
                }
                if let Some(plus_rect) = state.session_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewSession,
                            input: String::new(),
                        });
                        return true;
                    }
                }
                let session_rects = state.session_strip_rects.clone();
                for (sess_idx, rect) in &session_rects {
                    if rect_contains(*rect, col, row) {
                        state.active_session = *sess_idx;
                        return true;
                    }
                }
                return false;
            }

            // ── Row 1: tabs strip ─────────────────────────────────────────────
            if row == 1 {
                state.context_menu = None;
                if let Some(plus_rect) = state.tab_plus_rect {
                    if rect_contains(plus_rect, col, row) {
                        state.prompt = Some(NamePrompt {
                            kind: PromptKind::NewTab,
                            input: String::new(),
                        });
                        return true;
                    }
                }
                let chip_rects = state.tab_chip_rects.clone();
                for (tab_idx, chip_rect) in &chip_rects {
                    if rect_contains(*chip_rect, col, row) {
                        let close_col = chip_rect.x + chip_rect.width.saturating_sub(1);
                        if col == close_col {
                            let slot_idx = {
                                let sv = &state.sessions[state.active_session];
                                let tab = &sv.tabs[*tab_idx];
                                focused_slot_idx(tab.focus_pane, &state.slots)
                            };
                            if let Some(si) = slot_idx {
                                close_pane_by_slot_idx(state, si);
                                if state.sessions.is_empty() {
                                    return true;
                                }
                            }
                            return true;
                        }
                        state.dragging_tab = Some((*tab_idx, col));
                        state.sessions[state.active_session].active_tab = *tab_idx;
                        return true;
                    }
                }
                return false;
            }

            // ── Body: boundary, pane, ribbon chips ────────────────────────────

            // Check if clicking near a split boundary to start a drag.
            {
                let sv = &mut state.sessions[state.active_session];
                let tab = &mut sv.tabs[sv.active_tab];
                for boundary in tab.boundaries.clone() {
                    let hit = if boundary.is_hsplit {
                        row.abs_diff(boundary.coord) <= 1
                    } else {
                        col.abs_diff(boundary.coord) <= 1
                    };
                    if hit {
                        let click_pos = (col, row);
                        let is_double = state
                            .last_click
                            .as_ref()
                            .map(|lc| {
                                ClickTracker::click_count(
                                    now,
                                    lc.last_at,
                                    lc.last_pos,
                                    click_pos,
                                    lc.count,
                                    CLICK_WINDOW_MS,
                                ) >= 2
                            })
                            .unwrap_or(false);

                        if is_double {
                            if let Some(children) =
                                children_at_mut(&mut tab.root, &boundary.parent_path)
                            {
                                let n = children.len() as u16;
                                if let Some(each) = 100u16.checked_div(n) {
                                    let rem = 100 - each * n;
                                    for (i, (_, w)) in children.iter_mut().enumerate() {
                                        *w = each + if i == 0 { rem } else { 0 };
                                    }
                                }
                            }
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: 2,
                                pane_idx: usize::MAX,
                            });
                            return true;
                        }

                        let start_coord = if boundary.is_hsplit { row } else { col };
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
                        state.last_click = Some(ClickTracker {
                            last_at: now,
                            last_pos: click_pos,
                            count: 1,
                            pane_idx: usize::MAX,
                        });
                        return true;
                    }
                }
            }

            // Check if clicking inside a sidebar row.
            if state.sidebar_open {
                let sidebar_width: u16 = 24;
                let sidebar_rect =
                    Rect::new(body_area.x, body_area.y, sidebar_width, body_area.height);
                if rect_contains(sidebar_rect, col, row) {
                    let inner_y = sidebar_rect.y.saturating_add(1);
                    let row_idx = row.saturating_sub(inner_y) as usize;
                    if row_idx < state.sidebar_data.len() {
                        state.sidebar_cursor = row_idx;
                        state.sidebar_focused = true;
                        let target_pane_id = state.sidebar_data[row_idx].id;
                        let pane_in_tab = {
                            let sv = &state.sessions[state.active_session];
                            let tab = &sv.tabs[sv.active_tab];
                            pane_leaves_in_order(&tab.root).contains(&target_pane_id)
                        };
                        if pane_in_tab {
                            let sv = &mut state.sessions[state.active_session];
                            let tab = &mut sv.tabs[sv.active_tab];
                            tab.focus_pane = target_pane_id;
                        }
                    }
                    return true;
                }
            }

            // Check if clicking inside a leaf pane (ribbon chips + text selection).
            let sv = &state.sessions[state.active_session];
            let mut leaf_rects: Vec<(pyre_proto::PaneId, Rect)> = Vec::new();
            collect_leaf_rects(&sv.tabs[sv.active_tab].root, body_area, &mut leaf_rects);
            let leaf_rects_with_slots: Vec<(usize, Rect)> = leaf_rects
                .iter()
                .filter_map(|(pid, r)| pane_to_slot_idx(&state.slots, *pid).map(|i| (i, *r)))
                .collect();
            for (slot_idx, rect) in leaf_rects_with_slots {
                if rect_contains(rect, col, row) {
                    focus_slot(state, slot_idx);

                    // ── Ribbon chip click ──────────────────────────────────────
                    let chip_rects: Vec<(usize, Rect)> = state.slots[slot_idx]
                        .as_ref()
                        .map(|s| s.ribbon_chip_rects.clone())
                        .unwrap_or_default();
                    for (chip_idx, chip_rect) in &chip_rects {
                        if rect_contains(*chip_rect, col, row) {
                            let click_pos = (col, row);
                            let click_count = state
                                .last_click
                                .as_ref()
                                .map(|lc| {
                                    ClickTracker::click_count(
                                        now,
                                        lc.last_at,
                                        lc.last_pos,
                                        click_pos,
                                        lc.count,
                                        CLICK_WINDOW_MS,
                                    )
                                })
                                .unwrap_or(1);
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: click_count,
                                pane_idx: slot_idx,
                            });
                            if let Some(slot) = state.slots[slot_idx].as_mut() {
                                slot.ribbon_cursor = Some(*chip_idx);
                                if click_count >= 2 {
                                    // Double-click: open pager via deferred action.
                                    state.pending_menu_action =
                                        Some(PendingMenuAction::ContextMenuCommit);
                                }
                            }
                            return true;
                        }
                    }

                    // ── Multi-click text selection ─────────────────────────────
                    if let Some(slot) = state.slots[slot_idx].as_ref() {
                        // `last_screen_rect` is the content area AFTER the render pass
                        // applies border inset (1 cell each side) and removes the ribbon
                        // row (1 row from the bottom).  When the pane has not yet been
                        // rendered (area == 0), derive the equivalent rect from the leaf
                        // `rect` which still includes the border.  This keeps first-click
                        // coordinates consistent with post-render clicks.
                        let content = if slot.last_screen_rect.area() > 0 {
                            slot.last_screen_rect
                        } else {
                            // Border inset: x+1, y+1, width-2; ribbon row: height-3 total.
                            Rect::new(
                                rect.x.saturating_add(1),
                                rect.y.saturating_add(1),
                                rect.width.saturating_sub(2),
                                rect.height.saturating_sub(3),
                            )
                        };
                        if rect_contains(content, col, row) {
                            let sel_row = row.saturating_sub(content.y);
                            let sel_col = col.saturating_sub(content.x);
                            let click_pos = (col, row);

                            let click_count = state
                                .last_click
                                .as_ref()
                                .map(|lc| {
                                    ClickTracker::click_count(
                                        now,
                                        lc.last_at,
                                        lc.last_pos,
                                        click_pos,
                                        lc.count,
                                        CLICK_WINDOW_MS,
                                    )
                                })
                                .unwrap_or(1);
                            state.last_click = Some(ClickTracker {
                                last_at: now,
                                last_pos: click_pos,
                                count: click_count,
                                pane_idx: slot_idx,
                            });

                            let sel_base = if slot.scroll_offset > 0 {
                                SelectionBase::Scrollback(slot.scroll_offset)
                            } else {
                                SelectionBase::Live
                            };

                            let (start, end) = if click_count >= 3 {
                                let grid = slot.term.grid();
                                let last_col = (grid.columns().saturating_sub(1)) as u16;
                                ((sel_row, 0u16), (sel_row, last_col))
                            } else if click_count == 2 {
                                let grid = slot.term.grid();
                                let (wc0, wc1) = word_bounds(grid, sel_row, sel_col);
                                ((sel_row, wc0), (sel_row, wc1))
                            } else {
                                ((sel_row, sel_col), (sel_row, sel_col))
                            };

                            state.selection = Some(Selection {
                                pane_idx: slot_idx,
                                start,
                                end,
                                dragging: click_count == 1,
                                base: sel_base,
                            });
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Tab drag reorder ──────────────────────────────────────────────────
        MouseEventKind::Drag(MouseButton::Left) if row == 1 => {
            if let Some((from_idx, start_col)) = state.dragging_tab {
                let chip_rects = state.tab_chip_rects.clone();
                for (over_idx, chip_rect) in &chip_rects {
                    if rect_contains(*chip_rect, col, row) && *over_idx != from_idx {
                        let mid = chip_rect.x + chip_rect.width / 2;
                        let dragging_right = col > start_col;
                        let cross = if dragging_right {
                            col >= mid
                        } else {
                            col <= mid
                        };
                        if cross {
                            use crate::model::tab::tab_reorder;
                            let sv = &mut state.sessions[state.active_session];
                            let tabs = std::mem::take(&mut sv.tabs);
                            sv.tabs = tab_reorder(tabs, from_idx, *over_idx);
                            sv.active_tab = *over_idx;
                            state.dragging_tab = Some((*over_idx, col));
                        }
                        return true;
                    }
                }
            }
            false
        }

        MouseEventKind::Drag(MouseButton::Left) => {
            if row != 1 {
                state.dragging_tab = None;
            }
            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            if let Some(ref drag) = tab.drag {
                let cur_coord = if drag.boundary.is_hsplit { row } else { col };
                let delta = cur_coord as i32 - drag.start_coord as i32;
                let parent_size = drag.boundary.parent_size.max(1) as i32;
                let delta_pct = (delta * 100) / parent_size;
                let idx = drag.boundary.child_idx;
                let new_weights = apply_resize_weights(&drag.start_weights, idx, delta_pct, 5);
                let parent_path = drag.boundary.parent_path.clone();
                if let Some(children) = children_at_mut(&mut tab.root, &parent_path) {
                    for (i, w) in new_weights.iter().enumerate() {
                        if i < children.len() {
                            children[i].1 = *w;
                        }
                    }
                }
                return true;
            }
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    if let Some(slot) = state.slots[sel.pane_idx].as_ref() {
                        // Use last_screen_rect when available.  For panes that have not
                        // yet been rendered (area == 0), fall back to the leaf rect with
                        // the same border+ribbon inset used in the down-handler (fix #1).
                        // This avoids producing garbage coordinates on the very first drag
                        // event before the first render frame completes.
                        let content = if slot.last_screen_rect.area() > 0 {
                            slot.last_screen_rect
                        } else {
                            // Locate the leaf rect for this pane by scanning the active
                            // tab layout, then apply the same border+ribbon inset.
                            let pane_id = slot.pane_id;
                            let sv = &state.sessions[state.active_session];
                            let mut leaf_rects: Vec<(pyre_proto::PaneId, Rect)> = Vec::new();
                            collect_leaf_rects(
                                &sv.tabs[sv.active_tab].root,
                                body_area,
                                &mut leaf_rects,
                            );
                            if let Some((_, leaf_rect)) =
                                leaf_rects.iter().find(|(pid, _)| *pid == pane_id)
                            {
                                Rect::new(
                                    leaf_rect.x.saturating_add(1),
                                    leaf_rect.y.saturating_add(1),
                                    leaf_rect.width.saturating_sub(2),
                                    leaf_rect.height.saturating_sub(3),
                                )
                            } else {
                                // Pane not in current layout; skip this motion event.
                                return true;
                            }
                        };
                        // Clamp the drag row to the visible content area in BOTH
                        // directions (fix #3, option B). We deliberately do NOT
                        // mutate `scroll_offset` here: the old code bumped it on an
                        // upward drag past the top edge but no-op'd on a downward
                        // drag from offset 0, an asymmetry that broke bottom→top
                        // selection (highlight vanished, copy extracted shifted/empty
                        // text). Keeping `scroll_offset` constant for the whole drag
                        // means the viewport-relative `start`/`end` coords stay valid
                        // and `sel.base` always matches the live offset, so highlight
                        // and copy agree. Drag-past-edge auto-scroll is dropped; the
                        // common in-viewport selection is now correct in both axes.
                        let new_row = if row < content.y {
                            0u16
                        } else if row >= content.y + content.height {
                            content.height.saturating_sub(1)
                        } else {
                            row.saturating_sub(content.y)
                        };
                        let new_col = col
                            .saturating_sub(content.x)
                            .min(content.width.saturating_sub(1));
                        sel.end = (new_row, new_col);
                        return true;
                    }
                }
            }
            false
        }

        MouseEventKind::Up(MouseButton::Left) => {
            state.dragging_tab = None;
            let sv = &mut state.sessions[state.active_session];
            let tab = &mut sv.tabs[sv.active_tab];
            if tab.drag.is_some() {
                tab.drag = None;
                return true;
            }
            if let Some(ref mut sel) = state.selection {
                if sel.dragging {
                    sel.dragging = false;
                    let pane_idx = sel.pane_idx;
                    let ((r0, c0), (r1, c1)) = sel.normalized();
                    if let Some(slot) = state.slots[pane_idx].as_ref() {
                        // Use the LIVE scroll offset, not the stale `sel.base`
                        // snapshot (fix #2). The viewport-relative selection rows
                        // are mapped to grid lines through whatever offset is on
                        // screen RIGHT NOW, exactly as the render formula does.
                        // Since fix #3 keeps scroll_offset constant during a drag
                        // this equals `sel.base`, but reading the live value is
                        // strictly correct and keeps highlight and copy in
                        // lock-step.
                        let trimmed = extract_selection_text(
                            slot.term.grid(),
                            (r0, c0),
                            (r1, c1),
                            slot.scroll_offset,
                        );
                        if !trimmed.is_empty() {
                            if let Err(e) = crate::clipboard::copy_to_clipboard(&trimmed) {
                                tracing::warn!("clipboard copy failed: {e}");
                            }
                        }
                    }
                    return true;
                }
            }
            false
        }

        // ── Hover: boundary "drag to resize" status hint ──────────────────────
        MouseEventKind::Moved => {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            if tab.drag.is_some()
                || state
                    .selection
                    .as_ref()
                    .map(|s| s.dragging)
                    .unwrap_or(false)
            {
                return false;
            }
            let mut on_boundary = false;
            for boundary in &tab.boundaries {
                let hit = if boundary.is_hsplit {
                    row.abs_diff(boundary.coord) <= 1
                } else {
                    col.abs_diff(boundary.coord) <= 1
                };
                if hit {
                    on_boundary = true;
                    break;
                }
            }
            if on_boundary {
                state.status_msg = Some("drag to resize".to_owned());
            } else if state.status_msg.as_deref() == Some("drag to resize") {
                state.status_msg = None;
            }
            false
        }

        _ => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    // ── Close-X hit-rect helpers ──────────────────────────────────────────────

    /// Given a tab-chip rect (as rendered in draw_frame), compute the single
    /// column that should trigger close.  Mirrors the logic in handle_mouse.
    fn close_col_for(chip_rect: Rect) -> u16 {
        chip_rect.x + chip_rect.width.saturating_sub(1)
    }

    /// Returns true only when `col` lands on the close-X cell of `chip_rect`.
    fn hits_close_x(chip_rect: Rect, col: u16) -> bool {
        rect_contains(chip_rect, col, chip_rect.y) && col == close_col_for(chip_rect)
    }

    /// I-3: the close-X hit rect must be exactly 1 cell at the right edge of
    /// the tab chip, not the entire chip width.
    ///
    /// Regression guard: before Wave-1 extraction the hit rect covered the full
    /// title bar; this test pins the narrowed-to-1-cell invariant.
    #[test]
    fn test_close_x_click_hits_only_target_pane() {
        // Simulate 3 tab chips at row 1:
        //   chip 0: " 1 ×" → cols [0, 3], close-X at col 3
        //   chip 1: " 2 ×" → cols [5, 8], close-X at col 8
        //   chip 2: " 3 ×" → cols [10, 13], close-X at col 13
        let chips: Vec<Rect> = vec![
            Rect::new(0, 1, 4, 1),  // chip 0
            Rect::new(5, 1, 4, 1),  // chip 1
            Rect::new(10, 1, 4, 1), // chip 2
        ];

        // Clicking the close-X of chip 1 (col 8) must:
        //   - hit chip 1 only
        //   - NOT hit chips 0 or 2
        let close_col_1 = close_col_for(chips[1]);
        assert_eq!(close_col_1, 8, "close-X of chip 1 must be at col 8");

        assert!(
            hits_close_x(chips[1], close_col_1),
            "clicking col {close_col_1} on chip 1 must trigger close"
        );
        assert!(
            !hits_close_x(chips[0], close_col_1),
            "col {close_col_1} must NOT trigger close on chip 0"
        );
        assert!(
            !hits_close_x(chips[2], close_col_1),
            "col {close_col_1} must NOT trigger close on chip 2"
        );

        // Clicking the body of chip 1 (not the × column) must NOT trigger close.
        for body_col in chips[1].x..(chips[1].x + chips[1].width - 1) {
            assert!(
                !hits_close_x(chips[1], body_col),
                "body col {body_col} of chip 1 must NOT trigger close"
            );
        }

        // Clicking column 0 (far left, chip 0 body) must not hit chip 1 close-X.
        assert!(
            !hits_close_x(chips[1], 0),
            "col 0 (chip 0 body) must not trigger chip 1 close"
        );
    }

    /// Verify that resize weights are balanced after application.
    #[test]
    fn test_apply_resize_weights_preserves_total() {
        let weights = vec![50u16, 50u16];
        let out = apply_resize_weights(&weights, 0, 10, 5);
        assert_eq!(
            out[0] + out[1],
            100,
            "total weight must be preserved after resize"
        );
        assert_eq!(out[0], 60, "left pane must grow by delta_pct");
        assert_eq!(out[1], 40, "right pane must shrink by delta_pct");
    }

    // ── Coordinate translation tests (fix #1 and fix #2) ─────────────────────

    /// Helper that mirrors the border+ribbon inset applied in fix #1 / fix #2.
    /// `rect` is the border-inclusive leaf rect from `collect_leaf_rects`.
    fn border_inset(rect: Rect) -> Rect {
        Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(3),
        )
    }

    /// Helper that mirrors the coordinate translation applied in the down-handler
    /// and drag-handler: given `content` (inner rect) and a global (col, row),
    /// compute (sel_row, sel_col).
    fn to_sel_coords(content: Rect, col: u16, row: u16) -> (u16, u16) {
        let sel_row = row.saturating_sub(content.y);
        let sel_col = col.saturating_sub(content.x);
        (sel_row, sel_col)
    }

    /// Fix #1: the border-inset fallback must produce the same (sel_row, sel_col)
    /// as using `last_screen_rect` directly for a geometrically equivalent rect.
    ///
    /// Scenario: leaf rect at (5, 3) with width=20, height=10.
    /// Expected last_screen_rect: x=6, y=4, width=18, height=7.
    /// A click at global (10, 6) should yield (sel_row=2, sel_col=4) in both cases.
    #[test]
    fn test_border_inset_fallback_matches_last_screen_rect() {
        let leaf_rect = Rect::new(5, 3, 20, 10);

        // Simulate what render_pane stores as last_screen_rect:
        //   inner = border_block.inner(leaf_rect) => x+1, y+1, w-2, h-2 = (6,4,18,8)
        //   content_area = split[0] from [Min(1), Length(1)] => height = inner.height - 1 = 7
        let simulated_last_screen_rect = Rect::new(6, 4, 18, 7);

        let fallback = border_inset(leaf_rect);

        assert_eq!(
            fallback, simulated_last_screen_rect,
            "border_inset fallback must match simulated last_screen_rect"
        );

        // Verify coordinate translation is identical for both.
        let global_col: u16 = 10;
        let global_row: u16 = 6;

        let coords_via_lsr = to_sel_coords(simulated_last_screen_rect, global_col, global_row);
        let coords_via_fallback = to_sel_coords(fallback, global_col, global_row);

        assert_eq!(
            coords_via_lsr, coords_via_fallback,
            "sel coords must be identical whether using last_screen_rect or border-inset fallback"
        );
        assert_eq!(
            coords_via_lsr,
            (2, 4),
            "click at global (10,6) on rect at (6,4) must yield (sel_row=2, sel_col=4)"
        );
    }

    /// Fix #1: the old fallback `rect` (border-inclusive) produces coordinates
    /// off by 1 in both axes compared to `last_screen_rect`.
    #[test]
    fn test_old_rect_fallback_was_off_by_one() {
        let leaf_rect = Rect::new(5, 3, 20, 10);
        let correct_inner = border_inset(leaf_rect); // (6, 4, 18, 7)

        let global_col: u16 = 10;
        let global_row: u16 = 6;

        // Using the (now-fixed) inner rect: sel_col = 10-6 = 4, sel_row = 6-4 = 2
        let (row_correct, col_correct) = to_sel_coords(correct_inner, global_col, global_row);

        // Old code used border-inclusive `rect`: sel_col = 10-5 = 5, sel_row = 6-3 = 3
        let (row_old, col_old) = to_sel_coords(leaf_rect, global_col, global_row);

        assert_eq!(
            (row_correct, col_correct),
            (2, 4),
            "fixed path must yield (2, 4)"
        );
        assert_eq!(
            (row_old, col_old),
            (3, 5),
            "old path yields (3, 5) — off by one"
        );
        assert_ne!(
            (row_correct, col_correct),
            (row_old, col_old),
            "old and new must differ, confirming the bug was real"
        );
    }

    // ── Reverse-drag selection regression (the bottom→top bug) ───────────────

    use alacritty_terminal::term::cell::Cell;
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
    use alacritty_terminal::Term;

    use crate::model::pane::EventProxy;
    use crate::model::selection::{Selection, SelectionBase};
    use crate::render::pane::TermSize;

    /// Build a real alacritty grid pre-filled with `lines`, one per row,
    /// at the live offset (no scrollback). Returns a `Term` whose grid the
    /// production `extract_selection_text` can read.
    fn grid_with_lines(lines: &[&str], cols: usize) -> Term<EventProxy> {
        let rows = lines.len();
        let mut term = Term::new(
            TermConfig::default(),
            &TermSize::new(cols, rows),
            EventProxy::new(),
        );
        // The `processor` field on PaneSlot resolves to the default Timeout
        // handler (StdSyncHandler); a standalone `let` cannot infer that, so
        // name it explicitly here.
        let mut processor: AnsiProcessor = AnsiProcessor::new();
        // CRLF between rows so the cursor lands at column 0 of the next line.
        let payload = lines.join("\r\n");
        processor.advance(&mut term, payload.as_bytes());
        term
    }

    /// Run the FULL production selection pipeline (`Selection::normalized()` →
    /// `extract_selection_text`) for a given anchor/head. Mirrors exactly what
    /// the MouseUp copy handler does, so the assertion exercises real logic.
    fn copy_via_pipeline(
        grid: &alacritty_terminal::grid::Grid<Cell>,
        anchor: (u16, u16),
        head: (u16, u16),
    ) -> String {
        let sel = Selection {
            pane_idx: 0,
            start: anchor,
            end: head,
            dragging: false,
            base: SelectionBase::Live,
        };
        let (lo, hi) = sel.normalized();
        // Live view ⇒ scroll_offset 0, matching an in-viewport drag.
        extract_selection_text(grid, lo, hi, 0)
    }

    /// The core regression: a REVERSE (bottom→top) drag over a region must
    /// produce byte-identical copied text to the FORWARD (top→bottom) drag over
    /// the SAME region. Before the fix the reverse path bumped `scroll_offset`
    /// and de-synced normalization vs extraction; here we pin them equal.
    ///
    /// Non-tautological: both directions flow through the real
    /// `Selection::normalized()` + `extract_selection_text`, against a live
    /// alacritty grid — not a re-statement of the formula.
    #[test]
    fn test_reverse_drag_copy_matches_forward_drag() {
        let lines = ["alpha", "bravo", "charlie"];
        let term = grid_with_lines(&lines, 16);
        let grid = term.grid();

        // Region: from (row 0, col 1) to (row 2, col 4) inclusive.
        let top = (0u16, 1u16);
        let bottom = (2u16, 4u16);

        // Forward: anchor=top, head=bottom (drag down).
        let forward = copy_via_pipeline(grid, top, bottom);
        // Reverse: anchor=bottom, head=top (drag up) — same visual region.
        let reverse = copy_via_pipeline(grid, bottom, top);

        assert_eq!(
            forward, reverse,
            "reverse (bottom→top) drag must copy the same text as forward (top→bottom)"
        );

        // And the content must be the actual selected span, not empty/shifted:
        //   row0 cols 1..=15 = "lpha" (rest blank, trimmed)
        //   row1 full        = "bravo"
        //   row2 cols 0..=4  = "charl"
        assert_eq!(
            forward, "lpha\nbravo\ncharl",
            "extracted text must match the real grid contents under the span"
        );
    }

    /// Reverse selection confined to a SINGLE row (head left of anchor) must
    /// also normalize+extract identically to its forward twin. Guards the
    /// column-swap leg of `normalized()` independently of the row-swap leg.
    #[test]
    fn test_reverse_single_row_selection_matches_forward() {
        let lines = ["hello world"];
        let term = grid_with_lines(&lines, 16);
        let grid = term.grid();

        let left = (0u16, 2u16);
        let right = (0u16, 8u16);

        let forward = copy_via_pipeline(grid, left, right); // drag right
        let reverse = copy_via_pipeline(grid, right, left); // drag left

        assert_eq!(
            forward, reverse,
            "right→left drag must copy the same text as left→right on one row"
        );
        assert_eq!(forward, "llo wor", "single-row span must extract exactly");
    }

    // NOTE: the scrollback line_idx↔render-formula invariant is:
    //   line_idx = grid_row as i32 - scroll_offset as i32
    // This mirrors render/pane.rs:
    //   display_line = TermLine(row as i32 - grid.display_offset() as i32)
    // where display_offset == scroll_offset (set via
    //   scroll_display(Scroll::Delta(scroll_offset as i32))).
    // `extract_selection_text` is now the single home for that formula, and the
    // tests above drive it through a live grid. If the highlight mapping in
    // render/pane.rs and this formula ever diverge, the visible highlight and
    // the copied text will disagree on a scrolled terminal.
}
