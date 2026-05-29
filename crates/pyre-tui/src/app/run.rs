//! Main TUI event loop — extracted from main.rs (Wave 1F).
//!
//! `initial_app_state` builds an AppState from a single attached session/pane.
//! `run_tui` is the async entry point called by `main` after CLI parsing.

use std::collections::HashMap;
use std::io::stdout;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use crossterm::event::Event;
use pyre_proto::{
    blocks::{BlockHit, SearchBlocksReq},
    layout::LayoutNode,
    Block, PaneId, PyreDaemonClient, SessionId, SpawnReq, SpawnResp,
};
use pyre_themes::{Registry, Theme};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Terminal;

use crate::app::pane_ops::{
    apply_focus_request, attach_pane, close_pane_by_slot_idx, compute_pane_inner_size,
    split_active, term_size,
};
use crate::app::sessions::SessionView;
use crate::app::state::{AppState, PendingMenuAction};
use crate::fire_motion::AnimClock;
use crate::input::keyboard::{handle_key, KeyAction};
use crate::input::mouse::handle_mouse;
use crate::model::context_menu::{MenuItem, MENU_ITEMS};
use crate::model::layout::{focused_slot_idx, pane_leaves_in_order, pane_to_slot_idx};
use crate::model::pane::{PaneEvent, PaneSlot};
use crate::model::prompt::{NamePrompt, PromptKind};
use crate::model::tab::Tab;
use crate::model::toast::{Toast, ToastDeck};
use crate::render::frame::draw_frame;
use crate::render::overlay::pager::PagerState;
use crate::render::overlay::search::{parse_search_input, SearchState};
use crate::render::sidebar::session_name_for;
use crate::rpc::events::{spawn_block_poll_task, spawn_push_event_task};
use crate::rpc::TermGuard;
use crate::PaneInit;

// ─────────────────────────────────────────────────────────────────────────────
// Initial state builder
// ─────────────────────────────────────────────────────────────────────────────

