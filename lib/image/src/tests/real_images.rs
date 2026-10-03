//! Validation against real-world image files shipped with Windows.
//!
//! These tests are `#[ignore]`d: they read files from their system locations (never copied into
//! the repository) and are meant to be run manually, preferably in release mode:
//!
//! ```text
//! cargo test -p vimage --release -- --ignored --nocapture
//! ```
//!
//! Downscaled PNG previews of decoded images are written to `target/agents/vimage/out/` for
//! visual inspection.

use std::collections::BTreeMap;
use std::format;
use std::fs;
use std::path::{Path, PathBuf};
use std::println;
use std::string::{String, ToString};
use std::time::Instant;
use std::vec::Vec;

use crate::{DecodeOptions, Filter, Format, Image, ImageError};

fn out_dir() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/out");
    let _ = fs::create_dir_all(&p);
    p
}

/// Recursively collects files with one of the given (lowercase) extensions.
fn walk(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&p, exts, out);
        } else if let Some(ext) = p.extension().and_then(|x| x.to_str())
            && exts.contains(&ext.to_ascii_lowercase().as_str())
        {
            out.push(p);
        }
    }
}

fn file_stem(p: &Path) -> String {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("image").replace(' ', "_")
}

/// Writes a preview (at most 1024 pixels wide/high) as PNG.
fn save_preview(img: &Image, name: &str) {
    let t = crate::thumbnail(img, 1024, 1024, Filter::Lanczos3);
    let png = crate::png::encode(&t, 6).expect("encode preview");
    // Verify our own PNG output before writing it.
    assert_eq!(crate::png::decode(&png).expect("decode preview"), t);
    fs::write(out_dir().join(name), png).expect("write preview");
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore]
fn windows_wallpapers_and_lock_screens() {
    let mut files = Vec::new();
    walk(Path::new(r"C:\Windows\Web"), &["jpg", "jpeg", "png", "bmp"], &mut files);
    walk(Path::new(r"C:\Windows\SystemApps"), &["jpg", "jpeg"], &mut files);
    assert!(!files.is_empty(), "no sample images found");
    let mut progressive = Vec::new();
    for f in &files {
        let data = fs::read(f).expect("read");
        let info = crate::read_info(&data).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let t = Instant::now();
        let img = crate::decode(&data).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let dt = ms(t);
        assert_eq!((img.width, img.height), (info.width, info.height), "{}", f.display());
        assert!(img.width >= 16 && img.height >= 16);
        let detail = if info.format == Format::Jpeg {
            let j = crate::jpeg::read_info(&data).unwrap();
            format!("{:?} {} comps", j.color_model, j.components)
        } else {
            String::new()
        };
        println!(
            "{:>9} {:<70} {}x{} prog={} orient={:?} {} decode {:.1} ms",
            info.format.name(),
            f.display().to_string(),
            img.width,
            img.height,
            info.progressive,
            info.orientation,
            detail,
            dt
        );
        if info.progressive {
            progressive.push(f.display().to_string());
        }
        let name = format!(
            "wall_{}_{}.png",
            f.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str()).unwrap_or(""),
            file_stem(f)
        );
        save_preview(&img, &name);
    }
    println!("{} files, progressive: {progressive:?}", files.len());
}

