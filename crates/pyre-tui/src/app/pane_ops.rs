//! Pane operations — extracted from main.rs (Wave 1F).
//!
//! These async functions orchestrate RPC calls and local state mutations for
//! the common pane lifecycle: attach, split, open, close, focus.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::SinkExt;
use futures::StreamExt;
use pyre_proto::{
    layout::{LayoutNode, Orient},
    InputFrame, OpenPaneReq, OpenPaneSplitReq, OutputFrame, PaneId, SessionId, SpawnReq, SpawnResp,
    MODE_STREAM,
};
use ratatui::layout::Rect;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
use alacritty_terminal::Term;

use crate::app::sessions::SessionView;
use crate::app::state::AppState;
use crate::model::layout::{focused_slot_idx, pane_leaves_in_order};
use crate::model::pane::{EventProxy, PaneEvent, PaneSlot};
use crate::model::tab::Tab;
use crate::render::pane::TermSize;

// ─────────────────────────────────────────────────────────────────────────────
// Terminal size helpers (local to this module)
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

pub(crate) fn compute_pane_inner_size(term_cols: u16, term_rows: u16) -> (u16, u16) {
    let cols = term_cols.saturating_sub(2).max(1);
    let rows = term_rows.saturating_sub(6).max(1);
    (cols, rows)
}

// ─────────────────────────────────────────────────────────────────────────────
// Stream connection + background tasks for one pane
// ─────────────────────────────────────────────────────────────────────────────

