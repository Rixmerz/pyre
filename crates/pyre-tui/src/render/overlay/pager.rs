use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as RatatuiBlock, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::theme;

/// State for the block stdout modal pager (Enter in ribbon mode).
pub struct PagerState {
    /// Block identifier shown in the title bar.
    pub block_id: String,
    /// Exit code shown in the title bar (`None` = still running).
    pub exit_code: Option<i32>,
    /// Output lines (raw bytes decoded as lossy UTF-8, split on `\n`).
    pub lines: Vec<String>,
    /// First visible line index (scrolled position).
    pub scroll: usize,
}

impl PagerState {
    pub fn new(block_id: String, exit_code: Option<i32>, raw: &[u8]) -> Self {
        let text = String::from_utf8_lossy(raw);
        let lines: Vec<String> = text.split('\n').map(|l| l.to_owned()).collect();
        Self {
            block_id,
            exit_code,
            lines,
            scroll: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn scroll_up(&mut self, n: usize) -> bool {
        let prev = self.scroll;
        self.scroll = self.scroll.saturating_sub(n);
        self.scroll != prev
    }

    pub fn scroll_down(&mut self, n: usize, visible_rows: usize) -> bool {
        let prev = self.scroll;
        let max_scroll = self.len().saturating_sub(visible_rows);
        self.scroll = (self.scroll + n).min(max_scroll);
        self.scroll != prev
    }
}

/// Render the full-screen block stdout pager overlay.
///
/// Layout (top-to-bottom):
///   - Title bar  (1 row): block id + exit code
///   - Body       (fill):  scrollable stdout lines
///   - Footer     (1 row): scroll hints + keybinding hint
pub fn render_pager(frame: &mut ratatui::Frame, pager: &PagerState, t: &theme::LegacyTheme) {
    let area = frame.area();

    // Dim the background to indicate a blocking overlay.
    frame.render_widget(Clear, area);

    let outer = RatatuiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(t.border_focus())
        .style(t.overlay());

    // Build a title that includes the block id and exit code.
    let exit_label = match pager.exit_code {
        None => " running ".to_owned(),
        Some(0) => " exit 0 ".to_owned(),
        Some(n) => format!(" exit {n} "),
    };
    let title_str = format!(" {} |{exit_label}", &pager.block_id);

    let outer_with_title = outer.title(Span::styled(title_str, t.title(t.primary)));
    let inner = outer_with_title.inner(area);
    frame.render_widget(outer_with_title, area);

    // Split inner into body + footer.
    let splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let body_area = splits[0];
    let footer_area = splits[1];

    let visible_rows = body_area.height as usize;

    // Collect visible lines.
    let visible: Vec<Line> = pager
        .lines
        .iter()
        .skip(pager.scroll)
        .take(visible_rows)
        .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(t.text))))
        .collect();

    let body = Paragraph::new(visible)
        .style(t.bg_style())
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(body, body_area);

    // Scrollbar on the right edge of body_area.
    let total_lines = pager.len();
    if total_lines > visible_rows {
        let mut sb_state =
            ScrollbarState::new(total_lines.saturating_sub(visible_rows)).position(pager.scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            body_area,
            &mut sb_state,
        );
    }

    // Footer: hint line.
    let hint = Paragraph::new(Line::from(vec![
        Span::styled(" ↑/↓ ", Style::default().fg(t.primary)),
        Span::styled("scroll  ", Style::default().fg(t.text_dim)),
        Span::styled("PgUp/PgDn ", Style::default().fg(t.primary)),
        Span::styled("page  ", Style::default().fg(t.text_dim)),
        Span::styled("q/Esc ", Style::default().fg(t.primary)),
        Span::styled("close", Style::default().fg(t.text_dim)),
    ]))
    .style(t.bg_style());
    frame.render_widget(hint, footer_area);
}
