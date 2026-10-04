//! `assetgen` — generates the Veda wallpapers and sample pictures at build time.
//!
//! ```text
//! assetgen OUT_DIR [FILTER]
//! ```
//!
//! Writes `OUT_DIR/wallpapers/*` (installed as `/system/wallpapers`) and
//! `OUT_DIR/samples/Pictures/*` (copied to `~/Pictures` at boot). Everything is procedural and
//! deterministic. Only these two directories are touched (other generators share `OUT_DIR`).
//! The optional `FILTER` renders only files whose name contains it (handy while tuning).

mod noise;
mod paint;
mod photos;
mod wallpapers;

use std::path::{Path, PathBuf};
use std::time::Instant;

use paint::Buf;
use vimage::jpeg::{EncodeOptions, Subsampling};

/// How a generated image is stored.
#[derive(Clone, Copy)]
enum Store {
    /// Baseline JPEG at the given quality.
    Jpeg(u8, Subsampling),
    /// Progressive JPEG.
    ProgressiveJpeg(u8),
    /// JPEG stored rotated by 90 degrees counter-clockwise with EXIF orientation 6, so that
    /// viewers that honour EXIF show it upright.
    RotatedJpeg(u8),
    Png,
    Bmp,
    Qoi,
}

struct Item {
    dir: &'static str,
    name: &'static str,
    w: usize,
    h: usize,
    render: fn(usize, usize) -> Buf,
    store: Store,
}

const WALLPAPERS: &str = "wallpapers";
const PICTURES: &str = "samples/Pictures";

fn items() -> Vec<Item> {
    let wp = |name, render: fn(usize, usize) -> Buf| Item {
        dir: WALLPAPERS,
        name,
        w: 1920,
        h: 1200,
        render,
        store: Store::Jpeg(90, Subsampling::Yuv420),
    };
    let pic = |name, w, h, render, store| Item { dir: PICTURES, name, w, h, render, store };
    vec![
        // The default wallpaper is the first file name in byte order.
        wp("Aurora.jpg", wallpapers::aurora),
        wp("Dunes.jpg", wallpapers::dunes),
        wp("Nebula.jpg", wallpapers::nebula),
        wp("Pastel.jpg", wallpapers::pastel),
        wp("Ridges.jpg", wallpapers::ridges),
        pic("Alpine Lake.jpg", 2400, 1600, photos::alpine_lake, Store::Jpeg(90, Subsampling::Yuv420)),
        pic("City Lights.jpg", 1920, 1280, photos::city_lights, Store::ProgressiveJpeg(90)),
        pic("Lighthouse.jpg", 1200, 1600, photos::lighthouse, Store::RotatedJpeg(90)),
        pic("Soap Film.jpg", 1024, 1024, photos::soap_film, Store::Jpeg(92, Subsampling::Yuv444)),
        pic("Still Life.qoi", 960, 720, photos::still_life, Store::Qoi),
        pic("Glass Orb.png", 900, 900, photos::glass_orb, Store::Png),
        pic("Retro Sunset.bmp", 720, 450, photos::retro_sunset, Store::Bmp),
        pic("Misty Forest.jpg", 640, 480, photos::misty_forest, Store::Jpeg(90, Subsampling::Yuv420)),
    ]
}

fn encode(buf: &Buf, store: Store, seed: u32) -> Vec<u8> {
    let img = buf.to_image(seed);
    let jpeg = |img: &vimage::Image, quality, subsampling, progressive| {
        let opts =
            EncodeOptions { quality, subsampling, optimize_huffman: true, progressive, ..EncodeOptions::default() };
        vimage::jpeg::encode(img, &opts).expect("JPEG encoding")
    };
    match store {
        Store::Jpeg(q, s) => jpeg(&img, q, s, false),
        Store::ProgressiveJpeg(q) => jpeg(&img, q, Subsampling::Yuv420, true),
        Store::RotatedJpeg(q) => {
            let stored = vimage::rotate270(&img);
            paint::with_exif_orientation(&jpeg(&stored, q, Subsampling::Yuv420, false), 6)
        }
        Store::Png => vimage::png::encode(&img, 6).expect("PNG encoding"),
        Store::Bmp => paint::bmp24(&img),
        Store::Qoi => vimage::qoi::encode(&img).expect("QOI encoding"),
    }
}

/// Removes the files of a directory this tool owns (not its subdirectories).
fn clean(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_file() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(out) = args.next().map(PathBuf::from) else {
        eprintln!("usage: assetgen OUT_DIR [FILTER]");
        std::process::exit(2);
    };
    let filter = args.next();
    let started = Instant::now();
    if filter.is_none() {
        for dir in [WALLPAPERS, PICTURES] {
            clean(&out.join(dir));
        }
    }
    let mut total = 0usize;
    for (i, item) in items().into_iter().enumerate() {
        if filter.as_deref().is_some_and(|f| !item.name.contains(f)) {
            continue;
        }
        let t = Instant::now();
        let buf = (item.render)(item.w, item.h);
        let bytes = encode(&buf, item.store, 0x5EED + i as u32);
        // Every file must decode with the system's own decoder.
        let decoded = vimage::decode(&bytes).expect("generated image decodes");
        assert_eq!((decoded.width as usize, decoded.height as usize), (item.w, item.h), "{}", item.name);
        let dir = out.join(item.dir);
        std::fs::create_dir_all(&dir).expect("creating the output directory");
        std::fs::write(dir.join(item.name), &bytes).expect("writing the output file");
        // While tuning, `ASSETGEN_PREVIEW=DIR` also writes every image as PNG (as displayed).
        if let Some(preview) = std::env::var_os("ASSETGEN_PREVIEW") {
            let preview = PathBuf::from(preview);
            std::fs::create_dir_all(&preview).expect("creating the preview directory");
            let png = vimage::png::encode(&decoded, 1).expect("PNG encoding");
            std::fs::write(preview.join(format!("{}.png", item.name)), png).expect("writing the preview");
        }
        total += bytes.len();
        println!(
            "  {:<34} {:>4}x{:<4} {:>8} KiB  {:>5.0} ms",
            format!("{}/{}", item.dir, item.name),
            item.w,
            item.h,
            bytes.len() / 1024,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    println!("assetgen: {:.1} MiB in {:.1} s", total as f64 / (1024.0 * 1024.0), started.elapsed().as_secs_f64());
}
