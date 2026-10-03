//! Tests against the bundled fonts.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use std::println;

use crate::*;

pub(crate) const INTER: &[u8] = include_bytes!("../../../assets/fonts/Inter-Regular.otf");
pub(crate) const INTER_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Inter-SemiBold.otf");
pub(crate) const LATO: &[u8] = include_bytes!("../../../assets/fonts/Lato-Regular.ttf");
pub(crate) const LATO_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Lato-Bold.ttf");
pub(crate) const JBM: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
pub(crate) const JBM_BOLD: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf");

pub(crate) const ALL: [(&str, &[u8]); 6] = [
    ("Inter-Regular", INTER),
    ("Inter-SemiBold", INTER_SEMIBOLD),
    ("Lato-Regular", LATO),
    ("Lato-Bold", LATO_BOLD),
    ("JetBrainsMono-Regular", JBM),
    ("JetBrainsMono-Bold", JBM_BOLD),
];

fn font(data: &'static [u8]) -> Font<'static> {
    Font::from_bytes(data).unwrap()
}

fn gid(f: &Font<'_>, c: char) -> GlyphId {
    f.glyph_index(c).unwrap_or_else(|| panic!("no glyph for {c:?}"))
}

/// The UI fallback chain: Inter -> Lato -> JetBrains Mono.
fn ui_collection() -> FontCollection<'static> {
    let mut c = FontCollection::new();
    c.add(font(INTER));
    c.add(font(LATO));
    c.add(font(JBM));
    c
}

#[test]
fn parse_bundled_fonts() {
    let expected = [
        (2548, 2816, "Inter", "Regular", OutlineFormat::Cff),
        (2548, 2816, "Inter", "Semi Bold", OutlineFormat::Cff),
        (3023, 2000, "Lato", "Regular", OutlineFormat::TrueType),
        (3023, 2000, "Lato", "Bold", OutlineFormat::TrueType),
        (1743, 1000, "JetBrains Mono", "Regular", OutlineFormat::TrueType),
        (1743, 1000, "JetBrains Mono", "Bold", OutlineFormat::TrueType),
    ];
    for ((name, data), (glyphs, upem, family, sub, fmt)) in ALL.iter().zip(expected) {
        let f = font(data);
        assert_eq!(f.num_glyphs(), glyphs, "{name}");
        assert_eq!(f.units_per_em(), upem, "{name}");
        assert_eq!(f.family_name().as_deref(), Some(family), "{name}");
        assert_eq!(f.subfamily_name().as_deref(), Some(sub), "{name}");
        assert_eq!(f.outline_format(), fmt, "{name}");
        let m = f.metrics();
        assert!(m.ascender > 0 && m.descender < 0, "{name}");
        assert!(m.x_height > 0 && m.x_height < m.cap_height && m.cap_height < m.ascender, "{name}: {m:?}");
        assert_eq!(Font::count(data), 1);
        assert!(f.full_name().unwrap().starts_with(family));
        assert!(f.postscript_name().is_some() && f.version().is_some());
    }
    let jbm = font(JBM);
    assert!(jbm.metrics().is_monospace);
    assert!(!font(INTER).metrics().is_monospace);
    assert_eq!(font(LATO_BOLD).metrics().weight_class, 700);
}

#[test]
fn font_names_and_licenses() {
    println!();
    for (name, data) in ALL {
        let f = font(data);
        let copyright = f.copyright().expect("copyright (name id 0)");
        let license = f.license().expect("license (name id 13)");
        let url = f.license_url().unwrap_or_default();
        println!("{name}:");
        println!("  copyright (0):    {copyright}");
        println!("  license (13):     {license}");
        println!("  license URL (14): {url}");
        assert!(copyright.contains("Copyright") || copyright.contains('©'), "{name}: {copyright}");
        assert!(license.contains("Open Font License") || license.contains("OFL"), "{name}: {license}");
    }
}

#[test]
fn cmap_ascii_latin1_and_euro() {
    for (name, data) in ALL {
        let f = font(data);
        for c in (0x20u8..=0x7E).map(char::from) {
            assert!(f.glyph_index(c).is_some(), "{name}: missing {c:?}");
        }
        for cp in 0xA0u32..=0xFF {
            let c = char::from_u32(cp).unwrap();
            assert!(f.glyph_index(c).is_some(), "{name}: missing U+{cp:04X}");
        }
        for c in ['€', '—', '–', '“', '”', '…', '•', 'Œ', 'œ', 'Ÿ', 'Ł', 'ł', 'Ș'] {
            assert!(f.glyph_index(c).is_some(), "{name}: missing {c:?}");
        }
        // Distinct glyphs for distinct letters, consistent ASCII fast path.
        assert_ne!(gid(&f, 'a'), gid(&f, 'b'));
        assert_eq!(f.glyph_index('A'), f.glyph_index('A'));
        assert_eq!(f.glyph_index('\u{10FFFF}'), None);
        assert_eq!(f.glyph_index('\u{0378}'), None, "{name}: unassigned code point");
        assert_eq!(f.glyph_index('\u{10FFFD}'), None, "{name}: plane 16 private use");
    }
}

