//! The startup sequence.
//!
//! The boot loader's splash (the gradient and the ring, `vsplash`) is on
//! the screen when the window system starts, and stays as it is while the
//! system starts its drivers: nothing is drawn until the screen frames go
//! to from then on has it (`Screen::settled`). Where a driver is coming for
//! the display (the driver VM's Linux's), that is once the driver shows its
//! first picture, the splash as the loader painted it; otherwise the
//! firmware's framebuffer, at once. So the splash never moves on a screen a
//! driver is about to take over (which would freeze it), and a display
//! that has to be set up anew is set up under a still picture.
//!
//! Then the window system brings the splash to life, without a seam (its
//! first frame is the loader's, pixel for pixel): a soft light gathers
//! around the ring and breathes, a brighter glint going round it while the
//! system works, and the name and the tagline rise into view below. Once
//! the desktop has drawn itself, the splash has been seen long enough not
//! to be a flash, and the shell has what comes with the desktop's first
//! appearance (its startup sound, `display::desktop_ready`), the ring
//! swells and fades, the words lift away and the desktop dissolves in, the
//! shell told as it does (`WindowEvent::Appearing`): the sound starts with
//! it.
//!
//! Until then windows are composed as usual into the back buffer, but the
//! screen shows the splash layer, redrawn where it moves. During the
//! dissolve every frame mixes the splash layer with the composed screen.
//! Where the GPU composes (`gpu`), it draws the splash itself, from the
//! same look: the gradient, the light and the ring in a shader, the words
//! as textures, over the desktop as it comes in.
//! A compositor restarted after a crash shows the desktop straight away:
//! init asks for the sequence (`splash`) only when the system starts.

use alloc::vec::Vec;
use core::f32::consts::PI;

use alloc::string::String;

use vgfx::{Bitmap, Canvas, Color, Rect, Text};
use vmath::FloatExt;
use vrt::println;
use vsplash::Ring;

use crate::gpu::{Gpu, Slot, Splash};
use crate::screen::Screen;

const MS: u64 = 1_000_000;
/// The loader's splash waits this long at most for the screen frames go
/// to (a driver that is coming but does not show its first picture): then
/// the sequence goes on, on whatever screen there is.
const HOLD: u64 = 20_000 * MS;
/// The light gathers around the ring; then the name, then the tagline,
/// rise into view.
const GLOW_IN: (u64, u64) = (0, 600 * MS);
const NAME_IN: (u64, u64) = (150 * MS, 750 * MS);
const TAGLINE_IN: (u64, u64) = (350 * MS, 950 * MS);
/// How far the words rise.
const RISE: f32 = 12.0;
/// The splash is seen at least this long, so that it never flashes by and
/// the tagline can be read.
const MIN_SPLASH: u64 = 1800 * MS;
/// Without a desktop after this long, the screen is shown as it is.
const MAX_SPLASH: u64 = 20_000 * MS;
/// Once the desktop has drawn itself and the splash has been seen long
/// enough, how long the shell may take still to have what comes with the
/// desktop (its startup sound, waiting for the sound card's driver).
const SHELL_WAIT: u64 = 3_000 * MS;
/// The dissolve, and its parts: the words lift away, the light swells and
/// fades, the ring swells and fades, the desktop comes in.
const REVEAL: u64 = 950 * MS;
const WORDS_OUT: (u64, u64) = (0, 400 * MS);
const LIFT: f32 = 8.0;
const GLOW_SWELL: (u64, u64) = (0, 250 * MS);
const GLOW_OUT: (u64, u64) = (150 * MS, 600 * MS);
const RING_GROW: (u64, u64) = (0, 600 * MS);
const RING_OUT: (u64, u64) = (100 * MS, 600 * MS);
const DESKTOP_IN: (u64, u64) = (200 * MS, REVEAL);
/// One breath of the light, and one turn of its glint.
const BREATH: u64 = 2600 * MS;
const ORBIT: u64 = 2000 * MS;