/// Build an `AppState` from one already-attached initial session/pane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn initial_app_state(
    session: SessionId,
    session_name: String,
    initial_slot: PaneSlot,
    control: PyreDaemonClient,
    socket: PathBuf,
    shell: Option<String>,
    blocks_rx: tokio::sync::watch::Receiver<HashMap<PaneId, Vec<Block>>>,
    theme: Theme,
    toast_deck: ToastDeck,
    toast_rx: tokio::sync::mpsc::Receiver<Toast>,
) -> AppState {
    let initial_pane_id = initial_slot.pane_id;
    AppState {
        sessions: vec![SessionView::new_single_pane(
            session,
            session_name,
            initial_pane_id,
        )],
        active_session: 0,
        slots: vec![Some(initial_slot)],
        control,
        socket,
        shell,
        search: SearchState::default(),
        status_msg: None,
        sidebar_open: false,
        sidebar_data: Vec::new(),
        sidebar_last_poll: Instant::now() - Duration::from_secs(10),
        sidebar_cursor: 0,
        sidebar_focused: false,
        selection: None,
        last_click: None,
        context_menu: None,
        pid_inspect: None,
        prompt: None,
        session_strip_rects: Vec::new(),
        session_strip_scroll: 0,
        session_strip_left_arrow: None,
        session_strip_right_arrow: None,
        session_plus_rect: None,
        tab_plus_rect: None,
        pending_resizes: Vec::new(),
        tab_chip_rects: Vec::new(),
        dragging_tab: None,
        pager_rect: None,
        session_list_last_poll: Instant::now() - Duration::from_secs(10),
        layout_resync_last_poll: Instant::now() - Duration::from_secs(10),
        blocks_rx,
        anim: AnimClock::new(),
        pager: None,
        theme,
        theme_picker: None,
        toast_deck,
        toast_rx,
        pending_menu_action: None,
        session_lost: false,
        last_split_at: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main TUI loop
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn run_tui(
    socket: PathBuf,
    init: PaneInit,
    control: PyreDaemonClient,
    shell: Option<String>,
) -> Result<()> {
    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let term_rect = terminal.size()?;
    let (init_cols, init_rows) = compute_pane_inner_size(term_rect.width, term_rect.height);

    let (session, session_name, pane) = match init {
        PaneInit::Existing {
            session,
            session_name,
            pane,
        } => (session, session_name, pane),
        PaneInit::Spawn => {
            let req = SpawnReq {
                shell: shell.clone(),
                cwd: std::env::current_dir().ok(),
                cols: init_cols,
                rows: init_rows,
                env: std::env::vars().collect(),
                name: None,
            };
            let SpawnResp { session, pane } = control
                .spawn(tarpc::context::current(), req)
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon spawn: {e}"))?;
            let short8: String = session.0.to_string().chars().take(8).collect();
            (session, format!("session-{short8}"), pane)
        }
    };

    let mut initial_slot = attach_pane(&socket, session, pane, init_cols, init_rows).await?;

    // Pre-populate the block ribbon for the initial pane so that Ctrl-Space [
    // shows previous command history immediately on reattach (S3).
    match tokio::time::timeout(
        Duration::from_secs(2),
        control.replay(tarpc::context::current(), pane, 20),
    )
    .await
    {
        Ok(Ok(Ok(replay))) => {
            if !replay.recent.is_empty() {
                tracing::debug!(
                    pane_id = %pane.0,
                    blocks = replay.recent.len(),
                    "reattach: pre-populated block ribbon"
                );
                initial_slot.recent_blocks = replay.recent;
            }
        }
        Ok(Ok(Err(e))) => tracing::debug!(pane_id = %pane.0, "replay rpc error (non-fatal): {e}"),
        Ok(Err(_)) => tracing::debug!(pane_id = %pane.0, "replay transport error (non-fatal)"),
        Err(_) => tracing::debug!(pane_id = %pane.0, "replay rpc timeout (non-fatal)"),
    }

    let blocks_rx = spawn_block_poll_task(control.clone());

    let theme = {
        let reg = Registry::builtin();
        let name = pyre_themes::config::load_theme_name()
            .unwrap_or(None)
            .unwrap_or_else(|| Registry::default_theme().to_owned());
        reg.get(&name)
            .or_else(|| reg.get(Registry::default_theme()))
            .expect("ember always present")
            .clone()
    };

    let notif_cfg = pyre_themes::config::load_notifications_config().unwrap_or_default();
    let toast_rx = spawn_push_event_task(socket.clone(), Duration::from_millis(notif_cfg.ttl_ms));
    let toast_deck = ToastDeck::new(notif_cfg.enabled, notif_cfg.ttl_ms, notif_cfg.max_visible);

    let mut state = initial_app_state(
        session,
        session_name,
        initial_slot,
        control,
        socket,
        shell,
        blocks_rx,
        theme,
        toast_deck,
        toast_rx,
    );

    // Eagerly discover all other sessions the daemon already knows about.
    if let Ok(Ok(daemon_sessions)) = state.control.list_sessions(tarpc::context::current()).await {
        let eager_rect = terminal.size().unwrap_or(term_rect);
        let (ec, er) = compute_pane_inner_size(eager_rect.width, eager_rect.height);
        for info in daemon_sessions {
            if info.id == session {
                continue;
            }
            if let Ok(Ok(panes)) = state
                .control
                .list_panes(tarpc::context::current(), info.id)
                .await
            {
                if let Some(p) = panes.into_iter().next() {
                    if let Ok(mut slot) = attach_pane(&state.socket, info.id, p.id, ec, er).await {
                        if let Ok(Ok(Ok(replay))) = tokio::time::timeout(
                            Duration::from_secs(2),
                            state.control.replay(tarpc::context::current(), p.id, 20),
                        )
                        .await
                        {
                            if !replay.recent.is_empty() {
                                slot.recent_blocks = replay.recent;
                            }
                        }
                        let eager_pane_id = p.id;
                        state.slots.push(Some(slot));
                        state.sessions.push(SessionView::new_single_pane(
                            info.id,
                            info.name,
                            eager_pane_id,
                        ));
                    }
                }
            }
        }
        state.session_list_last_poll = Instant::now();
    }

    let mut prefix_active = false;

    let mut loop_frames_drawn: u64 = 0;
    let mut loop_bytes_processed: u64 = 0;
    let mut loop_stats_at = Instant::now();

    loop {
        // Drain pane output into parsers and scrollback buffers.
        let mut closed_slots: Vec<(usize, u64)> = Vec::new();
        for (slot_idx, slot_opt) in state.slots.iter_mut().enumerate() {
            if let Some(slot) = slot_opt {
                while let Ok(event) = slot.output_rx.try_recv() {
                    match event {
                        PaneEvent::Output(data) => {
                            slot.frames_received += 1;
                            loop_bytes_processed += data.len() as u64;
                            slot.process_output(&data);
                            let responses = slot.drain_pty_responses();
                            if !responses.is_empty() {
                                let _ = slot.input_tx.try_send(Bytes::from(responses));
                            }
                        }
                        PaneEvent::Closed { frames_received } => {
                            closed_slots.push((slot_idx, frames_received));
                            break;
                        }
                    }
                }
            }
        }
        for (slot_idx, frames_received) in closed_slots {
            if frames_received == 0 {
                tracing::warn!(
                    slot_idx,
                    "stream closed with 0 frames; skipping close_pane RPC"
                );
                if slot_idx < state.slots.len() {
                    state.slots[slot_idx] = None;
                }
            } else {
                close_pane_by_slot_idx(&mut state, slot_idx);
            }
        }
        // Guard: closing the last pane via PTY stream-close empties sessions on
        // this same iteration.  The 1s-gated session-list block (which has its
        // own `sessions.is_empty()` check) does NOT run every tick, so without
        // this guard the code below that indexes `state.sessions[active_session]`
        // would panic with an index-out-of-bounds on the very iteration where
        // the last shell exited.
        if state.sessions.is_empty() {
            break;
        }

        // Drain latest block snapshot (non-blocking).
        if state.blocks_rx.has_changed().unwrap_or(false) {
            let map = state.blocks_rx.borrow_and_update().clone();
            for slot in state.slots.iter_mut().flatten() {
                let pane_id = slot.pane_id;
                if let Some(blocks) = map.get(&pane_id) {
                    slot.recent_blocks = blocks.clone();
                    if let Some(cursor) = slot.ribbon_cursor {
                        if !slot.recent_blocks.is_empty() {
                            slot.ribbon_cursor = Some(cursor.min(slot.recent_blocks.len() - 1));
                        } else {
                            slot.ribbon_cursor = None;
                        }
                    }
                } else {
                    slot.recent_blocks.clear();
                    slot.ribbon_cursor = None;
                }
            }
        }

        // Drain toasts (non-blocking).
        state.toast_deck.tick();
        while let Ok(toast) = state.toast_rx.try_recv() {
            state.toast_deck.push(toast.title, toast.body, toast.kind);
        }

        // Search debounce: fire query 150 ms after last keystroke.
        if state.search.open
            && state.search.pending_query.is_some()
            && state.search.last_query_at.elapsed() >= Duration::from_millis(150)
        {
            let raw = state.search.pending_query.take().expect("checked Some");
            let (query, failures_only) = parse_search_input(&raw);
            state.search.failures_only = failures_only;
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<BlockHit>>(1);
            state.search.rx = Some(rx);
            let client = state.control.clone();
            let req = SearchBlocksReq {
                query,
                limit: 20,
                failures_only,
            };
            tokio::spawn(async move {
                let hits = client
                    .search_blocks(tarpc::context::current(), req)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let _ = tx.send(hits).await;
            });
        }

        // Drain search results.
        if let Some(ref mut rx) = state.search.rx {
            if let Ok(hits) = rx.try_recv() {
                state.search.results = hits;
                state.search.cursor = 0;
            }
        }

        apply_focus_request(&mut state).await;

        // Pane-meta poll — 1 s.
        if state.sidebar_last_poll.elapsed() >= Duration::from_secs(1) {
            state.sidebar_last_poll = Instant::now();
            if let Ok(Ok(mut panes)) = state
                .control
                .list_all_panes(tarpc::context::current())
                .await
            {
                panes.truncate(50);
                panes.sort_by(|a, b| {
                    session_name_for(&state, a.session)
                        .cmp(&session_name_for(&state, b.session))
                        .then_with(|| a.id.0.cmp(&b.id.0))
                });
                state.sidebar_data = panes;
                state.sidebar_cursor = state
                    .sidebar_cursor
                    .min(state.sidebar_data.len().saturating_sub(1));
            }
        }

        // Session-list sync — 1 s.
        if state.session_list_last_poll.elapsed() >= Duration::from_secs(1) {
            state.session_list_last_poll = Instant::now();
            if let Ok(Ok(daemon_sessions)) =
                state.control.list_sessions(tarpc::context::current()).await
            {
                let prev_active_id = state.sessions.get(state.active_session).map(|sv| sv.id);
                let known_ids: Vec<SessionId> = state.sessions.iter().map(|s| s.id).collect();
                for info in &daemon_sessions {
                    if !known_ids.contains(&info.id) {
                        match state
                            .control
                            .list_panes(tarpc::context::current(), info.id)
                            .await
                        {
                            Ok(Ok(panes)) if !panes.is_empty() => {
                                let pane_id = panes[0].id;
                                let (sc, sr) = {
                                    let (tc, tr) = term_size();
                                    compute_pane_inner_size(tc, tr)
                                };
                                match attach_pane(&state.socket, info.id, pane_id, sc, sr).await {
                                    Ok(slot) => {
                                        state.slots.push(Some(slot));
                                        state.sessions.push(SessionView::new_single_pane(
                                            info.id,
                                            info.name.clone(),
                                            pane_id,
                                        ));
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "session-sync: attach_pane for session {} failed: {e}",
                                            info.id
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Sync panes within existing sessions.
                for info in &daemon_sessions {
                    let sv_idx = match state.sessions.iter().position(|s| s.id == info.id) {
                        Some(i) => i,
                        None => continue,
                    };

                    let local_pane_ids: Vec<PaneId> = {
                        let sv = &state.sessions[sv_idx];
                        sv.tabs
                            .iter()
                            .flat_map(|tab| pane_leaves_in_order(&tab.root))
                            .collect()
                    };

                    let daemon_panes = match state
                        .control
                        .list_panes(tarpc::context::current(), info.id)
                        .await
                    {
                        Ok(Ok(p)) => p,
                        _ => continue,
                    };

                    let new_panes: Vec<_> = daemon_panes
                        .iter()
                        .filter(|p| !local_pane_ids.contains(&p.id))
                        .collect();

                    if !new_panes.is_empty() {
                        let fresh_layout =
                            crate::rpc::layout::get_session_layout(&state.control, info.id).await;

                        for pane_info in &new_panes {
                            // Guard against double-attach: `split_active` may have already
                            // attached a slot for this pane id before the 1s poll fires.
                            // Attaching twice creates two competing output streams → flicker.
                            if pane_to_slot_idx(&state.slots, pane_info.id).is_some() {
                                tracing::debug!(
                                    "pane-sync: slot for pane {} already exists, skipping attach",
                                    pane_info.id,
                                );
                                continue;
                            }
                            let (pc, pr) = {
                                let (tc, tr) = term_size();
                                compute_pane_inner_size(tc, tr)
                            };
                            match attach_pane(&state.socket, info.id, pane_info.id, pc, pr).await {
                                Ok(slot) => {
                                    state.slots.push(Some(slot));
                                    tracing::info!(
                                        "pane-sync: attached slot for pane {} in session {}",
                                        pane_info.id,
                                        info.id,
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "pane-sync: attach_pane for pane {} in session {} \
                                         failed: {e}",
                                        pane_info.id,
                                        info.id,
                                    );
                                }
                            }
                        }

                        let sv = &mut state.sessions[sv_idx];
                        if let Some(layout) = fresh_layout {
                            let at = sv.active_tab;
                            let old_focus = sv.tabs[at].focus_pane;
                            let new_leaves = pane_leaves_in_order(&layout);
                            let focus = if new_leaves.contains(&old_focus) {
                                old_focus
                            } else {
                                new_leaves.into_iter().next().unwrap_or(old_focus)
                            };
                            sv.tabs[at].root = layout;
                            sv.tabs[at].focus_pane = focus;
                            tracing::info!(
                                "pane-sync: applied daemon layout to active tab of session {}",
                                info.id,
                            );
                        } else {
                            for pane_info in &new_panes {
                                if pane_to_slot_idx(&state.slots, pane_info.id).is_some() {
                                    let tab_n = sv.tabs.len() + 1;
                                    sv.tabs.push(Tab {
                                        root: LayoutNode::Leaf(pane_info.id),
                                        focus_pane: pane_info.id,
                                        zoomed: None,
                                        boundaries: Vec::new(),
                                        drag: None,
                                    });
                                    tracing::warn!(
                                        "pane-sync: fallback — new pane {} added as tab-{} \
                                         (get_session_layout failed)",
                                        pane_info.id,
                                        tab_n,
                                    );
                                }
                            }
                        }
                    }

                    // Prune panes the daemon no longer reports.
                    let daemon_ids_for_session: Vec<PaneId> =
                        daemon_panes.iter().map(|p| p.id).collect();
                    let slots_to_drop: Vec<usize> = {
                        let sv = &state.sessions[sv_idx];
                        let mut to_drop = Vec::new();
                        for tab in &sv.tabs {
                            for pid in pane_leaves_in_order(&tab.root) {
                                if !daemon_ids_for_session.contains(&pid) {
                                    if let Some(idx) = pane_to_slot_idx(&state.slots, pid) {
                                        to_drop.push(idx);
                                    }
                                }
                            }
                        }
                        to_drop
                    };
                    for slot_idx in slots_to_drop {
                        state.slots[slot_idx] = None;
                    }
                }

                // Prune sessions that disappeared from the daemon.
                let daemon_ids: Vec<SessionId> = daemon_sessions.iter().map(|s| s.id).collect();
                let to_remove: Vec<usize> = state
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, sv)| !daemon_ids.contains(&sv.id))
                    .map(|(i, _)| i)
                    .collect();
                for &idx in to_remove.iter().rev() {
                    state.sessions.remove(idx);
                }

                crate::app::active::restore_active_session(
                    &state.sessions,
                    &mut state.active_session,
                    prev_active_id,
                );
            }
        }

        if state.sessions.is_empty() {
            break;
        }

        // Periodic layout resync — 5 s safety net.
        if state.layout_resync_last_poll.elapsed() >= Duration::from_secs(5) {
            state.layout_resync_last_poll = Instant::now();
            let active_session_id = state.sessions[state.active_session].id;
            if let Some(fresh_layout) =
                crate::rpc::layout::get_session_layout(&state.control, active_session_id).await
            {
                let si = state.active_session;
                let at = state.sessions[si].active_tab;
                let daemon_leaves = pane_leaves_in_order(&fresh_layout);
                let mut new_ids: Vec<PaneId> = Vec::new();
                for &pid in &daemon_leaves {
                    if pane_to_slot_idx(&state.slots, pid).is_none() {
                        new_ids.push(pid);
                    }
                }
                for pid in new_ids {
                    let (pc, pr) = {
                        let (tc, tr) = term_size();
                        compute_pane_inner_size(tc, tr)
                    };
                    match attach_pane(&state.socket, active_session_id, pid, pc, pr).await {
                        Ok(slot) => {
                            state.slots.push(Some(slot));
                            tracing::info!(
                                "layout-resync: attached missing slot for pane {} in session {}",
                                pid,
                                active_session_id,
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "layout-resync: attach_pane for pane {} failed: {e}",
                                pid,
                            );
                        }
                    }
                }
                let old_leaves = pane_leaves_in_order(&state.sessions[si].tabs[at].root);
                let new_leaves = pane_leaves_in_order(&fresh_layout);
                let recently_split = state
                    .last_split_at
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(false);
                if old_leaves != new_leaves && !recently_split {
                    let old_focus = state.sessions[si].tabs[at].focus_pane;
                    // Only reassign focus when the current focus is no longer a valid leaf.
                    // This preserves user focus when it is still present in the new layout.
                    let focus = if new_leaves.contains(&old_focus) {
                        old_focus
                    } else {
                        new_leaves.into_iter().next().unwrap_or(old_focus)
                    };
                    state.sessions[si].tabs[at].root = fresh_layout;
                    state.sessions[si].tabs[at].focus_pane = focus;
                    tracing::info!(
                        "layout-resync: updated tab layout for session {}",
                        active_session_id,
                    );
                }
            }
        }

        // Session-lost detection.
        //
        // The active tab is "lost" when every pane in it has a dead (None) slot.
        // Recovery order:
        //   1. Another live tab within the SAME session (switch tab).
        //   2. Another live session entirely (switch session).
        //   3. No live panes anywhere → show the session-lost overlay.
        {
            let si = state.active_session;
            let ti = state.sessions[si].active_tab;
            let all_dead = pane_leaves_in_order(&state.sessions[si].tabs[ti].root)
                .iter()
                .all(|pid| pane_to_slot_idx(&state.slots, *pid).is_none());

            if all_dead && !state.session_lost {
                // Try another tab in the same session first.
                let alt_tab =
                    state.sessions[si]
                        .tabs
                        .iter()
                        .enumerate()
                        .find(|&(other_ti, tab)| {
                            other_ti != ti
                                && pane_leaves_in_order(&tab.root)
                                    .iter()
                                    .any(|pid| pane_to_slot_idx(&state.slots, *pid).is_some())
                        });
                if let Some((next_ti, _)) = alt_tab {
                    state.sessions[si].active_tab = next_ti;
                    state.session_lost = false;
                } else {
                    // No live tab in this session — try another session.
                    let alt_session = (0..state.sessions.len()).find(|&other_si| {
                        if other_si == si {
                            return false;
                        }
                        let other_ti = state.sessions[other_si].active_tab;
                        pane_leaves_in_order(&state.sessions[other_si].tabs[other_ti].root)
                            .iter()
                            .any(|pid| pane_to_slot_idx(&state.slots, *pid).is_some())
                    });
                    if let Some(next_si) = alt_session {
                        state.active_session = next_si;
                        state.session_lost = false;
                    } else {
                        state.session_lost = true;
                    }
                }
            } else if !all_dead {
                state.session_lost = false;
            }
        }

        state.anim.tick();
        draw_frame(&mut terminal, &mut state, prefix_active)?;
        loop_frames_drawn += 1;

        if loop_stats_at.elapsed() >= Duration::from_secs(1) {
            tracing::debug!(
                frames_drawn = loop_frames_drawn,
                bytes_processed = loop_bytes_processed,
                "event-loop: 1s stats"
            );
            loop_frames_drawn = 0;
            loop_bytes_processed = 0;
            loop_stats_at = Instant::now();
        }

        // Drain pending resize RPCs (fire-and-forget).
        let resizes = std::mem::take(&mut state.pending_resizes);
        if !resizes.is_empty() {
            let client = state.control.clone();
            tokio::spawn(async move {
                for (pane_id, size) in resizes {
                    let req = pyre_proto::ResizePaneReq { pane_id, size };
                    let _ = client.resize_pane(tarpc::context::current(), req).await;
                }
            });
        }

        if !crossterm::event::poll(Duration::from_millis(16))? {
            continue;
        }

        let term_size_rect = terminal.size()?;
        let outer_rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(term_size_rect.into());
        let body_area = outer_rects[2];

        match crossterm::event::read()? {
            Event::Mouse(me) => {
                handle_mouse(&mut state, me, body_area);
                if let Some(action) = state.pending_menu_action.take() {
                    match action {
                        PendingMenuAction::SplitH => {
                            if let Err(e) = split_active(&mut state, true).await {
                                tracing::warn!("context menu HSplit: {e}");
                            }
                        }
                        PendingMenuAction::SplitV => {
                            if let Err(e) = split_active(&mut state, false).await {
                                tracing::warn!("context menu VSplit: {e}");
                            }
                        }
                        PendingMenuAction::RenameSession => {
                            let sv = &state.sessions[state.active_session];
                            state.prompt = Some(NamePrompt {
                                kind: PromptKind::RenameSession(sv.id),
                                input: sv.name.clone(),
                            });
                        }
                        PendingMenuAction::SearchJump(idx) => {
                            if idx < state.search.results.len() {
                                let hit = &state.search.results[idx];
                                let target_pane = hit.block.pane;
                                let target_block = hit.block.id;
                                type JumpTarget = (usize, usize, PaneId, usize);
                                let mut jump: Option<JumpTarget> = None;
                                'search_jump: for (si, sv) in state.sessions.iter().enumerate() {
                                    for (ti, tab) in sv.tabs.iter().enumerate() {
                                        for pid in pane_leaves_in_order(&tab.root) {
                                            if pid == target_pane {
                                                if let Some(slot_idx) =
                                                    pane_to_slot_idx(&state.slots, pid)
                                                {
                                                    jump = Some((si, ti, pid, slot_idx));
                                                    if si == state.active_session {
                                                        break 'search_jump;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some((si, ti, pane_id, slot_idx)) = jump {
                                    state.active_session = si;
                                    state.sessions[si].active_tab = ti;
                                    state.sessions[si].tabs[ti].focus_pane = pane_id;
                                    if let Some(c) = state.slots[slot_idx].as_ref().and_then(|s| {
                                        s.recent_blocks.iter().position(|b| b.id == target_block)
                                    }) {
                                        if let Some(slot) = state.slots[slot_idx].as_mut() {
                                            slot.ribbon_cursor = Some(c);
                                        }
                                    }
                                } else {
                                    state.status_msg =
                                        Some("search: result pane not loaded".to_owned());
                                }
                            }
                            state.search.open = false;
                            state.search.rx = None;
                        }
                        PendingMenuAction::ContextMenuActivate(item_idx) => {
                            if let Some(menu) = state.context_menu.take() {
                                let idx = item_idx.min(MENU_ITEMS.len().saturating_sub(1));
                                let item = MENU_ITEMS[idx];
                                let target = menu.target_slot;
                                match item {
                                    MenuItem::Copy => {
                                        if let Some(ref sel) = state.selection.clone() {
                                            let pane_idx = sel.pane_idx;
                                            let ((r0, c0), (r1, c1)) = sel.normalized();
                                            if let Some(slot) = state.slots[pane_idx].as_ref() {
                                                use alacritty_terminal::grid::Dimensions;
                                                use alacritty_terminal::index::{
                                                    Column as TermColumn, Line as TermLine,
                                                    Point as TermPoint,
                                                };
                                                let grid = slot.term.grid();
                                                let num_cols = grid.columns();
                                                let mut text = String::new();
                                                for gr in r0..=r1 {
                                                    if gr > r0 {
                                                        text.push('\n');
                                                    }
                                                    let cs = if gr == r0 { c0 as usize } else { 0 };
                                                    let ce = if gr == r1 {
                                                        c1 as usize
                                                    } else {
                                                        num_cols.saturating_sub(1)
                                                    };
                                                    for c in cs..=ce {
                                                        let pt = TermPoint::new(
                                                            TermLine(gr as i32),
                                                            TermColumn(c),
                                                        );
                                                        let ch = grid[pt].c;
                                                        text.push(if ch == '\0' {
                                                            ' '
                                                        } else {
                                                            ch
                                                        });
                                                    }
                                                }
                                                let trimmed: String = text
                                                    .lines()
                                                    .map(|l| l.trim_end())
                                                    .collect::<Vec<_>>()
                                                    .join("\n");
                                                if !trimmed.is_empty() {
                                                    let _ = crate::clipboard::copy_to_clipboard(
                                                        &trimmed,
                                                    );
                                                    state.status_msg = Some("copied".to_owned());
                                                }
                                            }
                                        }
                                    }
                                    MenuItem::KillPane => {
                                        close_pane_by_slot_idx(&mut state, target);
                                        if state.sessions.is_empty() {
                                            break;
                                        }
                                    }
                                    MenuItem::SplitH => {
                                        if let Err(e) = split_active(&mut state, true).await {
                                            tracing::warn!("context menu mouse HSplit: {e}");
                                        }
                                    }
                                    MenuItem::SplitV => {
                                        if let Err(e) = split_active(&mut state, false).await {
                                            tracing::warn!("context menu mouse VSplit: {e}");
                                        }
                                    }
                                    MenuItem::ZoomToggle => {
                                        let sv = state.active_session_view_mut();
                                        let tab = &mut sv.tabs[sv.active_tab];
                                        if tab.zoomed.is_some() {
                                            tab.zoomed = None;
                                        } else {
                                            tab.zoomed = Some(tab.focus_pane);
                                        }
                                    }
                                    MenuItem::InspectPid => {
                                        if let Some(slot) = state.slots[target].as_ref() {
                                            let pane_id = slot.pane_id;
                                            match state
                                                .control
                                                .inspect_pid(tarpc::context::current(), pane_id)
                                                .await
                                            {
                                                Ok(Ok(info)) => {
                                                    state.pid_inspect = Some(info);
                                                }
                                                Ok(Err(e)) => {
                                                    state.status_msg =
                                                        Some(format!("inspect_pid: {e}"));
                                                }
                                                Err(e) => {
                                                    state.status_msg =
                                                        Some(format!("rpc transport: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        PendingMenuAction::ContextMenuCommit => {
                            let Some(sv) = state.sessions.get(state.active_session) else {
                                continue;
                            };
                            let tab = &sv.tabs[sv.active_tab];
                            if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                                if let Some(slot) = state.slots[slot_idx].as_ref() {
                                    if let Some(cursor) = slot.ribbon_cursor {
                                        if let Some(block) = slot.recent_blocks.get(cursor) {
                                            let block_id_str = block.id.0.to_string();
                                            let exit_code = block.exit_code;
                                            let block_id = block.id;
                                            match state
                                                .control
                                                .get_block_stdout(
                                                    tarpc::context::current(),
                                                    block_id,
                                                )
                                                .await
                                            {
                                                Ok(Ok(raw)) => {
                                                    state.pager = Some(PagerState::new(
                                                        block_id_str,
                                                        exit_code,
                                                        &raw,
                                                    ));
                                                }
                                                Ok(Err(e)) => {
                                                    state.status_msg =
                                                        Some(format!("get_block_stdout: {e}"));
                                                }
                                                Err(e) => {
                                                    state.status_msg =
                                                        Some(format!("rpc transport: {e}"));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if state.sessions.is_empty() {
                    break;
                }
            }

            Event::Key(key_event) => {
                let code = key_event.code;
                let mods = key_event.modifiers;
                match handle_key(&mut state, code, mods, &mut prefix_active, body_area).await {
                    KeyAction::Quit => break,
                    KeyAction::Continue => {}
                }
            }

            Event::Paste(s) => {
                let mut buf = Vec::with_capacity(s.len() + 12);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(s.as_bytes());
                buf.extend_from_slice(b"\x1b[201~");
                let bytes = bytes::Bytes::from(buf);
                let sv = &state.sessions[state.active_session];
                let tab = &sv.tabs[sv.active_tab];
                if let Some(slot_idx) = focused_slot_idx(tab.focus_pane, &state.slots) {
                    if let Some(slot) = state.slots[slot_idx].as_mut() {
                        slot.scroll_offset = 0;
                        let send_result = slot.input_tx.send(bytes.clone()).await;
                        tracing::debug!(
                            slot_idx,
                            paste_bytes = bytes.len(),
                            send_ok = send_result.is_ok(),
                            "send_keys: bracketed paste input_tx.send"
                        );
                    }
                }
            }

            Event::Resize(new_cols, new_rows) => {
                terminal.clear()?;
                tracing::debug!("terminal resized to {new_cols}x{new_rows}");
            }

            _ => {}
        }
    }

    Ok(())
}
