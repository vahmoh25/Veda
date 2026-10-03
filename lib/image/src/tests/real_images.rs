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
    if files.is_empty() {
        println!("skipped: no Windows sample images found");
        return;
    }
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
    if files.is_empty() {
        println!("skipped: no Windows PNG files found");
        return;
    }
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
    if files.is_empty() {
        println!("skipped: no BMP files found");
    }
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
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\Assets\Images\copilot-upsell.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\DesktopSpotlight\Assets\Images\image_1.jpg",
        r"C:\Windows\Web\touchkeyboard\TouchKeyboardThemeLight002.jpg",
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
    if files.is_empty() {
        println!("skipped: no Windows PNG files found");
        return;
    }
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

/// PowerShell script that decodes every file listed in `-List` with the Windows decoder
/// (GDI+/WIC) and writes `<index>.argb` files to `-Out`: width and height (u32 LE), then
/// straight-alpha BGRA pixels.
const WINDOWS_DECODE_PS1: &str = r#"
param([string]$List, [string]$Out)
Add-Type -AssemblyName System.Drawing
$i = 0
foreach ($f in Get-Content $List) {
  $img = [System.Drawing.Image]::FromFile($f)
  $bmp = New-Object System.Drawing.Bitmap($img.Width, $img.Height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
  $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
  $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
  $g.DrawImage($img, 0, 0, $img.Width, $img.Height)
  $g.Dispose()
  $rect = New-Object System.Drawing.Rectangle(0, 0, $bmp.Width, $bmp.Height)
  $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $bytes = New-Object byte[] ($data.Stride * $bmp.Height)
  [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
  $bmp.UnlockBits($data)
  $ms = New-Object System.IO.MemoryStream
  $ms.Write([BitConverter]::GetBytes([uint32]$bmp.Width), 0, 4)
  $ms.Write([BitConverter]::GetBytes([uint32]$bmp.Height), 0, 4)
  $ms.Write($bytes, 0, $bytes.Length)
  [IO.File]::WriteAllBytes((Join-Path $Out "$i.argb"), $ms.ToArray())
  $bmp.Dispose(); $img.Dispose()
  $i++
}
"#;

/// Decodes `files` with the Windows decoder by running [`WINDOWS_DECODE_PS1`]. Returns `None`
/// when PowerShell is not available (or fails).
fn windows_decode(files: &[PathBuf]) -> Option<Vec<Image>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/windows_ref");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).ok()?;
    let script = dir.join("decode.ps1");
    let list = dir.join("list.txt");
    fs::write(&script, WINDOWS_DECODE_PS1).ok()?;
    let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
    fs::write(&list, names.join("\r\n")).ok()?;
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-List")
        .arg(&list)
        .arg("-Out")
        .arg(&dir)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let mut images = Vec::new();
    for i in 0..files.len() {
        let raw = fs::read(dir.join(format!("{i}.argb"))).ok()?;
        let w = u32::from_le_bytes(raw.get(0..4)?.try_into().ok()?);
        let h = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?);
        let px = raw[8..].as_chunks::<4>().0.iter().map(|&c| u32::from_le_bytes(c)).collect();
        images.push(Image::from_pixels(w, h, px).ok()?);
    }
    Some(images)
}

/// Largest per-channel difference over the given channel shifts.
fn max_channel_diff(a: &Image, b: &Image, shifts: &[u32]) -> i32 {
    let mut worst = 0;
    for (&p, &q) in a.pixels.iter().zip(&b.pixels) {
        for &s in shifts {
            worst = worst.max((((p >> s) & 0xFF) as i32 - ((q >> s) & 0xFF) as i32).abs());
        }
    }
    worst
}

/// Compares our decoders with the Windows decoder (GDI+/WIC) on real files: baseline and
/// progressive JPEGs with 4:4:4 and 4:2:0 sampling, restart markers, and BMPs.
#[test]
#[ignore]
fn compare_with_windows_decoder() {
    let files: Vec<PathBuf> = [
        r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg",
        r"C:\Windows\Web\Wallpaper\ThemeA\img22.jpg",
        r"C:\Windows\Web\Wallpaper\ThemeB\img25.jpg",
        r"C:\Windows\Web\Wallpaper\Spotlight\img50.jpg",
        r"C:\Windows\Web\Screen\img105.jpg",
        r"C:\Windows\Web\touchkeyboard\TouchKeyboardThemeDark000.jpg",
        r"C:\Windows\Web\touchkeyboard\TouchKeyboardThemeLight002.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\DesktopSpotlight\Assets\Images\image_1.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\DesktopSpotlight\Assets\Images\image_2.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\Assets\Images\copilot-upsell.jpg",
        r"C:\Windows\SystemApps\MicrosoftWindows.Client.FileExp_cw5n1h2txyewy\FileExplorerExtensions\Assets\images\contrast-black\GalleryColdStateLeft.jpg",
        r"C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\ItemTemplates\VC\ATL\ATLControl\1033\Toolbar.bmp",
        r"C:\Program Files\qemu\share\qemu-nsis.bmp",
    ]
    .iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect();
    if files.is_empty() {
        println!("skipped: no Windows sample images found");
        return;
    }
    let Some(theirs) = windows_decode(&files) else {
        println!("Windows decoder (PowerShell/GDI+) not available");
        return;
    };
    let no_rotate = DecodeOptions { apply_orientation: false, ..DecodeOptions::DEFAULT };
    for (f, reference) in files.iter().zip(&theirs) {
        let ours = match crate::decode_with(&fs::read(f).unwrap(), &no_rotate) {
            Ok(i) => i,
            Err(ImageError::Unsupported(m)) => {
                println!("{}: unsupported ({m})", f.display());
                continue;
            }
            Err(e) => panic!("{}: {e}", f.display()),
        };
        assert_eq!((ours.width, ours.height), (reference.width, reference.height), "{}", f.display());
        let p = crate::jpeg::tests::psnr(&ours, reference);
        let diff = max_channel_diff(&ours, reference, &[0, 8, 16]);
        println!("{:<28} PSNR vs Windows {p:6.2} dB, max channel difference {diff}", file_stem(f));
        assert!(p > 35.0, "{}: {p:.2} dB", f.display());
    }
    println!("{} files compared", files.len());
}

