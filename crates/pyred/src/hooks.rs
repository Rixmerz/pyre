//! Hooks — fire external side-effects on pane state changes.
//!
//! Loads `$XDG_CONFIG_HOME/pyre/hooks.toml` (default `~/.config/pyre/hooks.toml`)
//! at daemon startup. If the file is absent the config is empty and all hooks
//! are no-ops.
//!
//! Schema:
//! ```toml
//! [on_state_change]
//! webhook = "http://localhost:8080/pyre/state"   # optional
//! notify_send = true                              # optional
//! ```

use pyre_proto::{PaneId, PaneStateKind, SessionId};
use serde::Deserialize;

// ─────────────────────────────────────────────────────────────────────────────
// Config types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub on_state_change: OnStateChange,
}

#[derive(Debug, Default, Deserialize)]
pub struct OnStateChange {
    /// HTTP endpoint to POST a JSON state-change payload to.
    #[serde(default)]
    pub webhook: Option<String>,
    /// If true, emit a `notify-send` notification when state → WaitingInput.
    #[serde(default)]
    pub notify_send: bool,
}

impl HooksConfig {
    /// Load from `$XDG_CONFIG_HOME/pyre/hooks.toml` or `~/.config/pyre/hooks.toml`.
    /// Returns an empty (no-op) config if the file does not exist.
    pub fn load() -> Self {
        let path = config_path();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!("hooks.toml read error ({}): {e}", path.display());
                return Self::default();
            }
        };
        match toml::from_str::<HooksConfig>(&content) {
            Ok(cfg) => {
                tracing::info!("hooks loaded from {}", path.display());
                cfg
            }
            Err(e) => {
                tracing::warn!("hooks.toml parse error: {e}");
                Self::default()
            }
        }
    }

    /// Fire all configured hooks for a state-change event. Async, non-panicking.
    pub async fn fire_state_change(
        &self,
        session: SessionId,
        pane: PaneId,
        state: PaneStateKind,
        reason: &str,
    ) {
        let session_short = &session.0.to_string()[..8];
        let pane_short = &pane.0.to_string()[..8];
        tracing::debug!(
            "state change: session={session_short} pane={pane_short} state={state} reason={reason}"
        );

        // Webhook POST.
        if let Some(ref url) = self.on_state_change.webhook {
            let payload = serde_json::json!({
                "session": session.0.to_string(),
                "pane": pane.0.to_string(),
                "state": state.to_string(),
                "reason": reason,
                "ts": chrono::Utc::now().to_rfc3339(),
            });
            let url = url.clone();
            tokio::spawn(async move {
                match reqwest::Client::new()
                    .post(&url)
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            tracing::warn!("webhook {url}: HTTP {}", resp.status());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("webhook {url}: {e}");
                    }
                }
            });
        }

        if self.on_state_change.notify_send && state == PaneStateKind::WaitingInput {
            let pane_short = pane_short.to_string();
            #[cfg(target_os = "linux")]
            tokio::task::spawn_blocking(move || {
                let _ = std::process::Command::new("notify-send")
                    .arg("pyre")
                    .arg(format!("pane {pane_short} waiting for input"))
                    .status();
            });
            #[cfg(target_os = "macos")]
            tokio::task::spawn_blocking(move || {
                let body = format!("pane {pane_short} waiting for input");
                let script = format!(
                    r#"display notification "{}" with title "pyre""#,
                    body.replace('\\', "\\\\").replace('"', "\\\"")
                );
                let _ = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .status();
            });
        }
    }
}

fn config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join(".config")
        });
    base.join("pyre").join("hooks.toml")
}
