//! Config helpers for reading and writing the pyre theme selection.
//!
//! Reads/writes `$XDG_CONFIG_HOME/pyre/config.toml` under the `[ui]` section.
//! All other sections are preserved on write via round-trip through `toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[ui.notifications]` section of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// Whether toast notifications are enabled.
    #[serde(default = "default_notifications_enabled")]
    pub enabled: bool,
    /// Time-to-live for each toast in milliseconds.
    #[serde(default = "default_ttl_ms")]
    pub ttl_ms: u64,
    /// Maximum number of toasts visible at once.
    #[serde(default = "default_max_visible")]
    pub max_visible: usize,
}

fn default_notifications_enabled() -> bool {
    true
}

fn default_ttl_ms() -> u64 {
    4000
}

fn default_max_visible() -> usize {
    5
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: default_notifications_enabled(),
            ttl_ms: default_ttl_ms(),
            max_visible: default_max_visible(),
        }
    }
}

/// `[ui]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiConfig {
    /// Theme name, e.g. `"catppuccin-mocha"`. Absent = use registry default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Notification toast settings.
    #[serde(default)]
    pub notifications: NotificationsConfig,
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

/// Load the notification config block from `config.toml`.
/// Returns defaults when the file is absent or the key is not set.
pub fn load_notifications_config() -> Result<NotificationsConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(NotificationsConfig::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
    Ok(cfg.ui.notifications)
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
