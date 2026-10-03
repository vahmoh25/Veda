//! The boot splash: a dark gradient with the Vindows logo, drawn directly
//! into the GOP framebuffer while the system loads.

pub struct Surface {
    pub base: *mut u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// `true` if the framebuffer stores R in the low byte.
    pub rgb: bool,
}

impl Surface {
    fn put(&self, x: u32, y: u32, c: u32) {
        let c = if self.rgb { (c & 0xFF00FF00) | ((c >> 16) & 0xFF) | ((c & 0xFF) << 16) } else { c };
        // SAFETY: callers keep (x, y) inside the framebuffer.
        unsafe { self.base.add((y * self.stride + x) as usize).write_volatile(c) }
    }
}

fn mix(a: u32, b: u32, t: u32) -> u32 {
    // t in 0..=256
    let ch = |s: u32| {
        let (x, y) = ((a >> s) & 0xFF, (b >> s) & 0xFF);
        ((x * (256 - t) + y * t) >> 8) << s
    };
    ch(16) | ch(8) | ch(0)
}

/// Coverage (0..=256) of a rounded rectangle at pixel centre (px, py).
fn rounded_coverage(px: i32, py: i32, x0: i32, y0: i32, size: i32, radius: i32) -> u32 {
    let (fx, fy) = (px * 4 + 2, py * 4 + 2); // quarter-pixel units
    let (rx0, ry0, rx1, ry1) = (x0 * 4, y0 * 4, (x0 + size) * 4, (y0 + size) * 4);
    let r = radius * 4;
    let cx = fx.clamp(rx0 + r, rx1 - r);
    let cy = fy.clamp(ry0 + r, ry1 - r);
    let (dx, dy) = (fx - cx, fy - cy);
    let d2 = dx * dx + dy * dy;
    let (inner, outer) = ((r - 2) * (r - 2), (r + 2) * (r + 2));
    if fx < rx0 - 2 || fx > rx1 + 2 || fy < ry0 - 2 || fy > ry1 + 2 || d2 >= outer {
        0
    } else if d2 <= inner {
        256
    } else {
        // Linear falloff across the one-pixel anti-aliasing band.
        (((outer - d2) * 256) / (outer - inner)) as u32
    }
}

/// Paints the background gradient and the logo, centred.
pub fn draw(s: &Surface) {
    let (w, h) = (s.width, s.height);
    let top = 0x070A16;
    let bottom = 0x121A35;
    let tile = (h / 12).clamp(28, 96) as i32;
    let gap = tile / 7;
    let radius = tile / 5;
    let total = tile * 2 + gap;
    let ox = (w as i32 - total) / 2;
    let oy = (h as i32 - total) / 2 - h as i32 / 16;
    // Tile gradients (top-left -> bottom-right), cyan through violet.
    let colors = [(0x2FD4FF, 0x3D8BFF), (0x4D7CFF, 0x7A5CFF), (0x3D8BFF, 0x6A63FF), (0x7A5CFF, 0xC04DFF)];

    for y in 0..h {
        let bg_row = mix(top, bottom, y * 256 / h.max(1));
        for x in 0..w {
            let mut c = bg_row;
            let (px, py) = (x as i32, y as i32);
            if px >= ox - 2 && px < ox + total + 2 && py >= oy - 2 && py < oy + total + 2 {
                for (i, &(c0, c1)) in colors.iter().enumerate() {
                    let tx = ox + (i as i32 % 2) * (tile + gap);
                    let ty = oy + (i as i32 / 2) * (tile + gap);
                    let cov = rounded_coverage(px, py, tx, ty, tile, radius);
                    if cov > 0 {
                        let t = (((px - tx) + (py - ty)).clamp(0, tile * 2) * 256 / (tile * 2)) as u32;
                        c = mix(c, mix(c0, c1, t), cov);
                    }
                }
            }
            s.put(x, y, c);
        }
    }
}
