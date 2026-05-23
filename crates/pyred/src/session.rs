//! Multi-pane session registry for pyred.
//!
//! Each `SessionState` owns a set of `PaneState`s. The registry holds all
//! live sessions and provides the coordination surface used by server.rs.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use pyre_proto::{
    AgentKind, BlockEvent, LayoutNode, OpenPaneReq, Orient, PaneEvent, PaneEventKind, PaneId,
    PaneInfo, PaneStateKind, SessionId, SessionInfo, SpawnReq,
};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use nix::sys::signal::{kill as nix_kill, Signal};

use crate::pty::spawn_pty;
use crate::state::PaneStateTracker;
use crate::store::Store;

pub struct PaneState {
    pub id: PaneId,
    pub session: SessionId,
    /// Optional human-readable label assigned at pane creation time.
    pub name: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub shell: String,
    pub created_at: DateTime<Utc>,
    pub closed_at: Mutex<Option<DateTime<Utc>>>,
    // PTY plumbing — same fields that lived in PtySession.
    pub master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub output_tx: broadcast::Sender<Bytes>,
    #[allow(dead_code)] // phase 6+: stream connections subscribe to block events
    pub events_tx: broadcast::Sender<BlockEvent>,
    pub input_tx: mpsc::Sender<Bytes>,
    #[allow(dead_code)] // Arc is captured by the child-wait task in pty.rs; field keeps it alive
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// OS PID of the child process. Stored separately so `kill()` can send
    /// SIGTERM without acquiring the `child` mutex — which is held by the
    /// child-wait task for the lifetime of the process (blocking on `wait()`).
    /// Acquiring `child` from `kill()` while the wait task holds it would
    /// deadlock: `wait()` blocks until the child exits, but the kill signal
    /// can only be sent after `child.lock()` is acquired.
    pub child_pid: u32,
    pub ringbuf: Arc<StdMutex<crate::ringbuf::RingBuf>>,
    /// State tracker — updated by output path and parser; polled by state engine.
    pub state_tracker: Arc<StdMutex<PaneStateTracker>>,
    /// Cancelled when the child process exits; unblocks stream handlers so they
    /// drop their sockets and clients receive EOF.  Sticky: cancelled() resolves
    /// immediately if already cancelled, eliminating the Notify edge-trigger race.
    pub close_token: CancellationToken,
}

impl PaneState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: PaneId,
        session: SessionId,
        name: Option<String>,
        cols: u16,
        rows: u16,
        shell: String,
        created_at: DateTime<Utc>,
        master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
        output_tx: broadcast::Sender<Bytes>,
        events_tx: broadcast::Sender<BlockEvent>,
        input_tx: mpsc::Sender<Bytes>,
        child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
        child_pid: u32,
        ringbuf: Arc<StdMutex<crate::ringbuf::RingBuf>>,
        state_tracker: Arc<StdMutex<PaneStateTracker>>,
        close_token: CancellationToken,
    ) -> Self {
        Self {
            id,
            session,
            name,
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
            child_pid,
            ringbuf,
            state_tracker,
            close_token,
        }
    }
}

impl PaneState {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.output_tx.subscribe()
    }

    /// Send SIGTERM to the child process.
    ///
    /// Does NOT acquire the `child` mutex.  The child-wait task holds that
    /// mutex for the entire lifetime of the process (blocking inside
    /// `portable_pty::Child::wait()`), so locking it here would deadlock:
    /// `wait()` blocks until the child exits, but the kill signal can only
    /// be delivered after `child.lock()` is acquired.  Instead we use the
    /// stored PID and send the signal directly via nix — which is safe
    /// because the PID is immutable for the lifetime of this `PaneState`.
    pub fn kill(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let pid = nix::unistd::Pid::from_raw(self.child_pid as i32);
            // SIGTERM first; the child-wait task will observe the exit and
            // call remove_pane (which handles session eviction).
            nix_kill(pid, Signal::SIGTERM)
                .map_err(|e| anyhow!("kill(SIGTERM) pid {}: {e}", self.child_pid))?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("kill() not supported on non-unix");
        }
        Ok(())
    }
}