#[test]
fn advance_widths_are_plausible() {
    for (name, data) in ALL {
        let f = font(data);
        let em = f.units_per_em() as f32;
        let adv = |c| f.advance_width(gid(&f, c)) as f32 / em;
        for c in (0x21u8..=0x7E).map(char::from) {
            let a = adv(c);
            assert!(a > 0.15 && a < 1.2, "{name}: advance of {c:?} = {a}");
        }
        let space = adv(' ');
        assert!(space > 0.15 && space < 0.65, "{name}: space {space}");
        if !f.metrics().is_monospace {
            assert!(adv('i') < adv('m'), "{name}");
            assert!(adv('l') < adv('W'), "{name}");
        }
        assert_eq!(f.advance_width(GlyphId(f.num_glyphs())), 0);
        assert!(f.left_side_bearing(gid(&f, 'H')) > 0);
    }
}

#[test]
fn monospace_advances_are_equal() {
    for data in [JBM, JBM_BOLD] {
        let f = font(data);
        let w = f.advance_width(gid(&f, 'M'));
        assert_eq!(w, 600);
        for c in (0x20u8..=0x7E).map(char::from).chain("éüßÆ€—…".chars()) {
            assert_eq!(f.advance_width(gid(&f, c)), w, "{c:?}");
        }
        let fonts = {
            let mut c = FontCollection::new();
            c.add(f.clone());
            c
        };
        let st = fonts.scaled(0, 20.0);
        assert!((st.measure("iiii") - st.measure("WWWW")).abs() < 1e-4);
    }
}

#[test]
fn kerning_from_gpos() {
    for data in [INTER, INTER_SEMIBOLD, LATO, LATO_BOLD] {
        let f = font(data);
        assert!(f.has_kerning());
        for (a, b) in [('A', 'V'), ('T', 'o'), ('T', 'e'), ('V', 'A'), ('L', 'T'), ('Y', 'o')] {
            let k = f.kerning(gid(&f, a), gid(&f, b));
            assert!(k < 0, "{:?} {a}{b} = {k}", f.full_name());
            // Within a sane range: less than a third of an em.
            assert!((k as i32).abs() < f.units_per_em() as i32 / 3);
        }
        assert_eq!(f.kerning(gid(&f, 'o'), gid(&f, 'o')), 0);
        // The cache returns exactly the uncached values (including after collisions).
        let letters: Vec<GlyphId> = ('A'..='Z').chain('a'..='z').map(|c| gid(&f, c)).collect();
        for _ in 0..2 {
            for &l in &letters {
                for &r in &letters {
                    assert_eq!(f.kerning(l, r), f.kerning_uncached(l, r));
                }
            }
        }
    }
    let j = font(JBM);
    assert!(!j.has_kerning());
    assert_eq!(j.kerning(gid(&j, 'A'), gid(&j, 'V')), 0);
}

/// Bounds of the flattened outline (close to the true curve bounds).
fn flat_bounds(f: &Font<'_>, g: GlyphId) -> Option<Rect> {
    let mut p = Path::new();
    f.outline(g, &mut p).unwrap();
    let mut r: Option<Rect> = None;
    p.flatten(0.5, |el| {
        if let PathEl::MoveTo(q) | PathEl::LineTo(q) = el {
            match &mut r {
                Some(r) => r.include(q),
                None => r = Some(Rect::from_point(q)),
            }
        }
    });
    r
}

use vraster::PathEl;

#[test]
fn all_outlines_parse_and_lie_within_head_bbox() {
    for (name, data) in ALL {
        let f = font(data);
        let m = *f.metrics();
        let mut nonempty = 0;
        let mut scratch = OutlineScratch::new();
        for g in 0..f.num_glyphs() {
            let g = GlyphId(g);
            let mut b = BoundsSink::default();
            f.outline_with(g, &mut b, &mut scratch).unwrap_or_else(|e| panic!("{name}: glyph {} failed: {e}", g.0));
            let Some(cb) = b.bounds else { continue };
            nonempty += 1;
            let fb = flat_bounds(&f, g).unwrap();
            let tol = 1.5;
            assert!(
                fb.x0 >= m.x_min as f32 - tol
                    && fb.y0 >= m.y_min as f32 - tol
                    && fb.x1 <= m.x_max as f32 + tol
                    && fb.y1 <= m.y_max as f32 + tol,
                "{name}: glyph {} bounds {fb:?} outside head bbox {m:?}",
                g.0
            );
            // TrueType: the control box equals the bbox stored in the glyph header.
            if let Some(h) = f.glyph_header_bbox(g) {
                let hb = [h[0] as f32, h[1] as f32, h[2] as f32, h[3] as f32];
                let d = (cb.x0 - hb[0])
                    .abs()
                    .max((cb.y0 - hb[1]).abs())
                    .max((cb.x1 - hb[2]).abs())
                    .max((cb.y1 - hb[3]).abs());
                assert!(d <= 1.0, "{name}: glyph {} control box {cb:?} vs header {h:?}", g.0);
            }
        }
        assert!(nonempty > f.num_glyphs() as usize / 2, "{name}: only {nonempty} non-empty glyphs");
    }
}

