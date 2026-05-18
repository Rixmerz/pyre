//! Pane state tracker — heuristic + self-report lifecycle engine for S5b.
//!
//! Each live pane owns a `PaneStateTracker` (wrapped in `Arc<StdMutex>`).
//! A daemon-wide async task (`spawn_state_engine`) polls all trackers every
//! 500 ms and transitions state according to the heuristic rules below.
//!
//! OSC 133 markers are fed in from the parser task via `push_marker`.
//! Raw output bytes update `last_output_at` via `touch_output`.
//! `set_override` locks the state for a configurable window before the
//! heuristic resumes.

use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;

use pyre_proto::PaneStateKind;

// ─────────────────────────────────────────────────────────────────────────────
// OSC 133 marker enum
// ─────────────────────────────────────────────────────────────────────────────

/// Shell-integration marker values from OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Osc133Marker {
    /// A — prompt start.
    A,
    /// B — command start (pre-execution).
    B,
    /// C — output start (command accepted, running).
    C,
    /// D — block end (command finished).
    D { exit_code: Option<i32> },
}

// ─────────────────────────────────────────────────────────────────────────────
// Known sets
// ─────────────────────────────────────────────────────────────────────────────

static INTERACTIVE_SET: &[&str] = &[
    "vim",
    "nvim",
    "vi",
    "less",
    "more",
    "top",
    "htop",
    "btop",
    "man",
    "nano",
    "emacs",
    "emacs-nox",
    "helix",
    "hx",
    "lazygit",
    "fzf",
    "ranger",
    "nnn",
    "mc",
];

static SHELL_SET: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh"];

// ─────────────────────────────────────────────────────────────────────────────
// PaneStateTracker
// ─────────────────────────────────────────────────────────────────────────────

pub struct PaneStateTracker {
    pub state: PaneStateKind,
    pub reason: String,
    pub last_output_at: Instant,
    pub last_marker: Option<Osc133Marker>,
    /// Wall-clock instant the last D marker was received.
    pub last_d_at: Option<Instant>,
    pub foreground_cmd: Option<String>,
    pub root_pid: u32,
    /// Override: if `Some`, ignore heuristic until this instant.
    pub override_until: Option<Instant>,
    pub overridden_state: Option<PaneStateKind>,
    pub overridden_reason: Option<String>,
    /// Watch channel: receivers subscribe to state changes.
    pub watch_tx: watch::Sender<PaneStateKind>,
}

impl PaneStateTracker {
    pub fn new(root_pid: u32) -> (Self, watch::Receiver<PaneStateKind>) {
        let (tx, rx) = watch::channel(PaneStateKind::Running);
        let tracker = Self {
            state: PaneStateKind::Running,
            reason: "init".to_string(),
            last_output_at: Instant::now(),
            last_marker: None,
            last_d_at: None,
            foreground_cmd: None,
            root_pid,
            override_until: None,
            overridden_state: None,
            overridden_reason: None,
            watch_tx: tx,
        };
        (tracker, rx)
    }

    /// Called whenever raw bytes arrive from the PTY reader thread.
    pub fn touch_output(&mut self) {
        self.last_output_at = Instant::now();
    }

    /// Called by the parser when an OSC 133 marker is parsed.
    pub fn push_marker(&mut self, marker: Osc133Marker) {
        if matches!(marker, Osc133Marker::D { .. }) {
            self.last_d_at = Some(Instant::now());
        }
        self.last_marker = Some(marker);
    }

    /// Set an override that expires after `secs` seconds.
    pub fn set_override(&mut self, state: PaneStateKind, reason: String, secs: u64) {
        self.overridden_state = Some(state);
        self.overridden_reason = Some(reason);
        self.override_until = Some(Instant::now() + std::time::Duration::from_secs(secs));
    }

