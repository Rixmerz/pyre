//! Help overlay — Ctrl-Space ? (prefix `?`).
//!
//! Lists all prefix-key bindings grouped by category. Centered floating
//! panel themed like the theme picker and search overlays. Dismissed by
//! Esc, q, or a second `?`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph};

use crate::theme;

// ─────────────────────────────────────────────────────────────────────────────
// Binding categories
// ─────────────────────────────────────────────────────────────────────────────

/// A single binding entry: (key display, description).
type BindingEntry = (&'static str, &'static str);

/// Categorised prefix-key bindings, in display order.
const CATEGORIES: &[(&str, &[BindingEntry])] = &[
    (
        "Panes",
        &[
            ("Ctrl-Space c", "New pane in current session"),
            ("Ctrl-Space \"", "Horizontal split (below)"),
            ("Ctrl-Space %", "Vertical split (right)"),
            ("Ctrl-Space x", "Close focused pane"),
            ("Ctrl-Space z", "Toggle zoom (fullscreen)"),
            ("Ctrl-Space →/↓", "Focus next pane"),
            ("Ctrl-Space ←/↑", "Focus previous pane"),
        ],
    ),
    (
        "Sessions",
        &[
            ("Ctrl-Space S", "New session (with name prompt)"),
            ("Ctrl-Space ,", "Rename active session"),
            ("Ctrl-Space n", "Next tab"),
            ("Ctrl-Space p", "Previous tab"),
            ("Ctrl-Space d", "Detach (leave daemon running)"),
            ("Ctrl-Space q", "Quit TUI"),
        ],
    ),
    (
        "Scrollback / Blocks",
        &[
            ("Ctrl-Space [", "Enter scrollback mode"),
            ("Ctrl-Space ]", "Exit scrollback mode"),
            ("Ctrl-Space y", "Copy last block stdout to clipboard"),
        ],
    ),
    (
        "Overlays / Misc",
        &[
            ("Ctrl-Space /", "Search blocks (Tantivy)"),
            ("Ctrl-Space s", "Toggle sidebar"),
            ("Ctrl-Space T", "Theme picker"),
            ("Ctrl-Space N", "Toggle toast notifications"),
            ("Ctrl-Space ?", "This help overlay"),
        ],
    ),
];

// ─────────────────────────────────────────────────────────────────────────────
// Renderer
// ─────────────────────────────────────────────────────────────────────────────

/// Render the centered help overlay.
pub fn render_help_overlay(frame: &mut ratatui::Frame, t: &theme::LegacyTheme) {
    let area = frame.area();

    // Compute content: count all rows we need (category headers + entries + gaps).
    let total_content_rows: u16 = CATEGORIES
        .iter()
        .map(|(_, entries)| {
            // 1 header row + entries + 1 blank separator (except after last category)
            1u16 + entries.len() as u16 + 1
        })
        .sum::<u16>()
        .saturating_sub(1); // no trailing blank after last category

    // Find widest key label + widest description for layout.
    let max_key_w: u16 = CATEGORIES
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|(k, _)| k.len() as u16)
        .max()
        .unwrap_or(16);

    let max_desc_w: u16 = CATEGORIES
        .iter()
        .flat_map(|(_, entries)| entries.iter())
        .map(|(_, d)| d.len() as u16)
        .max()
        .unwrap_or(30);

    // Panel: key_col + 2 (gap) + desc_col + 2 (inner padding) + 2 (border)
    let content_w = max_key_w + 2 + max_desc_w + 2;
    let w = content_w.max(50).min(area.width.saturating_sub(4));

    // Panel height = content rows + 2 (top/bottom border).
    let h = (total_content_rows + 2).min(area.height.saturating_sub(4));

    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let overlay_rect = Rect::new(x, y, w, h);

    frame.render_widget(Clear, overlay_rect);

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .title(Span::styled(
            " key bindings  q / Esc to close ",
            t.title(t.primary),
        ))
        .style(t.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Render rows top-to-bottom inside inner.
    let mut row_y = inner.y;
    let bottom = inner.y + inner.height;

    for (cat_idx, (category, entries)) in CATEGORIES.iter().enumerate() {
        if row_y >= bottom {
            break;
        }

        // Category header.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                *category,
                Style::default()
                    .fg(t.primary)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )))
            .style(t.overlay()),
            Rect::new(inner.x, row_y, inner.width, 1),
        );
        row_y += 1;

        // Entry rows.
        for (key, desc) in entries.iter() {
            if row_y >= bottom {
                break;
            }
            let key_span = Span::styled(format!("{key:<width$}", width = max_key_w as usize), {
                Style::default().fg(t.secondary)
            });
            let sep_span = Span::styled("  ", Style::default().fg(t.text_dim));
            let desc_span = Span::styled(*desc, Style::default().fg(t.text));
            frame.render_widget(
                Paragraph::new(Line::from(vec![key_span, sep_span, desc_span])).style(t.overlay()),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            row_y += 1;
        }

        // Blank separator between categories (not after the last one).
        if cat_idx + 1 < CATEGORIES.len() && row_y < bottom {
            row_y += 1;
        }
    }
}