#[test]
fn outline_geometry() {
    // 'H' spans the cap height; 'o' overshoots the x-height slightly; 'p' descends.
    for (name, data) in ALL {
        let f = font(data);
        let m = *f.metrics();
        let b = f.glyph_bounds(gid(&f, 'H')).unwrap().unwrap();
        assert!((b.y1 - m.cap_height as f32).abs() <= m.units_per_em as f32 * 0.02, "{name}: H top {}", b.y1);
        assert!(b.y0.abs() <= 1.0, "{name}: H bottom {}", b.y0);
        let o = flat_bounds(&f, gid(&f, 'o')).unwrap();
        assert!(o.y1 >= m.x_height as f32 && o.y1 < m.x_height as f32 * 1.06, "{name}: o top {}", o.y1);
        let p = f.glyph_bounds(gid(&f, 'p')).unwrap().unwrap();
        assert!(p.y0 < -(m.units_per_em as f32) * 0.1, "{name}: p bottom {}", p.y0);
        // Space has no outline.
        assert!(f.glyph_bounds(gid(&f, ' ')).unwrap().is_none());
        // Invalid glyph ids are errors.
        assert_eq!(f.glyph_bounds(GlyphId(u16::MAX)), Err(FontError::InvalidGlyph));
        // Pixel paths are scaled and flipped.
        let path = f.glyph_path(gid(&f, 'H'), 100.0).unwrap();
        let pb = path.bounds().unwrap();
        let cap_px = m.cap_height as f32 * 100.0 / m.units_per_em as f32;
        assert!((pb.y0 + cap_px).abs() < 2.0 && pb.y1.abs() < 0.5, "{name}: {pb:?}");
    }
}

/// Sum of the signed areas of the flattened contours (font units squared).
fn outline_area(f: &Font<'_>, g: GlyphId, scale: f32) -> f64 {
    let mut p = Path::new();
    f.outline(g, &mut p).unwrap();
    let mut area = 0.0f64;
    let mut first = Point::ZERO;
    let mut last = Point::ZERO;
    let acc = |a: Point, b: Point| (a.x as f64 * b.y as f64 - b.x as f64 * a.y as f64) * 0.5;
    p.flatten(0.01, |el| match el {
        PathEl::MoveTo(q) => {
            area += acc(last, first);
            first = q;
            last = q;
        }
        PathEl::LineTo(q) => {
            area += acc(last, q);
            last = q;
        }
        _ => {}
    });
    area += acc(last, first);
    (area * (scale as f64) * (scale as f64)).abs()
}

#[test]
fn rasterized_ink_matches_outline_area() {
    let mut r = GlyphRasterizer::with_options(RasterOptions::LINEAR);
    for (name, data) in ALL {
        let f = font(data);
        for c in ['H', 'O', 'e', 'g', '8', '@', 'W', 'x'] {
            let g = gid(&f, c);
            let size = 37.0;
            let expected = outline_area(&f, g, size / f.units_per_em() as f32);
            for sub in [0.0f32, 0.25, 0.5, 0.75] {
                let bmp = r.rasterize(&f, g, size, sub);
                let ink: f64 = bmp.data.iter().map(|&v| v as f64 / 255.0).sum();
                let err = (ink - expected).abs() / expected;
                assert!(err < 0.01, "{name} {c:?} sub {sub}: ink {ink} vs area {expected}");
            }
        }
    }
}

