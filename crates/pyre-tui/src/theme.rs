//! Theme adapter for pyre-tui.
//!
//! Wraps a `pyre_themes::Theme` and exposes the same convenience style-builder
//! methods that the render functions use, so the existing call sites (`T.border()`,
//! `T.tab_active()`, etc.) keep working without modification.

use pyre_themes::Palette;
use ratatui::style::{Color, Modifier, Style};

/// Convenience wrapper around a palette reference, providing style builders.
/// Prepared for Wave 2 GPU consumption; not yet wired into any render path.
// dead_code: planned Wave 2 GPU renderer will consume ThemeRef instead of LegacyTheme
pub struct ThemeRef<'a>(pub &'a Palette);

#[allow(dead_code)]
impl<'a> ThemeRef<'a> {
    fn bg(&self) -> Color {
        self.0.bg.to_ratatui()
    }
    fn fg(&self) -> Color {
        self.0.fg.to_ratatui()
    }
    fn fg_dim(&self) -> Color {
        self.0.fg_dim.to_ratatui()
    }
    fn bg_dim(&self) -> Color {
        self.0.bg_dim.to_ratatui()
    }

    pub fn bg_style(&self) -> Style {
        Style::default().bg(self.bg()).fg(self.fg())
    }

    pub fn title(&self, text_color: Color) -> Style {
        Style::default().fg(text_color).add_modifier(Modifier::BOLD)
    }

    pub fn tab_active(&self) -> Style {
        Style::default()
            .fg(self.bg())
            .bg(self.0.accent.to_ratatui())
            .add_modifier(Modifier::BOLD)
    }

    pub fn tab_inactive(&self) -> Style {
        Style::default().fg(self.fg_dim()).bg(self.bg_dim())
    }

    pub fn status(&self) -> Style {
        Style::default().fg(self.fg()).bg(self.bg_dim())
    }

    pub fn border(&self) -> Style {
        Style::default().fg(self.0.border.to_ratatui())
    }

    pub fn border_focus(&self) -> Style {
        Style::default().fg(self.0.border_focus.to_ratatui())
    }

    pub fn overlay(&self) -> Style {
        Style::default().bg(self.bg_dim()).fg(self.fg())
    }

    pub fn selection(&self) -> Style {
        Style::default()
            .fg(self.bg())
            .bg(self.0.cursor.to_ratatui())
            .add_modifier(Modifier::BOLD)
    }
}

/// Legacy `EMBER` constant — kept so that `fire_motion` module still compiles
/// without changes.  New code should use `ThemeRef` instead.
pub struct LegacyTheme {
    pub bg: Color,
    pub surface: Color,
    pub muted_bg: Color,
    pub border: Color,
    pub border_focus: Color,
    pub primary: Color,
    pub secondary: Color,
    pub spark: Color,
    pub text: Color,
    pub text_dim: Color,
    pub ok: Color,
    pub err: Color,
    pub info: Color,
}

impl LegacyTheme {
    pub fn bg_style(&self) -> Style {
        Style::default().bg(self.bg).fg(self.text)
    }
    pub fn title(&self, text_color: Color) -> Style {
        Style::default().fg(text_color).add_modifier(Modifier::BOLD)
    }
    pub fn tab_active(&self) -> Style {
        Style::default()
            .fg(self.bg)
            .bg(self.primary)
            .add_modifier(Modifier::BOLD)
    }
    pub fn tab_inactive(&self) -> Style {
        Style::default().fg(self.text_dim).bg(self.muted_bg)
    }
    pub fn status(&self) -> Style {
        Style::default().fg(self.text).bg(self.surface)
    }
    pub fn border(&self) -> Style {
        Style::default().fg(self.border)
    }
    pub fn border_focus(&self) -> Style {
        Style::default().fg(self.border_focus)
    }
    pub fn overlay(&self) -> Style {
        Style::default().bg(self.surface).fg(self.text)
    }
    pub fn selection(&self) -> Style {
        Style::default()
            .fg(self.bg)
            .bg(self.spark)
            .add_modifier(Modifier::BOLD)
    }
}

impl LegacyTheme {
    /// Build a `LegacyTheme` from a `pyre_themes::Palette` so dynamic theme
    /// switching works without touching every render callsite.
    pub fn from_palette(p: &pyre_themes::Palette) -> Self {
        // Map semantic palette roles to the legacy field names.
        // `surface` / `muted_bg` have no direct palette equivalent —
        // use bg_dim for both (close enough for border/overlay backgrounds).
        Self {
            bg: p.bg.to_ratatui(),
            surface: p.bg_dim.to_ratatui(),
            muted_bg: p.bg_dim.to_ratatui(),
            border: p.border.to_ratatui(),
            border_focus: p.border_focus.to_ratatui(),
            primary: p.accent.to_ratatui(),
            secondary: p.warn.to_ratatui(),
            spark: p.cursor.to_ratatui(),
            text: p.fg.to_ratatui(),
            text_dim: p.fg_dim.to_ratatui(),
            ok: p.ok.to_ratatui(),
            err: p.error.to_ratatui(),
            info: p.ansi[4].to_ratatui(), // ANSI blue as info proxy
        }
    }
}

/// The Ember palette as a `LegacyTheme`.
/// Retained as a compile-time reference value for tests and future GPU render code.
// dead_code: fire_motion migrated to runtime palette; EMBER kept as a named constant
// for snapshot tests and Wave 2 GPU renderer baseline — do not delete yet.
#[allow(dead_code)]
pub const EMBER: LegacyTheme = LegacyTheme {
    bg: Color::Rgb(0x0d, 0x0a, 0x08),
    surface: Color::Rgb(0x1a, 0x14, 0x10),
    muted_bg: Color::Rgb(0x25, 0x1b, 0x14),
    border: Color::Rgb(0x3d, 0x2a, 0x1f),
    border_focus: Color::Rgb(0xff, 0x6b, 0x35),
    primary: Color::Rgb(0xff, 0x6b, 0x35),
    secondary: Color::Rgb(0xff, 0xb3, 0x47),
    spark: Color::Rgb(0xf7, 0xc9, 0x48),
    text: Color::Rgb(0xe8, 0xe3, 0xd8),
    text_dim: Color::Rgb(0x7a, 0x5c, 0x4a),
    ok: Color::Rgb(0x6d, 0xbf, 0x6a),
    err: Color::Rgb(0xe0, 0x52, 0x4d),
    info: Color::Rgb(0x5a, 0x9f, 0xd4),
};
