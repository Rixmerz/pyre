//! Keyboard event handler — extracted from run_tui event loop (Wave 1E).
//!
//! `handle_key` processes one `Event::Key` from crossterm and returns a
//! `KeyAction` so the caller can drive the event loop (`Continue`, `Quit`).
//! The `prefix_active` bool remains a local variable in the caller; this
//! module receives it by mutable reference so prefix detection stays in one
//! place.

use std::time::Instant;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use crossterm::event::{KeyCode, KeyModifiers};
use pyre_themes::Registry;
use ratatui::layout::Rect;

use bytes::Bytes;

use crate::app::pane_ops::{close_pane_by_slot_idx, open_new_session, open_new_tab, split_active};
use crate::app::state::AppState;
use crate::model::context_menu::{MenuItem, MENU_ITEMS};
use crate::model::layout::{focused_slot_idx, pane_leaves_in_order, pane_to_slot_idx};
use crate::model::prompt::PromptKind;
use crate::render::overlay::pager::PagerState;

// ─────────────────────────────────────────────────────────────────────────────
// Key serialization
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Bytes> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let byte = (c.to_ascii_lowercase() as u8) & 0x1f;
            return Some(Bytes::copy_from_slice(&[byte]));
        }
    }

    let bytes: &[u8] = match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            return Some(Bytes::copy_from_slice(s.as_bytes()));
        }
        KeyCode::Enter => b"\r",
        KeyCode::Backspace => b"\x7f",
        KeyCode::Tab => b"\t",
        KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A",
        KeyCode::Down => b"\x1b[B",
        KeyCode::Right => b"\x1b[C",
        KeyCode::Left => b"\x1b[D",
        KeyCode::BackTab => b"\x1b[Z",
        KeyCode::Home => b"\x1b[H",
        KeyCode::End => b"\x1b[F",
        KeyCode::Insert => b"\x1b[2~",
        KeyCode::Delete => b"\x1b[3~",
        KeyCode::F(1) => b"\x1bOP",
        KeyCode::F(2) => b"\x1bOQ",
        KeyCode::F(3) => b"\x1bOR",
        KeyCode::F(4) => b"\x1bOS",
        KeyCode::F(5) => b"\x1b[15~",
        KeyCode::F(6) => b"\x1b[17~",
        KeyCode::F(7) => b"\x1b[18~",
        KeyCode::F(8) => b"\x1b[19~",
        KeyCode::F(9) => b"\x1b[20~",
        KeyCode::F(10) => b"\x1b[21~",
        KeyCode::F(11) => b"\x1b[23~",
        KeyCode::F(12) => b"\x1b[24~",
        _ => return None,
    };
    Some(Bytes::copy_from_slice(bytes))
}

use super::prefix::{handle_prefix_key, PrefixAction};

/// Outcome of a single key-event dispatch.
pub(crate) enum KeyAction {
    /// Normal; caller should `continue` the event loop.
    Continue,
    /// TUI should quit; caller should `break` the event loop.
    Quit,
}

