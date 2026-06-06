use std::collections::HashMap;

use crate::fire_motion;
use crate::model::pane::{PaneSlot, SplitBoundary};
use crate::model::selection::{Selection, SelectionBase};
use crate::theme;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use pyre_proto::{layout::LayoutNode, PaneId};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block as RatatuiBlock, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

/// Minimal Dimensions impl for creating/resizing an alacritty Term.
pub(crate) struct TermSize {
    cols: usize,
    rows: usize,
}

impl TermSize {
    pub(crate) fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Convert an alacritty/vte AnsiColor to a ratatui Color.
/// Returns None for "default" colors so ratatui uses its own defaults.
pub(crate) fn ansi_color(color: AnsiColor) -> Option<Color> {
    match color {
        AnsiColor::Named(nc) => match nc {
            NamedColor::Black => Some(Color::Black),
            NamedColor::Red => Some(Color::Red),
            NamedColor::Green => Some(Color::Green),
            NamedColor::Yellow => Some(Color::Yellow),
            NamedColor::Blue => Some(Color::Blue),
            NamedColor::Magenta => Some(Color::Magenta),
            NamedColor::Cyan => Some(Color::Cyan),
            NamedColor::White => Some(Color::Gray),
            NamedColor::BrightBlack => Some(Color::DarkGray),
            NamedColor::BrightRed => Some(Color::LightRed),
            NamedColor::BrightGreen => Some(Color::LightGreen),
            NamedColor::BrightYellow => Some(Color::LightYellow),
            NamedColor::BrightBlue => Some(Color::LightBlue),
            NamedColor::BrightMagenta => Some(Color::LightMagenta),
            NamedColor::BrightCyan => Some(Color::LightCyan),
            NamedColor::BrightWhite => Some(Color::White),
            // Foreground/Background are "default" — let ratatui use terminal defaults.
            NamedColor::Foreground | NamedColor::Background => None,
            // Dim variants: map to corresponding base color.
            NamedColor::DimBlack => Some(Color::Black),
            NamedColor::DimRed => Some(Color::Red),
            NamedColor::DimGreen => Some(Color::Green),
            NamedColor::DimYellow => Some(Color::Yellow),
            NamedColor::DimBlue => Some(Color::Blue),
            NamedColor::DimMagenta => Some(Color::Magenta),
            NamedColor::DimCyan => Some(Color::Cyan),
            NamedColor::DimWhite => Some(Color::Gray),
            // Cursor/DimForeground/etc — treat as default.
            _ => None,
        },
        AnsiColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AnsiColor::Indexed(i) => Some(Color::Indexed(i)),
    }
}

pub(crate) fn pane_needs_attention(meta: &[pyre_proto::PaneInfo], pane_id: PaneId) -> bool {
    meta.iter()
        .any(|p| p.id == pane_id && p.state == pyre_proto::PaneStateKind::WaitingInput && !p.seen)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane(
    frame: &mut ratatui::Frame,
    area: Rect,
    slot: &mut PaneSlot,
    focused: bool,
    selection: Option<&Selection>,
    slot_idx: usize,
    pending_resizes: &mut Vec<(PaneId, pyre_proto::PaneSize)>,
    anim_frame: u64,
    attention: bool,
    theme: &theme::LegacyTheme,
    panes_meta: &[pyre_proto::PaneInfo],
) {
    let short8: String = slot.pane_id.0.to_string().chars().take(8).collect();
    let seed = slot.pane_id.0.as_u128() as u32;
    // Use the user-provided name when available; fall back to short UUID prefix.
    let pane_title: String = panes_meta
        .iter()
        .find(|p| p.id == slot.pane_id)
        .and_then(|p| p.name.as_deref().filter(|s| !s.is_empty()))
        .map(|s| format!(" {s} "))
        .unwrap_or_else(|| format!(" pane {short8} "));
    let border_block = if focused {
        let border_style = if attention {
            fire_motion::ember_border_style(anim_frame, seed, theme.border_focus, theme.spark)
        } else {
            theme.border_focus()
        };
        let title_style = if attention {
            fire_motion::ember_title_style(anim_frame, seed, theme.primary, theme.spark)
        } else {
            theme.title(theme.primary)
        };
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick)
            .border_style(border_style)
            .title(Span::styled(pane_title.clone(), title_style))
    } else if attention {
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fire_motion::ember_border_style(
                anim_frame,
                seed,
                theme.border,
                theme.primary,
            ))
            .title(Span::styled(
                pane_title.clone(),
                fire_motion::ember_title_style(anim_frame, seed, theme.text_dim, theme.secondary),
            ))
    } else {
        RatatuiBlock::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.border())
            .title(Span::styled(pane_title, theme.title(theme.text_dim)))
    };

    let inner = border_block.inner(area);
    frame.render_widget(border_block, area);

    // Guard: if the allocated area is degenerate (e.g. terminal is narrower than
    // the minimum pane layout), skip all rendering for this pane. Passing cols=0
    // or rows=0 to alacritty_terminal Grid::resize causes an underflow at
    // `Column(columns - 1)` and panics. This can happen when the host terminal is
    // very small, a split produces a zero-height child, or the TUI is run inside a
    // pseudo-terminal with a 0×0 initial size (e.g. `script` without explicit stty).
    if inner.width == 0 || inner.height < 2 {
        // Area is too small to fit border (2 rows) + ribbon (1 row) + at least 1
        // content row. Leave parser_sized as-is so the next frame will retry.
        return;
    }

    // Split inner area: vt100/scrollback area (Min 1) on top, ribbon (1 line) at bottom.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let content_area = split[0];
    let ribbon_area = split[1];

    // Store the content rect for mouse hit-test (used by scroll wheel handler).
    slot.last_screen_rect = content_area;

    // ── Unified render: scroll_display shifts the alacritty view; 0 = live ──
    // Peek total scrollback capacity by temporarily jumping to Top, reading
    // history_size(), then restoring to our desired offset.
    slot.scrollback_capacity = slot.term.grid().history_size();
    // Clamp current offset in case old lines aged out of the ring buffer.
    slot.scroll_offset = slot.scroll_offset.min(slot.scrollback_capacity);
    // Set display_offset to our desired scrollback position.
    slot.term.grid_mut().scroll_display(Scroll::Bottom);
    if slot.scroll_offset > 0 {
        slot.term
            .grid_mut()
            .scroll_display(Scroll::Delta(slot.scroll_offset as i32));
    }

    // When scrolled back, reserve 1 column on the right for a scrollbar.
    let (sb_area, text_area) = if slot.scroll_offset > 0 && content_area.width > 1 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(content_area);
        (Some(split[1]), split[0])
    } else {
        (None, content_area)
    };

    // Bug A fix: sync terminal dimensions to the actual visible area each frame.
    // If the terminal was never resized (e.g. after a split), it still thinks it
    // is the original full-terminal size and positions output beyond the pane
    // bounds, producing invisible or overlapping lines.
    {
        let target_rows = text_area.height as usize;
        let target_cols = text_area.width as usize;
        let cur_rows = slot.term.grid().screen_lines();
        let cur_cols = slot.term.grid().columns();

        // Log on first call only (before parser_sized is set).
        if !slot.parser_sized {
            tracing::debug!(
                slot_idx,
                text_area.width,
                text_area.height,
                parser_rows = cur_rows,
                parser_cols = cur_cols,
                "render_pane: first call"
            );
        }

        if cur_rows != target_rows || cur_cols != target_cols {
            tracing::debug!(
                slot_idx,
                old_rows = cur_rows,
                old_cols = cur_cols,
                new_rows = target_rows,
                new_cols = target_cols,
                "render_pane: terminal resize"
            );
            // Belt-and-suspenders guard: alacritty Grid::resize panics when
            // columns == 0 (underflow at `Column(columns - 1)`). The outer
            // `inner.height < 2` guard above should prevent this, but a split
            // with Constraint::Min(1) can still yield height 0 in degenerate
            // layouts. Skip the resize instead of panicking; the next frame
            // will retry once the terminal has a proper size.
            if target_cols > 0 && target_rows > 0 {
                slot.term.resize(TermSize::new(target_cols, target_rows));
            }
        }
        // On the first render we now know the real pane area. Drain any bytes
        // that arrived before this frame (buffered in pending_output at wrong
        // size) through the correctly-sized terminal, then mark as sized so
        // subsequent bytes go directly to the terminal.
        if !slot.parser_sized {
            slot.parser_sized = true;
            if !slot.pending_output.is_empty() {
                let buffered = std::mem::take(&mut slot.pending_output);
                slot.processor.advance(&mut slot.term, &buffered);
            }
        }
        // Fire resize RPC when dims changed AND differ from last sent — avoid
        // spamming the daemon every frame. Collected into pending_resizes and
        // drained after draw() returns (async context).
        let (last_cols, last_rows) = slot.last_sent_size;
        let target_cols_u16 = target_cols as u16;
        let target_rows_u16 = target_rows as u16;
        if target_cols_u16 != last_cols || target_rows_u16 != last_rows {
            slot.last_sent_size = (target_cols_u16, target_rows_u16);
            pending_resizes.push((
                slot.pane_id,
                pyre_proto::PaneSize {
                    cols: target_cols_u16,
                    rows: target_rows_u16,
                },
            ));
        }
    }

    {
        let grid = slot.term.grid();
        let num_rows = grid.screen_lines();
        let num_cols = grid.columns();
        let mut lines: Vec<Line> = Vec::with_capacity(text_area.height as usize);

        for row in 0..text_area.height as usize {
            let mut spans: Vec<Span> = Vec::new();
            let mut current_text = String::new();
            let mut current_style = Style::default();

            // The viewport top line when scrolled: display_offset lines above Line(0).
            // display_iter visits rows from top of viewport downward; we index directly.
            let display_line = TermLine(row as i32 - grid.display_offset() as i32);

            for col in 0..text_area.width as usize {
                let (ch, fg, bg, flags) = if row < num_rows && col < num_cols {
                    let cell = &grid[TermPoint::new(display_line, TermColumn(col))];
                    let ch = if cell.c == '\0' { ' ' } else { cell.c };
                    (ch, ansi_color(cell.fg), ansi_color(cell.bg), cell.flags)
                } else {
                    (' ', None, None, CellFlags::empty())
                };

                let mut style = Style::default()
                    .fg(fg.unwrap_or(Color::Reset))
                    .bg(bg.unwrap_or(Color::Reset));

                if flags.contains(CellFlags::BOLD) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if flags.contains(CellFlags::DIM) {
                    style = style.add_modifier(Modifier::DIM);
                }
                if flags.contains(CellFlags::ITALIC) {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if flags.intersects(CellFlags::ALL_UNDERLINES) {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if flags.contains(CellFlags::INVERSE) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if flags.contains(CellFlags::HIDDEN) {
                    style = style.add_modifier(Modifier::HIDDEN);
                }
                if flags.contains(CellFlags::STRIKEOUT) {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }

                if style == current_style {
                    current_text.push(ch);
                } else {
                    if !current_text.is_empty() {
                        spans.push(Span::styled(current_text.clone(), current_style));
                        current_text.clear();
                    }
                    current_text.push(ch);
                    current_style = style;
                }
            }
            if !current_text.is_empty() {
                spans.push(Span::styled(current_text, current_style));
            }
            lines.push(Line::from(spans));
        }

        frame.render_widget(Paragraph::new(lines), text_area);
    }

    // Overlay selection highlight.
    //
    // The selection's `start`/`end` are viewport-relative to the scroll offset
    // captured in `sel.base` at MouseDown. We render the highlight whenever that
    // base offset still matches the CURRENT `slot.scroll_offset` — i.e. the
    // viewport has not scrolled away from where the selection lives. This is the
    // same coordinate contract the copy path uses (mouse.rs MouseUp), so the
    // visible highlight and the eventually-copied text always agree.
    //
    // Previously this was gated on `scroll_offset == 0 && SelectionBase::Live`,
    // which hid the highlight the instant any scroll occurred. With drag no
    // longer mutating scroll_offset (fix #3), base and live offset stay equal for
    // an in-viewport drag in BOTH directions, so reverse (bottom→top) selections
    // now highlight correctly; scrolled-back selections (base == Scrollback(N))
    // also highlight while their content remains on screen.
    if let Some(sel) = selection {
        if sel.pane_idx == slot_idx {
            let base_offset = match sel.base {
                SelectionBase::Live => 0,
                SelectionBase::Scrollback(off) => off,
            };
            if base_offset == slot.scroll_offset {
                let ((r0, c0), (r1, c1)) = sel.normalized();
                for row in r0..=r1.min(text_area.height.saturating_sub(1)) {
                    let col_start = if row == r0 { c0 } else { 0 };
                    let col_end = if row == r1 {
                        c1.min(text_area.width.saturating_sub(1))
                    } else {
                        text_area.width.saturating_sub(1)
                    };
                    for col in col_start..=col_end {
                        let sx = text_area.x + col;
                        let sy = text_area.y + row;
                        if sx < text_area.x + text_area.width && sy < text_area.y + text_area.height
                        {
                            if let Some(cell) = frame.buffer_mut().cell_mut((sx, sy)) {
                                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                            }
                        }
                    }
                }
            }
        }
    }

    // Scrollbar when scrolled back.
    if let Some(sb_rect) = sb_area {
        let total_scrollback = slot.scrollback_capacity;
        let virtual_total = total_scrollback.max(1);
        let position = virtual_total.saturating_sub(slot.scroll_offset);
        let mut sb_state = ScrollbarState::new(virtual_total).position(position);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .style(theme.border())
                .thumb_style(Style::default().fg(theme.primary))
                .track_symbol(Some("│"))
                .thumb_symbol("█"),
            sb_rect,
            &mut sb_state,
        );
    }

    // ── ribbon render ──
    render_ribbon(frame, ribbon_area, slot, theme);
}

