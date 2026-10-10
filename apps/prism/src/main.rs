//! Prism: an OpenGL ES 3.0 showcase.
//!
//! A reflective torus knot under a sky, with orbiting crystals, shadows
//! and a fountain of sparks (see `prism_scene`), rendered with `vgl` (on the
//! GPU through Veda's renderer if there is one, in software otherwise)
//! and shown in a window. The scene is rendered at a resolution that adapts
//! to what rendering costs, and scaled to the window.
//!
//! Controls: arrows turn the camera, Page Up/Down zoom, Space pauses,
//! S toggles shadows, P sparks, F3 statistics, Esc quits.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use prism_scene::scene::{Controls, Scene};
use vabi::{WaitItem, signals};
use vgfx::{Color, Rect};
use vgl::backend::Present;
use vgl::{Config, gl};
use vproto::display::{WindowEvent, WindowSpec};
use vproto::input::keys;
use vui::window::{Window, connect};

vrt::entry!(main);

/// Internal resolution divisors to choose from.
const SCALES: [f32; 6] = [1.0, 1.25, 1.5, 2.0, 2.5, 3.0];
/// The frame time to keep to (30 frames a second). The resolution drops
/// while rendering takes most of it, and rises when rendering takes little:
/// what the renderer costs decides, not the frame rate, which presenting
/// and other windows limit as well (on the GPU, rendering is cheap and a
/// lower resolution would gain nothing).
const FRAME_MS: f32 = 1000.0 / 30.0;

/// Keys held down.
#[derive(Default)]
struct Held {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    zoom_in: bool,
    zoom_out: bool,
}

fn internal_size(w: i32, h: i32, scale: f32) -> (i32, i32) {
    (((w as f32 / scale) as i32).max(16), ((h as f32 / scale) as i32).max(16))
}

