//! The boot splash: a dark gradient with the Veda logo (OS1's white
//! ring from "Her"), drawn directly into the GOP framebuffer while the
//! system loads.

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

/// The integer square root of `n`.
fn isqrt(n: u32) -> u32 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Paints the background gradient and the logo, centred: a white ring a
/// quarter of its radius wide, as large as the old four tiles were.
pub fn draw(s: &Surface) {
    let (w, h) = (s.width, s.height);
    let top = 0x070A16;
    let bottom = 0x121A35;
    // Radii and distances in sixteenths of a pixel.
    let outer = (h / 12).clamp(28, 96) as i32 * 16;
    let inner = outer * 7 / 9;
    let cx = w as i32 * 8;
    let cy = (h as i32 / 2 - h as i32 / 16) * 16;
    for y in 0..h {
        let bg_row = mix(top, bottom, y * 256 / h.max(1));
        let dy = y as i32 * 16 + 8 - cy;
        for x in 0..w {
            let mut c = bg_row;
            let dx = x as i32 * 16 + 8 - cx;
            if dx.abs() <= outer + 16 && dy.abs() <= outer + 16 {
                let d = isqrt((dx * dx + dy * dy) as u32) as i32;
                // Covered across a one-pixel band at each edge.
                let cov = ((outer + 8 - d).clamp(0, 16) * (d - inner + 8).clamp(0, 16)) as u32;
                if cov > 0 {
                    c = mix(c, 0xFFFFFF, cov);
                }
            }
            s.put(x, y, c);
        }
    }
}
