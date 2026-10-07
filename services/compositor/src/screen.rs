//! The screen: where composed frames go.
//!
//! The compositor composes into a back buffer and copies what changed to
//! the screen. At first, and on machines without a display driver that
//! can flip, that is the framebuffer the firmware left, written in place:
//! frames are paced by a timer, and the screen may show one half written.
//! Once a driver attaches (`vproto::displaydev`), frames go into its
//! pictures instead, which it shows in turn from the start of a vertical
//! blank: each frame is written into a picture that is not on the screen,
//! then asked for. So the screen only ever shows whole frames, and the next
//! frame is composed once the last is on the screen: at the screen's own
//! pace. Animations are timed for the moment their frames will be seen.
//!
//! Only what changed is written, so each picture keeps a list of what
//! changed while frames went into the others ("stale"). Before a frame goes
//! into a picture, that is brought up to date from what the screen is to
//! show: the back buffer, or the startup sequence's layer.
//!
//! If the driver goes away, or stops answering, frames go into all of its
//! pictures and into the firmware's framebuffer, in place: whichever of
//! them the screen shows, it shows the frames.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use vgfx::{Bitmap, Damage, Rect};
use vproto::displaydev::{self as dd, DisplayDevError, FlipState, Link, Shown};
use vrt::object::Event;
use vrt::println;
use vrt::vm::Mapping;

/// Shortest time between two composed frames without flips (60 Hz).
pub(crate) const FRAME_NS: u64 = 16_666_666;
/// A flip the driver has not carried out after this long: it stopped
/// answering.
const FLIP_TIMEOUT_NS: u64 = 1_000_000_000;

/// Memory the screen may show: rows of pixels `pitch` bytes apart.
struct Surface {
    map: Mapping,
    pitch: usize,
    /// What changed while frames went elsewhere.
    stale: Damage,
}

impl Surface {
    /// The pixels of row `y` across `r`.
    fn row(&mut self, y: i32, r: Rect) -> &mut [u32] {
        // SAFETY: the mapping covers `pitch * height` bytes, and callers
        // keep `r` on the screen.
        unsafe {
            core::slice::from_raw_parts_mut(
                (self.map.as_ptr().add(y as usize * self.pitch) as *mut u32).add(r.x as usize),
                r.w as usize,
            )
        }
    }

    /// Copies `r` of `from` (as large as the screen) into the surface.
    fn copy(&mut self, from: &Bitmap, r: Rect, rgb: bool) {
        let w = from.width;
        for y in r.y..r.bottom() {
            let src = &from.pixels[(y * w + r.x) as usize..(y * w + r.right()) as usize];
            let dst = self.row(y, r);
            if rgb {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = swap_red_blue(s);
                }
            } else {
                dst.copy_from_slice(src);
            }
        }
    }
}

/// A pixel for a framebuffer that stores red in the low byte.
fn swap_red_blue(s: u32) -> u32 {
    (s & 0xFF00_FF00) | ((s >> 16) & 0xFF) | ((s & 0xFF) << 16)
}

/// A display driver's pictures, and the flips between them.
struct Flips {
    /// The driver's connection, and its name.
    key: u64,
    name: String,
    pictures: Vec<Surface>,
    state: FlipState,
    request: Event,
    done: Event,
    /// The picture on the screen (`None`: still the firmware's).
    shown: Option<usize>,
    /// The picture the frame being composed goes into.
    target: usize,
    /// The flip asked for: the picture, the request's number, and when.
    pending: Option<(usize, u32, u64)>,
    seq: u32,
    /// The last frame was asked for (one that changed nothing was not).
    flipped: bool,
    /// The last flip carried out, with the screen's timing then.
    last: Shown,
    /// Flips carried out, and the screen's frame counter at the first.
    count: u64,
    first_frames: Option<u32>,
    /// Gone, or stopped answering: frames go everywhere, in place.
    lost: bool,
}

/// The surfaces a frame goes into.
fn targets<'a>(firmware: &'a mut Surface, flips: &'a mut Option<Flips>) -> Vec<&'a mut Surface> {
    let Some(f) = flips else { return vec![firmware] };
    if f.lost {
        core::iter::once(firmware).chain(f.pictures.iter_mut()).collect()
    } else {
        vec![&mut f.pictures[f.target]]
    }
}

