//! `kms` — Veda's display driver for Linux: a display Linux drives (KMS:
//! a GPU's display engine, or QEMU's standard VGA in tests) as the screen
//! Veda's compositor flips, through `displaydev`, as Veda's own display
//! drivers attach theirs.
//!
//! The compositor draws its frames into pictures of Veda's memory, which
//! this driver hands it, and Linux's driver shows them as they are:
//! imported as dma-bufs. The bridge maps each picture into the guest, and
//! a display engine reads it through the IOMMU, without snooping the
//! processor's caches (the pictures are write-combining memory); a display
//! without one copies it into its own memory as it updates. A driver that
//! cannot import gets each picture (cached memory then, which this driver
//! reads) copied into a buffer of its own before the flip. Flips happen at
//! the vertical blank; their events, on Veda's clock, tell the compositor
//! when frames were shown.
//!
//! The mode is set at the compositor's first frame, with Linux's atomic
//! modesetting: one of the size of the compositor's screen
//! (`displaydev::screen`) if the display has one, the one it prefers of
//! them; else the display's own (a laptop's panel has no other), the
//! display engine scaling the pictures to it as their proportions allow,
//! in its middle. The display is the one the compositor draws on, whose
//! memory holds the firmware's framebuffer (a machine may have more GPUs
//! with outputs): the card whose device has it at the host's address of
//! one of its BARs, which the monitor writes on the kernel's command line
//! (`veda.device=`), and the compositor checks again.

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

use drm::{Card, DumbMap, ModeInfo, Output, Place};

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

impl Shows {
    /// A framebuffer pictures are shown in.
    fn any_framebuffer(&self) -> u32 {
        match self {
            Shows::Pictures(fbs) => fbs[0],
            Shows::Copies { buffers, .. } => buffers[0].0,
        }
    }
}

