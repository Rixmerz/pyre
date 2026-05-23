use std::time::Instant;

use pyre_proto::blocks::BlockHit;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as RatatuiBlock, BorderType, Borders, Clear, List, ListItem, Paragraph,
};
use tokio::sync::mpsc;

use crate::fire_motion;
use crate::theme;

/// State for the full-text search overlay (Ctrl-B /).
pub struct SearchState {
    pub open: bool,
    pub input: String,
    /// Selected result index.
    pub cursor: usize,
    pub results: Vec<BlockHit>,
    pub last_query_at: Instant,
    pub pending_query: Option<String>,
    /// Prefix `!` in the search box sets this (non-zero exit only).
    pub failures_only: bool,
    pub rx: Option<mpsc::Receiver<Vec<BlockHit>>>,
    /// Result row rects captured during last render: (result_index, rect).
    pub result_rects: Vec<(usize, Rect)>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            cursor: 0,
            results: Vec::new(),
            last_query_at: Instant::now(),
            pending_query: None,
            failures_only: false,
            rx: None,
            result_rects: Vec::new(),
        }
    }
}

pub fn parse_search_input(input: &str) -> (String, bool) {
    if let Some(rest) = input.strip_prefix('!') {
        (rest.trim_start().to_string(), true)
    } else {
        (input.to_string(), false)
    }
}

/// Render the search overlay centered on the terminal.
pub fn render_search_overlay(
    frame: &mut ratatui::Frame,
    search: &mut SearchState,
    anim_frame: u64,
    t: &theme::LegacyTheme,
) {
    let area = frame.area();

    // Centered rect: ~70% width, ~60% height.
    let w = (area.width as f32 * 0.70) as u16;
    let h = (area.height as f32 * 0.60) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let overlay_rect = Rect::new(x, y, w.max(20), h.max(6));

    // Clear backing area so panes don't bleed through.
    frame.render_widget(Clear, overlay_rect);

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .title(Span::styled(" search (! = failures) ", t.title(t.primary)))
        .style(t.overlay());
    let inner = outer.inner(overlay_rect);
    frame.render_widget(outer, overlay_rect);

    // Split inner: 3-line input box + remainder for results.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let input_area = split[0];
    let results_area = split[1];

    // Input box — prompt prefix `> ` in primary, query in text, cursor █ in spark.
    let cursor_f = anim_frame;
    let input_spans = vec![
        Span::styled("> ", Style::default().fg(t.primary)),
        Span::styled(search.input.as_str(), Style::default().fg(t.text)),
        Span::styled(
            "█",
            fire_motion::ember_fg_style(cursor_f, 0x_a11ce, t.spark, t.secondary, 0.9),
        ),
    ];
    let input_block = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.border())
        .style(t.overlay());
    let input_para = Paragraph::new(Line::from(input_spans))
        .block(input_block)
        .style(t.bg_style());
    frame.render_widget(input_para, input_area);

    // Set host cursor at end of input text: inner input area x + 2 (prompt) + query len.
    // The input block has 1-cell border on each side, so inner starts at input_area.x + 1.
    let inner_x = input_area.x + 1;
    let inner_y = input_area.y + 1;
    // "> " prefix (2 chars) + query length, clamped to inner width.
    let inner_width = input_area.width.saturating_sub(2); // subtract left+right border
    let cursor_col = (2u16 + search.input.len() as u16).min(inner_width.saturating_sub(1));
    frame.set_cursor_position((inner_x + cursor_col, inner_y));

    // Results list.
    let items: Vec<ListItem> = search
        .results
        .iter()
        .map(|hit| {
            let b = &hit.block;
            let pane_short: String = b.pane.0.to_string().chars().take(8).collect();
            let ts_short = b.started_at.format("%H:%M:%S").to_string();
            let snippet: String = if hit.snippet.is_empty() {
                b.command.chars().take(80).collect()
            } else {
                hit.snippet.chars().take(80).collect()
            };
            ListItem::new(format!("[{pane_short}] {ts_short} {snippet}"))
                .style(Style::default().fg(t.text))
        })
        .collect();

    let list = List::new(items)
        .block(
            RatatuiBlock::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(t.border())
                .title(Span::styled(
                    format!(" {} results ", search.results.len()),
                    Style::default().fg(t.text_dim),
                ))
                .style(t.overlay()),
        )
        .highlight_style(t.selection());

    // Use a stateful list so we can highlight the cursor item.
    let mut list_state = ratatui::widgets::ListState::default();
    if !search.results.is_empty() {
        list_state.select(Some(search.cursor));
    }
    frame.render_stateful_widget(list, results_area, &mut list_state);

    // Populate result_rects for mouse click-to-jump. Each result row is 1 line
    // tall; the list block has a 1-cell border on each side, so body starts at
    // results_area.y + 1 (top border) and x = results_area.x + 1 (left border).
    let inner_x = results_area.x.saturating_add(1);
    let inner_y = results_area.y.saturating_add(1);
    let inner_w = results_area.width.saturating_sub(2);
    search.result_rects = search
        .results
        .iter()
        .enumerate()
        .filter_map(|(i, _)| {
            let row_y = inner_y.checked_add(i as u16)?;
            if row_y
                >= results_area
                    .y
                    .saturating_add(results_area.height)
                    .saturating_sub(1)
            {
                return None; // clipped by bottom border
            }
            Some((i, Rect::new(inner_x, row_y, inner_w, 1)))
        })
        .collect();
}
