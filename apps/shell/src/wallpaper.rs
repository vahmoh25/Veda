//! Desktop wallpapers: loading from image files, a procedural default, and
//! the blurred copy used for translucent ("mica") shell surfaces.

use alloc::string::String;
use alloc::vec::Vec;

use vgfx::Bitmap;
use vmath::FloatExt;

/// Name of the built-in procedural wallpaper (used in place of a path).
pub const PROCEDURAL: &str = "procedural";

/// The wallpaper at screen resolution plus a heavily blurred version.
pub struct Wallpaper {
    /// File it came from, or [`PROCEDURAL`].
    pub path: String,
    pub image: Bitmap,
    pub blurred: Bitmap,
}

/// Cheap, smooth blur: shrink 8x, blur, then scale back up.
fn soft_blur(src: &Bitmap) -> Bitmap {
    let (w, h) = ((src.width / 8).max(1), (src.height / 8).max(1));
    let mut small = src.resized(w, h);
    small.blur(3, 3);
    small.resized(src.width, src.height)
}

impl Wallpaper {
    pub fn from_bitmap(path: &str, image: Bitmap) -> Wallpaper {
        let blurred = soft_blur(&image);
        Wallpaper { path: path.into(), image, blurred }
    }

    /// Decodes an image file and scales it to cover a `w` x `h` screen.
    pub fn load(path: &str, bytes: &[u8], w: i32, h: i32) -> Option<Wallpaper> {
        let img = vimage::decode(bytes).ok()?;
        let src = Bitmap::from_straight(img.width as i32, img.height as i32, img.pixels);
        Some(Wallpaper::from_bitmap(path, src.cover(w, h)))
    }

    /// Deep blue with luminous blooms and a ribbon of light, used when no
    /// wallpaper files are installed. Rendered at half resolution (it is
    /// all soft gradients) and dithered at full resolution.
    pub fn procedural(w: i32, h: i32) -> Wallpaper {
        let (hw, hh) = ((w / 2).max(1), (h / 2).max(1));
        let (fw, fh) = (hw as f32, hh as f32);
        // (centre x, centre y, radius, r, g, b, strength), in screen fractions.
        let blooms: [(f32, f32, f32, f32, f32, f32, f32); 4] = [
            (0.22, 0.30, 0.65, 59.0, 91.0, 255.0, 0.50),
            (0.80, 0.62, 0.60, 164.0, 59.0, 255.0, 0.42),
            (0.58, 0.12, 0.42, 31.0, 209.0, 255.0, 0.28),
            (0.45, 0.95, 0.50, 255.0, 70.0, 160.0, 0.18),
        ];
        // The ribbon's centre line depends only on x.
        let wave: Vec<f32> = (0..hw)
            .map(|x| x as f32 / fw)
            .map(|fx| 0.56 + 0.10 * (fx * 5.2 + 0.8).sin() + 0.04 * (fx * 13.0).sin())
            .collect();
        let mut small = Bitmap::new(hw, hh);
        for y in 0..hh {
            let fy = y as f32 / fh;
            let base = (11.0 + 15.0 * fy, 16.0 + 4.0 * fy, 38.0 + 26.0 * fy);
            for x in 0..hw {
                let fx = x as f32 / fw;
                let (mut r, mut g, mut b) = base;
                for &(cx, cy, rad, cr, cg, cb, s) in &blooms {
                    let dx = (fx - cx) * fw / fh;
                    let dy = fy - cy;
                    let d2 = (dx * dx + dy * dy) / (rad * rad);
                    if d2 < 1.0 {
                        let k = (1.0 - d2) * (1.0 - d2) * s;
                        r += (cr - r) * k;
                        g += (cg - g) * k;
                        b += (cb - b) * k;
                    }
                }
                let d = (fy - wave[x as usize]) / 0.045;
                let rk = 0.55 / (1.0 + d * d);
                r += (90.0 + 120.0 * fx - r) * rk;
                g += (200.0 - 60.0 * fx - g) * rk;
                b += (255.0 - b) * rk;
                // A thin bright core.
                let core = 0.35 / (1.0 + 16.0 * d * d);
                r += (255.0 - r) * core;
                g += (255.0 - g) * core;
                b += (255.0 - b) * core;
                let px = |v: f32| v.clamp(0.0, 255.0) as u32;
                small.pixels[(y * hw + x) as usize] = 0xFF00_0000 | px(r) << 16 | px(g) << 8 | px(b);
            }
        }
        let mut image = small.resized(w, h);
        // Dither to hide banding in the gradients.
        let mut seed = 0x1234_5678u32;
        for p in image.pixels.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let n = (seed & 3) as i32 - 1;
            let ch = |v: u32| (v as i32 + n).clamp(0, 255) as u32;
            *p = 0xFF00_0000 | ch(*p >> 16 & 0xFF) << 16 | ch(*p >> 8 & 0xFF) << 8 | ch(*p & 0xFF);
        }
        Wallpaper::from_bitmap(PROCEDURAL, image)
    }
}

/// Wallpaper files shipped with the system, sorted by name.
pub fn list(vfs: &vproto::vfs::Client) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(Ok(entries)) = vfs.read_dir("/system/wallpapers".into()) {
        for e in entries.iter().filter(|e| !e.is_dir) {
            let lower = e.name.to_lowercase();
            if [".png", ".jpg", ".jpeg", ".bmp", ".qoi"].iter().any(|ext| lower.ends_with(ext)) {
                out.push(alloc::format!("/system/wallpapers/{}", e.name));
            }
        }
    }
    out.sort();
    out
}
