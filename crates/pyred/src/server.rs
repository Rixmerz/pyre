//! tarpc `PyreDaemon` service implementation.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use pyre_proto::{
    layout, AttachAck, Block, BlockHit, BlockId, ListBlocksReq, OpenPaneReq, OpenPaneSplitReq,
    PaneEvent, PaneId, PaneInfo, PaneStateKind, PyreError, ReplayBlocks, ResizePaneReq,
    ResizePaneRes, SearchBlocksReq, SessionId, SessionInfo, SpawnReq, SpawnResp,
};
use tarpc::context;

use crate::index::BlockIndex;
use crate::session::SessionRegistry;

#[derive(Clone)]
pub struct DaemonImpl {
    pub registry: Arc<SessionRegistry>,
    pub store: Arc<crate::store::Store>,
    pub block_index: Arc<BlockIndex>,
    /// Pending focus requests enqueued by `request_focus` and dequeued by `take_focus_request`.
    /// Shared across all control connections so pyrec and the TUI see the same queue.
    pub focus_queue: Arc<Mutex<VecDeque<PaneId>>>,
}

impl pyre_proto::service::PyreDaemon for DaemonImpl {
    async fn spawn(self, _ctx: context::Context, req: SpawnReq) -> Result<SpawnResp, PyreError> {
        let session = self
            .registry
            .new_session(self.store.clone(), req.name.clone())
            .await;
        let open_req = OpenPaneReq {
            session: session.id,
            shell: req.shell,
            cwd: req.cwd,
            cols: req.cols,
            rows: req.rows,
            env: req.env,
            name: None,
        };
        let pane = self
            .registry
            .open_pane(
                session.id,
                open_req,
                self.store.clone(),
                self.block_index.clone(),
            )
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?;
        Ok(SpawnResp {
            session: session.id,
            pane: pane.id,
        })
    }

    async fn attach(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<AttachAck, PyreError> {
        let s = self
            .registry
            .get_session(session)
            .await
            .ok_or(PyreError::NoSuchSession(session))?;
        // Return dimensions from the first live pane, or 0/0 if none.
        let (cols, rows) = {
            let panes = s.panes.lock().await;
            panes
                .values()
                .next()
                .map(|p| (p.cols, p.rows))
                .unwrap_or((0, 0))
        };
        Ok(AttachAck {
            session: s.id,
            cols,
            rows,
        })
    }

    async fn detach(self, _ctx: context::Context, _session: SessionId) -> Result<(), PyreError> {
        // S1: detach is a no-op — stream connection close already detaches.
        Ok(())
    }

    async fn kill(self, _ctx: context::Context, session: SessionId) -> Result<(), PyreError> {
        self.registry
            .kill_session(session)
            .await
            .map_err(|_| PyreError::NoSuchSession(session))
    }

    async fn list_blocks(
        self,
        _ctx: context::Context,
        req: ListBlocksReq,
    ) -> Result<Vec<Block>, PyreError> {
        self.store
            .list_blocks(req.session, req.limit)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn search_blocks(
        self,
        _ctx: context::Context,
        req: SearchBlocksReq,
    ) -> Result<Vec<BlockHit>, PyreError> {
        let block_index = self.block_index.clone();
        let query = req.query.clone();
        let failures_only = req.failures_only;
        let limit = req.limit;
        let session = req.session;
        let pane = req.pane;
        let exit_code = req.exit_code;
        let ids = tokio::task::spawn_blocking(move || {
            block_index.search(&query, limit, failures_only, session, pane, exit_code)
        })
        .await
        .map_err(|e| PyreError::Io(e.to_string()))?
        .map_err(|e| PyreError::Io(e.to_string()))?;

        let mut blocks = Vec::with_capacity(ids.len());
        for id in ids {
            match self.store.get_block(id).await {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("get_block {id:?}: {e:#}");
                }
            }
        }
        let store = self.store.clone();
        let hits = tokio::task::spawn_blocking(move || {
            crate::search_filter::hits_with_snippets(&store, blocks, 160)
        })
        .await
        .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(hits)
    }

    async fn list_sessions(self, _ctx: context::Context) -> Result<Vec<SessionInfo>, PyreError> {
        Ok(self.registry.list_sessions().await)
    }

    async fn list_panes(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<Vec<PaneInfo>, PyreError> {
        Ok(self.registry.list_panes(session).await)
    }

    async fn open_pane(
        self,
        _ctx: context::Context,
        req: OpenPaneReq,
    ) -> Result<PaneId, PyreError> {
        let session_id = req.session;
        let pane = self
            .registry
            .open_pane(
                session_id,
                req,
                self.store.clone(),
                self.block_index.clone(),
            )
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))?;
        Ok(pane.id)
    }

