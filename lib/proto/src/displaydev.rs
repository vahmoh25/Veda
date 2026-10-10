//! The display driver protocol (service `"displaydev"`, provided by the
//! compositor).
//!
//! The compositor draws into the framebuffer the firmware left it, in
//! place, until a display driver attaches. A driver that can flip (show one
//! of several pictures, from the start of a vertical blank on) hands the
//! compositor pictures of its own: memory the display engine scans out
//! directly, in the framebuffer's size and pixel format. From then on the
//! compositor draws each frame into a picture that is not on the screen and
//! asks for it; the driver shows it from the next vertical blank. So a
//! frame is never seen half drawn (no tearing), and frames come at the
//! screen's own pace.
//!
//! The driver keeps the mode the firmware set and takes over the very
//! picture the compositor draws into: [`Screen::firmware`] says where that
//! is, and the compositor checks it against its framebuffer. A driver that
//! sets the mode itself (Linux's, in the driver VM) sets the compositor's
//! ([`displaydev::screen`]). The compositor accepts drivers only
//! (connections from `devmgr`, which starts them).
//!
//! # Flips
//!
//! Requests and their completions go through a shared page ([`FlipState`],
//! layout in [`state`]) and two events: `request` (compositor to driver: a
//! picture is queued) and `done` (driver to compositor: the queued picture
//! is on the screen). Each side writes only its own fields, the request's
//! or the completion's number last, and only one request is outstanding at
//! a time: the compositor queues the next once the last is done.
//!
//! The completion also says when the vertical blank it happened at began,
//! and how long the screen takes for a frame, so that the compositor can
//! time its animations for the moment a frame will be seen.
//!
//! If the driver goes away, the compositor keeps drawing into the pictures
//! (all of them, not knowing which one the screen shows); if the
//! compositor goes away, the driver shows the firmware's framebuffer again
//! and attaches to the next compositor.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vabi::Error;
use vipc::{enumeration, message, protocol};
use vrt::object::{Event, Vmo};
use vrt::vm::Mapping;

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DisplayDevError {
        /// Only drivers (started by `devmgr`) may attach.
        Denied = 1,
        /// A driver is attached already.
        Busy = 2,
        /// Not the screen the compositor draws on: another size, pixel
        /// format or framebuffer.
        Mismatch = 3,
        /// Too few pictures, or ones too small or that cannot be mapped.
        BadPictures = 4,
    }
}

impl core::fmt::Display for DisplayDevError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DisplayDevError::Denied => "only drivers may attach",
            DisplayDevError::Busy => "a driver is attached already",
            DisplayDevError::Mismatch => "not the screen the compositor draws on",
            DisplayDevError::BadPictures => "unusable pictures",
        })
    }
}

message! {
    /// The screen a driver took over.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Screen {
        /// The display hardware, for the log ("Intel Alder Lake-P").
        pub name: String,
        pub width: u32,
        pub height: u32,
        /// Bytes from one row of a picture to the next.
        pub stride: u32,
        /// Red in the low byte of each pixel (otherwise blue: `0xXXRRGGBB`).
        pub rgbx: bool,
        /// Where the picture on the screen was when the driver took it over,
        /// as the processor reaches it: the physical addresses the firmware
        /// may have given as its framebuffer's (through the graphics
        /// aperture, or the memory itself).
        pub firmware: Vec<u64>,
        /// The time between two vertical blanks (ns; 0 if not known).
        pub period_ns: u64,
    }
}

message! {
    /// The screen the compositor draws on.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ScreenMode {
        pub width: u32,
        pub height: u32,
        /// Red in the low byte of each pixel (otherwise blue).
        pub rgbx: bool,
        /// Where the firmware's framebuffer is (physical address; 0: none):
        /// the display to drive is the one whose memory it is in.
        pub framebuffer: u64,
    }
}

message! {
    /// A driver's pictures and the means to flip between them.
    #[derive(Debug)]
    pub struct Link {
        /// At least two, each at least `stride * height` bytes:
        /// write-combining memory the display engine reads directly.
        pub pictures: Vec<Vmo>,
        /// The shared page ([`state`]).
        pub state: Vmo,
        /// Compositor to driver: a picture was queued.
        pub request: Event,
        /// Driver to compositor: the queued picture is on the screen.
        pub done: Event,
    }
}

