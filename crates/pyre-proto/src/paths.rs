//! Runtime path helpers shared by CLI and TUI.

use std::path::{Path, PathBuf};

/// Directory for ephemeral pyre files next to the control socket.
pub fn runtime_pyre_dir(socket: &Path) -> PathBuf {
    socket
        .parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("pyre")
}

/// `pyrec select-pane` writes this file; `pyre` TUI consumes and deletes it.
pub fn focus_request_path(socket: &Path) -> PathBuf {
    runtime_pyre_dir(socket).join("focus.request")
}
