//! Tiling layout model for pyre-gpu (S6.2-1).
//!
//! `LayoutNode` mirrors the TUI's `LayoutNode` (pyre-tui/src/main.rs:583-588)
//! but uses `PaneId` as leaf keys rather than a `usize` index, so panes can
//! be addressed by their daemon-assigned UUID throughout their lifetime.
//!
//! Weights are `u16` percentages; siblings must sum to 100. When a new split
//! is created equal weights are assigned automatically.

use pyre_proto::PaneId;

/// Cardinal direction for focus traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Down,
    Up,
    Right,
}

/// Split orientation for new panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orient {
    /// Side-by-side (new pane appears to the right).
    Vertical,
    /// Stacked (new pane appears below).
    Horizontal,
}

/// A pixel-space rectangle inside the framebuffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    #[allow(dead_code)] // useful utility for future drag-resize hit-testing
    pub fn contains_pt(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// Recursive tiling tree.
///
/// `HSplit` children are stacked **top-to-bottom** (split on the horizontal
/// axis). `VSplit` children are placed **side-by-side** (split on the vertical
/// axis). This matches the TUI convention.
#[derive(Clone, Debug)]
pub enum LayoutNode {
    Leaf(PaneId),
    /// Top-to-bottom stack. Weights sum to 100.
    HSplit(Vec<(LayoutNode, u16)>),
    /// Side-by-side columns. Weights sum to 100.
    VSplit(Vec<(LayoutNode, u16)>),
}

impl LayoutNode {
    // ─── Public API ───────────────────────────────────────────────────────────

    /// Compute the pixel `Rect` of `target` within `viewport`.
    pub fn rect_for(&self, viewport: Rect, target: &PaneId) -> Option<Rect> {
        self.leaves_inner(viewport)
            .into_iter()
            .find(|(id, _)| id == target)
            .map(|(_, r)| r)
    }

    /// Return all (PaneId, Rect) pairs for the current viewport, in DFS order.
    pub fn leaves(&self, viewport: Rect) -> Vec<(PaneId, Rect)> {
        self.leaves_inner(viewport)
    }

    /// Insert `new_pane` adjacent to `focused` with the given orientation.
    /// The focused pane's parent is replaced by a two-child split; existing
    /// children keep their weight halved (equal split 50/50 on the new axis).
    pub fn split_focused(&mut self, focused: &PaneId, new_pane: PaneId, orient: Orient) {
        self.split_focused_inner(focused, new_pane, orient);
    }

    /// Remove `pane` from the tree. Collapses single-child splits.
    /// Returns the `PaneId` that should receive focus after removal, if any.
    pub fn close(&mut self, pane: &PaneId) -> Option<PaneId> {
        // Collect all remaining panes in order before mutation.
        let all = self.all_leaves();
        let pos = all.iter().position(|id| id == pane)?;

        self.remove_leaf(pane);

        // After removal, pick a neighbour to focus.
        let remaining = self.all_leaves();
        if remaining.is_empty() {
            return None;
        }
        // Try to focus the pane that was right before the removed one.
        let focus_pos = if pos > 0 { pos - 1 } else { 0 };
        remaining.into_iter().nth(focus_pos)
    }

    /// Return the `PaneId` in direction `dir` from `current`, or `None` if
    /// there is no neighbour in that direction.
    ///
    /// Strategy: compute rects for a unit viewport, find current rect, then
    /// pick the leaf whose center is nearest to the edge of `current` in the
    /// requested direction.
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

        // For each other leaf, check whether it is a valid neighbour in dir.
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

        // Pick the candidate with the smallest gap distance from `cur_rect`.
        let cur_cx = cur_rect.x + cur_rect.w / 2;
        let cur_cy = cur_rect.y + cur_rect.h / 2;

        candidates
            .into_iter()
            .min_by_key(|(_, r)| {
                let cx = r.x + r.w / 2;
                let cy = r.y + r.h / 2;
                // Manhattan distance between centres.
                (cx as i64 - cur_cx as i64).unsigned_abs()
                    + (cy as i64 - cur_cy as i64).unsigned_abs()
            })
            .map(|(id, _)| id)
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    fn all_leaves(&self) -> Vec<PaneId> {
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
                let total_weight: u16 = children.iter().map(|(_, w)| w).sum();
                let total_weight = total_weight.max(1);
                let mut y = vp.y;
                for (node, weight) in children {
                    let h = (vp.h as u64 * *weight as u64 / total_weight as u64) as u32;
                    let child_vp = Rect {
                        x: vp.x,
                        y,
                        w: vp.w,
                        h,
                    };
                    out.extend(node.leaves_inner(child_vp));
                    y += h;
                }
                out
            }
            LayoutNode::VSplit(children) => {
                let mut out = Vec::new();
                let total_weight: u16 = children.iter().map(|(_, w)| w).sum();
                let total_weight = total_weight.max(1);
                let mut x = vp.x;
                for (node, weight) in children {
                    let w = (vp.w as u64 * *weight as u64 / total_weight as u64) as u32;
                    let child_vp = Rect {
                        x,
                        y: vp.y,
                        w,
                        h: vp.h,
                    };
                    out.extend(node.leaves_inner(child_vp));
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
                    // Replace this leaf with a two-child split.
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
    /// Returns `true` if the caller's slot should be removed.
    fn remove_leaf(&mut self, pane: &PaneId) -> bool {
        match self {
            LayoutNode::Leaf(id) => id == pane,
            LayoutNode::HSplit(children) | LayoutNode::VSplit(children) => {
                children.retain_mut(|(child, _)| !child.remove_leaf(pane));
                // Collapse a single-child split by replacing self with the child.
                if children.len() == 1 {
                    let (only_child, _) = children.drain(..).next().expect("just checked len==1");
                    *self = only_child;
                }
                false
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pyre_proto::PaneId;

    fn vp() -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 1000,
        }
    }

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
        // Left pane starts at x=0, right pane at x=500.
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
}
