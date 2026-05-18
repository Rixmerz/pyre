//! Ember theme — colour palette and convenience style builders for pyre-tui.

use ratatui::style::{Color, Modifier, Style};

#[allow(dead_code)]
pub struct Theme {
    pub bg: Color,           // #0d0a08  carbon
    pub surface: Color,      // #1a1410  raised surface (overlay bg)
    pub muted_bg: Color,     // #251b14  inactive elements
    pub border: Color,       // #3d2a1f  default border
    pub border_focus: Color, // #ff6b35  ember
    pub primary: Color,      // #ff6b35  ember (accents, focused titles)
    pub secondary: Color,    // #ffb347  flame (highlights)
    pub spark: Color,        // #f7c948  spark (cursor, selection)
    pub text: Color,         // #e8e3d8  primary text
    pub text_dim: Color,     // #7a5c4a  secondary text
    pub ok: Color,           // #6dbf6a  success (exit 0)
    pub err: Color,          // #e0524d  error (exit !=0)
    pub info: Color,         // #5a9fd4  info
}

pub const EMBER: Theme = Theme {
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

impl Theme {
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