const NAME: &str = "Veda";
const TAGLINE: &str = "The agentic-native operating system";
const NAME_COLOR: Color = Color::hex(0xF4_F7FF);
const TAGLINE_COLOR: Color = Color::hex(0xA3_AECB);
/// The light's colour, and its strength at the ring's edges.
const GLOW: u32 = 0x8E_B1FF;
const GLOW_PEAK: f32 = 0.30;
/// How far the light reaches beyond the ring and into it (of the ring's
/// outer and inner radius), how much brighter its glint is, and how
/// narrow (a power of a cosine).
const REACH_OUT: f32 = 0.95;
const REACH_IN: f32 = 0.55;
const GLINT: f32 = 0.6;
const GLINT_POWER: i32 = 8;
/// How much the ring swells as it fades.
const RING_SWELL: f32 = 0.22;

/// `t`'s progress through `(from, to)`, 0 to 1.
fn progress(t: u64, (from, to): (u64, u64)) -> f32 {
    if t <= from {
        0.0
    } else if t >= to {
        1.0
    } else {
        (t - from) as f32 / (to - from) as f32
    }
}

fn ease_out(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

fn ease_in(p: f32) -> f32 {
    p * p * p
}

fn ease_in_out(p: f32) -> f32 {
    if p < 0.5 { 4.0 * p * p * p } else { 1.0 - (2.0 - 2.0 * p).powi(3) / 2.0 }
}

/// `r` without `hole` (inside it, or empty): up to four rectangles, those
/// above and below it across `r`, and those beside it.
fn outside(r: Rect, hole: Rect) -> [Rect; 4] {
    if hole.is_empty() {
        return [r, Rect::default(), Rect::default(), Rect::default()];
    }
    [
        Rect::new(r.x, r.y, r.w, hole.y - r.y),
        Rect::new(r.x, hole.bottom(), r.w, r.bottom() - hole.bottom()),
        Rect::new(r.x, hole.y, hole.x - r.x, hole.h),
        Rect::new(hole.right(), hole.y, r.right() - hole.right(), hole.h),
    ]
}

/// `0xRRGGBB` pixels (`vsplash`'s) as the layer keeps them, opaque.
fn opaque(line: &mut [u32]) {
    for px in line {
        *px |= 0xFF00_0000;
    }
}

/// How far `t` is through its cycle of `period`, 0 to 1.
fn cycle(t: u64, period: u64) -> f32 {
    (t % period) as f32 / period as f32
}

/// How the splash looks at one moment.
struct Look {
    /// The light's strength (1: as it settles).
    glow: f32,
    /// Where its glint is, in turns clockwise from the top.
    glint: f32,
    /// The ring's size (1: the loader's) and opacity.
    scale: f32,
    ring: f32,
    /// The words' opacity and how far below their place they are.
    name: (f32, f32),
    tagline: (f32, f32),
}

/// A line of text drawn once, and where its top-left corner is at rest.
struct Words {
    bitmap: Bitmap,
    x: i32,
    y: i32,
}

impl Words {
    /// `bitmap` centred on `cx`, with its top at `top`.
    fn at(bitmap: Bitmap, cx: i32, top: i32) -> Words {
        Words { x: cx - bitmap.width / 2, y: top, bitmap }
    }

    fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.bitmap.width, self.bitmap.height)
    }
}

/// `line` in `font` at `size`, its letters `tracking` pixels further apart
/// than their advance, on a transparent bitmap that just holds it.
fn draw_line(t: &mut Text, (font, size): (usize, f32), line: &str, color: Color, tracking: f32) -> Bitmap {
    let m = t.metrics(font, size);
    let width = t.measure(font, size, line) + tracking * line.chars().count().saturating_sub(1) as f32;
    let pad = 2;
    let (w, h) = (width.ceil() as i32 + 2 * pad, (m.ascent + m.descent.abs()).ceil() as i32 + 2 * pad);
    let mut bitmap = Bitmap::new(w, h);
    let mut c = Canvas::for_bitmap(&mut bitmap);
    let (mut x, mut buf) = (pad as f32, [0u8; 4]);
    for ch in line.chars() {
        x += t.draw(&mut c, font, size, x, pad as f32 + m.ascent, ch.encode_utf8(&mut buf), color) + tracking;
    }
    bitmap
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The loader's splash is on the screen as it is: nothing is drawn.
    Hold,
    /// Coming to life (since `started`).
    Splash,
    /// Dissolving into the desktop since then.
    Reveal(u64),
}

