use pyre_proto::{layout::LayoutNode, PaneId, SessionId};

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
            }],
            active_tab: 0,
        }
    }
}