protocol! {
    /// Display drivers attach the screens they can flip to the compositor.
    pub mod displaydev = "displaydev" {
        /// Hands the compositor the screen's pictures: it draws into them
        /// from its next frame on, and asks for one as soon as it holds the
        /// whole screen. Attached until the connection closes.
        1 => fn attach(screen: Screen, link: Link) -> Result<(), DisplayDevError>;
        /// The screen the compositor draws on: what a driver that sets the
        /// display's mode itself sets before it attaches.
        2 => fn screen() -> Result<ScreenMode, DisplayDevError>;
    }
}

/// The least number of pictures a driver hands over.
pub const MIN_PICTURES: usize = 2;
/// The most the compositor takes.
pub const MAX_PICTURES: usize = 3;

/// Layout of the shared page (all fields little-endian and naturally
/// aligned; the two sides' fields on separate cache lines).
pub mod state {
    /// Compositor: the picture to show next (u32), then the request's
    /// number (u32), counting from 1, written last.
    pub const QUEUED: usize = 0;
    pub const QUEUED_SEQ: usize = 4;
    /// Driver: the number of the last request now on the screen (u32),
    /// written last.
    pub const SHOWN_SEQ: usize = 64;
    /// Driver: the screen's frame counter at the vertical blank it was
    /// shown from (u32).
    pub const FRAMES: usize = 68;
    /// Driver: when that vertical blank began (u64, monotonic ns).
    pub const BLANK_NS: usize = 72;
    /// Driver: the time between two vertical blanks (u64 ns; 0 if not
    /// known).
    pub const PERIOD_NS: usize = 80;
    pub const SIZE: usize = 4096;
}

/// A completed flip, as the driver reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shown {
    /// The request's number (0: none yet).
    pub seq: u32,
    /// The screen's frame counter at the vertical blank it happened at.
    pub frames: u32,
    /// When that blank began (monotonic ns).
    pub blank_ns: u64,
    /// The time between two blanks (ns; 0 if not known).
    pub period_ns: u64,
}

/// The shared page, mapped. Either side uses it: the compositor queues and
/// reads completions, the driver reads requests and reports completions.
pub struct FlipState {
    map: Mapping,
}

impl FlipState {
    /// A new page (the driver's); also returns a handle for the peer.
    pub fn create() -> Result<(FlipState, Vmo), Error> {
        let vmo = Vmo::create(state::SIZE)?;
        let peer = Vmo::from_handle(vmo.0.duplicate(None)?);
        Ok((FlipState::map(vmo)?, peer))
    }

    /// Maps a page the peer created.
    pub fn map(vmo: Vmo) -> Result<FlipState, Error> {
        if vmo.size()? < state::SIZE {
            return Err(Error::InvalidArgs);
        }
        let map = Mapping::new(vmo, state::SIZE, vabi::map_flags::READ | vabi::map_flags::WRITE)?;
        Ok(FlipState { map })
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        // SAFETY: the offsets in `state` are inside the page and aligned.
        unsafe { &*(self.map.as_ptr().add(off) as *const AtomicU32) }
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        // SAFETY: as above.
        unsafe { &*(self.map.as_ptr().add(off) as *const AtomicU64) }
    }

    /// Compositor: asks for `picture` as request `seq`.
    pub fn queue(&self, picture: u32, seq: u32) {
        self.u32_at(state::QUEUED).store(picture, Ordering::Relaxed);
        self.u32_at(state::QUEUED_SEQ).store(seq, Ordering::Release);
    }

    /// Driver: the last request, `(picture, seq)`.
    pub fn queued(&self) -> (u32, u32) {
        let seq = self.u32_at(state::QUEUED_SEQ).load(Ordering::Acquire);
        (self.u32_at(state::QUEUED).load(Ordering::Relaxed), seq)
    }

    /// Driver: reports a completed request.
    pub fn set_shown(&self, shown: Shown) {
        self.u32_at(state::FRAMES).store(shown.frames, Ordering::Relaxed);
        self.u64_at(state::BLANK_NS).store(shown.blank_ns, Ordering::Relaxed);
        self.u64_at(state::PERIOD_NS).store(shown.period_ns, Ordering::Relaxed);
        self.u32_at(state::SHOWN_SEQ).store(shown.seq, Ordering::Release);
    }

    /// Compositor: the last completed request.
    pub fn shown(&self) -> Shown {
        let seq = self.u32_at(state::SHOWN_SEQ).load(Ordering::Acquire);
        Shown {
            seq,
            frames: self.u32_at(state::FRAMES).load(Ordering::Relaxed),
            blank_ns: self.u64_at(state::BLANK_NS).load(Ordering::Relaxed),
            period_ns: self.u64_at(state::PERIOD_NS).load(Ordering::Relaxed),
        }
    }
}
