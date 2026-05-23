use pyre_themes::{Registry, Theme};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph};

use crate::theme;

/// State for the theme picker overlay (Ctrl-B T).
pub struct ThemePickerState {
    /// Index of the currently highlighted theme in the registry list.
    pub cursor: usize,
    /// Snapshot of theme names from the registry, in display order.
    pub names: Vec<&'static str>,
    /// Theme that was active when the picker opened — restored on Esc/q.
    pub original_theme: Theme,
}

/// Render the theme picker overlay.
pub fn render_theme_picker(
    frame: &mut ratatui::Frame,
    picker: &ThemePickerState,
    t: &theme::LegacyTheme,
) {
    let reg = Registry::builtin();
    let themes = reg.list();

    let area = frame.area();
    let w = (area.width as f32 * 0.60) as u16;
    let h = (area.height as f32 * 0.70) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(40), h.max(8));

    frame.render_widget(Clear, overlay_rect);

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .title(Span::styled(
            " theme picker  ↑/↓ select  Enter apply  Esc cancel ",
            t.title(t.primary),
        ))
        .style(t.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    let visible_rows = inner.height as usize;

    // Scroll window: keep cursor visible.
    let scroll_top = if picker.cursor >= visible_rows {
        picker.cursor - visible_rows + 1
    } else {
        0
    };

    for (row_idx, theme_idx) in (scroll_top..).take(visible_rows).enumerate() {
        let Some(row_theme) = themes.get(theme_idx) else {
            break;
        };

        let y_pos = inner.y + row_idx as u16;
        let is_selected = theme_idx == picker.cursor;

        let kind_badge = match row_theme.kind {
            pyre_themes::ThemeKind::Dark => "dark ",
            pyre_themes::ThemeKind::Light => "lite ",
        };

        // Each swatch uses THAT theme's own palette colours so the user can
        // see what each theme looks like before committing.
        let swatch_roles: [ratatui::style::Color; 8] = [
            row_theme.palette.bg.to_ratatui(),
            row_theme.palette.fg.to_ratatui(),
            row_theme.palette.accent.to_ratatui(),
            row_theme.palette.border_focus.to_ratatui(),
            row_theme.palette.cursor.to_ratatui(),
            row_theme.palette.ok.to_ratatui(),
            row_theme.palette.warn.to_ratatui(),
            row_theme.palette.error.to_ratatui(),
        ];

        // Selected row uses active theme's primary/bg; others use active bg/fg.
        let row_bg = if is_selected { t.primary } else { t.bg };
        let row_fg = if is_selected { t.bg } else { t.text };

        let label = format!("{kind_badge}{}", row_theme.display_name);
        let mut spans: Vec<Span> = vec![Span::styled(
            format!(" {label:<28} "),
            Style::default().fg(row_fg).bg(row_bg),
        )];

        for swatch in &swatch_roles {
            spans.push(Span::styled("░", Style::default().fg(*swatch).bg(row_bg)));
        }
        spans.push(Span::styled(" ", Style::default().bg(row_bg)));

        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, y_pos, inner.width, 1),
        );
    }
}
