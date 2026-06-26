use pyre_proto::{layout::LayoutNode, PaneId, SessionId, WindowId};

use crate::model::layout::{pane_leaves_in_order, pane_to_slot_idx};
use crate::model::pane::PaneSlot;
use crate::model::tab::Tab;

// ─────────────────────────────────────────────────────────────────────────────
// SessionView
// ─────────────────────────────────────────────────────────────────────────────

/// Per-session view: tabs and panes for one daemon session.
pub struct SessionView {
    pub id: SessionId,
    pub name: String,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
}

impl SessionView {
    /// Construct a new SessionView with a single tab rooted at `pane_id`.
    pub fn new_single_pane(id: SessionId, name: String, pane_id: PaneId) -> Self {
        Self {
            id,
            name,
            tabs: vec![Tab {
                root: LayoutNode::Leaf(pane_id),
                focus_pane: pane_id,
                zoomed: None,
                boundaries: Vec::new(),
                drag: None,
                window_id: WindowId(uuid::Uuid::nil()),
                window_name: String::new(),
            }],
            active_tab: 0,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stale-session helpers  (I-4, I-5)
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` when `sv` has at least one pane whose slot is alive in `slots`.
///
/// A session where every pane ID in every tab maps to a `None` slot is stale —
/// its worker crashed and the slots were pruned.  Such sessions must not be
/// rendered as clickable pills (I-5) and must not be treated as auto-spawn
/// targets (I-7).
pub fn session_is_live(sv: &SessionView, slots: &[Option<PaneSlot>]) -> bool {
    sv.tabs.iter().any(|tab| {
        pane_leaves_in_order(&tab.root)
            .iter()
            .any(|&pid| pane_to_slot_idx(slots, pid).is_some())
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(pane_id: PaneId) -> SessionView {
        SessionView::new_single_pane(SessionId(uuid::Uuid::new_v4()), "test".into(), pane_id)
    }

    /// I-5: a session whose only pane has a dead (None) slot is not live.
    #[test]
    fn test_list_sessions_skips_stale_workers() {
        let dead_pane = PaneId(uuid::Uuid::new_v4());
        let live_pane = PaneId(uuid::Uuid::new_v4());

        // Two sessions: one stale (dead_pane has no slot), one live.
        let stale = make_session(dead_pane);
        let live = make_session(live_pane);

        // slots: index 0 = None (dead), index 1 = a minimal live entry.
        // We can't construct PaneSlot (needs alacritty Term), so we use the
        // None/Some structure via the pane_to_slot_idx path which only checks
        // that slots[i].is_some() && slots[i].pane_id == pid.
        // Simulate with an empty slots vec (all None):
        let all_none: Vec<Option<PaneSlot>> = vec![None];

        assert!(
            !session_is_live(&stale, &all_none),
            "stale session (all slots None) must NOT be live"
        );

        // For the live session we verify the logic with an empty slots vec too —
        // if no slot carries live_pane, the session is also not live.
        assert!(
            !session_is_live(&live, &all_none),
            "session with no matching live slot must NOT be live"
        );

        // When slots is empty, no session is live.
        let empty: Vec<Option<PaneSlot>> = Vec::new();
        assert!(!session_is_live(&stale, &empty));
        assert!(!session_is_live(&live, &empty));
    }

    /// Verify that `new_single_pane` produces exactly one tab with one leaf.
    #[test]
    fn test_new_single_pane_structure() {
        let pid = PaneId(uuid::Uuid::new_v4());
        let sv = SessionView::new_single_pane(SessionId(uuid::Uuid::new_v4()), "s".into(), pid);
        assert_eq!(sv.tabs.len(), 1);
        let leaves = pane_leaves_in_order(&sv.tabs[0].root);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0], pid);
        assert_eq!(sv.tabs[0].focus_pane, pid);
        assert_eq!(sv.active_tab, 0);
    }
}
