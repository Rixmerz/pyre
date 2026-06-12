//! Ctrl-Space prefix key handler — extracted from run_tui event loop (Wave 1E).
//!
//! The prefix state machine is a single boolean (`prefix_active`) local to the
//! event loop. When Ctrl-Space is detected the flag is set; on the next key event
//! this module dispatches the bound action and returns `PrefixAction` so the
//! caller knows whether to `continue` the loop or `break` (quit).

use std::time::{Duration, Instant};

use crossterm::event::KeyCode;
use pyre_themes::Registry;

use crate::app::pane_ops::{close_focused_pane, open_new_tab, split_active};
use crate::app::state::AppState;
use crate::model::layout::{focus_next, focused_slot_idx};
use crate::model::prompt::{NamePrompt, PromptKind};
use crate::render::overlay::picker::ThemePickerState;

/// Outcome of a prefix-key dispatch, returned to the event loop.
#[derive(Debug)]
pub(crate) enum PrefixAction {
    /// Action executed; caller should `continue` the event loop.
    Continue,
    /// Quit requested (Ctrl-Space q or Ctrl-Space x with no sessions left).
    Quit,
    /// Detach requested (Ctrl-Space d): exit TUI but leave the daemon session running.
    Detach,
    /// Help overlay toggled (Ctrl-Space ?).
    ToggleHelp,
}

/// Classify a prefix key into an immediate `PrefixAction` without inspecting
/// `AppState`.  Returns `Some(action)` for keys that produce a definitive
/// outcome regardless of application state, and `None` for all other keys
/// (whose handling requires mutable access to `AppState`).
///
/// Extracted as a pure, synchronous function so the dispatch table can be
/// verified in unit tests without constructing a live daemon client.
// Only called from the inline test module; suppress the dead_code lint for
// non-test builds rather than making the function pub or adding a production
// call site prematurely.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn immediate_prefix_action(code: KeyCode) -> Option<PrefixAction> {
    match code {
        KeyCode::Char('q') => Some(PrefixAction::Quit),
        KeyCode::Char('d') => Some(PrefixAction::Detach),
        KeyCode::Char('?') => Some(PrefixAction::ToggleHelp),
        _ => None,
    }
}

/// Handle one key following a Ctrl-Space prefix.
///
/// The caller must clear `prefix_active` before calling this. The function
/// handles the entire prefix match and returns a `PrefixAction` so the caller
/// can drive the loop control-flow (`continue` vs `break`).
pub(crate) async fn handle_prefix_key(state: &mut AppState, code: KeyCode) -> PrefixAction {
    match code {
        KeyCode::Char('q') => return PrefixAction::Quit,

        // Detach: exit TUI, leave daemon session alive (Ctrl-Space d).
        KeyCode::Char('d') => return PrefixAction::Detach,

        // Help overlay toggle (Ctrl-Space ?).
        KeyCode::Char('?') => return PrefixAction::ToggleHelp,

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

// ─────────────────────────────────────────────────────────────────────────────
// Tests — prefix dispatch table (pure, no daemon required)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl-Space ? must map to PrefixAction::ToggleHelp.
    ///
    /// Verifies the help-overlay dispatch entry in the prefix table so that a
    /// future rename or re-ordering of the match arms cannot silently drop the
    /// binding.
    #[test]
    fn question_mark_dispatches_to_toggle_help() {
        let action = immediate_prefix_action(KeyCode::Char('?'));
        assert!(
            matches!(action, Some(PrefixAction::ToggleHelp)),
            "Ctrl-Space ? must produce PrefixAction::ToggleHelp; got: {action:?}",
        );
    }

    /// Ctrl-Space d must map to PrefixAction::Detach.
    ///
    /// Detach exits the TUI while leaving the daemon session running; this test
    /// pins that binding so it cannot regress to Quit or Continue.
    #[test]
    fn d_key_dispatches_to_detach() {
        let action = immediate_prefix_action(KeyCode::Char('d'));
        assert!(
            matches!(action, Some(PrefixAction::Detach)),
            "Ctrl-Space d must produce PrefixAction::Detach; got: {action:?}",
        );
    }

    /// Ctrl-Space q must map to PrefixAction::Quit.
    ///
    /// Guard: Quit must not accidentally swap to Detach or vice-versa.
    #[test]
    fn q_key_dispatches_to_quit() {
        let action = immediate_prefix_action(KeyCode::Char('q'));
        assert!(
            matches!(action, Some(PrefixAction::Quit)),
            "Ctrl-Space q must produce PrefixAction::Quit; got: {action:?}",
        );
    }

    /// Keys that need AppState (e.g. 'c', 'z') must return None from the pure
    /// helper so the async path handles them instead.
    #[test]
    fn stateful_key_returns_none_from_immediate() {
        for key in [
            KeyCode::Char('c'),
            KeyCode::Char('z'),
            KeyCode::Char('n'),
            KeyCode::Char('p'),
            KeyCode::Char('x'),
            KeyCode::Char('s'),
        ] {
            let action = immediate_prefix_action(key);
            assert!(
                action.is_none(),
                "key {key:?} must return None from immediate_prefix_action (needs AppState); got: {action:?}",
            );
        }
    }
}
