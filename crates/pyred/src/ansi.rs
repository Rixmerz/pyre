//! Shared ANSI/VT100 helpers used across pyred modules.
//!
//! Centralises the ANSI escape-sequence regex so it is compiled exactly once
//! for the lifetime of the process, regardless of how many code paths call
//! `capture_pane` or similar stripping functions.

use regex::Regex;
use std::sync::LazyLock;

/// Regex that matches CSI (Control Sequence Introducer) escape sequences of
/// the form `ESC [ <param bytes> <final byte>`.  Used to strip ANSI colour
/// and cursor-movement codes from captured PTY output.
///
/// Pattern: `\x1b[` followed by zero or more parameter bytes (0x20–0x3F)
/// then exactly one final byte (0x40–0x7E).
pub static ANSI_CSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    // SAFETY: the regex literal is validated at compile time by construction;
    // panicking here would be a programmer error, not a runtime condition.
    Regex::new(r"\x1b\[[\x20-\x3f]*[\x40-\x7e]").expect("ANSI_CSI_RE is a valid regex")
});