#[test]
fn glyph_bitmap_placement() {
    let f = font(INTER);
    let mut r = GlyphRasterizer::with_options(RasterOptions::LINEAR);
    let h = r.rasterize(&f, gid(&f, 'H'), 20.0, 0.0);
    let cap = f.metrics().cap_height as f32 * 20.0 / f.units_per_em() as f32;
    assert_eq!(h.top, vraster::math::ceil(cap) as i32);
    assert_eq!(h.height as i32, h.top, "H sits on the baseline");
    assert!(h.left >= 0 && h.left <= 3);
    assert!(h.data.contains(&255), "stems should have fully covered pixels");
    assert_eq!(h.data.len(), (h.width * h.height) as usize);
    let p = r.rasterize(&f, gid(&f, 'p'), 20.0, 0.0);
    assert!(p.top - p.height as i32 <= -4, "p descends below the baseline");
    let space = r.rasterize(&f, gid(&f, ' '), 20.0, 0.0);
    assert!(space.is_empty() && space.data.is_empty());
    // Subpixel offsets move ink to the right without changing it.
    let a0 = r.rasterize(&f, gid(&f, 'l'), 16.0, 0.0);
    let a5 = r.rasterize(&f, gid(&f, 'l'), 16.0, 0.5);
    // Left edge of the stem from the coverage of its first pixel in a middle row.
    let edge = |b: &GlyphBitmap| {
        let row = b.row(b.height / 2);
        let i = row.iter().position(|&v| v > 0).unwrap();
        b.left as f32 + i as f32 + 1.0 - row[i] as f32 / 255.0
    };
    assert!((edge(&a5) - edge(&a0) - 0.5).abs() < 0.01, "{} {}", edge(&a0), edge(&a5));
    let ink = |b: &GlyphBitmap| b.data.iter().map(|&v| v as u32).sum::<u32>();
    assert!((ink(&a5) as i32 - ink(&a0) as i32).abs() <= a0.height as i32 * 2);
    // Degenerate sizes give empty bitmaps.
    assert!(r.rasterize(&f, gid(&f, 'H'), 0.0, 0.0).is_empty());
    assert!(r.rasterize(&f, gid(&f, 'H'), f32::NAN, 0.0).is_empty());
    assert!(r.rasterize(&f, GlyphId(60000), 12.0, 0.0).is_empty());
    // Darkening adds ink; gamma brightens partial coverage.
    let lin = r.rasterize(&f, gid(&f, 'e'), 12.0, 0.0);
    let mut d = GlyphRasterizer::with_options(RasterOptions {
        darkening: 0.4,
        darkening_full_size: 20.0,
        darkening_zero_size: 30.0,
        ..RasterOptions::LINEAR
    });
    let dark = d.rasterize(&f, gid(&f, 'e'), 12.0, 0.0);
    assert!(ink(&dark) > ink(&lin) * 11 / 10);
    let mut gm = GlyphRasterizer::with_options(RasterOptions { gamma: 1.8, ..RasterOptions::LINEAR });
    assert!(ink(&gm.rasterize(&f, gid(&f, 'e'), 12.0, 0.0)) > ink(&lin));
    // The free function matches the default rasterizer.
    assert_eq!(
        rasterize_glyph(&f, gid(&f, 'Q'), 13.0, 0.25),
        GlyphRasterizer::new().rasterize(&f, gid(&f, 'Q'), 13.0, 0.25)
    );
}

#[test]
fn raster_options_darkening_curve() {
    let o = RasterOptions::default();
    assert_eq!(o.darkening_at(10.0), o.darkening);
    assert_eq!(o.darkening_at(o.darkening_zero_size), 0.0);
    assert_eq!(o.darkening_at(100.0), 0.0);
    let mid = o.darkening_at((o.darkening_full_size + o.darkening_zero_size) * 0.5);
    assert!((mid - o.darkening * 0.5).abs() < 1e-5);
    assert_eq!(RasterOptions::LINEAR.darkening_at(8.0), 0.0);
}

#[test]
fn composite_glyphs() {
    // Lato builds accented letters from components: 'é' = 'e' + acute.
    let f = font(LATO);
    let e = f.glyph_bounds(gid(&f, 'e')).unwrap().unwrap();
    let ea = f.glyph_bounds(gid(&f, 'é')).unwrap().unwrap();
    assert!((ea.y0 - e.y0).abs() < 1.0 && ea.y1 > e.y1 + 200.0, "{e:?} {ea:?}");
    let mut composites = 0;
    for g in 0..f.num_glyphs() {
        if let Ok(d) = glyf_data(&f, g)
            && d.len() >= 2
            && i16::from_be_bytes([d[0], d[1]]) < 0
        {
            composites += 1;
        }
    }
    assert!(composites > 100, "Lato should contain many composite glyphs, found {composites}");
}

fn glyf_data<'a>(f: &Font<'a>, g: u16) -> Result<&'a [u8], FontError> {
    let loca = f.table(*b"loca").unwrap();
    let glyf = f.table(*b"glyf").unwrap();
    let long = i16::from_be_bytes([f.table(*b"head").unwrap()[50], f.table(*b"head").unwrap()[51]]) != 0;
    let i = g as usize;
    let (s, e) = if long {
        let rd = |o: usize| u32::from_be_bytes([loca[o], loca[o + 1], loca[o + 2], loca[o + 3]]) as usize;
        (rd(i * 4), rd(i * 4 + 4))
    } else {
        let rd = |o: usize| u16::from_be_bytes([loca[o], loca[o + 1]]) as usize * 2;
        (rd(i * 2), rd(i * 2 + 2))
    };
    glyf.get(s..e).ok_or(FontError::MalformedGlyph)
}