    async fn close_pane(self, _ctx: context::Context, pane: PaneId) -> Result<(), PyreError> {
        self.registry
            .close_pane(pane, Some(&self.store))
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn replay(
        self,
        _ctx: context::Context,
        pane: PaneId,
        recent_blocks: u32,
    ) -> Result<ReplayBlocks, PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        let snapshot = {
            let rb = pane_state.ringbuf.lock().unwrap_or_else(|e| {
                tracing::error!("ringbuf lock poisoned in replay; recovering guard: {e}");
                e.into_inner()
            });
            rb.snapshot()
        };
        let recent = self
            .store
            .list_blocks_for_pane(pane, recent_blocks)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(ReplayBlocks { recent, snapshot })
    }

    async fn get_block_stdout(
        self,
        _ctx: context::Context,
        block_id: BlockId,
    ) -> Result<Vec<u8>, PyreError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.read_block_stdout(block_id))
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn capture_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
        lines: u32,
    ) -> Result<Vec<u8>, PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;

        let snapshot = {
            let rb = pane_state.ringbuf.lock().unwrap_or_else(|e| {
                tracing::error!("ringbuf lock poisoned in capture_pane; recovering guard: {e}");
                e.into_inner()
            });
            rb.snapshot()
        };

        let lossy = String::from_utf8_lossy(&snapshot);
        let stripped = crate::ansi::ANSI_CSI_RE.replace_all(&lossy, "");
        let all_lines: Vec<&str> = stripped.split('\n').collect();
        let take = (lines as usize).min(all_lines.len());
        let tail_lines = &all_lines[all_lines.len().saturating_sub(take)..];
        let joined = tail_lines.join("\n");
        Ok(joined.into_bytes())
    }

    async fn close_session(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<(), PyreError> {
        self.registry
            .kill_session(session)
            .await
            .map_err(|_| PyreError::NoSuchSession(session))
    }

    async fn set_pane_state(
        self,
        _ctx: context::Context,
        pane: PaneId,
        state: PaneStateKind,
        reason: String,
    ) -> Result<(), PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;

        let override_secs: u64 = std::env::var("PYRE_OVERRIDE_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);

        let mut t = pane_state
            .state_tracker
            .lock()
            .map_err(|_| PyreError::Io("tracker lock poisoned".into()))?;
        t.set_override(state, reason, override_secs);
        Ok(())
    }

    async fn list_all_panes(self, _ctx: context::Context) -> Result<Vec<PaneInfo>, PyreError> {
        Ok(self.registry.list_all_panes().await)
    }

    async fn send_keys(
        self,
        _ctx: context::Context,
        pane: PaneId,
        bytes: Vec<u8>,
    ) -> Result<(), PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        pane_state
            .input_tx
            .send(bytes::Bytes::from(bytes))
            .await
            .map_err(|e| PyreError::Io(format!("input channel closed: {e}")))?;
        Ok(())
    }

    async fn inspect_pid(
        self,
        _ctx: context::Context,
        pane: PaneId,
    ) -> Result<pyre_proto::PidInspect, PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;

        let pid = pane_state
            .state_tracker
            .lock()
            .map(|t| t.root_pid)
            .unwrap_or(0);
        Ok(crate::inspect::inspect_pid(pid))
    }

    async fn rename_session(
        self,
        _ctx: context::Context,
        session: SessionId,
        name: String,
    ) -> Result<(), PyreError> {
        self.registry
            .rename_session(session, name, &self.store)
            .await
            .map_err(|_| PyreError::NoSuchSession(session))
    }

    async fn rename_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
        name: String,
    ) -> Result<(), PyreError> {
        self.registry
            .rename_pane(pane, name, &self.store)
            .await
            .map_err(|_| PyreError::NoSuchPane(pane))
    }

    async fn resize_pane(
        self,
        _ctx: context::Context,
        req: ResizePaneReq,
    ) -> Result<ResizePaneRes, PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(req.pane_id)
            .await
            .ok_or(PyreError::NoSuchPane(req.pane_id))?;
        pane_state
            .master
            .lock()
            .await
            .resize(portable_pty::PtySize {
                rows: req.size.rows,
                cols: req.size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PyreError::Io(format!("resize: {e}")))?;
        Ok(ResizePaneRes { ok: true })
    }

    async fn wait_pane_state(
        self,
        _ctx: context::Context,
        pane: PaneId,
        state: PaneStateKind,
        timeout_ms: u32,
    ) -> Result<bool, PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;

        let mut rx = {
            let t = pane_state
                .state_tracker
                .lock()
                .map_err(|_| PyreError::Io("tracker lock poisoned".into()))?;
            if t.state == state {
                return Ok(true);
            }
            t.subscribe()
        };

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms.max(1) as u64);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            tokio::select! {
                _ = tokio::time::sleep(remaining) => return Ok(false),
                changed = rx.changed() => {
                    if changed.is_err() {
                        return Ok(false);
                    }
                    if *rx.borrow() == state {
                        return Ok(true);
                    }
                }
            }
        }
    }

    async fn mark_pane_seen(self, _ctx: context::Context, pane: PaneId) -> Result<(), PyreError> {
        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;
        pane_state
            .state_tracker
            .lock()
            .map_err(|_| PyreError::Io("tracker lock poisoned".into()))?
            .mark_seen();
        Ok(())
    }

    async fn last_block_for_pane(
        self,
        _ctx: context::Context,
        pane: PaneId,
    ) -> Result<Option<Block>, PyreError> {
        let blocks = self
            .store
            .list_blocks_for_pane(pane, 1)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(blocks.into_iter().next())
    }

    async fn request_focus(
        self,
        _ctx: context::Context,
        pane_id: PaneId,
    ) -> Result<bool, PyreError> {
        self.focus_queue
            .lock()
            .map_err(|_| PyreError::Io("focus_queue lock poisoned".into()))?
            .push_back(pane_id);
        Ok(true)
    }

    async fn take_focus_request(self, _ctx: context::Context) -> Result<Option<PaneId>, PyreError> {
        Ok(self
            .focus_queue
            .lock()
            .map_err(|_| PyreError::Io("focus_queue lock poisoned".into()))?
            .pop_front())
    }

    async fn gc_stale_sessions(self, _ctx: context::Context) -> Result<Vec<String>, PyreError> {
        let sessions = self.registry.list_sessions().await;
        let mut evicted = Vec::new();
        for s in sessions {
            if s.pane_count == 0 {
                // kill_session removes from the in-memory registry.
                match self.registry.kill_session(s.id).await {
                    Ok(()) => evicted.push(s.id.0.to_string()),
                    Err(e) => tracing::warn!("gc_stale_sessions: skip {}: {e:#}", s.id),
                }
            }
        }
        tracing::info!("gc_stale_sessions: evicted {} session(s)", evicted.len());
        Ok(evicted)
    }

    // ── Layout RPCs (M7-C, ADR-0005) ──────────────────────────────────────

    async fn open_pane_split(
        self,
        _ctx: context::Context,
        req: OpenPaneSplitReq,
    ) -> Result<PaneId, PyreError> {
        self.registry
            .open_pane_split(
                req.parent_pane,
                req.orient,
                req.name,
                req.cwd,
                req.cmd,
                self.store.clone(),
                self.block_index.clone(),
            )
            .await
            .map_err(|e| PyreError::SpawnFailed(e.to_string()))
    }

    async fn set_pane_weight(
        self,
        _ctx: context::Context,
        pane: PaneId,
        weight: u16,
    ) -> Result<(), PyreError> {
        self.registry
            .set_pane_weight(pane, weight, &self.store)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
    }

    async fn get_session_layout(
        self,
        _ctx: context::Context,
        session_id: SessionId,
    ) -> Result<layout::LayoutNode, PyreError> {
        self.registry
            .get_layout(session_id)
            .await
            .ok_or(PyreError::NoSuchSession(session_id))
    }

    async fn next_pane_event(
        self,
        _ctx: context::Context,
        after_seq: u64,
        timeout_ms: u32,
    ) -> Result<Vec<PaneEvent>, PyreError> {
        // Drain any already-buffered events with seq > after_seq and subscribe
        // to the live broadcast in one atomic step so no event can slip through.
        let (history, mut rx) = self.registry.events_after(after_seq);
        if !history.is_empty() {
            return Ok(history);
        }

        let deadline = tokio::time::Duration::from_millis(timeout_ms.max(1) as u64);
        let mut collected: Vec<PaneEvent> = Vec::new();

        // Collect the first event that arrives, then drain any additional
        // events that land within a short coalescing window (1 ms) so callers
        // receive a small batch rather than one event per RPC round-trip.
        let got_first = tokio::time::timeout(deadline, async {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.seq > after_seq => {
                        collected.push(ev);
                        break;
                    }
                    Ok(_) => continue, // stale event from before our cursor
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Receiver fell behind; drain ring for missed events.
                        let (missed, new_rx) = self.registry.events_after(after_seq);
                        rx = new_rx;
                        if !missed.is_empty() {
                            collected.extend(missed);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;

        if got_first.is_err() {
            // Timeout — normal; return empty vec.
            return Ok(vec![]);
        }

        // Coalesce: drain any additional events that arrive within 1 ms.
        let coalesce = tokio::time::Duration::from_millis(1);
        loop {
            match tokio::time::timeout(coalesce, rx.recv()).await {
                Ok(Ok(ev)) if ev.seq > after_seq => collected.push(ev),
                _ => break,
            }
        }

        Ok(collected)
    }
}
