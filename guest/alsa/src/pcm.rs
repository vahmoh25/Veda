//! A PCM device of Linux's sound system through the kernel's own interface
//! (`/dev/snd/pcmC*D*p` and `...c`, and the ioctls of
//! `include/uapi/sound/asound.h`): configured for interleaved 16-bit
//! frames, written (playback) or read (capture) without blocking, and
//! asked how much is in its buffer.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use guest_sys::{POLLIN, POLLOUT, ioc, ioctl, poll};

const O_NONBLOCK: i32 = 0o4000;

/// Parameters of `snd_pcm_hw_params`: masks, then intervals.
mod param {
    pub const ACCESS: usize = 0;
    pub const FORMAT: usize = 1;
    pub const SUBFORMAT: usize = 2;
    pub const FIRST_INTERVAL: usize = 8;
    pub const CHANNELS: usize = 10;
    pub const RATE: usize = 11;
    pub const PERIOD_SIZE: usize = 13;
    pub const BUFFER_SIZE: usize = 17;
}

const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_S16_LE: u32 = 2;
const SUBFORMAT_STD: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct Mask {
    bits: [u32; 8],
}

/// `snd_interval`: the bits after `min` and `max` are `openmin`,
/// `openmax`, `integer`, `empty`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Interval {
    min: u32,
    max: u32,
    flags: u32,
}

const INTEGER: u32 = 1 << 2;

#[repr(C)]
struct HwParams {
    flags: u32,
    masks: [Mask; 3],
    reserved_masks: [Mask; 5],
    intervals: [Interval; 12],
    reserved_intervals: [Interval; 9],
    rmask: u32,
    cmask: u32,
    info: u32,
    msbits: u32,
    rate_num: u32,
    rate_den: u32,
    fifo_size: u64,
    sync: [u8; 16],
    reserved: [u8; 48],
}

const _: () = assert!(size_of::<HwParams>() == 608);

/// `snd_xferi`.
#[repr(C)]
struct Transfer {
    result: i64,
    buf: usize,
    frames: u64,
}

const HW_PARAMS: u32 = ioc(3, b'A', 0x11, size_of::<HwParams>());
const DELAY: u32 = ioc(2, b'A', 0x21, size_of::<i64>());
const PREPARE: u32 = ioc(0, b'A', 0x40, 0);
const START: u32 = ioc(0, b'A', 0x42, 0);
const DROP: u32 = ioc(0, b'A', 0x43, 0);
const WRITEI_FRAMES: u32 = ioc(1, b'A', 0x50, size_of::<Transfer>());
const READI_FRAMES: u32 = ioc(2, b'A', 0x51, size_of::<Transfer>());

/// Which way a device's frames go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Playback,
    Capture,
}

impl HwParams {
    /// Everything allowed, every parameter asked for.
    fn any() -> HwParams {
        let interval = Interval { min: 0, max: u32::MAX, flags: 0 };
        HwParams {
            flags: 0,
            masks: [Mask { bits: [u32::MAX; 8] }; 3],
            reserved_masks: [Mask { bits: [0; 8] }; 5],
            intervals: [interval; 12],
            reserved_intervals: [Interval { min: 0, max: 0, flags: 0 }; 9],
            rmask: u32::MAX,
            cmask: 0,
            info: u32::MAX,
            msbits: 0,
            rate_num: 0,
            rate_den: 0,
            fifo_size: 0,
            sync: [0; 16],
            reserved: [0; 48],
        }
    }

    fn only(&mut self, mask: usize, value: u32) {
        let mut m = Mask { bits: [0; 8] };
        m.bits[value as usize / 32] = 1 << (value % 32);
        self.masks[mask] = m;
    }

    fn within(&mut self, interval: usize, min: u32, max: u32) {
        self.intervals[interval - param::FIRST_INTERVAL] = Interval { min, max, flags: INTEGER };
    }

    fn get(&self, interval: usize) -> u32 {
        self.intervals[interval - param::FIRST_INTERVAL].min
    }
}

/// An open device.
pub struct Pcm {
    file: File,
    stream: Stream,
    pub rate: u32,
    pub channels: u32,
    /// Frames per period (between the card's interrupts), and of the
    /// card's whole buffer.
    pub period: u32,
    pub buffer: u32,
}

