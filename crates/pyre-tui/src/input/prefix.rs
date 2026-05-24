//! Ctrl-Space prefix key handler — extracted from run_tui event loop (Wave 1E).
//!
//! The prefix state machine is a single boolean (`prefix_active`) local to the
//! event loop. When Ctrl-Space is detected the flag is set; on the next key event
//! this module dispatches the bound action and returns `PrefixAction` so the
//! caller knows whether to `continue` the loop or `break` (quit).

use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use pyre_themes::Registry;

use crate::render::overlay::picker::ThemePickerState;
use crate::{
    close_focused_pane, focus_next, focused_slot_idx, open_new_tab, split_active, AppState,
    NamePrompt, PromptKind,
};

/// Outcome of a prefix-key dispatch, returned to the event loop.
pub(crate) enum PrefixAction {
    /// Action executed; caller should `continue` the event loop.
    Continue,
    /// Quit requested (Ctrl-Space q or Ctrl-Space x with no sessions left).
    Quit,
}

/// Handle one key following a Ctrl-Space prefix.
///
/// The caller must clear `prefix_active` before calling this. The function
/// handles the entire prefix match and returns a `PrefixAction` so the caller
/// can drive the loop control-flow (`continue` vs `break`).
pub(crate) async fn handle_prefix_key(state: &mut AppState, code: KeyCode) -> PrefixAction {
    match code {
        KeyCode::Char('q') => return PrefixAction::Quit,

        KeyCode::Char('c') => {
            if let Err(e) = open_new_tab(state, None).await {
                tracing::warn!("open_new_tab failed: {e}");
            }
        }

        KeyCode::Char('n') => {
            let sv = state.active_session_view_mut();
            sv.active_tab = (sv.active_tab + 1) % sv.tabs.len();
        }

        KeyCode::Char('p') => {
            let sv = state.active_session_view_mut();
            sv.active_tab = (sv.active_tab + sv.tabs.len() - 1) % sv.tabs.len();
        }

        KeyCode::Char('"') => {
            if let Err(e) = split_active(state, true).await {
                tracing::warn!("HSplit failed: {e}");
            }
        }

        KeyCode::Char('%') => {
            if let Err(e) = split_active(state, false).await {
                tracing::warn!("VSplit failed: {e}");
            }
        }

        KeyCode::Right | KeyCode::Down => {
            let si = state.active_session;
            let ti = state.sessions[si].active_tab;
            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, true);
        }

        KeyCode::Left | KeyCode::Up => {
            let si = state.active_session;
            let ti = state.sessions[si].active_tab;
            focus_next(&mut state.sessions[si].tabs[ti], &state.slots, false);
        }

        // Enter scrollback mode for focused pane (block ribbon)
        KeyCode::Char('[') => {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                if let Some(slot) = state.slots[slot_idx].as_mut() {
                    let last = slot.recent_blocks.len().saturating_sub(1);
                    slot.ribbon_cursor = Some(last);
                }
            }
        }

        // Exit scrollback mode for focused pane (block ribbon)
        KeyCode::Char(']') => {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                if let Some(slot) = state.slots[slot_idx].as_mut() {
                    slot.ribbon_cursor = None;
                }
            }
        }

        // Open search overlay
        KeyCode::Char('/') => {
            state.search.open = true;
            state.search.input.clear();
            state.search.cursor = 0;
            state.search.results.clear();
            state.search.pending_query = None;
            state.search.rx = None;
            state.status_msg = None;
        }

        // Zoom toggle (Ctrl-Space z)
        KeyCode::Char('z') => {
            let sv = state.active_session_view_mut();
            let tab = &mut sv.tabs[sv.active_tab];
            if tab.zoomed.is_some() {
                tab.zoomed = None;
            } else {
                tab.zoomed = Some(tab.focus_pane);
            }
        }

        // Copy last block stdout to clipboard (Ctrl-Space y)
        KeyCode::Char('y') => {
            let sv = &state.sessions[state.active_session];
            let tab = &sv.tabs[sv.active_tab];
            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                if let Some(slot) = state.slots[slot_idx].as_ref() {
                    if let Some(last_block) = slot.recent_blocks.last() {
                        let block_id = last_block.id;
                        match state
                            .control
                            .get_block_stdout(tarpc::context::current(), block_id)
                            .await
                        {
                            Ok(Ok(bytes)) => {
                                let text = String::from_utf8_lossy(&bytes);
                                match crate::clipboard::copy_to_clipboard(&text) {
                                    Ok(()) => {
                                        state.status_msg = Some("copied to clipboard".to_owned());
                                    }
                                    Err(e) => {
                                        tracing::warn!("clipboard: {e}");
                                        state.status_msg = Some(format!("clipboard error: {e}"));
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                state.status_msg = Some(format!("get_block_stdout rpc: {e}"));
                            }
                            Err(e) => {
                                state.status_msg = Some(format!("rpc transport: {e}"));
                            }
                        }
                    } else {
                        state.status_msg = Some("no blocks".to_owned());
                    }
                }
            }
        }

        // Close focused pane (Ctrl-Space x)
        KeyCode::Char('x') => {
            close_focused_pane(state);
            // If all sessions are gone, exit the TUI loop.
            if state.sessions.is_empty() {
                return PrefixAction::Quit;
            }
        }

        // Toggle sidebar (Ctrl-Space s)
        KeyCode::Char('s') => {
            state.sidebar_open = !state.sidebar_open;
            if state.sidebar_open {
                state.sidebar_focused = true;
                state.sidebar_last_poll = Instant::now() - Duration::from_secs(10);
            } else {
                state.sidebar_focused = false;
            }
        }

        // New session (Ctrl-Space S — uppercase to avoid collision with Ctrl-Space s sidebar)
        KeyCode::Char('S') => {
            state.prompt = Some(NamePrompt {
                kind: PromptKind::NewSession,
                input: String::new(),
            });
        }

        // Theme picker (Ctrl-Space T — uppercase to avoid collision with lower-t)
        KeyCode::Char('T') => {
            let reg = Registry::builtin();
            let names: Vec<&'static str> = reg.list().iter().map(|t| t.name).collect();
            // Pre-select the currently active theme.
            let cursor = names
                .iter()
                .position(|&n| n == state.theme.name)
                .unwrap_or(0);
            // Snapshot the current theme so Esc can restore it.
            let original_theme = state.theme.clone();
            state.theme_picker = Some(ThemePickerState {
                cursor,
                names,
                original_theme,
            });
        }

        // Rename active session (Ctrl-Space , — mirrors tmux rename-session)
        KeyCode::Char(',') => {
            let sv = &state.sessions[state.active_session];
            let current_name = sv.name.clone();
            let session_id = sv.id;
            state.prompt = Some(NamePrompt {
                kind: PromptKind::RenameSession(session_id),
                input: current_name,
            });
        }

        // Toggle toast notifications (Ctrl-Space N)
        KeyCode::Char('N') => {
            state.toast_deck.enabled = !state.toast_deck.enabled;
            let label = if state.toast_deck.enabled {
                "notifications on"
            } else {
                "notifications off"
            };
            state.status_msg = Some(label.to_owned());
        }

        // All other prefix keys consumed silently
        _ => {}
    }

    PrefixAction::Continue
}
