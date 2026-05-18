//! tarpc `PyreDaemon` service implementation.

use std::sync::Arc;

use pyre_proto::{
    AttachAck, Block, BlockHit, ListBlocksReq, OpenPaneReq, PaneId, PaneInfo, PyreError,
    ReplayBlocks, SearchBlocksReq, SessionId, SessionInfo, SpawnReq, SpawnResp,
};
use tarpc::context;

use crate::session::SessionRegistry;

#[derive(Clone)]
pub struct DaemonImpl {
    pub registry: Arc<SessionRegistry>,
    pub store: Arc<crate::store::Store>,
}

impl pyre_proto::service::PyreDaemon for DaemonImpl {
    async fn spawn(self, _ctx: context::Context, req: SpawnReq) -> Result<SpawnResp, PyreError> {
        let session = self.registry.new_session(self.store.clone()).await;
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
            .open_pane(session.id, open_req, self.store.clone())
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
        self.store
            .linear_search(&req.query, req.limit)
            .await
            .map_err(|e| PyreError::Io(e.to_string()))
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
            .open_pane(session_id, req, self.store.clone())
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
}
