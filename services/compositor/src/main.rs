//! `compositor` — the Vindows window system.
//!
//! Owns the framebuffer and composes client windows into it. It provides two
//! services:
//!
//! * `display` — clients create windows, attach shared pixel buffers and
//!   present frames (see `vproto::display`);
//! * `input` — device drivers report keyboard and pointer events.
//!
//! Rendering is damage driven: only regions that changed are recomposited
//! into the back buffer and copied to the framebuffer, at most once per
//! display frame.

#![no_std]
#![no_main]

extern crate alloc;

mod decor;
mod input;
mod keymap;
mod render;
mod session;
mod state;
mod switcher;
mod window;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vgfx::{Bitmap, Damage, Rect, Text};
use vipc::WaitSet;
use vproto::display::{Cursor, display};
use vproto::input::InputEvent;
use vrt::object::{Channel, Vmo};
use vrt::println;
use vrt::vm::Mapping;

use decor::{Decor, DecorState};
use keymap::Keyboard;
use render::Screen;
use session::Session;
use state::{Compositor, DisplayClient};

vrt::entry!(main);

/// Shortest time between two composited frames (60 Hz).
const FRAME_NS: u64 = 16_666_666;

/// Reads a font file from the system image and leaks it (fonts live forever).
fn load_font(vfs: &vproto::vfs::Client, path: &str) -> Option<&'static [u8]> {
    let (vmo, len) = vfs.read_file(path.into()).ok()?.ok()?;
    let mut data = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut data).ok()?;
    Some(data.leak())
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
    let mut title_font = 0;
    if let Ok(ch) = vproto::connect(vproto::vfs::NAME) {
        let vfs = vproto::vfs::Client::new(ch);
        for (i, path) in
            ["/system/fonts/Inter-SemiBold.otf", "/system/fonts/Inter-Regular.otf", "/system/fonts/Lato-Regular.ttf"]
                .iter()
                .enumerate()
        {
            if let Some(data) = load_font(&vfs, path)
                && let Some(idx) = text.add_font(data)
                && i == 0
            {
                title_font = idx;
            }
        }
    }
    if text.font_count() == 0 {
        println!("warning: no fonts available, titles will be blank");
    }

    let display_listener = vproto::register(display::NAME).expect("cannot register the display service");
    let input_listener = vproto::register(vproto::input::NAME).expect("cannot register the input service");
    println!("display {}x{} ready", width, height);

    let mut comp = Compositor {
        screen: Screen { fb, pitch, rgb: info.framebuffer_format == 2, back: Bitmap::new(width, height) },
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
    };
    comp.update_cursor_rect();
    comp.damage.add(comp.screen_rect());
    comp.composite();

    let mut inputs: BTreeMap<u64, Channel> = BTreeMap::new();
    let mut next_key = 10u64;
    const DISPLAY_KEY: u64 = 1;
    const INPUT_KEY: u64 = 2;
    const INPUT_BASE: u64 = 1 << 40;
    loop {
        let now = vrt::time::now_ns();
        let mut deadline = vabi::DEADLINE_INFINITE;
        if !comp.damage.is_empty() || comp.animating() {
            deadline = (comp.last_frame + FRAME_NS).max(now);
        }
        if let Some(r) = comp.keyboard.repeat_deadline() {
            deadline = deadline.min(r);
        }
        let mut ws = WaitSet::new();
        ws.add(display_listener.raw(), signals::READABLE, DISPLAY_KEY);
        ws.add(input_listener.raw(), signals::READABLE, INPUT_KEY);
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
        if (!comp.damage.is_empty() || comp.animating()) && now >= comp.last_frame + FRAME_NS {
            comp.composite();
        }
    }
}
