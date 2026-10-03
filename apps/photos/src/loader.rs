//! Reading and decoding pictures on background threads.
//!
//! Two workers, each with its own VFS connection: one decodes full pictures for the viewer (the
//! picture on screen is always decoded before any prefetching), the other makes thumbnails for
//! the library and the filmstrip. Results come back through a mutex and an [`Event`] that the
//! UI waits on (see `App::wait_handles`), so decoding never blocks drawing.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vgfx::Bitmap;
use vimage::{Filter, Format, Image, ImageError, Orientation};
use vproto::fs::FsError;
use vproto::vfs;
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};

#[allow(unused_imports)]
use vmath::FloatExt;

/// Files larger than this are not opened.
const MAX_FILE: u64 = 128 << 20;
/// Size of a library card thumbnail.
pub const CARD_W: u32 = 216;
pub const CARD_H: u32 = 144;
/// Size of a filmstrip thumbnail.
pub const STRIP_W: u32 = 76;
pub const STRIP_H: u32 = 52;
/// The low-resolution preview (shown while the full picture loads) fits in this box.
const PREVIEW_W: u32 = 400;
const PREVIEW_H: u32 = 300;
/// Thumbnails are cropped from a copy of the picture that fits in this square.
const BASE_SIZE: u32 = 640;
/// The mip chain stops at this size.
const SMALLEST_LEVEL: i32 = 96;

/// Facts about a picture file, from its headers.
#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub format: Format,
    /// Size as displayed (after the EXIF orientation).
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    pub orientation: Orientation,
    /// Progressive JPEG or interlaced PNG.
    pub progressive: bool,
    pub file_size: u64,
}

/// A decoded picture, ready for drawing.
pub struct Picture {
    /// Mip chain of premultiplied bitmaps: the full picture first, then halves.
    pub levels: Vec<Bitmap>,
    /// Some pixels are not opaque (drawn over a checkerboard).
    pub has_alpha: bool,
    /// The file failed its checksums and was decoded without verifying them.
    pub damaged: bool,
}

impl Picture {
    pub fn width(&self) -> u32 {
        self.levels[0].width as u32
    }

    pub fn height(&self) -> u32 {
        self.levels[0].height as u32
    }
}

/// Thumbnails of one picture (premultiplied bitmaps).
pub struct Thumbs {
    /// Library card: `CARD_W` x `CARD_H`, cropped to fill, with rounded corners.
    pub card: Bitmap,
    /// Filmstrip: `STRIP_W` x `STRIP_H`, cropped to fill, with rounded corners.
    pub strip: Bitmap,
    /// The whole picture at low resolution.
    pub preview: Bitmap,
    pub has_alpha: bool,
}

/// A finished job.
pub enum Done {
    Picture { path: String, result: Result<Arc<Picture>, String>, meta: Option<Meta> },
    Thumbs { path: String, result: Result<Thumbs, String>, meta: Option<Meta> },
}

struct Shared {
    pictures: Mutex<VecDeque<String>>,
    pictures_wake: Condvar,
    thumbs: Mutex<VecDeque<String>>,
    thumbs_wake: Condvar,
    done: Mutex<Vec<Done>>,
    /// The picture the decoder is working on.
    active: Mutex<Option<String>>,
}

/// The UI side of the decoding threads.
pub struct Loader {
    shared: Arc<Shared>,
    event: Event,
}

impl Loader {
    /// Starts the two worker threads.
    pub fn start() -> Option<Loader> {
        let event = Event::create().ok()?;
        let shared = Arc::new(Shared {
            pictures: Mutex::new(VecDeque::new()),
            pictures_wake: Condvar::new(),
            thumbs: Mutex::new(VecDeque::new()),
            thumbs_wake: Condvar::new(),
            done: Mutex::new(Vec::new()),
            active: Mutex::new(None),
        });
        // The picture on screen matters more than thumbnails; both yield to the UI thread.
        let workers =
            [("picture-decoder", false, vabi::priority::NORMAL - 1), ("thumbnailer", true, vabi::priority::NORMAL - 2)];
        for (name, thumbs, priority) in workers {
            let s = shared.clone();
            let ev = Event::from_handle(event.0.duplicate(None).ok()?);
            vrt::thread::Builder::new()
                .name(name)
                .stack_size(512 * 1024)
                .priority(priority)
                .spawn(move || worker(s, ev, thumbs))
                .ok()?;
        }
        Some(Loader { shared, event })
    }