/// Frames a second, to two decimals, of a screen whose frames take
/// `period` ns.
fn rate(period: u64) -> String {
    let millihertz = 1_000_000_000_000 / period.max(1);
    format!("{}.{:02}", millihertz / 1000, millihertz % 1000 / 10)
}

/// The screen and the back buffer frames are composed into.
pub(crate) struct Screen {
    pub(crate) back: Bitmap,
    /// Red in the low byte of each pixel.
    rgb: bool,
    firmware: Surface,
    flips: Option<Flips>,
    /// What the frame being composed wrote.
    written: Damage,
    /// The time the last frame was composed for.
    clock: u64,
}

impl Screen {
    /// The screen on the firmware's framebuffer (`pitch` bytes a row).
    pub(crate) fn new(fb: Mapping, pitch: usize, rgb: bool, width: i32, height: i32) -> Screen {
        Screen {
            back: Bitmap::new(width, height),
            rgb,
            firmware: Surface { map: fb, pitch, stale: Damage::new() },
            flips: None,
            written: Damage::new(),
            clock: 0,
        }
    }

    pub(crate) fn rect(&self) -> Rect {
        self.back.rect()
    }

    // ---- frames ------------------------------------------------------------

    /// When a frame composed at `now` will be seen: at the first vertical
    /// blank at least a quarter of a frame away, when a driver flips and
    /// keeps time; `now` otherwise. Never before the last frame's.
    pub(crate) fn frame_time(&mut self, now: u64) -> u64 {
        let t = match &self.flips {
            Some(f) if !f.lost && f.last.period_ns > 0 && f.last.blank_ns > 0 => {
                let (period, blank) = (f.last.period_ns, f.last.blank_ns);
                blank + (now + period / 4).saturating_sub(blank).div_ceil(period).max(1) * period
            }
            _ => now,
        };
        self.clock = self.clock.max(t);
        self.clock
    }

    /// Starts a frame: chooses where it goes, and brings that up to date
    /// where it is stale from what the screen shows (`layer` during the
    /// startup sequence, the back buffer otherwise).
    pub(crate) fn begin_frame(&mut self, layer: Option<&Bitmap>) {
        self.written.clear();
        let Screen { back, rgb, firmware, flips, .. } = self;
        if let Some(f) = flips
            && !f.lost
        {
            f.target = f.shown.map_or(0, |s| (s + 1) % f.pictures.len());
        }
        let source: &Bitmap = layer.unwrap_or(back);
        for t in targets(firmware, flips) {
            for r in t.stale.take() {
                t.copy(source, r, *rgb);
            }
        }
    }

    /// Copies a region of the back buffer to the screen.
    pub(crate) fn flush(&mut self, r: Rect) {
        let r = r.intersect(&self.rect());
        if r.is_empty() {
            return;
        }
        let Screen { back, rgb, firmware, flips, written, .. } = self;
        for t in targets(firmware, flips) {
            t.copy(back, r, *rgb);
        }
        written.add(r);
    }

    /// Copies a region of `layer` (as large as the screen) to the screen,
    /// in place of the back buffer.
    pub(crate) fn flush_from(&mut self, layer: &Bitmap, r: Rect) {
        let r = r.intersect(&self.rect());
        if r.is_empty() {
            return;
        }
        let Screen { rgb, firmware, flips, written, .. } = self;
        for t in targets(firmware, flips) {
            t.copy(layer, r, *rgb);
        }
        written.add(r);
    }

    /// Shows the back buffer through `layer` (as large as the screen) in
    /// `r`: `alpha` parts of 256 of the back buffer.
    pub(crate) fn flush_mixed(&mut self, layer: &Bitmap, alpha: u32, r: Rect) {
        let r = r.intersect(&self.rect());
        if r.is_empty() {
            return;
        }
        let Screen { back, rgb, firmware, flips, written, .. } = self;
        let (w, t) = (back.width, alpha.min(256));
        for target in targets(firmware, flips) {
            for y in r.y..r.bottom() {
                let span = (y * w + r.x) as usize..(y * w + r.right()) as usize;
                let (over, under) = (&layer.pixels[span.clone()], &back.pixels[span]);
                for ((d, &a), &b) in target.row(y, r).iter_mut().zip(over).zip(under) {
                    // Red and blue together, then green.
                    let rb = (((a & 0xFF_00FF) * (256 - t) + (b & 0xFF_00FF) * t) >> 8) & 0xFF_00FF;
                    let g = (((a & 0x00_FF00) * (256 - t) + (b & 0x00_FF00) * t) >> 8) & 0x00_FF00;
                    let s = 0xFF00_0000 | rb | g;
                    *d = if *rgb { swap_red_blue(s) } else { s };
                }
            }
        }
        written.add(r);
    }

