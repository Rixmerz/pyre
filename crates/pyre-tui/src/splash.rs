//! Pyre startup fire — full-screen propagation animation before the TUI loads.
//!
//! Uses the shared [`fire_motion`] engine (heat buffer, wind field, palette).
//! Half-block Unicode (`▀`) doubles vertical resolution. Two phases:
//!   Phase A — eruption: bottom seeded hot, adaptive passes until flame tops out.
//!   Phase B — launch: buffer shifts up each frame until the fire clears.
//!
//! Skipped when stdout is not a TTY, `PYRE_NO_SPLASH=1`, or `--no-splash`.
//!
//! The splash color palette is overridden by the active theme's palette roles:
//!   - Core / mid-flame  → `border_focus` (ember/accent role)
//!   - Tip / hottest     → `cursor`       (spark/bright role)
//!   - Edges / cool      → `warn`         (amber role, used as flame edge)
//!
//! Values below the "edge" threshold still render through the generic fire
//! gradient so black/cold cells look consistent.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use crate::fire_motion::{self, Rng, MAX_HEAT, PALETTE};

const FRAME_DELAY: Duration = Duration::from_millis(9);
const PHASE_A_END: usize = 14;
const FRAMES_TOTAL: usize = 65;

/// Theme-derived color overrides for the splash flame.
///
/// Each field is `(r, g, b)`. `None` means "use the built-in fire palette
/// entry at that heat level". All three default to `None` so the classic
/// ember look is preserved when no theme config is present.
pub struct SplashColors {
    /// Color applied at heat levels ≥ 38 (hottest tip / white-hot).
    pub tip: Option<(u8, u8, u8)>,
    /// Color applied at heat levels 20..37 (bright core).
    pub core: Option<(u8, u8, u8)>,
    /// Color applied at heat levels 8..19 (outer edges).
    pub edge: Option<(u8, u8, u8)>,
}

impl SplashColors {
    pub fn from_palette(p: &pyre_themes::Palette) -> Self {
        fn rgb(c: pyre_themes::Rgb) -> (u8, u8, u8) {
            (c.0, c.1, c.2)
        }
        Self {
            tip: Some(rgb(p.cursor)),
            core: Some(rgb(p.border_focus)),
            edge: Some(rgb(p.warn)),
        }
    }
}

pub fn play_splash(no_splash: bool, colors: Option<SplashColors>) {
    if no_splash {
        return;
    }
    if std::env::var("PYRE_NO_SPLASH").as_deref() == Ok("1") {
        return;
    }
    if !io::stdout().is_terminal() {
        return;
    }
    let _ = run_fire(colors.unwrap_or(SplashColors {
        tip: None,
        core: None,
        edge: None,
    }));
}

/// Map a heat index (0..=MAX_HEAT) to an `(r, g, b)` triplet, honoring any
/// theme color overrides for the tip / core / edge heat bands.
#[inline]
fn heat_to_rgb(heat: usize, sc: &SplashColors) -> (u8, u8, u8) {
    let idx = heat.min(MAX_HEAT as usize);
    if idx >= 38 {
        sc.tip.unwrap_or(PALETTE[idx])
    } else if idx >= 20 {
        // Blend palette base toward theme core color so the gradient is smooth.
        if let Some((tr, tg, tb)) = sc.core {
            let base = PALETTE[idx];
            let blend = (idx - 20) as f32 / 18.0; // 0.0 at 20, 1.0 at 38
            let r = (base.0 as f32 * (1.0 - blend) + tr as f32 * blend).round() as u8;
            let g = (base.1 as f32 * (1.0 - blend) + tg as f32 * blend).round() as u8;
            let b = (base.2 as f32 * (1.0 - blend) + tb as f32 * blend).round() as u8;
            (r, g, b)
        } else {
            PALETTE[idx]
        }
    } else if idx >= 8 {
        if let Some((tr, tg, tb)) = sc.edge {
            let base = PALETTE[idx];
            let blend = (idx - 8) as f32 / 12.0; // 0.0 at 8, 1.0 at 20
            let r = (base.0 as f32 * (1.0 - blend) + tr as f32 * blend).round() as u8;
            let g = (base.1 as f32 * (1.0 - blend) + tg as f32 * blend).round() as u8;
            let b = (base.2 as f32 * (1.0 - blend) + tb as f32 * blend).round() as u8;
            (r, g, b)
        } else {
            PALETTE[idx]
        }
    } else {
        // Cold / black cells — always use the built-in dark entries.
        PALETTE[idx]
    }
}

fn run_fire(sc: SplashColors) -> io::Result<()> {
    let mut out = io::stdout();

    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if term_cols == 0 || term_rows == 0 {
        return Ok(());
    }

    let cols = term_cols as usize;
    let heat_rows = term_rows as usize * 2;
    let passes_per_frame_a = ((heat_rows as f32 / PHASE_A_END as f32).ceil() as usize).clamp(2, 16);

    let mut heat = vec![0u8; cols * heat_rows];
    let updraft = fire_motion::updraft_field(cols);
    let mut rng = Rng::new();

    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    out.flush()?;

    for frame in 0..FRAMES_TOTAL {
        if frame < PHASE_A_END {
            let bottom = heat_rows - 1;
            let t = frame as f32 / 18.0;
            for x in 0..cols {
                let xn = x as f32 / cols.max(1) as f32;
                let bias = ((xn * 4.0 + t).sin() * 0.5 + 0.5) * 6.0;
                let v = MAX_HEAT.saturating_sub(rng.range(3) as u8 + bias as u8);
                heat[bottom * cols + x] = v;
                if bottom >= 1 {
                    heat[(bottom - 1) * cols + x] = v.saturating_sub(rng.range(2) as u8);
                }
            }
            for _ in 0..passes_per_frame_a {
                fire_motion::propagate(&mut heat, cols, heat_rows, &mut rng, frame, 0, &updraft);
            }
        } else {
            heat.copy_within(cols.., 0);
            let bottom_start = (heat_rows - 1) * cols;
            for cell in &mut heat[bottom_start..] {
                *cell = 0;
            }
            let phase_b_len = FRAMES_TOTAL - PHASE_A_END;
            let phase_b_frame = frame - PHASE_A_END;
            let cooling_base = (phase_b_frame * 3 / phase_b_len) as u8;
            fire_motion::propagate(
                &mut heat,
                cols,
                heat_rows,
                &mut rng,
                frame,
                cooling_base,
                &updraft,
            );
        }

        let mut buf = Vec::with_capacity(cols * term_rows as usize * 26);
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

                let (fr, fg, fb) = heat_to_rgb(top_heat, &sc);
                let (br, bg, bb) = heat_to_rgb(bot_heat, &sc);

                write!(
                    buf,
                    "\x1b[38;2;{fr};{fg};{fb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}"
                )
                .expect("Vec write is infallible");
            }

            if tr + 1 < term_rows as usize {
                buf.extend_from_slice(b"\x1b[0m\r\n");
            }
        }

        buf.extend_from_slice(b"\x1b[0m");
        out.write_all(&buf)?;
        out.flush()?;
        thread::sleep(FRAME_DELAY);
    }

    out.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    Ok(())
}
