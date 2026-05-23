//! Context-menu model types — extracted from main.rs (Wave 1F).

use ratatui::layout::Rect;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuItem {
    Copy,
    KillPane,
    SplitH,
    SplitV,
    ZoomToggle,
    InspectPid,
}

impl MenuItem {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Copy => " Copy selection",
            Self::KillPane => " Kill pane",
            Self::SplitH => " Split horizontal",
            Self::SplitV => " Split vertical",
            Self::ZoomToggle => " Zoom toggle",
            Self::InspectPid => " Inspect PID",
        }
    }
}

pub(crate) const MENU_ITEMS: &[MenuItem] = &[
    MenuItem::Copy,
    MenuItem::KillPane,
    MenuItem::SplitH,
    MenuItem::SplitV,
    MenuItem::ZoomToggle,
    MenuItem::InspectPid,
];

pub(crate) struct ContextMenu {
    pub(crate) rect: Rect,
    pub(crate) cursor: usize,
    pub(crate) target_slot: usize,
    pub(crate) item_rects: Vec<Rect>,
}