/// What the sequence waits for, as it is now.
#[derive(Clone, Copy)]
pub(crate) struct Progress {
    /// The screen frames go to from now on has the splash
    /// (`Screen::settled`).
    pub(crate) settled: bool,
    /// The desktop has drawn itself.
    pub(crate) drawn: bool,
}

/// How the sequence moved on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moved {
    No,
    /// It came to life.
    Started,
    /// The desktop began to appear.
    Appearing,
    /// It is over (logged): the composed screen is to be shown as it is
    /// from now on.
    Over,
}

pub(crate) struct Startup {
    /// When the sequence was made, and when it came to life.
    created: u64,
    started: u64,
    phase: Phase,
    /// The shell has what comes with the desktop's appearance.
    cleared: bool,
    /// Since when only that has been waited for.
    waiting_for_shell: Option<u64>,
    /// The splash as the screen shows it.
    layer: Bitmap,
    ring: Ring,
    /// The box the light and the swollen ring fill; for each of its pixels,
    /// its distance from the ring's centre (in quarters of a pixel) and its
    /// direction (in 256ths of a turn, clockwise from the top).
    around: Rect,
    distance: Vec<u16>,
    direction: Vec<u8>,
    /// The ring at rest: its coverage of each pixel of the box (of 256).
    rest: Vec<u16>,
    name: Option<Words>,
    tagline: Option<Words>,
    /// What the animation redraws.
    region: Rect,
    frames: u32,
}

impl Startup {
    /// The sequence on a `width` x `height` screen, the name in font
    /// `fonts.0` and the tagline in `fonts.1` (if there are fonts).
    pub(crate) fn new(width: i32, height: i32, text: &mut Text, fonts: (usize, usize), now: u64) -> Startup {
        let (w, h) = (width as u32, height as u32);
        let screen = Rect::new(0, 0, width, height);
        let mut layer = Bitmap::new(width, height);
        for (y, line) in layer.pixels.chunks_exact_mut(w as usize).enumerate() {
            vsplash::row(w, h, y as u32, line);
            opaque(line);
        }
        let ring = Ring::place(w, h);
        let (cx, cy, radius) = (ring.cx as f32 / 16.0, ring.cy as f32 / 16.0, ring.outer as f32 / 16.0);

        let reach = (radius * (1.0 + RING_SWELL) * (1.0 + REACH_OUT)).ceil() as i32 + 2;
        let around = Rect::new(cx as i32 - reach, cy as i32 - reach, 2 * reach, 2 * reach).intersect(&screen);
        let mut distance = Vec::with_capacity((around.w * around.h) as usize);
        let mut direction = Vec::with_capacity((around.w * around.h) as usize);
        let mut rest = Vec::with_capacity((around.w * around.h) as usize);
        for y in around.y..around.bottom() {
            for x in around.x..around.right() {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                distance.push(((dx * dx + dy * dy).sqrt() * 4.0).min(65_535.0) as u16);
                let turn = dx.atan2(-dy) / (2.0 * PI);
                direction.push(((turn + 1.0) * 256.0) as i32 as u8);
                rest.push(ring.coverage(x, y) as u16);
            }
        }

        let (name, tagline) = if text.font_count() > 0 {
            let name_size = (radius * 0.5).max(22.0);
            let tagline_size = (radius * 0.2).max(13.0);
            let name = draw_line(text, (fonts.0, name_size), NAME, NAME_COLOR, name_size * 0.06);
            let name = Words::at(name, cx as i32, (cy + radius * 1.5) as i32);
            let below = name.rect().bottom() + (tagline_size * 0.5) as i32;
            let tagline = draw_line(text, (fonts.1, tagline_size), TAGLINE, TAGLINE_COLOR, 0.0);
            (Some(name), Some(Words::at(tagline, cx as i32, below)))
        } else {
            (None, None)
        };
        // The words' paths: from below their place up past it.
        let mut region = around;
        for words in name.iter().chain(tagline.iter()) {
            let r = words.rect();
            region = region.union(&Rect::new(r.x, r.y - LIFT as i32 - 1, r.w, r.h + (RISE + LIFT) as i32 + 2));
        }
        Startup {
            created: now,
            started: now,
            phase: Phase::Hold,
            cleared: false,
            waiting_for_shell: None,
            layer,
            ring,
            around,
            distance,
            direction,
            rest,
            name,
            tagline,
            region: region.intersect(&screen),
            frames: 0,
        }
    }