#[test]
fn collection_fallback() {
    let fonts = ui_collection();
    let inter = fonts.font(0).unwrap();
    let jbm = fonts.font(2).unwrap();
    // Find characters that Inter lacks but JetBrains Mono has (box drawing, symbols, ...).
    let missing: Vec<char> = (0x2000u32..0x2700)
        .filter_map(char::from_u32)
        .filter(|&c| !inter.has_glyph(c) && !fonts.font(1).unwrap().has_glyph(c) && jbm.has_glyph(c))
        .take(5)
        .collect();
    assert!(!missing.is_empty(), "expected some JetBrains Mono-only characters");
    for &c in &missing {
        let (fi, g) = fonts.resolve(0, c);
        assert_eq!(fi, 2, "{c:?}");
        assert_eq!(Some(g), jbm.glyph_index(c));
    }
    // Inter has 'A'; primary wins.
    assert_eq!(fonts.resolve(0, 'A').0, 0);
    assert_eq!(fonts.resolve(1, 'A').0, 1);
    // Nothing has the unassigned U+0378: .notdef of the primary.
    assert_eq!(fonts.resolve(1, '\u{0378}'), (1, GlyphId::NOTDEF));
    // Layout reports the fallback font index per glyph.
    let st = fonts.scaled(0, 16.0);
    let text: String = ['a', missing[0], 'b'].iter().collect();
    let glyphs = st.layout_line(&text, Point::new(0.0, 20.0));
    assert_eq!(glyphs.len(), 3);
    assert_eq!(glyphs[0].font, 0);
    assert_eq!(glyphs[1].font, 2);
    assert_eq!(glyphs[2].font, 0);
    assert_eq!(glyphs[1].cluster, 1);
    assert_eq!(glyphs[2].cluster as usize, 1 + missing[0].len_utf8());
}

#[test]
fn scaled_metrics_and_measuring() {
    let fonts = ui_collection();
    let st = fonts.scaled(0, 16.0);
    let m = fonts.font(0).unwrap().metrics();
    let s = 16.0 / m.units_per_em as f32;
    assert!((st.ascent() - m.ascender as f32 * s).abs() < 1e-4);
    assert!((st.descent() + m.descender as f32 * s).abs() < 1e-4);
    assert!((st.line_height() - (st.ascent() + st.descent() + st.line_gap())).abs() < 1e-4);
    assert!(st.x_height() > 6.0 && st.x_height() < 10.0, "{}", st.x_height());
    assert!(st.cap_height() > 10.0 && st.cap_height() < 13.0);
    assert!(st.line_height() > 16.0 && st.line_height() < 24.0);
    let (upos, uthick) = st.underline();
    assert!(upos > 0.0 && uthick >= 1.0);
    // Kerning makes "AV" narrower than its parts.
    let av = st.measure("AV");
    assert!(av < st.advance('A') + st.advance('V') - 0.5, "{av}");
    assert_eq!(st.measure(""), 0.0);
    assert!((st.measure("ab") - st.advance('a') - st.advance('b')).abs() < 0.5);
    // Control characters are invisible; tabs snap to stops of four spaces.
    assert_eq!(st.measure("\u{200B}\u{FEFF}\r\n"), 0.0);
    let tab = st.space_advance() * 4.0;
    assert!((st.measure("\t") - tab).abs() < 1e-3);
    assert!((st.measure("a\t") - tab).abs() < 1e-3);
    assert!((st.measure("a\tb") - tab - st.advance('b')).abs() < 1e-3);
    // Layout positions match the measurements.
    let text = "Type AVATAR to Vindows";
    let glyphs = st.layout_line(text, Point::new(10.0, 50.0));
    assert_eq!(glyphs.len(), text.chars().filter(|c| !c.is_whitespace()).count());
    for w in glyphs.windows(2) {
        assert!(w[1].x > w[0].x);
    }
    for g in &glyphs {
        assert_eq!(g.y, 50.0);
        let idx = g.cluster as usize;
        let expect = 10.0 + st.x_for_index(text, idx);
        assert!((g.x - expect).abs() < 1e-3);
    }
    let mut out = Vec::new();
    let end = st.layout_line_into(text, Point::new(10.0, 50.0), &mut out);
    assert!((end - 10.0 - st.measure(text)).abs() < 1e-3);
    assert_eq!(out, glyphs);
    // A degenerate style measures nothing but does not panic.
    let empty = FontCollection::new();
    let es = empty.scaled(0, 12.0);
    assert_eq!(es.measure("abc"), 0.0);
    assert_eq!(es.line_height(), 0.0);
    assert!(es.layout_line("abc", Point::ZERO).is_empty());
}