/// Attach to an existing pane's I/O stream and return an initialised `PaneSlot`.
pub(crate) async fn attach_pane(
    socket: &Path,
    session: SessionId,
    pane_id: PaneId,
    cols: u16,
    rows: u16,
) -> Result<PaneSlot> {
    tracing::debug!(cols, rows, pane_id = %pane_id.0, "attach_pane: entry");

    let mut stream_sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane_id.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let frame_read = FramedRead::new(rd, LengthDelimitedCodec::new());
    let frame_write = FramedWrite::new(wr, LengthDelimitedCodec::new());

    let mut output_frames: tokio_serde::SymmetricallyFramed<_, OutputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_read, SymmetricalBincode::default());
    let mut input_frames: tokio_serde::SymmetricallyFramed<_, InputFrame, _> =
        tokio_serde::SymmetricallyFramed::new(frame_write, SymmetricalBincode::default());

    // Bug B fix: bumped to 1024 to absorb output bursts without blocking the
    // net→UI task. The sender uses try_send so a full channel drops the chunk
    // (output loss) instead of hanging the UI loop (backpressure stall).
    let (net_tx, output_rx) = mpsc::channel::<PaneEvent>(1024);
    let (input_tx, mut key_rx) = mpsc::channel::<Bytes>(64);

    // net → UI
    tokio::spawn(async move {
        let mut frames: u64 = 0;
        while let Some(frame) = output_frames.next().await {
            match frame {
                Ok(f) => {
                    frames += 1;
                    if let Err(e) = net_tx.try_send(PaneEvent::Output(f.data)) {
                        match e {
                            mpsc::error::TrySendError::Full(_) => {
                                // Channel saturated during burst — drop chunk, keep running.
                                tracing::warn!("net→UI channel full; dropping output chunk");
                            }
                            mpsc::error::TrySendError::Closed(_) => break,
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Stream ended. Carry frame count so the UI can distinguish a
        // connection-level failure (0 frames) from a real pane exit (≥1 frames).
        let _ = net_tx.try_send(PaneEvent::Closed {
            frames_received: frames,
        });
    });

    // UI → net
    // Batch keystrokes: after the first byte arrives, drain all queued bytes
    // into a single concatenated buffer and send one InputFrame per tick.
    // This converts N sequential framed UDS writes down to 1 per render tick,
    // eliminating per-keystroke serialization latency for fast typists.
    tokio::spawn(async move {
        while let Some(first) = key_rx.recv().await {
            let mut buf: Vec<u8> = first.to_vec();
            while let Ok(more) = key_rx.try_recv() {
                buf.extend_from_slice(&more);
            }
            let batch_len = buf.len();
            let t0 = std::time::Instant::now();
            let send_result = input_frames
                .send(InputFrame {
                    session,
                    data: Bytes::from(buf),
                })
                .await;
            let elapsed_us = t0.elapsed().as_micros();
            tracing::debug!(
                batch_bytes = batch_len,
                elapsed_us,
                send_ok = send_result.is_ok(),
                "send_keys: input_frames RPC send"
            );
            if send_result.is_err() {
                break;
            }
        }
    });

    tracing::debug!(rows, cols, pane_id = %pane_id.0, "attach_pane: creating alacritty Term");
    let event_proxy = EventProxy::new();
    let term_config = TermConfig::default();
    let term = Term::new(
        term_config,
        &TermSize::new(cols as usize, rows as usize),
        event_proxy.clone(),
    );
    Ok(PaneSlot {
        pane_id,
        term,
        processor: AnsiProcessor::new(),
        event_proxy,
        input_tx,
        output_rx,
        recent_blocks: Vec::new(),
        ribbon_cursor: None,
        last_sent_size: (cols, rows),
        frames_received: 0,
        scroll_offset: 0,
        scrollback_capacity: 0,
        last_screen_rect: Rect::default(),
        ribbon_chip_rects: Vec::new(),
        pending_output: Vec::new(),
        parser_sized: false,
        last_output_log: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane focus
// ─────────────────────────────────────────────────────────────────────────────

/// Update active session's active tab focus_pane to point at the given slot index.
pub(crate) fn focus_slot(state: &mut AppState, target_slot_idx: usize) {
    let target_pane_id = match state.slots.get(target_slot_idx).and_then(|s| s.as_ref()) {
        Some(slot) => slot.pane_id,
        None => return,
    };
    let sv = &state.sessions[state.active_session];
    let tab = &sv.tabs[sv.active_tab];
    if pane_leaves_in_order(&tab.root).contains(&target_pane_id) {
        let sv = &mut state.sessions[state.active_session];
        sv.tabs[sv.active_tab].focus_pane = target_pane_id;
    }
}

/// Replaces the former dropfile (`focus.request`) approach with an RPC call so
/// that concurrent `pyrec select-pane` invocations are handled atomically.
pub(crate) async fn apply_focus_request(state: &mut AppState) {
    let Ok(Ok(Some(pane_id))) = state
        .control
        .take_focus_request(tarpc::context::current())
        .await
    else {
        return;
    };
    if let Some(slot_idx) = state
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|slot| slot.pane_id == pane_id))
    {
        focus_slot(state, slot_idx);
        let short = &pane_id.to_string()[..8];
        state.status_msg = Some(format!("focused pane {short} (select-pane)"));
    } else {
        state.status_msg = Some(format!(
            "select-pane: pane {pane_id} not open in this TUI — attach it first"
        ));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Split / open
// ─────────────────────────────────────────────────────────────────────────────

/// Split the active leaf. `horizontal` = true means HSplit (top/bottom).
/// M7-D: delegates to `open_pane_split` RPC; daemon owns layout.
/// Local layout is updated optimistically via `split_focused`; the daemon's
/// `LayoutChanged` event will reconcile on the next broadcast poll.
pub(crate) async fn split_active(state: &mut AppState, horizontal: bool) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();

    let focused_pane = {
        let sv = state.active_session_view_mut();
        let tab = &mut sv.tabs[sv.active_tab];
        tab.zoomed = None;
        tab.focus_pane
    };

    let orient = if horizontal {
        Orient::Horizontal
    } else {
        Orient::Vertical
    };
    let req = OpenPaneSplitReq {
        parent_pane: focused_pane,
        orient,
        name: None,
        cwd: std::env::current_dir().ok(),
        cmd: None,
    };
    let new_pane_id = state
        .control
        .open_pane_split(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane_split: {e}"))?;

    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    state.slots.push(Some(slot));

    let sv = state.active_session_view_mut();
    let tab = &mut sv.tabs[sv.active_tab];
    tab.root.split_focused(&focused_pane, new_pane_id, orient);
    tab.focus_pane = new_pane_id;

    Ok(())
}

/// Open a new pane in a new tab within the active session.
pub(crate) async fn open_new_tab(state: &mut AppState, label: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let session_id = state.active_session_id();
    let req = OpenPaneReq {
        session: session_id,
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let new_pane_id = state
        .control
        .open_pane(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon open_pane: {e}"))?;

    let slot = attach_pane(&state.socket, session_id, new_pane_id, cols, rows).await?;
    state.slots.push(Some(slot));

    let sv = state.active_session_view_mut();
    let tab_n = sv.tabs.len() + 1;
    let _label = label
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| format!("tab-{tab_n}"));
    sv.tabs.push(Tab {
        root: LayoutNode::Leaf(new_pane_id),
        focus_pane: new_pane_id,
        zoomed: None,
        boundaries: Vec::new(),
        drag: None,
    });
    sv.active_tab = sv.tabs.len() - 1;

    Ok(())
}

/// Spawn a brand-new daemon session and push a SessionView.
pub(crate) async fn open_new_session(state: &mut AppState, name: Option<String>) -> Result<()> {
    let (term_cols, term_rows) = term_size();
    let (cols, rows) = compute_pane_inner_size(term_cols, term_rows);
    let resolved_name = name.filter(|n| !n.is_empty());
    let req = SpawnReq {
        shell: state.shell.clone(),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: resolved_name.clone(),
    };
    let SpawnResp { session, pane } = state
        .control
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon spawn: {e}"))?;

    let slot = attach_pane(&state.socket, session, pane, cols, rows).await?;
    state.slots.push(Some(slot));

    let short8: String = session.0.to_string().chars().take(8).collect();
    let display_name = resolved_name.unwrap_or_else(|| format!("session-{short8}"));

    state.sessions.push(SessionView {
        id: session,
        name: display_name,
        tabs: vec![Tab {
            root: LayoutNode::Leaf(pane),
            focus_pane: pane,
            zoomed: None,
            boundaries: Vec::new(),
            drag: None,
        }],
        active_tab: 0,
    });
    state.active_session = state.sessions.len() - 1;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane close / layout collapse
// ─────────────────────────────────────────────────────────────────────────────

/// Locate the (session_idx, tab_idx) for a given PaneId.
fn locate_pane(state: &AppState, target_pane: PaneId) -> Option<(usize, usize)> {
    for (si, sess) in state.sessions.iter().enumerate() {
        for (ti, tab) in sess.tabs.iter().enumerate() {
            if pane_leaves_in_order(&tab.root).contains(&target_pane) {
                return Some((si, ti));
            }
        }
    }
    None
}

/// Close a pane by its slot index.
/// Removes the leaf from the layout tree, drops the slot, cascades tab/session removal.
pub(crate) fn close_pane_by_slot_idx(state: &mut AppState, slot_idx: usize) {
    let pane_id = match state.slots.get(slot_idx).and_then(|s| s.as_ref()) {
        Some(slot) => slot.pane_id,
        None => return,
    };

    let (sess_idx, tab_idx) = match locate_pane(state, pane_id) {
        Some(loc) => loc,
        None => return,
    };

    // Fire close_pane RPC fire-and-forget so the daemon evicts the pane.
    {
        let client = state.control.clone();
        tokio::runtime::Handle::current().spawn(async move {
            let _ = client.close_pane(tarpc::context::current(), pane_id).await;
        });
    }

    let new_focus_pane = state.sessions[sess_idx].tabs[tab_idx].root.close(&pane_id);

    if slot_idx < state.slots.len() {
        state.slots[slot_idx] = None;
    }

    // `LayoutNode::close` returns `None` in two cases:
    //   (a) `pane_id` was not found in the tree, OR
    //   (b) `pane_id` was the only leaf — the root cannot be replaced with an
    //       Empty variant, so the tree is left as `Leaf(pane_id)` even though
    //       the close was logically successful.
    //
    // Distinguish (b) from (a): if `new_focus_pane.is_none()` AND the tree
    // still only contains `pane_id`, we are in case (b) — the tab is now
    // empty.  Force `remaining` to empty so the tab-removal path below fires.
    let remaining = {
        let leaves = pane_leaves_in_order(&state.sessions[sess_idx].tabs[tab_idx].root);
        if new_focus_pane.is_none() && leaves.as_slice() == [pane_id] {
            vec![]
        } else {
            leaves
        }
    };

    if remaining.is_empty() {
        state.sessions[sess_idx].tabs.remove(tab_idx);
        if state.sessions[sess_idx].tabs.is_empty() {
            state.sessions.remove(sess_idx);
            if state.sessions.is_empty() {
                return;
            }
            state.active_session = state.active_session.min(state.sessions.len() - 1);
        } else {
            state.sessions[sess_idx].active_tab =
                tab_idx.min(state.sessions[sess_idx].tabs.len() - 1);
            let new_tab_idx = state.sessions[sess_idx].active_tab;
            if let Some(&first_pane) =
                pane_leaves_in_order(&state.sessions[sess_idx].tabs[new_tab_idx].root).first()
            {
                state.sessions[sess_idx].tabs[new_tab_idx].focus_pane = first_pane;
            }
        }
    } else {
        let focus = new_focus_pane
            .filter(|p| remaining.contains(p))
            .or_else(|| remaining.into_iter().next());
        if let Some(fp) = focus {
            state.sessions[sess_idx].tabs[tab_idx].focus_pane = fp;
        }
        state.sessions[sess_idx].tabs[tab_idx].zoomed = None;
    }
}

/// Close the focused pane in the active tab.
pub(crate) fn close_focused_pane(state: &mut AppState) {
    let sess_idx = state.active_session;
    let tab_idx = state.sessions[sess_idx].active_tab;
    let focus_pane = state.sessions[sess_idx].tabs[tab_idx].focus_pane;
    if let Some(slot_idx) = focused_slot_idx(focus_pane, &state.slots) {
        close_pane_by_slot_idx(state, slot_idx);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use futures::StreamExt;
    use pyre_proto::{
        layout::LayoutNode, PaneId, PyreDaemon, PyreDaemonClient, PyreError, SessionId,
    };
    use tarpc::server::{BaseChannel, Channel};
    use tokio::sync::{mpsc, watch};

    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
    use alacritty_terminal::Term;

    use crate::app::sessions::SessionView;
    use crate::app::state::AppState;
    use crate::fire_motion::AnimClock;
    use crate::model::layout::pane_leaves_in_order;
    use crate::model::pane::{EventProxy, PaneEvent, PaneSlot};
    use crate::model::tab::Tab;
    use crate::model::toast::ToastDeck;
    use crate::render::overlay::search::SearchState;
    use crate::render::pane::TermSize;

    use super::{close_focused_pane, close_pane_by_slot_idx};

    // ── Stub daemon ──────────────────────────────────────────────────────────

    /// A no-op tarpc server that satisfies the trait bound so we can build a
    /// `PyreDaemonClient` without a live daemon process.
    /// `close_pane_by_slot_idx` fires the close RPC fire-and-forget and
    /// ignores the result, so `close_pane` returning `Ok(())` is sufficient.
    #[derive(Clone)]
    struct StubDaemon;

    impl pyre_proto::service::PyreDaemon for StubDaemon {
        async fn spawn(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::SpawnReq,
        ) -> Result<pyre_proto::SpawnResp, PyreError> {
            Err(PyreError::SpawnFailed("stub".into()))
        }
        async fn attach(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<pyre_proto::AttachAck, PyreError> {
            Err(PyreError::NoSuchSession(_session))
        }
        async fn detach(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn kill(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn list_blocks(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::blocks::ListBlocksReq,
        ) -> Result<Vec<pyre_proto::Block>, PyreError> {
            Ok(vec![])
        }
        async fn search_blocks(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::blocks::SearchBlocksReq,
        ) -> Result<Vec<pyre_proto::blocks::BlockHit>, PyreError> {
            Ok(vec![])
        }
        async fn list_sessions(
            self,
            _ctx: tarpc::context::Context,
        ) -> Result<Vec<pyre_proto::SessionInfo>, PyreError> {
            Ok(vec![])
        }
        async fn list_panes(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<Vec<pyre_proto::PaneInfo>, PyreError> {
            Ok(vec![])
        }
        async fn open_pane(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::OpenPaneReq,
        ) -> Result<PaneId, PyreError> {
            Err(PyreError::SpawnFailed("stub".into()))
        }
        async fn close_pane(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
        ) -> Result<(), PyreError> {
            Ok(()) // fire-and-forget; must not fail
        }
        async fn replay(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _recent_blocks: u32,
        ) -> Result<pyre_proto::ReplayBlocks, PyreError> {
            Err(PyreError::NoSuchPane(_pane))
        }
        async fn get_block_stdout(
            self,
            _ctx: tarpc::context::Context,
            _block_id: pyre_proto::BlockId,
        ) -> Result<Vec<u8>, PyreError> {
            Ok(vec![])
        }
        async fn capture_pane(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _lines: u32,
        ) -> Result<Vec<u8>, PyreError> {
            Ok(vec![])
        }
        async fn close_session(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn set_pane_state(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _state: pyre_proto::PaneStateKind,
            _reason: String,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn list_all_panes(
            self,
            _ctx: tarpc::context::Context,
        ) -> Result<Vec<pyre_proto::PaneInfo>, PyreError> {
            Ok(vec![])
        }
        async fn inspect_pid(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
        ) -> Result<pyre_proto::PidInspect, PyreError> {
            Err(PyreError::NoSuchPane(_pane))
        }
        async fn send_keys(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _bytes: Vec<u8>,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn resize_pane(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::ResizePaneReq,
        ) -> Result<pyre_proto::ResizePaneRes, PyreError> {
            Err(PyreError::NoSuchPane(_req.pane_id))
        }
        async fn rename_session(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
            _name: String,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn wait_pane_state(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _state: pyre_proto::PaneStateKind,
            _timeout_ms: u32,
        ) -> Result<bool, PyreError> {
            Ok(false)
        }
        async fn mark_pane_seen(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn last_block_for_pane(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
        ) -> Result<Option<pyre_proto::Block>, PyreError> {
            Ok(None)
        }
        async fn request_focus(
            self,
            _ctx: tarpc::context::Context,
            _pane_id: PaneId,
        ) -> Result<bool, PyreError> {
            Ok(false)
        }
        async fn take_focus_request(
            self,
            _ctx: tarpc::context::Context,
        ) -> Result<Option<PaneId>, PyreError> {
            Ok(None)
        }
        async fn next_pane_event(
            self,
            _ctx: tarpc::context::Context,
            _after_seq: u64,
            _timeout_ms: u32,
        ) -> Result<Vec<pyre_proto::service::PaneEvent>, PyreError> {
            Ok(vec![])
        }
        async fn gc_stale_sessions(
            self,
            _ctx: tarpc::context::Context,
        ) -> Result<Vec<String>, PyreError> {
            Ok(vec![])
        }
        async fn open_pane_split(
            self,
            _ctx: tarpc::context::Context,
            _req: pyre_proto::service::OpenPaneSplitReq,
        ) -> Result<PaneId, PyreError> {
            Err(PyreError::SpawnFailed("stub".into()))
        }
        async fn set_pane_weight(
            self,
            _ctx: tarpc::context::Context,
            _pane: PaneId,
            _weight: u16,
        ) -> Result<(), PyreError> {
            Ok(())
        }
        async fn get_session_layout(
            self,
            _ctx: tarpc::context::Context,
            _session: SessionId,
        ) -> Result<LayoutNode, PyreError> {
            Err(PyreError::NoSuchSession(_session))
        }
    }

    // ── Test helpers ─────────────────────────────────────────────────────────

    /// Build an in-process tarpc client backed by `StubDaemon`.
    async fn stub_client() -> PyreDaemonClient {
        let (client_transport, server_transport) = tarpc::transport::channel::unbounded();
        let server = BaseChannel::with_defaults(server_transport);
        tokio::spawn(
            server
                .execute(StubDaemon.serve())
                .for_each(|resp| async move {
                    tokio::spawn(resp);
                }),
        );
        PyreDaemonClient::new(tarpc::client::Config::default(), client_transport).spawn()
    }

    /// Construct a minimal live `PaneSlot` for a given `PaneId`.
    fn make_slot(pane_id: PaneId) -> PaneSlot {
        let event_proxy = EventProxy::new();
        let term = Term::new(
            TermConfig::default(),
            &TermSize::new(80, 24),
            event_proxy.clone(),
        );
        let (input_tx, _input_rx) = mpsc::channel::<Bytes>(8);
        let (_output_tx, output_rx) = mpsc::channel::<PaneEvent>(8);
        PaneSlot {
            pane_id,
            term,
            processor: AnsiProcessor::new(),
            event_proxy,
            input_tx,
            output_rx,
            recent_blocks: vec![],
            ribbon_cursor: None,
            last_sent_size: (80, 24),
            frames_received: 0,
            scroll_offset: 0,
            scrollback_capacity: 0,
            last_screen_rect: ratatui::layout::Rect::default(),
            ribbon_chip_rects: vec![],
            pending_output: vec![],
            parser_sized: false,
            last_output_log: None,
        }
    }

    /// Build a minimal `AppState` with one session containing a VSplit of two
    /// panes: `pane_a` (focused) and `pane_b`.  Two live slots are inserted.
    async fn two_pane_state(pane_a: PaneId, pane_b: PaneId, session: SessionId) -> AppState {
        let control = stub_client().await;
        let (_, blocks_rx) = watch::channel(HashMap::new());
        let (_, toast_rx) = mpsc::channel(1);

        let reg = pyre_themes::Registry::builtin();
        let theme = reg
            .get(pyre_themes::Registry::default_theme())
            .expect("default theme present")
            .clone();

        let tab = Tab {
            root: LayoutNode::VSplit(vec![
                (LayoutNode::Leaf(pane_a), 50),
                (LayoutNode::Leaf(pane_b), 50),
            ]),
            focus_pane: pane_a,
            zoomed: None,
            boundaries: vec![],
            drag: None,
        };

        AppState {
            sessions: vec![SessionView {
                id: session,
                name: "test-session".into(),
                tabs: vec![tab],
                active_tab: 0,
            }],
            active_session: 0,
            slots: vec![Some(make_slot(pane_a)), Some(make_slot(pane_b))],
            session_lost: false,
            control,
            socket: PathBuf::from("/tmp/pyre-test.sock"),
            shell: None,
            search: SearchState::default(),
            status_msg: None,
            sidebar_open: false,
            sidebar_data: vec![],
            sidebar_last_poll: Instant::now() - Duration::from_secs(10),
            sidebar_cursor: 0,
            sidebar_focused: false,
            selection: None,
            last_click: None,
            context_menu: None,
            pid_inspect: None,
            prompt: None,
            session_strip_rects: vec![],
            session_strip_scroll: 0,
            session_strip_left_arrow: None,
            session_strip_right_arrow: None,
            session_plus_rect: None,
            tab_plus_rect: None,
            pending_resizes: vec![],
            tab_chip_rects: vec![],
            dragging_tab: None,
            pager_rect: None,
            session_list_last_poll: Instant::now() - Duration::from_secs(10),
            layout_resync_last_poll: Instant::now() - Duration::from_secs(10),
            blocks_rx,
            anim: AnimClock::new(),
            pager: None,
            theme,
            theme_picker: None,
            toast_deck: ToastDeck::new(false, 3000, 3),
            toast_rx,
            pending_menu_action: None,
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Closing the focused pane of a 2-pane session removes only that pane,
    /// the sibling pane survives, the session is not removed, and
    /// `sessions.is_empty()` remains false (no should_quit).
    ///
    /// This is the primary regression guard for the Ctrl-B+x / close-X bug:
    /// before the fix, the `LayoutNode::close` single-leaf path left a zombie
    /// leaf in the tree after the SECOND pane was closed, preventing tab
    /// removal and causing `session_lost` to fire spuriously.
    #[tokio::test]
    async fn close_focused_pane_of_split_removes_only_target() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let session = SessionId::new();

        let mut state = two_pane_state(pane_a, pane_b, session).await;

        // Precondition: 2 panes in the tab.
        assert_eq!(
            pane_leaves_in_order(&state.sessions[0].tabs[0].root).len(),
            2,
            "precondition: tab must have 2 panes"
        );

        // Act: close pane_a (the focused pane, slot index 0).
        close_pane_by_slot_idx(&mut state, 0);

        // Session must survive.
        assert!(
            !state.sessions.is_empty(),
            "sessions must not be empty after closing one pane of a split"
        );

        // Exactly one pane left in the tab.
        let remaining = pane_leaves_in_order(&state.sessions[0].tabs[0].root);
        assert_eq!(
            remaining.len(),
            1,
            "tab must have exactly 1 pane after closing one of two"
        );
        assert_eq!(
            remaining[0], pane_b,
            "the surviving pane must be pane_b (the sibling)"
        );

        // Focus must have shifted to pane_b.
        assert_eq!(
            state.sessions[0].tabs[0].focus_pane, pane_b,
            "focus must be on the surviving pane"
        );

        // Slot 0 is dead (closed), slot 1 is still live.
        assert!(
            state.slots[0].is_none(),
            "slot 0 (pane_a) must be None after close"
        );
        assert!(state.slots[1].is_some(), "slot 1 (pane_b) must remain live");
    }

    /// Closing the LAST pane in a single-pane session removes the tab AND
    /// the session.  The caller (event loop) checks `sessions.is_empty()` to
    /// decide whether to quit — this test verifies that path triggers correctly
    /// rather than leaving a zombie tab with a dead slot.
    ///
    /// Before the fix in `close_pane_by_slot_idx`, `LayoutNode::close` returned
    /// `None` on a single-leaf tree but left the tree as `Leaf(pane_id)`.
    /// `pane_leaves_in_order` then returned `[pane_id]` (non-empty), so the
    /// tab was NOT removed.  The session-lost overlay fired instead of a clean
    /// quit, and subsequent key presses were ignored by the overlay intercept
    /// until the user explicitly pressed 'q'.
    #[tokio::test]
    async fn close_last_pane_removes_tab_and_session() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let session = SessionId::new();

        // Start with a 2-pane state and close both panes sequentially.
        let mut state = two_pane_state(pane_a, pane_b, session).await;

        // Close pane_a (slot 0) — leaves a 1-pane tab.
        close_pane_by_slot_idx(&mut state, 0);
        assert!(
            !state.sessions.is_empty(),
            "session must survive after first close"
        );
        assert_eq!(
            pane_leaves_in_order(&state.sessions[0].tabs[0].root).len(),
            1,
            "one pane must remain after first close"
        );

        // Close pane_b (now slot 1) — last pane in the session.
        close_pane_by_slot_idx(&mut state, 1);

        // Session must be gone.
        assert!(
            state.sessions.is_empty(),
            "sessions must be empty after closing all panes — event loop should quit"
        );
    }

    /// `close_focused_pane` dispatches correctly through `focused_slot_idx`
    /// and produces the same outcome as calling `close_pane_by_slot_idx`
    /// directly — only the focused pane is removed.
    #[tokio::test]
    async fn close_focused_pane_dispatch_removes_only_focus() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let session = SessionId::new();

        let mut state = two_pane_state(pane_a, pane_b, session).await;
        // pane_a is the focused pane (set in two_pane_state).
        assert_eq!(state.sessions[0].tabs[0].focus_pane, pane_a);

        close_focused_pane(&mut state);

        assert!(
            !state.sessions.is_empty(),
            "should_quit must be false — session survives"
        );
        let remaining = pane_leaves_in_order(&state.sessions[0].tabs[0].root);
        assert_eq!(remaining, vec![pane_b], "only pane_b must remain");
        assert_eq!(state.sessions[0].tabs[0].focus_pane, pane_b);
    }
}