    /// The splash as the screen shows it (during the dissolve, under the
    /// desktop).
    pub(crate) fn layer(&self) -> &Bitmap {
        &self.layer
    }

    /// Moves the sequence on to `now` as `progress` lets it.
    pub(crate) fn advance(&mut self, now: u64, progress: Progress, how: &dyn Fn() -> String) -> Moved {
        match self.phase {
            Phase::Hold => {
                if !self.release(now, progress.settled) {
                    return Moved::No;
                }
                Moved::Started
            }
            Phase::Splash => {
                let t = now.saturating_sub(self.started);
                let ready = progress.drawn && t >= MIN_SPLASH;
                if ready {
                    self.waiting_for_shell.get_or_insert(now);
                }
                let waited = self.waiting_for_shell.is_some_and(|since| now >= since + SHELL_WAIT);
                if !((ready && (self.cleared || waited)) || t >= MAX_SPLASH) {
                    return Moved::No;
                }
                if !progress.drawn {
                    println!("no desktop after {} s; showing the screen as it is", t / 1_000_000_000);
                } else if !self.cleared {
                    println!("the shell did not say the desktop may appear; it appears all the same");
                }
                println!("the desktop appears ({} ms after the splash came to life)", t / MS);
                self.phase = Phase::Reveal(now);
                Moved::Appearing
            }
            Phase::Reveal(start) => {
                if now.saturating_sub(start) < REVEAL {
                    return Moved::No;
                }
                let t = now.saturating_sub(self.started);
                println!(
                    "desktop shown after {} ms ({} ms held; {} frames, {} a second; {})",
                    now.saturating_sub(self.created) / MS,
                    self.started.saturating_sub(self.created) / MS,
                    self.frames,
                    self.frames as u64 * 1_000_000_000 / t.max(1),
                    how()
                );
                Moved::Over
            }
        }
    }

    /// Brings the held splash to life once the screen frames go to from
    /// now on has it (`settled`), or after [`HOLD`] all the same: whether
    /// it did.
    pub(crate) fn release(&mut self, now: u64, settled: bool) -> bool {
        if self.phase != Phase::Hold {
            return false;
        }
        if !settled && now < self.created + HOLD {
            return false;
        }
        if settled {
            println!("the splash comes to life (held {} ms)", now.saturating_sub(self.created) / MS);
        } else {
            println!(
                "the display's driver did not show its first picture in {} s; the splash comes to life",
                HOLD / 1_000_000_000
            );
        }
        self.phase = Phase::Splash;
        self.started = now;
        true
    }

    /// Whether the loader's splash is held, as it is.
    pub(crate) fn holding(&self) -> bool {
        self.phase == Phase::Hold
    }

    /// When the hold ends without the screen, while it holds.
    pub(crate) fn deadline(&self) -> Option<u64> {
        self.holding().then_some(self.created + HOLD)
    }

    /// The shell has what comes with the desktop's appearance.
    pub(crate) fn clear(&mut self) {
        self.cleared = true;
    }

