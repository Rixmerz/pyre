use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph};

use crate::theme;

/// Render a centered "Session ended" overlay.
///
/// Drawn when `state.session_lost == true` — all pane slots for the active
/// session's active tab are None (daemon evicted the session). Covers the
/// entire frame so the blank pane area is not visible.
pub fn render_session_lost_overlay(frame: &mut ratatui::Frame, t: &theme::LegacyTheme) {
    let area = frame.area();
    // Clear the whole frame first.
    frame.render_widget(RatatuiBlock::default().style(t.bg_style()), area);

    let w: u16 = 52;
    let h: u16 = 5;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let overlay_rect = Rect::new(x, y, w.min(area.width), h.min(area.height));

    frame.render_widget(Clear, overlay_rect);

    let block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.text_dim))
        .title(Span::styled(
            " session ended ",
            Style::default().fg(t.text_dim),
        ))
        .style(t.bg_style());
    let inner = block.inner(overlay_rect);
    frame.render_widget(block, overlay_rect);

    let splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "All panes closed.",
            Style::default().fg(t.text_dim),
        )]))
        .style(t.bg_style()),
        splits[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Press ", Style::default().fg(t.text_dim)),
            Span::styled(
                "q",
                Style::default().fg(t.primary).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" / ", Style::default().fg(t.text_dim)),
            Span::styled(
                "Esc",
                Style::default().fg(t.primary).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" / ", Style::default().fg(t.text_dim)),
            Span::styled(
                "Ctrl-C",
                Style::default().fg(t.primary).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to quit.", Style::default().fg(t.text_dim)),
        ]))
        .style(t.bg_style()),
        splits[1],
    );
}