    /// Asks for the full picture at `path`. An `urgent` request (the picture on screen) is
    /// decoded next; others wait behind the queued requests.
    pub fn request_picture(&self, path: &str, urgent: bool) {
        let mut q = self.shared.pictures.lock();
        if self.shared.active.lock().as_deref() == Some(path) {
            return;
        }
        if let Some(i) = q.iter().position(|p| p == path) {
            if urgent && i > 0 {
                let p = q.remove(i).unwrap_or_default();
                q.push_front(p);
            }
            return;
        }
        if urgent {
            q.push_front(path.to_string());
        } else {
            q.push_back(path.to_string());
        }
        drop(q);
        self.shared.pictures_wake.notify_one();
    }

    /// Drops queued picture requests for paths not in `keep`.
    pub fn retain_pictures(&self, keep: &[String]) {
        self.shared.pictures.lock().retain(|p| keep.contains(p));
    }

    /// Asks for the thumbnails of `path`; `first` puts the request at the head of the queue.
    pub fn request_thumbs(&self, path: &str, first: bool) {
        let mut q = self.shared.thumbs.lock();
        if let Some(i) = q.iter().position(|p| p == path) {
            if !first {
                return;
            }
            q.remove(i);
        }
        if first {
            q.push_front(path.to_string());
        } else {
            q.push_back(path.to_string());
        }
        drop(q);
        self.shared.thumbs_wake.notify_one();
    }

    /// Forgets thumbnail requests that have not started yet; returns their paths.
    pub fn cancel_thumbs(&self) -> Vec<String> {
        self.shared.thumbs.lock().drain(..).collect()
    }

    /// Finished jobs since the last call.
    pub fn take(&self) -> Vec<Done> {
        let _ = self.event.clear();
        core::mem::take(&mut *self.shared.done.lock())
    }

    /// Signalled when jobs have finished.
    pub fn event_handle(&self) -> RawHandle {
        self.event.raw()
    }
}

fn worker(shared: Arc<Shared>, event: Event, thumbs: bool) {
    let (queue, wake) =
        if thumbs { (&shared.thumbs, &shared.thumbs_wake) } else { (&shared.pictures, &shared.pictures_wake) };
    let mut vfs: Option<vfs::Client> = None;
    loop {
        let path = {
            let mut q = queue.lock();
            loop {
                if let Some(p) = q.pop_front() {
                    if !thumbs {
                        // Set under the queue lock, so that a request always finds the path in
                        // one of the two.
                        *shared.active.lock() = Some(p.clone());
                    }
                    break p;
                }
                q = wake.wait(q);
            }
        };
        if vfs.is_none() {
            vfs = vproto::connect(vfs::NAME).ok().map(vfs::Client::new);
        }
        let data = match &vfs {
            Some(client) => read_file(client, &path),
            None => Err(Failure::Service),
        };
        if matches!(data, Err(Failure::Service)) {
            // Reconnect for the next job.
            vfs = None;
        }
        let done = match data {
            Err(e) => {
                let e = e.to_string();
                if thumbs {
                    Done::Thumbs { path, result: Err(e), meta: None }
                } else {
                    Done::Picture { path, result: Err(e), meta: None }
                }
            }
            Ok(data) => {
                let meta = vimage::read_info(&data).ok().map(|i| Meta {
                    format: i.format,
                    width: i.width,
                    height: i.height,
                    has_alpha: i.has_alpha,
                    orientation: i.orientation,
                    progressive: i.progressive,
                    file_size: data.len() as u64,
                });
                let started = vrt::time::now_ns();
                let decoded = decode(&data);
                let decode_ms = (vrt::time::now_ns() - started) / 1_000_000;
                drop(data);
                let name = path.rsplit('/').next().unwrap_or("");
                if thumbs {
                    let result = decoded.map(|(img, _)| make_thumbs(&img));
                    let total_ms = (vrt::time::now_ns() - started) / 1_000_000;
                    match &result {
                        Ok(_) => vrt::println!("thumbnail of {name}: decoded in {decode_ms} ms, {total_ms} ms in all"),
                        Err(e) => vrt::println!("no thumbnail for {name}: {e}"),
                    }
                    Done::Thumbs { path, result, meta }
                } else {
                    let result = decoded.map(|(img, damaged)| Arc::new(make_picture(img, damaged)));
                    let total_ms = (vrt::time::now_ns() - started) / 1_000_000;
                    match &result {
                        Ok(p) => vrt::println!(
                            "opened {name} ({}x{}): decoded in {decode_ms} ms, ready in {total_ms} ms",
                            p.width(),
                            p.height()
                        ),
                        Err(e) => vrt::println!("cannot open {name}: {e}"),
                    }
                    Done::Picture { path, result, meta }
                }
            }
        };
        shared.done.lock().push(done);
        if !thumbs {
            *shared.active.lock() = None;
        }
        let _ = event.signal();
    }
}

