//! tarpc `PyreDaemon` service implementation.

use std::sync::Arc;

use pyre_proto::{AttachAck, PyreError, SessionId, SpawnReq};
use tarpc::context;

use crate::pty::{spawn_pty, SessionRegistry};

#[derive(Clone)]
pub struct DaemonImpl {
    pub registry: Arc<SessionRegistry>,
}

impl pyre_proto::service::PyreDaemon for DaemonImpl {
    async fn spawn(self, _ctx: context::Context, req: SpawnReq) -> Result<SessionId, PyreError> {
        let sess = spawn_pty(req).map_err(|e| PyreError::SpawnFailed(e.to_string()))?;
        let arc = self.registry.insert(sess).await;
        Ok(arc.id)
    }

    async fn attach(
        self,
        _ctx: context::Context,
        session: SessionId,
    ) -> Result<AttachAck, PyreError> {
        let s = self
            .registry
            .get(session)
            .await
            .ok_or(PyreError::NoSuchSession(session))?;
        Ok(AttachAck {
            session: s.id,
            cols: s.cols,
            rows: s.rows,
        })
    }

    async fn detach(self, _ctx: context::Context, _session: SessionId) -> Result<(), PyreError> {
        // S1: detach is a no-op — stream connection close already detaches.
        Ok(())
    }

    async fn kill(self, _ctx: context::Context, session: SessionId) -> Result<(), PyreError> {
        let s = self
            .registry
            .remove(session)
            .await
            .ok_or(PyreError::NoSuchSession(session))?;
        s.kill().await.map_err(|e| PyreError::Io(e.to_string()))?;
        Ok(())
    }
}
