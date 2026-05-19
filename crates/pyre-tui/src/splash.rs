//! DOOM-fire splash animation played before the TUI initialises.
//!
//! Uses the canonical DOOM PSX fire algorithm with half-block Unicode rendering
//! (`▀`) to achieve double vertical resolution. Each terminal row renders two
//! heat-buffer rows stacked: foreground = top pixel, background = bottom pixel.
//!
//! The animation is skipped when:
//! - stdout is not a TTY
//! - `PYRE_NO_SPLASH=1` is set
//! - the `--no-splash` CLI flag was passed
//! - the terminal reports size 0×0

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

// ─── Palette ──────────────────────────────────────────────────────────────────
//
// Literal DOOM fire palette, 37 entries (indices 0-36).
// Index 0 = black (cold), index 36 = white-hot.

const PALETTE: [(u8, u8, u8); 37] = [
    (0, 0, 0),
    (7, 7, 7),
    (31, 7, 7),
    (47, 15, 7),
    (71, 15, 7),
    (87, 23, 7),
    (103, 31, 7),
    (119, 31, 7),
    (143, 39, 7),
    (159, 47, 7),
    (175, 63, 7),
    (191, 71, 7),
    (199, 71, 7),
    (223, 79, 7),
    (223, 87, 7),
    (223, 87, 7),
    (215, 95, 7),
    (215, 95, 7),
    (215, 103, 15),
    (207, 111, 15),
    (207, 119, 15),
    (207, 127, 15),
    (207, 135, 23),
    (199, 135, 23),
    (199, 143, 23),
    (199, 151, 31),
    (191, 159, 31),
    (191, 159, 31),
    (191, 167, 39),
    (191, 167, 39),
    (191, 175, 47),
    (183, 175, 47),
    (183, 183, 47),
    (183, 183, 55),
    (207, 207, 111),
    (223, 223, 159),
    (239, 239, 199),
];

const MAX_HEAT: u8 = 36;

// ─── Timing ───────────────────────────────────────────────────────────────────

const FRAME_DELAY: Duration = Duration::from_millis(14);
const FRAMES_TOTAL: usize = 90;
const FRAMES_RAMPUP: usize = 15;
const FRAMES_SUSTAIN: usize = 65; // sustain ends here, fadeout begins

// ─── Inline xorshift32 RNG (no external deps) ─────────────────────────────────

struct Rng(u32);

impl Rng {
    fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x12345678);
        Self(seed ^ 0xdeadbeef)
    }

    #[inline]
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    #[inline]
    fn range(&mut self, max: u32) -> u32 {
        if max == 0 {
            return 0;
        }
        self.next() % max
    }
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Play the DOOM-fire splash animation.
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
    let _ = run_fire();
}

// ─── Core fire loop ───────────────────────────────────────────────────────────

fn run_fire() -> io::Result<()> {
    let mut out = io::stdout();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if term_cols == 0 || term_rows == 0 {
        return Ok(());
    }

    let cols = term_cols as usize;
    // Each terminal row = 2 heat rows (half-block rendering doubles resolution).
    let heat_rows = term_rows as usize * 2;

    // Heat buffer: row-major, index = y * cols + x.
    // y=0 is top (cold), y=heat_rows-1 is bottom (seed row).
    let mut heat = vec![0u8; cols * heat_rows];

    let mut rng = Rng::new();

    // Enter alt screen, hide cursor.
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    out.flush()?;

    for frame in 0..FRAMES_TOTAL {
        // Determine seed strength for this frame.
        let seed_strength: u8 = if frame < FRAMES_RAMPUP {
            // Ramp up: explode from base.
            let ratio = frame as u32 * MAX_HEAT as u32 / FRAMES_RAMPUP as u32;
            ratio.min(MAX_HEAT as u32) as u8
        } else if frame < FRAMES_SUSTAIN {
            MAX_HEAT
        } else {
            // Fadeout: linearly decay seed to 0.
            let elapsed = frame - FRAMES_SUSTAIN;
            let total = FRAMES_TOTAL - FRAMES_SUSTAIN;
            let decay = elapsed as u32 * MAX_HEAT as u32 / total as u32;
            MAX_HEAT.saturating_sub(decay as u8)
        };

        // Seed bottom two heat rows for a thick base.
        let bottom1 = heat_rows - 1;
        let bottom2 = heat_rows.saturating_sub(2);
        for x in 0..cols {
            let flicker = rng.range(3) as u8; // 0-2
            let val = seed_strength.saturating_sub(flicker.saturating_sub(1));
            heat[bottom1 * cols + x] = val.min(MAX_HEAT);
            heat[bottom2 * cols + x] = val.min(MAX_HEAT);
        }

        // Propagate fire upward (from second-to-last row up to row 0).
        // Wind bias changes direction every ~15 frames for gusts.
        let wind_bias: i32 = if (frame / 15) % 2 == 0 { 1 } else { -1 };
        for y in 0..heat_rows.saturating_sub(1) {
            for x in 0..cols {
                let jitter = rng.range(3) as i32 - 1 + wind_bias;
                let cooling = rng.range(3) as u8; // 0-2
                let src_x = (x as i32 + jitter).clamp(0, cols as i32 - 1) as usize;
                let src_heat = heat[(y + 1) * cols + src_x];
                heat[y * cols + x] = src_heat.saturating_sub(cooling);
            }
        }

        // Render frame into a single buffer.
        // Each terminal row (tr) renders heat rows tr*2 (top) and tr*2+1 (bottom).
        // Char = '▀', fg = palette[top_heat], bg = palette[bot_heat].
        let mut buf = String::with_capacity(cols * term_rows as usize * 32);

        // Move cursor home.
        buf.push_str("\x1b[H");

        for tr in 0..term_rows as usize {
            let top_row = tr * 2;
            let bot_row = tr * 2 + 1;

            for x in 0..cols {
                let top_heat = heat[top_row * cols + x].min(MAX_HEAT) as usize;
                let bot_heat = if bot_row < heat_rows {
                    heat[bot_row * cols + x].min(MAX_HEAT) as usize
                } else {
                    0
                };

                let (fr, fg, fb) = PALETTE[top_heat];
                let (br, bg, bb) = PALETTE[bot_heat];

                // fg (38;2) + bg (48;2) + half-block glyph
                buf.push_str(&format!(
                    "\x1b[38;2;{fr};{fg};{fb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}"
                ));
            }

            // Reset at end of each row, move to next line (avoid scroll on last row).
            if tr + 1 < term_rows as usize {
                buf.push_str("\x1b[0m\r\n");
            }
        }

        buf.push_str("\x1b[0m");

        out.write_all(buf.as_bytes())?;
        out.flush()?;

        thread::sleep(FRAME_DELAY);
    }

    // Cleanup: show cursor, reset colors, leave alt screen.
    out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
    out.flush()?;

    Ok(())
}
