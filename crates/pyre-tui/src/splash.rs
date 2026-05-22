//! Pyre startup fire — full-screen propagation animation before the TUI loads.
//!
//! Uses the shared [`fire_motion`] engine (heat buffer, wind field, palette).
//! Half-block Unicode (`▀`) doubles vertical resolution. Two phases:
//!   Phase A — eruption: bottom seeded hot, adaptive passes until flame tops out.
//!   Phase B — launch: buffer shifts up each frame until the fire clears.
//!
//! Skipped when stdout is not a TTY, `PYRE_NO_SPLASH=1`, or `--no-splash`.
//!
//! The splash color palette is fully derived from the active theme:
//!   - core_hot (heat ≥ 38, white-hot tip)  → `cursor`
//!   - core     (heat 29..37, bright body)   → `border_focus`
//!   - mid      (heat 20..28, mid flame)     → `warn`
//!   - edge     (heat 10..19, outer edge)    → `accent`
//!   - smoke    (heat 3..9,   cool tip/ash)  → `fg_dim`
//!   - bg       (heat 0..2,   cold/black)    → `bg`
//!   - title_fg (title text, unused here)    → `fg`
//!
//! Every heat band blends from the built-in PALETTE base toward the theme
//! color so the transition between bands stays smooth.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use crate::fire_motion::{self, Rng, MAX_HEAT, PALETTE};

const FRAME_DELAY: Duration = Duration::from_millis(9);
const PHASE_A_END: usize = 14;
const FRAMES_TOTAL: usize = 65;

/// Theme-derived color overrides for the splash flame.
///
/// Every field is `(r, g, b)`.  `None` means "use the built-in fire palette
/// entry at that heat level".  All fields default to `None` so the classic
/// ember look is preserved when no theme config is present.
pub struct SplashColors {
    /// Heat ≥ 38 — white-hot tip / spark.  Maps to `cursor`.
    pub core_hot: Option<(u8, u8, u8)>,
    /// Heat 29..37 — bright inner body.    Maps to `border_focus`.
    pub core: Option<(u8, u8, u8)>,
    /// Heat 20..28 — mid-flame band.       Maps to `warn`.
    pub mid: Option<(u8, u8, u8)>,
    /// Heat 10..19 — outer flame edge.     Maps to `accent`.
    pub edge: Option<(u8, u8, u8)>,
    /// Heat 3..9  — cool ash / smoke.      Maps to `fg_dim`.
    pub smoke: Option<(u8, u8, u8)>,
    /// Heat 0..2  — cold background fill.  Maps to `bg`.
    pub bg: Option<(u8, u8, u8)>,
    /// Reserved for future title-text rendering.  Maps to `fg`.
    #[allow(dead_code)]
    pub title_fg: Option<(u8, u8, u8)>,
}

impl SplashColors {
    pub fn from_palette(p: &pyre_themes::Palette) -> Self {
        fn rgb(c: pyre_themes::Rgb) -> (u8, u8, u8) {
            (c.0, c.1, c.2)
        }
        Self {
            core_hot: Some(rgb(p.cursor)),
            core: Some(rgb(p.border_focus)),
            mid: Some(rgb(p.warn)),
            edge: Some(rgb(p.accent)),
            smoke: Some(rgb(p.fg_dim)),
            bg: Some(rgb(p.bg)),
            title_fg: Some(rgb(p.fg)),
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
        core_hot: None,
        core: None,
        mid: None,
        edge: None,
        smoke: None,
        bg: None,
        title_fg: None,
    }));
}

/// Linearly interpolate between `base` (palette default) and `theme` color.
///
/// `t` is in [0.0, 1.0]: 0.0 = pure palette base, 1.0 = pure theme color.
#[inline]
fn blend(base: (u8, u8, u8), theme: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let r = (base.0 as f32 * (1.0 - t) + theme.0 as f32 * t).round() as u8;
    let g = (base.1 as f32 * (1.0 - t) + theme.1 as f32 * t).round() as u8;
    let b = (base.2 as f32 * (1.0 - t) + theme.2 as f32 * t).round() as u8;
    (r, g, b)
}

/// Map a heat index (0..=MAX_HEAT) to an `(r, g, b)` triplet, honoring all
/// theme color overrides across every heat band.
///
/// Band layout (inclusive):
///   ≥ 38         core_hot  — white-hot spark tip
///   29 ..= 37    core      — bright inner body
///   20 ..= 28    mid       — mid-flame band
///   10 ..= 19    edge      — outer flame edge
///    3 ..=  9    smoke     — cool ash / smoke
///    0 ..=  2    bg        — cold background fill
///
/// Each band blends linearly from the built-in PALETTE base toward the theme
/// color so no hard color discontinuities appear at band boundaries.
#[inline]
fn heat_to_rgb(heat: usize, sc: &SplashColors) -> (u8, u8, u8) {
    let idx = heat.min(MAX_HEAT as usize);
    if idx >= 38 {
        // core_hot: white-hot tip.  Full substitution (no blend needed — these
        // are already near-white in the built-in palette).
        sc.core_hot.unwrap_or(PALETTE[idx])
    } else if idx >= 29 {
        // core band: blend 0→1 across indices 29..37.
        if let Some(theme) = sc.core {
            let t = (idx - 29) as f32 / 8.0; // 0.0 at 29, 1.0 at 37
            blend(PALETTE[idx], theme, t)
        } else {
            PALETTE[idx]
        }
    } else if idx >= 20 {
        // mid band: blend 0→1 across indices 20..28.
        if let Some(theme) = sc.mid {
            let t = (idx - 20) as f32 / 8.0; // 0.0 at 20, 1.0 at 28
            blend(PALETTE[idx], theme, t)
        } else {
            PALETTE[idx]
        }
    } else if idx >= 10 {
        // edge band: blend 0→1 across indices 10..19.
        if let Some(theme) = sc.edge {
            let t = (idx - 10) as f32 / 9.0; // 0.0 at 10, 1.0 at 19
            blend(PALETTE[idx], theme, t)
        } else {
            PALETTE[idx]
        }
    } else if idx >= 3 {
        // smoke / ash: blend 0→1 across indices 3..9.
        if let Some(theme) = sc.smoke {
            let t = (idx - 3) as f32 / 6.0; // 0.0 at 3, 1.0 at 9
            blend(PALETTE[idx], theme, t)
        } else {
            PALETTE[idx]
        }
    } else {
        // bg: cold fill (indices 0..2).  Full substitution toward theme bg.
        sc.bg.unwrap_or(PALETTE[idx])
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
