//! The terminal: `isatty`, its size and its modes (`termios`).
//!
//! A program run by the Terminal gets the terminal's shared state
//! (`vproto::tty`) at startup; its descriptors that are the terminal's
//! socket endpoint are the terminal.

use alloc::sync::Arc;

use vproto::tty::{self, Tty};
use vrt::object::Vmo;
use vrt::sync::Mutex;

use crate::fd::{self, Object};
use crate::linux::errno::{EINVAL, ENOTTY};
use crate::linux::{Termios, Winsize, ioctl};
use crate::{process, user};

static TTY: Mutex<Option<Arc<Tty>>> = Mutex::new(None);

pub fn adopt(vmo: Vmo) {
    *TTY.lock() = Tty::open(vmo).map(Arc::new);
}

pub fn get() -> Option<Arc<Tty>> {
    TTY.lock().clone()
}

/// The kernel object id of the terminal's socket endpoint, if there is a
/// terminal.
pub fn socket_koid() -> Option<u64> {
    TTY.lock().as_ref().map(|t| t.socket())
}

/// How a read from the terminal ends (its `termios`).
pub enum Input {
    /// Canonical mode: at the end of a line.
    Lines,
    /// Non-canonical: once `min` bytes are there, or `time` tenths of a
    /// second pass between bytes (from the start, when `min` is 0).
    Bytes { min: u8, time: u8 },
}

pub fn input() -> Input {
    match TTY.lock().as_ref().map(|t| t.termios()) {
        Some(t) if t.lflag & tty::lflag::ICANON == 0 => {
            Input::Bytes { min: t.cc[tty::cc::VMIN], time: t.cc[tty::cc::VTIME] }
        }
        _ => Input::Lines,
    }
}

/// A duplicate of the terminal's state for a child process.
pub fn vmo_for_child() -> Option<Vmo> {
    let t = get()?;
    t.vmo().0.duplicate(None).ok().map(Vmo::from_handle)
}

/// Terminal `ioctl`s on `fd`.
///
/// # Safety
/// `arg` must point at what the request takes.
pub unsafe fn ioctl(fd: i32, request: u32, arg: usize) -> Result<usize, isize> {
    let desc = fd::get(fd)?;
    let Object::Stream(s) = &desc.object else { return Err(ENOTTY) };
    if !s.is_tty() {
        return Err(ENOTTY);
    }
    let Some(t) = get() else { return Err(ENOTTY) };
    match request {
        ioctl::TIOCGWINSZ => {
            let (rows, cols) = t.size();
            let (xpixel, ypixel) = t.pixels();
            // SAFETY: per the caller.
            unsafe { user::write(arg, Winsize { row: rows, col: cols, xpixel, ypixel })? };
            Ok(0)
        }
        ioctl::TIOCSWINSZ => {
            // SAFETY: per the caller.
            let w: Winsize = unsafe { user::read(arg)? };
            t.set_size(w.row, w.col);
            Ok(0)
        }
        ioctl::TCGETS => {
            let m = t.termios();
            let mut cc = [0u8; 19];
            cc.copy_from_slice(&m.cc);
            let k = Termios { iflag: m.iflag, oflag: m.oflag, cflag: m.cflag, lflag: m.lflag, line: 0, cc };
            // SAFETY: per the caller.
            unsafe { user::write(arg, k)? };
            Ok(0)
        }
        ioctl::TCSETS | ioctl::TCSETSW | ioctl::TCSETSF => {
            // SAFETY: per the caller.
            let k: Termios = unsafe { user::read(arg)? };
            t.set_termios(&tty::Termios { iflag: k.iflag, oflag: k.oflag, cflag: k.cflag, lflag: k.lflag, cc: k.cc });
            Ok(0)
        }
        // The terminal has one process group: the program's.
        ioctl::TIOCGPGRP => {
            // SAFETY: per the caller.
            unsafe { user::write(arg, process::pid())? };
            Ok(0)
        }
        ioctl::TIOCSPGRP | ioctl::TIOCSCTTY | ioctl::TIOCNOTTY => Ok(0),
        ioctl::TCSBRK | ioctl::TCXONC | ioctl::TCFLSH => Ok(0),
        _ => Err(EINVAL),
    }
}
