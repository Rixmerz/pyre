//! Pyre startup fire — full-screen propagation animation before the TUI loads.
//!
//! Uses the shared [`fire_motion`] engine (heat buffer, wind field, palette).
//! Half-block Unicode (`▀`) doubles vertical resolution. Two phases:
//!   Phase A — eruption: bottom seeded hot, adaptive passes until flame tops out.
//!   Phase B — launch: buffer shifts up each frame until the fire clears.
//!
//! Skipped when stdout is not a TTY, `PYRE_NO_SPLASH=1`, or `--no-splash`.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use crate::fire_motion::{self, Rng, MAX_HEAT, PALETTE};

const FRAME_DELAY: Duration = Duration::from_millis(9);
const PHASE_A_END: usize = 14;
const FRAMES_TOTAL: usize = 65;

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

fn run_fire() -> io::Result<()> {
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

                let (fr, fg, fb) = PALETTE[top_heat];
                let (br, bg, bb) = PALETTE[bot_heat];

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
