//! A small game harness (feature `app`): one `vui` window, keyboard state,
//! frame timing, 3D rendering at a reduced internal resolution scaled to the
//! window, and a HUD layer drawn with `vgfx` at full resolution.
//!
//! ```ignore
//! struct MyGame { /* ... */ }
//! impl v3d::app::Game for MyGame {
//!     fn update(&mut self, app: &mut v3d::app::AppState, dt: f32) { /* input, simulation */ }
//!     fn render(&mut self, r: &mut v3d::Renderer) { /* r.frame(...) and draws */ }
//!     fn hud(&mut self, hud: &mut v3d::app::Hud) { /* text and panels */ }
//! }
//! v3d::app::run(v3d::app::Settings::new("My Game", "mygame"), |r| MyGame::new(r))
//! ```
//!
//! F3 toggles a performance overlay; statistics are also logged every five
//! seconds.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vgfx::{Bitmap, Canvas, Color, Rect, Text};
use vproto::display::{WindowEvent, WindowSpec};
use vproto::input::keys;
use vui::window::{Window, connect};

use crate::pool::ThreadPool;
use crate::renderer::Renderer;

/// Keyboard state.
#[derive(Clone)]
pub struct Keys {
    down: [bool; 256],
    pressed: [bool; 256],
    released: [bool; 256],
}

impl core::fmt::Debug for Keys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Keys")
    }
}

impl Keys {
    fn new() -> Keys {
        Keys { down: [false; 256], pressed: [false; 256], released: [false; 256] }
    }

    /// The key is held (or was pressed since the last frame).
    pub fn down(&self, code: u16) -> bool {
        let c = code as usize & 255;
        self.down[c] || self.pressed[c]
    }

    /// The key went down since the last frame.
    pub fn pressed(&self, code: u16) -> bool {
        self.pressed[code as usize & 255]
    }

    /// The key went up since the last frame.
    pub fn released(&self, code: u16) -> bool {
        self.released[code as usize & 255]
    }

    /// Any of the keys is held.
    pub fn any_down(&self, codes: &[u16]) -> bool {
        codes.iter().any(|&c| self.down(c))
    }

    /// Any of the keys was pressed.
    pub fn any_pressed(&self, codes: &[u16]) -> bool {
        codes.iter().any(|&c| self.pressed(c))
    }

    fn end_frame(&mut self) {
        self.pressed = [false; 256];
        self.released = [false; 256];
    }

    fn clear(&mut self) {
        self.down = [false; 256];
    }
}

/// Window and input state passed to [`Game::update`].
#[derive(Debug)]
pub struct AppState {
    /// Keyboard state (held keys and this frame's presses).
    pub keys: Keys,
    /// Seconds since the game started.
    pub time: f64,
    /// Window width in pixels.
    pub width: i32,
    /// Window height in pixels.
    pub height: i32,
    /// The window has the keyboard focus (games pause when it is lost).
    pub focused: bool,
    /// Set to close the window.
    pub quit: bool,
    /// Measured frames per second.
    pub fps: f32,
    /// Toggled with F8: games let the computer play (for demos and
    /// automated testing).
    pub autopilot: bool,
}

/// Fonts loaded for the HUD (indices into [`Hud::text`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudFont {
    /// The UI sans-serif face.
    Regular,
    /// The bold UI face.
    Bold,
    /// The monospaced face (numbers that should not jitter).
    Mono,
    /// The bold monospaced face.
    MonoBold,
}

/// Pre-rendered HUD pieces (see [`Hud::cached`]).
#[derive(Default)]
pub struct HudCache {
    items: Vec<(u64, Bitmap)>,
}

/// The HUD drawing context: the window canvas (already showing the 3D
/// image) and the text renderer.
pub struct Hud<'a, 'c> {
    /// The window's canvas, showing the presented 3D image.
    pub canvas: &'a mut Canvas<'c>,
    /// The text renderer with the HUD fonts loaded.
    pub text: &'a mut Text,
    /// Font indices for [`HudFont`] (see [`Hud::font_index`]).
    pub fonts: [usize; 4],
    /// Canvas width in pixels.
    pub width: i32,
    /// Canvas height in pixels.
    pub height: i32,
    /// Layers kept between frames by [`Hud::cached`].
    pub cache: &'a mut HudCache,
}

