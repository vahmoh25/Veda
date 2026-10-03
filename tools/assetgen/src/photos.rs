//! Sample "photos" for `~/Pictures`: procedural scenes in every supported format.

use std::f32::consts::TAU;

use crate::noise::{Rng, fbm, hash, hash01, noise, noise1, ridged};
use crate::paint::{Buf, Rgb, add, clamp01, gradient, hex, mix, mul, mulc, render, smoothstep, tonemap};
use crate::wallpapers::{heightfield, splat, stars, vignette};

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    [v[0] / l, v[1] / l, v[2] / l]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// "Alpine Lake": snowy peaks above a forested shore, mirrored in a calm mountain lake. The
/// terrain is a height field; the reflection is a second render from a camera mirrored below
/// the water plane.
pub fn alpine_lake(w: usize, h: usize) -> Buf {
    let ss = 2;
    let (sw, sh) = (w * ss, h * ss);
    let horizon = sh as f32 * 0.5;
    let focal = sw as f32 * 0.85;
    let cam_h = 2.0;
    let shore = 80.0;
    let sun = normalize([0.72, 0.5, -0.25]);
    let ground = |x: f32, z: f32| -> f32 {
        // Forested hills along the far shore.
        let ramp = smoothstep(shore, shore + 10.0, z);
        let hills = ramp * (1.0 + 4.5 * (fbm(x * 0.02, z * 0.02, 4, 300) * 0.5 + 0.5)) * (0.7 + z.min(220.0) / 300.0);
        // The range: broad massifs carved by domain-warped ridged noise, highest a little
        // right of the centre.
        let wx = x + 40.0 * fbm(x * 0.003, z * 0.003, 3, 310);
        let wz = z + 40.0 * fbm(x * 0.003 + 5.2, z * 0.003 - 1.3, 3, 311);
        let broad = fbm(wx * 0.0028, wz * 0.0028, 3, 313) * 0.5 + 0.5;
        let r = ridged(wx * 0.006, wz * 0.006, 8, 312);
        let a = x / z;
        let env = 0.45 + 0.55 * (-((a - 0.08) / 0.3).powi(2)).exp();
        hills + smoothstep(150.0, 320.0, z) * env * (70.0 * broad + 75.0 * r * broad)
    };
    // Conifers on a jittered grid, with clearings, thinning out towards the tree line.
    let trees = |x: f32, z: f32, g: f32| -> f32 {
        let treeline = 22.0 + 6.0 * noise(x * 0.02, z * 0.02, 320);
        let clearings = smoothstep(-0.5, -0.1, noise(x * 0.06, z * 0.06, 322));
        let density = smoothstep(treeline, treeline - 6.0, g) * smoothstep(shore + 0.6, shore + 2.0, z) * clearings;
        if density <= 0.0 {
            return 0.0;
        }
        let cell = 0.9;
        let (gx, gz) = ((x / cell).floor() as i32, (z / cell).floor() as i32);
        let mut best = 0.0f32;
        for dz in -1..=1 {
            for dx in -1..=1 {
                let hs = hash(gx + dx, gz + dz, 321);
                if (hs >> 24) as f32 / 255.0 > density {
                    continue;
                }
                let px = (gx + dx) as f32 * cell + cell * (hs & 0xFF) as f32 / 255.0;
                let pz = (gz + dz) as f32 * cell + cell * ((hs >> 8) & 0xFF) as f32 / 255.0;
                let th = 1.3 + 1.5 * ((hs >> 16) & 0xFF) as f32 / 255.0;
                let d = ((x - px).powi(2) + (z - pz).powi(2)).sqrt() / (th * 0.24);
                if d < 1.0 {
                    best = best.max(th * (1.0 - d));
                }
            }
        }
        best
    };
    let height = |x: f32, z: f32| -> f32 {
        let g = ground(x, z);
        g + trees(x, z, g)
    };
    let haze = hex(0xB4CBE3);
    let shade = |x: f32, z: f32, hh: f32, n: [f32; 3]| -> Rgb {
        let g = ground(x, z);
        let tree = (hh - g).max(0.0);
        let diff = dot(n, sun).max(0.0);
        let snowline = 50.0 + 10.0 * fbm(x * 0.02, z * 0.02, 3, 330);
        let flat = n[1] + 0.25 * noise(x * 0.15, z * 0.15, 331);
        let snow = smoothstep(snowline - 5.0, snowline + 5.0, g) * smoothstep(0.3, 0.6, flat);
        let strata = noise(x * 0.01, g * 0.12, 333) * 0.5 + 0.5;
        let rock = mix(
            mix(hex(0x4A4440), hex(0x8C8176), noise(x * 0.05, z * 0.05 + g * 0.08, 332) * 0.5 + 0.5),
            hex(0x6E5E50),
            strata * 0.4,
        );
        let meadow = smoothstep(36.0, 22.0, g) * smoothstep(0.45, 0.75, n[1]);
        let mut albedo = mix(rock, hex(0x4B5C31), meadow * 0.85);
        if tree > 0.02 {
            let kind = noise(x * 0.3, z * 0.3, 334) * 0.5 + 0.5;
            albedo = mix(
                mix(hex(0x0E2418), hex(0x2D4A2A), clamp01(tree / 2.6)),
                hex(0x4F5A2A),
                smoothstep(0.7, 0.9, kind) * 0.6,
            );
        }
        albedo = mix(albedo, hex(0xF4F7FA), snow);
        let sunlight = mul(mulc(albedo, [1.0, 0.94, 0.84]), 1.35 * diff);
        let skylight = mulc(albedo, mul([0.5, 0.64, 0.88], 0.24 + 0.16 * n[1]));
        let fog = 1.0 - (-(z - shore) * 0.0014).exp();
        mix(add(sunlight, skylight), haze, 0.92 * fog)
    };
    let sky_stops = [(0.0, hex(0x2B5DA6)), (0.5, hex(0x5F90CE)), (0.85, hex(0xA9C8E8)), (1.0, hex(0xCFE0F0))];
    let cloud = |u: f32, v: f32| -> f32 {
        let q = fbm(u * 1.6, v * 2.4, 3, 350);
        let d = fbm(u * 2.8 + 0.7 * q, v * 6.0 + 0.5 * q, 6, 351) * 0.5 + 0.5;
        let band = smoothstep(0.08, 0.3, v) * smoothstep(0.85, 0.55, v);
        clamp01((d - 0.55) * 4.5) * band
    };
    let sky = |x: usize, y: usize| -> Rgb {
        let u = x as f32 / sw as f32;
        let v = y as f32 / horizon;
        let mut c = gradient(&sky_stops, v.clamp(0.0, 1.0));
        c = add(c, mul([0.16, 0.12, 0.05], (-((u - 1.05).powi(2) + (v * 0.6).powi(2)) / 0.25).exp()));
        let d = cloud(u, v);
        if d > 0.0 {
            let lit = clamp01(0.7 + 2.0 * (d - cloud(u, v - 0.035)));
            c = mix(c, mix(hex(0x9EABC2), hex(0xFFFFFF), lit), smoothstep(0.0, 0.6, d));
        }
        c
    };
    let (direct, _) = heightfield(sw, sh, horizon, focal, cam_h, (shore, 3000.0), &height, &shade, &sky);
    let (mirror, _) = heightfield(sw, sh, horizon, focal, -cam_h, (shore, 3000.0), &height, &shade, &sky);
    let shore_y = horizon + cam_h * focal / shore;
    let water = hex(0x163C44);
    let mut buf = render(w, h, ss, |x, y| {
        let (fx, fy) = (x * ss as f32, y * ss as f32);
        if fy < shore_y {
            return direct.get(fx as usize, fy as usize);
        }
        // The point on the water plane seen through this pixel.
        let dist = cam_h * focal / (fy - horizon);
        let wx = (fx - sw as f32 / 2.0) / focal * dist;
        let near = clamp01((shore - dist) / shore);
        let sin_a = (fy - horizon) / ((fy - horizon).powi(2) + focal * focal).sqrt();
        let fresnel = 0.02 + 0.98 * (1.0 - sin_a).powi(5);
        // Gentle ripples displace the mirrored image, more strongly close to the camera.
        let ripple = noise(wx * 0.6, dist * 2.2, 340) + 0.5 * noise(wx * 1.7, dist * 5.0, 341);
        let amp = 1.0 + 10.0 * near * near;
        let src_y = 2.0 * horizon - fy + amp * ripple;
        let src_x = fx + 0.4 * amp * noise(wx * 0.9, dist * 3.0, 342);
        let r = mirror.sample(src_x, src_y.max(0.5));
        // Wind lanes: patches where small waves blur the reflection into a lighter sheen.
        let lane = smoothstep(0.58, 0.8, fbm(wx * 0.02, dist * 0.08, 4, 343) * 0.5 + 0.5);
        let sheen = mix(r, hex(0xA9C3DE), lane * 0.45);
        mix(water, mulc(sheen, [0.9, 0.95, 0.98]), 0.3 + 0.7 * fresnel)
    });
    vignette(&mut buf, 0.18);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// "City Lights": out-of-focus city lights at night (bokeh).
pub fn city_lights(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let mut buf = render(w, h, 1, |_x, y| {
        let v = y / fh;
        let mut c = mix(hex(0x05070F), hex(0x0E1426), v);
        c = add(c, mul(hex(0x3A1D10), smoothstep(0.55, 1.0, v) * 0.8));
        c
    });
    let palette = [
        (hex(0xFFB75E), 6.0),
        (hex(0xFF8A3D), 3.0),
        (hex(0xFF4A4A), 1.2),
        (hex(0xFFE2B0), 3.0),
        (hex(0x7FD3E8), 0.8),
        (hex(0x8C9BFF), 0.5),
    ];
    let total: f32 = palette.iter().map(|p| p.1).sum();
    let mut rng = Rng::new(808);
    let mut discs = Vec::new();
    for i in 0..230 {
        let big = i < 45;
        let x = rng.f32() * fw;
        let y = fh * (0.12 + 0.86 * rng.f32().powf(0.85)) + rng.range(-40.0, 40.0);
        let r = if big { rng.range(42.0, 100.0) } else { rng.range(8.0, 36.0) };
        let mut pick = rng.f32() * total;
        let mut color = palette[0].0;
        for (c, wgt) in palette {
            if pick < wgt {
                color = c;
                break;
            }
            pick -= wgt;
        }
        // Larger (more defocused) discs spread the same light thinner.
        let intensity = rng.range(0.45, 1.0) * 0.3 * (24.0 / r).powf(1.15);
        discs.push((x, y, r, color, intensity));
    }
    for (cx, cy, r, color, k) in discs {
        let x0 = ((cx - r - 2.0).floor() as i32).max(0);
        let x1 = ((cx + r + 2.0).ceil() as i32).min(w as i32 - 1);
        let y0 = ((cy - r - 2.0).floor() as i32).max(0);
        let y1 = ((cy + r + 2.0).ceil() as i32).min(h as i32 - 1);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let d = (dx * dx + dy * dy).sqrt() / r;
                if d > 1.05 {
                    continue;
                }
                let edge = smoothstep(1.0 + 1.5 / r, 1.0 - 1.5 / r, d);
                let rim = 1.0 + 0.45 * smoothstep(0.7, 0.97, d);
                let inner = 0.85 + 0.15 * (1.0 - d * d);
                let p = &mut buf.px[y as usize * w + x as usize];
                *p = add(*p, mul(color, k * edge * rim * inner));
            }
        }
    }
    // A soft glow around everything bright, and a few sharp points of light.
    let mut glow = buf.clone();
    glow.blur(18);
    for (p, g) in buf.px.iter_mut().zip(&glow.px) {
        *p = add(*p, mul(*g, 0.35));
    }
    for _ in 0..60 {
        let (x, y) = (rng.f32() * fw, fh * rng.range(0.3, 0.95));
        let c = mix(hex(0xFFD08A), hex(0xFFFFFF), rng.f32());
        splat(&mut buf, x, y, 1.2, mul(c, rng.range(0.4, 1.2)));
    }
    for p in &mut buf.px {
        *p = tonemap(mul(*p, 1.1));
    }
    buf
}

