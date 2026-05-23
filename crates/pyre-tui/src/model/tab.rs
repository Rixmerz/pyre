use pyre_proto::{layout::LayoutNode, PaneId};

use crate::model::pane::SplitBoundary;

/// One tab, owning a layout tree and a cursor into the focused leaf.
pub struct Tab {
    /// Daemon-owned tiling tree. Leaves carry stable `PaneId` UUIDs (M7-D).
    pub root: LayoutNode,
    /// The `PaneId` of the focused pane. Stable across layout mutations.
    pub focus_pane: PaneId,
    /// When Some, renders only this pane filling the full body area (zoom).
    pub zoomed: Option<PaneId>,
    /// Boundaries collected during the last render, used for drag-resize hit-test.
    pub boundaries: Vec<SplitBoundary>,
    /// Active drag state (set on mouse-down near a boundary).
    pub drag: Option<crate::model::pane::DragState>,
}

/// Reorder tabs: move tab at `from` to position `to` (0-based), shifting others.
/// Returns the new vec. No-op if indices are equal or out of range.
pub fn tab_reorder(mut tabs: Vec<Tab>, from: usize, to: usize) -> Vec<Tab> {
    if from == to || from >= tabs.len() || to >= tabs.len() {
        return tabs;
    }
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
    tabs
}
