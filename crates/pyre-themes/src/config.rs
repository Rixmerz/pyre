//! Config helpers for reading and writing the pyre theme selection.
//!
//! Reads/writes `$XDG_CONFIG_HOME/pyre/config.toml` under the `[ui]` section.
//! All other sections are preserved on write via round-trip through `toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[ui]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Theme name, e.g. `"catppuccin-mocha"`. Absent = use registry default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

/// Minimal top-level config shape — only the `[ui]` section is managed here.
/// All other keys are captured in `extra` and written back verbatim so we do
/// not clobber pyred's `[pyred]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ui: UiConfig,

    /// Catch-all for any keys we do not own (e.g. `[pyred]`).
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// Returns `$XDG_CONFIG_HOME/pyre/config.toml` using XDG conventions.
pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("pyre").join("config.toml");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("pyre")
        .join("config.toml")
}

/// Load the theme name from config.  Returns `None` when the file is absent
/// or the key is not set.
pub fn load_theme_name() -> Result<Option<String>> {
    let path = config_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
    Ok(cfg.ui.theme)
}

/// Persist a theme name to config, preserving all other keys.
pub fn save_theme_name(name: &str) -> Result<()> {
    let path = config_path();

    // Read existing config to preserve other sections.
    let mut cfg = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        toml::from_str::<Config>(&raw)
            .with_context(|| format!("parse config {}", path.display()))?
    } else {
        Config::default()
    };

    cfg.ui.theme = Some(name.to_owned());

    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }

    let serialized = toml::to_string_pretty(&cfg).context("serialize config")?;
    std::fs::write(&path, serialized)
        .with_context(|| format!("write config {}", path.display()))?;

    Ok(())
}
