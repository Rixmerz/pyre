use crate::fire_motion;
use crate::theme;
use crate::AppState;
use pyre_proto::PaneStateKind;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, List, ListItem};

/// Map a pane state to the dot character used in the sidebar and status rollup.
pub(crate) fn state_dot_char(state: PaneStateKind) -> char {
    use PaneStateKind::*;
    match state {
        Running => '●',
        WaitingInput => '◎',
        Idle => '○',
        Interactive => '◆',
        Crashed => '✗',
        Done => '◦',
    }
}

/// Map a pane state to the dot color used in the sidebar.
pub(crate) fn state_dot_color(state: PaneStateKind, t: &theme::LegacyTheme) -> Color {
    use PaneStateKind::*;
    match state {
        Running => t.ok,
        WaitingInput => t.spark,
        Idle | Done => t.text_dim,
        Interactive => t.info,
        Crashed => t.err,
    }
}

/// Agent-friendly label for sidebar (maps daemon state + seen flag).
pub(crate) fn agent_ui_label(state: PaneStateKind, seen: bool) -> &'static str {
    use PaneStateKind::*;
    match (state, seen) {
        (WaitingInput, _) => "blocked",
        (Running, _) => "working",
        (Interactive, _) => "interactive",
        (Crashed, _) => "crashed",
        (Done, false) => "done",
        (Done, true) => "idle",
        (Idle, _) => "idle",
    }
}

/// Worst pane in a session (for session-strip rollup).
pub(crate) fn session_worst_pane(
    sidebar: &[pyre_proto::PaneInfo],
    session_id: pyre_proto::SessionId,
) -> Option<&pyre_proto::PaneInfo> {
    use PaneStateKind::*;
    let rank = |s: PaneStateKind| -> u8 {
        match s {
            Crashed => 0,
            WaitingInput => 1,
            Running => 2,
            Interactive => 3,
            Idle => 4,
            Done => 5,
        }
    };
    sidebar
        .iter()
        .filter(|p| p.session == session_id)
        .min_by_key(|p| rank(p.state))
}

/// Resolve the display name for a session by ID, falling back to a UUID prefix.
pub(crate) fn session_name_for(state: &AppState, session_id: pyre_proto::SessionId) -> String {
    state
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.name.clone())
        .unwrap_or_else(|| {
            let s = session_id.0.to_string();
            s[..8.min(s.len())].to_string()
        })
}

pub(crate) fn render_sidebar(
    frame: &mut ratatui::Frame,
    area: Rect,
    state: &AppState,
    t: &theme::LegacyTheme,
) {
    let block = RatatuiBlock::default()
        .borders(Borders::RIGHT)
        .border_type(BorderType::Rounded)
        .style(t.bg_style())
        .title(Span::styled(" agents ", t.title(t.primary)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = state
        .sidebar_data
        .iter()
        .enumerate()
        .take(inner.height as usize)
        .map(|(i, info)| {
            let sess = session_name_for(state, info.session);
            let dot = state_dot_char(info.state);
            let anim_f = state.anim.frame();
            let seed = info.id.0.as_u128() as u32;
            let dot_color = if info.state == PaneStateKind::WaitingInput && !info.seen {
                let p = fire_motion::pulse_phase(anim_f, seed, 9.0);
                fire_motion::lerp_rgb(
                    fire_motion::rgb_tuple(state_dot_color(info.state, t)),
                    fire_motion::rgb_tuple(t.secondary),
                    p * 0.55,
                )
            } else {
                state_dot_color(info.state, t)
            };
            let label = agent_ui_label(info.state, info.seen);
            let agent = info.agent.label();
            let id_str = info.id.0.to_string();
            let pane_short = &id_str[..8.min(id_str.len())];
            let row_style = if i == state.sidebar_cursor && state.sidebar_focused {
                Style::default()
                    .fg(t.bg)
                    .bg(t.primary)
                    .add_modifier(Modifier::BOLD)
            } else if i == state.sidebar_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if info.state == PaneStateKind::WaitingInput && !info.seen {
                fire_motion::ember_fg_style(anim_f, seed, t.spark, t.text, 0.45)
            } else {
                Style::default().fg(t.text)
            };
            ListItem::new(Line::from(vec![
                Span::styled("  ", row_style),
                Span::styled(dot.to_string(), Style::default().fg(dot_color)),
                Span::styled(format!(" {sess} {label} {agent} {pane_short}"), row_style),
            ]))
        })
        .collect();

    let list = List::new(items).style(t.bg_style());
    frame.render_widget(list, inner);
}