fn main() -> i32 {
    let display = match connect() {
        Ok(d) => d,
        Err(e) => {
            vrt::println!("cannot connect to the display: {:?}", e);
            return 1;
        }
    };
    let mut spec = WindowSpec::new("Prism", 1024, 640);
    spec.app_id = "prism".into();
    spec.min_width = 320;
    spec.min_height = 200;
    let mut window = match Window::new(&display, spec) {
        Ok(w) => w,
        Err(e) => {
            vrt::println!("cannot create a window: {:?}", e);
            return 1;
        }
    };
    let (mut text, fonts) = vui::load_fonts();
    let mut level = 0usize;
    let (iw, ih) = internal_size(window.width, window.height, SCALES[level]);
    let config = Config { width: iw as u32, height: ih as u32, ..Config::default() };
    let mut gl = vgl::veda::context(config);
    let renderer: String = gl.get_string(gl::RENDERER).unwrap_or_default();
    vrt::println!("{} ({})", renderer, gl.get_string(gl::VERSION).unwrap_or_default());
    let mut scene = match Scene::new(&mut gl) {
        Ok(s) => s,
        Err(e) => {
            vrt::println!("{}", e);
            return 1;
        }
    };
    let mut held = Held::default();
    let mut show_stats = false;
    let start = vrt::time::now_ns();
    let mut last = start;
    let (mut frames, mut fps, mut fps_since) = (0u32, 0.0f32, start);
    let mut log_since = start;
    let mut render_ms = 0.0f32;
    let mut since_change = 0u32;
    loop {
        for ev in window.poll_events() {
            match ev {
                WindowEvent::Key { code, pressed, repeat, .. } => {
                    match code {
                        keys::LEFT => held.left = pressed,
                        keys::RIGHT => held.right = pressed,
                        keys::UP => held.up = pressed,
                        keys::DOWN => held.down = pressed,
                        keys::PAGEUP => held.zoom_in = pressed,
                        keys::PAGEDOWN => held.zoom_out = pressed,
                        _ => {}
                    }
                    if pressed && !repeat {
                        match code {
                            keys::SPACE => scene.options.paused = !scene.options.paused,
                            keys::S => scene.options.shadows = !scene.options.shadows,
                            keys::P => scene.options.particles = !scene.options.particles,
                            keys::F3 => show_stats = !show_stats,
                            keys::ESC => return 0,
                            _ => {}
                        }
                    }
                }
                WindowEvent::Focus { focused: false } => held = Held::default(),
                WindowEvent::CloseRequested {} => return 0,
                _ => {}
            }
        }
        if window.closed {
            return 0;
        }
        let now = vrt::time::now_ns();
        let dt = ((now - last) as f64 / 1e9).clamp(0.0, 0.1) as f32;
        last = now;
        let axis = |a: bool, b: bool| f32::from(u8::from(a)) - f32::from(u8::from(b));
        let controls = Controls {
            yaw: axis(held.left, held.right) * 1.4,
            pitch: axis(held.up, held.down) * 0.8,
            zoom: axis(held.zoom_out, held.zoom_in) * 0.9,
        };
        scene.update(dt, controls);

        // Render at the internal resolution.
        let (iw, ih) = internal_size(window.width, window.height, SCALES[level]);
        gl.resize(iw as u32, ih as u32);
        let t0 = vrt::time::now_ns();
        scene.render(&mut gl, iw, ih, dt);
        gl.flush();
        let t1 = vrt::time::now_ns();

        // Wait until the window can take a frame.
        while !window.can_draw() {
            let mut items = [WaitItem {
                handle: window.event_channel().raw(),
                signals: signals::READABLE | signals::PEER_CLOSED,
                ..Default::default()
            }];
            let _ = vrt::object::wait_many(&mut items, vrt::time::now_ns() + 50_000_000);
            for ev in window.poll_events() {
                if let WindowEvent::CloseRequested {} = ev {
                    return 0;
                }
            }
            if window.closed {
                return 0;
            }
        }
        let Ok(mut canvas) = window.begin_frame() else { continue };
        let (cw, ch) = (canvas.width(), canvas.height());
        {
            let (pixels, stride) = canvas.pixels_mut();
            let mut dst =
                Present { pixels, stride: stride as usize, width: cw as u32, height: ch as u32, opaque: true };
            gl.present_to(&mut dst);
        }
        // The HUD.
        let title = fonts[1];
        let body = fonts[0];
        text.draw(&mut canvas, title, 22.0, 18.0, 34.0, "Prism", Color(0xFFFFFFFF));
        let line = format!("OpenGL ES 3.0  \u{00B7}  {renderer}");
        text.draw(&mut canvas, body, 13.0, 18.0, 54.0, &line, Color(0xD0E8EEF8));
        let help = "Arrows: camera   PgUp/PgDn: zoom   Space: pause   S: shadows   P: sparks   F3: stats";
        text.draw(&mut canvas, body, 12.0, 18.0, ch as f32 - 14.0, help, Color(0xB0E8EEF8));
        if show_stats {
            let lines = [format!("{fps:.1} fps   render {render_ms:.1} ms"), format!("{iw} x {ih} -> {cw} x {ch}")];
            let (px, py) = (cw - 250, 16);
            canvas.fill_rounded_rect(Rect::new(px, py, 234, 54), 8.0, Color(0xB0101418));
            for (i, l) in lines.iter().enumerate() {
                text.draw(
                    &mut canvas,
                    fonts[2],
                    12.0,
                    (px + 10) as f32,
                    (py + 22 + 18 * i as i32) as f32,
                    l,
                    Color(0xFFE8F0F8),
                );
            }
        }
        drop(canvas);
        let _ = window.present(&[]);

        // Frame rate, and the resolution it calls for.
        frames += 1;
        render_ms = render_ms * 0.9 + (t1 - t0) as f32 / 1e6 * 0.1;
        if now - fps_since >= 1_000_000_000 {
            fps = frames as f32 / ((now - fps_since) as f32 / 1e9);
            frames = 0;
            fps_since = now;
            if now - log_since >= 5_000_000_000 {
                log_since = now;
                vrt::println!("{:.1} fps, render {:.1} ms, {}x{} -> {}x{}", fps, render_ms, iw, ih, cw, ch);
            }
            since_change += 1;
            if since_change >= 2 {
                let old = level;
                if render_ms > FRAME_MS * 0.75 && level + 1 < SCALES.len() {
                    level += 1;
                } else if render_ms < FRAME_MS * 0.3 && level > 0 {
                    level -= 1;
                }
                if level != old {
                    since_change = 0;
                }
            }
        }
    }
}