/// The display, set up for the compositor's screen.
struct Display {
    card: Card,
    output: Output,
    mode: ModeInfo,
    /// Where the pictures go on the display.
    place: Place,
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

/// The mode to show pictures of `size` in, of `modes` (a display's), and
/// where they go on the display: a mode of their size if there is one (the
/// one the display prefers of them, the fastest), all of it; else the
/// display's own (the mode it prefers, else its largest), the pictures as
/// large as their proportions let them be on it, in its middle.
fn choose(modes: &[ModeInfo], size: (u32, u32)) -> Option<(ModeInfo, Place)> {
    if size.0 == 0 || size.1 == 0 {
        return None;
    }
    let exact = modes.iter().filter(|m| m.size() == size).max_by_key(|m| (m.preferred(), m.vrefresh));
    let mode = exact.or_else(|| {
        modes.iter().max_by_key(|m| {
            let (w, h) = m.size();
            (m.preferred(), w as u64 * h as u64, m.vrefresh)
        })
    })?;
    Some((*mode, fit(size, mode.size())))
}

/// Where a picture of `size` goes on a display of `on`'s: as large as its
/// proportions let it be, in the middle.
fn fit((width, height): (u32, u32), on: (u32, u32)) -> Place {
    let (w, h) = (width as u64, height as u64);
    let (on_w, on_h) = (on.0 as u64, on.1 as u64);
    // Its sides reach the display's together, or the one that reaches it
    // first is as long as the display's.
    let (w, h) = if on_w * h <= on_h * w { (on_w, h * on_w / w) } else { (w * on_h / h, on_h) };
    let (w, h) = (w as u32, h as u32);
    Place { x: (on.0 - w) / 2, y: (on.1 - h) / 2, width: w, height: h }
}

/// Pictures `card` shows as they are, imported: write-combining memory (a
/// display engine reads it without snooping the processor's caches), and
/// their framebuffers. Writable by devices: Linux's DRM drivers map what
/// they import both ways (i915 does), though a display only reads it.
fn imported(card: &Card, width: u32, height: u32, stride: u32, bytes: usize) -> Result<(Vec<Vmo>, Vec<u32>), String> {
    let mut pictures = Vec::new();
    let mut framebuffers = Vec::new();
    for _ in 0..displaydev::MIN_PICTURES {
        let fb = Vmo::create_with(bytes, vabi::vmo_flags::WRITE_COMBINING)
            .map_err(|e| format!("no write-combining memory: {e}"))
            .and_then(|vmo| {
                let buf: OwnedFd = vrt::guest::dmabuf(vmo.raw(), 0, bytes as u64, true).map_err(|e| format!("{e}"))?;
                let fb = card.import_framebuffer(buf.as_raw_fd(), width, height, stride).map_err(|e| format!("{e}"))?;
                Ok((vmo, fb))
            });
        match fb {
            Ok((vmo, fb)) => {
                pictures.push(vmo);
                framebuffers.push(fb);
            }
            Err(e) => {
                for fb in framebuffers {
                    card.remove_framebuffer(fb);
                }
                return Err(e);
            }
        }
    }
    Ok((pictures, framebuffers))
}

/// Pictures `card` gets copies of, in buffers of its own: memory this
/// driver reads (cached), and how they are shown.
fn copied(card: &Card, width: u32, height: u32, bytes: usize) -> Result<(Vec<Vmo>, Shows), String> {
    let mut pictures = Vec::new();
    let mut buffers = Vec::new();
    let mut maps = Vec::new();
    for _ in 0..displaydev::MIN_PICTURES {
        let vmo = Vmo::create(bytes).map_err(|e| format!("no memory for a picture: {e}"))?;
        buffers.push(card.dumb_framebuffer(width, height).map_err(|e| format!("no buffer: {e}"))?);
        let read = Vmo::from_handle(vmo.0.duplicate(None).map_err(|e| format!("{e}"))?);
        maps.push(Mapping::new(read, bytes, vabi::map_flags::READ).map_err(|e| format!("{e}"))?);
        pictures.push(vmo);
    }
    Ok((pictures, Shows::Copies { buffers, pictures: maps, next: 0 }))
}

impl Display {
    /// The display set up for `screen`: its mode, where the pictures go,
    /// and the pictures.
    fn new(card: Card, output: Output, location: &str, screen: ScreenMode) -> Result<Display, String> {
        if screen.rgbx {
            return Err("the compositor's pixels hold red in the low byte; the display takes XRGB".into());
        }
        let (width, height) = (screen.width, screen.height);
        let (mode, place) = choose(&output.modes, (width, height))
            .ok_or_else(|| format!("the display has no mode to show {width}x{height} pictures in"))?;
        let stride = (width * 4).next_multiple_of(STRIDE_ALIGN);
        let bytes = (stride as usize * height as usize).next_multiple_of(4096);
        let (pictures, shows) = match imported(&card, width, height, stride, bytes) {
            Ok((pictures, framebuffers)) => (pictures, Shows::Pictures(framebuffers)),
            Err(e) => {
                println!("kms: {} cannot show Veda's memory ({e}); it gets copies", card.driver);
                copied(&card, width, height, bytes)?
            }
        };
        // Before the compositor counts on the display: whether its driver
        // takes the mode, and the pictures where they go (scaled, it may
        // not).
        card.test_mode(&output, shows.any_framebuffer(), (width, height), &mode, place).map_err(|e| {
            let (w, h) = mode.size();
            format!(
                "{} cannot show {width}x{height} pictures at {}x{} in a {w}x{h} mode: {e}",
                card.driver, place.width, place.height
            )
        })?;
        let name = format!("{} (Linux)", card.driver);
        let firmware = host_addresses(location);
        Ok(Display { card, output, mode, place, width, height, stride, pictures, shows, on: false, name, firmware })
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
            self.card.set_mode(&self.output, fb, (self.width, self.height), &self.mode, self.place)?;
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

/// Whether Linux's driver `driver` leaves the firmware's framebuffer's
/// memory only pixels: QEMU's standard VGA's (bochs-drm) holds nothing but
/// pictures; a GPU's driver uses that memory for its own (an Intel GPU's
/// aperture, which i915's tables translate to whatever they map).
fn keeps_firmware_memory(driver: &str) -> bool {
    driver == "bochs-drm"
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
        firmware_kept: keeps_firmware_memory(&d.card.driver),
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
                    let (w, h) = d.mode.size();
                    println!(
                        "kms: {}: {w}x{h} at {}.{:03} Hz, the compositor's pictures {how}",
                        d.name,
                        millihertz / 1000,
                        millihertz % 1000
                    );
                    let p = d.place;
                    if (p.width, p.height) != (d.width, d.height) {
                        println!(
                            "kms: {}: the compositor's {}x{} scaled to {}x{} at ({}, {})",
                            d.name, d.width, d.height, p.width, p.height, p.x, p.y
                        );
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u16, height: u16, hz: u32, preferred: bool) -> ModeInfo {
        let mut m = ModeInfo::default();
        (m.hdisplay, m.vdisplay, m.vrefresh) = (width, height, hz);
        if preferred {
            m.kind |= drm::MODE_TYPE_PREFERRED;
        }
        m
    }

    fn place(x: u32, y: u32, width: u32, height: u32) -> Place {
        Place { x, y, width, height }
    }

    #[test]
    fn a_mode_of_the_pictures_size_shows_them_whole() {
        let modes = [mode(1920, 1080, 60, false), mode(1280, 800, 60, false), mode(1280, 800, 75, false)];
        let (m, at) = choose(&modes, (1280, 800)).unwrap();
        assert_eq!((m.size(), m.vrefresh, at), ((1280, 800), 75, place(0, 0, 1280, 800)));
        // The display's preferred one among them, before a faster one.
        let modes = [mode(1280, 800, 75, false), mode(1280, 800, 60, true)];
        assert_eq!(choose(&modes, (1280, 800)).unwrap().0.vrefresh, 60);
    }

    #[test]
    fn else_the_displays_own_mode_with_the_pictures_scaled() {
        // A laptop's panel: its one mode, twice the pictures' width; their
        // proportions leave rows above and below.
        let panel = [mode(3840, 2400, 60, true), mode(3840, 2400, 48, false)];
        let (m, at) = choose(&panel, (1920, 1080)).unwrap();
        assert_eq!((m.size(), m.vrefresh, at), ((3840, 2400), 60, place(0, 120, 3840, 2160)));
        assert_eq!(choose(&panel, (1920, 1200)).unwrap().1, place(0, 0, 3840, 2400));
        // Columns left and right; smaller than the pictures (1366x768 is a
        // little wider than 16:9).
        assert_eq!(fit((1280, 800), (1920, 1080)), place(96, 0, 1728, 1080));
        assert_eq!(fit((1920, 1080), (1366, 768)), place(0, 0, 1365, 768));
        // No preferred mode: the largest.
        let monitor = [mode(1024, 768, 60, false), mode(2560, 1440, 60, false), mode(1920, 1080, 60, false)];
        assert_eq!(choose(&monitor, (1280, 800)).unwrap().0.size(), (2560, 1440));
    }

    #[test]
    fn nothing_to_show_in_no_mode() {
        assert!(choose(&[], (1280, 800)).is_none());
        assert!(choose(&[mode(1280, 800, 60, true)], (0, 800)).is_none());
    }
}
