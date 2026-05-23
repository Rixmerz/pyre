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
    let Ok(Ok(Some(pane_str))) = state
        .control
        .take_focus_request(tarpc::context::current())
        .await
    else {
        return;
    };
    let Ok(pane_uuid) = uuid::Uuid::parse_str(&pane_str) else {
        return;
    };
    let pane_id = PaneId(pane_uuid);
    if let Some(slot_idx) = state
        .slots
        .iter()
        .position(|s| s.as_ref().is_some_and(|slot| slot.pane_id == pane_id))
    {
        focus_slot(state, slot_idx);
        let short: String = pane_str.chars().take(8).collect();
        state.status_msg = Some(format!("focused pane {short} (select-pane)"));
    } else {
        state.status_msg = Some(format!(
            "select-pane: pane {pane_str} not open in this TUI — attach it first"
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

    let remaining = pane_leaves_in_order(&state.sessions[sess_idx].tabs[tab_idx].root);

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