    /// How the splash looks at `now`, and how far the desktop has come in
    /// (0 to 1).
    fn look(&self, now: u64) -> (Look, f32) {
        let t = now.saturating_sub(self.started);
        match self.phase {
            Phase::Hold => (self.splash(0), 0.0),
            Phase::Splash => (self.splash(t), 0.0),
            Phase::Reveal(start) => {
                let r = now.saturating_sub(start);
                (self.reveal(t, r), ease_in_out(progress(r, DESKTOP_IN)))
            }
        }
    }

    /// Whether the desktop is coming in: frames show it (under the splash),
    /// all of the screen at a time.
    pub(crate) fn revealing(&self) -> bool {
        matches!(self.phase, Phase::Reveal(_))
    }

    /// What the splash redraws while the desktop is not coming in.
    pub(crate) fn region(&self) -> Rect {
        self.region
    }

    /// Draws the sequence's frame at `now` on the screen: nothing while the
    /// loader's splash is held (a driver's first picture is the layer, as
    /// it is, where it is stale).
    pub(crate) fn frame(&mut self, screen: &mut Screen, now: u64) {
        if self.holding() {
            return;
        }
        let (look, desktop) = self.look(now);
        self.paint(&look);
        let alpha = (desktop * 256.0) as u32;
        if alpha == 0 {
            screen.flush_from(&self.layer, self.region);
        } else {
            let all = screen.rect();
            screen.flush_mixed(&self.layer, alpha, all);
        }
        self.frames += 1;
    }

    /// Draws the sequence's frame at `now` with the GPU, within `clip`:
    /// the splash, over the desktop the caller drew while it comes in.
    pub(crate) fn frame_gpu(&mut self, g: &mut Gpu, clip: Rect, now: u64) {
        let (look, desktop) = self.look(now);
        let ring = self.ring.scaled((look.scale * 256.0) as i32, 256);
        let above = 1.0 - desktop;
        let splash = Splash {
            top: vsplash::TOP,
            bottom: vsplash::BOTTOM,
            centre: (ring.cx as f32 / 16.0, ring.cy as f32 / 16.0),
            outer: ring.outer as f32 / 16.0,
            inner: ring.inner as f32 / 16.0,
            ring: look.ring,
            strength: look.glow * GLOW_PEAK,
            glow: GLOW,
            glint: look.glint,
            opacity: above,
        };
        // Where the light reaches, the gradient, the light and the ring; the
        // gradient alone everywhere else (each pixel drawn once: as the
        // splash dissolves, it is mixed with the desktop once).
        let screen = Rect::new(0, 0, self.layer.width, self.layer.height).intersect(&clip);
        let lit = self.around.intersect(&screen);
        for r in outside(screen, lit) {
            g.splash_rows(r, &splash);
        }
        g.splash(lit, &splash);
        for (i, (words, (opacity, dy))) in
            [(&self.name, look.name), (&self.tagline, look.tagline)].into_iter().enumerate()
        {
            let Some(words) = words else { continue };
            let slot = Slot::Words(i as u8);
            let t = match g.cached(slot, 1) {
                Some(t) => t,
                None => g.upload(slot, 1, &words.bitmap),
            };
            let at = words.rect().translate(0, dy.round() as i32);
            g.image(t, Rect::new(0, 0, t.w, t.h), at, opacity * above, false, None);
        }
        self.frames += 1;
    }

    /// While the system starts.
    fn splash(&self, t: u64) -> Look {
        let breath = 0.78 + 0.22 * (2.0 * PI * cycle(t, BREATH)).sin();
        let rise = |span| {
            let p = ease_out(progress(t, span));
            (p, RISE * (1.0 - p))
        };
        Look {
            glow: ease_out(progress(t, GLOW_IN)) * breath,
            glint: cycle(t, ORBIT),
            scale: 1.0,
            ring: 1.0,
            name: rise(NAME_IN),
            tagline: rise(TAGLINE_IN),
        }
    }

