//! Rasterize terminal cells into an RGBA buffer for softbuffer.

use crate::atlas::{GlyphAtlas, CELL_H, CELL_W};
use crate::term::CellRgb;

pub struct Painter {
    pub(crate) atlas: GlyphAtlas,
}

impl Painter {
    pub fn from_system() -> Option<Self> {
        GlyphAtlas::from_system().map(|atlas| Self { atlas })
    }

    pub fn paint(&mut self, cells: &[CellRgb], cols: usize, rows: usize, buffer: &mut [u32]) {
        let width = cols * CELL_W;
        let height = rows * CELL_H;
        debug_assert_eq!(buffer.len(), width * height);

        for (idx, cell) in cells.iter().enumerate() {
            let col = idx % cols;
            let row = idx / cols;
            self.atlas.paint_cell(
                buffer,
                width,
                height,
                col,
                row,
                cell.ch,
                cell.fg,
                cell.bg,
                cell.bold,
            );
        }
    }
}
