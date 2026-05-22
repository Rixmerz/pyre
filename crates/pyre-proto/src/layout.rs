//! Canonical tiling layout model shared across pyre clients and the daemon.
//!
//! `LayoutNode` is the wire type for session split topology.  It lives in
//! `pyre-proto` so every crate — `pyred`, `pyre-tui`, `pyre-gpu`, `pyre-mcp`
//! — shares one definition without duplicating it.
//!
//! ## Invariants
//!
//! - Sibling weights (the `u16` in `HSplit`/`VSplit` children) **must sum to
//!   100**.  `split_focused` initialises splits at 50/50.  `set_weight` clamps
//!   the requested value to ≥ 5 and rebalances siblings proportionally.
//! - `LayoutNode::Leaf` carries a `PaneId` — the daemon-assigned UUID that is
//!   stable across the pane's lifetime.  This is intentionally different from
//!   the TUI's process-local `usize` slot index, which is not a stable
//!   identifier.
//! - `close` removes a leaf and collapses single-child splits automatically.

use serde::{Deserialize, Serialize};

use crate::PaneId;

// ─────────────────────────────────────────────────────────────────────────────
// Core types
// ─────────────────────────────────────────────────────────────────────────────

/// Recursive tiling tree.
///
/// `HSplit` children are stacked **top-to-bottom** (split on the horizontal
/// axis).  `VSplit` children are placed **side-by-side** (split on the
/// vertical axis).  This matches the TUI and GPU convention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutNode {
    Leaf(PaneId),
    /// Top-to-bottom stack.  Weights sum to 100.
    HSplit(Vec<(LayoutNode, u16)>),
    /// Side-by-side columns.  Weights sum to 100.
    VSplit(Vec<(LayoutNode, u16)>),
}

/// Split orientation for new panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orient {
    /// Side-by-side (new pane appears to the right).
    Vertical,
    /// Stacked (new pane appears below).
    Horizontal,
}

