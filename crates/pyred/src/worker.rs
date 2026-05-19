//! Worker process entry point (ADR-002 Option C, hybrid mode).
//!
//! The worker is spawned by the supervisor via `pyred --mode worker`.
//! It owns a single session's PTY instances and streams `BlockEvent`s
//! to the supervisor over the `SupervisorWorker` tarpc service.
//!
//! # Lifecycle
//!
//! 1. Read env: `PYRE_SESSION_ID`, `PYRE_WORKER_SOCK`, `PYRE_SUPERVISOR_SOCK`.
//! 2. Open per-session sqlite shard; recover any persisted panes.
//! 3. Register with supervisor via `SupervisorWorker::register_worker`.
//! 4. Bind `WorkerControl` UDS at `PYRE_WORKER_SOCK`; serve requests.
//! 5. Heartbeat supervisor every 5 s.
//! 6. Exit cleanly when `shutdown()` is called or all panes are closed.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use pyre_proto::supervisor::{
    BlockEvent, BlockKind, RpcError, SupervisorWorkerClient, WorkerControl,
};
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

// ---------------------------------------------------------------------------
// Env config
// ---------------------------------------------------------------------------

/// Parsed environment configuration for the worker process.
#[derive(Debug)]
pub struct WorkerEnv {
    pub session_id: String,
    pub worker_sock: PathBuf,
    pub supervisor_sock: PathBuf,
}

impl WorkerEnv {
    /// Read and validate the three required env vars.
    ///
    /// Returns an error (not a panic) when any var is missing so callers can
    /// surface a clean message and exit with a non-zero code.
    pub fn from_env() -> Result<Self> {
        let session_id =
            std::env::var("PYRE_SESSION_ID").context("missing required env var PYRE_SESSION_ID")?;
        if session_id.is_empty() {
            bail!("PYRE_SESSION_ID must not be empty");
        }

        let worker_sock = std::env::var("PYRE_WORKER_SOCK")
            .context("missing required env var PYRE_WORKER_SOCK")?;
        if worker_sock.is_empty() {
            bail!("PYRE_WORKER_SOCK must not be empty");
        }

        let supervisor_sock = std::env::var("PYRE_SUPERVISOR_SOCK")
            .context("missing required env var PYRE_SUPERVISOR_SOCK")?;
        if supervisor_sock.is_empty() {
            bail!("PYRE_SUPERVISOR_SOCK must not be empty");
        }

        Ok(Self {
            session_id,
            worker_sock: PathBuf::from(worker_sock),
            supervisor_sock: PathBuf::from(supervisor_sock),
        })
    }
}

// ---------------------------------------------------------------------------
// Per-pane state
// ---------------------------------------------------------------------------

struct PaneHandle {
    /// OS PID of the shell child.
    child_pid: u32,
    /// PTY master — guarded because resize and the output reader both touch it.
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    /// Channel for writing input bytes into the PTY.
    input_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// Per-session sqlite shard
// ---------------------------------------------------------------------------

struct WorkerShard {
    db: sqlx::SqlitePool,
}

impl WorkerShard {
    async fn open(session_id: &str) -> Result<Self> {
        let state_home = if let Ok(p) = std::env::var("XDG_STATE_HOME") {
            PathBuf::from(p)
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".local")
                .join("state")
        };
        let session_dir = state_home.join("pyre").join("sessions").join(session_id);
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("mkdir {}", session_dir.display()))?;

        let db_path = session_dir.join("state.db");
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(opts)
            .await
            .context("open worker shard sqlite")?;

        // Create schema.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS panes (
                slot_idx INTEGER PRIMARY KEY,
                shell    TEXT NOT NULL,
                cwd      TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .context("create panes table")?;

