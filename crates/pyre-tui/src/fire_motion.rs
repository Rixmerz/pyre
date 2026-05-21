//! Pyre-native fire motion — palette, propagation helpers, and UI pulse curves.
//!
//! All motion is procedural (sine fields, heat propagation, xorshift RNG). No
//! animation libraries, sprite sheets, or third-party easing crates.
//!
//! The startup splash (`splash.rs`) and in-TUI accents share this module so
//! motion feels like one product, not a generic terminal theme.

use std::time::Instant;

use ratatui::style::{Color, Modifier, Style};

// ─── Heat palette (splash + TUI accents) ─────────────────────────────────────

/// Extended fire palette: index 0 = cold/black, 48 = white-hot.
pub const PALETTE: [(u8, u8, u8); 49] = [
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

pub const MAX_HEAT: u8 = 48;

// ─── RNG (splash propagation) ─────────────────────────────────────────────────

pub struct Rng(u32);

impl Rng {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x12345678);
        Self(seed ^ 0xdeadbeef)
    }

    #[inline]
    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    #[inline]
    pub fn range(&mut self, max: u32) -> u32 {
        if max == 0 {
            return 0;
        }
        self.next() % max
    }
}

// ─── Wind / updraft (coherent flame tongues) ──────────────────────────────────

/// Horizontal wind offset for heat propagation (~ -5..+5 columns).
#[inline]
pub fn wind_at(x: usize, y: usize, frame: usize, cols: usize) -> i32 {
    let xn = x as f32 / cols.max(1) as f32;
    let yn = y as f32 / 80.0;
    let t = frame as f32 / 22.0;
    let w1 = (xn * 3.0 + t).sin();
    let w2 = (xn * 7.3 + yn * 4.1 - t * 0.7).sin() * 0.7;
    let w3 = (xn * 13.7 - yn * 2.7 + t * 1.3).sin() * 0.4;
    ((w1 + w2 + w3) * 2.8).round() as i32
}

pub fn updraft_field(cols: usize) -> Vec<i32> {
    (0..cols)
        .map(|x| {
            let xn = x as f32 / cols.max(1) as f32;
            let u = (xn * 5.7).sin() * 1.5 + (xn * 11.3 + 1.7).sin() * 1.0;
            u.round() as i32
        })
        .collect()
}

pub fn propagate(
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
            let src_x = (x as i32 + wind_at(x, y, frame, cols)).clamp(0, cols as i32 - 1) as usize;
            let src_y = if y + 2 < heat_rows && rng.range(8) == 0 {
                y + 2
            } else {
                y + 1
            };
            let cooling_adj = (cooling_base as i32 - updraft[x]).max(0) as u8;
            let per_cell_cooling = rng.range(cooling_adj as u32 + 1) as u8;
            let total_cooling = per_cell_cooling.saturating_add(cooling_base);
            let src_heat = heat[src_y * cols + src_x];
            heat[y * cols + x] = src_heat.saturating_sub(total_cooling);
        }
    }
}

// ─── TUI motion clock ─────────────────────────────────────────────────────────

/// Monotonic frame counter for in-TUI ember pulses (one tick per redraw).
#[derive(Debug, Clone)]
pub struct AnimClock {
    frame: u64,
    started: Instant,
}

impl AnimClock {
    pub fn new() -> Self {
        Self {
            frame: 0,
            started: Instant::now(),
        }
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    #[allow(dead_code)]
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }
}

/// Phase 0.0..1.0 — desynced per `seed` so neighbours do not pulse in lockstep.
#[inline]
pub fn pulse_phase(frame: u64, seed: u32, speed: f32) -> f32 {
    let t = frame as f32 / speed + seed as f32 * 0.618;
    (t.sin() * 0.5 + 0.5).clamp(0.0, 1.0)
}

#[inline]
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

#[inline]
pub fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::Rgb(
        lerp_u8(a.0, b.0, t),
        lerp_u8(a.1, b.1, t),
        lerp_u8(a.2, b.2, t),
    )
}

/// Map heat index 0..MAX_HEAT to palette color (splash rendering, future ribbon heat).
#[inline]
#[allow(dead_code)]
pub fn color_at_heat(heat: u8) -> Color {
    let i = heat.min(MAX_HEAT) as usize;
    let (r, g, b) = PALETTE[i];
    Color::Rgb(r, g, b)
}

/// Ember border: oscillate between ember orange and spark gold.
pub fn ember_border_style(frame: u64, seed: u32, base: Color, hot: Color) -> Style {
    let p = pulse_phase(frame, seed, 14.0);
    let flicker = pulse_phase(frame.wrapping_add(3), seed ^ 0x9e37, 5.5);
    let t = (p * 0.65 + flicker * 0.35).clamp(0.0, 1.0);
    Style::default().fg(lerp_rgb(rgb_tuple(base), rgb_tuple(hot), t))
}

/// Foreground pulse toward spark (sidebar dots, cursors, titles).
pub fn ember_fg_style(frame: u64, seed: u32, base: Color, accent: Color, strength: f32) -> Style {
    let p = pulse_phase(frame, seed, 12.0);
    let t = (p * strength).clamp(0.0, 1.0);
    Style::default().fg(lerp_rgb(rgb_tuple(base), rgb_tuple(accent), t))
}

pub fn ember_title_style(frame: u64, seed: u32, base: Color, accent: Color) -> Style {
    ember_fg_style(frame, seed, base, accent, 0.85).add_modifier(Modifier::BOLD)
}

#[inline]
pub fn rgb_tuple(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0xff, 0x6b, 0x35),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_phase_bounded() {
        for i in 0..100 {
            let p = pulse_phase(i, 42, 10.0);
            assert!((0.0..=1.0).contains(&p));
        }
    }

    #[test]
    fn wind_is_coherent_not_random() {
        let a = wind_at(10, 5, 0, 80);
        let b = wind_at(11, 5, 0, 80);
        assert!((a - b).abs() <= 3);
    }
}