    /// Ends a frame composed at `now`: what it wrote is stale in the
    /// pictures it did not go into, and the one it went into is asked for
    /// (if the frame changed something, or the screen still shows the
    /// firmware's picture).
    pub(crate) fn end_frame(&mut self, now: u64) {
        let Screen { flips, written, .. } = self;
        let Some(f) = flips.as_mut().filter(|f| !f.lost) else { return };
        for (i, p) in f.pictures.iter_mut().enumerate() {
            if i != f.target {
                for &r in written.rects() {
                    p.stale.add(r);
                }
            }
        }
        f.flipped = !written.is_empty() || f.shown.is_none();
        if f.flipped {
            // What was written reaches memory, out of the processor's
            // write-combining buffers, before the driver shows it.
            fence(Ordering::SeqCst);
            f.seq = f.seq.wrapping_add(1).max(1);
            f.state.queue(f.target as u32, f.seq);
            let _ = f.request.signal();
            f.pending = Some((f.target, f.seq, now));
        }
    }

    /// Whether a frame is due although nothing changed: the driver's first
    /// picture is to be shown, or, after it went away, surfaces are to be
    /// brought up to date.
    pub(crate) fn needs_frame(&self) -> bool {
        match &self.flips {
            None => false,
            Some(f) if f.lost => !self.firmware.stale.is_empty() || f.pictures.iter().any(|p| !p.stale.is_empty()),
            Some(f) => f.shown.is_none() && f.pending.is_none(),
        }
    }

    /// When the next frame may be composed, the last at `last`: while a
    /// flip waits, once it is carried out; right away after a flip; a frame
    /// after the last otherwise.
    pub(crate) fn next_frame(&self, last: u64) -> u64 {
        match &self.flips {
            Some(f) if !f.lost && f.pending.is_some() => vabi::DEADLINE_INFINITE,
            Some(f) if !f.lost && f.flipped => last,
            _ => last + FRAME_NS,
        }
    }

    // ---- the display driver ------------------------------------------------

    /// A display driver on connection `key` hands its pictures over; the
    /// firmware's framebuffer is at `framebuffer` (physical address).
    pub(crate) fn attach(
        &mut self,
        key: u64,
        screen: dd::Screen,
        link: Link,
        framebuffer: u64,
    ) -> Result<(), DisplayDevError> {
        if self.flips.is_some() {
            return Err(DisplayDevError::Busy);
        }
        let (width, height) = (self.back.width as u32, self.back.height as u32);
        let pitch = screen.stride as usize;
        let mismatch = if (screen.width, screen.height) != (width, height) {
            Some(format!("its picture is {}x{}, the screen {}x{}", screen.width, screen.height, width, height))
        } else if screen.rgbx != self.rgb {
            Some(String::from("its pixels hold their colours in another order"))
        } else if pitch < width as usize * 4 {
            Some(format!("{} bytes a row are too few", pitch))
        } else if framebuffer == 0 || !screen.firmware.contains(&framebuffer) {
            let at: Vec<String> = screen.firmware.iter().map(|a| format!("{:#x}", a)).collect();
            Some(format!("its picture is at {}, the firmware's framebuffer at {:#x}", at.join(" or "), framebuffer))
        } else {
            None
        };
        if let Some(why) = mismatch {
            println!("{} cannot have the screen: {}", screen.name, why);
            return Err(DisplayDevError::Mismatch);
        }
        if !(dd::MIN_PICTURES..=dd::MAX_PICTURES).contains(&link.pictures.len()) {
            return Err(DisplayDevError::BadPictures);
        }
        let bytes = (pitch * height as usize).next_multiple_of(4096);
        let mut pictures = Vec::new();
        for vmo in link.pictures {
            if vmo.size().map_or(true, |size| size < bytes) {
                return Err(DisplayDevError::BadPictures);
            }
            let map = Mapping::new(vmo, bytes, vabi::map_flags::READ | vabi::map_flags::WRITE)
                .map_err(|_| DisplayDevError::BadPictures)?;
            let mut stale = Damage::new();
            stale.add(self.rect());
            pictures.push(Surface { map, pitch, stale });
        }
        let state = FlipState::map(link.state).map_err(|_| DisplayDevError::BadPictures)?;
        println!(
            "{} attached: {}x{} at {} frames a second, {} pictures; frames are flipped at the vertical blank",
            screen.name,
            width,
            height,
            if screen.period_ns > 0 { rate(screen.period_ns) } else { String::from("an unknown number of") },
            pictures.len()
        );
        self.flips = Some(Flips {
            key,
            name: screen.name,
            pictures,
            state,
            request: link.request,
            done: link.done,
            shown: None,
            target: 0,
            pending: None,
            seq: 0,
            flipped: false,
            last: Shown { period_ns: screen.period_ns, ..Shown::default() },
            count: 0,
            first_frames: None,
            lost: false,
        });
        Ok(())
    }

