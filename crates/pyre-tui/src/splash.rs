//! DOOM-fire splash animation played before the TUI initialises.
//!
//! Uses the canonical DOOM PSX fire algorithm with half-block Unicode rendering
//! (`▀`) to achieve double vertical resolution. Each terminal row renders two
//! heat-buffer rows stacked: foreground = top pixel, background = bottom pixel.
//!
//! Two-phase animation:
//!   Phase A — Eruption (frames 0..PHASE_A_END): bottom seeded MAX_HEAT,
//!             adaptive propagation passes per frame so fire reaches full height
//!             regardless of terminal size.
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
// Extended DOOM fire palette: 49 entries (indices 0-48).
// Index 0 = black (cold), index 48 = white-hot.
// Entries 0-36 match the canonical DOOM palette; 37-48 extend into
// orange-white for the higher MAX_HEAT headroom.

const PALETTE: [(u8, u8, u8); 49] = [
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
    // Extended entries 37-48: orange-white headroom for MAX_HEAT=48
    (239, 239, 215),
    (239, 239, 223),
    (239, 243, 227),
    (243, 243, 231),
    (243, 243, 235),
    (245, 245, 237),
    (247, 247, 239),
    (249, 249, 241),
    (251, 251, 245),
    (253, 253, 249),
    (255, 255, 253),
    (255, 255, 255),
];

/// Maximum heat value. Extended to 48 so the flame retains a visible glow
/// (dark red) even at the very top of a 60-row terminal during peak Phase A.
const MAX_HEAT: u8 = 48;

// ─── Timing & phase constants ─────────────────────────────────────────────────

/// Frame interval (~111 fps).
const FRAME_DELAY: Duration = Duration::from_millis(9);

/// Last frame of Phase A (eruption). Range [0, PHASE_A_END).
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

// ─── Coherent wind field ──────────────────────────────────────────────────────

/// Returns a horizontal wind offset for position (x, y) at the given frame.
///
/// Sums three sines with non-rational frequency ratios so the pattern never
/// repeats and has no bilateral symmetry. Nearby cells receive similar values
/// producing coherent swirls and asymmetric flame tongues.
///
/// Output range: approximately -5..+5 columns.
#[inline]
fn wind_at(x: usize, y: usize, frame: usize, cols: usize) -> i32 {
    let xn = x as f32 / cols.max(1) as f32;
    let yn = y as f32 / 80.0;
    let t = frame as f32 / 22.0;
    let w1 = (xn * 3.0 + t).sin();
    let w2 = (xn * 7.3 + yn * 4.1 - t * 0.7).sin() * 0.7;
    let w3 = (xn * 13.7 - yn * 2.7 + t * 1.3).sin() * 0.4;
    ((w1 + w2 + w3) * 2.8).round() as i32
}

// ─── Per-column updraft field ─────────────────────────────────────────────────

/// Precomputed per-column updraft bias (range -2..+2).
///
/// High-updraft columns cool less so heat climbs further there,
/// producing vertical asymmetry: some columns tower above their neighbours.
fn updraft_field(cols: usize) -> Vec<i32> {
    (0..cols)
        .map(|x| {
            let xn = x as f32 / cols.max(1) as f32;
            let u = (xn * 5.7).sin() * 1.5 + (xn * 11.3 + 1.7).sin() * 1.0;
            u.round() as i32
        })
        .collect()
}

// ─── Propagation helper ───────────────────────────────────────────────────────

/// One full upward-propagation pass over the heat buffer.
///
/// Uses a coherent 2D wind field (`wind_at`) instead of per-cell random jitter
/// so nearby cells see similar horizontal offsets, producing visible swirls.
/// `cooling_base` adds extra cooling on top of the per-cell random amount
/// (used in Phase B to accelerate dissipation as the body rises).
fn propagate(
    heat: &mut [u8],
    cols: usize,
    heat_rows: usize,
    rng: &mut Rng,
    frame: usize,
    cooling_base: u8,
    updraft: &[i32],
) {
    for y in 0..heat_rows.saturating_sub(1) {
        for x in 0..cols {
            // Coherent horizontal offset from wind field.
            let src_x = (x as i32 + wind_at(x, y, frame, cols)).clamp(0, cols as i32 - 1) as usize;

            // Vertical spike: ~12% chance to pull from y+2 instead of y+1.
            let src_y = if y + 2 < heat_rows && rng.range(8) == 0 {
                y + 2
            } else {
                y + 1
            };

            // Per-column updraft reduces effective cooling: high-updraft
            // columns let heat climb further than their neighbours.
            let cooling_adj = (cooling_base as i32 - updraft[x]).max(0) as u8;
            let per_cell_cooling = rng.range(cooling_adj as u32 + 1) as u8;
            let total_cooling = per_cell_cooling.saturating_add(cooling_base);

            let src_heat = heat[src_y * cols + src_x];
            heat[y * cols + x] = src_heat.saturating_sub(total_cooling);
        }
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

    // Adaptive passes per Phase A frame: must cover the full heat_rows height
    // within PHASE_A_END frames. For 60-row terminal (120 heat_rows):
    //   ceil(120 / 14) = 9 passes/frame × 14 frames = 126 steps → tops out.
    // For 24-row terminal (48 heat_rows): 4 passes.
    // For 120-row terminal (240 heat_rows): clamped at 16.
    let passes_per_frame_a = ((heat_rows as f32 / PHASE_A_END as f32).ceil() as usize).clamp(2, 16);

    // Heat buffer: row-major, index = y * cols + x.
    // y=0 is top (cold), y=heat_rows-1 is bottom (seed row).
    let mut heat = vec![0u8; cols * heat_rows];

    // Precompute updraft field (static across all frames).
    let updraft = updraft_field(cols);

    let mut rng = Rng::new();

    // Enter alt screen, hide cursor.
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    out.flush()?;

    for frame in 0..FRAMES_TOTAL {
        if frame < PHASE_A_END {
            // ── Phase A: Eruption ───────────────────────────────────────────
            // Asymmetric bottom seed: hot spots drift across the base over time
            // so different parts of the flame are stronger at different moments.
            let bottom = heat_rows - 1;
            let t = frame as f32 / 18.0;
            for x in 0..cols {
                let xn = x as f32 / cols.max(1) as f32;
                let bias = ((xn * 4.0 + t).sin() * 0.5 + 0.5) * 6.0; // 0..6
                let v = MAX_HEAT.saturating_sub(rng.range(3) as u8 + bias as u8);
                heat[bottom * cols + x] = v;
                if bottom >= 1 {
                    heat[(bottom - 1) * cols + x] = v.saturating_sub(rng.range(2) as u8);
                }
            }

            // Adaptive propagation passes: flame reaches top in PHASE_A_END frames
            // regardless of terminal height.
            for _ in 0..passes_per_frame_a {
                propagate(&mut heat, cols, heat_rows, &mut rng, frame, 0, &updraft);
            }
        } else {
            // ── Phase B: Launch & consume ───────────────────────────────────
            // 1. Translate buffer upward by 1 row.
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
            propagate(
                &mut heat,
                cols,
                heat_rows,
                &mut rng,
                frame,
                cooling_base,
                &updraft,
            );
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
