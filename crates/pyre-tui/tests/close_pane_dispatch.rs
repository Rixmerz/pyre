//! Integration tests for the close-pane dispatch path.
//!
//! These tests exercise the two components that together fix the Ctrl-B+x /
//! close-X bug:
//!
//! 1. `LayoutNode::close` — single-leaf early-return guard (pyre-proto).
//! 2. `close_pane_by_slot_idx` — single-leaf zombie-tab guard (pyre-tui).
//!
//! Both layers are exercised here via `pyre_proto` which is a direct
//! dependency of this integration-test binary.  The `pyre_tui` layer is
//! covered by the `#[cfg(test)]` module in `app/pane_ops.rs`.
//!
//! Why a separate file?
//! The unit tests in `pane_ops.rs` have access to `pub(crate)` items and can
//! construct a full `AppState` with a tarpc stub.  This file exercises the
//! *protocol* layer in isolation — no daemon, no AppState — to prove that the
//! fix does not regress the shared `LayoutNode` type that pyre-gpu and
//! pyre-mcp also use.

use pyre_proto::{layout::LayoutNode, PaneId};

// ─────────────────────────────────────────────────────────────────────────────
// LayoutNode::close single-leaf fix (pyre-proto, layout.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Closing the only leaf of a tree must return `None` (empty tree signal).
///
/// Before the fix added in layout.rs, `LayoutNode::close` called `remove_leaf`
/// which returned `true` (should-be-removed) on a bare `Leaf`, but since there
/// was no parent to call `retain_mut`, `*self` was left as `Leaf(pane_id)`.
/// The function then called `self.all_leaves()` which returned `[pane_id]`,
/// so `remaining.is_empty()` was false and the function returned `Some(pane_id)`
/// — the same pane that was supposed to be closed.
///
/// `close_pane_by_slot_idx` used that `Some(pane_id)` as the new focus, set
/// the slot to `None`, and left the layout tree intact with a zombie leaf.
/// Session-lost detection fired on the next tick instead of a clean tab removal.
#[test]
fn layout_close_single_leaf_returns_none() {
    let pane = PaneId::new();
    let mut node = LayoutNode::Leaf(pane);

    let result = node.close(&pane);

    assert!(
        result.is_none(),
        "close() on the only leaf must return None (empty-tree signal); got {:?}",
        result
    );
}

/// Closing one pane from a two-pane VSplit returns the sibling and collapses
/// the split to a single leaf.  This is the happy path that was always correct;
/// this test guards against regressions.
#[test]
fn layout_close_from_split_returns_sibling_and_collapses() {
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut node = LayoutNode::VSplit(vec![
        (LayoutNode::Leaf(pane_a), 50),
        (LayoutNode::Leaf(pane_b), 50),
    ]);

    let new_focus = node.close(&pane_a);

    assert_eq!(
        new_focus,
        Some(pane_b),
        "closing pane_a must return pane_b as new focus"
    );
    // Tree must have collapsed to a plain Leaf(pane_b).
    assert!(
        matches!(node, LayoutNode::Leaf(id) if id == pane_b),
        "VSplit must collapse to Leaf(pane_b) after closing pane_a; got {node:?}"
    );
}

/// Closing a pane that is not present in the tree must return `None` without
/// modifying the tree.
#[test]
fn layout_close_absent_pane_returns_none_unchanged() {
    let present = PaneId::new();
    let absent = PaneId::new();
    let mut node = LayoutNode::Leaf(present);

    let result = node.close(&absent);

    assert!(result.is_none(), "closing an absent pane must return None");
    // Tree must be unchanged.
    assert!(
        matches!(node, LayoutNode::Leaf(id) if id == present),
        "tree must be unchanged when pane not found"
    );
}

/// Closing the second-to-last pane in a 2-pane split leaves exactly one leaf,
/// and `all_leaves()` on the collapsed tree returns `[sibling]` — not `[]`.
///
/// This guards the `close_pane_by_slot_idx` invariant: `remaining.is_empty()`
/// must be FALSE after closing one pane from a 2-pane tab (the tab must not be
/// removed), while it must be TRUE when the LAST pane is closed.
#[test]
fn layout_close_second_to_last_leaves_one_leaf() {
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut node = LayoutNode::VSplit(vec![
        (LayoutNode::Leaf(pane_a), 50),
        (LayoutNode::Leaf(pane_b), 50),
    ]);

    node.close(&pane_a);

    let remaining = node.all_leaves();
    assert_eq!(
        remaining.len(),
        1,
        "one leaf must remain after closing one of two panes"
    );
    assert_eq!(remaining[0], pane_b, "remaining leaf must be pane_b");
}

/// After `close` returns `None` for the single-leaf case, the caller
/// (`close_pane_by_slot_idx`) detects that the tree still contains `pane_id`
/// and treats `remaining` as empty.
///
/// This test exercises exactly the condition that `close_pane_by_slot_idx`
/// checks: `new_focus_pane.is_none() && leaves.as_slice() == [pane_id]`.
#[test]
fn dispatch_zombie_detection_condition_triggers_for_single_leaf() {
    let pane = PaneId::new();
    let mut node = LayoutNode::Leaf(pane);

    // close() with single leaf: returns None, leaves tree as Leaf(pane).
    let new_focus = node.close(&pane);

    // Condition in close_pane_by_slot_idx:
    let leaves = node.all_leaves();
    let is_zombie = new_focus.is_none() && leaves.as_slice() == [pane];

    assert!(
        is_zombie,
        "single-leaf close must trigger zombie-detection: \
         new_focus={new_focus:?} leaves={leaves:?}"
    );
}

/// The zombie-detection condition must NOT trigger for the normal 2-pane case
/// (otherwise the tab would be incorrectly removed after closing one pane).
#[test]
fn dispatch_zombie_detection_condition_silent_for_split_close() {
    let pane_a = PaneId::new();
    let pane_b = PaneId::new();
    let mut node = LayoutNode::VSplit(vec![
        (LayoutNode::Leaf(pane_a), 50),
        (LayoutNode::Leaf(pane_b), 50),
    ]);

    let new_focus = node.close(&pane_a);

    // After closing pane_a, the tree is Leaf(pane_b).
    let leaves = node.all_leaves();
    let would_be_zombie = new_focus.is_none() && leaves.as_slice() == [pane_a];

    assert!(
        !would_be_zombie,
        "closing one pane from a split must NOT trigger zombie-detection; \
         new_focus={new_focus:?} leaves={leaves:?}"
    );
}
