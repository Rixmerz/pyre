//! Render smoke tests — Wave 5 harness scaffold.
//!
//! These tests exercise the ratatui [`TestBackend`] infrastructure used by the
//! pyre-tui render layer. Full integration tests against `draw_frame` and the
//! narrowed render functions (`render_session_strip`, `render_toast_deck`, etc.)
//! require those functions to be accessible from the library root (`lib.rs`).
//! That migration lands in Wave 5 of the refactor plan (see `docs/INVARIANTS.md`
//! and `.claude/notions/refactor-plan-v04.md` section 6).
//!
//! Until then, this file:
//!   1. Verifies the TestBackend + Terminal round-trip compiles and works.
//!   2. Provides the snapshot fixture infrastructure via `insta`.
//!   3. Serves as the integration test entry point CI runs with:
//!      `cargo test -p pyre-tui --test render_smoke`
//!
//! When Wave 5 moves render functions into lib.rs, replace the placeholder
//! assertions below with `draw_frame` + `insta::assert_snapshot!` calls.

use insta::assert_snapshot;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Build a fresh 80x24 TestBackend terminal — the canonical pyre smoke size.
fn test_terminal() -> Terminal<TestBackend> {
    let backend = TestBackend::new(80, 24);
    Terminal::new(backend).expect("TestBackend terminal must construct without error")
}

// ─────────────────────────────────────────────────────────────────────────────
// Smoke tests
// ─────────────────────────────────────────────────────────────────────────────

/// Verify that the TestBackend + Terminal round-trip works end-to-end.
///
/// Renders a single Paragraph widget containing "pyre" and asserts the backend
/// buffer captures it. This is the baseline that all future render tests build on.
#[test]
fn test_backend_renders_paragraph() {
    let mut term = test_terminal();

    term.draw(|frame| {
        let area = Rect::new(0, 0, 6, 1);
        frame.render_widget(
            Paragraph::new(Span::styled("pyre", Style::default().fg(Color::White))),
            area,
        );
    })
    .expect("draw must not fail on TestBackend");

    let buf = terminal_buffer_string(&term);
    assert!(
        buf.contains("pyre"),
        "backend buffer must contain rendered text 'pyre'; got: {buf}"
    );
}

/// Snapshot the empty 80x24 frame to pin the baseline buffer shape.
///
/// `insta` will create `tests/snapshots/render_smoke__empty_frame.snap` on first
/// run. Subsequent runs assert against it. Run `cargo insta review` to accept
/// new snapshots after intentional layout changes.
#[test]
fn test_snapshot_empty_frame() {
    let mut term = test_terminal();

    // Draw an empty frame — no widgets. Snapshot the buffer topology.
    term.draw(|_frame| {}).expect("empty draw must succeed");

    let buf = terminal_buffer_string(&term);
    assert_snapshot!("empty_frame", buf);
}

/// Verify that a styled "Loading…" placeholder renders without panic.
///
/// This mirrors what the TUI bootstrap screen shows while waiting for the first
/// daemon response. The exact text and position will change when the real
/// splash / session-list logic is wired; update the snapshot at that point.
#[test]
fn test_loading_placeholder_renders() {
    let mut term = test_terminal();

    term.draw(|frame| {
        let area = frame.area();
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Loading…",
                Style::default().fg(Color::DarkGray),
            )),
            area,
        );
    })
    .expect("loading placeholder draw must succeed");

    let buf = terminal_buffer_string(&term);
    assert!(
        buf.contains("Loading"),
        "buffer must contain 'Loading' text; got: {buf}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return a deterministic string representation of the terminal backend's buffer.
///
/// `TestBackend::buffer()` returns a `Buffer` whose `Debug` impl lists all cells.
/// We convert it to a flat cell-content string instead, which is more readable
/// in snapshots and assertion failure messages.
fn terminal_buffer_string(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    buf.content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}
