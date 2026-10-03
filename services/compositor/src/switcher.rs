//! The Alt+Tab window switcher: while Alt is held, an overlay shows
//! thumbnails of the open windows (most recently used first); Tab and
//! Shift+Tab move the selection and releasing Alt activates it.

use alloc::vec::Vec;

use vgfx::{Bitmap, Rect};

use crate::window::Window;

/// Largest thumbnail size.
pub const THUMB_W: i32 = 200;
pub const THUMB_H: i32 = 126;
const CELL_W: i32 = 228;
const CELL_H: i32 = 184;
const PAD: i32 = 18;

pub struct Switcher {
    /// Candidate windows, most recently used first.
    pub windows: Vec<u32>,
    /// Snapshots of their contents, taken when the switcher opened.
    pub thumbs: Vec<Option<Bitmap>>,
    pub selected: usize,
    /// The overlay panel on screen.
    pub rect: Rect,
    per_row: i32,
}

impl Switcher {
    pub fn new(windows: Vec<u32>, thumbs: Vec<Option<Bitmap>>, selected: usize, screen: Rect) -> Switcher {
        let n = windows.len() as i32;
        let per_row = n.min(((screen.w - 80 - 2 * PAD) / CELL_W).max(1)).max(1);
        let rows = (n + per_row - 1) / per_row;
        let (w, h) = (per_row * CELL_W + 2 * PAD, rows * CELL_H + 2 * PAD);
        let rect = Rect::new(screen.x + (screen.w - w) / 2, screen.y + (screen.h - h) / 2, w, h);
        Switcher { windows, thumbs, selected, rect, per_row }
    }

    /// Moves the selection to the next (or previous) window.
    pub fn step(&mut self, backwards: bool) {
        let n = self.windows.len();
        self.selected = if backwards { (self.selected + n - 1) % n } else { (self.selected + 1) % n };
    }

    /// The cell of window `i`.
    pub fn cell(&self, i: usize) -> Rect {
        let (col, row) = (i as i32 % self.per_row, i as i32 / self.per_row);
        Rect::new(self.rect.x + PAD + col * CELL_W, self.rect.y + PAD + row * CELL_H, CELL_W, CELL_H)
    }

    /// Where the thumbnail of window `i` sits inside its cell.
    pub fn thumb_box(&self, i: usize) -> Rect {
        let c = self.cell(i);
        Rect::new(c.x + (CELL_W - THUMB_W) / 2, c.y + 14, THUMB_W, THUMB_H)
    }

    /// Everything the overlay paints, including its shadow.
    pub fn bounds(&self) -> Rect {
        self.rect.inflate(40)
    }
}

/// A small copy of a window's current frame (3x3 supersampled), or `None`
/// if it has not drawn anything yet.
pub fn thumbnail(w: &Window) -> Option<Bitmap> {
    let (buf, index) = (w.buffers.as_ref()?, w.current?);
    let (sw, sh) = (buf.width.max(1), buf.height.max(1));
    let scale = (THUMB_W as f32 / sw as f32).min(THUMB_H as f32 / sh as f32).min(1.0);
    let (tw, th) = (((sw as f32 * scale) as i32).max(1), ((sh as f32 * scale) as i32).max(1));
    let src = buf.pixels(index);
    let mut out = Bitmap::new(tw, th);
    for y in 0..th {
        for x in 0..tw {
            let mut acc = [0u32; 4];
            for sy in 0..3 {
                for sx in 0..3 {
                    let px = (((x * 3 + sx) as f32 + 0.5) * sw as f32 / (tw * 3) as f32) as i32;
                    let py = (((y * 3 + sy) as f32 + 0.5) * sh as f32 / (th * 3) as f32) as i32;
                    let p = src[(py.min(sh - 1) * buf.stride + px.min(sw - 1)) as usize];
                    acc[0] += p >> 24;
                    acc[1] += (p >> 16) & 0xFF;
                    acc[2] += (p >> 8) & 0xFF;
                    acc[3] += p & 0xFF;
                }
            }
            out.pixels[(y * tw + x) as usize] =
                (acc[0] / 9) << 24 | (acc[1] / 9) << 16 | (acc[2] / 9) << 8 | (acc[3] / 9);
        }
    }
    Some(out)
}
