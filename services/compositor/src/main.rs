//! `compositor` — the Veda window system.
//!
//! Owns the screen and composes client windows onto it. It provides three
//! services:
//!
//! * `display` — clients create windows, attach shared pixel buffers and
//!   present frames (see `vproto::display`);
//! * `input` — device drivers report keyboard and pointer events;
//! * `displaydev` — a display driver hands over pictures it flips between
//!   at the vertical blank (see `screen`).
//!
//! Rendering is damage driven: only regions that changed are recomposited,
//! at most once per display frame: by the GPU, straight into the picture
//! the display shows next, where a driver flips and the GPU can draw into
//! its pictures (`gpu`); otherwise by the processor into the back buffer,
//! which is copied to the screen.

#![no_std]
#![no_main]

extern crate alloc;

mod decor;
mod gpu;
mod input;
mod keymap;
mod render;
mod screen;
mod session;
mod startup;
mod state;
mod switcher;
mod window;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vgfx::{Damage, Rect, Text};
use vipc::WaitSet;
use vproto::display::{Cursor, display};
use vproto::displaydev::{self, DisplayDevError, Link, displaydev as driver_protocol};
use vproto::input::InputEvent;
use vrt::object::{Channel, Vmo};
use vrt::println;
use vrt::vm::Mapping;

use decor::{Decor, DecorState};
use keymap::Keyboard;
use screen::Screen;
use session::Session;
use startup::Startup;
use state::{Compositor, DisplayClient};

vrt::entry!(main);

/// Reads a font file from the system image and leaks it (fonts live forever).
fn load_font(vfs: &vproto::vfs::Client, path: &str) -> Option<&'static [u8]> {
    let (vmo, len) = vfs.read_file(path.into()).ok()?.ok()?;
    let mut data = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut data).ok()?;
    Some(data.leak())
}

/// A display driver's connection (`displaydev`).
struct DriverLink<'a> {
    screen: &'a mut Screen,
    key: u64,
    /// A driver's (started by devmgr); nobody else may attach.
    trusted: bool,
    /// The firmware's framebuffer (physical address).
    framebuffer: u64,
}

impl driver_protocol::Server for DriverLink<'_> {
    fn attach(&mut self, screen: displaydev::Screen, link: Link) -> Result<(), DisplayDevError> {
        if !self.trusted {
            println!("{} cannot have the screen: only drivers may attach", screen.name);
            return Err(DisplayDevError::Denied);
        }
        let name = screen.name.clone();
        let result = self.screen.attach(self.key, screen, link, self.framebuffer);
        // A mismatch is told in detail where it is found.
        if let Err(e) = result
            && e != DisplayDevError::Mismatch
        {
            println!("{} cannot have the screen: {}", name, e);
        }
        result
    }

    fn screen(&mut self) -> Result<displaydev::ScreenMode, DisplayDevError> {
        if !self.trusted {
            return Err(DisplayDevError::Denied);
        }
        let r = self.screen.rect();
        Ok(displaydev::ScreenMode {
            width: r.w as u32,
            height: r.h as u32,
            rgbx: self.screen.rgb(),
            framebuffer: self.framebuffer,
        })
    }

    fn expect(&mut self, device: Option<String>) -> Result<(), DisplayDevError> {
        if !self.trusted {
            return Err(DisplayDevError::Denied);
        }
        self.screen.expect(self.key, device);
        Ok(())
    }
}

