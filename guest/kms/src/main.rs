//! `kms` — Veda's display driver for Linux: a display Linux drives (KMS:
//! a GPU's display engine, or QEMU's standard VGA in tests) as the screen
//! Veda's compositor flips, through `displaydev`, as Veda's own display
//! drivers attach theirs.
//!
//! The compositor draws its frames into pictures of Veda's memory, which
//! this driver hands it, and Linux's driver shows them as they are:
//! imported as dma-bufs (the bridge maps each picture into the guest; a
//! display engine reads it through the IOMMU, a display without one copies
//! it into its own memory as it updates). A driver that cannot import gets
//! each picture copied into a buffer of its own before the flip. Flips
//! happen at the vertical blank; their events, on Veda's clock, tell the
//! compositor when frames were shown.
//!
//! Linux's driver sets the mode itself: the compositor's screen
//! (`displaydev::screen`), in the mode of that size the display prefers,
//! from the compositor's first frame on. The display is the one the
//! compositor draws on, whose memory holds the firmware's framebuffer (a
//! machine may have more GPUs with outputs): the card whose device has it
//! at the host's address of one of its BARs, which the monitor writes on
//! the kernel's command line (`veda.device=`), and the compositor checks
//! again.

mod drm;

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use guest_sys::{POLLIN, PollFd, poll_many};
use vabi::signals;
use vproto::displaydev::{self, DisplayDevError, FlipState, Link, Screen, ScreenMode, Shown, displaydev as protocol};
use vrt::object::{Channel, Event, Vmo};
use vrt::vm::Mapping;

use drm::{Card, DumbMap, ModeInfo, Output};

/// How long to wait for Linux's driver to make the card.
const CARD_WAIT: Duration = Duration::from_secs(10);
/// Bytes a picture's rows are aligned to (display engines' linear
/// framebuffers need 64 to 256).
const STRIDE_ALIGN: u32 = 256;

/// How to show the compositor's pictures.
enum Shows {
    /// Its framebuffers are the pictures themselves.
    Pictures(Vec<u32>),
    /// The pictures are copied into the card's own buffers, which take
    /// turns: their framebuffers, strides and memory, and the pictures
    /// mapped to read from.
    Copies { buffers: Vec<(u32, u32, DumbMap)>, pictures: Vec<Mapping>, next: usize },
}

/// The display, set up for the compositor's screen.
struct Display {
    card: Card,
    output: Output,
    mode: ModeInfo,
    width: u32,
    height: u32,
    stride: u32,
    /// The pictures (Veda's memory), as the compositor gets them.
    pictures: Vec<Vmo>,
    shows: Shows,
    /// Whether the mode is set (at the compositor's first frame).
    on: bool,
    name: String,
    firmware: Vec<u64>,
}

fn link_name(path: String) -> Option<String> {
    Some(std::fs::read_link(path).ok()?.file_name()?.to_string_lossy().into_owned())
}

