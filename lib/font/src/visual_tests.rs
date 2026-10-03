//! Visual checks: render sample text with every bundled font to PNG files for inspection.
//!
//! Run with `cargo test -p vfont -- --ignored --nocapture`; images are written to
//! `target/agents/vfont/samples/` (relative to the workspace root).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::tests::ALL;
use crate::*;

pub(crate) const SAMPLE: &str = "The quick brown fox jumps over the lazy dog 0123456789 — Vindows €$@&!?";
pub(crate) const UI_SAMPLE: &str = "File  Edit  View  Settings  Music  Photos";

/// Minimal PNG encoder (RGB8, stored deflate blocks).
pub(crate) fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, t) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *t = c;
        }
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }
    fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = Vec::with_capacity(4 + data.len());
        body.extend_from_slice(tag);
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * stride..(y + 1) * stride]);
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(if i + 1 == chunks.len() { 1 } else { 0 });
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &v in &raw {
        a = (a + v as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// An RGB image.
pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32, bg: [u8; 3]) -> Self {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width * height {
            rgb.extend_from_slice(&bg);
        }
        Canvas { width, height, rgb }
    }

    /// Blends a glyph bitmap with its top-left corner at `(x, y)` (sRGB-space blending, like a
    /// simple compositor).
    pub fn draw(&mut self, bmp: &GlyphBitmap, x: i32, y: i32, color: [u8; 3]) {
        for by in 0..bmp.height as i32 {
            let py = y + by;
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            for bx in 0..bmp.width as i32 {
                let px = x + bx;
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                let a = bmp.get(bx as u32, by as u32) as u32;
                if a == 0 {
                    continue;
                }
                let i = ((py as u32 * self.width + px as u32) * 3) as usize;
                for (c, &s) in color.iter().enumerate() {
                    let d = self.rgb[i + c] as u32;
                    self.rgb[i + c] = ((s as u32 * a + d * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }

    pub fn zoomed(&self, x0: u32, y0: u32, w: u32, h: u32, k: u32) -> Canvas {
        let w = w.min(self.width - x0);
        let h = h.min(self.height - y0);
        let mut out = Canvas::new(w * k, h * k, [0, 0, 0]);
        for y in 0..h * k {
            for x in 0..w * k {
                let (sx, sy) = (x0 + x / k, y0 + y / k);
                let si = ((sy * self.width + sx) * 3) as usize;
                let di = ((y * w * k + x) * 3) as usize;
                out.rgb[di..di + 3].copy_from_slice(&self.rgb[si..si + 3]);
            }
        }
        out
    }

    pub fn save(&self, name: &str) {
        let dir = String::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/agents/vfont/samples"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = format!("{dir}/{name}");
        std::fs::write(&path, encode_png(self.width, self.height, &self.rgb)).unwrap();
        std::println!("wrote {path}");
    }
}

/// Draws one line of text with its baseline at `baseline`; returns the end x.
pub(crate) fn draw_line(
    cv: &mut Canvas,
    cache: &mut GlyphCache,
    style: &ScaledFont<'_, '_>,
    text: &str,
    x: f32,
    baseline: i32,
    color: [u8; 3],
) -> f32 {
    cache.render_line(style, text, Point::new(x, baseline as f32), |gx, gy, bmp| cv.draw(bmp, gx, gy, color))
}

/// Renders the sample page for collection font `primary` and returns (canvas, y ranges of the
/// 12/14 px blocks for zooming).
fn render_page(
    fonts: &FontCollection<'_>,
    primary: usize,
    options: RasterOptions,
    fg: [u8; 3],
    bg: [u8; 3],
    width: u32,
) -> (Canvas, Vec<(u32, u32)>) {
    let mut cache = GlyphCache::with_options(8 << 20, options);
    let sizes = [12.0f32, 14.0, 18.0, 32.0, 64.0];
    // Pre-compute the height.
    let mut height = 16.0;
    for &s in &sizes {
        let st = fonts.scaled(primary, s);
        let lines =
            st.wrap_lines(SAMPLE, width as f32 - 20.0).len() + st.wrap_lines(UI_SAMPLE, width as f32 - 20.0).len();
        height += vraster::math::ceil(st.line_height()) * lines as f32 + 10.0;
    }
    let mut cv = Canvas::new(width, height as u32 + 10, bg);
    let mut y = 8.0f32;
    let mut blocks = Vec::new();
    for &s in &sizes {
        let st = fonts.scaled(primary, s);
        let lh = vraster::math::ceil(st.line_height());
        let top = y as u32;
        for text in [SAMPLE, UI_SAMPLE] {
            for range in st.wrap_lines(text, width as f32 - 20.0) {
                let baseline = vraster::math::round(y + st.ascent()) as i32;
                draw_line(&mut cv, &mut cache, &st, &text[range], 10.0, baseline, fg);
                y += lh;
            }
        }
        y += 10.0;
        blocks.push((top, y as u32));
    }
    (cv, blocks)
}

fn collection_with_primary(primary: usize) -> FontCollection<'static> {
    let mut fonts = FontCollection::new();
    fonts.add(Font::from_bytes(ALL[primary].1).unwrap());
    for (i, (_, data)) in ALL.iter().enumerate() {
        if i != primary {
            fonts.add(Font::from_bytes(data).unwrap());
        }
    }
    fonts
}

#[test]
#[ignore]
fn render_samples_png() {
    let dark = [0x22, 0x22, 0x22];
    let white = [0xFF, 0xFF, 0xFF];
    let light_fg = [0xF2, 0xF2, 0xF2];
    let dark_bg = [0x1E, 0x20, 0x24];
    for (i, (name, _)) in ALL.iter().enumerate() {
        let fonts = collection_with_primary(i);
        let (cv, blocks) = render_page(&fonts, 0, RasterOptions::default(), dark, white, 1400);
        cv.save(&format!("{name}_light.png"));
        let (y0, _) = blocks[0];
        let (_, y1) = blocks[1];
        cv.zoomed(0, y0, 460, y1 - y0, 3).save(&format!("{name}_zoom_light.png"));
        let (cv, blocks) = render_page(&fonts, 0, RasterOptions::default(), light_fg, dark_bg, 1400);
        cv.save(&format!("{name}_dark.png"));
        let (y0, _) = blocks[0];
        let (_, y1) = blocks[1];
        cv.zoomed(0, y0, 460, y1 - y0, 3).save(&format!("{name}_zoom_dark.png"));
    }
}

/// Pixel-level detail: small crops of 12/14 px text at 5x zoom for each font and option set.
#[test]
#[ignore]
fn render_detail_png() {
    let variants: [(&str, RasterOptions); 3] = [
        ("linear", RasterOptions::LINEAR),
        ("default", RasterOptions::default()),
        ("gamma", RasterOptions { gamma: 1.3, ..RasterOptions::default() }),
    ];
    for (fi, (name, _)) in ALL.iter().enumerate() {
        let fonts = collection_with_primary(fi);
        for (vname, opts) in &variants {
            let mut cv = Canvas::new(150, 84, [255, 255, 255]);
            for yy in 42..84 {
                for xx in 0..150 {
                    let i = ((yy * 150 + xx) * 3) as usize;
                    cv.rgb[i..i + 3].copy_from_slice(&[0x1E, 0x20, 0x24]);
                }
            }
            let mut cache = GlyphCache::with_options(1 << 20, *opts);
            for (k, (y0, fg)) in [(0.0f32, [0x22u8, 0x22, 0x22]), (42.0, [0xF2, 0xF2, 0xF2])].iter().enumerate() {
                let _ = k;
                let st = fonts.scaled(0, 12.0);
                let b = vraster::math::round(y0 + 2.0 + st.ascent()) as i32;
                draw_line(&mut cv, &mut cache, &st, "Settings Edit gyp", 3.0, b, *fg);
                let st = fonts.scaled(0, 14.0);
                let b2 = b + vraster::math::ceil(st.line_height()) as i32 - 1;
                draw_line(&mut cv, &mut cache, &st, "Quick fox 0123", 3.0, b2, *fg);
            }
            cv.zoomed(0, 0, 150, 84, 5).save(&format!("detail_{name}_{vname}.png"));
        }
    }
}

/// Kerning on/off, accented (composite) glyphs, fallback glyphs, code and large curves.
#[test]
#[ignore]
fn render_showcase_png() {
    let mut fonts = FontCollection::new();
    let inter = fonts.add(Font::from_bytes(crate::tests::INTER).unwrap());
    let lato = fonts.add(Font::from_bytes(crate::tests::LATO).unwrap());
    let jbm = fonts.add(Font::from_bytes(crate::tests::JBM).unwrap());
    let mut cv = Canvas::new(1000, 640, [255, 255, 255]);
    let mut cache = GlyphCache::new(8 << 20);
    let ink = [0x22, 0x22, 0x22];
    let blue = [0x20, 0x50, 0xB0];
    let mut y = 6.0f32;
    let mut line = |cv: &mut Canvas, st: ScaledFont<'_, '_>, text: &str, color: [u8; 3], y: &mut f32| {
        let b = vraster::math::round(*y + st.ascent()) as i32;
        draw_line(cv, &mut cache, &st, text, 10.0, b, color);
        *y += vraster::math::ceil(st.line_height());
    };
    let kern = "AVATAR Toyota Te Ta We Yo LT P. F. Vindows";
    for f in [inter, lato] {
        line(&mut cv, fonts.scaled(f, 26.0), kern, ink, &mut y);
        line(&mut cv, fonts.scaled(f, 26.0).with_kerning(false), kern, blue, &mut y);
    }
    let accents = "Ångström façade naïve über São Paulo Łódź Œuvre Ærø «1–2» “quote” ‘a’ … ©®™ ½ ¿¡";
    line(&mut cv, fonts.scaled(inter, 18.0), accents, ink, &mut y);
    line(&mut cv, fonts.scaled(lato, 18.0), accents, ink, &mut y);
    line(
        &mut cv,
        fonts.scaled(inter, 18.0),
        "Fallback: ┌──┐ │ok│ └──┘ ▲ ◆ ★ ⌘ ⏎ ← → ↑ ↓ ∑ √ ∞ ≠ ≥ λ π Ω Ж ж",
        ink,
        &mut y,
    );
    line(&mut cv, fonts.scaled(jbm, 16.0), "fn main() { let x = 0x1F; // -> != >= || && |> === }", ink, &mut y);
    line(
        &mut cv,
        fonts.scaled(inter, 13.0),
        "Small UI: Save  Cancel  Apply  OK  Open recent…  Preferences  100%  12:45",
        ink,
        &mut y,
    );
    line(
        &mut cv,
        fonts.scaled(inter, 11.0),
        "Tiny 11px: The quick brown fox jumps over the lazy dog — 0123456789",
        ink,
        &mut y,
    );
    line(&mut cv, fonts.scaled(inter, 110.0), "Rag&@€g", ink, &mut y);
    cv.save("showcase.png");
    cv.zoomed(0, 0, 500, 200, 2).save("showcase_kerning_zoom.png");
}

/// 8x crops of a few words at 12 px to inspect individual pixels.
#[test]
#[ignore]
fn render_pixels_png() {
    for (fi, short) in [(0usize, "inter"), (2, "lato"), (4, "jbm")] {
        let fonts = collection_with_primary(fi);
        let snap = RasterOptions { vertical_snap_size: 24.0, ..RasterOptions::default() };
        let snap_lin = RasterOptions { vertical_snap_size: 24.0, ..RasterOptions::LINEAR };
        for (vname, opts) in [
            ("linear", RasterOptions::LINEAR),
            ("default", RasterOptions::default()),
            ("snap", snap),
            ("snaplin", snap_lin),
        ] {
            let mut cv = Canvas::new(64, 34, [255, 255, 255]);
            let mut cache = GlyphCache::with_options(1 << 20, opts);
            let st = fonts.scaled(0, 12.0);
            draw_line(&mut cv, &mut cache, &st, "minus", 2.0, 12, [0x22, 0x22, 0x22]);
            draw_line(&mut cv, &mut cache, &st, "Edge 4", 2.0, 28, [0x22, 0x22, 0x22]);
            cv.zoomed(0, 0, 64, 34, 8).save(&format!("pixels_{short}_{vname}.png"));
        }
    }
}

/// Side-by-side comparison of rendering options at small sizes (Inter and Lato).
#[test]
#[ignore]
fn render_options_comparison_png() {
    let variants: [(&str, RasterOptions); 4] = [
        ("linear", RasterOptions::LINEAR),
        ("default", RasterOptions::default()),
        ("dark05", RasterOptions { darkening: 0.5, ..RasterOptions::default() }),
        ("gamma14", RasterOptions { gamma: 1.4, darkening: 0.0, ..RasterOptions::default() }),
    ];
    const BAND: u32 = 64;
    for (fi, short) in [(0usize, "inter"), (2, "lato"), (4, "jbm")] {
        let fonts = collection_with_primary(fi);
        let mut cv = Canvas::new(520, BAND * 8 + 8, [255, 255, 255]);
        let mut band = 4u32;
        for (bg, fg) in [([255u8, 255, 255], [0x22u8, 0x22, 0x22]), ([0x1E, 0x20, 0x24], [0xF2, 0xF2, 0xF2])] {
            for (_, opts) in &variants {
                let mut cache = GlyphCache::with_options(1 << 20, *opts);
                for yy in band..(band + BAND).min(cv.height) {
                    for xx in 0..cv.width {
                        let i = ((yy * cv.width + xx) * 3) as usize;
                        cv.rgb[i..i + 3].copy_from_slice(&bg);
                    }
                }
                let mut y = band as f32 + 2.0;
                for &s in &[12.0f32, 14.0, 16.0] {
                    let st = fonts.scaled(0, s);
                    let baseline = vraster::math::round(y + st.ascent()) as i32;
                    draw_line(&mut cv, &mut cache, &st, "Settings File Edit Help — quick brown fox", 6.0, baseline, fg);
                    y += vraster::math::ceil(st.line_height());
                }
                band += BAND;
            }
        }
        cv.save(&format!("options_{short}.png"));
        cv.zoomed(0, 0, 260, cv.height, 3).save(&format!("options_{short}_zoom.png"));
    }
}