        Ok(Self { db: pool })
    }

    async fn upsert_pane(&self, slot_idx: u32, shell: &str, cwd: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO panes (slot_idx, shell, cwd) VALUES (?1, ?2, ?3)
             ON CONFLICT(slot_idx) DO UPDATE SET shell = excluded.shell, cwd = excluded.cwd",
        )
        .bind(slot_idx as i64)
        .bind(shell)
        .bind(cwd)
        .execute(&self.db)
        .await
        .context("upsert pane")?;
        Ok(())
    }

    async fn delete_pane(&self, slot_idx: u32) -> Result<()> {
        sqlx::query("DELETE FROM panes WHERE slot_idx = ?1")
            .bind(slot_idx as i64)
            .execute(&self.db)
            .await
            .context("delete pane")?;
        Ok(())
    }

    /// Return the last captured snapshot bytes for `slot_idx`, or empty vec if none.
    async fn load_pane_snapshot(&self, _slot_idx: u32) -> Result<Vec<u8>> {
        // Snapshot persistence is deferred to S3. Return empty bytes for now;
        // capture_pane will return an empty result rather than an error.
        Ok(Vec::new())
    }

    async fn load_panes(&self) -> Result<Vec<(u32, String, String)>> {
        let rows = sqlx::query("SELECT slot_idx, shell, cwd FROM panes")
            .fetch_all(&self.db)
            .await
            .context("load panes")?;
        let mut out = Vec::new();
        for row in rows {
            use sqlx::Row;
            let slot_idx: i64 = row.get("slot_idx");
            let shell: String = row.get("shell");
            let cwd: String = row.get("cwd");
            out.push((slot_idx as u32, shell, cwd));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Worker state shared across tasks
// ---------------------------------------------------------------------------

struct WorkerState {
    session_id: String,
    panes: RwLock<HashMap<u32, PaneHandle>>,
    shard: WorkerShard,
    /// tarpc client back to the supervisor.
    sv_client: SupervisorWorkerClient,
    /// Set to true when shutdown has been requested.
    shutdown_flag: tokio::sync::watch::Sender<bool>,
}

impl WorkerState {
    /// Spawn a PTY for `slot_idx` and register it. Also persists to the shard.
    async fn open_pane(&self, slot_idx: u32, shell: String, cwd: String) -> Result<()> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("openpty: {e}"))?;

        let resolved_shell = if shell.is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
        } else {
            shell.clone()
        };

        let mut cmd = CommandBuilder::new(&resolved_shell);
        if !cwd.is_empty() {
            cmd.cwd(&cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn shell {resolved_shell}"))?;

        let child_pid = child.process_id().unwrap_or(0);

        // Wrap master in Arc<Mutex> for shared resize + input access.
        let master = Arc::new(Mutex::new(pair.master));

        // Input channel: async sender → blocking writer thread.
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let mut writer = master
            .lock()
            .await
            .take_writer()
            .map_err(|e| anyhow::anyhow!("take_writer: {e}"))?;
        std::thread::Builder::new()
            .name(format!("pty-writer-{slot_idx}"))
            .spawn(move || {
                while let Some(chunk) = input_rx.blocking_recv() {
                    if write_all_bytes(&mut writer, &chunk).is_err() {
                        break;
                    }
                }
            })
            .context("spawn pty writer thread")?;

        // Output reader: blocking thread → async output task → supervisor.
        let mut reader = master
            .lock()
            .await
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("clone_reader: {e}"))?;

        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1024);
        std::thread::Builder::new()
            .name(format!("pty-reader-{slot_idx}"))
            .spawn(move || {
                let mut buf = vec![0u8; 4096];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if raw_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .context("spawn pty reader thread")?;

        // Async relay: raw_rx → supervisor block_event (fire-and-forget).
        let sv = self.sv_client.clone();
        let session_id = self.session_id.clone();
        let now_ms = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        };
        tokio::spawn(async move {
            while let Some(bytes) = raw_rx.recv().await {
                let ev = BlockEvent {
                    session_id: session_id.clone(),
                    slot_idx,
                    kind: BlockKind::Stdout,
                    bytes,
                    ts_ms: now_ms(),
                };
                // Fire-and-forget; throttle/batching is supervisor-side.
                let _ = sv.block_event(context::current(), ev).await;
            }
            tracing::info!(slot_idx, "pane output relay ended");
        });

        let handle = PaneHandle {
            child_pid,
            master,
            input_tx,
        };

        self.panes.write().await.insert(slot_idx, handle);
        self.shard.upsert_pane(slot_idx, &shell, &cwd).await?;
        tracing::info!(slot_idx, child_pid, "pane opened");
        Ok(())
    }

    /// Kill the PTY for `slot_idx`, remove from map, persist, and notify supervisor.
    async fn close_pane(&self, slot_idx: u32) -> Result<()> {
        let handle = {
            let mut panes = self.panes.write().await;
            panes.remove(&slot_idx)
        };
        if let Some(h) = handle {
            // SIGTERM the child.
            #[cfg(unix)]
            {
                let pid = nix::unistd::Pid::from_raw(h.child_pid as i32);
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
            }
        }
        self.shard.delete_pane(slot_idx).await?;
        // Notify supervisor.
        let _ = self
            .sv_client
            .pane_closed(context::current(), self.session_id.clone(), slot_idx)
            .await;
        tracing::info!(slot_idx, "pane closed");

        // If no panes remain, signal shutdown.
        if self.panes.read().await.is_empty() {
            tracing::info!("all panes closed — worker exiting");
            let _ = self.shutdown_flag.send(true);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WorkerControl tarpc service implementation
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WorkerControlImpl {
    state: Arc<WorkerState>,
}

impl WorkerControl for WorkerControlImpl {
    async fn shutdown(self, _ctx: context::Context, grace_secs: u32) -> Result<(), RpcError> {
        tracing::info!(grace_secs, "worker: shutdown requested");
        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(grace_secs as u64)).await;
            // SIGTERM all panes.
            let slot_ids: Vec<u32> = state.panes.read().await.keys().cloned().collect();
            for sid in slot_ids {
                let _ = state.close_pane(sid).await;
            }
            let _ = state.shutdown_flag.send(true);
        });
        Ok(())
    }

    async fn attach_pane(
        self,
        _ctx: context::Context,
        slot_idx: u32,
        client_id: String,
    ) -> Result<(), RpcError> {
        let panes = self.state.panes.read().await;
        if panes.contains_key(&slot_idx) {
            tracing::info!(slot_idx, client_id, "pane attached");
            Ok(())
        } else {
            tracing::warn!(slot_idx, "attach_pane: unknown slot");
            Ok(()) // no-op per spec
        }
    }

    async fn resize_pane(
        self,
        _ctx: context::Context,
        slot_idx: u32,
        cols: u16,
        rows: u16,
    ) -> Result<(), RpcError> {
        let panes = self.state.panes.read().await;
        let handle = panes
            .get(&slot_idx)
            .ok_or(RpcError::UnknownSlot(slot_idx))?;
        handle
            .master
            .lock()
            .await
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| RpcError::Internal(format!("resize: {e}")))?;
        Ok(())
    }

    async fn send_keys(
        self,
        _ctx: context::Context,
        slot_idx: u32,
        bytes: Vec<u8>,
    ) -> Result<(), RpcError> {
        let panes = self.state.panes.read().await;
        let handle = panes
            .get(&slot_idx)
            .ok_or(RpcError::UnknownSlot(slot_idx))?;
        handle
            .input_tx
            .send(bytes)
            .await
            .map_err(|_| RpcError::Internal("input_tx closed".into()))?;
        Ok(())
    }

    async fn open_pane(
        self,
        _ctx: context::Context,
        slot_idx: u32,
        shell: String,
        cwd: String,
    ) -> Result<(), RpcError> {
        self.state
            .open_pane(slot_idx, shell, cwd)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))
    }

    async fn close_pane(self, _ctx: context::Context, slot_idx: u32) -> Result<(), RpcError> {
        self.state
            .close_pane(slot_idx)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))
    }

    async fn capture_pane(
        self,
        _ctx: context::Context,
        slot_idx: u32,
        lines: u32,
    ) -> Result<Vec<u8>, RpcError> {
        use regex::Regex;
        use std::sync::OnceLock;

        static ANSI_RE: OnceLock<Regex> = OnceLock::new();
        let re = ANSI_RE.get_or_init(|| {
            Regex::new(r"\x1b\[[\x20-\x3f]*[\x40-\x7e]").expect("static regex is valid")
        });

        // Read recent bytes from the worker shard for this pane.
        let raw = self
            .state
            .shard
            .load_pane_snapshot(slot_idx)
            .await
            .map_err(|e| RpcError::Internal(e.to_string()))?;

        let lossy = String::from_utf8_lossy(&raw);
        let stripped = re.replace_all(&lossy, "");
        let all_lines: Vec<&str> = stripped.split('\n').collect();
        let take = (lines as usize).min(all_lines.len());
        let tail = &all_lines[all_lines.len().saturating_sub(take)..];
        Ok(tail.join("\n").into_bytes())
    }

    async fn list_panes(self, _ctx: context::Context) -> Result<Vec<u32>, RpcError> {
        let panes = self.state.panes.read().await;
        Ok(panes.keys().cloned().collect())
    }
}

