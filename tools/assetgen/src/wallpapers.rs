//! The desktop wallpapers (1920x1200).

use std::f32::consts::{PI, TAU};

use crate::noise::{Rng, fbm, noise, noise1, ridged};
use crate::paint::{Buf, Rgb, add, clamp01, gradient, hex, mix, mul, render, smoothstep, tonemap};

/// Adds a soft round light (Gaussian profile) to `buf`.
pub fn splat(buf: &mut Buf, cx: f32, cy: f32, sigma: f32, color: Rgb) {
    let r = (sigma * 3.0).ceil() as i32;
    let (ix, iy) = (cx.floor() as i32, cy.floor() as i32);
    for y in iy - r..=iy + r {
        for x in ix - r..=ix + r {
            if x < 0 || y < 0 || x >= buf.w as i32 || y >= buf.h as i32 {
                continue;
            }
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let k = (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
            let p = &mut buf.px[y as usize * buf.w + x as usize];
            *p = add(*p, mul(color, k));
        }
    }
}

/// A field of stars: many faint ones, a few bright ones. `density(u, v)` scales brightness by
/// position (0 hides stars).
pub fn stars(buf: &mut Buf, count: usize, seed: u64, density: impl Fn(f32, f32) -> f32) {
    let mut rng = Rng::new(seed);
    let (w, h) = (buf.w as f32, buf.h as f32);
    for _ in 0..count {
        let (x, y) = (rng.f32() * w, rng.f32() * h);
        let r = rng.f32();
        let bright = (0.08 + 0.9 * r.powi(7)) * density(x / w, y / h);
        if bright <= 0.01 {
            continue;
        }
        let tint = rng.f32();
        let color = if tint < 0.3 {
            [0.75, 0.85, 1.0]
        } else if tint < 0.45 {
            [1.0, 0.9, 0.75]
        } else {
            [1.0, 1.0, 1.0]
        };
        let sigma = 0.45 + 0.9 * r.powi(10) + 0.25 * rng.f32();
        splat(buf, x, y, sigma, mul(color, bright * 1.6));
        if bright > 0.6 {
            // A soft halo around the brightest stars.
            splat(buf, x, y, sigma * 4.0, mul(color, bright * 0.08));
        }
    }
}

/// Darkens the corners a little.
pub fn vignette(buf: &mut Buf, strength: f32) {
    let (w, h) = (buf.w as f32, buf.h as f32);
    for y in 0..buf.h {
        for x in 0..buf.w {
            let dx = (x as f32 + 0.5) / w - 0.5;
            let dy = ((y as f32 + 0.5) / h - 0.5) * h / w;
            let r = (dx * dx + dy * dy).sqrt() * 1.6;
            let k = 1.0 - strength * smoothstep(0.35, 1.05, r);
            let p = &mut buf.px[y * buf.w + x];
            *p = mul(*p, k);
        }
    }
}

/// A large, soft coloured glow (quadratic falloff) blended over `c`.
fn bloom(c: Rgb, u: f32, v: f32, aspect: f32, (cx, cy, rad, color, s): (f32, f32, f32, Rgb, f32)) -> Rgb {
    let dx = (u - cx) * aspect;
    let dy = v - cy;
    let d2 = (dx * dx + dy * dy) / (rad * rad);
    if d2 >= 1.0 {
        return c;
    }
    let k = (1.0 - d2) * (1.0 - d2) * s;
    mix(c, color, k)
}

struct Ribbon {
    /// Centre line: base height plus three sine waves (amplitude, frequency, phase).
    y0: f32,
    waves: [(f32, f32, f32); 3],
    /// Half width in screen heights.
    width: f32,
    /// Twist: the visible width follows |cos(pi (f u + p))|.
    twist: (f32, f32),
    colors: [Rgb; 3],
    strength: f32,
    halo: f32,
}

impl Ribbon {
    fn center(&self, u: f32) -> f32 {
        self.y0 + self.waves.iter().map(|&(a, f, p)| a * (TAU * f * u + p).sin()).sum::<f32>()
    }
}

/// "Aurora": silky ribbons of light flowing across a deep blue and violet night, with soft
/// blooms and a dusting of stars. The default wallpaper.
pub fn aurora(w: usize, h: usize) -> Buf {
    let aspect = w as f32 / h as f32;
    let ribbons = [
        Ribbon {
            y0: 0.60,
            waves: [(0.13, 0.55, 2.3), (0.045, 1.35, 0.4), (0.015, 3.1, 1.7)],
            width: 0.050,
            twist: (1.15, 0.30),
            colors: [hex(0x2FE6FF), hex(0x5C7CFF), hex(0xC264FF)],
            strength: 1.0,
            halo: 0.55,
        },
        Ribbon {
            y0: 0.53,
            waves: [(0.11, 0.62, 2.9), (0.05, 1.1, 2.2), (0.012, 2.7, 0.3)],
            width: 0.022,
            twist: (1.7, 0.75),
            colors: [hex(0x34F5C0), hex(0x40B8FF), hex(0x8A7DFF)],
            strength: 0.75,
            halo: 0.4,
        },
        Ribbon {
            y0: 0.70,
            waves: [(0.09, 0.48, 1.6), (0.04, 1.6, 3.0), (0.01, 3.7, 2.2)],
            width: 0.034,
            twist: (0.9, 0.1),
            colors: [hex(0x6A5CFF), hex(0xC04CF0), hex(0xFF5CA8)],
            strength: 0.55,
            halo: 0.45,
        },
    ];
    // Per-column ribbon geometry.
    struct Col {
        center: f32,
        half: f32,
        sheen: f32,
        color: Rgb,
    }
    let cols: Vec<Vec<Col>> = ribbons
        .iter()
        .map(|r| {
            (0..w)
                .map(|x| {
                    let u = (x as f32 + 0.5) / w as f32;
                    let tw = (PI * (r.twist.0 * u + r.twist.1)).cos().abs();
                    let center = r.center(u) + 0.012 * noise1(u * 3.0, 11);
                    let color = gradient(&[(0.0, r.colors[0]), (0.5, r.colors[1]), (1.0, r.colors[2])], u);
                    Col { center, half: r.width * (0.16 + 0.84 * tw), sheen: 1.0 + 0.9 * (1.0 - tw).powi(4), color }
                })
                .collect()
        })
        .collect();
    let blooms: [(f32, f32, f32, Rgb, f32); 5] = [
        (0.20, 0.22, 0.70, hex(0x2B48D6), 0.50),
        (0.86, 0.70, 0.62, hex(0x7A2BD9), 0.45),
        (0.62, 0.10, 0.45, hex(0x169BD1), 0.25),
        (0.40, 1.02, 0.55, hex(0xC2338A), 0.22),
        (0.05, 0.92, 0.40, hex(0x3A1F9E), 0.30),
    ];
    let mut buf = render(w, h, 1, |x, y| {
        let (u, v) = (x / w as f32, y / h as f32);
        let mut c = mix(hex(0x05081A), hex(0x150F3A), smoothstep(0.0, 1.0, v));
        for b in blooms {
            c = bloom(c, u, v, aspect, b);
        }
        // Faint large-scale cloudiness keeps the field from looking flat.
        let cloud = fbm(u * 2.5 * aspect, v * 2.5, 4, 3) * 0.5 + 0.5;
        c = mul(c, 0.86 + 0.28 * cloud);
        let xi = (x as usize).min(w - 1);
        let mut light = [0.0f32; 3];
        for (i, (r, col)) in ribbons.iter().zip(&cols).enumerate() {
            let c0 = &col[xi];
            let dv = v - c0.center;
            let d = dv / c0.half;
            let ad = d.abs();
            if ad < 1.2 {
                // A glowing core, a soft body and a faint silky sheen across the band.
                let core = (-(d / 0.32).powi(2)).exp() * 0.95;
                let body = (1.0 - (ad / 1.2).powi(2)).max(0.0).powf(1.5) * 0.42;
                let filament = (-((d + 0.45) / 0.07).powi(2)).exp() * 0.35;
                let sheen = 0.93 + 0.07 * (d * 26.0 + u * 5.0).sin();
                let k = (core + body + filament) * sheen * c0.sheen * r.strength;
                // The core runs whiter, like an overexposed light trail.
                let tint = mix(c0.color, [1.0, 1.0, 1.0], core * 0.35);
                light = add(light, mul(tint, k));
            }
            let sigma = r.width * 2.8;
            let halo = (-(dv / sigma).powi(2)).exp() * r.halo * r.strength * (0.6 + 0.4 * c0.sheen);
            light = add(light, mul(c0.color, halo * 0.55));
            // Aurora curtains: faint vertical rays rising above the two upper ribbons.
            if i < 2 && dv < 0.0 {
                let rise = (-(-dv) / (0.16 - 0.06 * i as f32)).exp();
                let rays = (noise1(u * 90.0 + i as f32 * 13.0, 70 + i as u32) * 0.5 + 0.5).powi(2)
                    * (0.6 + 0.4 * noise1(u * 7.0, 72 + i as u32));
                let curtain = rise * rays * 0.32 * r.strength * smoothstep(0.0, -1.5 * c0.half, dv);
                light = add(light, mul(mix(c0.color, hex(0x7CFFC8), 0.25), curtain));
            }
        }
        add(c, light)
    });
    // A few motes of light drifting along the main ribbon, and stars in the upper sky.
    let mut rng = Rng::new(77);
    for _ in 0..40 {
        let u = rng.f32();
        let col = &cols[0][((u * w as f32) as usize).min(w - 1)];
        let off = rng.range(-3.0, 3.0) * col.half;
        let (x, y) = (u * w as f32, (col.center + off) * h as f32);
        let s = rng.range(0.9, 2.2);
        splat(&mut buf, x, y, s, mul(add(col.color, [0.35, 0.35, 0.35]), rng.range(0.12, 0.4)));
    }
    stars(&mut buf, 2600, 5, |u, v| smoothstep(0.82, 0.25, v) * (0.5 + 0.5 * smoothstep(0.0, 0.3, u.min(1.0 - u))));
    vignette(&mut buf, 0.35);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// Renders a height field seen from a low camera looking along +z, column by column with a
/// y-buffer (front to back). `shade(x, z, height, normal)` gives the surface colour and `sky(v)`
/// the colour above the terrain (`v` = 0 at the top, 1 at the horizon). Returns the image and a
/// mask of sky pixels.
pub fn heightfield(
    w: usize,
    h: usize,
    horizon: f32,
    focal: f32,
    cam_h: f32,
    (z_near, z_far): (f32, f32),
    height: &(dyn Fn(f32, f32) -> f32 + Sync),
    shade: &(dyn Fn(f32, f32, f32, [f32; 3]) -> Rgb + Sync),
    sky: &(dyn Fn(usize, usize) -> Rgb + Sync),
) -> (Buf, Vec<bool>) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Vec<(usize, Vec<Rgb>, usize)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let next = &next;
                s.spawn(move || {
                    let mut out = Vec::new();
                    loop {
                        let x = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if x >= w {
                            break;
                        }
                        let mut col = vec![[0.0f32; 3]; h];
                        let dir_x = (x as f32 + 0.5 - w as f32 / 2.0) / focal;
                        let mut ybuf = h as i32;
                        let mut z = z_near;
                        while z < z_far && ybuf > 0 {
                            let wx = dir_x * z;
                            let hh = height(wx, z);
                            let y = (horizon + (cam_h - hh) * focal / z).floor() as i32;
                            if y < ybuf {
                                let e = 0.05 + z * 0.002;
                                let nx = height(wx + e, z) - height(wx - e, z);
                                let nz = height(wx, z + e) - height(wx, z - e);
                                let l = (nx * nx + 4.0 * e * e + nz * nz).sqrt();
                                let n = [-nx / l, 2.0 * e / l, -nz / l];
                                let c = shade(wx, z, hh, n);
                                for yy in y.max(0)..ybuf {
                                    col[yy as usize] = c;
                                }
                                ybuf = y.max(0);
                            }
                            z += 0.004 + z * 0.0012;
                        }
                        for (yy, px) in col.iter_mut().enumerate().take(ybuf.max(0) as usize) {
                            *px = sky(x, yy);
                        }
                        out.push((x, col, ybuf.max(0) as usize));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut buf = Buf::new(w, h);
    let mut sky_mask = vec![false; w * h];
    for (x, col, top) in results.into_iter().flatten() {
        for (y, c) in col.into_iter().enumerate() {
            buf.px[y * w + x] = c;
            sky_mask[y * w + x] = y < top;
        }
    }
    (buf, sky_mask)
}

/// "Dunes": rolling sand dunes at golden hour, with warm light, cool shadows and atmospheric
/// haze, rendered as a height field from a low camera.
pub fn dunes(w: usize, h: usize) -> Buf {
    let ss = 2;
    let (sw, sh) = (w * ss, h * ss);
    let horizon = sh as f32 * 0.44;
    let focal = sw as f32 * 0.75;
    let sun = normalize([-0.82, 0.30, 0.48]);
    let (ct, st) = (0.75f32.cos(), 0.75f32.sin());
    let height = |x: f32, z: f32| -> f32 {
        let mx = x + 7.0 * fbm(x * 0.015, z * 0.015, 3, 21);
        let mz = z + 5.0 * fbm(x * 0.017 + 4.0, z * 0.017, 3, 22);
        // Main dunes: a gentle windward slope and a steeper, smooth slip face.
        let t = (mx * st + mz * ct) / 24.0 + 0.3 * noise(mx * 0.012, mz * 0.012, 23);
        let f = t - t.floor();
        let profile = if f < 0.72 {
            let k = f / 0.72;
            k * k * (3.0 - 2.0 * k)
        } else {
            let k = (f - 0.72) / 0.28;
            1.0 - k * k * (3.0 - 2.0 * k)
        };
        let big = 5.2 * profile * (0.7 + 0.4 * (noise(mx * 0.02, mz * 0.02, 24) * 0.5 + 0.5));
        // Smaller dunes crossing at another angle.
        let t2 = (mx * 0.35 - mz * 0.94) / 9.0 + 0.4 * noise(mx * 0.04, mz * 0.04, 25);
        let f2 = t2 - t2.floor();
        let small = 0.9 * (1.0 - (2.0 * f2 - 1.0).powi(2));
        // Wind ripples, only noticeable close to the camera.
        let ripple = 0.035 * (mx * 2.1 + mz * 0.9 + 1.5 * noise(mx * 0.3, mz * 0.3, 26)).sin();
        big + small + ripple
    };
    let lit = hex(0xF3B26C);
    let shadow = hex(0x87566A);
    let sky_fill = hex(0x6676B4);
    let haze = hex(0xF4D1AE);
    let shade = |_x: f32, z: f32, _hh: f32, n: [f32; 3]| -> Rgb {
        let diff = (n[0] * sun[0] + n[1] * sun[1] + n[2] * sun[2]).max(0.0);
        let k = smoothstep(0.0, 0.62, diff);
        let mut c = mix(shadow, lit, k);
        // Cool skylight in the shadows, a warm glint along sunlit crests.
        c = add(c, mul(sky_fill, 0.22 * (1.0 - k) * n[1].max(0.0)));
        c = add(c, mul([1.0, 0.86, 0.62], 0.18 * smoothstep(0.75, 1.0, diff)));
        let fog = 1.0 - (-(z - 2.0) * 0.0095).exp();
        mix(c, haze, fog.powf(1.1))
    };
    let sky_stops = [(0.0, hex(0x6D82C2)), (0.45, hex(0xBE9DBF)), (0.8, hex(0xF2BE98)), (1.0, hex(0xFBE0BA))];
    let sky = |_x: usize, y: usize| -> Rgb { gradient(&sky_stops, (y as f32 / horizon).clamp(0.0, 1.0)) };
    let (big, mask) = heightfield(sw, sh, horizon, focal, 6.0, (1.5, 320.0), &height, &shade, &sky);
    let mut buf = big.downsample(ss);
    // The low sun: a bright disc above the horizon with a wide warm glow.
    let (sx, sy) = (w as f32 * 0.27, h as f32 * 0.385);
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f32 + 0.5 - sx, y as f32 + 0.5 - sy);
            let d = (dx * dx + dy * dy).sqrt() / h as f32;
            let sky_frac = [(0, 0), (1, 0), (0, 1), (1, 1)]
                .iter()
                .filter(|(ox, oy)| mask[(y * ss + oy) * sw + x * ss + ox])
                .count() as f32
                / 4.0;
            let glow = 0.5 * (-d / 0.08).exp() + 0.22 * (-d / 0.3).exp();
            let disc = smoothstep(0.026, 0.023, d) * sky_frac;
            let i = y * w + x;
            buf.px[i] = add(buf.px[i], mul([1.0, 0.8, 0.55], glow * (0.35 + 0.65 * sky_frac) + disc));
        }
    }
    vignette(&mut buf, 0.22);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// "Ridges": layered mountain ridges at dusk, fading into haze towards a glowing horizon.
pub fn ridges(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let layers = 7;
    let sky = [
        (0.0, hex(0x141A45)),
        (0.30, hex(0x3A2E6E)),
        (0.48, hex(0x8A4A86)),
        (0.60, hex(0xE07F7A)),
        (0.68, hex(0xFFC08A)),
    ];
    let (sun_x, sun_y, sun_r) = (0.64 * fw, 0.452 * fh, 0.040 * fh);
    // Ridge lines per layer and column (screen y of the crest).
    let ridge: Vec<Vec<f32>> = (0..layers)
        .map(|i| {
            let t = i as f32 / (layers - 1) as f32;
            let base = 0.50 + 0.43 * t.powf(1.1);
            let amp = 0.07 + 0.12 * t;
            let freq = 1.3 + 2.2 * (1.0 - t);
            (0..w)
                .map(|x| {
                    let u = x as f32 / fw;
                    let r = ridged(u * freq + i as f32 * 7.3, i as f32 * 3.1, 6, 40 + i as u32);
                    let n = fbm(u * freq * 0.6, i as f32, 3, 60 + i as u32);
                    (base - amp * (r * 0.9 + 0.35 * n)) * fh
                })
                .collect()
        })
        .collect();
    // Smoothed ridge lines: valley mist is measured from these, so it lies in soft bands.
    let smooth: Vec<Vec<f32>> = ridge
        .iter()
        .map(|line| {
            let r = (w / 7).max(1) as isize;
            (0..w as isize)
                .map(|x| {
                    let (a, b) = ((x - r).max(0), (x + r).min(w as isize - 1));
                    line[a as usize..=b as usize].iter().sum::<f32>() / (b - a + 1) as f32
                })
                .collect()
        })
        .collect();
    let far = hex(0xE8A6A6);
    let near = hex(0x1A1638);
    let fog = hex(0xF4B8A0);
    let buf = render(w, h, 2, |x, y| {
        let v = y / fh;
        let mut c = gradient(&sky, v / 0.68);
        // Sun and its glow.
        let (dx, dy) = (x - sun_x, y - sun_y);
        let d = (dx * dx + dy * dy).sqrt();
        c = add(c, mul([1.0, 0.75, 0.5], 0.5 * (-d / (fh * 0.08)).exp() + 0.22 * (-d / (fh * 0.28)).exp()));
        c = add(c, mul([1.0, 0.9, 0.72], 1.4 * smoothstep(sun_r + 1.0, sun_r - 1.0, d)));
        let xi = (x as usize).min(w - 1);
        for (i, line) in ridge.iter().enumerate() {
            let t = i as f32 / (layers - 1) as f32;
            let crest = line[xi];
            let soft = smooth[i][xi];
            let cover = clamp01(y - crest + 0.5);
            if cover <= 0.0 {
                continue;
            }
            let base_col = mix(far, near, t.powf(0.8));
            // Mist pooling in the valleys below each ridge.
            let depth = (y - soft) / fh;
            let mist = smoothstep(-0.03, 0.14 - 0.06 * t, depth) * (0.5 - 0.32 * t);
            let mut lc = mix(base_col, fog, mist);
            // A thin rim of light along the crests, strongest near the sun.
            let toward_sun = (-((x - sun_x) / (fw * 0.35)).powi(2)).exp();
            let rim = (-(y - crest) / 1.6).exp() * (1.0 - t).powi(2) * 0.2 * toward_sun;
            lc = add(lc, mul([1.0, 0.7, 0.5], rim));
            c = mix(c, lc, cover);
        }
        c
    });
    let mut buf = buf;
    stars(&mut buf, 900, 9, |_, v| smoothstep(0.32, 0.05, v) * 0.7);
    vignette(&mut buf, 0.2);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// "Nebula": wisps of glowing gas and dark dust in deep space, with a field of stars.
pub fn nebula(w: usize, h: usize) -> Buf {
    let aspect = w as f32 / h as f32;
    let mut emission = render(w, h, 1, |x, y| {
        let (u, v) = (x / w as f32, y / h as f32);
        let (px, py) = (u * aspect * 1.4, v * 1.4);
        let wx = fbm(px + 1.3, py + 7.1, 4, 101);
        let wy = fbm(px + 8.2, py + 2.4, 4, 102);
        let (qx, qy) = (px + 0.9 * wx, py + 0.9 * wy);
        let gas = fbm(qx, qy, 7, 103) * 0.5 + 0.5;
        // The cloud lies along a sweeping diagonal band; elsewhere space stays dark.
        let center = 0.30 + 0.45 * u + 0.08 * (u * 4.0).sin();
        let band = (-((v - center) / 0.26).powi(2)).exp();
        let core = (-(((u - 0.58) * aspect).powi(2) + (v - 0.58).powi(2)) / 0.05).exp();
        let density = (smoothstep(0.38, 0.85, gas) * band + 0.35 * core * gas).min(1.0);
        let filaments = ridged(qx * 2.2, qy * 2.2, 5, 104).powi(5) * band * 0.9;
        // Hue varies across the cloud: teal-blue on the outside, magenta and warm pink inside.
        let hue = clamp01(fbm(px * 0.7 + 3.0, py * 0.7, 3, 105) * 0.9 + 0.5);
        let outer = mix(hex(0x2052D6), hex(0x16C2D9), hue);
        let inner = mix(hex(0xC72A9A), hex(0xFF5C86), hue);
        let mut c = mix(outer, inner, smoothstep(0.25, 0.75, density));
        c = mul(c, density.powf(1.5) * 1.45);
        c = add(c, mul(mix(hex(0xFFB8D0), hex(0xA8E8FF), hue), filaments));
        c = add(c, mul(hex(0xFFE4D0), core * density * 0.6));
        // Dark dust lanes cutting through the brighter regions.
        let dust = smoothstep(0.52, 0.8, fbm(qx * 1.7 + 4.0, qy * 1.7, 6, 106) * 0.5 + 0.5);
        mul(c, 1.0 - 0.85 * dust * band)
    });
    // Bloom: the glowing gas lights up its surroundings.
    let mut glow = emission.clone();
    glow.blur(24);
    let mut buf = render(w, h, 1, |_x, y| {
        let v = y / h as f32;
        mix(hex(0x04050B), hex(0x07081A), v)
    });
    for ((p, e), g) in buf.px.iter_mut().zip(&emission.px).zip(&glow.px) {
        *p = add(add(*p, *e), mul(*g, 0.55));
    }
    emission.px.clear();
    stars(&mut buf, 6000, 21, |_, _| 1.0);
    // A few bright foreground stars with diffraction spikes.
    let mut rng = Rng::new(31);
    for _ in 0..7 {
        let (x, y) = (rng.f32() * w as f32, rng.f32() * h as f32);
        let color = mix([1.0, 0.95, 0.9], [0.75, 0.88, 1.0], rng.f32());
        let s = rng.range(0.6, 1.0);
        splat(&mut buf, x, y, 1.5, mul(color, 2.2 * s));
        splat(&mut buf, x, y, 6.0, mul(color, 0.14 * s));
        for k in 1..55 {
            let f = (-(k as f32) / 16.0).exp() * 0.3 * s;
            for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                splat(&mut buf, x + dx * k as f32, y + dy * k as f32, 0.65, mul(color, f));
            }
        }
    }
    vignette(&mut buf, 0.4);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// A sine wave: amplitude, frequency and phase.
type Wave = (f32, f32, f32);

/// "Pastel": layered paper-cut waves in soft pastels, with gentle shadows and highlights.
pub fn pastel(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    // Each layer: base height, two waves, top and bottom colours.
    let layers: [(f32, [Wave; 2], Rgb, Rgb); 5] = [
        (0.30, [(0.06, 0.8, 0.4), (0.02, 2.1, 1.2)], hex(0xFFC9B8), hex(0xFFB0A8)),
        (0.44, [(0.07, 0.65, 2.0), (0.025, 1.7, 0.3)], hex(0xFFAFC5), hex(0xF58FB4)),
        (0.58, [(0.06, 0.9, 3.6), (0.02, 2.4, 2.6)], hex(0xD5B3FF), hex(0xB495F5)),
        (0.71, [(0.05, 0.7, 5.1), (0.02, 1.9, 4.0)], hex(0x9FB8FF), hex(0x7E9BF2)),
        (0.84, [(0.045, 1.0, 0.9), (0.015, 2.6, 5.5)], hex(0x8FE0E0), hex(0x6CC6D6)),
    ];
    let edge = |i: usize, u: f32| -> f32 {
        let (base, waves, _, _) = layers[i];
        base + waves.iter().map(|&(a, f, p)| a * (TAU * f * u + p).sin()).sum::<f32>()
    };
    render(w, h, 2, |x, y| {
        let (u, v) = (x / fw, y / fh);
        let mut c = mix(hex(0xFFE6D6), hex(0xFFD2C4), v / 0.4);
        c = mix(c, hex(0xFFF3EA), (-((u - 0.75).powi(2) + (v - 0.12).powi(2)) / 0.05).exp() * 0.6);
        for i in 0..layers.len() {
            let top = edge(i, u);
            let d = (v - top) * fh; // pixels below the edge
            // The layer in front casts a soft shadow onto the one behind, just above its edge.
            if d < 0.0 {
                let shadow = (-(-d) / 26.0).exp() * 0.16;
                c = mul(c, 1.0 - shadow);
                continue;
            }
            let (_, _, top_c, bottom_c) = layers[i];
            let next = if i + 1 < layers.len() { edge(i + 1, u) } else { 1.1 };
            let t = clamp01((v - top) / (next - top).max(0.02));
            let mut lc = mix(top_c, bottom_c, t);
            // A crisp light edge along the top of the paper.
            lc = mix(lc, [1.0, 0.98, 0.97], (-d / 2.2).exp() * 0.55);
            c = mix(c, lc, clamp01(d + 0.5));
        }
        c
    })
}