#[test]
#[ignore]
fn windows_png_corpus() {
    let mut files = Vec::new();
    walk(Path::new(r"C:\Windows\SystemApps"), &["png"], &mut files);
    walk(Path::new(r"C:\Windows\Web"), &["png"], &mut files);
    walk(Path::new(r"C:\Program Files\qemu"), &["png"], &mut files);
    assert!(files.len() > 100);
    let mut kinds: BTreeMap<String, (usize, PathBuf)> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut bad_crc = Vec::new();
    let mut slowest: Vec<(f64, PathBuf)> = Vec::new();
    let mut total_bytes = 0usize;
    let mut total_pixels = 0u64;
    let t = Instant::now();
    for f in &files {
        let data = fs::read(f).expect("read");
        total_bytes += data.len();
        let info = match crate::png::read_info(&data) {
            Ok(i) => i,
            Err(e) => {
                failures.push(format!("{}: {e}", f.display()));
                continue;
            }
        };
        let t1 = Instant::now();
        let mut result = crate::png::decode(&data);
        if result == Err(ImageError::ChecksumMismatch("PNG chunk CRC")) {
            // A few shipped files really have bad CRCs (libpng rejects them too).
            bad_crc.push(f.display().to_string());
            result =
                crate::png::decode_with(&data, &DecodeOptions { verify_checksums: false, ..DecodeOptions::DEFAULT });
        }
        slowest.push((ms(t1), f.clone()));
        match result {
            Ok(img) => {
                assert_eq!((img.width, img.height), (info.width, info.height));
                total_pixels += img.pixels.len() as u64;
                let key = format!(
                    "{:?} {}-bit{}{}",
                    info.color_type,
                    info.bit_depth,
                    if info.interlaced { " interlaced" } else { "" },
                    if info.has_alpha { " alpha" } else { "" }
                );
                let e = kinds.entry(key).or_insert((0, f.clone()));
                e.0 += 1;
            }
            Err(e) => failures.push(format!("{}: {e}", f.display())),
        }
    }
    let dt = ms(t);
    for (k, (n, example)) in &kinds {
        println!("{n:>6}  {k:<40} e.g. {}", example.display());
    }
    println!(
        "{} PNG files, {:.1} MB, {:.1} Mpixels decoded in {:.0} ms",
        files.len(),
        total_bytes as f64 / 1e6,
        total_pixels as f64 / 1e6,
        dt
    );
    // Previews of one file per kind.
    for (i, (k, (_, example))) in kinds.iter().enumerate() {
        let img = crate::png::decode(&fs::read(example).unwrap()).unwrap();
        let tag: String = k.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        // Composite over a checkerboard so transparency is visible in the preview.
        let shown = Image::from_fn(img.width, img.height, |x, y| {
            let p = img.get(x, y).unwrap();
            let bg = if (x / 8 + y / 8) % 2 == 0 { 0xC0 } else { 0x80 };
            let a = p >> 24;
            let mix = |c: u32| (c * a + bg * (255 - a) + 127) / 255;
            0xFF00_0000 | mix((p >> 16) & 0xFF) << 16 | mix((p >> 8) & 0xFF) << 8 | mix(p & 0xFF)
        });
        save_preview(&shown, &format!("png_{i:02}_{tag}.png"));
    }
    slowest.sort_by(|a, b| b.0.total_cmp(&a.0));
    let decode_total: f64 = slowest.iter().map(|s| s.0).sum();
    println!("pure decode time {decode_total:.0} ms; slowest:");
    for (t, f) in slowest.iter().take(8) {
        println!("  {t:7.1} ms  {}", f.display());
    }
    for f in &bad_crc {
        println!("bad CRC (decoded with verification off): {f}");
    }
    for f in &failures {
        println!("FAILED {f}");
    }
    assert!(failures.is_empty(), "{} failures", failures.len());
}

#[test]
#[ignore]
fn windows_bmps() {
    let mut files = Vec::new();
    for dir in [
        r"C:\Windows\SystemApps",
        r"C:\Windows\WinSxS\amd64_microsoft-windows-usertiles-client_31bf3856ad364e35_10.0.26100.1_none_26898bc4817b0485",
        r"C:\Program Files\Windows Media Player",
        r"C:\Program Files\qemu",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE",
    ] {
        walk(Path::new(dir), &["bmp", "dib"], &mut files);
    }
    let mut ok = 0;
    for f in &files {
        let data = fs::read(f).expect("read");
        let info = match crate::bmp::read_info(&data) {
            Ok(i) => i,
            Err(e) => {
                println!("{}: header error {e}", f.display());
                continue;
            }
        };
        let img = crate::bmp::decode(&data).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        assert_eq!((img.width, img.height), (info.width, info.height));
        println!(
            "{:<110} {}x{} {}bpp compression={} top_down={} alpha={} (decoded alpha: {})",
            f.display().to_string(),
            info.width,
            info.height,
            info.bits_per_pixel,
            info.compression,
            info.top_down,
            info.has_alpha,
            img.has_alpha()
        );
        assert_eq!(info.has_alpha, img.has_alpha(), "{}", f.display());
        if ok < 12 {
            save_preview(&img, &format!("bmp_{ok:02}_{}.png", file_stem(f)));
        }
        ok += 1;
    }
    println!("{ok} BMP files decoded");
    assert!(ok > 5);
}