    /// Evaluate the current state given the heuristic rules. Returns `true` if
    /// the state changed (so callers can fire hooks / watch notifications).
    pub fn evaluate(&mut self) -> bool {
        // Override window takes priority.
        if let Some(until) = self.override_until {
            if Instant::now() < until {
                let s = self.overridden_state.unwrap_or(PaneStateKind::Running);
                let r = self
                    .overridden_reason
                    .clone()
                    .unwrap_or_else(|| "override".to_string());
                return self.apply(s, r);
            } else {
                // Override expired — resume heuristic.
                self.override_until = None;
                self.overridden_state = None;
                self.overridden_reason = None;
            }
        }

        let fg = self.foreground_cmd.as_deref().unwrap_or("");
        let fg_base = fg.rsplit('/').next().unwrap_or(fg);

        // 1. PID dead → Done / Crashed.
        if self.root_pid > 0 && !pid_alive(self.root_pid) {
            let last_exit = self.last_marker.and_then(|m| {
                if let Osc133Marker::D { exit_code } = m {
                    exit_code
                } else {
                    None
                }
            });
            if last_exit.is_some_and(|c| c != 0) {
                return self.apply(
                    PaneStateKind::Crashed,
                    "process exited non-zero".to_string(),
                );
            } else {
                return self.apply(PaneStateKind::Done, "process exited".to_string());
            }
        }

        // 2. Foreground is an interactive program.
        if INTERACTIVE_SET.contains(&fg_base) {
            return self.apply(PaneStateKind::Interactive, fg_base.to_string());
        }

        // 3. OSC 133 A or B + idle > 2s + foreground is a shell → WaitingInput.
        if matches!(
            self.last_marker,
            Some(Osc133Marker::B) | Some(Osc133Marker::A)
        ) {
            let idle = self.last_output_at.elapsed();
            if idle > std::time::Duration::from_secs(2) && is_shell(fg_base) {
                return self.apply(
                    PaneStateKind::WaitingInput,
                    "shell prompt, idle > 2s".to_string(),
                );
            }
        }

        // 4. OSC 133 D > 5s ago with no new output → Idle.
        if let Some(d_at) = self.last_d_at {
            if d_at.elapsed() > std::time::Duration::from_secs(5)
                && self.last_output_at < d_at + std::time::Duration::from_secs(1)
            {
                return self.apply(PaneStateKind::Idle, "quiet after block end".to_string());
            }
        }

        // 5. Default: Running.
        self.apply(PaneStateKind::Running, "active".to_string())
    }

    fn apply(&mut self, new_state: PaneStateKind, new_reason: String) -> bool {
        if self.state == new_state {
            return false;
        }
        self.state = new_state;
        self.reason = new_reason;
        let _ = self.watch_tx.send_replace(new_state);
        true
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// /proc helpers
// ─────────────────────────────────────────────────────────────────────────────

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn is_shell(name: &str) -> bool {
    SHELL_SET.contains(&name)
}

/// Walk /proc/{pid}/task/{pid}/children recursively to find the deepest leaf PID,
/// then return its comm. Falls back to root_pid's own comm.
pub fn foreground_of(root_pid: u32) -> Option<String> {
    let deepest = deepest_child(root_pid);
    read_comm(deepest)
}

fn deepest_child(pid: u32) -> u32 {
    let children_path = format!("/proc/{pid}/task/{pid}/children");
    if let Ok(content) = std::fs::read_to_string(&children_path) {
        let children: Vec<u32> = content
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if let Some(&last) = children.last() {
            return deepest_child(last);
        }
    }
    pid
}

fn read_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon-wide tick task
// ─────────────────────────────────────────────────────────────────────────────

/// Spawns a background tokio task that evaluates all pane state trackers every
/// 500 ms and fires hooks on transitions.
pub fn spawn_state_engine(
    registry: Arc<crate::session::SessionRegistry>,
    hooks: Arc<crate::hooks::HooksConfig>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let pane_trackers = registry.all_trackers().await;
            for (session_id, pane_id, tracker_arc) in pane_trackers {
                let (changed, new_state, reason) = {
                    let mut t = tracker_arc.lock().expect("tracker poisoned");
                    t.foreground_cmd = foreground_of(t.root_pid);
                    let changed = t.evaluate();
                    (changed, t.state, t.reason.clone())
                };
                if changed {
                    hooks
                        .fire_state_change(session_id, pane_id, new_state, &reason)
                        .await;
                }
            }
        }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker() -> PaneStateTracker {
        let (t, _rx) = PaneStateTracker::new(0);
        t
    }

    #[test]
    fn default_state_is_running() {
        let t = make_tracker();
        assert_eq!(t.state, PaneStateKind::Running);
    }

    #[test]
    fn override_expires_and_resumes_heuristic() {
        let mut t = make_tracker();
        t.overridden_state = Some(PaneStateKind::WaitingInput);
        t.overridden_reason = Some("test".to_string());
        // Already expired.
        t.override_until = Some(Instant::now() - std::time::Duration::from_millis(1));
        t.evaluate();
        assert!(t.override_until.is_none());
    }

    #[test]
    fn interactive_set_detected() {
        let mut t = make_tracker();
        t.foreground_cmd = Some("nvim".to_string());
        t.evaluate();
        assert_eq!(t.state, PaneStateKind::Interactive);
    }

    #[test]
    fn waiting_input_after_b_marker_idle() {
        let mut t = make_tracker();
        t.foreground_cmd = Some("bash".to_string());
        t.last_marker = Some(Osc133Marker::B);
        t.last_output_at = Instant::now() - std::time::Duration::from_secs(5);
        t.evaluate();
        assert_eq!(t.state, PaneStateKind::WaitingInput);
    }
}
