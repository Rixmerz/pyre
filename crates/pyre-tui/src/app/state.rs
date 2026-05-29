use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use pyre_proto::{Block, PaneId, PidInspect, PyreDaemonClient, SessionId};
use pyre_themes::Theme;
use ratatui::layout::Rect;
use tokio::sync::{mpsc, watch};

use crate::app::sessions::SessionView;
use crate::fire_motion::AnimClock;
use crate::model::context_menu::ContextMenu;
use crate::model::pane::PaneSlot;
use crate::model::prompt::NamePrompt;
use crate::model::selection::{ClickTracker, Selection};
use crate::model::toast::{Toast, ToastDeck};
use crate::render::overlay::pager::PagerState;
use crate::render::overlay::picker::ThemePickerState;
use crate::render::overlay::search::SearchState;

// ─────────────────────────────────────────────────────────────────────────────
// Deferred async actions
// ─────────────────────────────────────────────────────────────────────────────

/// Actions that require async context (RPC calls) but originate from the sync
/// `handle_mouse` function. The event loop drains this after every mouse event.
#[allow(dead_code)]
pub enum PendingMenuAction {
    /// Execute the highlighted item of the context menu.
    ContextMenuCommit,
    /// Activate a specific context menu item by index (mouse-left on item row).
    ContextMenuActivate(usize),
    /// Split active pane horizontally (HSplit).
    SplitH,
    /// Split active pane vertically (VSplit).
    SplitV,
    /// Open a rename prompt for the active session.
    RenameSession,
    /// Jump to search result at given index (mouse click on result row).
    SearchJump(usize),
}

// ─────────────────────────────────────────────────────────────────────────────
// AppState
// ─────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub struct AppState {
    /// All known sessions (may have tabs loaded lazily).
    pub sessions: Vec<SessionView>,
    /// Index into `sessions` that is currently displayed.
    pub active_session: usize,
    /// All attached pane slots (shared across all sessions). None = closed/removed.
    pub slots: Vec<Option<PaneSlot>>,
    /// Set to `true` when the active session's active tab has no live pane slots.
    /// Triggers a "Session ended" overlay and accepts q/Esc/Ctrl-C to quit.
    pub session_lost: bool,
    pub control: PyreDaemonClient,
    pub socket: PathBuf,
    pub shell: Option<String>,
    pub search: SearchState,
    /// One-line status message shown when action feedback is needed.
    pub status_msg: Option<String>,
    /// Whether the sidebar is visible.
    pub sidebar_open: bool,
    /// Cached pane info — used for sidebar display AND pane border titles.
    /// Refreshed every second regardless of sidebar visibility so that
    /// `render_pane` can resolve user-provided names even when the sidebar
    /// is closed.
    pub sidebar_data: Vec<pyre_proto::PaneInfo>,
    /// Last time sidebar data was fetched.
    pub sidebar_last_poll: Instant,
    /// Selected row index within the sidebar.
    pub sidebar_cursor: usize,
    /// Whether the sidebar panel has keyboard focus.
    pub sidebar_focused: bool,
    /// Active text selection (drag or click-to-select).
    pub selection: Option<Selection>,
    /// State for double/triple-click detection.
    pub last_click: Option<ClickTracker>,
    /// Right-click context menu state.
    pub context_menu: Option<ContextMenu>,
    /// PID inspect overlay data.
    pub pid_inspect: Option<PidInspect>,
    /// Name-prompt overlay (new session or new tab).
    pub prompt: Option<NamePrompt>,
    /// Session strip hit-test rects: (session_vec_index, rect).
    pub session_strip_rects: Vec<(usize, Rect)>,
    /// Horizontal scroll offset (in columns) for the session strip.
    pub session_strip_scroll: usize,
    /// Rect of the left-scroll indicator `◄` in the session strip (when overflow left).
    pub session_strip_left_arrow: Option<Rect>,
    /// Rect of the right-scroll indicator `►` in the session strip (when overflow right).
    pub session_strip_right_arrow: Option<Rect>,
    /// Rect of the [+] button in the session strip.
    pub session_plus_rect: Option<Rect>,
    /// Rect of the [+] button in the tabs strip.
    pub tab_plus_rect: Option<Rect>,
    /// Queued resize RPCs collected by render_pane (sync); drained after each draw.
    pub pending_resizes: Vec<(PaneId, pyre_proto::PaneSize)>,
    /// Per-tab chip rects captured during last render: vec of (tab_vec_index, chip_rect).
    pub tab_chip_rects: Vec<(usize, Rect)>,
    /// Active tab-drag: (tab_vec_index, start_col) — set on mouse-down on a chip.
    pub dragging_tab: Option<(usize, u16)>,
    /// Rect of the pager overlay as rendered last frame (for mouse-wheel routing).
    pub pager_rect: Option<Rect>,
    /// Last time the session list was refreshed from the daemon.
    pub session_list_last_poll: Instant,
    /// Last time the active session's layout was resynced from the daemon.
    /// Acts as a safety-net periodic refresh of the tab's LayoutNode tree.
    pub layout_resync_last_poll: Instant,
    /// Latest block snapshot delivered by the background poll task.
    /// Key = PaneId, value = blocks for that pane (up to 20, newest last).
    pub blocks_rx: watch::Receiver<HashMap<PaneId, Vec<Block>>>,
    /// In-TUI ember motion (shared curves with startup splash).
    pub anim: AnimClock,
    /// Block stdout modal pager (Some = open, None = closed).
    pub pager: Option<PagerState>,
    /// Active theme (loaded from config on startup, switchable at runtime).
    pub theme: Theme,
    /// Theme picker overlay (Some = open, None = closed).
    pub theme_picker: Option<ThemePickerState>,
    /// Ephemeral toast notifications (pane state changes).
    pub toast_deck: ToastDeck,
    /// Receiver for toasts produced by the background push-event task.
    pub toast_rx: mpsc::Receiver<Toast>,
    /// Deferred async action queued by the (sync) mouse handler and drained in the event loop.
    pub pending_menu_action: Option<PendingMenuAction>,
    /// Timestamp of the most recent `split_active` call.
    /// Used by the 5s layout-resync to skip clobbering focus immediately after a split.
    pub last_split_at: Option<Instant>,
}

impl AppState {
    /// Convenience: active session's session id.
    ///
    /// # Panics
    /// Panics if `sessions` is empty or `active_session` is out of bounds.
    /// Callers must check `sessions.is_empty()` before calling this.
    pub fn active_session_id(&self) -> SessionId {
        self.sessions
            .get(self.active_session)
            .expect(
                "active_session out of bounds — caller must check sessions.is_empty() before this",
            )
            .id
    }

    /// Convenience: active session view (mutable).
    ///
    /// # Panics
    /// Panics if `sessions` is empty or `active_session` is out of bounds.
    /// Callers must check `sessions.is_empty()` before calling this.
    pub fn active_session_view_mut(&mut self) -> &mut SessionView {
        self.sessions
            .get_mut(self.active_session)
            .expect(
                "active_session out of bounds — caller must check sessions.is_empty() before this",
            )
    }
}