/// Decodes the largest sample JPEG and PNG repeatedly and reports timings, plus encoder and
/// resampler timings.
#[test]
#[ignore]
fn timings() {
    let jpegs = [
        r"C:\Windows\Web\Wallpaper\Windows\img0.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\DesktopSpotlight\Assets\Images\image_0.jpg",
        r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg",
    ];
    let pngs =
        [r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\VoiceIsolation\Assets\twoMics_dark.png"];
    let best = |f: &mut dyn FnMut()| -> f64 {
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            f();
            best = best.min(ms(t));
        }
        best
    };
    let mut decoded: Vec<Image> = Vec::new();
    for path in jpegs.iter().chain(pngs.iter()) {
        let Ok(data) = fs::read(path) else { continue };
        let info = crate::read_info(&data).unwrap();
        let mut img = Image::default();
        let t = best(&mut || img = crate::decode(&data).unwrap());
        let lax = DecodeOptions { verify_checksums: false, ..DecodeOptions::DEFAULT };
        let t2 = best(&mut || img = crate::decode_with(&data, &lax).unwrap());
        println!(
            "decode {:<5} {}x{} ({:.2} MB, progressive={}): {:.1} ms ({:.1} ms without checksums) — {}",
            info.format.name(),
            img.width,
            img.height,
            data.len() as f64 / 1e6,
            info.progressive,
            t,
            t2,
            path
        );
        decoded.push(img);
    }
    let Some(photo) = decoded.first().cloned() else { return };
    let mut out = Vec::new();
    for level in [1u8, 6, 9] {
        let t = best(&mut || out = crate::png::encode(&photo, level).unwrap());
        println!("png::encode level {level}: {:.1} ms, {} bytes", t, out.len());
    }
    let t = best(&mut || out = crate::jpeg::encode(&photo, &crate::jpeg::EncodeOptions::default()).unwrap());
    println!("jpeg::encode q90 4:2:0: {:.1} ms, {} bytes", t, out.len());
    let mut small = Image::default();
    for filter in [Filter::Nearest, Filter::Bilinear, Filter::Box, Filter::Lanczos3] {
        let t = best(&mut || small = crate::resize(&photo, 1280, 720, filter));
        let t2 = best(&mut || small = crate::thumbnail(&photo, 256, 256, filter));
        println!(
            "resize {}x{} -> 1280x720 {filter:?}: {:.1} ms; thumbnail 256: {:.1} ms",
            photo.width, photo.height, t, t2
        );
    }
    let t = best(&mut || small = crate::rotate90(&photo));
    println!("rotate90: {:.1} ms", t);
}

/// Compares our JPEG decoder with the Windows (WIC/GDI+) decoder: reference BMPs produced by
/// `target/agents/vimage/gdiplus_reference.ps1` are compared with our output.
#[test]
#[ignore]
fn compare_with_windows_decoder() {
    let ref_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/ref");
    let Ok(entries) = fs::read_dir(&ref_dir) else {
        println!("no reference directory; run gdiplus_reference.ps1 first");
        return;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("bmp") {
            continue;
        }
        // The reference name is `<stem>.bmp`, its source path is stored next to it in `<stem>.src`.
        let Ok(src) = fs::read_to_string(p.with_extension("src")) else { continue };
        let src = src.trim();
        let Ok(data) = fs::read(src) else { continue };
        let reference = crate::bmp::decode(&fs::read(&p).unwrap()).unwrap();
        let no_rotate = DecodeOptions { apply_orientation: false, ..DecodeOptions::DEFAULT };
        let ours = match crate::decode_with(&data, &no_rotate) {
            Ok(i) => i,
            Err(ImageError::Unsupported(m)) => {
                println!("{src}: unsupported ({m})");
                continue;
            }
            Err(e) => panic!("{src}: {e}"),
        };
        assert_eq!((ours.width, ours.height), (reference.width, reference.height), "{src}");
        let p = crate::jpeg::tests::psnr(&ours, &reference);
        let mut max_diff = 0i32;
        for (&a, &b) in ours.pixels.iter().zip(&reference.pixels) {
            for s in [0, 8, 16] {
                max_diff = max_diff.max((((a >> s) & 0xFF) as i32 - ((b >> s) & 0xFF) as i32).abs());
            }
        }
        println!("{src}: PSNR vs Windows decoder {p:.2} dB, max channel difference {max_diff}");
        assert!(p > 35.0, "{src}: {p:.2} dB");
        n += 1;
    }
    println!("{n} files compared");
}