/// Why a file could not be read.
enum Failure {
    Fs(FsError),
    Service,
    TooLarge,
    NoMemory,
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Failure::Fs(FsError::NotFound) => f.write_str("The file no longer exists."),
            Failure::Fs(e) => write!(f, "The file could not be read ({e})."),
            Failure::Service => f.write_str("The file system is not responding."),
            Failure::TooLarge => f.write_str("The file is too large to open."),
            Failure::NoMemory => f.write_str("There is not enough memory to open this picture."),
        }
    }
}

fn read_file(vfs: &vfs::Client, path: &str) -> Result<Vec<u8>, Failure> {
    let (vmo, len) = match vfs.read_file(path.into()) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(Failure::Fs(e)),
        Err(_) => return Err(Failure::Service),
    };
    if len > MAX_FILE {
        return Err(Failure::TooLarge);
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(len as usize).map_err(|_| Failure::NoMemory)?;
    buf.resize(len as usize, 0);
    vmo.read(0, &mut buf).map_err(|_| Failure::Service)?;
    Ok(buf)
}

/// Decodes a picture. Files whose only fault is a checksum (common with PNGs written by tools
/// that do not update CRCs) are decoded without verifying checksums; the flag reports that.
fn decode(data: &[u8]) -> Result<(Image, bool), String> {
    vimage::decode_lenient(data).map_err(describe)
}

/// A decoding error in words for the user.
fn describe(e: ImageError) -> String {
    match e {
        ImageError::UnknownFormat => "This file is not a picture Photos can open.".into(),
        ImageError::Truncated => "The file is incomplete: it ends before the picture does.".into(),
        ImageError::Invalid(why) => format!("The file is damaged ({why})."),
        ImageError::Unsupported(what) => format!("This picture uses a feature Photos does not support ({what})."),
        ImageError::TooLarge { width, height } => format!("The picture is too large ({width} × {height} pixels)."),
        ImageError::ChecksumMismatch(_) => "The file is damaged (checksum mismatch).".into(),
        ImageError::OutOfMemory => "There is not enough memory to open this picture.".into(),
        e => format!("The picture could not be decoded ({e})."),
    }
}

/// Builds the mip chain of a decoded picture.
fn make_picture(img: Image, damaged: bool) -> Picture {
    let has_alpha = img.pixels.iter().any(|p| p >> 24 != 0xFF);
    let mut levels = alloc::vec![Bitmap::from_straight(img.width as i32, img.height as i32, img.pixels)];
    loop {
        let last = &levels[levels.len() - 1];
        if last.width.max(last.height) <= SMALLEST_LEVEL || last.width < 2 || last.height < 2 {
            break;
        }
        let next = half(last);
        levels.push(next);
    }
    Picture { levels, has_alpha, damaged }
}