impl Hud<'_, '_> {
    /// Draws a HUD piece that rarely changes. The first time a `key` is
    /// seen, `draw` renders the piece into a transparent `w` x `h` bitmap
    /// (in its own coordinates); afterwards the bitmap is just blended at
    /// (`x`, `y`). Include everything the content depends on in `key`.
    /// Vector paths, circles and gradients are slow under CPU emulation, so
    /// this keeps elaborate HUDs cheap.
    pub fn cached(
        &mut self,
        key: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        draw: impl FnOnce(&mut Canvas, &mut Text, [usize; 4]),
    ) {
        let found = self.cache.items.iter().position(|(k, b)| *k == key && b.width == w && b.height == h);
        let index = match found {
            Some(i) => i,
            None => {
                let mut bmp = Bitmap::new(w.max(1), h.max(1));
                {
                    let mut c = Canvas::for_bitmap(&mut bmp);
                    draw(&mut c, self.text, self.fonts);
                }
                if self.cache.items.len() >= 96 {
                    self.cache.items.remove(0);
                }
                self.cache.items.push((key, bmp));
                self.cache.items.len() - 1
            }
        };
        self.canvas.draw_bitmap(&self.cache.items[index].1, x, y, 255);
    }

    /// Font index for a [`HudFont`] (for use inside [`Hud::cached`]).
    pub fn font_index(fonts: [usize; 4], f: HudFont) -> usize {
        fonts[match f {
            HudFont::Regular => 0,
            HudFont::Bold => 1,
            HudFont::Mono => 2,
            HudFont::MonoBold => 3,
        }]
    }

    fn font(&self, f: HudFont) -> usize {
        self.fonts[match f {
            HudFont::Regular => 0,
            HudFont::Bold => 1,
            HudFont::Mono => 2,
            HudFont::MonoBold => 3,
        }]
    }

    /// Width of `s` in pixels.
    pub fn measure(&self, font: HudFont, size: f32, s: &str) -> f32 {
        self.text.measure(self.font(font), size, s)
    }

    /// Draws text with its baseline at `y`; `align` 0 = left, 1 = centre,
    /// 2 = right of `x`. Returns the width.
    pub fn text(&mut self, x: f32, y: f32, size: f32, font: HudFont, color: u32, align: u8, s: &str) -> f32 {
        let f = self.font(font);
        let w = self.text.measure(f, size, s);
        let x = match align {
            1 => x - w / 2.0,
            2 => x - w,
            _ => x,
        };
        self.text.draw(self.canvas, f, size, (x + 0.5) as i32 as f32, (y + 0.5) as i32 as f32, s, Color(color));
        w
    }

    /// Text with a soft dark shadow (readable over any scene).
    pub fn text_shadow(&mut self, x: f32, y: f32, size: f32, font: HudFont, color: u32, align: u8, s: &str) -> f32 {
        let off = (size / 14.0).max(1.0);
        let a = (color >> 24).min(255);
        let shadow = (a * 3 / 4) << 24;
        self.text(x + off, y + off, size, font, shadow, align, s);
        self.text(x, y, size, font, color, align, s)
    }

    /// Fills a rectangle (straight-alpha colour, blended).
    pub fn fill(&mut self, r: Rect, color: u32) {
        self.canvas.fill_rect(r, Color(color));
    }

    /// A rounded, translucent panel.
    pub fn panel(&mut self, r: Rect, radius: f32, color: u32) {
        self.canvas.fill_rounded_rect(r, radius, Color(color));
    }

    /// A rounded panel with a vertical gradient.
    pub fn panel_gradient(&mut self, r: Rect, radius: f32, top: u32, bottom: u32) {
        self.canvas.fill_rounded_rect_gradient(r, radius, Color(top), Color(bottom));
    }

    /// A rounded outline.
    pub fn outline(&mut self, r: Rect, radius: f32, width: f32, color: u32) {
        self.canvas.stroke_rounded_rect(r, radius, width, Color(color));
    }

    /// A filled circle.
    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: u32) {
        self.canvas.fill_circle(cx, cy, radius, Color(color));
    }

    /// An anti-aliased line.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: u32) {
        self.canvas.draw_line(x0, y0, x1, y1, width, Color(color));
    }

    /// Darkens (or tints) the whole screen with a translucent colour.
    /// Black uses a fast path (one multiply per channel pair per pixel).
    pub fn dim(&mut self, color: u32) {
        let a = color >> 24;
        if color & 0x00FF_FFFF != 0 {
            let r = Rect::new(0, 0, self.width, self.height);
            self.canvas.fill_rect(r, Color(color));
            return;
        }
        let k = 256 - (a + (a >> 7)); // keep 0..256
        let (w, h) = (self.width.max(0) as usize, self.height.max(0) as usize);
        let (pixels, stride) = self.canvas.pixels_mut();
        let stride = stride.max(0) as usize;
        for y in 0..h {
            let Some(row) = pixels.get_mut(y * stride..y * stride + w) else { break };
            for p in row {
                let v = *p;
                *p = ((((v & 0x00FF_00FF) * k) >> 8) & 0x00FF_00FF)
                    | ((((v & 0x0000_FF00) * k) >> 8) & 0x0000_FF00)
                    | (v & 0xFF00_0000);
            }
        }
    }
}