/// Dumps the filtered PNG scanlines of a few real images to `target/agents/vimage/zcmp/` and
/// reports our compressed sizes, so they can be compared with zlib on identical input.
#[test]
#[ignore]
fn deflate_ratio_dump() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/zcmp");
    let _ = fs::create_dir_all(&dir);
    let sources = [
        r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\DesktopSpotlight\Assets\Images\image_1.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\VoiceIsolation\Assets\twoMics_dark.png",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.Core_cw5n1h2txyewy\FamilyValueProp\Assets\Images\family_promo_banner.png",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CoreAI_cw5n1h2txyewy\DiscoveryOverlay\Assets\TutorialMode\TutorialModeFirstLast_Dark.png",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\WindowsBackup\Assets\OneDriveInstallPageImage.png",
    ];
    for src in sources {
        let Ok(data) = fs::read(src) else { continue };
        let mut img = crate::decode(&data).unwrap();
        if img.width > 1920 {
            img = crate::resize(&img, 1920, img.height * 1920 / img.width, Filter::Box);
        }
        let opaque = !img.has_alpha();
        let raw = crate::png::filtered_scanlines(&img, opaque, true).unwrap();
        let name = file_stem(Path::new(src));
        fs::write(dir.join(format!("{name}.raw")), &raw).unwrap();
        let mut line = format!("{name:<32} {}x{} {:>9} raw", img.width, img.height, raw.len());
        for level in [1u8, 6, 9] {
            let t = Instant::now();
            let z = crate::deflate::zlib_compress(&raw, level);
            let dt = ms(t);
            assert_eq!(crate::inflate::zlib_decompress(&z, raw.len()).unwrap(), raw);
            fs::write(dir.join(format!("{name}.l{level}.z")), &z).unwrap();
            line += &format!("  L{level}: {:>8} ({:.0} ms)", z.len(), dt);
        }
        println!("{line}");
    }
}

/// Composites `img` over a gray checkerboard so that transparency is visible.
fn over_checkerboard(img: &Image) -> Image {
    Image::from_fn(img.width, img.height, |x, y| {
        let p = img.get(x, y).unwrap();
        let bg = if (x / 6 + y / 6) % 2 == 0 { 0xC8 } else { 0x90 };
        let a = p >> 24;
        let mix = |c: u32| (c * a + bg * (255 - a) + 127) / 255;
        0xFF00_0000 | mix((p >> 16) & 0xFF) << 16 | mix((p >> 8) & 0xFF) << 8 | mix(p & 0xFF)
    })
}

/// A contact sheet of PNG files of every color type (one row per kind) for visual inspection.
#[test]
#[ignore]
fn png_contact_sheet() {
    let mut files = Vec::new();
    walk(Path::new(r"C:\Windows\SystemApps"), &["png"], &mut files);
    let mut by_kind: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for f in &files {
        let Ok(data) = fs::read(f) else { continue };
        let Ok(info) = crate::png::read_info(&data) else { continue };
        if info.width < 16 || info.height < 16 {
            continue;
        }
        let key = format!("{:?}{}{}", info.color_type, info.bit_depth, if info.interlaced { "i" } else { "" });
        let v = by_kind.entry(key).or_default();
        // Prefer variety: skip files from the same directory.
        if v.len() < 8 && !v.iter().any(|p| p.parent() == f.parent()) {
            v.push(f.clone());
        }
    }
    const CELL: u32 = 96;
    let rows = by_kind.len() as u32;
    let mut sheet = Image::filled(CELL * 8, CELL * rows, 0xFF30_3030);
    for (r, (kind, list)) in by_kind.iter().enumerate() {
        println!("row {r}: {kind}: {} files", list.len());
        for (c, f) in list.iter().enumerate() {
            let img = crate::png::decode(&fs::read(f).unwrap()).unwrap();
            let (tw, th) = crate::ops::fit_size(img.width.max(1) * 16, img.height.max(1) * 16, CELL - 4, CELL - 4);
            let (tw, th) =
                if img.width < tw { (tw, th) } else { crate::ops::fit_size(img.width, img.height, CELL - 4, CELL - 4) };
            let filter = if img.width < tw { Filter::Nearest } else { Filter::Lanczos3 };
            let t = over_checkerboard(&crate::resize(&img, tw, th, filter));
            for y in 0..t.height {
                for x in 0..t.width {
                    sheet.set(c as u32 * CELL + 2 + x, r as u32 * CELL + 2 + y, t.get(x, y).unwrap());
                }
            }
        }
    }
    let png = crate::png::encode(&sheet, 6).unwrap();
    fs::write(out_dir().join("png_contact_sheet.png"), png).unwrap();
}