/// Halves a premultiplied bitmap with a 2x2 box filter (an odd last row or column is dropped).
fn half(b: &Bitmap) -> Bitmap {
    let (w, h) = (b.width / 2, b.height / 2);
    let mut out = Bitmap::new(w, h);
    let sw = b.width as usize;
    for y in 0..h as usize {
        let r0 = &b.pixels[2 * y * sw..2 * y * sw + 2 * w as usize];
        let r1 = &b.pixels[(2 * y + 1) * sw..(2 * y + 1) * sw + 2 * w as usize];
        let dst = &mut out.pixels[y * w as usize..(y + 1) * w as usize];
        for (x, d) in dst.iter_mut().enumerate() {
            let (a, b, c, e) = (r0[2 * x], r0[2 * x + 1], r1[2 * x], r1[2 * x + 1]);
            // Two channels per lane: every sum of four 8-bit values fits in 10 bits.
            let rb = ((a & 0x00FF_00FF) + (b & 0x00FF_00FF) + (c & 0x00FF_00FF) + (e & 0x00FF_00FF) + 0x0002_0002) >> 2;
            let ag = (((a >> 8) & 0x00FF_00FF)
                + ((b >> 8) & 0x00FF_00FF)
                + ((c >> 8) & 0x00FF_00FF)
                + ((e >> 8) & 0x00FF_00FF)
                + 0x0002_0002)
                >> 2;
            *d = (rb & 0x00FF_00FF) | ((ag & 0x00FF_00FF) << 8);
        }
    }
    out
}

fn make_thumbs(img: &Image) -> Thumbs {
    let has_alpha = img.pixels.iter().any(|p| p >> 24 != 0xFF);
    // An intermediate size with enough pixels for the crops of any aspect ratio.
    let base = vimage::thumbnail(img, BASE_SIZE, BASE_SIZE, Filter::Bilinear);
    let card = finish(cover(&base, CARD_W, CARD_H), has_alpha, 10.0);
    let strip = finish(cover(&base, STRIP_W, STRIP_H), has_alpha, 6.0);
    let preview = vimage::thumbnail(&base, PREVIEW_W, PREVIEW_H, Filter::Bilinear);
    let preview = Bitmap::from_straight(preview.width as i32, preview.height as i32, preview.pixels);
    Thumbs { card, strip, preview, has_alpha }
}

/// Crops the centre of `img` to the aspect ratio of `w` x `h` and scales it to that size.
fn cover(img: &Image, w: u32, h: u32) -> Image {
    let (iw, ih) = (img.width.max(1), img.height.max(1));
    let (cw, ch) = if iw as u64 * h as u64 > ih as u64 * w as u64 {
        (((ih as u64 * w as u64) / h as u64).max(1) as u32, ih)
    } else {
        (iw, ((iw as u64 * h as u64) / w as u64).max(1) as u32)
    };
    let cropped = vimage::crop(img, (iw - cw) / 2, (ih - ch) / 2, cw, ch);
    vimage::resize(&cropped, w, h, Filter::Bilinear)
}

/// Puts a transparent thumbnail on a checkerboard and rounds its corners; returns it
/// premultiplied.
fn finish(mut img: Image, has_alpha: bool, radius: f32) -> Bitmap {
    let (w, h) = (img.width as usize, img.height as usize);
    if has_alpha {
        for (i, p) in img.pixels.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let bg: u32 = if ((x / 6) ^ (y / 6)) & 1 == 0 { 0x46_464E } else { 0x38_383F };
            let a = *p >> 24;
            let mix = |s: u32| -> u32 {
                let (f, b) = ((*p >> s) & 0xFF, (bg >> s) & 0xFF);
                (f * a + b * (255 - a) + 127) / 255
            };
            *p = 0xFF00_0000 | mix(16) << 16 | mix(8) << 8 | mix(0);
        }
    }
    round_corners(&mut img.pixels, w, h, radius);
    Bitmap::from_straight(w as i32, h as i32, img.pixels)
}

/// Fades the corners of a straight-alpha image outside a rounded rectangle (anti-aliased).
pub fn round_corners(pixels: &mut [u32], w: usize, h: usize, radius: f32) {
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
    let n = r.ceil() as usize;
    for y in 0..n.min(h) {
        for x in 0..n.min(w) {
            // Distance from the corner circle's centre, for the top-left corner.
            let (dx, dy) = (r - (x as f32 + 0.5), r - (y as f32 + 0.5));
            let d = (dx * dx + dy * dy).sqrt();
            let cover = (r - d + 0.5).clamp(0.0, 1.0);
            if cover >= 1.0 {
                continue;
            }
            for (cx, cy) in [(x, y), (w - 1 - x, y), (x, h - 1 - y), (w - 1 - x, h - 1 - y)] {
                let p = &mut pixels[cy * w + cx];
                let a = ((*p >> 24) as f32 * cover + 0.5) as u32;
                *p = (*p & 0x00FF_FFFF) | a << 24;
            }
        }
    }
}