fn enc_dir() -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/agents/vimage/enc");
    let _ = fs::create_dir_all(&p);
    p
}

/// The image used for the encoder interoperability checks: a real photo at an odd size, plus
/// a variant with a smooth alpha gradient and fully transparent squares.
fn encoder_sample() -> Option<(Image, Image)> {
    let data = fs::read(r"C:\Windows\Web\4K\Wallpaper\Windows\img0_1920x1200.jpg").ok()?;
    let photo = crate::resize(&crate::decode(&data).ok()?, 803, 501, Filter::Lanczos3);
    let alpha = Image::from_fn(photo.width, photo.height, |x, y| {
        let a = ((x * 255) / photo.width).min(255);
        let p = photo.get(x, y).unwrap();
        if (x / 40 + y / 40) % 7 == 0 { p & 0x00FF_FFFF } else { (a << 24) | (p & 0x00FF_FFFF) }
    });
    Some((photo, alpha))
}

/// Encodes a real photo with every encoder variant (written to `target/agents/vimage/enc/` for
/// inspection), has the Windows decoder decode the files, and compares: lossless formats must
/// reproduce the source exactly and JPEGs must decode identically to our decoder (except 4:1:1
/// and CMYK, which differ by design, see below).
#[test]
#[ignore]
fn compare_encoder_output_with_windows() {
    use crate::jpeg::{EncodeOptions, Subsampling};
    let Some((photo, alpha)) = encoder_sample() else {
        println!("skipped: the Windows sample wallpaper is missing");
        return;
    };
    let dir = enc_dir();
    let d = EncodeOptions::default();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
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
        files.push((format!("{name}.jpg"), crate::jpeg::encode_sampled(&photo, &opts, hs, vs).unwrap()));
    }
    // Adobe RGB and CMYK files (all components full resolution).
    let (w, h) = (photo.width as usize, photo.height as usize);
    let ch: [Vec<u8>; 3] = [16, 8, 0].map(|s| photo.pixels.iter().map(|&p| (p >> s) as u8).collect());
    let k: Vec<u8> = (0..w * h).map(|i| 255 - ((i % w) * 160 / w) as u8).collect();
    let q95 = EncodeOptions { quality: 95, ..d };
    let adobe = |planes: &[&[u8]], t| crate::jpeg::encode_planes(planes, w, h, Some(t), &q95, false).unwrap();
    files.push(("jadobergb.jpg".into(), adobe(&[&ch[0], &ch[1], &ch[2]], 0)));
    files.push(("jcmyk.jpg".into(), adobe(&[&ch[0], &ch[1], &ch[2], &k], 0)));
    files.push(("prgb.png".into(), crate::png::encode(&photo, 6).unwrap()));
    files.push(("prgba.png".into(), crate::png::encode(&alpha, 9).unwrap()));
    files.push(("pstored.png".into(), crate::png::encode(&photo, 0).unwrap()));
    files.push(("brgba.bmp".into(), crate::bmp::encode(&alpha).unwrap()));
    files.push(("brgb.bmp".into(), crate::bmp::encode(&photo).unwrap()));
    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(name, data)| {
            let p = dir.join(name);
            fs::write(&p, data).unwrap();
            p
        })
        .collect();
    let Some(theirs) = windows_decode(&paths) else {
        println!("Windows decoder (PowerShell/GDI+) not available");
        return;
    };
    for ((name, data), reference) in files.iter().zip(&theirs) {
        let ours = crate::decode(data).unwrap();
        let p = crate::jpeg::tests::psnr(&ours, reference);
        let diff = max_channel_diff(&ours, reference, &[0, 8, 16, 24]);
        println!("{name:<20} Windows vs ours: PSNR {p:6.2} dB, max channel difference {diff}");
        if name.ends_with(".png") || name.ends_with(".bmp") {
            let source = if name.contains("rgba") { &alpha } else { &photo };
            assert_eq!(&ours, source, "{name}: our decoder");
            if name.contains("rgba") {
                // GDI+ round-trips through premultiplied alpha: compare alpha exactly and the
                // premultiplied color within rounding.
                let mut worst = 0;
                for (&a, &b) in ours.pixels.iter().zip(&reference.pixels) {
                    assert_eq!(a >> 24, b >> 24, "{name}: alpha differs");
                    let (pa, pb) = (crate::premultiply_pixel(a), crate::premultiply_pixel(b));
                    for s in [0, 8, 16] {
                        worst = worst.max((((pa >> s) & 0xFF) as i32 - ((pb >> s) & 0xFF) as i32).abs());
                    }
                }
                println!("{name:<20} premultiplied max difference {worst}");
                assert!(worst <= 1, "{name}: {worst}");
            } else {
                assert_eq!(diff, 0, "{name}: Windows decodes our file differently");
            }
        } else if name.contains("cmyk") {
            // Windows converts CMYK with a color-managed (print) model; we use the standard
            // naive inverted-CMYK formula. Only report.
        } else if name.contains("411") {
            // Windows replicates 4x-subsampled chroma; we interpolate it.
            assert!(p > 38.0, "{name}: {p:.2} dB");
        } else {
            assert_eq!(diff, 0, "{name}: {p:.2} dB");
        }
    }
    println!("{} files compared", files.len());
}
