//! Application icons: every app is shown as a rounded gradient tile with a
//! white line glyph, coloured by app id.

use vgfx::{Canvas, Color, Rect};
use vproto::init::AppInfo;
use vui::Icon;

/// Preferred order of apps on the taskbar and in the start menu.
const ORDER: [&str; 10] =
    ["files", "editor", "photos", "music", "terminal", "settings", "taskmgr", "racer", "starfall", "prism"];

/// Sorts apps into their display order (unknown apps last, by name).
pub fn sort(apps: &mut [AppInfo]) {
    apps.sort_by_key(|a| (ORDER.iter().position(|id| *id == a.id).unwrap_or(ORDER.len()), a.name.to_lowercase()));
}

/// Gradient (top, bottom) of an app's tile.
pub fn tile_colors(id: &str) -> (Color, Color) {
    match id {
        "editor" => (Color::hex(0x5B95FF), Color::hex(0x2F5BDB)),
        "files" | "home" => (Color::hex(0xFFC857), Color::hex(0xF08A24)),
        "photos" => (Color::hex(0xFF7AB2), Color::hex(0xA64DFF)),
        "music" => (Color::hex(0xFF6B7A), Color::hex(0xD12F6E)),
        "terminal" => (Color::hex(0x454B59), Color::hex(0x1C1F26)),
        "settings" => (Color::hex(0x98A2B8), Color::hex(0x56607A)),
        "taskmgr" => (Color::hex(0x35D9AE), Color::hex(0x10897A)),
        "about" => (Color::hex(0x8C7CFF), Color::hex(0x4B3FD1)),
        "racer" => (Color::hex(0xFF9A4A), Color::hex(0xE0402F)),
        "starfall" => (Color::hex(0x7A6BFF), Color::hex(0x231C78)),
        "prism" => (Color::hex(0x5CE1E6), Color::hex(0x3A5BD9)),
        // The Trash: brushed metal, with a dark glyph (see `glyph_color`).
        "trash" => (Color::hex(0xEEF1F6), Color::hex(0xAAB3C3)),
        // Notifications about problems.
        "warning" => (Color::hex(0xFFC14D), Color::hex(0xE0761F)),
        _ => {
            // Stable colours for unknown apps, from a small palette.
            const PALETTE: [(u32, u32); 5] = [
                (0x4FC3F7, 0x1E88E5),
                (0x81C784, 0x2E7D32),
                (0xBA68C8, 0x6A1B9A),
                (0xFFB74D, 0xEF6C00),
                (0x4DB6AC, 0x00695C),
            ];
            let h = id.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
            let (a, b) = PALETTE[h as usize % PALETTE.len()];
            (Color::hex(a), Color::hex(b))
        }
    }
}

/// The glyph shown on an app's tile.
pub fn icon_for(app: &AppInfo) -> Icon {
    Icon::by_name(&app.icon).or_else(|| Icon::by_name(&app.id)).unwrap_or(Icon::Grid)
}

/// The colour of the glyph on a tile: white, but dark on the Trash's light
/// one.
fn glyph_color(id: &str) -> Color {
    if id == "trash" { Color::hex(0x3D4657) } else { Color::WHITE }
}

/// Draws a tile for app `id` with `icon` filling the square `r`.
pub fn draw_tile(c: &mut Canvas, r: Rect, id: &str, icon: Icon) {
    let (top, bottom) = tile_colors(id);
    let radius = r.w as f32 * 0.26;
    c.fill_rounded_rect_gradient(r, radius, top, bottom);
    // A soft highlight along the top edge gives the tile some depth.
    c.save();
    c.clip_to(Rect::new(r.x, r.y, r.w, r.h / 2));
    c.stroke_rounded_rect(r, radius, 1.0, Color::rgba(255, 255, 255, 46));
    c.restore();
    icon.draw(c, r, r.w as f32 * 0.58, glyph_color(id));
}

/// Case-insensitive match of a search query against an app.
pub fn matches(app: &AppInfo, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || app.name.to_lowercase().contains(&q)
        || app.id.to_lowercase().contains(&q)
        || app.category.to_lowercase().contains(&q)
        || app.description.to_lowercase().contains(&q)
}

/// Ranks a matching app: prefix matches of the name come first.
pub fn rank(app: &AppInfo, query: &str) -> u32 {
    let q = query.trim().to_lowercase();
    let name = app.name.to_lowercase();
    if name.starts_with(&q) {
        0
    } else if name.split(' ').any(|w| w.starts_with(&q)) {
        1
    } else if name.contains(&q) {
        2
    } else {
        3
    }
}