/// A contact sheet of assorted BMP files (alpha shown over a checkerboard).
#[test]
#[ignore]
fn bmp_contact_sheet() {
    let files = [
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CoreAI_cw5n1h2txyewy\DiscoveryOverlay\Assets\cursor-default-glow.bmp",
        r"C:\Windows\WinSxS\amd64_microsoft-windows-usertiles-client_31bf3856ad364e35_10.0.26100.1_none_26898bc4817b0485\user.bmp",
        r"C:\Program Files\Windows Media Player\Network Sharing\wmpnss_color48.bmp",
        r"C:\Program Files\qemu\share\icons\hicolor\32x32\apps\qemu.bmp",
        r"C:\Program Files\qemu\share\qemu-nsis.bmp",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\Extensions\Microsoft\VsGraphics\Assets\Images\mreTeapot.bmp",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\ItemTemplates\VC\ATL\ATLControl\1033\Toolbar.bmp",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\VC\VCProjectItems\Resource\bitmap.bmp",
    ];
    const CELL: u32 = 200;
    let mut sheet = Image::filled(CELL * 4, CELL * 2, 0xFF30_3030);
    for (i, f) in files.iter().enumerate() {
        let Ok(data) = fs::read(f) else { continue };
        let img = crate::decode(&data).unwrap();
        let (tw, th) = if img.width * 4 <= CELL {
            (img.width * 4, img.height * 4)
        } else {
            crate::ops::fit_size(img.width, img.height, CELL - 8, CELL - 8)
        };
        let filter = if tw > img.width { Filter::Nearest } else { Filter::Lanczos3 };
        let t = over_checkerboard(&crate::resize(&img, tw, th, filter));
        let (cx, cy) = ((i as u32 % 4) * CELL + 4, (i as u32 / 4) * CELL + 4);
        for y in 0..t.height {
            for x in 0..t.width {
                sheet.set(cx + x, cy + y, t.get(x, y).unwrap());
            }
        }
    }
    fs::write(out_dir().join("bmp_contact_sheet.png"), crate::png::encode(&sheet, 6).unwrap()).unwrap();
}

fn enc_dir() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/enc");
    let _ = fs::create_dir_all(&p);
    p
}