    /// The event the driver signals when a flip waits and is carried out.
    pub(crate) fn done_event(&self) -> Option<vabi::RawHandle> {
        self.flips.as_ref().filter(|f| !f.lost && f.pending.is_some()).map(|f| f.done.raw())
    }

    /// The driver signalled: the picture asked for may be on the screen.
    pub(crate) fn flip_done(&mut self) {
        let Some(f) = self.flips.as_mut() else { return };
        let _ = f.done.clear();
        let shown = f.state.shown();
        if let Some((picture, seq, _)) = f.pending
            && shown.seq == seq
        {
            f.pending = None;
            f.shown = Some(picture);
            f.count += 1;
            f.first_frames.get_or_insert(shown.frames);
            f.last = Shown { period_ns: if shown.period_ns > 0 { shown.period_ns } else { f.last.period_ns }, ..shown };
        }
    }

    /// When the waiting flip times out, if one waits.
    pub(crate) fn timeout(&self) -> Option<u64> {
        let f = self.flips.as_ref().filter(|f| !f.lost)?;
        f.pending.map(|(_, _, since)| since + FLIP_TIMEOUT_NS)
    }

    /// Gives up on a driver that does not carry its flips out.
    pub(crate) fn check(&mut self, now: u64) {
        if self.timeout().is_some_and(|t| now >= t) {
            let name = self.flips.as_ref().map(|f| f.name.clone()).unwrap_or_default();
            self.lose(&format!("{} stopped answering", name));
        }
    }

    /// Connection `key` closed: if it was the driver's, it went away.
    pub(crate) fn detach(&mut self, key: u64) {
        if let Some(f) = &self.flips
            && f.key == key
            && !f.lost
        {
            let why = format!("{} went away", f.name);
            self.lose(&why);
        }
    }

    /// Frames go into every picture and the firmware's framebuffer from now
    /// on, all of them to be brought up to date first.
    fn lose(&mut self, why: &str) {
        println!("{}: frames go into its pictures and the firmware's framebuffer, in place", why);
        let all = self.rect();
        self.firmware.stale.add(all);
        if let Some(f) = &mut self.flips {
            f.lost = true;
            f.pending = None;
            for p in &mut f.pictures {
                p.stale.add(all);
            }
        }
    }

    /// Where frames go, for the log.
    pub(crate) fn describe(&self) -> String {
        match &self.flips {
            None => String::from("drawn into the firmware's framebuffer"),
            Some(f) if f.lost => format!("drawn in place: {} went away", f.name),
            Some(f) => {
                let blanks = f.first_frames.map_or(0, |first| f.last.frames.wrapping_sub(first) as u64 + 1);
                format!(
                    "flipped by {} at {} frames a second: {} flips over {} vertical blanks",
                    f.name,
                    rate(f.last.period_ns),
                    f.count,
                    blanks
                )
            }
        }
    }
}
