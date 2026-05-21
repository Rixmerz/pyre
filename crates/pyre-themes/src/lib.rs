//! pyre-themes — built-in colour palette registry for pyre.
//!
//! Provides 18 palettes covering popular themes (ember, catppuccin, tokyo-night,
//! gruvbox, one-dark, solarized, kanagawa, rose-pine, vesper, nord, dracula,
//! terminal) plus light variants.  The active theme is loaded from
//! `$XDG_CONFIG_HOME/pyre/config.toml` under `[ui] theme = "<name>"`.

pub mod config;
pub mod palettes;

/// 8-bit RGB colour, serialisable and const-constructible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Construct from components.  `const` so palettes can be compile-time constants.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }

    /// Convert to a [`ratatui::style::Color::Rgb`] value.
    pub fn to_ratatui(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(self.0, self.1, self.2)
    }

    /// Return as `[r, g, b, 0xff]` — useful for GPU renderers.
    pub fn to_rgba8(self) -> [u8; 4] {
        [self.0, self.1, self.2, 0xff]
    }

    /// Parse a `#rrggbb` hex string.  Returns `None` on malformed input.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let s = hex.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self(r, g, b))
    }
}

/// Whether a theme is intended for dark or light terminal backgrounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    Dark,
    Light,
}

/// Full colour palette for one theme.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Palette {
    /// Primary background (e.g. editor/body bg).
    pub bg: Rgb,
    /// Dimmed background (inactive panels, overlays).
    pub bg_dim: Rgb,
    /// Primary foreground (body text).
    pub fg: Rgb,
    /// Dimmed foreground (comments, inactive labels).
    pub fg_dim: Rgb,
    /// Unfocused border.
    pub border: Rgb,
    /// Focused / active border.
    pub border_focus: Rgb,
    /// Cursor colour.
    pub cursor: Rgb,
    /// Primary accent (keybinding highlights, active tab bg, etc.).
    pub accent: Rgb,
    /// Success / exit-0 indicator.
    pub ok: Rgb,
    /// Warning indicator.
    pub warn: Rgb,
    /// Error / exit-nonzero indicator.
    pub error: Rgb,
    /// 16-entry ANSI colour table (`[0]`=black … `[15]`=bright-white).
    pub ansi: [Rgb; 16],
}

/// A named theme bundling metadata with its palette.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Machine-readable identifier, e.g. `"catppuccin-mocha"`.
    pub name: &'static str,
    /// Human-readable label shown in the picker.
    pub display_name: &'static str,
    /// Dark or light.
    pub kind: ThemeKind,
    /// Colour values.
    pub palette: Palette,
}

/// Registry of all built-in themes.
pub struct Registry {
    themes: Vec<Theme>,
}

impl Registry {
    /// Construct a registry containing all 18 built-in palettes.
    pub fn builtin() -> Self {
        use palettes::*;

        macro_rules! entry {
            ($mod:ident) => {{
                let (name, display_name, kind) = $mod::META;
                Theme {
                    name,
                    display_name,
                    kind,
                    palette: $mod::PALETTE.clone(),
                }
            }};
        }

        Self {
            themes: vec![
                entry!(ember),
                entry!(catppuccin_mocha),
                entry!(catppuccin_latte),
                entry!(tokyo_night),
                entry!(tokyo_night_light),
                entry!(gruvbox_dark),
                entry!(gruvbox_light),
                entry!(one_dark),
                entry!(one_light),
                entry!(solarized_dark),
                entry!(solarized_light),
                entry!(kanagawa),
                entry!(rose_pine),
                entry!(rose_pine_dawn),
                entry!(vesper),
                entry!(nord),
                entry!(dracula),
                entry!(terminal),
            ],
        }
    }

    /// Look up a theme by its machine-readable name.
    pub fn get(&self, name: &str) -> Option<&Theme> {
        self.themes.iter().find(|t| t.name == name)
    }

    /// All registered themes in display order.
    pub fn list(&self) -> &[Theme] {
        &self.themes
    }

    /// Name of the default theme used when no config entry is present.
    pub fn default_theme() -> &'static str {
        "ember"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn builtin_has_18() {
        assert_eq!(Registry::builtin().list().len(), 18);
    }

    #[test]
    fn ember_is_default() {
        let reg = Registry::builtin();
        assert!(reg.get(Registry::default_theme()).is_some());
    }

    #[test]
    fn hex_roundtrip() {
        let c = Rgb::from_hex("#ff7f3f").unwrap();
        assert_eq!(c, Rgb(0xff, 0x7f, 0x3f));
    }

    #[test]
    fn hex_without_hash() {
        let c = Rgb::from_hex("aabbcc").unwrap();
        assert_eq!(c, Rgb(0xaa, 0xbb, 0xcc));
    }

    #[test]
    fn hex_invalid_returns_none() {
        assert!(Rgb::from_hex("#gg0000").is_none());
        assert!(Rgb::from_hex("#fff").is_none());
    }

    #[test]
    fn every_theme_has_distinct_name() {
        let reg = Registry::builtin();
        let names: HashSet<&str> = reg.list().iter().map(|t| t.name).collect();
        assert_eq!(names.len(), reg.list().len());
    }

    #[test]
    fn to_rgba8_sets_alpha_ff() {
        let c = Rgb::new(0x12, 0x34, 0x56);
        assert_eq!(c.to_rgba8(), [0x12, 0x34, 0x56, 0xff]);
    }
}
