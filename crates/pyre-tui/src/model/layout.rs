//! Layout helper functions — extracted from main.rs (Wave 1F).
//!
//! These are pure, synchronous helpers that operate on `LayoutNode` trees
//! and slot vectors. No async, no RPC.

use std::collections::HashMap;

use pyre_proto::{layout::LayoutNode, PaneId};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::model::pane::PaneSlot;

/// Walk the tree DFS and collect `PaneId` for every leaf, in order.
pub(crate) fn pane_leaves_in_order(node: &LayoutNode) -> Vec<PaneId> {
    match node {
        LayoutNode::Leaf(id) => vec![*id],
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => children
            .iter()
            .flat_map(|(c, _)| pane_leaves_in_order(c))
            .collect(),
    }
}

/// Look up the slot index for a `PaneId` by scanning `slots`.
pub(crate) fn pane_to_slot_idx(slots: &[Option<PaneSlot>], pane_id: PaneId) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.as_ref().map(|sl| sl.pane_id == pane_id).unwrap_or(false))
}

/// Return the slot index for `focus_pane` in `slots`.
pub(crate) fn focused_slot_idx(focus_pane: PaneId, slots: &[Option<PaneSlot>]) -> Option<usize> {
    pane_to_slot_idx(slots, focus_pane)
}

/// Build a `PaneId → slot_idx` map for the current slots vec.
pub(crate) fn build_pane_slot_map(slots: &[Option<PaneSlot>]) -> HashMap<PaneId, usize> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|sl| (sl.pane_id, i)))
        .collect()
}

/// Mutably access the children of the split node at `path`.
pub(crate) fn children_at_mut<'a>(
    root: &'a mut LayoutNode,
    path: &[usize],
) -> Option<&'a mut Vec<(LayoutNode, u16)>> {
    let mut node = root;
    for &idx in path {
        match node {
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                node = &mut children[idx].0;
            }
            LayoutNode::Leaf(_) => return None,
        }
    }
    match node {
        LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => Some(children),
        LayoutNode::Leaf(_) => None,
    }
}

/// Walk the layout tree and collect (PaneId, screen_rect) for each leaf,
/// computing rects the same way render_layout does (without actually rendering).
/// Callers convert PaneId → slot_idx via `pane_to_slot_idx`.
pub(crate) fn collect_leaf_rects(node: &LayoutNode, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        LayoutNode::Leaf(pane_id) => {
            out.push((*pane_id, area));
        }
        LayoutNode::HSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
        LayoutNode::VSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            for ((child, _), rect) in children.iter().zip(rects.iter()) {
                collect_leaf_rects(child, *rect, out);
            }
        }
    }
}

/// Returns true if (col, row) is inside `rect`.
pub(crate) fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Cycle focus to the next leaf (DFS order), wrapping around.
/// `slots` is passed so dead/None leaves can be skipped.
pub(crate) fn focus_next(
    tab: &mut crate::model::tab::Tab,
    slots: &[Option<PaneSlot>],
    forward: bool,
) {
    let live_panes: Vec<PaneId> = pane_leaves_in_order(&tab.root)
        .into_iter()
        .filter(|&pid| {
            pane_to_slot_idx(slots, pid)
                .and_then(|i| slots.get(i))
                .and_then(|s| s.as_ref())
                .is_some()
        })
        .collect();

    if live_panes.is_empty() {
        return;
    }

    let current_pos = live_panes
        .iter()
        .position(|&p| p == tab.focus_pane)
        .unwrap_or(0);

    let next_pos = if forward {
        (current_pos + 1) % live_panes.len()
    } else {
        (current_pos + live_panes.len() - 1) % live_panes.len()
    };

    tab.focus_pane = live_panes[next_pos];
}