pub struct SessionState {
    pub id: SessionId,
    pub name: RwLock<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: Mutex<DateTime<Utc>>,
    pub panes: Mutex<HashMap<PaneId, Arc<PaneState>>>,
    /// Persisted tiling layout for this session (ADR-0005 M7-C).
    ///
    /// Initialised to `LayoutNode::Leaf(first_pane_id)` when the first pane
    /// is created.  Mutations are serialised through this `Mutex` to prevent
    /// concurrent-split races (ADR-0005 open question #3).
    pub layout: Mutex<Option<LayoutNode>>,
}

/// Ring buffer capacity for pane events.  Large enough that a briefly
/// disconnected client can always catch up; small enough to be free memory.
const EVENT_RING_CAP: usize = 256;

pub struct SessionRegistry {
    sessions: Mutex<HashMap<SessionId, Arc<SessionState>>>,
    /// Broadcast channel: all pane lifecycle events.  Capacity = EVENT_RING_CAP.
    pub event_tx: broadcast::Sender<PaneEvent>,
    /// Ring buffer of the last EVENT_RING_CAP events; new subscribers can
    /// drain history before subscribing to live events.
    pub event_ring: StdMutex<std::collections::VecDeque<PaneEvent>>,
    /// Monotonically increasing sequence counter.  Zero is reserved as the
    /// "no events yet" sentinel for clients, so real events start at 1.
    seq: AtomicU64,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_RING_CAP);
        Self {
            sessions: Mutex::new(HashMap::new()),
            event_tx,
            event_ring: StdMutex::new(std::collections::VecDeque::with_capacity(EVENT_RING_CAP)),
            seq: AtomicU64::new(0),
        }
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Event broadcaster ────────────────────────────────────────────────────

    /// Assign the next sequence number, build a `PaneEvent`, push it into the
    /// ring buffer, and broadcast to all current subscribers.
    pub fn emit_event(
        &self,
        pane_id: PaneId,
        kind: PaneEventKind,
        state: Option<PaneStateKind>,
        agent: Option<AgentKind>,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let ev = PaneEvent {
            seq,
            pane_id,
            kind,
            state,
            agent,
        };
        {
            let mut ring = self.event_ring.lock().expect("event_ring poisoned");
            if ring.len() >= EVENT_RING_CAP {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        // Ignore lagged-receiver errors — they just mean a slow subscriber
        // missed some events and will get them from the ring on reconnect.
        let _ = self.event_tx.send(ev);
    }

    /// Return all events from the ring with seq > `after_seq`, then subscribe
    /// to the live broadcast.  Callers use this to avoid the TOCTOU gap
    /// between draining history and subscribing.
    pub fn events_after(&self, after_seq: u64) -> (Vec<PaneEvent>, broadcast::Receiver<PaneEvent>) {
        // Subscribe first so no live events can be missed between ring drain
        // and subscribing.
        let rx = self.event_tx.subscribe();
        let history: Vec<PaneEvent> = self
            .event_ring
            .lock()
            .expect("event_ring poisoned")
            .iter()
            .filter(|e| e.seq > after_seq)
            .cloned()
            .collect();
        (history, rx)
    }

    // ── Session/pane management ──────────────────────────────────────────────

    pub async fn new_session(&self, store: Arc<Store>, name: Option<String>) -> Arc<SessionState> {
        let id = SessionId::new();
        let short8: String = id.0.to_string().chars().take(8).collect();
        let resolved_name = name
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("session-{short8}"));
        let now = Utc::now();
        let state = Arc::new(SessionState {
            id,
            name: RwLock::new(resolved_name.clone()),
            created_at: now,
            last_active_at: Mutex::new(now),
            panes: Mutex::new(HashMap::new()),
            layout: Mutex::new(None),
        });
        self.sessions.lock().await.insert(id, state.clone());
        // Best-effort: persist the new session row. Errors are non-fatal here
        // because the in-memory registry is the authoritative source in S3.
        if let Err(e) = store.upsert_session(id, &resolved_name).await {
            tracing::warn!("upsert_session {id}: {e:#}");
        }
        state
    }

    /// Open a new pane inside an existing session. Spawns the PTY.
    pub async fn open_pane(
        self: &Arc<Self>,
        session_id: SessionId,
        req: OpenPaneReq,
        store: Arc<Store>,
        block_index: Arc<crate::index::BlockIndex>,
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
        let pane_name = req.name.clone();
        let spawn_req = SpawnReq {
            cols: req.cols,
            rows: req.rows,
            shell: req.shell,
            cwd: req.cwd,
            env: req.env,
            name: None,
        };

        // Read the session name so spawn_pty can re-persist it correctly
        // (instead of overwriting with empty string).
        let session_name_str = session.name.read().await.clone();

        let raw = spawn_pty(
            spawn_req,
            session_id,
            pane_name,
            Some(&session_name_str),
            store.clone(),
            block_index,
            Arc::clone(self),
        )
        .await?;
        let pane = Arc::new(raw);

        session.panes.lock().await.insert(pane.id, pane.clone());
        *session.last_active_at.lock().await = Utc::now();

        // Initialise layout on first pane: Leaf(pane_id).  Subsequent panes
        // are added via open_pane_split which calls split_focused explicitly.
        {
            let mut layout = session.layout.lock().await;
            if layout.is_none() {
                *layout = Some(LayoutNode::Leaf(pane.id));
                let json =
                    serde_json::to_string(layout.as_ref().expect("just set")).unwrap_or_default();
                drop(layout);
                if let Err(e) = store.upsert_session_layout(session.id, &json).await {
                    tracing::warn!("upsert_session_layout {}: {e:#}", session.id);
                }
            }
        }

        // Emit Spawned event *after* the pane is registered so any
        // subscriber that immediately calls list_all_panes will see it.
        let agent = pane
            .state_tracker
            .lock()
            .map(|t| t.agent)
            .unwrap_or(AgentKind::Unknown);
        self.emit_event(
            pane.id,
            PaneEventKind::Spawned,
            Some(PaneStateKind::Running),
            Some(agent),
        );

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

    pub async fn close_pane(&self, pane_id: PaneId, store: Option<&Store>) -> Result<()> {
        let (session, pane) = self
            .get_pane(pane_id)
            .await
            .ok_or_else(|| anyhow!("no such pane {pane_id}"))?;

        pane.kill()?;
        *pane.closed_at.lock().await = Some(Utc::now());
        session.panes.lock().await.remove(&pane_id);

        // Collapse the layout tree. Drop the layout lock before the async
        // SQLite write — same pattern as open_pane / open_pane_split — so we
        // never hold an async mutex guard across an .await point (which would
        // stall any concurrent caller that also needs session.layout).
        //
        // ADR-0005 write-before-broadcast invariant is preserved: we
        // persist first, emit LayoutChanged only after the block exits.
        let (layout_changed, persist_json) = {
            let mut layout = session.layout.lock().await;
            if let Some(ref mut tree) = *layout {
                tree.close(&pane_id);
                let json = serde_json::to_string(tree).unwrap_or_default();
                (true, Some(json))
            } else {
                (false, None)
            }
        };
        // layout lock is now released — safe to await the SQLite write.
        if let (Some(s), Some(json)) = (store, persist_json) {
            if let Err(e) = s.upsert_session_layout(session.id, &json).await {
                tracing::warn!("upsert_session_layout on close {}: {e:#}", session.id);
            }
        }
        if layout_changed {
            self.emit_event(pane_id, PaneEventKind::LayoutChanged, None, None);
        }

        self.evict_session_if_empty(session.id).await;
        self.emit_event(pane_id, PaneEventKind::Closed, None, None);
        Ok(())
    }

    /// Retrieve the current `LayoutNode` for a session, falling back to a
    /// single-leaf layout built from the first live pane if none is set.
    pub async fn get_layout(&self, session_id: SessionId) -> Option<LayoutNode> {
        let session = self.get_session(session_id).await?;
        let layout = session.layout.lock().await;
        if let Some(ref tree) = *layout {
            return Some(tree.clone());
        }
        // Fallback: build Leaf from first pane.
        drop(layout);
        let panes = session.panes.lock().await;
        panes.values().next().map(|p| LayoutNode::Leaf(p.id))
    }

    /// Split `parent_pane` in half, spawn a new sibling pane, update the
    /// session layout, persist, and emit `LayoutChanged`.
    ///
    /// Returns the `PaneId` of the newly created sibling pane.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_pane_split(
        self: &Arc<Self>,
        parent_pane: PaneId,
        orient: Orient,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
        cmd: Option<String>,
        store: Arc<Store>,
        block_index: Arc<crate::index::BlockIndex>,
    ) -> Result<PaneId> {
        let (session, parent_state) = self
            .get_pane(parent_pane)
            .await
            .ok_or_else(|| anyhow!("no such pane {parent_pane}"))?;

        // Build the open-pane request mirroring the parent pane's settings.
        let open_req = OpenPaneReq {
            session: session.id,
            shell: cmd.or_else(|| Some(parent_state.shell.clone())),
            cwd,
            cols: parent_state.cols,
            rows: parent_state.rows,
            env: vec![],
            name,
        };

        // Spawn the new pane (registers it in session.panes).
        let new_pane = self
            .open_pane(session.id, open_req, store.clone(), block_index)
            .await?;

        // Mutate the layout under the mutex — serialize concurrent splits.
        {
            let mut layout = session.layout.lock().await;
            let tree = layout.get_or_insert_with(|| LayoutNode::Leaf(parent_pane));
            tree.split_focused(&parent_pane, new_pane.id, orient);
            let json = serde_json::to_string(tree).unwrap_or_default();
            drop(layout);
            if let Err(e) = store.upsert_session_layout(session.id, &json).await {
                tracing::warn!("upsert_session_layout after split {}: {e:#}", session.id);
            }
        }

        // LayoutChanged must be emitted *after* the SQLite write (ADR-0005 invariant).
        self.emit_event(new_pane.id, PaneEventKind::LayoutChanged, None, None);

        Ok(new_pane.id)
    }

    /// Adjust the weight of the split-child containing `pane`, clamp to
    /// `[5, 95]`, rebalance siblings, persist, emit `LayoutChanged`.
    pub async fn set_pane_weight(&self, pane: PaneId, weight: u16, store: &Store) -> Result<()> {
        let (session, _) = self
            .get_pane(pane)
            .await
            .ok_or_else(|| anyhow!("no such pane {pane}"))?;

        {
            let mut layout = session.layout.lock().await;
            let tree = layout
                .as_mut()
                .ok_or_else(|| anyhow!("session has no layout"))?;
            tree.set_weight(&pane, weight);
            let json = serde_json::to_string(tree).unwrap_or_default();
            drop(layout);
            store.upsert_session_layout(session.id, &json).await?;
        }

        self.emit_event(pane, PaneEventKind::LayoutChanged, None, None);
        Ok(())
    }

    /// Persist a layout JSON string to the store and update the in-memory state.
    #[allow(dead_code)]
    pub async fn persist_layout_json(
        &self,
        session_id: SessionId,
        json: &str,
        store: &Store,
    ) -> Result<()> {
        let session = self
            .get_session(session_id)
            .await
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        let node: LayoutNode =
            serde_json::from_str(json).map_err(|e| anyhow!("invalid layout JSON: {e}"))?;
        *session.layout.lock().await = Some(node);
        store.upsert_session_layout(session_id, json).await
    }

    /// Remove `session_id` from the registry if it has no remaining panes.
    ///
    /// Must be called after the caller has already released `session.panes`
    /// (i.e. the panes Mutex guard is dropped). This function then acquires
    /// `self.sessions` and, while holding it, acquires `session.panes` to
    /// check for emptiness — preserving the same lock-ordering as `remove_pane`
    /// (sessions outer, panes inner).
    async fn evict_session_if_empty(&self, session_id: SessionId) {
        let mut sessions = self.sessions.lock().await;
        let is_empty = match sessions.get(&session_id) {
            Some(s) => s.panes.lock().await.is_empty(),
            None => return,
        };
        if is_empty {
            sessions.remove(&session_id);
            tracing::info!("session {session_id} removed (last pane closed via RPC)");
        }
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
        panes.values().map(pane_info_from_state).collect()
    }

    /// List all panes across all sessions (convenience; avoids N client RPCs).
    pub async fn list_all_panes(&self) -> Vec<PaneInfo> {
        let sessions = self.sessions.lock().await;
        let mut out = Vec::new();
        for s in sessions.values() {
            let panes = s.panes.lock().await;
            for p in panes.values() {
                out.push(pane_info_from_state(p));
            }
        }
        out
    }

    /// Return `(session_id, pane_id, tracker_arc)` for every live pane.
    /// Used by the state engine tick task.
    pub async fn all_trackers(
        &self,
    ) -> Vec<(
        pyre_proto::SessionId,
        pyre_proto::PaneId,
        Arc<StdMutex<PaneStateTracker>>,
    )> {
        let sessions = self.sessions.lock().await;
        let mut out = Vec::new();
        for s in sessions.values() {
            let panes = s.panes.lock().await;
            for p in panes.values() {
                out.push((s.id, p.id, p.state_tracker.clone()));
            }
        }
        out
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
            if let Err(e) = p.kill() {
                tracing::warn!("kill pane {}: {e:#}", p.id);
            }
            self.emit_event(p.id, PaneEventKind::Closed, None, None);
        }
        Ok(())
    }

    /// Remove a pane from its session. If the session has no remaining panes
    /// after removal it is also dropped from the registry.
    /// Called by the child-wait task in pty.rs when the shell exits.
    pub async fn remove_pane(&self, pane_id: PaneId) {
        let mut sessions = self.sessions.lock().await;
        let mut session_to_remove: Option<SessionId> = None;
        for (sid, sess) in sessions.iter() {
            let mut panes = sess.panes.lock().await;
            if panes.remove(&pane_id).is_some() {
                if panes.is_empty() {
                    session_to_remove = Some(*sid);
                }
                break;
            }
        }
        if let Some(sid) = session_to_remove {
            sessions.remove(&sid);
            tracing::info!("session {sid} removed (last pane exited)");
        }
        // Release sessions lock before emitting to avoid holding it across
        // the broadcast send.
        drop(sessions);
        self.emit_event(pane_id, PaneEventKind::Closed, None, None);
    }

    /// Rename a session in-memory and persist to SQLite.
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        name: String,
        store: &Store,
    ) -> anyhow::Result<()> {
        let session = self
            .sessions
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        *session.name.write().await = name.clone();
        store.upsert_session(session_id, &name).await?;
        Ok(())
    }

    /// Used by shutdown path in main.rs.
    pub async fn all_sessions(&self) -> Vec<Arc<SessionState>> {
        self.sessions.lock().await.values().cloned().collect()
    }
}

