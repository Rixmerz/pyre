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

// ─────────────────────────────────────────────────────────────────────────────
// Tests — ANSI_CSI_RE stripping behaviour
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::ANSI_CSI_RE;

    /// Plain text with no escape sequences must be left unchanged.
    ///
    /// Guards that the regex does not accidentally eat ordinary characters
    /// when applied to output that has already been cleaned.
    #[test]
    fn plain_text_is_preserved_unchanged() {
        let input = "hello, world!\n$ ";
        let result = ANSI_CSI_RE.replace_all(input, "");
        assert_eq!(
            result.as_ref(),
            input,
            "ANSI_CSI_RE must not alter plain text; got: {result:?}"
        );
    }

    /// A single SGR colour-reset sequence (`ESC [ 0 m`) must be fully stripped.
    ///
    /// This is the most common sequence produced by shells and CLI tools; its
    /// removal is the primary use-case for the regex.
    #[test]
    fn sgr_colour_reset_is_stripped() {
        let input = "\x1b[0mhello";
        let result = ANSI_CSI_RE.replace_all(input, "");
        assert_eq!(
            result.as_ref(),
            "hello",
            "SGR reset must be stripped; got: {result:?}"
        );
    }

    /// Multiple mixed CSI sequences embedded in a line must all be removed.
    ///
    /// Covers a realistic PTY output fragment: bold-red prefix, content, reset.
    #[test]
    fn multiple_csi_sequences_in_line_are_all_stripped() {
        // ESC[1;31m = bold red, ESC[0m = reset
        let input = "\x1b[1;31merror\x1b[0m: something failed";
        let result = ANSI_CSI_RE.replace_all(input, "");
        assert_eq!(
            result.as_ref(),
            "error: something failed",
            "all CSI sequences must be stripped; got: {result:?}"
        );
    }

    /// A cursor-movement sequence (`ESC [ 2 J`, erase display) is a non-SGR
    /// CSI sequence; it must also be stripped.
    #[test]
    fn cursor_movement_sequence_is_stripped() {
        // ESC[2J = Erase Display
        let input = "\x1b[2Jsome output";
        let result = ANSI_CSI_RE.replace_all(input, "");
        assert_eq!(
            result.as_ref(),
            "some output",
            "cursor-movement CSI must be stripped; got: {result:?}"
        );
    }

    /// A string with no escape byte at all must survive the replace unchanged
    /// (fast-path, avoids any false match inside 7-bit ASCII range).
    #[test]
    fn string_without_escape_byte_is_unchanged() {
        let input = "pyre v0.4.0 — terminal emulator";
        let result = ANSI_CSI_RE.replace_all(input, "");
        assert_eq!(result.as_ref(), input);
    }
}
