//! Terminal — respects the host terminal's own color scheme.
//! Uses near-black/near-white fallbacks; callers that understand ratatui's
//! Color::Reset can substitute Reset for bg/fg themselves.

use crate::{Palette, Rgb, ThemeKind};

pub const META: (&str, &str, ThemeKind) = ("terminal", "Terminal Default", ThemeKind::Dark);

pub const PALETTE: Palette = Palette {
    // Near-transparent dark bg — callers should use Color::Reset where possible.
    bg: Rgb::new(0x00, 0x00, 0x00),
    bg_dim: Rgb::new(0x12, 0x12, 0x12),
    fg: Rgb::new(0xd0, 0xd0, 0xd0),
    fg_dim: Rgb::new(0x80, 0x80, 0x80),
    border: Rgb::new(0x40, 0x40, 0x40),
    border_focus: Rgb::new(0x00, 0xaf, 0xff),
    cursor: Rgb::new(0xff, 0xff, 0xff),
    accent: Rgb::new(0x00, 0xaf, 0xff),
    ok: Rgb::new(0x00, 0xd7, 0x00),
    warn: Rgb::new(0xff, 0xd7, 0x00),
    error: Rgb::new(0xff, 0x00, 0x00),
    // Standard VGA / xterm 16-color palette.
    ansi: [
        Rgb::new(0x00, 0x00, 0x00), // black
        Rgb::new(0x80, 0x00, 0x00), // red
        Rgb::new(0x00, 0x80, 0x00), // green
        Rgb::new(0x80, 0x80, 0x00), // yellow
        Rgb::new(0x00, 0x00, 0x80), // blue
        Rgb::new(0x80, 0x00, 0x80), // magenta
        Rgb::new(0x00, 0x80, 0x80), // cyan
        Rgb::new(0xc0, 0xc0, 0xc0), // white
        Rgb::new(0x80, 0x80, 0x80), // bright black
        Rgb::new(0xff, 0x00, 0x00), // bright red
        Rgb::new(0x00, 0xff, 0x00), // bright green
        Rgb::new(0xff, 0xff, 0x00), // bright yellow
        Rgb::new(0x00, 0x00, 0xff), // bright blue
        Rgb::new(0xff, 0x00, 0xff), // bright magenta
        Rgb::new(0x00, 0xff, 0xff), // bright cyan
        Rgb::new(0xff, 0xff, 0xff), // bright white
    ],
};
