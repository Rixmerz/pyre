//! Clipboard helper for pyre-tui.
//!
//! Detects the best available clipboard backend at first call (cached via
//! OnceLock) and uses it for all subsequent calls.  Order of preference:
//!   Linux/Wayland: `wl-copy`
//!   Linux/X11:     `xclip -selection clipboard`
//!   Linux/X11:     `xsel --clipboard --input`  (fallback)
//!   macOS:         `pbcopy`
//!
//! If none are found the function returns an error (logged by callers; no
//! panic).

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Backend {
    WlCopy,
    Xclip,
    Xsel,
    // pbcopy is the macOS clipboard tool; unused on Linux but kept in the
    // same enum to avoid a parallel cfg-gated type.
    #[allow(dead_code)]
    Pbcopy,
}

static BACKEND: OnceLock<Option<Backend>> = OnceLock::new();

fn detect_backend() -> Option<Backend> {
    // On macOS, skip Wayland/X11 probes entirely and go straight to pbcopy.
    #[cfg(target_os = "macos")]
    if which("pbcopy") {
        return Some(Backend::Pbcopy);
    }

    #[cfg(not(target_os = "macos"))]
    for (name, backend) in [
        ("wl-copy", Backend::WlCopy),
        ("xclip", Backend::Xclip),
        ("xsel", Backend::Xsel),
    ] {
        if which(name) {
            return Some(backend);
        }
    }

    None
}

fn which(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Copy `text` to the system clipboard.
///
/// The clipboard backend is detected once at first call and cached.
/// Returns an error if no supported clipboard tool is found or if the
/// child process fails.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let backend = BACKEND
        .get_or_init(detect_backend)
        .ok_or_else(|| anyhow!("no clipboard tool found (tried wl-copy, xclip, xsel, pbcopy)"))?;

    let mut child = match backend {
        Backend::WlCopy => Command::new("wl-copy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
        Backend::Xclip => Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
        Backend::Xsel => Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
        Backend::Pbcopy => Command::new("pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    };

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("clipboard child stdin not captured"))?;
        stdin.write_all(text.as_bytes())?;
    } // drop stdin → EOF → child reads and exits

    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("clipboard tool exited with status {status}"));
    }
    Ok(())
}