/// A game driven by [`run`].
pub trait Game {
    /// Input and simulation for one frame of `dt` seconds.
    fn update(&mut self, app: &mut AppState, dt: f32);
    /// Renders the 3D scene (calls [`Renderer::frame`]).
    fn render(&mut self, r: &mut Renderer);
    /// Draws the HUD over the presented image.
    fn hud(&mut self, hud: &mut Hud);
}

/// Window and rendering settings.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Window title.
    pub title: String,
    /// Application id given to the compositor.
    pub app_id: String,
    /// Initial window width in pixels.
    pub width: u32,
    /// Initial window height in pixels.
    pub height: u32,
    /// Internal resolution = window size / scale.
    pub scale: f32,
    /// Adapt the scale (in steps of [`SCALE_LEVELS`] between `min_scale`
    /// and `max_scale`) to keep the frame rate between `target_fps` and
    /// about 1.6 times that.
    pub dynamic_resolution: bool,
    /// Smallest scale (sharpest image) dynamic resolution may pick.
    pub min_scale: f32,
    /// Largest scale (cheapest image) dynamic resolution may pick.
    pub max_scale: f32,
    /// Frame rate below which dynamic resolution lowers the resolution.
    pub target_fps: f32,
    /// Log statistics with this prefix every five seconds.
    pub log_name: &'static str,
}

impl Settings {
    /// A 1024x640 window rendered at 1/1.6 of its size, with dynamic
    /// resolution between 1/1.6 and 1/3 aiming at 28 fps or more.
    pub fn new(title: &str, app_id: &str) -> Settings {
        Settings {
            title: title.into(),
            app_id: app_id.into(),
            width: 1024,
            height: 640,
            scale: 1.6,
            dynamic_resolution: true,
            min_scale: 1.6,
            max_scale: 3.0,
            target_fps: 28.0,
            log_name: "v3d",
        }
    }
}

/// Internal-resolution divisors used by dynamic resolution (2.0 has a fast
/// scaling path).
pub const SCALE_LEVELS: [f32; 6] = [1.0, 1.333, 1.6, 2.0, 2.5, 3.0];

/// Applies pending window events; returns false when the window was closed.
fn pump_events(window: &mut Window, app: &mut AppState, show_stats: &mut bool) -> bool {
    for ev in window.poll_events() {
        match ev {
            WindowEvent::Key { code, pressed, .. } => {
                let c = code as usize & 255;
                if pressed {
                    if !app.keys.down[c] {
                        app.keys.pressed[c] = true;
                    }
                    app.keys.down[c] = true;
                    if code == keys::F8 {
                        app.autopilot = !app.autopilot;
                        vrt::println!("autopilot {}", if app.autopilot { "on" } else { "off" });
                    }
                    if code == keys::F3 {
                        *show_stats = !*show_stats;
                    }
                } else {
                    app.keys.down[c] = false;
                    app.keys.released[c] = true;
                }
            }
            WindowEvent::Focus { focused } => {
                app.focused = focused;
                if !focused {
                    app.keys.clear();
                }
            }
            WindowEvent::CloseRequested {} => return false,
            _ => {}
        }
    }
    true
}

fn internal_size(w: i32, h: i32, scale: f32) -> (usize, usize) {
    let s = scale.max(1.0);
    (((w as f32 / s) as usize).max(16), ((h as f32 / s) as usize).max(16))
}

