//! Rasterize terminal cells into an RGBA buffer for softbuffer.

use crate::atlas::{GlyphAtlas, CELL_H, CELL_W};
use crate::layout::Rect;
use crate::term::CellRgb;

/// Theme-derived colours used when drawing pane borders.
pub struct BorderColors {
    /// Colour for the focused pane border (`[r, g, b, a]`).
    pub focused: [u8; 4],
    /// Colour for unfocused pane borders (`[r, g, b, a]`).
    pub unfocused: [u8; 4],
}

pub struct Painter {
    pub(crate) atlas: GlyphAtlas,
}

impl Painter {
    pub fn from_system() -> Option<Self> {
        GlyphAtlas::from_system().map(|atlas| Self { atlas })
    }

    /// Paint the entire framebuffer (single-pane legacy path).
    #[allow(dead_code)] // kept for search-overlay tests and future single-pane compat
    pub fn paint(&mut self, cells: &[CellRgb], cols: usize, rows: usize, buffer: &mut [u32]) {
        let width = cols * CELL_W;
        let height = rows * CELL_H;
        debug_assert_eq!(buffer.len(), width * height);

        for (idx, cell) in cells.iter().enumerate() {
            let col = idx % cols;
            let row = idx / cols;
            self.atlas.paint_cell(
                buffer, width, height, col, row, cell.ch, cell.fg, cell.bg, cell.bold,
            );
        }
    }

    #[allow(clippy::too_many_arguments)] // cell raster needs position + rect + grid dims
    /// Paint `cells` (a cols×rows grid) into a sub-`rect` of `buffer`.
    ///
    /// `buf_w` / `buf_h` are the full framebuffer dimensions in pixels.
    /// `rect` is expressed in pixels. `cells` are indexed row-major by
    /// `(col, row)` where `col ∈ [0, cell_cols)` and `row ∈ [0, cell_rows)`.
    /// `bg_fill` is `[r, g, b, a]` used to clear any rows not covered by cells.
    pub fn paint_pane_at(
        &mut self,
        buffer: &mut [u32],
        buf_w: usize,
        buf_h: usize,
        rect: Rect,
        cells: &[CellRgb],
        cell_cols: usize,
        cell_rows: usize,
        bg_fill: [u8; 4],
    ) {
        for (idx, cell) in cells.iter().enumerate() {
            let col = idx % cell_cols;
            let row = idx / cell_cols;
            // Translate local (col, row) to absolute pixel coordinate.
            let abs_col = rect.x as usize / CELL_W + col;
            let abs_row = rect.y as usize / CELL_H + row;
            // Guard: do not paint outside the rect.
            if abs_col * CELL_W >= rect.x as usize + rect.w as usize {
                continue;
            }
            if abs_row * CELL_H >= rect.y as usize + rect.h as usize {
                continue;
            }
            self.atlas.paint_cell(
                buffer, buf_w, buf_h, abs_col, abs_row, cell.ch, cell.fg, cell.bg, cell.bold,
            );
        }
        // Row padding: if the grid does not fill the rect vertically, clear
        // the remaining rows to the default background.
        let painted_h = cell_rows * CELL_H;
        let rect_h = rect.h as usize;
        if painted_h < rect_h {
            let [r, g, b, _a] = bg_fill;
            let fill_pixel = u32::from_be_bytes([0, r, g, b]);
            let y_start = rect.y as usize + painted_h;
            for y in y_start..rect.y as usize + rect_h {
                if y >= buf_h {
                    break;
                }
                for x in rect.x as usize..rect.x as usize + rect.w as usize {
                    if x >= buf_w {
                        break;
                    }
                    buffer[y * buf_w + x] = fill_pixel;
                }
            }
        }
    }

    /// Draw a 1-pixel border around `rect`.
    ///
    /// `colors` carries the focused/unfocused RGBA values sourced from the
    /// active theme palette.  `focused` selects which colour is applied.
    pub fn paint_border(
        &self,
        buffer: &mut [u32],
        buf_w: usize,
        buf_h: usize,
        rect: Rect,
        focused: bool,
        colors: &BorderColors,
    ) {
        let [r, g, b, _a] = if focused {
            colors.focused
        } else {
            colors.unfocused
        };
        let color = u32::from_be_bytes([0, r, g, b]);
        let x0 = rect.x as usize;
        let y0 = rect.y as usize;
        let x1 = (rect.x + rect.w).saturating_sub(1) as usize;
        let y1 = (rect.y + rect.h).saturating_sub(1) as usize;

        // Top and bottom horizontal lines.
        for x in x0..=x1 {
            if x < buf_w {
                if y0 < buf_h {
                    buffer[y0 * buf_w + x] = color;
                }
                if y1 < buf_h {
                    buffer[y1 * buf_w + x] = color;
                }
            }
        }
        // Left and right vertical lines.
        for y in y0..=y1 {
            if y < buf_h {
                if x0 < buf_w {
                    buffer[y * buf_w + x0] = color;
                }
                if x1 < buf_w {
                    buffer[y * buf_w + x1] = color;
                }
            }
        }
    }
}