/// The host's addresses of the memory BARs of the guest's function at
/// `location` (`00:01.0`), from the kernel's command line.
fn host_addresses(location: &str) -> Vec<u64> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    cmdline
        .split_whitespace()
        .filter_map(|o| o.strip_prefix("veda.device="))
        .find_map(|o| {
            let mut fields = o.split(',');
            (fields.next() == Some(location)).then(|| {
                fields
                    .filter_map(|f| f.split_once(':'))
                    .filter_map(|(_, a)| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok())
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// The card of the display whose memory holds the firmware's framebuffer
/// (at `framebuffer`; any card without one), with a connected output: the
/// card, its output, and its PCI location. Looks again until there is one
/// (a monitor may be connected later), saying once why it waits.
fn find_card(framebuffer: u64) -> (Card, Output, String) {
    let start = Instant::now();
    let mut said = false;
    loop {
        let mut cards: Vec<String> = std::fs::read_dir("/dev/dri")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        cards.retain(|c| c.starts_with("card"));
        cards.sort();
        let mut why = Vec::new();
        for c in cards {
            let card = match Card::open(&format!("/dev/dri/{c}")) {
                Ok(card) => card,
                Err(e) => {
                    why.push(format!("{c}: {e}"));
                    continue;
                }
            };
            let location = link_name(format!("/sys/class/drm/{c}/device")).unwrap_or_default();
            let location = location.trim_start_matches("0000:").to_string();
            if framebuffer != 0 && !host_addresses(&location).contains(&framebuffer) {
                why.push(format!("{c} ({}): not the screen", card.driver));
                continue;
            }
            match card.outputs() {
                Ok(mut outputs) if !outputs.is_empty() => {
                    let output = outputs.remove(0);
                    return (card, output, location);
                }
                Ok(_) => why.push(format!("{c} ({}): nothing connected", card.driver)),
                Err(e) => why.push(format!("{c} ({}): {e}", card.driver)),
            }
        }
        if !said && start.elapsed() >= CARD_WAIT {
            let why = if why.is_empty() { String::from("no card") } else { why.join("; ") };
            println!("kms: waiting for a display ({why})");
            said = true;
        }
        std::thread::sleep(if said { Duration::from_secs(1) } else { Duration::from_millis(200) });
    }
}

/// Stays, doing nothing: what it would do cannot be (init would start it
/// again were it to end).
fn idle() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

impl Display {
    /// The display set up for `screen`: its mode, and the pictures.
    fn new(card: Card, output: Output, location: &str, screen: ScreenMode) -> Result<Display, String> {
        if screen.rgbx {
            return Err("the compositor's pixels hold red in the low byte; the display takes XRGB".into());
        }
        let mode = output
            .modes
            .iter()
            .filter(|m| (m.hdisplay as u32, m.vdisplay as u32) == (screen.width, screen.height))
            .max_by_key(|m| (m.preferred(), m.vrefresh))
            .copied()
            .ok_or_else(|| format!("the display has no {}x{} mode", screen.width, screen.height))?;
        let (width, height) = (screen.width, screen.height);
        let stride = (width * 4).next_multiple_of(STRIDE_ALIGN);
        let bytes = (stride as usize * height as usize).next_multiple_of(4096);
        let mut pictures = Vec::new();
        let mut framebuffers = Vec::new();
        let mut imports = true;
        for _ in 0..displaydev::MIN_PICTURES {
            let vmo = Vmo::create(bytes).map_err(|e| format!("no memory for a picture: {e}"))?;
            if imports {
                let fb = vrt::guest::dmabuf(vmo.raw(), 0, bytes as u64, false).map_err(|e| format!("{e}")).and_then(
                    |buf: OwnedFd| {
                        card.import_framebuffer(buf.as_raw_fd(), width, height, stride).map_err(|e| format!("{e}"))
                    },
                );
                match fb {
                    Ok(fb) => framebuffers.push(fb),
                    Err(e) => {
                        println!("kms: {} cannot show Veda's memory ({e}); it gets copies", card.driver);
                        imports = false;
                    }
                }
            }
            pictures.push(vmo);
        }
        let shows = if imports {
            Shows::Pictures(framebuffers)
        } else {
            for fb in framebuffers {
                card.remove_framebuffer(fb);
            }
            let mut buffers = Vec::new();
            let mut maps = Vec::new();
            for p in &pictures {
                let (fb, pitch, map) = card.dumb_framebuffer(width, height).map_err(|e| format!("no buffer: {e}"))?;
                buffers.push((fb, pitch, map));
                let vmo = Vmo::from_handle(p.0.duplicate(None).map_err(|e| format!("{e}"))?);
                maps.push(Mapping::new(vmo, bytes, vabi::map_flags::READ).map_err(|e| format!("{e}"))?);
            }
            Shows::Copies { buffers, pictures: maps, next: 0 }
        };
        let name = format!("{} (Linux)", card.driver);
        let firmware = host_addresses(location);
        Ok(Display { card, output, mode, width, height, stride, pictures, shows, on: false, name, firmware })
    }

    /// Shows `picture` from the next vertical blank on (from now on if the
    /// mode is not set yet: the compositor's first frame).
    fn show(&mut self, picture: usize, seq: u32) -> io::Result<bool> {
        let fb = match &mut self.shows {
            Shows::Pictures(fbs) => fbs[picture],
            Shows::Copies { buffers, pictures, next } => {
                let count = buffers.len();
                let (fb, pitch, map) = &mut buffers[*next];
                let from = &pictures[picture];
                // SAFETY: the picture's mapping, `len` bytes long.
                let bytes = unsafe { core::slice::from_raw_parts(from.as_ptr(), from.len()) };
                let row = self.width as usize * 4;
                map.copy_from(bytes, self.stride as usize, *pitch as usize, row, self.height as usize);
                *next = (*next + 1) % count;
                *fb
            }
        };
        if !self.on {
            self.card.set_mode(&self.output, fb, &self.mode)?;
            self.on = true;
            return Ok(false);
        }
        self.card.flip(self.output.crtc, fb, seq as u64)?;
        Ok(true)
    }
}

/// An attachment to the compositor.
struct Session {
    link: Channel,
    state: FlipState,
    request: Event,
    done: Event,
}

fn attach(d: &Display, client: protocol::Client) -> Result<Session, String> {
    let (state, state_vmo) = FlipState::create().map_err(|e| format!("{e}"))?;
    let (request, done) = (Event::create().map_err(|e| format!("{e}"))?, Event::create().map_err(|e| format!("{e}"))?);
    let dup = |e: &Event| e.0.duplicate(None).map(Event::from_handle).map_err(|e| format!("{e}"));
    let mut theirs = Vec::new();
    for p in &d.pictures {
        theirs.push(Vmo::from_handle(p.0.duplicate(None).map_err(|e| format!("{e}"))?));
    }
    let screen = Screen {
        name: d.name.clone(),
        width: d.width,
        height: d.height,
        stride: d.stride,
        rgbx: false,
        firmware: d.firmware.clone(),
        period_ns: d.mode.period_ns(),
    };
    let link = Link { pictures: theirs, state: state_vmo, request: dup(&request)?, done: dup(&done)? };
    match client.attach(screen, link) {
        Ok(Ok(())) => Ok(Session { link: client.into_channel(), state, request, done }),
        Ok(Err(e)) => Err(format!("the compositor refused the display: {e}")),
        Err(_) => Err("the compositor went away".into()),
    }
}

/// Veda's clock at Linux's monotonic `ns`.
fn veda_time(ns: u64) -> u64 {
    // Both clocks follow the TSC; their difference, read now.
    let (veda, linux) = (vrt::time::now_ns(), guest_sys::monotonic_ns());
    (ns as i64 + (veda as i64 - linux as i64)).max(0) as u64
}

/// Flips for one compositor until it goes away.
fn serve(d: &mut Display, s: &Session) -> io::Result<()> {
    let watch = |handle, signals| vrt::guest::watch(handle, signals).map_err(|e| io::Error::other(format!("{e}")));
    let mut request = File::from(watch(s.request.raw(), signals::SIGNALED)?);
    let closed = watch(s.link.raw(), signals::PEER_CLOSED)?;
    let period = d.mode.period_ns();
    let mut last_seq = 0;
    // A flip on its way to the screen, and the request that came meanwhile
    // (one at a time: the compositor waits for each).
    let mut flipping = false;
    let mut waiting: Option<(usize, u32)> = None;
    loop {
        let mut fds = [
            PollFd::new(request.as_raw_fd(), POLLIN),
            PollFd::new(closed.as_raw_fd(), POLLIN),
            PollFd::new(d.card.fd(), POLLIN),
        ];
        poll_many(&mut fds, -1)?;
        if fds[1].revents != 0 {
            return Ok(());
        }
        if fds[0].revents & POLLIN != 0 {
            let _ = request.read(&mut [0u8; 4]);
            let _ = s.request.clear();
            let (picture, seq) = s.state.queued();
            if seq != last_seq && (picture as usize) < d.pictures.len() {
                last_seq = seq;
                waiting = Some((picture as usize, seq));
            }
        }
        if fds[2].revents & POLLIN != 0 {
            for f in d.card.events()? {
                flipping = false;
                let shown = Shown {
                    seq: f.user_data as u32,
                    frames: f.sequence,
                    blank_ns: veda_time(f.blank_ns),
                    period_ns: period,
                };
                s.state.set_shown(shown);
                let _ = s.done.signal();
            }
        }
        if !flipping && let Some((picture, seq)) = waiting.take() {
            match d.show(picture, seq) {
                Ok(true) => flipping = true,
                // The mode is set: the picture is on the screen now.
                Ok(false) => {
                    let blank_ns = vrt::time::now_ns();
                    s.state.set_shown(Shown { seq, frames: 0, blank_ns, period_ns: period });
                    let _ = s.done.signal();
                    println!("kms: {}: showing the compositor's frames, {}x{}", d.name, d.width, d.height);
                }
                Err(e) if e.raw_os_error() == Some(16) => waiting = Some((picture, seq)),
                Err(e) => return Err(e),
            }
        }
    }
}

fn main() {
    let mut display: Option<Display> = None;
    loop {
        let client = match vproto::connect(protocol::NAME) {
            Ok(ch) => protocol::Client::new(ch),
            Err(_) => {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if display.is_none() {
            let screen = match client.screen() {
                Ok(Ok(s)) => s,
                Ok(Err(DisplayDevError::Denied)) | Err(_) => {
                    println!("kms: the compositor does not answer");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                Ok(Err(e)) => {
                    println!("kms: the compositor's screen: {e}");
                    idle();
                }
            };
            let (card, output, location) = find_card(screen.framebuffer);
            println!("kms: {} at pci {location}, connector {} on CRTC {}", card.driver, output.connector, output.crtc);
            match Display::new(card, output, &location, screen) {
                Ok(d) => {
                    let how = if matches!(d.shows, Shows::Pictures(_)) { "shown as they are" } else { "copied" };
                    let millihertz = 1_000_000_000_000 / d.mode.period_ns().max(1);
                    println!(
                        "kms: {}: {}x{} at {}.{:03} Hz, the compositor's pictures {how}",
                        d.name,
                        d.width,
                        d.height,
                        millihertz / 1000,
                        millihertz % 1000
                    );
                    display = Some(d);
                }
                Err(e) => {
                    println!("kms: {e}");
                    idle();
                }
            }
        }
        let d = display.as_mut().expect("the display");
        match attach(d, client) {
            Ok(session) => {
                println!("kms: {}: attached to the compositor", d.name);
                match serve(d, &session) {
                    Ok(()) => println!("kms: {}: the compositor went away", d.name),
                    Err(e) => {
                        println!("kms: {}: {e}", d.name);
                        return;
                    }
                }
            }
            Err(e) => {
                println!("kms: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
