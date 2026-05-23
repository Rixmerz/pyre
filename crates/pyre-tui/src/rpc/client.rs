use std::io::stdout;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use pyre_proto::{write_control_client, PaneId, PyreDaemonClient, SessionId};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::net::UnixStream;
use tokio::process::Command as TokioCommand;
use tokio_util::codec::LengthDelimitedCodec;

// ─────────────────────────────────────────────────────────────────────────────
// Terminal restore guard
// ─────────────────────────────────────────────────────────────────────────────

pub struct TermGuard;

impl TermGuard {
    pub fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon connection helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Connect to the daemon, spawning it if not yet running.
/// Retries every 100 ms for up to 5 s after spawning.
pub async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    // Try connecting first. On ENOENT/ECONNREFUSED, spawn pyred and retry.
    if let Ok(sock) = try_connect_control(socket).await {
        return Ok(sock);
    }

    // Daemon not running — spawn it.
    let pyred_bin = std::env::var("PYRED_BIN").ok().unwrap_or_else(|| {
        // Sibling of current_exe() (e.g. target/release/pyred next to target/release/pyre-tui).
        if let Ok(exe) = std::env::current_exe() {
            let sibling = exe.parent().map(|p| p.join("pyred"));
            if let Some(path) = sibling {
                if path.exists() {
                    return path.to_string_lossy().into_owned();
                }
            }
        }
        "pyred".to_owned()
    });

    tracing::info!(
        "pyred not reachable at {}; spawning {}",
        socket.display(),
        pyred_bin
    );
    let mut child = TokioCommand::new(&pyred_bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        // Inherit stderr so startup errors from pyred are visible in the
        // user's terminal (e.g. Tantivy lock contention, bind failures).
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn pyred binary '{pyred_bin}'"))?;

    // Poll every 100 ms for up to 5 s (50 attempts).
    // If the child exits before the socket becomes ready, surface its exit
    // status immediately rather than waiting out the full timeout.
    let mut last_err = anyhow!("daemon did not come up after 5 s");
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Check whether the child already exited (e.g. crashed on lock).
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(anyhow!(
                    "spawned pyred exited immediately with status {status}; \
                     check stderr for details (Tantivy lock? stale socket?)"
                ));
            }
            Ok(None) => {} // still running — keep polling
            Err(e) => tracing::warn!("try_wait on pyred child: {e}"),
        }

        match try_connect_control(socket).await {
            Ok(client) => return Ok(client),
            Err(e) => last_err = e,
        }
    }
    Err(last_err).with_context(|| {
        format!(
            "spawned pyred but socket {} never became ready; check pyred logs",
            socket.display()
        )
    })
}

/// Single non-retrying connect attempt; wraps the mode-byte handshake.
pub async fn try_connect_control(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    write_control_client(&mut sock).await?;

    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

// ─────────────────────────────────────────────────────────────────────────────
// Session / pane resolution helpers
// ─────────────────────────────────────────────────────────────────────────────

pub async fn resolve_session(client: &PyreDaemonClient, prefix: &str) -> Result<SessionId> {
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no session matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} sessions match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

pub async fn resolve_pane(
    client: &PyreDaemonClient,
    session: SessionId,
    prefix: &str,
) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("no pane matches prefix '{prefix}'")),
        1 => Ok(matches[0].id),
        _ => Err(anyhow!(
            "{} panes match prefix '{prefix}'; provide a longer prefix",
            matches.len()
        )),
    }
}

pub async fn first_pane(client: &PyreDaemonClient, session: SessionId) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc transport")?
        .map_err(|e| anyhow!("daemon list_panes: {e}"))?;

    panes
        .into_iter()
        .next()
        .map(|p| p.id)
        .ok_or_else(|| anyhow!("session has no panes"))
}