/// Render the one-line block ribbon inside `area`.
/// Captures chip rects into `slot.ribbon_chip_rects` for mouse hit-test.
fn render_ribbon(
    frame: &mut ratatui::Frame,
    area: Rect,
    slot: &mut PaneSlot,
    theme: &theme::LegacyTheme,
) {
    // Clear chip rects from the previous frame.
    slot.ribbon_chip_rects.clear();

    if slot.recent_blocks.is_empty() {
        let p =
            Paragraph::new(" (no blocks)").style(Style::default().fg(theme.text_dim).bg(theme.bg));
        frame.render_widget(p, area);
        return;
    }

    // Determine the highlighted index. None = live (last block).
    let is_live = slot.ribbon_cursor.is_none();
    let latest_idx = slot.recent_blocks.len().saturating_sub(1);
    let highlight_idx = slot.ribbon_cursor.unwrap_or(latest_idx);

    let mut spans: Vec<Span> = Vec::new();
    // Track x offset for chip rect calculation.
    let mut x_offset: u16 = area.x;

    for (i, b) in slot.recent_blocks.iter().enumerate() {
        let short4: String = b.id.0.to_string().chars().take(4).collect();

        // Exit code badge colour and prefix.
        let (badge_fg, live_prefix) = match b.exit_code {
            Some(0) => (theme.ok, ""),
            Some(_) => (theme.err, ""),
            None => (theme.spark, "●"),
        };

        let sep = if i > 0 { "│" } else { "" };
        let chip_text = format!("{live_prefix}▎b{short4}");
        let sep_len = sep.chars().count() as u16;
        let chip_len = chip_text.chars().count() as u16;

        // Record rect for this chip (separator not included in clickable area).
        if area.height > 0 && x_offset + sep_len < area.x + area.width {
            slot.ribbon_chip_rects.push((
                i,
                Rect::new(
                    x_offset + sep_len,
                    area.y,
                    chip_len.min((area.x + area.width).saturating_sub(x_offset + sep_len)),
                    1,
                ),
            ));
        }
        x_offset += sep_len + chip_len;

        let chip_style = if i == highlight_idx && !is_live {
            theme.selection()
        } else if i == latest_idx && is_live {
            Style::default()
                .fg(theme.bg)
                .bg(theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(badge_fg).bg(theme.muted_bg)
        };

        if i > 0 {
            spans.push(Span::styled("│", Style::default().fg(theme.text_dim)));
        }
        spans.push(Span::styled(chip_text, chip_style));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

/// Render a `LayoutNode` tree into `area`.
///
/// `focus_pane` is the currently focused `PaneId`; leaves matching it receive
/// the focus border.  `pane_slot` maps `PaneId → slot_idx` for O(1) lookup.
/// `current_path` tracks the tree path for drag-resize boundary records.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_layout(
    frame: &mut ratatui::Frame,
    area: Rect,
    node: &LayoutNode,
    slots: &mut Vec<Option<PaneSlot>>,
    focus_pane: PaneId,
    pane_slot: &HashMap<PaneId, usize>,
    current_path: &mut Vec<usize>,
    boundaries: &mut Vec<SplitBoundary>,
    selection: Option<&Selection>,
    pending_resizes: &mut Vec<(PaneId, pyre_proto::PaneSize)>,
    anim_frame: u64,
    panes_meta: &[pyre_proto::PaneInfo],
    theme: &theme::LegacyTheme,
) {
    match node {
        LayoutNode::Leaf(pane_id) => {
            if let Some(&slot_idx) = pane_slot.get(pane_id) {
                if let Some(slot) = slots.get_mut(slot_idx).and_then(|s| s.as_mut()) {
                    let focused = *pane_id == focus_pane;
                    let attention = pane_needs_attention(panes_meta, slot.pane_id);
                    render_pane(
                        frame,
                        area,
                        slot,
                        focused,
                        selection,
                        slot_idx,
                        pending_resizes,
                        anim_frame,
                        attention,
                        theme,
                        panes_meta,
                    );
                }
            }
        }
        LayoutNode::HSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            // Collect horizontal boundaries between children.
            for i in 0..children.len().saturating_sub(1) {
                let boundary_row = rects[i].y + rects[i].height;
                boundaries.push(SplitBoundary {
                    coord: boundary_row,
                    is_hsplit: true,
                    parent_path: current_path.clone(),
                    child_idx: i,
                    parent_size: area.height,
                });
            }
            for (i, ((child, _), rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(
                    frame,
                    *rect,
                    child,
                    slots,
                    focus_pane,
                    pane_slot,
                    current_path,
                    boundaries,
                    selection,
                    pending_resizes,
                    anim_frame,
                    panes_meta,
                    theme,
                );
                current_path.pop();
            }
        }
        LayoutNode::VSplit(children) => {
            let constraints: Vec<Constraint> = children
                .iter()
                .map(|(_, w)| Constraint::Percentage(*w))
                .collect();
            let rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(area);
            // Collect vertical boundaries between children.
            for i in 0..children.len().saturating_sub(1) {
                let boundary_col = rects[i].x + rects[i].width;
                boundaries.push(SplitBoundary {
                    coord: boundary_col,
                    is_hsplit: false,
                    parent_path: current_path.clone(),
                    child_idx: i,
                    parent_size: area.width,
                });
            }
            for (i, ((child, _), rect)) in children.iter().zip(rects.iter()).enumerate() {
                current_path.push(i);
                render_layout(
                    frame,
                    *rect,
                    child,
                    slots,
                    focus_pane,
                    pane_slot,
                    current_path,
                    boundaries,
                    selection,
                    pending_resizes,
                    anim_frame,
                    panes_meta,
                    theme,
                );
                current_path.pop();
            }
        }
    }
}
