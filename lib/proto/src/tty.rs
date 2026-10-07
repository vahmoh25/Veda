//! The terminal programs run in.
//!
//! A terminal (the Terminal application) runs a program with its standard
//! streams on one end of a socket and keeps the other end: what the program
//! writes comes out at the terminal, and the keys typed go in (a line at a
//! time after the terminal's line editing, in canonical mode). Several
//! programs share the terminal's socket endpoint at once, as processes
//! share a terminal device on Unix.
//!
//! The terminal and its programs also share a page, a VMO they are given
//! with role `TERMINAL`, holding the [`TtyState`]: the terminal keeps its
//! size up to date there, and programs set its modes there (canonical
//! input, echo, signal keys, output processing) through `termios`, which
//! the terminal follows. A program's descriptor is the terminal when it is
//! a socket endpoint whose kernel object id is [`TtyState::socket`].
//!
//! The page is shared with every program in the terminal, so neither side
//! trusts what the other wrote there: values are only read through the
//! accessors, which keep them in range.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use vabi::map_flags;
use vrt::object::Vmo;
use vrt::vm::Mapping;

/// `"VTTY"`
pub const MAGIC: u32 = u32::from_le_bytes(*b"VTTY");
/// Size of the shared page.
pub const SIZE: usize = 4096;

/// `termios` input flags.
pub mod iflag {
    pub const ICRNL: u32 = 0o400;
    pub const IXON: u32 = 0o2000;
    pub const IUTF8: u32 = 0o40000;
}

/// `termios` output flags.
pub mod oflag {
    /// Process output (the other output flags apply).
    pub const OPOST: u32 = 0o1;
    /// Newline means carriage return and newline.
    pub const ONLCR: u32 = 0o4;
}

/// `termios` control flags.
pub mod cflag {
    pub const B38400: u32 = 0o17;
    pub const CS8: u32 = 0o60;
    pub const CREAD: u32 = 0o200;
}

/// `termios` local flags.
pub mod lflag {
    /// The interrupt keys (Ctrl+C) send signals.
    pub const ISIG: u32 = 0o1;
    /// Canonical input: a line at a time, edited by the terminal.
    pub const ICANON: u32 = 0o2;
    /// The terminal echoes what is typed.
    pub const ECHO: u32 = 0o10;
    pub const ECHOE: u32 = 0o20;
    pub const ECHOK: u32 = 0o40;
    pub const ECHOCTL: u32 = 0o1000;
    pub const ECHOKE: u32 = 0o4000;
    pub const IEXTEN: u32 = 0o100000;
}

/// Indices of the control characters.
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VREPRINT: usize = 12;
    pub const VDISCARD: usize = 13;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    /// Control characters.
    pub const NCCS: usize = 19;
}

/// The terminal modes (the kernel's `struct termios` on Linux, without the
/// line discipline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub cc: [u8; cc::NCCS],
}

impl Default for Termios {
    /// What a terminal starts with (as `stty sane`).
    fn default() -> Termios {
        let mut c = [0u8; cc::NCCS];
        c[cc::VINTR] = 0x03;
        c[cc::VQUIT] = 0x1c;
        c[cc::VERASE] = 0x7f;
        c[cc::VKILL] = 0x15;
        c[cc::VEOF] = 0x04;
        c[cc::VMIN] = 1;
        c[cc::VSTART] = 0x11;
        c[cc::VSTOP] = 0x13;
        c[cc::VSUSP] = 0x1a;
        c[cc::VREPRINT] = 0x12;
        c[cc::VDISCARD] = 0x0f;
        c[cc::VWERASE] = 0x17;
        c[cc::VLNEXT] = 0x16;
        Termios {
            iflag: iflag::ICRNL | iflag::IXON | iflag::IUTF8,
            oflag: oflag::OPOST | oflag::ONLCR,
            cflag: cflag::B38400 | cflag::CS8 | cflag::CREAD,
            lflag: lflag::ISIG
                | lflag::ICANON
                | lflag::ECHO
                | lflag::ECHOE
                | lflag::ECHOK
                | lflag::ECHOCTL
                | lflag::ECHOKE
                | lflag::IEXTEN,
            cc: c,
        }
    }
}

/// The shared page.
#[repr(C)]
pub struct TtyState {
    magic: AtomicU32,
    /// Columns (high 16 bits) and rows (low 16 bits).
    size: AtomicU32,
    /// The kernel object id of the socket endpoint the programs hold.
    socket: AtomicU64,
    iflag: AtomicU32,
    oflag: AtomicU32,
    cflag: AtomicU32,
    lflag: AtomicU32,
    /// The control characters, four to a word.
    cc: [AtomicU32; cc::NCCS.div_ceil(4)],
    /// Pixel size of the text area: width (high 16 bits), height (low).
    pixels: AtomicU32,
}