#[test]
fn caret_helpers() {
    let fonts = ui_collection();
    let st = fonts.scaled(0, 15.0);
    let text = "Hello, Wörld € ok";
    let total = st.measure(text);
    assert_eq!(st.x_for_index(text, 0), 0.0);
    assert!((st.x_for_index(text, text.len()) - total).abs() < 1e-4);
    assert!((st.x_for_index(text, 999) - total).abs() < 1e-4);
    let boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    let mut last = -1.0;
    for &i in &boundaries {
        let x = st.x_for_index(text, i);
        assert!(x > last, "x must increase");
        last = x;
        // Round trip, also slightly to the right of the boundary.
        assert_eq!(st.index_for_x(text, x), i);
        assert_eq!(st.index_for_x(text, x + 0.3), i);
    }
    // Inside a multi-byte character: rounds down to its start.
    let o = text.find('ö').unwrap();
    assert_eq!(st.x_for_index(text, o + 1), st.x_for_index(text, o));
    // Far left / far right.
    assert_eq!(st.index_for_x(text, -100.0), 0);
    assert_eq!(st.index_for_x(text, 1e6), text.len());
    // Hit testing picks the nearest boundary.
    let x1 = st.x_for_index(text, 1);
    let x2 = st.x_for_index(text, 2);
    assert_eq!(st.index_for_x(text, x1 + (x2 - x1) * 0.4), 1);
    assert_eq!(st.index_for_x(text, x1 + (x2 - x1) * 0.6), 2);
    assert_eq!(st.index_for_x("", 5.0), 0);
}

fn check_wrap(st: &ScaledFont<'_, '_>, text: &str, width: f32) -> Vec<core::ops::Range<usize>> {
    let lines = st.wrap_lines(text, width);
    // Lines are in order, non-overlapping, and only skip line terminators.
    let mut pos = 0;
    for r in &lines {
        assert!(r.start >= pos && r.start <= r.end && r.end <= text.len(), "{lines:?}");
        let skipped = &text[pos..r.start];
        assert!(skipped.chars().all(|c| c == '\n' || c == '\r'), "skipped {skipped:?}");
        pos = r.end;
        let line = &text[r.clone()];
        let visible = line.trim_end();
        // Lines with a single visible character may overflow (hard break limit), as may a
        // single word after indentation (there is no break opportunity before it).
        if visible.chars().filter(|c| !c.is_whitespace()).count() > 1 {
            assert!(st.measure(visible) <= width + 1e-3, "line {line:?} too wide ({} > {width})", st.measure(visible));
        }
    }
    assert!(text[pos..].chars().all(|c| c == '\n' || c == '\r'));
    lines
}

#[test]
fn word_wrapping() {
    let fonts = ui_collection();
    let st = fonts.scaled(0, 14.0);
    let w = st.measure("hello world") + 1.0;
    let text = "hello world hello world hello";
    let lines = check_wrap(&st, text, w);
    let strs: Vec<&str> = lines.iter().map(|r| &text[r.clone()]).collect();
    assert_eq!(strs, ["hello world ", "hello world ", "hello"]);
    // Explicit newlines, empty lines and a trailing newline.
    let text = "one\n\ntwo\r\nthree\n";
    let lines = check_wrap(&st, text, 1000.0);
    let strs: Vec<&str> = lines.iter().map(|r| &text[r.clone()]).collect();
    assert_eq!(strs, ["one", "", "two", "three", ""]);
    // Overlong words are broken between characters.
    let text = "Supercalifragilisticexpialidocious!";
    let lines = check_wrap(&st, text, 60.0);
    assert!(lines.len() > 3);
    // Zero width still makes progress (one character per line).
    let lines = check_wrap(&st, "abc def", 0.0);
    assert_eq!(lines.len(), 6);
    // Runs of spaces hang at the end of a line.
    let text = "aaa     bbb";
    let lines = check_wrap(&st, text, st.measure("aaa  "));
    assert_eq!(&text[lines[0].clone()], "aaa     ");
    assert_eq!(&text[lines[1].clone()], "bbb");
    // Leading spaces are kept on the first line; no-break spaces do not break.
    let text = "  indented\u{A0}word next";
    let lines = check_wrap(&st, text, st.measure("  indented\u{A0}word") + 2.0);
    assert_eq!(&text[lines[0].clone()], "  indented\u{A0}word ");
    assert_eq!(check_wrap(&st, "", 100.0), vec![0..0]);
    // Random texts and widths keep the invariants.
    let mut seed = 12345u32;
    let words = ["a", "Vindows", "kernel", "€", "—", "wörld", "x", "supercalifragilistic", "\n", "  "];
    for _ in 0..200 {
        let mut text = String::new();
        for _ in 0..(seed % 20) {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            text.push_str(words[(seed >> 16) as usize % words.len()]);
            text.push(' ');
        }
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let width = (seed >> 16) as f32 % 300.0;
        check_wrap(&st, &text, width);
    }
}

#[test]
fn subpixel_positions() {
    assert_eq!(subpixel_position(10.0), (10, 0));
    assert_eq!(subpixel_position(10.24), (10, 1));
    assert_eq!(subpixel_position(10.5), (10, 2));
    assert_eq!(subpixel_position(10.74), (10, 3));
    assert_eq!(subpixel_position(10.9), (11, 0));
    assert_eq!(subpixel_position(-0.3), (-1, 3));
    assert_eq!(subpixel_position(f32::NAN), (0, 0));
}

