//! File-type icons: small line icons for the list view and large drawn
//! icons (folders, pages with a type badge, image thumbnails) for the grid.

use vgfx::{Bitmap, FillRule, Filter, Path};
use vui::{Align, Color, Font, Icon, Rect, Ui};

use vfiles::kind::{FileKind, file_kind};
use vfiles::path::extension;

pub const FOLDER: Color = Color::hex(0xF5B43C);
const IMAGE: Color = Color::hex(0xE879C9);
const AUDIO: Color = Color::hex(0xF2766B);
const TEXT: Color = Color::hex(0x6EA8FE);
const PROGRAM: Color = Color::hex(0x4CC38A);
const OTHER: Color = Color::hex(0x9A9AA8);

/// The accent colour of a file type.
pub fn kind_color(name: &str, is_dir: bool) -> Color {
    if is_dir {
        return FOLDER;
    }
    match file_kind(name) {
        FileKind::Image => IMAGE,
        FileKind::Audio => AUDIO,
        FileKind::Text => TEXT,
        FileKind::Program => PROGRAM,
        FileKind::Other => OTHER,
    }
}

/// The line icon of a file type.
pub fn small_icon(name: &str, is_dir: bool) -> Icon {
    if is_dir {
        return Icon::Folder;
    }
    match file_kind(name) {
        FileKind::Image => Icon::Image,
        FileKind::Audio => Icon::Music,
        FileKind::Text => Icon::Document,
        FileKind::Program => Icon::Cube,
        FileKind::Other => match extension(name).as_str() {
            "ttf" | "otf" => Icon::Edit,
            _ => Icon::File,
        },
    }
}

/// Draws a filled folder in `r` (square-ish).
pub fn draw_folder(ui: &mut Ui, r: Rect, opacity: f32) {
    let s = r.w.min(r.h) as f32;
    let x = r.x as f32 + (r.w as f32 - s) / 2.0;
    let y = r.y as f32 + (r.h as f32 - s) / 2.0;
    let rect = |fx: f32, fy: f32, fw: f32, fh: f32| {
        Rect::new((x + fx * s) as i32, (y + fy * s) as i32, (fw * s) as i32, (fh * s) as i32)
    };
    let back = Color::hex(0xDE9A2E).fade(opacity);
    ui.canvas.fill_rounded_rect(rect(0.06, 0.14, 0.40, 0.2), s * 0.06, back);
    ui.canvas.fill_rounded_rect(rect(0.06, 0.2, 0.88, 0.64), s * 0.07, back);
    ui.canvas.fill_rounded_rect(rect(0.06, 0.24, 0.88, 0.02), 0.0, Color::rgba(255, 255, 255, (90.0 * opacity) as u8));
    ui.canvas.fill_rounded_rect_gradient(
        rect(0.06, 0.3, 0.88, 0.56),
        s * 0.07,
        Color::hex(0xFFD36B).fade(opacity),
        Color::hex(0xF5AE3D).fade(opacity),
    );
}

/// Draws a document page with the file's type icon and extension badge.
pub fn draw_page(ui: &mut Ui, r: Rect, name: &str, opacity: f32) {
    let s = r.w.min(r.h) as f32;
    let x = r.x as f32 + (r.w as f32 - s) / 2.0;
    let y = r.y as f32 + (r.h as f32 - s) / 2.0;
    let page = Rect::new((x + s * 0.17) as i32, (y + s * 0.06) as i32, (s * 0.66) as i32, (s * 0.88) as i32);
    ui.canvas.draw_shadow(page.translate(0, 2), (s * 0.06) as i32, 6, Color::rgba(0, 0, 0, (70.0 * opacity) as u8));
    ui.canvas.fill_rounded_rect_gradient(
        page,
        s * 0.07,
        Color::hex(0xF4F5F9).fade(opacity),
        Color::hex(0xDADDE6).fade(opacity),
    );
    // Folded corner.
    let fold = s * 0.2;
    let (px, py) = (page.right() as f32, page.y as f32);
    let mut tri = Path::new();
    tri.move_to(px - fold, py);
    tri.line_to(px, py + fold);
    tri.line_to(px - fold + 2.0, py + fold);
    tri.close();
    ui.canvas.fill_path(&tri, Color::hex(0xB9BECC).fade(opacity), FillRule::NonZero);
    let color = kind_color(name, false).fade(opacity);
    let icon_r = Rect::new(page.x, page.y + (s * 0.1) as i32, page.w, (s * 0.42) as i32);
    small_icon(name, false).draw(&mut ui.canvas, icon_r, s * 0.34, color);
    let ext = extension(name).to_ascii_uppercase();
    if !ext.is_empty() && ext.len() <= 5 {
        let size = (s * 0.17).clamp(8.0, 13.0);
        let tw = ui.measure(&ext, Font::Bold, size) as i32 + 10;
        let badge = Rect::new(page.x + (page.w - tw) / 2, page.bottom() - (s * 0.3) as i32, tw, (size + 6.0) as i32);
        ui.canvas.fill_rounded_rect(badge, 4.0, color);
        ui.label(badge, &ext, Font::Bold, size, Color::WHITE.fade(opacity), Align::Center);
    }
}

/// Draws a thumbnail scaled to fit `r`, with a frame.
pub fn draw_thumbnail(ui: &mut Ui, r: Rect, bmp: &Bitmap, opacity: f32) {
    let (bw, bh) = (bmp.width.max(1), bmp.height.max(1));
    let scale = (r.w as f32 / bw as f32).min(r.h as f32 / bh as f32).min(1.5);
    let (w, h) = (((bw as f32 * scale) as i32).max(1), ((bh as f32 * scale) as i32).max(1));
    let dst = Rect::new(r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h);
    ui.canvas.draw_shadow(dst.translate(0, 2), 3, 6, Color::rgba(0, 0, 0, (90.0 * opacity) as u8));
    let alpha = (opacity * 255.0) as u8;
    if w == bw && h == bh {
        ui.canvas.draw_bitmap(bmp, dst.x, dst.y, alpha);
    } else {
        ui.canvas.draw_bitmap_scaled(bmp, dst, Filter::Bilinear, alpha);
    }
    ui.canvas.stroke_rounded_rect(dst.inflate(1), 2.0, 1.0, Color::rgba(255, 255, 255, (40.0 * opacity) as u8));
}

/// Draws the large icon of an entry.
pub fn draw_large(ui: &mut Ui, r: Rect, name: &str, is_dir: bool, opacity: f32) {
    if is_dir {
        draw_folder(ui, r, opacity);
    } else {
        draw_page(ui, r, name, opacity);
    }
}