/// "Lighthouse": a lighthouse on a rocky point at dusk (portrait).
pub fn lighthouse(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let horizon = fh * 0.66;
    let tx = fw * 0.62; // tower centre
    let tower_base = fh * 0.70;
    let tower_top = fh * 0.30;
    let sky = [
        (0.0, hex(0x0E1A3A)),
        (0.35, hex(0x2A3F78)),
        (0.62, hex(0x8C6C9A)),
        (0.82, hex(0xF0A07A)),
        (1.0, hex(0xFFD2A0)),
    ];
    let mut buf = render(w, h, 2, |x, y| {
        let v = y / horizon;
        let mut c;
        if y < horizon {
            c = gradient(&sky, v);
            // Wisps of cloud catching the last light.
            let n = fbm(x / fw * 2.5, y / fh * 10.0, 5, 501) * 0.5 + 0.5;
            let cloud = smoothstep(0.55, 0.8, n) * smoothstep(0.15, 0.45, v) * smoothstep(0.95, 0.6, v);
            c = mix(c, mix(hex(0x6E5A86), hex(0xFFB48A), v), cloud * 0.7);
        } else {
            // The sea, reflecting the warm horizon.
            let d = (y - horizon) / (fh - horizon);
            c = mix(hex(0xB5806E), hex(0x10213A), d.powf(0.35));
            let waves = (noise(x / fw * 40.0, d * 60.0 / (d + 0.05), 502) * 0.5 + 0.5).powi(3);
            c = add(c, mul(hex(0xFFC49A), waves * 0.25 * (1.0 - d)));
        }
        // The light beam sweeping to the left.
        let lamp = (tx, tower_top + fh * 0.035);
        let (dx, dy) = (x - lamp.0, y - lamp.1);
        if dx < 0.0 {
            let ang = (dy / -dx).atan();
            let spread = (-((ang - 0.04) / 0.07).powi(2)).exp();
            let fall = (-(-dx) / (fw * 0.9)).exp();
            c = add(c, mul(hex(0xFFE7B0), spread * fall * 0.35));
        }
        // Rocks.
        let rock_top = tower_base - fh * 0.02 + fh * 0.04 * ((x - tx) / fw * 3.0).powi(2)
            - fh * 0.015 * (ridged(x / fw * 6.0, 3.0, 5, 503) - 0.5);
        if y > rock_top && y < fh * 0.80 + fh * 0.02 * noise1(x / fw * 8.0, 504) {
            let shade = 0.5 + 0.5 * noise(x / fw * 30.0, y / fh * 30.0, 505);
            c = mix(hex(0x1A1720), hex(0x3B3038), shade * 0.6);
        }
        // The tower: a tapered cylinder with red bands.
        let t = ((y - tower_top) / (tower_base - tower_top)).clamp(0.0, 1.0);
        let half = fw * (0.05 + 0.025 * t);
        let in_tower = y >= tower_top && y <= tower_base && (x - tx).abs() <= half;
        if in_tower {
            let s = (x - tx) / half; // -1..1 across the cylinder
            let light = 0.55 + 0.45 * (1.0 - (s + 0.45).powi(2)).max(0.0).sqrt();
            let band = ((t * 4.0).fract() < 0.45) && t > 0.08;
            let base_col = if band { hex(0xC23B3B) } else { hex(0xF2EEE8) };
            c = mul(base_col, light * 0.95);
        }
        // Gallery, lantern room and roof.
        let gal_y = tower_top - fh * 0.008;
        if y >= gal_y && y <= tower_top + fh * 0.004 && (x - tx).abs() <= fw * 0.075 {
            c = hex(0x2A2830);
        }
        let lantern = (tower_top - fh * 0.06, gal_y);
        if y >= lantern.0 && y < lantern.1 && (x - tx).abs() <= fw * 0.045 {
            let glow = 1.0 - ((x - tx) / (fw * 0.045)).abs() * 0.4;
            c = mul(hex(0xFFE2A0), glow * 1.3);
            if ((x - tx) / (fw * 0.015)).fract().abs() < 0.12 {
                c = hex(0x2A2830);
            }
        }
        let roof_top = lantern.0 - fh * 0.035;
        if y >= roof_top && y < lantern.0 {
            let k = (y - roof_top) / (lantern.0 - roof_top);
            if (x - tx).abs() <= fw * 0.055 * k + 1.0 {
                c = hex(0x34303A);
            }
        }
        c
    });
    // Lamp glow and the first stars.
    splat(&mut buf, tx, tower_top - fh * 0.035, fw * 0.05, mul(hex(0xFFD58A), 0.5));
    stars(&mut buf, 700, 507, |_, v| smoothstep(0.3, 0.05, v) * 0.8);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// "Soap Film": swirling interference colours on a soap film (macro), from a spectral model of
/// thin-film interference integrated against the CIE colour matching functions.
pub fn soap_film(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    // CIE 1931 colour matching functions (the multi-lobe Gaussian fit by Wyman, Sloan and
    // Shirley), sampled every 10 nm.
    let g = |x: f32, mu: f32, s1: f32, s2: f32| {
        let t = (x - mu) / if x < mu { s1 } else { s2 };
        (-0.5 * t * t).exp()
    };
    let cmf: Vec<(f32, [f32; 3])> = (0..31)
        .map(|i| {
            let l = 400.0 + 10.0 * i as f32;
            let x = 1.056 * g(l, 599.8, 37.9, 31.0) + 0.362 * g(l, 442.0, 16.0, 26.7) - 0.065 * g(l, 501.1, 20.4, 26.2);
            let y = 0.821 * g(l, 568.8, 46.9, 40.5) + 0.286 * g(l, 530.9, 16.3, 31.1);
            let z = 1.217 * g(l, 437.0, 11.8, 36.0) + 0.681 * g(l, 459.0, 26.0, 13.8);
            (l, [x, y, z])
        })
        .collect();
    let to_rgb = |c: [f32; 3]| -> Rgb {
        [
            3.2406 * c[0] - 1.5372 * c[1] - 0.4986 * c[2],
            -0.9689 * c[0] + 1.8758 * c[1] + 0.0415 * c[2],
            0.0557 * c[0] - 0.2040 * c[1] + 1.0570 * c[2],
        ]
    };
    // White balance: a perfect reflector maps to (1, 1, 1).
    let white = to_rgb(cmf.iter().fold([0.0; 3], |a, (_, c)| add(a, *c)));
    // Reflectance of a film `d` nanometres thick (refractive index 1.33), normalised so that
    // constructive interference at every wavelength would be white; returns display values.
    let film = |d: f32| -> Rgb {
        let mut xyz = [0.0f32; 3];
        for (l, c) in &cmf {
            let s = (TAU * 1.33 * d / l).sin();
            xyz = add(xyz, mul(*c, s * s));
        }
        let rgb = to_rgb(xyz);
        [0, 1, 2].map(|i| (rgb[i] / white[i]).max(0.0).powf(1.0 / 2.2))
    };
    let mut buf = render(w, h, 2, |x, y| {
        let (u, v) = (x / fw, y / fh);
        // A slow vortex plus domain-warped turbulence; the film drains, so it is thinner at the top.
        let (cx, cy) = (u - 0.56, v - 0.42);
        let r = (cx * cx + cy * cy).sqrt();
        let a = cy.atan2(cx) + 3.2 * (-r * 4.0).exp();
        let (px, py) = (r * a.cos() * 2.4, r * a.sin() * 2.4);
        let q1 = fbm(px, py, 5, 601);
        let q2 = fbm(px + 5.2, py + 1.3, 5, 602);
        let t = fbm(px + 1.6 * q1, py + 1.6 * q2, 6, 603);
        let d = (60.0 + 560.0 * v.powf(0.8) + 320.0 * t).max(0.0);
        // A large soft box reflected in the film, falling off towards the corners.
        let light = 0.12 + 0.9 * smoothstep(0.95, 0.12, ((u - 0.42).powi(2) * 0.8 + (v - 0.4).powi(2)).sqrt());
        mul(film(d), light)
    });
    vignette(&mut buf, 0.25);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// "Still Life": a ray-traced studio still life of spheres and a cube on a seamless backdrop.
pub fn still_life(w: usize, h: usize) -> Buf {
    #[derive(Clone, Copy)]
    struct Sphere {
        c: [f32; 3],
        r: f32,
        albedo: Rgb,
        spec: f32,
        gloss: f32,
    }
    let spheres = [
        Sphere { c: [-1.15, 1.0, 5.6], r: 1.0, albedo: hex(0xD9694A), spec: 0.08, gloss: 20.0 },
        Sphere { c: [0.95, 0.75, 5.0], r: 0.75, albedo: hex(0x2E5FD8), spec: 0.6, gloss: 120.0 },
        Sphere { c: [0.1, 0.35, 4.1], r: 0.35, albedo: hex(0xE8B648), spec: 0.9, gloss: 200.0 },
    ];
    // A cube (rotated about y) on the left.
    let cube_c = [-3.1, 0.45, 6.6];
    let cube_h = 0.45;
    let (ca, sa) = (0.6f32.cos(), 0.6f32.sin());
    let light_pos = [-3.0, 6.0, 1.5];
    let light_r = 1.2;
    let cam = [0.0, 1.35, -0.8];
    let hit_sphere = |o: [f32; 3], d: [f32; 3], s: &Sphere| -> Option<f32> {
        let oc = sub3(o, s.c);
        let b = dot(oc, d);
        let cc = dot(oc, oc) - s.r * s.r;
        let disc = b * b - cc;
        if disc < 0.0 {
            return None;
        }
        let t = -b - disc.sqrt();
        (t > 1e-3).then_some(t)
    };
    let to_cube = |p: [f32; 3]| -> [f32; 3] {
        let q = sub3(p, cube_c);
        [ca * q[0] - sa * q[2], q[1], sa * q[0] + ca * q[2]]
    };
    let hit_cube = |o: [f32; 3], d: [f32; 3]| -> Option<(f32, [f32; 3])> {
        let lo = to_cube(o);
        let ld = [ca * d[0] - sa * d[2], d[1], sa * d[0] + ca * d[2]];
        let (mut t0, mut t1) = (f32::MIN, f32::MAX);
        let mut axis = 0;
        for i in 0..3 {
            if ld[i].abs() < 1e-9 {
                if lo[i].abs() > cube_h {
                    return None;
                }
                continue;
            }
            let (a, b) = ((-cube_h - lo[i]) / ld[i], (cube_h - lo[i]) / ld[i]);
            let (a, b) = (a.min(b), a.max(b));
            if a > t0 {
                t0 = a;
                axis = i;
            }
            t1 = t1.min(b);
        }
        if t0 > t1 || t0 < 1e-3 {
            return None;
        }
        let mut n_local = [0.0; 3];
        n_local[axis] = if ld[axis] > 0.0 { -1.0 } else { 1.0 };
        // Back to world space.
        let n = [ca * n_local[0] + sa * n_local[2], n_local[1], -sa * n_local[0] + ca * n_local[2]];
        Some((t0, n))
    };
    let occluded = |p: [f32; 3], l: [f32; 3], dist: f32| -> bool {
        spheres.iter().any(|s| hit_sphere(p, l, s).is_some_and(|t| t < dist))
            || hit_cube(p, l).is_some_and(|(t, _)| t < dist)
    };
    let (fw, fh) = (w as f32, h as f32);
    let buf = render(w, h, 2, |x, y| {
        let px = (x - fw / 2.0) / fh * 1.1;
        let py = -(y - fh / 2.0) / fh * 1.1 - 0.12;
        let d = normalize([px, py, 1.0]);
        // Find the nearest surface.
        let mut best: Option<(f32, [f32; 3], Rgb, f32, f32)> = None;
        for s in &spheres {
            if let Some(t) = hit_sphere(cam, d, s)
                && best.is_none_or(|b| t < b.0)
            {
                let p = add(cam, mul(d, t));
                best = Some((t, normalize(sub3(p, s.c)), s.albedo, s.spec, s.gloss));
            }
        }
        if let Some((t, n)) = hit_cube(cam, d)
            && best.is_none_or(|b| t < b.0)
        {
            best = Some((t, n, hex(0xEDEBE6), 0.15, 40.0));
        }
        // The backdrop: a seamless studio sweep (a floor curving up into a wall).
        let (z0, rad) = (7.5, 2.5);
        let floor_t = if d[1] < 0.0 { -cam[1] / d[1] } else { f32::MAX };
        let (bt, bn) = if floor_t < f32::MAX && cam[2] + floor_t * d[2] <= z0 {
            (floor_t, [0.0, 1.0, 0.0])
        } else {
            // The far side of a cylinder along x through (y = rad, z = z0).
            let (oy, oz) = (cam[1] - rad, cam[2] - z0);
            let a = d[1] * d[1] + d[2] * d[2];
            let b = oy * d[1] + oz * d[2];
            let c = oy * oy + oz * oz - rad * rad;
            let t = (-b + (b * b - a * c).max(0.0).sqrt()) / a;
            let q = add(cam, mul(d, t));
            if q[1] <= rad && q[2] >= z0 {
                (t, [0.0, (rad - q[1]) / rad, (z0 - q[2]) / rad])
            } else {
                ((z0 + rad - cam[2]) / d[2], [0.0, 0.0, -1.0])
            }
        };
        if best.is_none_or(|b| bt < b.0) {
            best = Some((bt, bn, hex(0xD8D2CA), 0.0, 1.0));
        }
        let (t, n, albedo, spec, gloss) = best.unwrap();
        let p = add(cam, mul(d, t));
        // Soft shadow: sample the disc-shaped area light along a spiral, rotated per sample
        // position so that penumbras turn into fine grain instead of bands.
        let mut lit = 0.0;
        let samples = 24;
        let rot = hash01((x * 4.0) as i32, (y * 4.0) as i32, 77) * TAU;
        for i in 0..samples {
            let a = i as f32 * 2.399_963 + rot;
            let rr = light_r * ((i as f32 + 0.5) / samples as f32).sqrt();
            let lp = [light_pos[0] + rr * a.cos(), light_pos[1], light_pos[2] + rr * a.sin()];
            let to = sub3(lp, p);
            let dist = dot(to, to).sqrt();
            let l = mul(to, 1.0 / dist);
            let ndl = dot(n, l).max(0.0);
            if ndl > 0.0 && !occluded(add(p, mul(n, 1e-3)), l, dist) {
                lit += ndl;
            }
        }
        lit /= samples as f32;
        let l = normalize(sub3(light_pos, p));
        let hv = normalize(sub3(l, d));
        let specular = spec * dot(n, hv).max(0.0).powf(gloss) * (lit * 1.5).min(1.0);
        // Ambient: brighter from above, darker in contact areas.
        let mut ao = 1.0;
        for s in &spheres {
            let to = sub3(s.c, p);
            let dist = dot(to, to).sqrt();
            if dist > 1e-3 {
                let k = (s.r / dist).powi(2) * dot(n, mul(to, 1.0 / dist)).max(0.0);
                ao *= 1.0 - 0.8 * k.min(1.0);
            }
        }
        let ambient = 0.30 + 0.12 * n[1];
        let warm = [1.0, 0.95, 0.88];
        let c = add(mulc(albedo, add(mul(warm, lit * 0.95), mul([0.82, 0.86, 0.95], ambient * ao))), [specular; 3]);
        // A gentle studio vignette on the backdrop.
        let vig = 1.0 - 0.35 * smoothstep(0.3, 1.0, ((x / fw - 0.5).powi(2) + (y / fh - 0.45).powi(2)).sqrt() * 1.6);
        mul(c, vig)
    });
    let mut buf = buf;
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}

/// "Glass Orb": a translucent glass sphere with a soft contact shadow, on a transparent
/// background (PNG with alpha).
pub fn glass_orb(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let (cx, cy, r) = (fw * 0.5, fh * 0.44, fw * 0.33);
    let ss = 2;
    let (sw, sh) = (w * ss, h * ss);
    let mut big = Buf::new(sw, sh);
    let mut alpha = vec![0.0f32; sw * sh];
    for y in 0..sh {
        for x in 0..sw {
            let (fx, fy) = ((x as f32 + 0.5) / ss as f32, (y as f32 + 0.5) / ss as f32);
            let (dx, dy) = ((fx - cx) / r, (fy - cy) / r);
            let d2 = dx * dx + dy * dy;
            let i = y * sw + x;
            // Contact shadow: a blurred ellipse under the orb.
            let (sx, sy) = ((fx - cx) / (r * 0.95), (fy - (cy + r * 1.02)) / (r * 0.16));
            let sd = sx * sx + sy * sy;
            let shadow_a = 0.55 * (-sd * 1.6).exp();
            let mut col = [0.03, 0.04, 0.08];
            let mut a = shadow_a;
            if d2 <= 1.0 {
                let z = (1.0 - d2).sqrt();
                let n = [dx, dy, z];
                let fres = 0.08 + 0.92 * (1.0 - z).powi(3);
                // Refracted interior: a teal-to-blue gradient with a caustic near the bottom.
                let interior = mix(hex(0x6FE3F0), hex(0x2B4FC9), clamp01(0.5 + 0.5 * dy + 0.2 * dx));
                let caustic = (-((dx + 0.1).powi(2) / 0.08 + (dy - 0.62).powi(2) / 0.02)).exp() * 0.9;
                let mut c = add(interior, mul(hex(0xE8FFFF), caustic));
                c = mix(c, hex(0xDDEBFF), fres * 0.7);
                // Specular highlights from a window-like light at the top left.
                let l = normalize([-0.5, -0.6, 0.62]);
                let hv = normalize(add(l, [0.0, 0.0, 1.0]));
                let spec = dot(n, hv).max(0.0).powf(90.0) * 1.4;
                let spec2 = (-((dx - 0.35).powi(2) + (dy - 0.42).powi(2)) / 0.004).exp() * 0.5;
                c = add(c, [spec + spec2; 3]);
                let body_a = 0.42 + 0.5 * fres + 0.5 * spec.min(1.0) + caustic * 0.3;
                // Composite the orb over its own shadow.
                let oa = clamp01(body_a);
                a = oa + shadow_a * (1.0 - oa);
                col = if a > 0.0 { mul(add(mul(c, oa), mul(col, shadow_a * (1.0 - oa))), 1.0 / a) } else { c };
            }
            big.px[i] = col.map(clamp01);
            alpha[i] = clamp01(a);
        }
    }
    big.alpha = Some(alpha);
    big.downsample(ss)
}

/// "Retro Sunset": a synthwave sunset over a neon grid (saved as BMP).
pub fn retro_sunset(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let horizon = fh * 0.62;
    let (sun_x, sun_y, sun_r) = (fw * 0.5, fh * 0.43, fh * 0.26);
    let sky = [(0.0, hex(0x170A33)), (0.45, hex(0x4A1466)), (0.8, hex(0xC2306E)), (1.0, hex(0xFF7A59))];
    render(w, h, 3, |x, y| {
        let mut c;
        if y < horizon {
            c = gradient(&sky, y / horizon);
            // The striped sun.
            let (dx, dy) = (x - sun_x, y - sun_y);
            let d = (dx * dx + dy * dy).sqrt();
            let k = (y - (sun_y - sun_r)) / (2.0 * sun_r);
            let stripe_gap = if k > 0.42 {
                let s = ((k - 0.42) * 11.0).fract();
                s < 0.08 + 0.5 * (k - 0.42) / 0.58
            } else {
                false
            };
            if d < sun_r && !stripe_gap {
                c = mix(hex(0xFFE36E), hex(0xFF3C8E), clamp01(k * 1.1));
            }
            c = add(c, mul(hex(0xFF5FA2), 0.35 * (-((d - sun_r).max(0.0)) / (fh * 0.06)).exp()));
            // Mountains with neon rims.
            let u = x / fw;
            let m = horizon - fh * (0.09 + 0.12 * (ridged(u * 3.0, 0.5, 5, 701) - 0.3).max(0.0));
            if y > m {
                c = mix(hex(0x1B0B35), hex(0x2D1150), (y - m) / (horizon - m));
                c = add(c, mul(hex(0xFF4FD8), (-(y - m) / 2.0).exp() * 0.9));
            }
        } else {
            // Perspective grid on a dark floor.
            let d = (y - horizon) / (fh - horizon);
            let z = 1.0 / d.max(1e-3);
            let gx = (x - fw / 2.0) / fw * z * 6.0;
            let gz = z * 1.6;
            let line = |v: f32, width: f32| smoothstep(width, 0.0, (v - v.round()).abs());
            let wline = 0.04 * z.min(8.0) / 2.0;
            // Fade the lines out towards the horizon, where they would only alias.
            let grid = line(gx, wline.max(0.02)).max(line(gz, (0.02 * z).min(0.3))) * smoothstep(0.0, 0.2, d);
            c = mix(hex(0x12062A), hex(0x2A0B4A), d);
            c = add(c, mul(mix(hex(0x29E0FF), hex(0xFF4FD8), 1.0 - d), grid * (0.4 + 0.6 * d)));
            c = add(c, mul(hex(0xFF5FA2), (-(y - horizon) / 6.0).exp() * 0.8));
        }
        c
    })
}

/// "Misty Forest": rows of tall trunks receding into sunlit morning fog, with light shafts.
pub fn misty_forest(w: usize, h: usize) -> Buf {
    let (fw, fh) = (w as f32, h as f32);
    let sun = (fw * 0.36, -fh * 0.1);
    let horizon = fh * 0.6;
    let layers = 7;
    // Trunks per layer, far to near: (x at the base, half width, lean per pixel of height).
    let mut rng = Rng::new(901);
    let rows: Vec<Vec<(f32, f32, f32)>> = (0..layers)
        .map(|layer| {
            let t = layer as f32 / (layers - 1) as f32;
            let count = (14.0 * (1.0 - t) + 2.0) as usize;
            (0..count)
                .map(|_| {
                    let half = fw * (0.0025 + 0.03 * t * t) * rng.range(0.7, 1.3);
                    (rng.range(-0.05, 1.05) * fw, half, rng.range(-0.025, 0.025))
                })
                .collect()
        })
        .collect();
    // Where the ground at layer `t` meets the trunks, and how much fog lies in front of it.
    let base_y = |t: f32| horizon + (fh - horizon) * 1.15 * t * t + fh * 0.02;
    let fog_of = |t: f32| 0.93 * (1.0 - t).powf(1.4);
    let fog_at = |x: f32, y: f32| -> Rgb {
        let d = ((x - sun.0).powi(2) + (y - sun.1).powi(2)).sqrt() / fh;
        mix(mix(hex(0x75837E), hex(0xC4C6B8), clamp01(1.25 - d * 0.7)), hex(0xFFF0CF), (-d * 1.7).exp())
    };
    let mut buf = render(w, h, 2, |x, y| {
        let fog = fog_at(x, y);
        let mut c = fog;
        // How much fog lies between the camera and the visible surface (for the light shafts).
        let mut haze = 1.0;
        // The forest floor: leaves and ferns, fading into the fog with distance.
        if y > horizon {
            let k = ((y - horizon) / (fh - horizon)).sqrt();
            let n = fbm(x * 0.15 / (0.15 + k), y * 0.45 / (0.15 + k), 5, 902) * 0.5 + 0.5;
            let moss = smoothstep(0.1, 0.5, noise(x * 0.02, y * 0.06, 903));
            let floor = mix(mix(hex(0x2A271A), hex(0x75603A), n), hex(0x3F4B27), moss * 0.6);
            let f = fog_of(clamp01(k * 0.9));
            c = mix(floor, fog, f);
            haze = f;
        }
        for (layer, row) in rows.iter().enumerate() {
            let t = layer as f32 / (layers - 1) as f32;
            let base = base_y(t);
            let above = base - y;
            if above < -(fw * 0.04 * t + 3.0) {
                continue;
            }
            let f = fog_of(t);
            for &(tx, half, lean) in row {
                let cx = tx + lean * above + half * 0.3 * noise1(y * 0.01 + tx, 906);
                // A soft contact shadow on the ground around the foot of the trunk.
                if above < 0.5 {
                    let k = (-((x - cx) / (half * 1.8)).powi(2)).exp() * (above / (half * 0.25 + 1.0)).exp();
                    c = mul(c, 1.0 - 0.5 * k * (1.0 - f));
                    continue;
                }
                // Slightly tapered, flaring out at the roots.
                let flare = 1.0 + 0.7 * (-above / (half * 1.5 + 1.0)).exp();
                let half = half * (1.0 + 0.12 * above / fh) * flare;
                let s = (x - cx) / half;
                if s.abs() > 1.0 + 1.0 / half {
                    continue;
                }
                let cover = clamp01(half - (x - cx).abs() + 0.5) * clamp01(above + 0.5);
                // Dark bark with vertical grain, and a rim of light on the side facing the sun.
                let grain = noise(s * 2.5 + tx, y * 0.04 / (0.3 + t), 905) * 0.5 + 0.5;
                let mut bark = mix(hex(0x231C17), hex(0x4A3B2E), grain * (0.5 + 0.5 * t));
                let toward = if cx > sun.0 { -s } else { s };
                bark = add(bark, mul(hex(0xFFD8A0), 0.3 * smoothstep(0.55, 1.0, toward) * t));
                c = mix(c, mix(bark, fog, f), cover);
                haze += (f - haze) * cover;
            }
        }
        // Shafts of light fanning out from the hidden sun, brightest in deep fog.
        let ang = (x - sun.0).atan2(y - sun.1);
        let shaft = (fbm(ang * 9.0, 0.5, 3, 907) * 0.5 + 0.5).powi(4) * 3.0;
        let fall = (-((x - sun.0).powi(2) + (y - sun.1).powi(2)).sqrt() / (fh * 0.9)).exp();
        add(c, mul(hex(0xFFE4B8), shaft * fall * (0.15 + 0.85 * haze) * 0.5))
    });
    vignette(&mut buf, 0.3);
    for p in &mut buf.px {
        *p = tonemap(*p);
    }
    buf
}