/// The image used for the encoder interoperability checks: a real photo at an odd size, plus
/// a variant with a smooth alpha gradient.
fn encoder_sample() -> Option<(Image, Image)> {
    let data = fs::read(r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg").ok()?;
    let photo = crate::resize(&crate::decode(&data).ok()?, 803, 501, Filter::Lanczos3);
    let alpha = Image::from_fn(photo.width, photo.height, |x, y| {
        let a = ((x * 255) / photo.width).min(255);
        let p = photo.get(x, y).unwrap();
        if (x / 40 + y / 40) % 7 == 0 { p & 0x00FF_FFFF } else { a << 24 | (p & 0x00FF_FFFF) }
    });
    Some((photo, alpha))
}

/// Writes files produced by our encoders to `target/agents/vimage/enc/`; the Windows decoder
/// then decodes them (`gdiplus_encoded.ps1`) for `compare_encoder_output_with_windows`.
#[test]
#[ignore]
fn write_encoder_samples() {
    use crate::jpeg::{EncodeOptions, Subsampling};
    let Some((photo, alpha)) = encoder_sample() else { return };
    let dir = enc_dir();
    let d = EncodeOptions::default();
    let jpegs: [(&str, EncodeOptions, (usize, usize)); 10] = [
        ("j420", d, (2, 2)),
        ("j422", EncodeOptions { subsampling: Subsampling::Yuv422, ..d }, (2, 1)),
        ("j444", EncodeOptions { subsampling: Subsampling::Yuv444, ..d }, (1, 1)),
        ("j440", d, (1, 2)),
        ("j411", d, (4, 1)),
        ("jprog", EncodeOptions { progressive: true, ..d }, (2, 2)),
        ("jprog444restart", EncodeOptions { progressive: true, restart_interval: 5, ..d }, (1, 1)),
        ("jgray", EncodeOptions { grayscale: true, ..d }, (1, 1)),
        ("jrestart_opt", EncodeOptions { restart_interval: 7, optimize_huffman: true, ..d }, (2, 2)),
        ("jq30", EncodeOptions { quality: 30, ..d }, (2, 2)),
    ];
    for (name, opts, (hs, vs)) in jpegs {
        let file = crate::jpeg::encode_sampled(&photo, &opts, hs, vs).unwrap();
        fs::write(dir.join(format!("{name}.jpg")), &file).unwrap();
    }
    fs::write(dir.join("prgb.png"), crate::png::encode(&photo, 6).unwrap()).unwrap();
    fs::write(dir.join("prgba.png"), crate::png::encode(&alpha, 9).unwrap()).unwrap();
    fs::write(dir.join("pstored.png"), crate::png::encode(&photo, 0).unwrap()).unwrap();
    fs::write(dir.join("brgba.bmp"), crate::bmp::encode(&alpha).unwrap()).unwrap();
    fs::write(dir.join("brgb.bmp"), crate::bmp::encode(&photo).unwrap()).unwrap();
    println!("wrote encoder samples to {}", dir.display());
}

/// Compares the Windows decoder's view of our encoders' output with ours: lossless formats must
/// reproduce the source exactly, JPEGs must decode (nearly) identically.
#[test]
#[ignore]
fn compare_encoder_output_with_windows() {
    let Some((photo, alpha)) = encoder_sample() else { return };
    let dir = enc_dir();
    let refs = dir.join("ref");
    let Ok(entries) = fs::read_dir(&refs) else {
        println!("no references; run gdiplus_encoded.ps1 first");
        return;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let r = e.path();
        let name = file_stem(&r);
        let Some(src) =
            fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).find(|p| file_stem(p) == name && p.is_file())
        else {
            continue;
        };
        let raw = fs::read(&r).unwrap();
        let (w, h) = (
            u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
        );
        let px: Vec<u32> =
            raw[8..].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let theirs = Image::from_pixels(w, h, px).unwrap();
        let ours = crate::decode(&fs::read(&src).unwrap()).unwrap();
        let p = crate::jpeg::tests::psnr(&ours, &theirs);
        let max_diff = ours
            .pixels
            .iter()
            .zip(&theirs.pixels)
            .flat_map(|(&a, &b)| [0, 8, 16, 24].map(|s| (((a >> s) & 0xFF) as i32 - ((b >> s) & 0xFF) as i32).abs()))
            .max()
            .unwrap_or(0);
        println!("{name:<16} Windows vs ours: PSNR {p:6.2} dB, max channel difference {max_diff}");
        match src.extension().and_then(|x| x.to_str()) {
            Some("png") | Some("bmp") => {
                let source = if name.contains("rgba") { &alpha } else { &photo };
                assert_eq!(&ours, source, "{name}: our decoder");
                if name.contains("rgba") {
                    // GDI+ round-trips through premultiplied alpha: compare alpha exactly and
                    // premultiplied color within rounding.
                    let mut worst = 0;
                    for (&a, &b) in ours.pixels.iter().zip(&theirs.pixels) {
                        assert_eq!(a >> 24, b >> 24, "{name}: alpha differs");
                        let (pa, pb) = (crate::premultiply_pixel(a), crate::premultiply_pixel(b));
                        for s in [0, 8, 16] {
                            worst = worst.max((((pa >> s) & 0xFF) as i32 - ((pb >> s) & 0xFF) as i32).abs());
                        }
                    }
                    println!("{name:<16} premultiplied max difference {worst}");
                    assert!(worst <= 1, "{name}: {worst}");
                } else {
                    assert_eq!(max_diff, 0, "{name}: Windows decodes our file differently");
                }
            }
            _ => assert!(p > 38.0, "{name}: {p:.2} dB"),
        }
        n += 1;
    }
    println!("{n} files compared");
}