const _: () = assert!(core::mem::size_of::<TtyState>() <= SIZE);

/// A terminal's shared state, mapped into this process.
pub struct Tty {
    map: Mapping,
}

impl Tty {
    /// Creates the state of a new terminal whose programs hold the socket
    /// endpoint `socket` (a kernel object id).
    pub fn create(socket: u64, rows: u16, cols: u16) -> Option<Tty> {
        let map = Mapping::anonymous(SIZE).ok()?;
        let tty = Tty { map };
        let s = tty.state();
        s.socket.store(socket, Ordering::Relaxed);
        tty.set_termios(&Termios::default());
        tty.set_size(rows, cols);
        s.magic.store(MAGIC, Ordering::Release);
        Some(tty)
    }

    /// Maps a terminal's state from the VMO a program was given.
    pub fn open(vmo: Vmo) -> Option<Tty> {
        if vmo.size().ok()? < SIZE {
            return None;
        }
        let map = Mapping::new(vmo, SIZE, map_flags::READ | map_flags::WRITE).ok()?;
        let tty = Tty { map };
        (tty.state().magic.load(Ordering::Acquire) == MAGIC).then_some(tty)
    }

    fn state(&self) -> &TtyState {
        // SAFETY: the mapping is page aligned, at least SIZE bytes long and
        // lives as long as `self`; every field is an atomic, valid for any
        // bit pattern, and accessed only atomically.
        unsafe { &*(self.map.addr() as *const TtyState) }
    }

    /// The VMO, to pass to programs.
    pub fn vmo(&self) -> &Vmo {
        self.map.vmo()
    }

    /// The kernel object id of the socket endpoint programs hold.
    pub fn socket(&self) -> u64 {
        self.state().socket.load(Ordering::Relaxed)
    }

    /// Makes `socket` (a kernel object id) the terminal's endpoint.
    pub fn set_socket(&self, socket: u64) {
        self.state().socket.store(socket, Ordering::Relaxed);
    }

    /// Rows and columns (at least 1 each).
    pub fn size(&self) -> (u16, u16) {
        let v = self.state().size.load(Ordering::Relaxed);
        ((v as u16).max(1), ((v >> 16) as u16).max(1))
    }

    pub fn set_size(&self, rows: u16, cols: u16) {
        self.state().size.store((cols as u32) << 16 | rows as u32, Ordering::Relaxed);
    }

    /// Width and height of the text area in pixels (0 if unknown).
    pub fn pixels(&self) -> (u16, u16) {
        let v = self.state().pixels.load(Ordering::Relaxed);
        ((v >> 16) as u16, v as u16)
    }

    pub fn set_pixels(&self, width: u16, height: u16) {
        self.state().pixels.store((width as u32) << 16 | height as u32, Ordering::Relaxed);
    }

    pub fn termios(&self) -> Termios {
        let s = self.state();
        let mut cc = [0u8; cc::NCCS];
        for (i, c) in cc.iter_mut().enumerate() {
            *c = (s.cc[i / 4].load(Ordering::Relaxed) >> (8 * (i % 4))) as u8;
        }
        Termios {
            iflag: s.iflag.load(Ordering::Relaxed),
            oflag: s.oflag.load(Ordering::Relaxed),
            cflag: s.cflag.load(Ordering::Relaxed),
            lflag: s.lflag.load(Ordering::Relaxed),
            cc,
        }
    }

    pub fn set_termios(&self, t: &Termios) {
        let s = self.state();
        s.iflag.store(t.iflag, Ordering::Relaxed);
        s.oflag.store(t.oflag, Ordering::Relaxed);
        s.cflag.store(t.cflag, Ordering::Relaxed);
        s.lflag.store(t.lflag, Ordering::Relaxed);
        for (w, chunk) in s.cc.iter().zip(t.cc.chunks(4)) {
            let mut v = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                v |= (c as u32) << (8 * i);
            }
            w.store(v, Ordering::Relaxed);
        }
    }

    /// Local flags (`lflag`): what the terminal does with typed keys.
    pub fn lflag(&self) -> u32 {
        self.state().lflag.load(Ordering::Relaxed)
    }

    /// Output flags (`oflag`).
    pub fn oflag(&self) -> u32 {
        self.state().oflag.load(Ordering::Relaxed)
    }
}