/// Cardinal direction for keyboard focus traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A pixel-space rectangle inside the framebuffer (or a unit viewport for
/// direction-finding purposes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Returns `true` if the pixel `(px, py)` falls inside this rectangle.
    #[allow(dead_code)] // useful for drag-resize hit-testing
    pub fn contains_pt(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LayoutNode impl
// ─────────────────────────────────────────────────────────────────────────────

impl LayoutNode {
    // ── Public API ────────────────────────────────────────────────────────────

    /// Compute the pixel `Rect` of `target` within `viewport`.
    ///
    /// Returns `None` if `target` is not present in the tree.
    pub fn rect_for(&self, viewport: Rect, target: &PaneId) -> Option<Rect> {
        self.leaves_inner(viewport)
            .into_iter()
            .find(|(id, _)| id == target)
            .map(|(_, r)| r)
    }

    /// Return all `(PaneId, Rect)` pairs for the given `viewport`, in DFS order.
    pub fn leaves(&self, viewport: Rect) -> Vec<(PaneId, Rect)> {
        self.leaves_inner(viewport)
    }

    /// Insert `new_pane` adjacent to `focused` with the given orientation.
    ///
    /// The focused leaf is replaced by a two-child split at 50/50 weight.
    pub fn split_focused(&mut self, focused: &PaneId, new_pane: PaneId, orient: Orient) {
        self.split_focused_inner(focused, new_pane, orient);
    }

    /// Remove `pane` from the tree, collapsing single-child splits.
    ///
    /// Returns the `PaneId` that should receive focus after removal, or `None`
    /// if the tree is now empty.
    pub fn close(&mut self, pane: &PaneId) -> Option<PaneId> {
        // Snapshot all panes in DFS order before mutating.
        let all = self.all_leaves();
        let pos = all.iter().position(|id| id == pane)?;

        self.remove_leaf(pane);

        let remaining = self.all_leaves();
        if remaining.is_empty() {
            return None;
        }
        // Prefer the pane that was immediately before the removed one.
        let focus_pos = if pos > 0 { pos - 1 } else { 0 };
        remaining.into_iter().nth(focus_pos)
    }

    /// Return the `PaneId` in direction `dir` from `current`.
    ///
    /// Uses a unit 1000×1000 virtual viewport for geometry comparisons.
    /// Returns `None` if there is no neighbour in that direction.
    pub fn focus_dir(&self, current: &PaneId, dir: Dir) -> Option<PaneId> {
        let vp = Rect {
            x: 0,
            y: 0,
            w: 1_000,
            h: 1_000,
        };
        let leaves = self.leaves_inner(vp);
        let cur_rect = leaves
            .iter()
            .find(|(id, _)| id == current)
            .map(|(_, r)| *r)?;

        let candidates: Vec<(PaneId, Rect)> = leaves
            .iter()
            .filter(|(id, r)| {
                if id == current {
                    return false;
                }
                match dir {
                    Dir::Left => r.x + r.w <= cur_rect.x,
                    Dir::Right => r.x >= cur_rect.x + cur_rect.w,
                    Dir::Up => r.y + r.h <= cur_rect.y,
                    Dir::Down => r.y >= cur_rect.y + cur_rect.h,
                }
            })
            .map(|(id, r)| (*id, *r))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        let cur_cx = cur_rect.x + cur_rect.w / 2;
        let cur_cy = cur_rect.y + cur_rect.h / 2;

        // Pick the candidate with the smallest Manhattan distance to `current`.
        candidates
            .into_iter()
            .min_by_key(|(_, r)| {
                let cx = r.x + r.w / 2;
                let cy = r.y + r.h / 2;
                (cx as i64 - cur_cx as i64).unsigned_abs()
                    + (cy as i64 - cur_cy as i64).unsigned_abs()
            })
            .map(|(id, _)| id)
    }

    /// Set the weight of the split child that contains `pane`.
    ///
    /// The weight is clamped to `[5, 95]`.  All siblings are rebalanced
    /// proportionally so the sum remains 100.  No-ops silently if `pane` is
    /// not found or the new weight equals the existing one.
    pub fn set_weight(&mut self, pane: &PaneId, weight: u16) {
        self.set_weight_inner(pane, weight);
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    pub fn all_leaves(&self) -> Vec<PaneId> {
        match self {
            LayoutNode::Leaf(id) => vec![*id],
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                children.iter().flat_map(|(n, _)| n.all_leaves()).collect()
            }
        }
    }

    fn leaves_inner(&self, vp: Rect) -> Vec<(PaneId, Rect)> {
        match self {
            LayoutNode::Leaf(id) => vec![(*id, vp)],
            LayoutNode::HSplit(children) => {
                let mut out = Vec::new();
                let total: u16 = children.iter().map(|(_, w)| w).sum();
                let total = total.max(1);
                let mut y = vp.y;
                for (node, weight) in children {
                    let h = (vp.h as u64 * *weight as u64 / total as u64) as u32;
                    out.extend(node.leaves_inner(Rect {
                        x: vp.x,
                        y,
                        w: vp.w,
                        h,
                    }));
                    y += h;
                }
                out
            }
            LayoutNode::VSplit(children) => {
                let mut out = Vec::new();
                let total: u16 = children.iter().map(|(_, w)| w).sum();
                let total = total.max(1);
                let mut x = vp.x;
                for (node, weight) in children {
                    let w = (vp.w as u64 * *weight as u64 / total as u64) as u32;
                    out.extend(node.leaves_inner(Rect {
                        x,
                        y: vp.y,
                        w,
                        h: vp.h,
                    }));
                    x += w;
                }
                out
            }
        }
    }

    /// Returns `true` if the node (or a descendant) was modified.
    fn split_focused_inner(&mut self, focused: &PaneId, new_pane: PaneId, orient: Orient) -> bool {
        match self {
            LayoutNode::Leaf(id) => {
                if id == focused {
                    let existing = LayoutNode::Leaf(*id);
                    let fresh = LayoutNode::Leaf(new_pane);
                    *self = match orient {
                        Orient::Vertical => LayoutNode::VSplit(vec![(existing, 50), (fresh, 50)]),
                        Orient::Horizontal => LayoutNode::HSplit(vec![(existing, 50), (fresh, 50)]),
                    };
                    true
                } else {
                    false
                }
            }
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                for (child, _) in children.iter_mut() {
                    if child.split_focused_inner(focused, new_pane, orient) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Remove the leaf `pane` from the tree, collapsing single-child splits.
    /// Returns `true` if the caller's slot should be removed (i.e. this node
    /// itself is the target leaf).
    fn remove_leaf(&mut self, pane: &PaneId) -> bool {
        match self {
            LayoutNode::Leaf(id) => id == pane,
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                children.retain_mut(|(child, _)| !child.remove_leaf(pane));
                // Collapse single-child split by replacing self with the child.
                if children.len() == 1 {
                    let (only, _) = children.drain(..).next().expect("len==1");
                    *self = only;
                }
                false
            }
        }
    }

    /// Walk the tree looking for the direct parent split that contains `pane`
    /// as a leaf descendant (not necessarily a direct child), and update that
    /// direct child's weight.
    fn set_weight_inner(&mut self, pane: &PaneId, new_weight: u16) -> bool {
        match self {
            LayoutNode::Leaf(_) => false,
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                // Check whether any direct child contains the target pane.
                let direct_pos = children
                    .iter()
                    .position(|(child, _)| child.all_leaves().contains(pane));

                if let Some(idx) = direct_pos {
                    // Clamp the requested weight to [5, 95] so no pane disappears.
                    let clamped = new_weight.clamp(5, 95);
                    let old = children[idx].1;
                    if clamped == old {
                        return true; // no change needed
                    }
                    let delta = clamped as i32 - old as i32;
                    children[idx].1 = clamped;

                    // Rebalance siblings: distribute -delta proportionally.
                    let siblings_total: i32 = children
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != idx)
                        .map(|(_, (_, w))| *w as i32)
                        .sum();

                    if siblings_total > 0 {
                        let mut leftover = -delta;
                        for (i, (_, w)) in children.iter_mut().enumerate() {
                            if i == idx {
                                continue;
                            }
                            let share = (*w as i32 * (-delta)) / siblings_total.max(1);
                            let adjusted = (*w as i32 + share).clamp(5, 95) as u16;
                            leftover -= adjusted as i32 - *w as i32;
                            *w = adjusted;
                        }
                        // Give the last non-target sibling any rounding leftover.
                        if let Some((_, w)) = children
                            .iter_mut()
                            .enumerate()
                            .filter(|(i, _)| *i != idx)
                            .last()
                            .map(|(_, x)| x)
                        {
                            *w = (*w as i32 + leftover).clamp(5, 95) as u16;
                        }
                    }

                    // Clamp entire vector to ensure sum stays sane (rounding drift).
                    // We do a final normalisation pass.
                    let total: u16 = children.iter().map(|(_, w)| *w).sum();
                    if total != 100 {
                        let diff = 100i32 - total as i32;
                        // Apply diff to the target child's weight (it already has the
                        // most headroom since we just set it).
                        children[idx].1 = (children[idx].1 as i32 + diff).clamp(5, 95) as u16;
                    }

                    return true;
                }

                // Not a direct parent — recurse.
                for (child, _) in children.iter_mut() {
                    if child.set_weight_inner(pane, new_weight) {
                        return true;
                    }
                }
                false
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 1000,
        }
    }

    // ── Migrated from pyre-gpu/src/layout.rs (commit 53354b4) ─────────────────

    #[test]
    fn leaf_full_viewport() {
        let id = PaneId::new();
        let node = LayoutNode::Leaf(id);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].0, id);
        assert_eq!(leaves[0].1, vp());
    }

    #[test]
    fn vsplit_two_equal_halves() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].1.x, 0);
        assert_eq!(leaves[1].1.x, 500);
        assert_eq!(leaves[0].1.w, 500);
        assert_eq!(leaves[1].1.w, 500);
    }

    #[test]
    fn hsplit_two_equal_halves() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::HSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].1.y, 0);
        assert_eq!(leaves[0].1.h, 500);
        assert_eq!(leaves[1].1.y, 500);
        assert_eq!(leaves[1].1.h, 500);
    }

    #[test]
    fn split_focused_vertical() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut node = LayoutNode::Leaf(a);
        node.split_focused(&a, b, Orient::Vertical);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].0, a);
        assert_eq!(leaves[1].0, b);
    }

    #[test]
    fn split_focused_horizontal() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut node = LayoutNode::Leaf(a);
        node.split_focused(&a, b, Orient::Horizontal);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].0, a);
        assert_eq!(leaves[1].0, b);
    }

    #[test]
    fn close_leaf_returns_sibling() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut node =
            LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        let new_focus = node.close(&b);
        assert_eq!(new_focus, Some(a));
        // Tree collapses to a plain leaf.
        assert!(matches!(node, LayoutNode::Leaf(_)));
    }

    #[test]
    fn close_first_pane() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut node =
            LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        let new_focus = node.close(&a);
        assert_eq!(new_focus, Some(b));
    }

    #[test]
    fn focus_dir_right() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        assert_eq!(node.focus_dir(&a, Dir::Right), Some(b));
        assert_eq!(node.focus_dir(&b, Dir::Right), None);
    }

    #[test]
    fn focus_dir_left() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        assert_eq!(node.focus_dir(&b, Dir::Left), Some(a));
        assert_eq!(node.focus_dir(&a, Dir::Left), None);
    }

    #[test]
    fn focus_dir_down() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::HSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        assert_eq!(node.focus_dir(&a, Dir::Down), Some(b));
    }

    #[test]
    fn focus_dir_up() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::HSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        assert_eq!(node.focus_dir(&b, Dir::Up), Some(a));
    }

    #[test]
    fn rect_for_finds_pane() {
        let a = PaneId::new();
        let b = PaneId::new();
        let node = LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        let ra = node.rect_for(vp(), &a).expect("should find a");
        let rb = node.rect_for(vp(), &b).expect("should find b");
        assert!(ra.x < rb.x, "a should be left of b");
    }

    #[test]
    fn three_way_split() {
        let a = PaneId::new();
        let b = PaneId::new();
        let c = PaneId::new();
        let node = LayoutNode::HSplit(vec![
            (LayoutNode::Leaf(a), 34),
            (LayoutNode::Leaf(b), 33),
            (LayoutNode::Leaf(c), 33),
        ]);
        let leaves = node.leaves(vp());
        assert_eq!(leaves.len(), 3);
    }

    // ── New tests added for M7-B ───────────────────────────────────────────────

    #[test]
    fn set_weight_updates_target_and_rebalances_sibling() {
        let a = PaneId::new();
        let b = PaneId::new();
        let mut node =
            LayoutNode::VSplit(vec![(LayoutNode::Leaf(a), 50), (LayoutNode::Leaf(b), 50)]);
        node.set_weight(&a, 70);
        let leaves = node.leaves(vp());
        // a gets ~700px wide (70%), b gets ~300px (30%).
        assert!(
            leaves[0].1.w >= 650,
            "a width should be near 700, got {}",
            leaves[0].1.w
        );
        assert!(
            leaves[1].1.w <= 350,
            "b width should be near 300, got {}",
            leaves[1].1.w
        );
    }

    #[test]
    fn serde_roundtrip() {
        let a = PaneId::new();
        let b = PaneId::new();
        let c = PaneId::new();
        let original = LayoutNode::VSplit(vec![
            (LayoutNode::Leaf(a), 40),
            (
                LayoutNode::HSplit(vec![(LayoutNode::Leaf(b), 60), (LayoutNode::Leaf(c), 40)]),
                60,
            ),
        ]);
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: LayoutNode = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, restored);
    }
}
