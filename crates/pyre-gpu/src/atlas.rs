//! Monospace glyph cache (fontdue + system font via fontdb).

use std::collections::HashMap;

use fontdue::{Font, FontSettings};

pub const CELL_W: usize = 10;
pub const CELL_H: usize = 20;
const FONT_PX: f32 = 14.0;

#[derive(Clone)]
struct CachedGlyph {
    width: usize,
    height: usize,
    left: i32,
    top: i32,
    pixels: Vec<u8>,
}

pub struct GlyphAtlas {
    font: Font,
    cache: HashMap<(char, bool), CachedGlyph>,
}

impl GlyphAtlas {
    pub fn from_system() -> Option<Self> {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let face = db.faces().find(|f| f.monospaced)?;
        let data = db.with_face_data(face.id, |bytes, _| bytes.to_vec())?;
        let font = Font::from_bytes(data, FontSettings::default()).ok()?;
        Some(Self {
            font,
            cache: HashMap::new(),
        })
    }

    fn glyph(&mut self, ch: char, bold: bool) -> &CachedGlyph {
        let key = (ch, bold);
        if !self.cache.contains_key(&key) {
            let (metrics, bitmap) = self.font.rasterize(ch, FONT_PX);
            let g = CachedGlyph {
                width: metrics.width,
                height: metrics.height,
                left: metrics.xmin,
                top: metrics.ymin,
                pixels: bitmap,
            };
            self.cache.insert(key, g);
        }
        self.cache.get(&key).expect("just inserted")
    }

    #[allow(clippy::too_many_arguments)] // cell raster needs position + colors + glyph
    pub fn paint_cell(
        &mut self,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        col: usize,
        row: usize,
        ch: char,
        fg: [u8; 3],
        bg: [u8; 3],
        bold: bool,
    ) {
        let x0 = col * CELL_W;
        let y0 = row * CELL_H;
        let bg_px = rgb_u32(bg);
        for dy in 0..CELL_H {
            let y = y0 + dy;
            if y >= height {
                break;
            }
            let start = y * width + x0;
            let end = (start + CELL_W).min(y * width + width);
            buffer[start..end].fill(bg_px);
        }
        if ch == ' ' {
            return;
        }
        let g = self.glyph(ch, bold).clone();
        let fg_px = rgb_u32(fg);
        let baseline = y0 + CELL_H.saturating_sub(4);
        for gy in 0..g.height {
            let py = baseline as i32 - g.top - gy as i32;
            if py < 0 || py as usize >= height {
                continue;
            }
            for gx in 0..g.width {
                let alpha = g.pixels[gy * g.width + gx];
                if alpha == 0 {
                    continue;
                }
                let px = x0 as i32 + g.left + gx as i32;
                if px < 0 || px as usize >= width {
                    continue;
                }
                let idx = py as usize * width + px as usize;
                buffer[idx] = blend(fg_px, bg_px, alpha);
            }
        }
    }
}

fn rgb_u32(rgb: [u8; 3]) -> u32 {
    u32::from(rgb[0]) << 16 | u32::from(rgb[1]) << 8 | u32::from(rgb[2])
}

fn blend(fg: u32, bg: u32, alpha: u8) -> u32 {
    let a = f32::from(alpha) / 255.0;
    let fr = ((fg >> 16) & 0xff) as f32;
    let fg_g = ((fg >> 8) & 0xff) as f32;
    let fb = (fg & 0xff) as f32;
    let br = ((bg >> 16) & 0xff) as f32;
    let bg_g = ((bg >> 8) & 0xff) as f32;
    let bb = (bg & 0xff) as f32;
    let r = (fr * a + br * (1.0 - a)) as u32;
    let g = (fg_g * a + bg_g * (1.0 - a)) as u32;
    let b = (fb * a + bb * (1.0 - a)) as u32;
    (r << 16) | (g << 8) | b
}

pub fn grid_dims_for_window(win_w: u32, win_h: u32) -> (usize, usize) {
    let cols = (win_w as usize / CELL_W).max(20);
    let rows = (win_h as usize / CELL_H).max(8);
    (cols, rows)
}
