//! Socket path resolution and connection helpers shared across pyre clients.
//!
//! # Canonical socket path
//!
//! `default_socket()` returns the same path all pyre binaries use by default.
//! It mirrors the logic in `pyre-tui/src/main.rs` (the reference copy):
//!
//! 1. `$XDG_RUNTIME_DIR/pyre.sock` when the variable is set.
//! 2. `/tmp/pyre-<uid>.sock` as a fallback.
//!
//! Callers that accept a `--socket` flag or a `PYRE_SOCK` / `PYRE_SOCKET`
//! env override should apply that override **before** calling `default_socket`.
//!
//! # Control-plane connection
//!
//! `connect_control` opens a `UnixStream`, writes the mode-byte + proto-version
//! handshake defined in [`crate::handshake`], and wraps the socket in a tarpc
//! length-delimited bincode transport.  It returns a live `PyreDaemonClient`.
//!
//! This is the no-retry variant used by `pyrec`, `pyre-mcp`, and `pyre-gpu`.
//! The TUI variant (which spawns `pyred` on demand and retries for 5 s) lives
//! in `pyre-tui/src/rpc/client.rs` and is intentionally kept there.
//!
//! # Stream connection
//!
//! `attach_stream` opens a `UnixStream` and writes the `MODE_STREAM` tag
//! followed by the 16-byte `SessionId` UUID and the 16-byte `PaneId` UUID.
//! The caller then owns the raw `UnixStream` and can wrap it in
//! `FramedRead`/`FramedWrite` for the `OutputFrame`/`InputFrame` protocol.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio_util::codec::LengthDelimitedCodec;

use crate::{write_control_client, PaneId, PyreDaemonClient, SessionId, MODE_STREAM};

// ─────────────────────────────────────────────────────────────────────────────
// Socket path
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical default socket path for a pyred daemon.
///
/// Matches `pyre-tui`'s own `default_socket()` exactly:
///
/// 1. `$XDG_RUNTIME_DIR/pyre.sock` when the env var is set.
/// 2. `/tmp/pyre-<uid>.sock` otherwise.
///
/// # Note on env-var overrides
///
/// Callers that support a `PYRE_SOCK`, `PYRE_SOCKET`, or `--socket` override
/// should check those **before** calling `default_socket()`.  This function
/// does not read those variables so that the lookup order remains under the
/// caller's control.
pub fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call on POSIX.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Control-plane connection
// ─────────────────────────────────────────────────────────────────────────────

/// Connect to a running pyred daemon and return a tarpc `PyreDaemonClient`.
///
/// Performs a single connection attempt with no retry.  If the daemon is not
/// running or the socket does not exist this returns an error immediately.
///
/// # Errors
///
/// Returns an error if:
/// - the socket path does not exist or the connection is refused,
/// - the mode-byte + proto-version handshake fails (e.g. version mismatch),
/// - the tarpc transport cannot be established.
pub async fn connect_control(socket: &Path) -> Result<PyreDaemonClient> {
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
// Stream connection
// ─────────────────────────────────────────────────────────────────────────────

/// Open a stream connection for a pane and return the raw socket.
///
/// Writes the `MODE_STREAM` tag byte (`0x02`), then the 16-byte `SessionId`
/// UUID bytes, then the 16-byte `PaneId` UUID bytes (32 bytes total after the
/// tag).  The daemon replies with one synthetic `OutputFrame` carrying the
/// ring-buffer snapshot, after which the connection is a bidirectional
/// length-delimited bincode channel.
///
/// The caller is responsible for wrapping the returned `UnixStream` in
/// `FramedRead`/`FramedWrite` with `LengthDelimitedCodec` and
/// `SymmetricalBincode::<OutputFrame>` / `SymmetricalBincode::<InputFrame>`.
///
/// # Errors
///
/// Returns an error if the socket cannot be connected or if the mode and UUID
/// bytes cannot be written.
pub async fn attach_stream(socket: &Path, session: SessionId, pane: PaneId) -> Result<UnixStream> {
    let mut stream_sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect stream {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;
    Ok(stream_sock)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize env-mutation tests so they don't race on `XDG_RUNTIME_DIR`.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn default_socket_uses_xdg_runtime_dir() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/42");

        let path = default_socket();
        assert_eq!(path, PathBuf::from("/run/user/42/pyre.sock"));

        // Restore.
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn default_socket_falls_back_to_tmp() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::remove_var("XDG_RUNTIME_DIR");

        let path = default_socket();
        let name = path
            .file_name()
            .expect("path must have a filename")
            .to_string_lossy();

        // Must be under /tmp and follow the pyre-<uid>.sock naming.
        assert!(
            path.starts_with("/tmp"),
            "expected /tmp prefix, got {path:?}"
        );
        assert!(
            name.starts_with("pyre-") && name.ends_with(".sock"),
            "expected pyre-<uid>.sock pattern, got {name}"
        );

        // Restore.
        if let Some(v) = saved {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }

    #[test]
    fn default_socket_xdg_takes_priority_over_tmp() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var("XDG_RUNTIME_DIR").ok();
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/99");

        let path = default_socket();
        assert!(
            path.starts_with("/run/user/99"),
            "XDG_RUNTIME_DIR must take priority, got {path:?}"
        );

        // Restore.
        match saved {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }
}