// ---------------------------------------------------------------------------
// Helper: write all bytes to a dyn Write without std::io::Write in scope
// ---------------------------------------------------------------------------

fn write_all_bytes(w: &mut dyn std::io::Write, buf: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        let n = w.write(&buf[written..])?;
        written += n;
    }
    w.flush()
}

// ---------------------------------------------------------------------------
// Supervisor connection helper
// ---------------------------------------------------------------------------

async fn connect_supervisor(sock: &PathBuf) -> Result<SupervisorWorkerClient> {
    for attempt in 0..10u32 {
        match UnixStream::connect(sock).await {
            Ok(stream) => {
                let transport = tarpc::serde_transport::new(
                    Framed::new(stream, LengthDelimitedCodec::new()),
                    Bincode::default(),
                );
                return Ok(
                    SupervisorWorkerClient::new(client::Config::default(), transport).spawn(),
                );
            }
            Err(_) if attempt < 9 => {
                tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt + 1))).await;
            }
            Err(e) => return Err(e).context("connect to supervisor"),
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the worker process.
///
/// # Errors
///
/// Returns an error if any required env var is missing, or if the worker
/// socket cannot be bound.
pub async fn run() -> Result<()> {
    let env = WorkerEnv::from_env().context("worker env")?;

    tracing::info!(
        session_id = %env.session_id,
        worker_sock = %env.worker_sock.display(),
        supervisor_sock = %env.supervisor_sock.display(),
        "worker starting"
    );

    // Open per-session shard.
    let shard = WorkerShard::open(&env.session_id)
        .await
        .context("open worker shard")?;

    // Connect to supervisor.
    let sv_client = connect_supervisor(&env.supervisor_sock)
        .await
        .context("connect supervisor")?;

    // Build shutdown watch channel.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let state = Arc::new(WorkerState {
        session_id: env.session_id.clone(),
        panes: RwLock::new(HashMap::new()),
        shard,
        sv_client: sv_client.clone(),
        shutdown_flag: shutdown_tx,
    });

    // --- Respawn recovery: re-open persisted panes ---
    {
        let persisted = state.shard.load_panes().await.unwrap_or_default();
        for (slot_idx, shell, cwd) in persisted {
            tracing::info!(slot_idx, shell, cwd, "recovering persisted pane");
            if let Err(e) = state.open_pane(slot_idx, shell, cwd).await {
                tracing::warn!(slot_idx, "pane recovery failed: {e:#}");
            }
        }
    }

    // --- Register with supervisor ---
    let worker_sock_str = env.worker_sock.to_string_lossy().to_string();
    let pid = std::process::id();

    // Bind WorkerControl socket first so supervisor can connect back on ack.
    if env.worker_sock.exists() {
        let _ = std::fs::remove_file(&env.worker_sock);
    }
    let listener = UnixListener::bind(&env.worker_sock)
        .with_context(|| format!("bind worker sock {}", env.worker_sock.display()))?;
    std::fs::set_permissions(&env.worker_sock, std::fs::Permissions::from_mode(0o700))
        .context("set worker sock perms")?;

    match sv_client
        .register_worker(
            context::current(),
            env.session_id.clone(),
            pid,
            worker_sock_str,
        )
        .await
    {
        Ok(Ok(ack)) => {
            tracing::info!(
                aggregated_index_ready = ack.aggregated_index_ready,
                "registered with supervisor"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!("supervisor rejected registration: {e}");
        }
        Err(e) => {
            tracing::warn!("register_worker RPC failed: {e}");
        }
    }

    // --- Heartbeat task ---
    {
        let sv = sv_client.clone();
        let sid = env.session_id.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                let mut backoff = Duration::from_millis(500);
                for _ in 0..3u32 {
                    match sv.heartbeat(context::current(), sid.clone()).await {
                        Ok(_) => break,
                        Err(e) => {
                            tracing::warn!("heartbeat failed: {e} — retrying");
                            tokio::time::sleep(backoff).await;
                            backoff *= 2;
                        }
                    }
                }
            }
        });
    }

    // --- WorkerControl accept loop ---
    {
        let state_accept = state.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let impl_ = WorkerControlImpl {
                            state: state_accept.clone(),
                        };
                        let transport = tarpc::serde_transport::new(
                            Framed::new(sock, LengthDelimitedCodec::new()),
                            Bincode::default(),
                        );
                        tokio::spawn(
                            BaseChannel::with_defaults(transport)
                                .execute(impl_.serve())
                                .for_each(|f| async move {
                                    tokio::spawn(f);
                                }),
                        );
                    }
                    Err(e) => {
                        tracing::error!("worker accept error: {e}");
                        break;
                    }
                }
            }
        });
    }

    // --- Wait for shutdown signal ---
    shutdown_rx.changed().await.ok();
    tracing::info!("worker shutdown complete");

    let _ = std::fs::remove_file(&env.worker_sock);
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize env-mutating tests: process env is global state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Verify that `WorkerEnv::from_env` fails fast when env vars are absent,
    /// and succeeds when all three are present.
    #[test]
    fn worker_env_fails_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("PYRE_SESSION_ID");
        std::env::remove_var("PYRE_WORKER_SOCK");
        std::env::remove_var("PYRE_SUPERVISOR_SOCK");

        let err = WorkerEnv::from_env();
        assert!(
            err.is_err(),
            "expected error when PYRE_SESSION_ID is missing"
        );
        assert!(
            err.unwrap_err().to_string().contains("PYRE_SESSION_ID"),
            "error should mention the missing var"
        );
    }

    #[test]
    fn worker_env_fails_when_worker_sock_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("PYRE_SESSION_ID");
        std::env::remove_var("PYRE_WORKER_SOCK");
        std::env::remove_var("PYRE_SUPERVISOR_SOCK");

        std::env::set_var("PYRE_SESSION_ID", "test-session");

        let err = WorkerEnv::from_env();
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("PYRE_WORKER_SOCK"));
    }

    #[test]
    fn worker_env_succeeds_when_all_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("PYRE_SESSION_ID");
        std::env::remove_var("PYRE_WORKER_SOCK");
        std::env::remove_var("PYRE_SUPERVISOR_SOCK");

        std::env::set_var("PYRE_SESSION_ID", "abc-123");
        std::env::set_var("PYRE_WORKER_SOCK", "/tmp/test-worker.sock");
        std::env::set_var("PYRE_SUPERVISOR_SOCK", "/tmp/test-supervisor.sock");

        let env = WorkerEnv::from_env().expect("should succeed with all vars present");
        assert_eq!(env.session_id, "abc-123");
        assert_eq!(env.worker_sock, PathBuf::from("/tmp/test-worker.sock"));
        assert_eq!(
            env.supervisor_sock,
            PathBuf::from("/tmp/test-supervisor.sock")
        );
    }
}
