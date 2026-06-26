//! Full-frame draw pass — assembles all render layers into a single terminal frame.
//!
//! Extracted from `draw_frame` in main.rs (Wave 1D refactor).

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph};
use ratatui::Terminal;

use crate::app::state::AppState;
use crate::fire_motion;
use crate::model::context_menu::MENU_ITEMS;
use crate::model::layout::{build_pane_slot_map, focused_slot_idx};
use crate::model::pane::SplitBoundary;
use crate::model::prompt::{NamePrompt, PromptKind};
use crate::render::overlay::help::render_help_overlay;
use crate::render::overlay::pager::render_pager;
use crate::render::overlay::picker::render_theme_picker;
use crate::render::overlay::search::render_search_overlay;
use crate::render::overlay::session_lost::render_session_lost_overlay;
use crate::render::pane::{pane_needs_attention, render_layout, render_pane};
use crate::render::session_strip::render_session_strip;
use crate::render::sidebar::render_sidebar;
use crate::render::toast::render_toast_deck;
use crate::theme;
use pyre_proto::layout::LayoutNode;

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers (only called from draw_frame)
// ─────────────────────────────────────────────────────────────────────────────

fn render_name_prompt(
    frame: &mut ratatui::Frame,
    prompt: &NamePrompt,
    anim_frame: u64,
    t: &theme::LegacyTheme,
) {
    let area = frame.area();
    let w = (area.width as f32 * 0.60) as u16;
    let h: u16 = 5;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(30), h);

    frame.render_widget(Clear, overlay_rect);

    let title = match prompt.kind {
        PromptKind::NewSession => " new session name ",
        PromptKind::NewTab => " new tab label ",
        PromptKind::RenameSession(_) => " rename session ",
        PromptKind::RenameWindow(_) => " rename window ",
    };

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .title(Span::styled(title, t.title(t.primary)))
        .style(t.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Input row (row 0 of inner) + hint row (row 1).
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let input_area = split[0];
    let hint_area = split[1];

    let input_spans = vec![
        Span::styled("> ", Style::default().fg(t.primary)),
        Span::styled(prompt.input.as_str(), Style::default().fg(t.text)),
        Span::styled(
            "█",
            fire_motion::ember_fg_style(anim_frame, 0xc0ffee, t.spark, t.secondary, 0.9),
        ),
    ];
    frame.render_widget(Paragraph::new(Line::from(input_spans)), input_area);

    let hint =
        Paragraph::new(" Enter = create  |  Esc = cancel").style(Style::default().fg(t.text_dim));
    frame.render_widget(hint, hint_area);

    // Host cursor at end of input.
    let cursor_col = (2u16 + prompt.input.len() as u16).min(input_area.width.saturating_sub(1));
    frame.set_cursor_position((input_area.x + cursor_col, input_area.y));
}