impl Pcm {
    /// Opens the device at `path` for interleaved 16-bit frames at `rate`,
    /// of as few of `channels` as it takes, with periods of `period` frames
    /// and a buffer of `periods` of them; failing that, the periods and
    /// buffer nearest that the card can do.
    pub fn open(
        path: &str,
        stream: Stream,
        rate: u32,
        channels: RangeInclusive<u32>,
        period: u32,
        periods: u32,
    ) -> io::Result<Pcm> {
        let mut options = OpenOptions::new();
        match stream {
            Stream::Playback => options.write(true),
            Stream::Capture => options.read(true),
        };
        let file = options.custom_flags(O_NONBLOCK).open(path)?;
        let fd = file.as_raw_fd();
        let configure = |exact: bool| -> io::Result<HwParams> {
            let mut p = HwParams::any();
            p.only(param::ACCESS, ACCESS_RW_INTERLEAVED);
            p.only(param::FORMAT, FORMAT_S16_LE);
            p.only(param::SUBFORMAT, SUBFORMAT_STD);
            p.within(param::CHANNELS, *channels.start(), *channels.end());
            p.within(param::RATE, rate, rate);
            if exact {
                p.within(param::PERIOD_SIZE, period, period);
                p.within(param::BUFFER_SIZE, period * periods, period * periods);
            } else {
                p.within(param::PERIOD_SIZE, period / 2, period * 2);
                p.within(param::BUFFER_SIZE, period * 4, period * periods * 2);
            }
            // SAFETY: the request's structure, which the kernel refines.
            unsafe { ioctl(fd, HW_PARAMS, &mut p as *mut HwParams as usize)? };
            Ok(p)
        };
        let p = configure(true).or_else(|_| configure(false))?;
        Ok(Pcm {
            file,
            stream,
            rate,
            channels: p.get(param::CHANNELS),
            period: p.get(param::PERIOD_SIZE),
            buffer: p.get(param::BUFFER_SIZE),
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn request(&self, request: u32) -> io::Result<()> {
        // SAFETY: a request that takes no argument.
        unsafe { ioctl(self.fd(), request, 0).map(|_| ()) }
    }

    /// Makes the device ready to play what is written next, or to record
    /// (after it was configured, ran dry or over, or was stopped).
    pub fn prepare(&self) -> io::Result<()> {
        self.request(PREPARE)
    }

    /// Starts a prepared device (a recording; playback starts with the
    /// first frames written).
    pub fn start(&self) -> io::Result<()> {
        self.request(START)
    }

    /// Stops at once, dropping what is queued.
    pub fn stop(&self) -> io::Result<()> {
        self.request(DROP)
    }

    /// Writes interleaved frames, without waiting: how many the device
    /// took (0 if it is full). `EPIPE` if it ran dry and stopped.
    pub fn write(&self, samples: &[i16]) -> io::Result<usize> {
        let mut t = Transfer {
            result: 0,
            buf: samples.as_ptr() as usize,
            frames: (samples.len() / self.channels as usize) as u64,
        };
        // SAFETY: the transfer names the samples, which the kernel reads.
        match unsafe { ioctl(self.fd(), WRITEI_FRAMES, &mut t as *mut Transfer as usize) } {
            Ok(_) => Ok(t.result as usize),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Reads the interleaved frames recorded, without waiting: how many
    /// came (0 if none). `EPIPE` if they ran over and recording stopped.
    pub fn read(&self, samples: &mut [i16]) -> io::Result<usize> {
        let mut t = Transfer {
            result: 0,
            buf: samples.as_mut_ptr() as usize,
            frames: (samples.len() / self.channels as usize) as u64,
        };
        // SAFETY: the transfer names the buffer, which the kernel writes.
        match unsafe { ioctl(self.fd(), READI_FRAMES, &mut t as *mut Transfer as usize) } {
            Ok(_) => Ok(t.result as usize),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Playback: frames between the last one written and the one heard
    /// now. Capture: frames recorded and not read yet.
    pub fn delay(&self) -> io::Result<u64> {
        let mut d = 0i64;
        // SAFETY: the kernel writes one snd_pcm_sframes_t.
        unsafe { ioctl(self.fd(), DELAY, &mut d as *mut i64 as usize)? };
        Ok(d.max(0) as u64)
    }

    /// Waits up to `ms` until the device has room for a period (playback),
    /// or has recorded one (capture).
    pub fn wait(&self, ms: i32) -> io::Result<bool> {
        let events = match self.stream {
            Stream::Playback => POLLOUT,
            Stream::Capture => POLLIN,
        };
        poll(self.fd(), events, ms).map(|e| e & events != 0)
    }
}