/// Dispatch one keyboard event.
///
/// `prefix_active` is passed by mutable reference so the caller's local
/// flag is cleared when a prefix-key match runs.
pub(crate) async fn handle_key(
    state: &mut AppState,
    code: KeyCode,
    mods: KeyModifiers,
    prefix_active: &mut bool,
    body_area: Rect,
) -> KeyAction {
    // Session-lost overlay intercepts all keys when active.
    // q / Esc / Ctrl-C all exit the TUI cleanly.
    if state.session_lost {
        match (code, mods) {
            (KeyCode::Char('q'), _)
            | (KeyCode::Esc, _)
            | (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyAction::Quit,
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Name-prompt intercepts all keys when open.
    if state.prompt.is_some() {
        match code {
            KeyCode::Esc => {
                state.prompt = None;
            }
            KeyCode::Backspace => {
                if let Some(ref mut p) = state.prompt {
                    p.input.pop();
                }
            }
            KeyCode::Enter => {
                if let Some(p) = state.prompt.take() {
                    let input = if p.input.is_empty() {
                        None
                    } else {
                        Some(p.input)
                    };
                    match p.kind {
                        PromptKind::NewSession => {
                            if let Err(e) = open_new_session(state, input).await {
                                tracing::warn!("open_new_session failed: {e}");
                            }
                        }
                        PromptKind::NewTab => {
                            if let Err(e) = open_new_tab(state, input).await {
                                tracing::warn!("open_new_tab failed: {e}");
                            }
                        }
                        PromptKind::RenameSession(session_id) => {
                            if let Some(new_name) = input {
                                match state
                                    .control
                                    .rename_session(
                                        tarpc::context::current(),
                                        session_id,
                                        new_name.clone(),
                                    )
                                    .await
                                {
                                    Ok(Ok(())) => {
                                        // Update local view immediately.
                                        if let Some(sv) =
                                            state.sessions.iter_mut().find(|s| s.id == session_id)
                                        {
                                            sv.name = new_name;
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        tracing::warn!("rename_session rpc error: {e}");
                                        state.status_msg = Some(format!("rename failed: {e}"));
                                    }
                                    Err(e) => {
                                        tracing::warn!("rename_session transport: {e}");
                                        state.status_msg = Some(format!("rename rpc: {e}"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(ref mut p) = state.prompt {
                    p.input.push(c);
                }
            }
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Context menu key handling — intercepts all keys while open.
    if state.context_menu.is_some() {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                state.context_menu = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ref mut m) = state.context_menu {
                    m.cursor = m.cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ref mut m) = state.context_menu {
                    let max = MENU_ITEMS.len().saturating_sub(1);
                    m.cursor = (m.cursor + 1).min(max);
                }
            }
            KeyCode::Enter => {
                if let Some(menu) = state.context_menu.take() {
                    let item = MENU_ITEMS[menu.cursor];
                    let target = menu.target_slot;
                    match item {
                        MenuItem::Copy => {
                            // Copy the current text selection (if any) or last block.
                            if let Some(ref sel) = state.selection.clone() {
                                let pane_idx = sel.pane_idx;
                                let ((r0, c0), (r1, c1)) = sel.normalized();
                                if let Some(slot) = state.slots[pane_idx].as_ref() {
                                    let grid = slot.term.grid();
                                    let num_cols = grid.columns();
                                    let mut text = String::new();
                                    for gr in r0..=r1 {
                                        if gr > r0 {
                                            text.push('\n');
                                        }
                                        let cs = if gr == r0 { c0 as usize } else { 0 };
                                        let ce = if gr == r1 {
                                            c1 as usize
                                        } else {
                                            num_cols.saturating_sub(1)
                                        };
                                        for c in cs..=ce {
                                            let pt =
                                                TermPoint::new(TermLine(gr as i32), TermColumn(c));
                                            let ch = grid[pt].c;
                                            text.push(if ch == '\0' { ' ' } else { ch });
                                        }
                                    }
                                    let trimmed: String = text
                                        .lines()
                                        .map(|l| l.trim_end())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    if !trimmed.is_empty() {
                                        let _ = crate::clipboard::copy_to_clipboard(&trimmed);
                                        state.status_msg = Some("copied".to_owned());
                                    }
                                }
                            }
                        }
                        MenuItem::KillPane => {
                            close_pane_by_slot_idx(state, target);
                            if state.sessions.is_empty() {
                                return KeyAction::Quit;
                            }
                        }
                        MenuItem::SplitH => {
                            if let Err(e) = split_active(state, true).await {
                                tracing::warn!("context menu HSplit: {e}");
                            }
                        }
                        MenuItem::SplitV => {
                            if let Err(e) = split_active(state, false).await {
                                tracing::warn!("context menu VSplit: {e}");
                            }
                        }
                        MenuItem::ZoomToggle => {
                            let sv = state.active_session_view_mut();
                            let tab = &mut sv.tabs[sv.active_tab];
                            if tab.zoomed.is_some() {
                                tab.zoomed = None;
                            } else {
                                tab.zoomed = Some(tab.focus_pane);
                            }
                        }
                        MenuItem::InspectPid => {
                            if let Some(slot) = state.slots[target].as_ref() {
                                let pane_id = slot.pane_id;
                                match state
                                    .control
                                    .inspect_pid(tarpc::context::current(), pane_id)
                                    .await
                                {
                                    Ok(Ok(info)) => {
                                        state.pid_inspect = Some(info);
                                    }
                                    Ok(Err(e)) => {
                                        state.status_msg = Some(format!("inspect_pid: {e}"));
                                    }
                                    Err(e) => {
                                        state.status_msg = Some(format!("rpc transport: {e}"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Theme picker key handling — intercepts all keys while open.
    if state.theme_picker.is_some() {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Restore original theme before closing.
                if let Some(p) = state.theme_picker.take() {
                    state.theme = p.original_theme;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ref mut p) = state.theme_picker {
                    p.cursor = p.cursor.saturating_sub(1);
                    // Live preview: apply the hovered theme immediately.
                    let reg = Registry::builtin();
                    if let Some(t) = reg.get(p.names[p.cursor]) {
                        state.theme = t.clone();
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ref mut p) = state.theme_picker {
                    let max = p.names.len().saturating_sub(1);
                    p.cursor = (p.cursor + 1).min(max);
                    // Live preview: apply the hovered theme immediately.
                    let reg = Registry::builtin();
                    if let Some(t) = reg.get(p.names[p.cursor]) {
                        state.theme = t.clone();
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(p) = state.theme_picker.take() {
                    let name = p.names[p.cursor];
                    let reg = Registry::builtin();
                    if let Some(t) = reg.get(name) {
                        // Theme already applied via live preview; persist to config.
                        state.theme = t.clone();
                        if let Err(e) = pyre_themes::config::save_theme_name(name) {
                            tracing::warn!("save theme failed: {e}");
                            state.status_msg = Some(format!("theme saved (warn: {e})"));
                        } else {
                            state.status_msg = Some(format!("theme: {}", t.display_name));
                        }
                    }
                }
            }
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Block stdout pager key handling — intercepts all keys while open.
    if state.pager.is_some() {
        let visible_rows = body_area.height.saturating_sub(2) as usize; // inner - footer
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                state.pager = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(ref mut p) = state.pager {
                    p.scroll_up(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(ref mut p) = state.pager {
                    p.scroll_down(1, visible_rows);
                }
            }
            KeyCode::PageUp => {
                let n = visible_rows.max(1);
                if let Some(ref mut p) = state.pager {
                    p.scroll_up(n);
                }
            }
            KeyCode::PageDown => {
                let n = visible_rows.max(1);
                if let Some(ref mut p) = state.pager {
                    p.scroll_down(n, visible_rows);
                }
            }
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Detect Ctrl-Space prefix.
    // Crossterm canonically delivers Ctrl-Space as Char(' ') + CONTROL, but
    // some terminals send it as KeyCode::Null + CONTROL — accept both.
    if !*prefix_active
        && mods.contains(KeyModifiers::CONTROL)
        && (matches!(code, KeyCode::Char(' ')) || matches!(code, KeyCode::Null))
    {
        *prefix_active = true;
        return KeyAction::Continue;
    }

    if *prefix_active {
        *prefix_active = false;
        return match handle_prefix_key(state, code).await {
            PrefixAction::Continue => KeyAction::Continue,
            PrefixAction::Quit => KeyAction::Quit,
        };
    }

    // Search overlay key handling — intercepts all keys while open.
    if state.search.open {
        match (code, mods) {
            (KeyCode::Esc, _) => {
                state.search.open = false;
                state.search.rx = None;
            }
            (KeyCode::Enter, _) => {
                if !state.search.results.is_empty() {
                    let hit = &state.search.results[state.search.cursor];
                    let target_pane = hit.block.pane;
                    let target_block = hit.block.id;

                    // Search all sessions + tabs for a loaded pane matching
                    // target_pane. Prefer the current session; fall back to others.
                    type JumpTarget = (usize, usize, pyre_proto::PaneId, usize);
                    let mut jump: Option<JumpTarget> = None;
                    'outer: for (si, sv) in state.sessions.iter().enumerate() {
                        for (ti, tab) in sv.tabs.iter().enumerate() {
                            for pid in pane_leaves_in_order(&tab.root) {
                                if pid == target_pane {
                                    if let Some(slot_idx) = pane_to_slot_idx(&state.slots, pid) {
                                        jump = Some((si, ti, pid, slot_idx));
                                        if si == state.active_session {
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some((si, ti, pane_id, slot_idx)) = jump {
                        state.active_session = si;
                        state.sessions[si].active_tab = ti;
                        state.sessions[si].tabs[ti].focus_pane = pane_id;
                        let maybe_cursor = state.slots[slot_idx].as_ref().and_then(|s| {
                            s.recent_blocks.iter().position(|b| b.id == target_block)
                        });
                        if let Some(c) = maybe_cursor {
                            if let Some(slot) = state.slots[slot_idx].as_mut() {
                                slot.ribbon_cursor = Some(c);
                            }
                        }
                    } else {
                        state.status_msg = Some("search: result pane not loaded".to_owned());
                    }
                }
                state.search.open = false;
                state.search.rx = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                state.search.cursor = state.search.cursor.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                let max = state.search.results.len().saturating_sub(1);
                state.search.cursor = (state.search.cursor + 1).min(max);
            }
            (KeyCode::Backspace, _) => {
                state.search.input.pop();
                state.search.pending_query = Some(state.search.input.clone());
                state.search.last_query_at = Instant::now();
            }
            (KeyCode::Char(c), _) => {
                state.search.input.push(c);
                state.search.pending_query = Some(state.search.input.clone());
                state.search.last_query_at = Instant::now();
            }
            _ => {}
        }
        return KeyAction::Continue;
    }

    // Sidebar navigation when sidebar is focused.
    if state.sidebar_open && state.sidebar_focused {
        match code {
            KeyCode::Up => {
                state.sidebar_cursor = state.sidebar_cursor.saturating_sub(1);
                return KeyAction::Continue;
            }
            KeyCode::Down => {
                let max = state.sidebar_data.len().saturating_sub(1);
                state.sidebar_cursor = (state.sidebar_cursor + 1).min(max);
                return KeyAction::Continue;
            }
            KeyCode::Enter => {
                if let Some(info) = state.sidebar_data.get(state.sidebar_cursor) {
                    let target = info.id;
                    let in_tab = {
                        let sv = &state.sessions[state.active_session];
                        let tab = &sv.tabs[sv.active_tab];
                        pane_leaves_in_order(&tab.root).contains(&target)
                    };
                    if in_tab {
                        let sv = &mut state.sessions[state.active_session];
                        sv.tabs[sv.active_tab].focus_pane = target;
                        state.sidebar_focused = false;
                        let _ = state
                            .control
                            .mark_pane_seen(tarpc::context::current(), target)
                            .await;
                    } else {
                        state.status_msg =
                            Some("open this pane first in a tab to focus".to_owned());
                    }
                }
                return KeyAction::Continue;
            }
            KeyCode::Esc => {
                state.sidebar_focused = false;
                return KeyAction::Continue;
            }
            _ => {}
        }
    }

    // Block ribbon scrollback navigation (Ctrl-B [ mode).
    {
        let sv = &state.sessions[state.active_session];
        let tab = &sv.tabs[sv.active_tab];
        if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
            if let Some(slot) = state.slots[slot_idx].as_ref() {
                if slot.ribbon_cursor.is_some() {
                    match code {
                        KeyCode::Left | KeyCode::Char('h') => {
                            let s = state.slots[slot_idx].as_mut().expect("checked");
                            s.ribbon_cursor = s.ribbon_cursor.map(|c| c.saturating_sub(1));
                            return KeyAction::Continue;
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            let s = state.slots[slot_idx].as_mut().expect("checked");
                            let max = s.recent_blocks.len().saturating_sub(1);
                            s.ribbon_cursor = s.ribbon_cursor.map(|c| (c + 1).min(max));
                            return KeyAction::Continue;
                        }
                        KeyCode::Esc => {
                            let s = state.slots[slot_idx].as_mut().expect("checked");
                            s.ribbon_cursor = None;
                            return KeyAction::Continue;
                        }
                        KeyCode::Enter => {
                            // Open modal pager for the focused block's stdout.
                            if let Some(cursor) = slot.ribbon_cursor {
                                if let Some(block) = slot.recent_blocks.get(cursor) {
                                    let block_id_str = block.id.0.to_string();
                                    let exit_code = block.exit_code;
                                    let block_id = block.id;
                                    match state
                                        .control
                                        .get_block_stdout(tarpc::context::current(), block_id)
                                        .await
                                    {
                                        Ok(Ok(bytes)) => {
                                            state.pager = Some(PagerState::new(
                                                block_id_str,
                                                exit_code,
                                                &bytes,
                                            ));
                                        }
                                        Ok(Err(e)) => {
                                            state.status_msg =
                                                Some(format!("pager: rpc error: {e}"));
                                        }
                                        Err(e) => {
                                            state.status_msg =
                                                Some(format!("pager: transport error: {e}"));
                                        }
                                    }
                                }
                            }
                            return KeyAction::Continue;
                        }
                        _ => {
                            return KeyAction::Continue;
                        }
                    }
                }
            }
        }
    }

    // PgUp / PgDn for scrollback buffer (unmodified only).
    if mods == KeyModifiers::NONE {
        let sv = &state.sessions[state.active_session];
        let tab = &sv.tabs[sv.active_tab];
        if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
            let half_page = (body_area.height / 2).max(1) as usize;
            if let Some(slot) = state.slots[slot_idx].as_mut() {
                match code {
                    KeyCode::PageUp => {
                        slot.scroll_offset =
                            (slot.scroll_offset + half_page).min(slot.scrollback_capacity);
                        return KeyAction::Continue;
                    }
                    KeyCode::PageDown => {
                        slot.scroll_offset = slot.scroll_offset.saturating_sub(half_page);
                        return KeyAction::Continue;
                    }
                    _ => {}
                }
            }
        }
    }

    // Forward key to focused pane.
    if let Some(bytes) = key_to_bytes(code, mods) {
        let sv = &state.sessions[state.active_session];
        let tab = &sv.tabs[sv.active_tab];
        if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
            if let Some(slot) = state.slots[slot_idx].as_mut() {
                slot.scroll_offset = 0;
                let t0 = Instant::now();
                let send_result = slot.input_tx.send(bytes.clone()).await;
                let elapsed_us = t0.elapsed().as_micros();
                tracing::debug!(
                    slot_idx,
                    key_bytes = bytes.len(),
                    elapsed_us,
                    send_ok = send_result.is_ok(),
                    "send_keys: input_tx.send (inline await)"
                );
            }
        }
    }

    KeyAction::Continue
}