#[test]
fn glyph_cache_hits_and_eviction() {
    let f = font(INTER);
    let l = font(LATO);
    let mut cache = GlyphCache::new(1 << 20);
    let a1 = cache.get(&f, gid(&f, 'a'), 14.0, 0).clone();
    let a2 = cache.get(&f, gid(&f, 'a'), 14.0, 0).clone();
    assert_eq!(a1, a2);
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);
    // Different subpixel bins, sizes and fonts are distinct entries.
    let _ = cache.get(&f, gid(&f, 'a'), 14.0, 1);
    let _ = cache.get(&f, gid(&f, 'a'), 14.5, 0);
    let _ = cache.get(&l, gid(&l, 'a'), 14.0, 0);
    assert_eq!(cache.len(), 4);
    assert_eq!(a1, GlyphRasterizer::new().rasterize(&f, gid(&f, 'a'), 14.0, 0.0));
    let b1 = cache.get(&f, gid(&f, 'a'), 14.0, 1).clone();
    assert_eq!(b1, GlyphRasterizer::new().rasterize(&f, gid(&f, 'a'), 14.0, 0.25));
    let key = GlyphKey::new(&f, gid(&f, 'a'), 14.0, 0);
    assert_eq!(cache.peek(&key), Some(&a1));

    // A tiny budget forces evictions; results stay correct.
    let mut small = GlyphCache::new(3000);
    let mut fresh = GlyphRasterizer::new();
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".chars().collect();
    let mut seed = 7u32;
    for _ in 0..2000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let c = chars[(seed >> 8) as usize % chars.len()];
        let bin = ((seed >> 4) % 4) as u8;
        let fnt = if seed & 1 == 0 { &f } else { &l };
        let got = small.get(fnt, gid(fnt, c), 12.0, bin).clone();
        assert_eq!(got, fresh.rasterize(fnt, gid(fnt, c), 12.0, bin as f32 / 4.0));
        let st = small.stats();
        assert!(st.bytes <= 3000 || st.entries == 1, "{st:?}");
    }
    let st = small.stats();
    assert!(st.evictions > 100 && st.hits > 0, "{st:?}");
    small.set_budget(0);
    assert!(small.is_empty());
    cache.clear();
    assert!(cache.is_empty() && cache.stats().bytes == 0);
}

