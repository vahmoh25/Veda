//! Float image buffers, colour helpers, parallel rendering and output encoding.

use vimage::Image;

/// A colour with channels in `[0, 1]` (display space, values above 1 are allowed while
/// accumulating light).
pub type Rgb = [f32; 3];

/// `0xRRGGBB` to a colour.
pub fn hex(c: u32) -> Rgb {
    [((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0]
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

pub fn add(a: Rgb, b: Rgb) -> Rgb {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn mul(a: Rgb, k: f32) -> Rgb {
    [a[0] * k, a[1] * k, a[2] * k]
}

pub fn mulc(a: Rgb, b: Rgb) -> Rgb {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}

pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Multi-stop gradient lookup: `stops` are `(position, colour)` sorted by position.
pub fn gradient(stops: &[(f32, Rgb)], t: f32) -> Rgb {
    if t <= stops[0].0 {
        return stops[0].1;
    }
    for w in stops.windows(2) {
        let ((p0, c0), (p1, c1)) = (w[0], w[1]);
        if t <= p1 {
            let k = smoothstep(0.0, 1.0, (t - p0) / (p1 - p0).max(1e-6));
            return mix(c0, c1, k);
        }
    }
    stops[stops.len() - 1].1
}

/// Soft exponential tone mapping that keeps colours below ~0.7 almost unchanged and rolls bright
/// light off smoothly instead of clipping.
pub fn tonemap(c: Rgb) -> Rgb {
    c.map(|v| if v <= 0.7 { v.max(0.0) } else { 0.7 + 0.3 * (1.0 - (-(v - 0.7) / 0.3).exp()) })
}

/// A floating-point image.
#[derive(Clone)]
pub struct Buf {
    pub w: usize,
    pub h: usize,
    pub px: Vec<Rgb>,
    /// Optional alpha channel (`[0, 1]`).
    pub alpha: Option<Vec<f32>>,
}

impl Buf {
    pub fn new(w: usize, h: usize) -> Buf {
        Buf { w, h, px: vec![[0.0; 3]; w * h], alpha: None }
    }

    pub fn get(&self, x: usize, y: usize) -> Rgb {
        self.px[y * self.w + x]
    }

    /// Bilinear sample with clamped edges (pixel centres at +0.5).
    pub fn sample(&self, x: f32, y: f32) -> Rgb {
        let x = (x - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (y - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let top = mix(self.get(x0, y0), self.get(x1, y0), fx);
        let bottom = mix(self.get(x0, y1), self.get(x1, y1), fx);
        mix(top, bottom, fy)
    }

    /// Separable box blur repeated three times (close to a Gaussian of sigma ~ radius).
    pub fn blur(&mut self, radius: usize) {
        if radius == 0 {
            return;
        }
        for _ in 0..3 {
            self.box_pass(radius, true);
            self.box_pass(radius, false);
        }
    }

    fn box_pass(&mut self, r: usize, horizontal: bool) {
        let (len, lines) = if horizontal { (self.w, self.h) } else { (self.h, self.w) };
        let idx = |line: usize, i: usize| if horizontal { line * self.w + i } else { i * self.w + line };
        let src = self.px.clone();
        let div = (2 * r + 1) as f32;
        for line in 0..lines {
            let at = |i: isize| src[idx(line, i.clamp(0, len as isize - 1) as usize)];
            let mut acc = [0.0f32; 3];
            for i in -(r as isize)..=(r as isize) {
                acc = add(acc, at(i));
            }
            for i in 0..len {
                self.px[idx(line, i)] = mul(acc, 1.0 / div);
                let (a, s) = (at(i as isize + r as isize + 1), at(i as isize - r as isize));
                acc = [acc[0] + a[0] - s[0], acc[1] + a[1] - s[1], acc[2] + a[2] - s[2]];
            }
        }
    }

    /// Averages `f x f` blocks (anti-aliasing after supersampled rendering).
    pub fn downsample(&self, f: usize) -> Buf {
        let (w, h) = (self.w / f, self.h / f);
        let mut out = Buf::new(w, h);
        let k = 1.0 / (f * f) as f32;
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0; 3];
                for dy in 0..f {
                    for dx in 0..f {
                        acc = add(acc, self.get(x * f + dx, y * f + dy));
                    }
                }
                out.px[y * w + x] = mul(acc, k);
            }
        }
        if let Some(a) = &self.alpha {
            let mut oa = vec![0.0; w * h];
            for y in 0..h {
                for x in 0..w {
                    let mut s = 0.0;
                    for dy in 0..f {
                        for dx in 0..f {
                            s += a[(y * f + dy) * self.w + x * f + dx];
                        }
                    }
                    oa[y * w + x] = s * k;
                }
            }
            out.alpha = Some(oa);
        }
        out
    }

    /// Converts to an 8-bit image with a little triangular dither (hides banding in smooth
    /// gradients). Colours are expected in display space.
    pub fn to_image(&self, seed: u32) -> Image {
        let mut pixels = Vec::with_capacity(self.w * self.h);
        for (i, c) in self.px.iter().enumerate() {
            let (x, y) = ((i % self.w) as i32, (i / self.w) as i32);
            let d = crate::noise::hash01(x, y, seed) + crate::noise::hash01(x, y, seed ^ 0xABCD) - 1.0;
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5 + d * 0.75).clamp(0.0, 255.0) as u32;
            let a = self.alpha.as_ref().map_or(255, |a| (a[i].clamp(0.0, 1.0) * 255.0 + 0.5) as u32);
            pixels.push(a << 24 | q(c[0]) << 16 | q(c[1]) << 8 | q(c[2]));
        }
        Image::from_pixels(self.w as u32, self.h as u32, pixels).expect("pixel count matches")
    }
}

/// Renders `w x h` pixels in parallel; `f(x, y)` receives pixel-centre coordinates. With
/// `ss > 1` every pixel is the average of `ss x ss` samples.
pub fn render(w: usize, h: usize, ss: usize, f: impl Fn(f32, f32) -> Rgb + Sync) -> Buf {
    let mut buf = Buf::new(w, h);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let rows_per = h.div_ceil(threads * 4).max(1);
    let chunks: Vec<(usize, &mut [Rgb])> = buf.px.chunks_mut(rows_per * w).enumerate().collect();
    let chunks = std::sync::Mutex::new(chunks);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let item = chunks.lock().unwrap().pop();
                    let Some((ci, rows)) = item else { break };
                    for (ri, row) in rows.chunks_mut(w).enumerate() {
                        let y = ci * rows_per + ri;
                        for (x, out) in row.iter_mut().enumerate() {
                            *out = if ss <= 1 {
                                f(x as f32 + 0.5, y as f32 + 0.5)
                            } else {
                                let mut acc = [0.0; 3];
                                for sy in 0..ss {
                                    for sx in 0..ss {
                                        let ox = (sx as f32 + 0.5) / ss as f32;
                                        let oy = (sy as f32 + 0.5) / ss as f32;
                                        acc = add(acc, f(x as f32 + ox, y as f32 + oy));
                                    }
                                }
                                mul(acc, 1.0 / (ss * ss) as f32)
                            };
                        }
                    }
                }
            });
        }
    });
    buf
}

/// Inserts an EXIF `APP1` segment with an orientation tag right after the JPEG `SOI` marker.
pub fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut app1 = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
    app1.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]);
    app1.extend_from_slice(&orientation.to_be_bytes());
    app1.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Writes a 24-bit uncompressed BMP (`BITMAPINFOHEADER`, bottom-up rows).
pub fn bmp24(img: &Image) -> Vec<u8> {
    let (w, h) = (img.width as usize, img.height as usize);
    let stride = (w * 3).div_ceil(4) * 4;
    let size = 54 + stride * h;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for row in img.pixels.chunks_exact(w).rev() {
        let start = out.len();
        for &p in row {
            out.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8]);
        }
        out.resize(start + stride, 0);
    }
    out
}