fn main() -> i32 {
    use vabi::startup::role;
    let (Some(fb_vmo), Some(info_vmo)) = (
        vrt::env::take_handle(role::FRAMEBUFFER).map(Vmo::from_handle),
        vrt::env::take_handle(role::BOOT_INFO).map(Vmo::from_handle),
    ) else {
        println!("no framebuffer");
        return 1;
    };
    let mut raw = [0u8; core::mem::size_of::<vabi::KernelBootInfo>()];
    let _ = info_vmo.read(0, &mut raw);
    // SAFETY: plain data written by the kernel.
    let info: vabi::KernelBootInfo = unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const vabi::KernelBootInfo) };
    let (width, height, pitch) =
        (info.framebuffer_width as i32, info.framebuffer_height as i32, info.framebuffer_pitch as usize);
    let fb_size = (pitch * height as usize).next_multiple_of(4096);
    let fb = match Mapping::new(fb_vmo, fb_size, vabi::map_flags::READ | vabi::map_flags::WRITE) {
        Ok(m) => m,
        Err(e) => {
            println!("cannot map the framebuffer: {}", e);
            return 1;
        }
    };

    let mut text = Text::new();
    // The fonts' indices: Inter SemiBold (titles), Inter Regular.
    let mut fonts = [None; 3];
    if let Ok(ch) = vproto::connect(vproto::vfs::NAME) {
        let vfs = vproto::vfs::Client::new(ch);
        for (i, path) in
            ["/system/fonts/Inter-SemiBold.otf", "/system/fonts/Inter-Regular.otf", "/system/fonts/Lato-Regular.ttf"]
                .iter()
                .enumerate()
        {
            if let Some(data) = load_font(&vfs, path) {
                fonts[i] = text.add_font(data);
            }
        }
    }
    let title_font = fonts[0].unwrap_or(0);
    let regular_font = fonts[1].unwrap_or(title_font);
    if text.font_count() == 0 {
        println!("warning: no fonts available, titles will be blank");
    }

    let display_listener = vproto::register(display::NAME).expect("cannot register the display service");
    let input_listener = vproto::register(vproto::input::NAME).expect("cannot register the input service");
    let driver_listener = vproto::register(driver_protocol::NAME).expect("cannot register the display driver service");
    println!("display {}x{} ready", width, height);

    let mut comp = Compositor {
        screen: Screen::new(fb, pitch, info.framebuffer_format == 2, width, height),
        decor: Decor::new(text, title_font),
        windows: BTreeMap::new(),
        order: Vec::new(),
        focused: None,
        next_id: 1,
        damage: Damage::new(),
        clients: BTreeMap::new(),
        shell: None,
        keyboard: Keyboard::default(),
        pointer: (width / 2, height / 2),
        cursor_rect: Rect::default(),
        cursor_shape: Cursor::Arrow,
        buttons: 0,
        drag: None,
        decor_state: DecorState::default(),
        hover_window: None,
        last_click: (0, 0, 0, 0, 0),
        click_count: 0,
        clipboard: String::new(),
        last_frame: 0,
        work_area: Rect::new(0, 0, width, height),
        switcher: None,
        snap: None,
        desktop_shown: Vec::new(),
        startup: None,
        gpu: None,
        gpu_ready: None,
        gpu_setup: None,
        gpu_tried: None,
    };
    // At system start (not after a restart), the screen stays black until
    // the display is up, then the splash fades in, comes to life and
    // dissolves into the desktop once it has drawn itself.
    if vrt::env::args().iter().any(|a| a == "splash") {
        let fonts = (title_font, regular_font);
        comp.startup = Some(Startup::new(width, height, &mut comp.decor.text, fonts, vrt::time::now_ns()));
    }
    comp.update_cursor_rect();
    comp.damage.add(comp.screen_rect());
    comp.composite();

    let mut inputs: BTreeMap<u64, Channel> = BTreeMap::new();
    // Display drivers' connections, and whether each is a driver's.
    let mut drivers: BTreeMap<u64, (Channel, bool)> = BTreeMap::new();
    let framebuffer = info.framebuffer_phys;
    let mut next_key = 10u64;
    const DISPLAY_KEY: u64 = 1;
    const INPUT_KEY: u64 = 2;
    const DRIVER_KEY: u64 = 3;
    const FLIP_DONE_KEY: u64 = 4;
    const GPU_SETUP_KEY: u64 = 5;
    const GPU_FENCE_KEY: u64 = 6;
    const INPUT_BASE: u64 = 1 << 40;
    const DRIVER_BASE: u64 = 1 << 41;
    loop {
        let now = vrt::time::now_ns();
        comp.screen.check(now);
        // A driver that flips gives pictures the GPU may draw into: it is
        // set up for them (once for each driver's), and no longer for the
        // pictures of a driver that went away.
        if let Some(driver) = comp.screen.driver()
            && comp.gpu_tried != Some(driver)
        {
            comp.gpu_tried = Some(driver);
            comp.gpu_ready = None;
            comp.drop_gpu("another display driver attached");
            comp.gpu_setup = comp.screen.pictures().and_then(gpu::Pending::start);
        }
        // The splash fades in once the screen is up.
        if let Some(s) = &mut comp.startup {
            s.release(now, comp.screen.settled());
        }
        let mut deadline = vabi::DEADLINE_INFINITE;
        if let Some(t) = comp.startup.as_ref().and_then(|s| s.deadline()) {
            deadline = t;
        }
        if comp.wants_frame() {
            deadline = deadline.min(comp.screen.next_frame(comp.last_frame).max(now));
        }
        if let Some(t) = comp.screen.timeout() {
            deadline = deadline.min(t);
        }
        if let Some(t) = comp.gpu_deadline() {
            deadline = deadline.min(t);
        }
        if let Some(r) = comp.keyboard.repeat_deadline() {
            deadline = deadline.min(r);
        }
        let mut ws = WaitSet::new();
        ws.add(display_listener.raw(), signals::READABLE, DISPLAY_KEY);
        ws.add(input_listener.raw(), signals::READABLE, INPUT_KEY);
        ws.add(driver_listener.raw(), signals::READABLE, DRIVER_KEY);
        if let Some(done) = comp.screen.done_event() {
            ws.add(done, signals::SIGNALED, FLIP_DONE_KEY);
        }
        if let Some(setup) = &comp.gpu_setup {
            ws.add(setup.event(), signals::SIGNALED, GPU_SETUP_KEY);
        }
        if let Some(g) = &comp.gpu
            && comp.screen.drawing().is_some()
        {
            ws.add(g.fence_event(), signals::SIGNALED, GPU_FENCE_KEY);
        }
        for (&k, (c, _)) in &drivers {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, DRIVER_BASE | k);
        }
        for (&k, c) in &comp.clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
        }
        for (&k, c) in &inputs {
            ws.add(c.raw(), signals::READABLE | signals::PEER_CLOSED, INPUT_BASE | k);
        }
        let ready = ws.wait(deadline).unwrap_or_default();
        for (key, observed) in ready {
            match key {
                DISPLAY_KEY => {
                    while let Some(ch) = vproto::accept(&display_listener) {
                        next_key += 1;
                        comp.clients.insert(next_key, DisplayClient { channel: ch, windows: Vec::new() });
                    }
                }
                INPUT_KEY => {
                    while let Some(ch) = vproto::accept(&input_listener) {
                        next_key += 1;
                        inputs.insert(next_key, ch);
                    }
                }
                DRIVER_KEY => {
                    while let Some((ch, who)) = vproto::accept_with_identity(&driver_listener) {
                        next_key += 1;
                        // devmgr starts the drivers with its own registry
                        // channel: they speak as devmgr.
                        drivers.insert(next_key, (ch, who.service && who.name == "devmgr"));
                    }
                }
                FLIP_DONE_KEY => comp.screen.flip_done(),
                GPU_SETUP_KEY => {
                    if let Some(result) = comp.gpu_setup.as_ref().and_then(|s| s.take()) {
                        comp.gpu_setup = None;
                        match result {
                            Ok(g) => comp.gpu_ready = Some(g),
                            Err(why) => println!("frames stay with the processor: {}", why),
                        }
                    }
                }
                GPU_FENCE_KEY => comp.gpu_signaled(vrt::time::now_ns()),
                k if k & DRIVER_BASE != 0 => {
                    let k = k & !DRIVER_BASE;
                    if observed & signals::READABLE != 0 {
                        while let Some(Ok(msg)) = drivers.get(&k).map(|(c, _)| c.read()) {
                            let trusted = drivers.get(&k).is_some_and(|(_, t)| *t);
                            let mut link = DriverLink { screen: &mut comp.screen, key: k, trusted, framebuffer };
                            match driver_protocol::dispatch(&mut link, msg) {
                                Ok(reply) => {
                                    if let Some((c, _)) = drivers.get(&k) {
                                        let _ = reply.send(c);
                                    }
                                }
                                Err(e) => println!("bad display driver request: {}", e),
                            }
                        }
                    } else if observed & signals::PEER_CLOSED != 0 {
                        drivers.remove(&k);
                        comp.screen.detach(k);
                    }
                }
                k if k & INPUT_BASE != 0 => {
                    let k = k & !INPUT_BASE;
                    if observed & signals::READABLE != 0 {
                        while let Some(Ok(msg)) = inputs.get(&k).map(|c| c.read()) {
                            if let Ok((_, events)) = vipc::decode_event::<Vec<InputEvent>>(msg) {
                                for ev in events {
                                    match ev {
                                        InputEvent::Absolute { x, y, max_x, max_y } => {
                                            let px = (x as i64 * width as i64 / max_x.max(1) as i64) as i32;
                                            let py = (y as i64 * height as i64 / max_y.max(1) as i64) as i32;
                                            comp.pointer_moved(px, py);
                                        }
                                        InputEvent::Motion { dx, dy } => {
                                            let (x, y) = comp.pointer;
                                            comp.pointer_moved(x + dx, y + dy);
                                        }
                                        InputEvent::Button { button, pressed } => comp.pointer_button(button, pressed),
                                        InputEvent::Scroll { dx, dy } => comp.scroll(dx, dy),
                                        InputEvent::Key { code, pressed } => comp.key(code, pressed),
                                    }
                                }
                            }
                        }
                    } else if observed & signals::PEER_CLOSED != 0 {
                        inputs.remove(&k);
                    }
                }
                k => {
                    if observed & signals::READABLE != 0 {
                        while let Some(Ok(msg)) = comp.clients.get(&k).map(|c| c.channel.read()) {
                            let mut s = Session { comp: &mut comp, client: k };
                            match display::dispatch(&mut s, msg) {
                                Ok(reply) => {
                                    if let Some(c) = comp.clients.get(&k) {
                                        let _ = reply.send(&c.channel);
                                    }
                                }
                                Err(e) => println!("bad display request: {}", e),
                            }
                        }
                    } else if observed & signals::PEER_CLOSED != 0 {
                        // Client gone: remove its windows.
                        if let Some(c) = comp.clients.remove(&k) {
                            for id in c.windows {
                                comp.destroy_window(id);
                            }
                        }
                        if comp.shell == Some(k) {
                            comp.shell = None;
                        }
                    }
                }
            }
        }
        let now = vrt::time::now_ns();
        while let Some(out) = comp.keyboard.poll_repeat(now) {
            comp.deliver_key(out);
        }
        // A frame the GPU draws that takes too long.
        if comp.gpu_deadline().is_some_and(|t| now >= t) {
            comp.gpu_signaled(now);
        }
        comp.screen.check(now);
        if comp.wants_frame() && now >= comp.screen.next_frame(comp.last_frame) {
            comp.composite();
        }
    }
}
