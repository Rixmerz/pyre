/// Render the toast deck in the bottom-right corner of `frame`.
///
/// Each card is 3 rows tall and 40 columns wide. Cards stack upward with a
/// 1-row gap. Border colour comes from the active palette.
use crate::model::toast::{Toast, ToastDeck, ToastKind};
use crate::theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph};

pub fn render_toast_deck(frame: &mut ratatui::Frame, deck: &ToastDeck, t: &theme::LegacyTheme) {
    if !deck.enabled || deck.toasts.is_empty() {
        return;
    }

    let area = frame.area();
    const CARD_W: u16 = 40;
    const CARD_H: u16 = 3;
    const GAP: u16 = 1;
    const MARGIN_RIGHT: u16 = 1;
    const MARGIN_BOTTOM: u16 = 2; // above the status bar

    // Iterate newest-first (back of deque) and place cards bottom-up.
    let visible: Vec<&Toast> = deck.toasts.iter().rev().take(deck.max_visible).collect();

    for (i, toast) in visible.iter().enumerate() {
        let card_x = area.width.saturating_sub(CARD_W + MARGIN_RIGHT);
        let card_bottom = area
            .height
            .saturating_sub(MARGIN_BOTTOM + i as u16 * (CARD_H + GAP));
        if card_bottom < CARD_H {
            break; // not enough vertical space
        }
        let card_y = card_bottom - CARD_H;
        let card_rect = Rect::new(card_x, card_y, CARD_W.min(area.width), CARD_H);

        let border_color = match toast.kind {
            ToastKind::Info => t.info,
            ToastKind::Success => t.ok,
            ToastKind::Warn => t.spark,
            ToastKind::Error => t.err,
        };

        // Clear backing cells so pane content doesn't bleed through.
        frame.render_widget(Clear, card_rect);

        let outer = RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(t.muted_bg));
        let inner = outer.inner(card_rect);
        frame.render_widget(outer, card_rect);

        // inner is 1 row tall (3 - 2 borders). Split into title + body
        // using the single row: title bold / body dim / progress bar via title suffix.
        let frac = toast.remaining_fraction();
        let bar_width = (inner.width as f32 * frac) as usize;
        let bar_empty = inner.width as usize - bar_width;
        let progress: String = format!("{}{}", "━".repeat(bar_width), "░".repeat(bar_empty));

        // With only 1 inner row we pack title + progress on the same line.
        // Two-row inner is possible when the terminal is tall enough; keep
        // rendering simple and always use 1 row body + title in the border.
        let title_span = Span::styled(
            format!(" {} ", toast.title),
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        );
        let body_span = Span::styled(format!("{} ", toast.body), Style::default().fg(t.text_dim));
        let prog_span = Span::styled(progress, Style::default().fg(border_color));

        // Render title in the block title position via a Paragraph on inner.
        // Pack: [bold title]  [body dim]  [progress].
        let line = Line::from(vec![title_span, body_span, prog_span]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(t.muted_bg)),
            inner,
        );
    }
}
