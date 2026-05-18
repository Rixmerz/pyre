//! Multi-pane session registry for pyred.
//!
//! Each `SessionState` owns a set of `PaneState`s. The registry holds all
//! live sessions and provides the coordination surface used by server.rs.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use pyre_proto::{BlockEvent, OpenPaneReq, PaneId, PaneInfo, SessionId, SessionInfo, SpawnReq};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};

use crate::pty::spawn_pty;
use crate::store::Store;

pub struct PaneState {
    pub id: PaneId,
    pub session: SessionId,
    pub cols: u16,
    pub rows: u16,
    pub shell: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Mutex<Option<DateTime<Utc>>>,
    // PTY plumbing — same fields that lived in PtySession.
    #[allow(dead_code)] // phase 6+: resize RPC
    pub master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub output_tx: broadcast::Sender<Bytes>,
    #[allow(dead_code)] // phase 6+: stream connections subscribe to block events
    pub events_tx: broadcast::Sender<BlockEvent>,
    pub input_tx: mpsc::Sender<Bytes>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    pub ringbuf: Arc<StdMutex<crate::ringbuf::RingBuf>>,
}

impl PaneState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: PaneId,
        session: SessionId,
        cols: u16,
        rows: u16,
        shell: String,
        created_at: DateTime<Utc>,
        master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
        output_tx: broadcast::Sender<Bytes>,
        events_tx: broadcast::Sender<BlockEvent>,
        input_tx: mpsc::Sender<Bytes>,
        child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
        ringbuf: Arc<StdMutex<crate::ringbuf::RingBuf>>,
    ) -> Self {
        Self {
            id,
            session,
            cols,
            rows,
            shell,
            created_at,
            closed_at: Mutex::new(None),
            master,
            output_tx,
            events_tx,
            input_tx,
            child,
            ringbuf,
        }
    }
}

impl PaneState {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.output_tx.subscribe()
    }

    pub async fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().await;
        let _ = child.kill();
        Ok(())
    }
}

pub struct SessionState {
    pub id: SessionId,
    pub name: RwLock<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: Mutex<DateTime<Utc>>,
    pub panes: Mutex<HashMap<PaneId, Arc<PaneState>>>,
}

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionId, Arc<SessionState>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn new_session(&self, store: Arc<Store>) -> Arc<SessionState> {
        let id = SessionId::new();
        let now = Utc::now();
        let state = Arc::new(SessionState {
            id,
            name: RwLock::new(String::new()),
            created_at: now,
            last_active_at: Mutex::new(now),
            panes: Mutex::new(HashMap::new()),
        });
        self.sessions.lock().await.insert(id, state.clone());
        // Best-effort: persist the new session row. Errors are non-fatal here
        // because the in-memory registry is the authoritative source in S3.
        if let Err(e) = store.upsert_session(id, "").await {
            tracing::warn!("upsert_session {id}: {e:#}");
        }
        state
    }

    /// Open a new pane inside an existing session. Spawns the PTY.
    pub async fn open_pane(
        &self,
        session_id: SessionId,
        req: OpenPaneReq,
        store: Arc<Store>,
    ) -> Result<Arc<PaneState>> {
        let session = {
            self.sessions
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| anyhow!("no such session {session_id}"))?
        };

        // Convert OpenPaneReq to the SpawnReq shape spawn_pty expects.
        let spawn_req = SpawnReq {
            cols: req.cols,
            rows: req.rows,
            shell: req.shell,
            cwd: req.cwd,
            env: req.env,
        };

        let raw = spawn_pty(spawn_req, session_id, store).await?;
        let pane = Arc::new(raw);

        session.panes.lock().await.insert(pane.id, pane.clone());
        *session.last_active_at.lock().await = Utc::now();

        Ok(pane)
    }

    pub async fn get_session(&self, id: SessionId) -> Option<Arc<SessionState>> {
        self.sessions.lock().await.get(&id).cloned()
    }

    /// Linear scan — acceptable for S3 pane counts.
    pub async fn get_pane(&self, pane: PaneId) -> Option<(Arc<SessionState>, Arc<PaneState>)> {
        let sessions = self.sessions.lock().await;
        for sess in sessions.values() {
            let panes = sess.panes.lock().await;
            if let Some(p) = panes.get(&pane) {
                return Some((sess.clone(), p.clone()));
            }
        }
        None
    }

    pub async fn close_pane(&self, pane_id: PaneId) -> Result<()> {
        let (session, pane) = self
            .get_pane(pane_id)
            .await
            .ok_or_else(|| anyhow!("no such pane {pane_id}"))?;

        pane.kill().await?;
        *pane.closed_at.lock().await = Some(Utc::now());
        session.panes.lock().await.remove(&pane_id);
        Ok(())
    }

    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().await;
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions.values() {
            let pane_count = s.panes.lock().await.len() as u32;
            out.push(SessionInfo {
                id: s.id,
                name: s.name.read().await.clone(),
                pane_count,
                created_at: s.created_at,
                last_active_at: *s.last_active_at.lock().await,
            });
        }
        out
    }

    pub async fn list_panes(&self, session_id: SessionId) -> Vec<PaneInfo> {
        let sessions = self.sessions.lock().await;
        let Some(s) = sessions.get(&session_id) else {
            return vec![];
        };
        let panes = s.panes.lock().await;
        panes
            .values()
            .map(|p| PaneInfo {
                id: p.id,
                session: p.session,
                cols: p.cols,
                rows: p.rows,
                shell: p.shell.clone(),
                created_at: p.created_at,
                closed_at: None, // panes in the map are live; closed ones are removed
            })
            .collect()
    }

    /// Kill and remove all panes for a session. Used by server kill().
    pub async fn kill_session(&self, session_id: SessionId) -> Result<()> {
        let session = {
            self.sessions
                .lock()
                .await
                .remove(&session_id)
                .ok_or_else(|| anyhow!("no such session {session_id}"))?
        };
        let panes: Vec<Arc<PaneState>> = {
            let map = session.panes.lock().await;
            map.values().cloned().collect()
        };
        for p in panes {
            if let Err(e) = p.kill().await {
                tracing::warn!("kill pane {}: {e:#}", p.id);
            }
        }
        Ok(())
    }

    /// Used by shutdown path in main.rs.
    pub async fn all_sessions(&self) -> Vec<Arc<SessionState>> {
        self.sessions.lock().await.values().cloned().collect()
    }
}
