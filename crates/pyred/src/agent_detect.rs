//! Heuristic classification of coding agents from foreground process names.

use pyre_proto::AgentKind;

/// Classify a foreground command basename (from `/proc/.../comm`).
pub fn classify_foreground(comm: &str) -> AgentKind {
    let base = comm.trim().to_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);

    match base {
        "claude" | "claude-code" => AgentKind::ClaudeCode,
        "codex" => AgentKind::Codex,
        "pi" => AgentKind::Pi,
        "opencode" => AgentKind::OpenCode,
        "cursor" | "cursor-agent" | "cursor-agent-cli" => AgentKind::CursorAgent,
        "droid" | "factory" => AgentKind::Droid,
        "amp" => AgentKind::Amp,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" => {
            AgentKind::Shell
        }
        _ if base.contains("claude") => AgentKind::ClaudeCode,
        _ if base.contains("codex") => AgentKind::Codex,
        _ => AgentKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_agents() {
        assert_eq!(classify_foreground("claude"), AgentKind::ClaudeCode);
        assert_eq!(classify_foreground("codex"), AgentKind::Codex);
        assert_eq!(classify_foreground("pi"), AgentKind::Pi);
        assert_eq!(classify_foreground("zsh"), AgentKind::Shell);
    }
}