/// Render the right-click context menu overlay.
///
/// The menu is a small popup anchored at `menu.rect`. Items are drawn
/// with the cursor row highlighted; Esc/Enter/click outside dismisses.
fn render_context_menu(frame: &mut ratatui::Frame, state: &mut AppState, t: &theme::LegacyTheme) {
    let menu = match state.context_menu.as_ref() {
        Some(m) => m,
        None => return,
    };

    // Compute a rect that fits the menu — width = longest label + 2, height = items + 2 (border).
    let max_label = MENU_ITEMS
        .iter()
        .map(|i| i.label().len())
        .max()
        .unwrap_or(10) as u16;
    let w = max_label + 4; // left border + space + label + right border
    let h = MENU_ITEMS.len() as u16 + 2;
    let area = frame.area();
    // Clamp so the menu stays on screen.
    let x = menu.rect.x.min(area.width.saturating_sub(w));
    let y = menu.rect.y.min(area.height.saturating_sub(h));
    let popup = Rect::new(x, y, w, h);

    frame.render_widget(Clear, popup);

    let block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.border_focus())
        .style(t.overlay());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let cursor = menu.cursor;
    // Collect item rects so the mouse handler can hit-test individual rows.
    let mut new_item_rects: Vec<Rect> = Vec::with_capacity(MENU_ITEMS.len());
    for (idx, item) in MENU_ITEMS.iter().enumerate() {
        if idx >= inner.height as usize {
            break;
        }
        let row_y = inner.y + idx as u16;
        let is_selected = idx == cursor;
        let style = if is_selected {
            t.selection()
        } else {
            Style::default().fg(t.text).bg(t.bg)
        };
        let label = format!("{:<width$}", item.label(), width = inner.width as usize);
        let item_rect = Rect::new(inner.x, row_y, inner.width, 1);
        frame.render_widget(Paragraph::new(Span::styled(label, style)), item_rect);
        new_item_rects.push(item_rect);
    }
    // Write back so the mouse handler has fresh rects every frame.
    if let Some(ref mut m) = state.context_menu {
        m.item_rects = new_item_rects;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

pub fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    state: &mut AppState,
    prefix_active: bool,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let t = theme::LegacyTheme::from_palette(&state.theme.palette);

        // Short-circuit: when session_lost is active, render only the overlay.
        if state.session_lost {
            frame.render_widget(RatatuiBlock::default().style(t.bg_style()), area);
            render_session_lost_overlay(frame, &t);
            return;
        }

        // Four rows: sessions strip (1) + tabs strip (1) + body (min 0) + status bar (1)
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let sessions_area = outer[0];
        let tabs_area = outer[1];
        let body_area = outer[2];
        let status_area = outer[3];

        // Frame clear — paint entire frame with bg_style so no bleed.
        frame.render_widget(RatatuiBlock::default().style(t.bg_style()), frame.area());

        // ── Row 0: sessions strip (with horizontal scroll) ──
        render_session_strip(frame, sessions_area, state, &t);

        // ── Row 1: tabs strip of active session ──
        {
            let sv = &state.sessions[state.active_session];
            let total_tabs = sv.tabs.len();
            let mut spans: Vec<Span> = Vec::new();
            let mut x_cursor: u16 = tabs_area.x;
            let mut new_tab_chip_rects: Vec<(usize, Rect)> = Vec::new();

            for (i, tab) in sv.tabs.iter().enumerate() {
                // Each chip: " N ×" or " name ×" — label + close button.
                let win_label = if tab.window_name.is_empty() {
                    format!("{}", i + 1)
                } else {
                    tab.window_name.clone()
                };
                let label = format!(" {win_label} ×");
                let len = label.chars().count() as u16;
                let style = if i == sv.active_tab {
                    t.tab_active()
                } else {
                    t.tab_inactive()
                };
                if tabs_area.height > 0 {
                    new_tab_chip_rects.push((i, Rect::new(x_cursor, tabs_area.y, len, 1)));
                }
                x_cursor += len;
                spans.push(Span::styled(label, style));
                if i + 1 < total_tabs {
                    spans.push(Span::styled(" ", Style::default().bg(t.bg)));
                    x_cursor += 1;
                }
            }

            // [+] button immediately after the last tab label (browser-style).
            let plus_label = "[+]";
            let plus_len = plus_label.len() as u16;
            let plus_x = x_cursor;
            let plus_rect =
                if tabs_area.height > 0 && plus_x + plus_len <= tabs_area.x + tabs_area.width {
                    Some(Rect::new(plus_x, tabs_area.y, plus_len, 1))
                } else {
                    None
                };
            if !spans.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(t.bg)));
            }
            spans.push(Span::styled(plus_label, t.tab_inactive()));

            state.tab_plus_rect = plus_rect;
            state.tab_chip_rects = new_tab_chip_rects;

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg)),
                tabs_area,
            );
        }

        // Body — optionally split horizontally for sidebar.
        let (sidebar_area_opt, pane_body_area) = if state.sidebar_open {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(24), Constraint::Min(0)])
                .split(body_area);
            (Some(cols[0]), cols[1])
        } else {
            (None, body_area)
        };

        if let Some(sbar_area) = sidebar_area_opt {
            render_sidebar(frame, sbar_area, state, &t);
        }

        // Render active tab's layout in the remaining area.
        let active_tab_idx = state.sessions[state.active_session].active_tab;
        let focus_pane_id = state.sessions[state.active_session].tabs[active_tab_idx].focus_pane;
        let zoomed = state.sessions[state.active_session].tabs[active_tab_idx].zoomed;
        let mut new_boundaries: Vec<SplitBoundary> = Vec::new();

        // SAFETY: we only borrow root via a raw pointer to avoid the
        // simultaneous mutable borrow of slots. render_layout only reads `root`
        // and mutates `slots` at disjoint indices; no mutation of `tabs` occurs.
        let root_ptr: *const LayoutNode =
            &state.sessions[state.active_session].tabs[active_tab_idx].root;

        let anim_frame = state.anim.frame();
        let panes_meta = state.sidebar_data.as_slice();

        // Build pane_slot map once per frame for O(1) lookups in render_layout.
        let pane_slot_map = build_pane_slot_map(&state.slots);

        if let Some(zoom_pane) = zoomed {
            // Zoom mode: render only the zoomed pane filling pane_body_area.
            if let Some(&slot_idx) = pane_slot_map.get(&zoom_pane) {
                if let Some(slot) = state.slots[slot_idx].as_mut() {
                    let attention = pane_needs_attention(panes_meta, slot.pane_id);
                    render_pane(
                        frame,
                        pane_body_area,
                        slot,
                        true,
                        state.selection.as_ref(),
                        slot_idx,
                        &mut state.pending_resizes,
                        anim_frame,
                        attention,
                        &t,
                        panes_meta,
                        true, // is_zoomed: this path renders the zoomed pane
                    );
                }
            }
        } else {
            let mut current_path: Vec<usize> = Vec::new();
            render_layout(
                frame,
                pane_body_area,
                unsafe { &*root_ptr },
                &mut state.slots,
                focus_pane_id,
                &pane_slot_map,
                &mut current_path,
                &mut new_boundaries,
                state.selection.as_ref(),
                &mut state.pending_resizes,
                anim_frame,
                panes_meta,
                &t,
            );
        }
        state.sessions[state.active_session].tabs[active_tab_idx].boundaries = new_boundaries;

        // Status bar — two segments + optional middle message.
        {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot = focused_slot_idx(tab.focus_pane, &state.slots);
            let is_zoomed = tab.zoomed.is_some();

            // Determine mode label and mid message.
            let (mode_label, mid_msg) = if state.search.open {
                (
                    "SEARCH",
                    Some(format!(
                        " search: {} ({} results) ",
                        state.search.input,
                        state.search.results.len()
                    )),
                )
            } else if prefix_active {
                (
                    "PREFIX",
                    state.status_msg.as_ref().map(|m| format!(" {m} ")),
                )
            } else if let Some(slot_idx) = focused_slot {
                let in_ribbon = state.slots[slot_idx]
                    .as_ref()
                    .map(|s| s.ribbon_cursor.is_some())
                    .unwrap_or(false);
                if in_ribbon {
                    (
                        "SCROLL",
                        state.status_msg.as_ref().map(|m| format!(" {m} ")),
                    )
                } else {
                    ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
                }
            } else {
                ("LIVE", state.status_msg.as_ref().map(|m| format!(" {m} ")))
            };

            // Left: ` ● {session_name} ▸ {pane} `
            let left_text = if let Some(slot_idx) = focused_slot {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    let pane_short = &slot.pane_id.0.to_string()[..8];
                    format!(" ● {} ▸ {pane_short} ", sv.name)
                } else {
                    format!(" ● {} ", sv.name)
                }
            } else {
                format!(" ● {} ", sv.name)
            };

            // Right: mode indicator + optional ZOOM chip
            let right_text = format!(" {mode_label} ");

            let mut status_spans: Vec<Span> = vec![Span::styled(left_text, t.status())];
            if let Some(msg) = mid_msg {
                status_spans.push(Span::styled(
                    msg,
                    Style::default().fg(t.secondary).bg(t.surface),
                ));
            }
            // Spacer to push mode to right — approximate with bg fill.
            status_spans.push(Span::styled(" ", Style::default().bg(t.surface)));
            if is_zoomed {
                status_spans.push(Span::styled(
                    " ZOOM ",
                    Style::default()
                        .fg(t.bg)
                        .bg(t.primary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            status_spans.push(Span::styled(
                right_text,
                Style::default()
                    .fg(t.bg)
                    .bg(t.primary)
                    .add_modifier(Modifier::BOLD),
            ));

            frame.render_widget(
                Paragraph::new(Line::from(status_spans)).style(t.status()),
                status_area,
            );
        }

        // Toast deck — rendered before blocking overlays so toasts appear
        // under modal dialogs (which is fine; user can still see them).
        render_toast_deck(frame, &state.toast_deck, &t);

        // Host-terminal cursor positioning.
        // Only one pane (the focused one, live view) owns the cursor.
        // Overlays or scrollback suppress it.
        if let Some(ref pager) = state.pager {
            // Block pager — full-screen, draws over everything, no cursor.
            let pager_full = frame.area();
            render_pager(frame, pager, &t);
            state.pager_rect = Some(pager_full);
        } else {
            state.pager_rect = None;
            if let Some(ref picker) = state.theme_picker {
                render_theme_picker(frame, picker, &t);
            } else if let Some(ref prompt) = state.prompt {
                render_name_prompt(frame, prompt, state.anim.frame(), &t);
            } else if state.search.open {
                // Search overlay — drawn on top of everything else and owns cursor.
                let anim_frame = state.anim.frame();
                render_search_overlay(frame, &mut state.search, anim_frame, &t);
            } else if state.help_open {
                render_help_overlay(frame, &t);
            }
        }

        // Context menu rendered on top of everything (including pager).
        if state.context_menu.is_some() {
            render_context_menu(frame, state, &t);
        }

        // Host-terminal cursor: only for live pane view (no overlay, no scrollback).
        if state.pager.is_none()
            && state.theme_picker.is_none()
            && state.prompt.is_none()
            && !state.search.open
            && !state.help_open
            && state.context_menu.is_none()
            && state.pid_inspect.is_none()
        {
            // No blocking overlay: propagate vt100 cursor from focused pane.
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            let focused_slot_idx = if let Some(zoom_pane) = tab.zoomed {
                focused_slot_idx(zoom_pane, &state.slots)
            } else {
                focused_slot_idx(tab.focus_pane, &state.slots)
            };
            if let Some(slot_idx) = focused_slot_idx {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    if slot.scroll_offset == 0 {
                        let vt_area = slot.last_screen_rect;
                        let cursor_pt = slot.term.grid().cursor.point;
                        let vt_row = cursor_pt.line.0.max(0) as u16;
                        let vt_col = cursor_pt.column.0 as u16;
                        let cursor_x = vt_area
                            .x
                            .saturating_add(vt_col)
                            .min(vt_area.x.saturating_add(vt_area.width).saturating_sub(1));
                        let cursor_y = vt_area
                            .y
                            .saturating_add(vt_row)
                            .min(vt_area.y.saturating_add(vt_area.height).saturating_sub(1));
                        frame.set_cursor_position((cursor_x, cursor_y));
                    }
                }
            }
        }
    })?;
    Ok(())
}