/// Build a `PaneInfo` from a live `PaneState`, reading the tracker under lock.
fn pane_info_from_state(p: &Arc<PaneState>) -> PaneInfo {
    let (state, reason, last_activity, foreground_cmd, root_pid, agent, seen) = {
        let t = p.state_tracker.lock().expect("tracker poisoned");
        let last_activity = chrono::Utc::now()
            - chrono::Duration::from_std(t.last_output_at.elapsed())
                .unwrap_or(chrono::Duration::zero());
        (
            t.state,
            t.reason.clone(),
            last_activity,
            t.foreground_cmd.clone(),
            t.root_pid,
            t.agent,
            t.seen,
        )
    };
    PaneInfo {
        id: p.id,
        session: p.session,
        cols: p.cols,
        rows: p.rows,
        shell: p.shell.clone(),
        created_at: p.created_at,
        closed_at: None,
        state,
        state_reason: reason,
        last_activity,
        foreground_cmd,
        root_pid,
        agent,
        seen,
        name: p.name.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pyre_proto::{LayoutNode, Orient, PaneId};

    /// A freshly created `SessionState` has no layout until the first pane opens.
    #[tokio::test]
    async fn default_layout_is_none_before_first_pane() {
        let sess = SessionState {
            id: SessionId::new(),
            name: RwLock::new("test".into()),
            created_at: Utc::now(),
            last_active_at: Mutex::new(Utc::now()),
            panes: Mutex::new(HashMap::new()),
            layout: Mutex::new(None),
        };
        let layout = sess.layout.lock().await;
        assert!(layout.is_none(), "layout must be None before first pane");
    }

    /// Initialising layout to a single Leaf then splitting produces the correct
    /// tree without touching the real PTY or store.
    #[tokio::test]
    async fn apply_split_updates_layout_node() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();

        let sess = SessionState {
            id: SessionId::new(),
            name: RwLock::new("test".into()),
            created_at: Utc::now(),
            last_active_at: Mutex::new(Utc::now()),
            panes: Mutex::new(HashMap::new()),
            layout: Mutex::new(Some(LayoutNode::Leaf(pane_a))),
        };

        // Simulate what open_pane_split does: split the focused leaf.
        {
            let mut layout = sess.layout.lock().await;
            let tree = layout.as_mut().expect("layout set");
            tree.split_focused(&pane_a, pane_b, Orient::Vertical);
        }

        let layout = sess.layout.lock().await;
        let tree = layout.as_ref().expect("layout present");
        let vp = pyre_proto::layout::Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 1000,
        };
        let leaves = tree.leaves(vp);
        assert_eq!(leaves.len(), 2, "split should produce 2 leaves");
        assert_eq!(leaves[0].0, pane_a);
        assert_eq!(leaves[1].0, pane_b);
    }

    /// Closing a pane collapses the split back to a single leaf.
    #[tokio::test]
    async fn close_pane_collapses_split() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();

        let mut tree = LayoutNode::VSplit(vec![
            (LayoutNode::Leaf(pane_a), 50),
            (LayoutNode::Leaf(pane_b), 50),
        ]);

        tree.close(&pane_b);

        // After close, the tree should be a single Leaf(pane_a).
        assert!(
            matches!(tree, LayoutNode::Leaf(id) if id == pane_a),
            "closing the second pane should collapse to Leaf(pane_a)"
        );
    }

    /// `set_weight` clamps to [5, 95] and rebalances siblings.
    #[tokio::test]
    async fn set_weight_clamps_and_rebalances() {
        let pane_a = PaneId::new();
        let pane_b = PaneId::new();
        let mut tree = LayoutNode::VSplit(vec![
            (LayoutNode::Leaf(pane_a), 50),
            (LayoutNode::Leaf(pane_b), 50),
        ]);

        tree.set_weight(&pane_a, 80);

        let vp = pyre_proto::layout::Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 1000,
        };
        let leaves = tree.leaves(vp);
        // pane_a gets 80%, pane_b gets 20%.
        assert!(leaves[0].1.w >= 750, "pane_a should be ~800px wide");
        assert!(leaves[1].1.w <= 250, "pane_b should be ~200px wide");
    }

    /// Ghost-leaf prune: closing a dead pane from a VSplit via `LayoutNode::close`
    /// removes only the dead leaf and collapses the split correctly.
    /// This is the same operation performed by the lazy reconcile in
    /// `get_session_layout` (supervisor) and `close_pane` (single-mode).
    #[tokio::test]
    async fn ghost_leaf_prune_collapses_dead_pane() {
        let live = PaneId::new();
        let dead = PaneId::new();

        let mut tree = LayoutNode::VSplit(vec![
            (LayoutNode::Leaf(live), 50),
            (LayoutNode::Leaf(dead), 50),
        ]);

        // Simulate what the lazy reconcile does: close the dead leaf.
        tree.close(&dead);

        // The tree must collapse back to a single Leaf for the live pane.
        assert!(
            matches!(tree, LayoutNode::Leaf(id) if id == live),
            "pruning the dead leaf should collapse to Leaf(live)"
        );

        // After pruning, all_leaves must contain exactly the live pane.
        let remaining = tree.all_leaves();
        assert_eq!(remaining, vec![live], "only the live pane should remain");
        assert!(
            !remaining.contains(&dead),
            "dead pane must not appear in all_leaves after prune"
        );
    }
}