#[test]
fn truetype_collection_wrapper() {
    // Wrap Lato in a one-font TTC (table offsets are file-relative, so shift them).
    let shift = 16u32;
    let mut ttc = Vec::new();
    ttc.extend_from_slice(b"ttcf");
    ttc.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    ttc.extend_from_slice(&1u32.to_be_bytes());
    ttc.extend_from_slice(&shift.to_be_bytes());
    let mut body = LATO.to_vec();
    let n = u16::from_be_bytes([body[4], body[5]]) as usize;
    for i in 0..n {
        let o = 12 + i * 16 + 8;
        let v = u32::from_be_bytes([body[o], body[o + 1], body[o + 2], body[o + 3]]) + shift;
        body[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }
    ttc.extend_from_slice(&body);
    assert_eq!(Font::count(&ttc), 1);
    let f = Font::from_collection(&ttc, 0).unwrap();
    assert_eq!(f.family_name().as_deref(), Some("Lato"));
    assert!(f.glyph_bounds(gid(&f, 'g')).unwrap().is_some());
    assert_eq!(Font::from_collection(&ttc, 1).err(), Some(FontError::InvalidFontIndex));
    assert_eq!(Font::from_collection(LATO, 1).err(), Some(FontError::InvalidFontIndex));
}

/// Exercises every API on a (possibly corrupt) font; must never panic.
fn exercise(data: &[u8], glyph_step: usize) -> bool {
    let Ok(f) = Font::from_bytes(data) else { return false };
    let _ = (f.family_name(), f.copyright(), f.license(), f.metrics().x_height);
    let mut r = GlyphRasterizer::new();
    let mut scratch = OutlineScratch::new();
    let n = f.num_glyphs() as usize;
    for g in (0..n.saturating_add(3)).step_by(glyph_step.max(1)) {
        let g = GlyphId(g.min(u16::MAX as usize) as u16);
        let mut b = BoundsSink::default();
        let _ = f.outline_with(g, &mut b, &mut scratch);
        let _ = f.advance_width(g);
        let _ = f.kerning(g, GlyphId(g.0.wrapping_add(1)));
        if g.0.is_multiple_of(7) {
            let _ = r.rasterize(&f, g, 11.0, 0.5);
        }
    }
    for c in "AVTo€é—\u{1F600}".chars() {
        let _ = f.glyph_index(c);
    }
    let mut fonts = FontCollection::new();
    fonts.add(f);
    let st = fonts.scaled(0, 13.0);
    let _ = st.measure("Te AV — x");
    let _ = st.wrap_lines("hello world, how are you", 30.0);
    true
}

#[test]
fn malformed_fonts_never_panic() {
    // Truncation at many lengths.
    for (_, data) in ALL {
        let len = data.len();
        let mut cuts: Vec<usize> = (0..64).collect();
        cuts.extend((1..60).map(|i| len * i / 60));
        for cut in cuts {
            exercise(&data[..cut.min(len)], 97);
        }
    }
    // Random byte corruption (deterministic PRNG), concentrated on the table directory and the
    // beginning of tables as well as spread over the whole file.
    let (mut parsed, mut total) = (0u32, 0u32);
    let mut seed = 0x1234_5678u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for (_, data) in ALL {
        for round in 0..150 {
            let mut d = data.to_vec();
            let flips = 1 + next() as usize % 40;
            for _ in 0..flips {
                let pos = if round % 2 == 0 { next() as usize % 512.min(d.len()) } else { next() as usize % d.len() };
                d[pos] = next() as u8;
            }
            parsed += exercise(&d, 11) as u32;
            total += 1;
        }
    }
    println!("corrupted fonts that still parsed: {parsed}/{total}");
    assert!(parsed > total / 4, "corruption should mostly hit glyph data");
    // Garbage that looks like a font header.
    let mut junk = vec![0u8; 4096];
    junk[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    junk[4] = 0;
    junk[5] = 200;
    exercise(&junk, 1);
    assert!(Font::from_bytes(&[]).is_err());
    assert!(Font::from_bytes(b"OTTO").is_err());
    assert_eq!(Font::count(b"nope"), 0);
}

#[test]
fn self_referencing_composite_is_an_error() {
    // Point the first component of a composite glyph at the glyph itself: the recursion limit
    // must turn this into an error instead of a stack overflow.
    let lato = font(LATO);
    let is_composite = |g: u16| glyf_data(&lato, g).map(|d| d.len() > 14 && d[0] & 0x80 != 0).unwrap_or(false);
    let target = (0..lato.num_glyphs()).find(|&g| is_composite(g)).unwrap();
    let data_off = glyf_data(&lato, target).unwrap().as_ptr() as usize - LATO.as_ptr() as usize;
    let mut d = LATO.to_vec();
    // The first component's glyph index follows the 10-byte header and the 2-byte flags.
    d[data_off + 12..data_off + 14].copy_from_slice(&target.to_be_bytes());
    let bad = Font::from_bytes(&d).unwrap();
    assert_eq!(bad.glyph_bounds(GlyphId(target)), Err(FontError::LimitExceeded));
    assert!(GlyphRasterizer::new().rasterize(&bad, GlyphId(target), 16.0, 0.0).is_empty());
}

#[test]
fn performance_smoke() {
    // Cold rasterization of a screenful of text and repeated layout; prints timings.
    let fonts = ui_collection();
    let st = fonts.scaled(0, 14.0);
    let text = "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs! 0123456789";
    let t0 = std::time::Instant::now();
    let mut cache = GlyphCache::new(4 << 20);
    let mut glyphs = Vec::new();
    for line in 0..60 {
        glyphs.clear();
        st.layout_line_into(text, Point::new(0.0, line as f32 * 18.0), &mut glyphs);
        for g in &glyphs {
            let (_, bin) = subpixel_position(g.x);
            let _ = cache.get(fonts.font(g.font as usize).unwrap(), g.glyph, 14.0, bin);
        }
    }
    let dt = t0.elapsed();
    let s = cache.stats();
    println!("60 lines x {} chars: {:?} ({} misses, {} hits)", text.len(), dt, s.misses, s.hits);
    let t1 = std::time::Instant::now();
    let mut total = 0.0;
    for _ in 0..1000 {
        total += st.measure(text);
    }
    println!("1000 x measure: {:?} ({total})", t1.elapsed());
    let t2 = std::time::Instant::now();
    let mut r = GlyphRasterizer::new();
    let f = fonts.font(0).unwrap();
    let mut n = 0;
    for c in (0x21u8..0x7F).map(char::from) {
        for bin in 0..4 {
            n += r.rasterize(f, gid(f, c), 14.0, bin as f32 * 0.25).data.len();
        }
    }
    println!("cold rasterize 94 glyphs x 4 bins @14px: {:?} ({n} bytes)", t2.elapsed());
}

#[test]
fn render_line_matches_manual_layout() {
    let fonts = ui_collection();
    let st = fonts.scaled(0, 15.0);
    let text = "Hi there, AV! ┌─┐";
    let origin = Point::new(3.3, 20.0);
    let mut cache = GlyphCache::new(1 << 20);
    let mut calls = Vec::new();
    let end = cache.render_line(&st, text, origin, |x, y, b| calls.push((x, y, b.width, b.height)));
    let mut manual = Vec::new();
    for g in st.layout_line(text, origin) {
        let (px, bin) = subpixel_position(g.x);
        let b = cache.get(fonts.font(g.font as usize).unwrap(), g.glyph, 15.0, bin);
        if !b.is_empty() {
            manual.push((px + b.left, 20 - b.top, b.width, b.height));
        }
    }
    assert_eq!(calls, manual);
    assert!(calls.len() >= 12);
    assert!((end - origin.x - st.measure(text)).abs() < 1e-3);
}
