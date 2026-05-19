//! DOOM-fire splash animation played before the TUI initialises.
//!
//! Uses the canonical DOOM PSX fire algorithm with half-block Unicode rendering
//! (`▀`) to achieve double vertical resolution. Each terminal row renders two
//! heat-buffer rows stacked: foreground = top pixel, background = bottom pixel.
//!
//! Two-phase animation:
//!   Phase A — Eruption (frames 0..PHASE_A_END): bottom seeded MAX_HEAT,
//!             double propagation pass per frame so fire reaches full height fast.
//!   Phase B — Launch & consume (frames PHASE_A_END..FRAMES_TOTAL): seeding
//!             stops; each frame translates the buffer upward by 1 row then
//!             applies cooling propagation, making the bottom clear as the body
//!             rises off the top of the screen.
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

// ─── Timing & phase constants ─────────────────────────────────────────────────

/// Frame interval in milliseconds (~100 fps).
const FRAME_DELAY: Duration = Duration::from_millis(9);

/// Last frame of Phase A (eruption). By this frame the flame should fill ~100%
/// of screen height. Range [0, PHASE_A_END).
const PHASE_A_END: usize = 14;

/// Total frames in the animation. Phase B runs [PHASE_A_END, FRAMES_TOTAL).
const FRAMES_TOTAL: usize = 65;

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

// ─── Propagation helper ───────────────────────────────────────────────────────

/// One full upward-propagation pass over the heat buffer.
///
/// Wind jitter is ±2 cols (wider than original ±1 for turbulence).
/// Occasionally propagates from y+2 instead of y+1 for vertical spikes.
/// `wind_bias` shifts the horizontal jitter left or right each call.
/// `cooling_base` adds extra cooling on top of the per-cell random amount
/// (used in Phase B to accelerate dissipation as the body rises).
fn propagate(
    heat: &mut [u8],
    cols: usize,
    heat_rows: usize,
    rng: &mut Rng,
    wind_bias: i32,
    cooling_base: u8,
) {
    for y in 0..heat_rows.saturating_sub(1) {
        for x in 0..cols {
            // Horizontal jitter: ±2 + wind bias for extra turbulence.
            let jitter = rng.range(5) as i32 - 2 + wind_bias;
            let src_x = (x as i32 + jitter).clamp(0, cols as i32 - 1) as usize;

            // Vertical spike: ~12% chance to pull from y+2 instead of y+1.
            let src_y = if y + 2 < heat_rows && rng.range(8) == 0 {
                y + 2
            } else {
                y + 1
            };

            let per_cell_cooling = rng.range(3) as u8; // 0-2
            let total_cooling = per_cell_cooling.saturating_add(cooling_base);
            let src_heat = heat[src_y * cols + src_x];
            heat[y * cols + x] = src_heat.saturating_sub(total_cooling);
        }
    }
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
        // Wind direction flips every 8 frames (was 15) — more chaotic gusts.
        let wind_bias: i32 = if (frame / 8) % 2 == 0 { 1 } else { -1 };

        if frame < PHASE_A_END {
            // ── Phase A: Eruption ───────────────────────────────────────────
            // Seed the bottom 2 rows at MAX_HEAT with flicker.
            let bottom1 = heat_rows - 1;
            let bottom2 = heat_rows.saturating_sub(2);
            for x in 0..cols {
                let flicker = rng.range(3) as u8; // 0-2
                let val = MAX_HEAT.saturating_sub(flicker.saturating_sub(1));
                heat[bottom1 * cols + x] = val;
                heat[bottom2 * cols + x] = val;
            }

            // Double propagation pass: heat reaches top in ~14 frames instead of ~30.
            propagate(&mut heat, cols, heat_rows, &mut rng, wind_bias, 0);
            propagate(&mut heat, cols, heat_rows, &mut rng, wind_bias, 0);
        } else {
            // ── Phase B: Launch & consume ───────────────────────────────────
            // 1. Translate buffer upward by 1 row.
            //    heat[y] = heat[y+1] for y in 0..total_pixels-1; new bottom = 0.
            heat.copy_within(cols.., 0);
            // Zero the new bottom row (position heat_rows-1).
            let bottom_start = (heat_rows - 1) * cols;
            for cell in &mut heat[bottom_start..] {
                *cell = 0;
            }

            // 2. Apply propagation + increasing cooling so flame dies out.
            //    cooling_base grows from 0 toward 3 over Phase B duration.
            let phase_b_len = FRAMES_TOTAL - PHASE_A_END;
            let phase_b_frame = frame - PHASE_A_END;
            let cooling_base = (phase_b_frame * 3 / phase_b_len) as u8;
            propagate(&mut heat, cols, heat_rows, &mut rng, wind_bias, cooling_base);
        }

        // ── Render frame into a single output buffer ─────────────────────────
        // Each terminal row (tr) renders heat rows tr*2 (top) and tr*2+1 (bottom).
        // Char = '▀', fg = palette[top_heat], bg = palette[bot_heat].
        let mut buf = Vec::with_capacity(cols * term_rows as usize * 26);

        // Move cursor home.
        buf.extend_from_slice(b"\x1b[H");

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

                // fg (38;2) + bg (48;2) + half-block glyph (UTF-8: E2 96 80)
                // Written manually to avoid per-cell String allocation.
                write!(
                    buf,
                    "\x1b[38;2;{fr};{fg};{fb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}"
                )
                .expect("Vec write is infallible");
            }

            // Reset at end of each row, move to next line (avoid scroll on last row).
            if tr + 1 < term_rows as usize {
                buf.extend_from_slice(b"\x1b[0m\r\n");
            }
        }

        buf.extend_from_slice(b"\x1b[0m");

        out.write_all(&buf)?;
        out.flush()?;

        thread::sleep(FRAME_DELAY);
    }

    // Cleanup: show cursor, reset colors, leave alt screen.
    out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
    out.flush()?;

    Ok(())
}
