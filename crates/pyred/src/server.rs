//! tarpc `PyreDaemon` service implementation.

use std::sync::Arc;

use pyre_proto::{
    AttachAck, Block, BlockHit, BlockId, ListBlocksReq, OpenPaneReq, PaneId, PaneInfo,
    PaneStateKind, PyreError, ReplayBlocks, ResizePaneReq, ResizePaneRes, SearchBlocksReq,
    SessionId, SessionInfo, SpawnReq, SpawnResp,
};
use tarpc::context;

use crate::index::BlockIndex;
use crate::session::SessionRegistry;

#[derive(Clone)]
pub struct DaemonImpl {
    pub registry: Arc<SessionRegistry>,
    pub store: Arc<crate::store::Store>,
    pub block_index: Arc<BlockIndex>,
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
        let limit = req.limit;
        let ids = tokio::task::spawn_blocking(move || block_index.search(&query, limit))
            .await
            .map_err(|e| PyreError::Io(e.to_string()))?
            .map_err(|e| PyreError::Io(e.to_string()))?;

        let mut hits = Vec::with_capacity(ids.len());
        for id in ids {
            match self.store.get_block(id).await {
                Ok(Some(block)) => hits.push(BlockHit {
                    block,
                    snippet: String::new(),
                }),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("get_block {id:?}: {e:#}");
                }
            }
        }
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
            .close_pane(pane)
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
            let rb = pane_state.ringbuf.lock().expect("ringbuf poisoned");
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
        use regex::Regex;
        use std::sync::OnceLock;

        static ANSI_RE: OnceLock<Regex> = OnceLock::new();
        let re = ANSI_RE.get_or_init(|| {
            Regex::new(r"\x1b\[[\x20-\x3f]*[\x40-\x7e]").expect("static regex is valid")
        });

        let (_session, pane_state) = self
            .registry
            .get_pane(pane)
            .await
            .ok_or(PyreError::NoSuchPane(pane))?;

        let snapshot = {
            let rb = pane_state.ringbuf.lock().expect("ringbuf poisoned");
            rb.snapshot()
        };

        let lossy = String::from_utf8_lossy(&snapshot);
        let stripped = re.replace_all(&lossy, "");
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
}
