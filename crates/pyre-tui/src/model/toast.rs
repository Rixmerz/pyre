// ─────────────────────────────────────────────────────────────────────────────
// Toast notification subsystem
// ─────────────────────────────────────────────────────────────────────────────

use std::time::{Duration, Instant};

/// Visual kind of a toast notification; controls border colour from the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

/// A single ephemeral notification card.
#[derive(Debug, Clone)]
pub struct Toast {
    /// Bold title line, e.g. "claude · pane #a1b2c3d4".
    pub title: String,
    /// Dimmed body line, e.g. "Waiting for input".
    pub body: String,
    /// Determines border colour.
    pub kind: ToastKind,
    /// When the toast was created.
    pub born_at: Instant,
    /// How long before it expires.
    pub ttl: Duration,
}

impl Toast {
    /// Fraction of TTL remaining [0.0, 1.0].
    pub fn remaining_fraction(&self) -> f32 {
        let elapsed = self.born_at.elapsed();
        if elapsed >= self.ttl {
            0.0
        } else {
            1.0 - (elapsed.as_secs_f32() / self.ttl.as_secs_f32())
        }
    }

    pub fn is_expired(&self) -> bool {
        self.born_at.elapsed() >= self.ttl
    }
}

/// Stack of live toasts rendered bottom-right.
pub struct ToastDeck {
    pub toasts: std::collections::VecDeque<Toast>,
    pub max_visible: usize,
    /// Whether toast display is enabled (toggleable via Ctrl-Space N).
    pub enabled: bool,
    /// TTL applied to new toasts.
    pub ttl: Duration,
}

impl ToastDeck {
    pub fn new(enabled: bool, ttl_ms: u64, max_visible: usize) -> Self {
        Self {
            toasts: std::collections::VecDeque::new(),
            max_visible,
            enabled,
            ttl: Duration::from_millis(ttl_ms),
        }
    }

    /// Push a new toast; trims oldest when over `max_visible`.
    pub fn push(&mut self, title: String, body: String, kind: ToastKind) {
        if !self.enabled {
            return;
        }
        let toast = Toast {
            title,
            body,
            kind,
            born_at: Instant::now(),
            ttl: self.ttl,
        };
        self.toasts.push_back(toast);
        while self.toasts.len() > self.max_visible {
            self.toasts.pop_front();
        }
    }

    /// Drop expired toasts. Call once per UI tick.
    pub fn tick(&mut self) {
        self.toasts.retain(|t| !t.is_expired());
    }
}

/// Map a `PaneEvent` to an optional toast.
/// Returns `None` for Idle/Running (spam suppression) and unknown states.
pub fn pane_event_to_toast(event: &pyre_proto::PaneEvent, ttl: Duration) -> Option<Toast> {
    use pyre_proto::{PaneEventKind, PaneStateKind};

    let short: String = event.pane_id.to_string().chars().take(8).collect();
    let agent_label = event
        .agent
        .map(|a| format!(" ({})", a.label()))
        .unwrap_or_default();
    let title = format!("{short}{agent_label}");

    let (body, kind) = match event.kind {
        PaneEventKind::Spawned => ("Spawned".to_owned(), ToastKind::Info),
        PaneEventKind::Closed => ("Closed".to_owned(), ToastKind::Info),
        PaneEventKind::StateChanged => {
            match event.state {
                Some(PaneStateKind::WaitingInput) => {
                    ("Waiting for input".to_owned(), ToastKind::Warn)
                }
                Some(PaneStateKind::Done) => ("Done".to_owned(), ToastKind::Success),
                Some(PaneStateKind::Crashed) => ("Failed".to_owned(), ToastKind::Error),
                // Idle and Running are high-frequency — suppress.
                Some(PaneStateKind::Idle) | Some(PaneStateKind::Running) => return None,
                _ => return None,
            }
        }
        // Layout topology changes do not produce a toast — clients re-fetch
        // via get_session_layout on this event (ADR-0005 M7-B).
        PaneEventKind::LayoutChanged => return None,
    };

    Some(Toast {
        title,
        body,
        kind,
        born_at: Instant::now(),
        ttl,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_push_trims_to_max_visible() {
        let mut deck = ToastDeck::new(true, 4000, 3);
        deck.push("a".into(), "body".into(), ToastKind::Info);
        deck.push("b".into(), "body".into(), ToastKind::Info);
        deck.push("c".into(), "body".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 3);
        deck.push("d".into(), "body".into(), ToastKind::Info);
        assert_eq!(deck.toasts.len(), 3);
        assert_eq!(deck.toasts.back().unwrap().title, "d");
        assert_eq!(deck.toasts.front().unwrap().title, "b");
    }

    #[test]
    fn deck_tick_drops_expired() {
        let mut deck = ToastDeck::new(true, 4000, 5);
        let expired = Toast {
            title: "old".into(),
            body: "body".into(),
            kind: ToastKind::Warn,
            born_at: Instant::now() - Duration::from_secs(5),
            ttl: Duration::from_secs(4),
        };
        deck.toasts.push_back(expired);
        deck.push("fresh".into(), "body".into(), ToastKind::Success);
        assert_eq!(deck.toasts.len(), 2);
        deck.tick();
        assert_eq!(deck.toasts.len(), 1);
        assert_eq!(deck.toasts.front().unwrap().title, "fresh");
    }

    fn make_event(
        kind: pyre_proto::PaneEventKind,
        state: Option<pyre_proto::PaneStateKind>,
    ) -> pyre_proto::PaneEvent {
        pyre_proto::PaneEvent {
            seq: 1,
            pane_id: pyre_proto::PaneId(
                uuid::Uuid::parse_str("aabbccdd-0000-0000-0000-000000000000").unwrap(),
            ),
            kind,
            state,
            agent: None,
        }
    }

    #[test]
    fn pane_event_to_toast_mapping() {
        use pyre_proto::{PaneEventKind, PaneStateKind};
        let ttl = Duration::from_millis(4000);

        let ev = make_event(PaneEventKind::Spawned, None);
        let t = pane_event_to_toast(&ev, ttl).expect("Spawned must produce a toast");
        assert_eq!(t.kind, ToastKind::Info);
        assert_eq!(t.body, "Spawned");

        let ev = make_event(PaneEventKind::Closed, None);
        let t = pane_event_to_toast(&ev, ttl).expect("Closed must produce a toast");
        assert_eq!(t.kind, ToastKind::Info);
        assert_eq!(t.body, "Closed");

        let ev = make_event(
            PaneEventKind::StateChanged,
            Some(PaneStateKind::WaitingInput),
        );
        let t = pane_event_to_toast(&ev, ttl).expect("WaitingInput must produce a toast");
        assert_eq!(t.kind, ToastKind::Warn);

        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Done));
        let t = pane_event_to_toast(&ev, ttl).expect("Done must produce a toast");
        assert_eq!(t.kind, ToastKind::Success);

        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Crashed));
        let t = pane_event_to_toast(&ev, ttl).expect("Crashed must produce a toast");
        assert_eq!(t.kind, ToastKind::Error);

        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Idle));
        assert!(
            pane_event_to_toast(&ev, ttl).is_none(),
            "Idle must be suppressed"
        );

        let ev = make_event(PaneEventKind::StateChanged, Some(PaneStateKind::Running));
        assert!(
            pane_event_to_toast(&ev, ttl).is_none(),
            "Running must be suppressed"
        );
    }
}
