//! Runtime path helpers shared by CLI and TUI.

use std::path::{Path, PathBuf};

/// Directory for ephemeral pyre files next to the control socket.
pub fn runtime_pyre_dir(socket: &Path) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("pyre")
}