/// Opens the window and runs `game` until it quits or the window closes.
/// `make` builds the game with access to the renderer (for textures).
pub fn run<G: Game>(settings: Settings, make: impl FnOnce(&mut Renderer) -> G) -> i32 {
    let display = match connect() {
        Ok(d) => d,
        Err(e) => {
            vrt::println!("{}: cannot connect to the display: {:?}", settings.log_name, e);
            return 1;
        }
    };
    let mut spec = WindowSpec::new(&settings.title, settings.width, settings.height);
    spec.app_id = settings.app_id.clone();
    spec.min_width = 320;
    spec.min_height = 200;
    let mut window = match Window::new(&display, spec) {
        Ok(w) => w,
        Err(e) => {
            vrt::println!("{}: cannot create a window: {:?}", settings.log_name, e);
            return 1;
        }
    };
    let (mut text, fonts) = vui::load_fonts();
    let mut hud_cache = HudCache::default();
    let pool = ThreadPool::named("v3d-worker", vrt::pool::cpus().min(16), Some(vabi::priority::NORMAL - 2));
    vrt::println!("{}: rendering with {} threads", settings.log_name, pool.threads());
    let level_of = |s: f32| {
        let mut best = 0;
        for (i, l) in SCALE_LEVELS.iter().enumerate() {
            if (l - s).abs() < (SCALE_LEVELS[best] - s).abs() {
                best = i;
            }
        }
        best
    };
    let mut level = level_of(settings.scale);
    let (min_level, max_level) = (level_of(settings.min_scale), level_of(settings.max_scale).max(level));
    let mut scale = if settings.dynamic_resolution { SCALE_LEVELS[level] } else { settings.scale };
    let mut since_change = 0u32;
    let (iw, ih) = internal_size(window.width, window.height, scale);
    let mut renderer = Renderer::new(iw, ih, pool);
    renderer.set_clock(vrt::time::now_ns);
    let filter = renderer.calibrate();
    vrt::println!("{}: 2x scaling filter {:?}", settings.log_name, filter);
    // Building a game's world can take a while under emulation: show the
    // title and a loading note first.
    for _ in 0..2 {
        let _ = window.poll_events();
        if let Ok(mut canvas) = window.begin_frame() {
            let (cw, ch) = (canvas.width(), canvas.height());
            canvas.fill_vertical_gradient(Rect::new(0, 0, cw, ch), Color(0xFF141A26), Color(0xFF06080C));
            let bold = fonts[1];
            let tw = text.measure(bold, 40.0, &settings.title);
            text.draw(
                &mut canvas,
                bold,
                40.0,
                (cw as f32 - tw) / 2.0,
                ch as f32 / 2.0 - 10.0,
                &settings.title,
                Color(0xFFFFFFFF),
            );
            let lw = text.measure(fonts[0], 15.0, "Loading...");
            text.draw(
                &mut canvas,
                fonts[0],
                15.0,
                (cw as f32 - lw) / 2.0,
                ch as f32 / 2.0 + 24.0,
                "Loading...",
                Color(0xFF9AA6BC),
            );
            let _ = window.present(&[]);
        }
        // Wait for the compositor to show it.
        let deadline = vrt::time::now_ns() + 300_000_000;
        while !window.can_draw() && vrt::time::now_ns() < deadline {
            let mut items = [WaitItem {
                handle: window.event_channel().raw(),
                signals: signals::READABLE | signals::PEER_CLOSED,
                ..Default::default()
            }];
            let _ = vrt::object::wait_many(&mut items, vrt::time::now_ns() + 20_000_000);
            let _ = window.poll_events();
        }
    }
    let t_make = vrt::time::now_ns();
    let mut game = make(&mut renderer);
    vrt::println!("{}: ready in {} ms", settings.log_name, (vrt::time::now_ns() - t_make) / 1_000_000);
    let mut app = AppState {
        keys: Keys::new(),
        time: 0.0,
        width: window.width,
        height: window.height,
        focused: true,
        quit: false,
        fps: 0.0,
        autopilot: false,
    };
    let start = vrt::time::now_ns();
    let mut last = start;
    let mut show_stats = false;
    // Frame-rate measurement.
    let mut fps_frames = 0u32;
    let mut fps_since = start;
    let mut log_since = start;
    // update, geometry, raster, present, hud, work (all but waiting), wait
    let mut acc = [0u64; 7];
    let mut acc_frames = 0u64;
    let mut last_ms = [0.0f32; 7];
    loop {
        if !pump_events(&mut window, &mut app, &mut show_stats) || window.closed || app.quit {
            return 0;
        }
        let now = vrt::time::now_ns();
        let dt = ((now - last) as f64 / 1e9).clamp(0.0, 0.1) as f32;
        last = now;
        app.time = (now - start) as f64 / 1e9;
        app.width = window.width;
        app.height = window.height;

        game.update(&mut app, dt);
        app.keys.end_frame();
        let t_update = vrt::time::now_ns();

        let (iw, ih) = internal_size(window.width, window.height, scale);
        if iw != renderer.width() || ih != renderer.height() {
            renderer.resize(iw, ih);
        }
        game.render(&mut renderer);
        let st = renderer.stats();
        let t_rendered = vrt::time::now_ns();

        // The 3D image of this frame was rendered while the compositor was
        // still showing the previous one; now wait until it is done with it.
        while !window.can_draw() {
            let mut items = [WaitItem {
                handle: window.event_channel().raw(),
                signals: signals::READABLE | signals::PEER_CLOSED,
                ..Default::default()
            }];
            let _ = vrt::object::wait_many(&mut items, vrt::time::now_ns() + 50_000_000);
            if !pump_events(&mut window, &mut app, &mut show_stats) || window.closed {
                return 0;
            }
        }
        let t_render = vrt::time::now_ns();

        let Ok(mut canvas) = window.begin_frame() else { continue };
        let (cw, ch) = (canvas.width(), canvas.height());
        {
            let (pixels, stride) = canvas.pixels_mut();
            renderer.present(pixels, stride as usize, cw as usize, ch as usize);
        }
        let t_present = vrt::time::now_ns();
        let mut hud = Hud { canvas: &mut canvas, text: &mut text, fonts, width: cw, height: ch, cache: &mut hud_cache };
        game.hud(&mut hud);
        if show_stats {
            let lines = [
                format!("{:.1} fps  work {:.1} ms  wait {:.1} ms", app.fps, last_ms[5], last_ms[6]),
                format!("update {:.1}  geometry {:.1}  raster {:.1}", last_ms[0], last_ms[1], last_ms[2]),
                format!("present {:.1}  hud {:.1} ms  ({:?})", last_ms[3], last_ms[4], renderer.upscale_2x()),
                format!(
                    "{}x{} -> {}x{}  {} threads",
                    renderer.width(),
                    renderer.height(),
                    cw,
                    ch,
                    renderer.pool().threads()
                ),
                format!(
                    "draws {} (culled {})  tris {}/{}  clipped {}",
                    st.draws, st.culled, st.rasterized, st.triangles, st.clipped
                ),
                format!("{} pixels in {} spans", st.pixels, st.spans),
            ];
            let h = 20 * lines.len() as i32 + 14;
            // Bottom centre, where games keep the HUD clear.
            let (px, py) = ((cw - 380) / 2, ch - h - 56);
            hud.panel(Rect::new(px, py, 380, h), 8.0, 0xC010_1418);
            for (i, l) in lines.iter().enumerate() {
                hud.text((px + 10) as f32, (py + 20) as f32 + 20.0 * i as f32, 13.0, HudFont::Mono, 0xFFE8F0F8, 0, l);
            }
        }
        let t_hud = vrt::time::now_ns();
        let _ = window.present(&[]);

        // Statistics.
        let wait = t_render - t_rendered;
        let times = [
            t_update - now,
            st.geometry_ns,
            st.raster_ns,
            t_present - t_render,
            t_hud - t_present,
            t_hud - now - wait,
            wait,
        ];
        for (a, t) in acc.iter_mut().zip(times) {
            *a += t;
        }
        acc_frames += 1;
        fps_frames += 1;
        if now - fps_since >= 1_000_000_000 {
            let secs = (now - fps_since) as f32 / 1e9;
            app.fps = fps_frames as f32 / secs;
            for (l, a) in last_ms.iter_mut().zip(acc) {
                *l = (a as f64 / acc_frames.max(1) as f64 / 1e6) as f32;
            }
            if settings.dynamic_resolution {
                // Change at most every two seconds, with hysteresis.
                since_change += 1;
                if since_change >= 2 {
                    let old = level;
                    if app.fps < settings.target_fps && level < max_level {
                        level += 1;
                    } else if app.fps > settings.target_fps * 1.6 && level > min_level {
                        level -= 1;
                    }
                    if level != old {
                        since_change = 0;
                        scale = SCALE_LEVELS[level];
                        vrt::println!("{}: {:.1} fps, render scale now 1/{}", settings.log_name, app.fps, scale);
                    }
                }
            }
            if now - log_since >= 5_000_000_000 {
                log_since = now;
                vrt::println!(
                    "{}: {:.1} fps, work {:.1} ms (update {:.1}, geometry {:.1}, raster {:.1}, present {:.1}, hud {:.1}), wait {:.1} ms, {}x{}, {} tris, {} draws, {} px, {} spans, raster cpu {:.1} ms, busy {:?} us, delay {:?} us",
                    settings.log_name,
                    app.fps,
                    last_ms[5],
                    last_ms[0],
                    last_ms[1],
                    last_ms[2],
                    last_ms[3],
                    last_ms[4],
                    last_ms[6],
                    renderer.width(),
                    renderer.height(),
                    st.rasterized,
                    st.draws - st.culled,
                    st.pixels,
                    st.spans,
                    st.raster_cpu_ns as f64 / 1e6,
                    &st.raster_busy_us[..renderer.pool().threads().min(8)],
                    &st.raster_delay_us[..renderer.pool().threads().min(8)]
                );
            }
            acc = [0; 7];
            acc_frames = 0;
            fps_frames = 0;
            fps_since = now;
        }
    }
}
