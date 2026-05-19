//! ASCII flame splash animation played before the TUI initialises.
//!
//! The animation is skipped when:
//! - stdout is not a TTY
//! - `PYRE_NO_SPLASH=1` is set
//! - the `--no-splash` CLI flag was passed
//!
//! All terminal state is restored after the animation: cursor visible,
//! screen cleared, no leftover colour codes.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    style::{Color, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
    ExecutableCommand, QueueableCommand,
};

// ─── Frame data ────────────────────────────────────────────────────────────────
//
// Each frame is a slice of rows (bottom → top order).
// Characters used deliberately avoid box-drawing, use organic fire chars.
// The flame leans slightly right and curls left at the tip — asymmetric.

/// Base flame shape, defined bottom → top.
/// Row 0 is the widest, hottest base; last row is the thin flickering tip.
const BASE_ROWS: &[&str] = &[
    r" ,\((`)),)\  /;((/'`\\ ",   // row 0  – wide base
    r"  ;(`\\\)/'\\\)  ((`)) ",   // row 1
    r"   ((`)),)\ / )\;`     ",   // row 2
    r"    ;( /(/  / )\;`     ",   // row 3
    r"     .)/'(\; (/('      ",   // row 4
    r"      (/('  ,/}'       ",   // row 5
    r"       ,/}' /;/'       ",   // row 6
    r"        /;/' ,/'.      ",   // row 7  – narrowing
    r"         ,/'.  ,       ",   // row 8
    r"           ,           ",   // row 9  – tip
];

/// Per-frame horizontal jitter (column offset added to center, signed).
/// Deterministic so the animation is reproducible.
const JITTER: &[i16] = &[0, 1, -1, 2, 0, -2, 1, -1, 0, 2, -1, 1, 0, -2, 1, 0, -1, 2, 0, 1];

/// Total number of animation frames.
const FRAME_COUNT: usize = 18;

/// Delay between frames (~67 ms → ~18 frames ≈ 1.2 s total).
const FRAME_DELAY: Duration = Duration::from_millis(67);

/// Number of base rows in the flame template.
const FLAME_HEIGHT: usize = BASE_ROWS.len();

// ─── Colour gradient ──────────────────────────────────────────────────────────
//
// Row 0 (base) = brightest white-yellow; row 9 (tip) = dim dark-red.

fn row_color(row_idx: usize) -> Color {
    // row_idx 0 = base (hottest), FLAME_HEIGHT-1 = tip (coolest)
    match row_idx {
        0 => Color::Rgb { r: 255, g: 255, b: 180 }, // white-yellow
        1 => Color::Rgb { r: 255, g: 230, b: 100 }, // pale yellow
        2 => Color::Rgb { r: 255, g: 190, b: 40 },  // amber
        3 => Color::Rgb { r: 255, g: 140, b: 0 },   // orange
        4 => Color::Rgb { r: 230, g: 80, b: 0 },    // deep orange
        5 => Color::Rgb { r: 200, g: 30, b: 0 },    // red-orange
        6 => Color::Rgb { r: 160, g: 10, b: 0 },    // red
        7 => Color::Rgb { r: 110, g: 5, b: 0 },     // dark red
        8 => Color::Rgb { r: 70, g: 3, b: 0 },      // very dark red
        _ => Color::Rgb { r: 40, g: 1, b: 0 },      // near-black red (tip)
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Play the flame splash animation.
///
/// Returns immediately (no-op) when stdout is not a TTY,
/// `PYRE_NO_SPLASH=1` is set, or `no_splash` is `true`.
pub fn play_splash(no_splash: bool) {
    if no_splash {
        return;
    }
    if std::env::var("PYRE_NO_SPLASH").as_deref() == Ok("1") {
        return;
    }
    if !io::stdout().is_terminal() {
        return;
    }

    // Best-effort: if anything goes wrong (narrow terminal, colour failure),
    // just skip the splash rather than crashing.
    let _ = run_animation();
}

// ─── Animation loop ───────────────────────────────────────────────────────────

fn run_animation() -> io::Result<()> {
    let mut out = io::stdout();

    let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));

    // Flame width = length of the widest row.
    let flame_width = BASE_ROWS
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(24) as u16;

    // Centre column for the flame block.
    let base_col = (term_cols.saturating_sub(flame_width)) / 2;

    // The flame base sits at the bottom of the terminal.
    // As frames advance the whole block drifts upward by `frame / 2` rows.
    let base_row_start = term_rows.saturating_sub(FLAME_HEIGHT as u16 + 1);

    out.execute(Hide)?;
    out.queue(Clear(ClearType::All))?;
    out.flush()?;

    for frame in 0..FRAME_COUNT {
        // Vertical shift: flame rises 1 row every 2 frames.
        let rise: u16 = (frame / 2) as u16;

        // Horizontal jitter for this frame.
        let jitter: i16 = JITTER[frame % JITTER.len()];

        // Clear screen for each frame.
        out.queue(Clear(ClearType::All))?;

        // Draw rows bottom→top.
        for (row_idx, row_text) in BASE_ROWS.iter().enumerate() {
            // As the flame rises, bottom rows disappear off-screen.
            // `screen_row` is measured from top (0 = top of terminal).
            let screen_row_signed: i32 = base_row_start as i32
                - rise as i32
                + row_idx as i32;

            if screen_row_signed < 0 || screen_row_signed >= term_rows as i32 {
                continue; // off-screen
            }
            let screen_row = screen_row_signed as u16;

            // Fade: bottom rows thin out as flame lifts (row 0 vanishes first).
            // We skip the bottom rows depending on how far we've risen.
            if rise as usize > 0 && row_idx < rise.saturating_sub(0) as usize {
                continue;
            }

            // Colour for this row.
            let color = row_color(row_idx);

            // Apply jitter to column.
            let col = (base_col as i32 + jitter as i32).max(0) as u16;

            out.queue(MoveTo(col, screen_row))?;
            out.queue(SetForegroundColor(color))?;
            out.queue(crossterm::style::Print(row_text))?;
        }

        out.flush()?;
        thread::sleep(FRAME_DELAY);
    }

    // Clean up: reset colour, clear screen, show cursor.
    out.execute(ResetColor)?;
    out.execute(Clear(ClearType::All))?;
    out.execute(MoveTo(0, 0))?;
    out.execute(Show)?;
    out.flush()?;

    Ok(())
}
