//! pyred configuration loaded from `$XDG_CONFIG_HOME/pyre/config.toml`.
//!
//! The file is optional; all fields have defaults. Unknown keys are ignored.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Process architecture model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessModel {
    /// Single-process daemon (default, v0.1.x back-compat).
    #[default]
    Single,
    /// Supervisor + per-session worker processes (ADR-002 Option C).
    Hybrid,
}

/// `[pyred]` section of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PyredConfig {
    #[serde(default)]
    pub process_model: ProcessModel,
    /// Set to `true` after a successful monolithic → hybrid migration.
    #[serde(default)]
    pub migration_completed: bool,
}

/// Top-level structure of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub pyred: PyredConfig,
}

impl Config {
    /// Load config from `$XDG_CONFIG_HOME/pyre/config.toml`.
    ///
    /// Returns `Config::default()` when the file is absent; propagates errors
    /// only for malformed TOML.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            tracing::debug!("no config file at {}; using defaults", path.display());
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parse config {}", path.display()))?;
        tracing::debug!("loaded config from {}: {:?}", path.display(), cfg);
        Ok(cfg)
    }
}

/// Returns `$XDG_CONFIG_HOME/pyre/config.toml` or the XDG default.
pub fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("pyre").join("config.toml");
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("pyre")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_single() {
        let cfg = Config::default();
        assert_eq!(cfg.pyred.process_model, ProcessModel::Single);
    }

    #[test]
    fn parse_hybrid_process_model() {
        let toml_str = r#"
[pyred]
process_model = "hybrid"
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.pyred.process_model, ProcessModel::Hybrid);
    }

    #[test]
    fn parse_single_process_model() {
        let toml_str = r#"
[pyred]
process_model = "single"
"#;
        let cfg: Config = toml::from_str(toml_str).expect("parse");
        assert_eq!(cfg.pyred.process_model, ProcessModel::Single);
    }

    #[test]
    fn missing_section_uses_defaults() {
        let cfg: Config = toml::from_str("").expect("parse empty");
        assert_eq!(cfg.pyred.process_model, ProcessModel::Single);
        assert!(!cfg.pyred.migration_completed);
    }
}