    /// `r` into the dissolve.
    fn reveal(&self, t: u64, r: u64) -> Look {
        let at_rest = self.splash(t);
        let words = ease_in_out(progress(r, WORDS_OUT));
        let away = |(opacity, dy): (f32, f32)| (opacity * (1.0 - words), dy - LIFT * words);
        let swell = 1.0 + 0.35 * (PI * progress(r, GLOW_SWELL)).sin();
        Look {
            glow: at_rest.glow * swell * (1.0 - ease_in(progress(r, GLOW_OUT))),
            glint: at_rest.glint,
            scale: 1.0 + RING_SWELL * ease_in_out(progress(r, RING_GROW)),
            ring: 1.0 - ease_in(progress(r, RING_OUT)),
            name: away(at_rest.name),
            tagline: away(at_rest.tagline),
        }
    }

    /// Redraws the animated part of the splash layer as `look` has it.
    fn paint(&mut self, look: &Look) {
        let (w, h) = (self.layer.width, self.layer.height);
        let ring = self.ring.scaled((look.scale * 256.0) as i32, 256);
        let (outer, inner) = (ring.outer as f32 / 16.0, ring.inner as f32 / 16.0);
        // The light by distance from the centre (in quarter pixels, of 256)
        // and its glint by direction (a factor, of 256).
        let strength = look.glow * GLOW_PEAK * 256.0;
        let reach = self.distance.iter().copied().max().unwrap_or(0) as usize;
        let light: Vec<u16> = (0..=reach)
            .map(|q| {
                let d = q as f32 / 4.0;
                let falloff = if d >= outer {
                    1.0 - (d - outer) / (REACH_OUT * outer)
                } else if d <= inner {
                    1.0 - (inner - d) / (REACH_IN * inner)
                } else {
                    1.0
                };
                (falloff.max(0.0).powi(2) * strength) as u16
            })
            .collect();
        let mut glint = [0u16; 256];
        for (a, g) in glint.iter_mut().enumerate() {
            let away = 2.0 * PI * (a as f32 / 256.0 - look.glint);
            *g = (256.0 * (1.0 + GLINT * ((1.0 + away.cos()) / 2.0).powi(GLINT_POWER))) as u16;
        }
        let ring_alpha = (look.ring * 256.0) as u32;
        // The ring changes size only as it dissolves.
        let at_rest = ring == self.ring;
        let around = self.around;
        let region = self.region;
        for y in region.y..region.bottom() {
            let start = (y * w) as usize;
            let line = &mut self.layer.pixels[start + region.x as usize..start + region.right() as usize];
            vsplash::background(y as u32, h as u32, region.x as u32, line);
            opaque(line);
            if y < around.y || y >= around.bottom() {
                continue;
            }
            let (x0, x1) = (around.x.max(region.x), around.right().min(region.right()));
            let lut = ((y - around.y) * around.w) as usize;
            for x in x0..x1 {
                let i = lut + (x - around.x) as usize;
                let lit = (light[self.distance[i] as usize] as u32 * glint[self.direction[i] as usize] as u32) >> 8;
                let row = line[(x - region.x) as usize] & 0xFF_FFFF;
                let mut c = if lit > 0 { vsplash::mix(row, GLOW, lit.min(256)) } else { row };
                let ring_cover = if at_rest { u32::from(self.rest[i]) } else { ring.coverage(x, y) };
                let cover = (ring_cover * ring_alpha) >> 8;
                if cover > 0 {
                    c = vsplash::mix(c, vsplash::RING, cover);
                }
                line[(x - region.x) as usize] = 0xFF00_0000 | c;
            }
        }
        let mut canvas = Canvas::for_bitmap(&mut self.layer);
        canvas.clip_to(region);
        for (words, (opacity, dy)) in [(&self.name, look.name), (&self.tagline, look.tagline)] {
            if let Some(words) = words
                && opacity > 0.0
            {
                canvas.draw_bitmap(&words.bitmap, words.x, words.y + dy.round() as i32, (opacity * 255.0) as u8);
            }
        }
    }
}
