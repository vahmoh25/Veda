//! Composition: damaged regions are redrawn bottom to top, by the GPU
//! straight into the picture the display shows next (`gpu`) where it can,
//! or by the processor into the back buffer, which is then copied to the
//! screen (`screen`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vgfx::{Canvas, Damage, Rect};
use vproto::display::WindowEvent;
use vrt::println;

use crate::decor;
use crate::gpu::{Gpu, Slot};
use crate::state::{Compositor, Drag};
use crate::window::{AnimKind, Window};

/// How long the GPU may take to draw a frame before it counts as stopped.
const GPU_TIMEOUT_NS: u64 = 2_000_000_000;
/// How long handing a frame to the GPU may take before it is logged, and
/// how many frames took that long.
const SLOW_FRAME_NS: u64 = 50_000_000;
static SLOW_FRAMES: AtomicU32 = AtomicU32::new(0);

/// How a window is drawn at `now`: its opacity, and how far below its
/// place (while it animates).
fn animation(w: &Window, now: u64) -> (f32, i32) {
    match w.anim {
        Some(a) => {
            let p = a.progress(now);
            match a.kind {
                AnimKind::Open | AnimKind::Restore => (p, ((1.0 - p) * 14.0) as i32),
                AnimKind::Close => (1.0 - p, (p * 10.0) as i32),
                AnimKind::Minimize => (1.0 - p, (p * 40.0) as i32),
            }
        }
        None => (1.0, 0),
    }
}

impl Compositor {
    // ---- rendering ---------------------------------------------------------

    pub(crate) fn animating(&self) -> bool {
        self.startup.is_some() || self.windows.values().any(|w| w.anim.is_some())
    }

    /// Whether a frame is to be composed (when the screen is ready for one).
    pub(crate) fn wants_frame(&self) -> bool {
        !self.damage.is_empty() || self.animating() || self.screen.needs_frame() || self.gpu_ready.is_some()
    }

    pub(crate) fn composite(&mut self) {
        let real = vrt::time::now_ns();
        // Animations as they are when the frame is seen.
        let now = self.screen.frame_time(real);
        // Advance animations.
        let mut finished_close = Vec::new();
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for id in ids {
            let w = self.windows.get_mut(&id).unwrap();
            if let Some(a) = w.anim {
                let r = w.paint_bounds();
                self.damage.add(r.inflate(4));
                if a.done(now) {
                    w.anim = None;
                    if a.kind == AnimKind::Close {
                        finished_close.push(id);
                    }
                }
            }
        }
        for id in finished_close {
            self.destroy_window(id);
        }
        self.take_gpu();
        if self.gpu.is_some() && !self.screen.flipping() {
            self.drop_gpu("the display no longer flips");
        }
        if self.gpu.is_some() {
            self.composite_gpu(now, real);
        } else {
            self.composite_cpu(now, real);
        }
        // Clients may now draw their next frame.
        let owed: Vec<(u32, u8)> =
            self.windows.iter_mut().filter_map(|(id, w)| w.frame_owed.take().map(|b| (*id, b))).collect();
        for (id, b) in owed {
            self.send(id, WindowEvent::FrameDone { shown: b });
        }
        self.last_frame = real;
    }

    /// Where frames go, and (where a driver flips them) who draws them, for
    /// the log.
    fn describe(&self) -> String {
        match &self.gpu {
            Some(g) => format!("{}; drawn by the GPU ({})", self.screen.describe(), g.renderer),
            None if self.screen.flipping() => format!("{}; drawn by the processor", self.screen.describe()),
            None => self.screen.describe(),
        }
    }

    // ---- by the processor ----------------------------------------------------

    fn composite_cpu(&mut self, now: u64, real: u64) {
        let screen = self.screen_rect();
        let order = self.paint_order();
        let rects = self.damage.take();
        self.screen.begin_frame(self.startup.as_ref().map(|s| s.layer()));
        // During the startup sequence the screen shows it instead.
        let shown = self.startup.is_none();
        for r in rects {
            let r = r.intersect(&screen);
            if r.is_empty() {
                continue;
            }
            let (w, h) = (self.screen.back.width, self.screen.back.height);
            let mut c = Canvas::new(&mut self.screen.back.pixels, w, h, w);
            c.clip_to(r);
            // Drawing starts at the topmost window that hides all of `r`:
            // nothing below it (the desktop under a game, say) is visible.
            let covering =
                order.iter().rposition(|id| self.windows.get(id).is_some_and(|w| w.opaque_rect().contains_rect(&r)));
            if covering.is_none() {
                decor::draw_background(&mut c, screen);
            }
            for id in &order[covering.unwrap_or(0)..] {
                // The snap preview goes just below the window being dragged.
                if let (Some((_, preview)), Some(Drag::Move { id: dragged, .. })) = (self.snap, self.drag)
                    && *id == dragged
                    && preview.inflate(8).intersects(&r)
                {
                    decor::draw_snap_preview(&mut c, preview);
                }
                let win = &self.windows[id];
                if !win.visible() || !win.paint_bounds().intersects(&r) {
                    continue;
                }
                let (opacity, dy) = animation(win, now);
                self.decor.draw_window(&mut c, win, self.focused == Some(*id), self.decor_state, opacity, dy);
            }
            if let Some(s) = &self.switcher
                && s.bounds().intersects(&r)
            {
                let titles: Vec<&str> =
                    s.windows.iter().map(|id| self.windows.get(id).map_or("", |w| w.title.as_str())).collect();
                self.decor.draw_switcher(&mut c, s, &titles);
            }
            // Cursor on top.
            if self.cursor_rect.intersects(&r)
                && let Some((b, hx, hy)) = self.decor.cursor(self.cursor_shape)
            {
                c.draw_bitmap(b, self.pointer.0 - hx, self.pointer.1 - hy, 255);
            }
            drop(c);
            if shown {
                self.screen.flush(r);
            }
        }
        // The GPU's copies of the windows, if it composes again, are made
        // anew.
        for w in self.windows.values_mut() {
            w.upload.clear();
            w.upload_all = true;
        }
        if self.startup.is_some() {
            let ready = self.desktop_ready();
            let how = self.describe();
            if let Some(s) = &mut self.startup
                && !s.frame(&mut self.screen, now, ready, &|| how.clone())
            {
                // Over: from now on the composed screen, as it is.
                self.startup = None;
                self.screen.flush(screen);
            }
        }
        self.screen.end_frame(real);
    }

    // ---- by the GPU ------------------------------------------------------------

    /// Composes a frame with the GPU into the picture the display shows
    /// next: what changed, what that picture lacks (it was not on the
    /// screen while frames went into the others), and during the startup
    /// sequence what of it moves, or (as the desktop comes in) everything.
    /// The picture is asked for once the GPU has drawn it.
    fn composite_gpu(&mut self, now: u64, real: u64) {
        let screen = self.screen_rect();
        let Some((target, shown, stale)) = self.screen.gpu_frame() else { return };
        let mut region = Damage::new();
        for r in self.damage.take() {
            region.add(r.intersect(&screen));
        }
        // What the picture lacks of the screen is copied from the picture
        // that shows it, or (while the firmware's is shown) drawn.
        if shown.is_none() {
            for &r in &stale {
                region.add(r.intersect(&screen));
            }
        }
        let startup = self.startup.as_ref().map(|s| (s.revealing(), s.region()));
        match startup {
            Some((true, _)) => region.add(screen),
            Some((false, moving)) => region.add(moving),
            None => {}
        }
        let clip = region.rects().iter().fold(Rect::default(), |a, r| if a.is_empty() { *r } else { a.union(r) });
        let desktop = startup.is_none_or(|(revealing, _)| revealing);
        let how = self.describe();
        let ready = self.desktop_ready();
        let started = vrt::time::now_ns();
        let mut g = self.gpu.take().expect("composing with the GPU");
        if let Some(from) = shown
            && !stale.is_empty()
        {
            g.copy(from, target, &stale);
        }
        g.begin(target, clip);
        if !clip.is_empty() && desktop {
            self.draw_scene(&mut g, clip, now);
        }
        if let Some(s) = &mut self.startup
            && !s.frame_gpu(&mut g, clip, now, ready, &|| how.clone())
        {
            // Over: this frame showed the desktop alone, all of it (the
            // splash came in as it was revealed), as frames will from now on.
            self.startup = None;
        }
        self.screen.gpu_wrote(clip);
        let fence = g.end();
        // The GPU's driver takes a frame in a millisecond or two: say when
        // it takes much longer (the first few times, then now and then).
        let took = vrt::time::now_ns() - started;
        if took >= SLOW_FRAME_NS {
            let n = SLOW_FRAMES.fetch_add(1, Ordering::Relaxed);
            if n < 3 || n.is_multiple_of(100) {
                println!(
                    "{} took {} ms to take a frame ({}x{} drawn; {} such frames)",
                    g.renderer,
                    took / 1_000_000,
                    clip.w,
                    clip.h,
                    n + 1
                );
            }
        }
        self.gpu = Some(g);
        match fence {
            Some(fence) => self.screen.end_gpu_frame(real, fence),
            None => self.drop_gpu("the GPU stopped answering"),
        }
    }

    /// The desktop within `clip`, as [`Compositor::composite_cpu`] draws
    /// it: the windows bottom to top, each window's texture brought up to
    /// date first.
    fn draw_scene(&mut self, g: &mut Gpu, clip: Rect, now: u64) {
        let screen = self.screen_rect();
        let order = self.paint_order();
        let covering =
            order.iter().rposition(|id| self.windows.get(id).is_some_and(|w| w.opaque_rect().contains_rect(&clip)));
        if covering.is_none() {
            decor::draw_background_gpu(g, screen);
        }
        for id in &order[covering.unwrap_or(0)..] {
            if let (Some((_, preview)), Some(Drag::Move { id: dragged, .. })) = (self.snap, self.drag)
                && *id == dragged
                && preview.inflate(8).intersects(&clip)
            {
                decor::draw_snap_preview_gpu(g, preview);
            }
            let Some(win) = self.windows.get_mut(id) else { continue };
            if !win.visible() || !win.paint_bounds().intersects(&clip) {
                continue;
            }
            // Its pixels, as the GPU has them.
            let content = match (&win.buffers, win.current) {
                (Some(b), Some(index)) => {
                    let changed = win.upload.take();
                    let all = core::mem::replace(&mut win.upload_all, false);
                    Some(g.window(*id, b.pixels(index), b.width, b.height, b.stride, all, &changed))
                }
                _ => None,
            };
            let win = &self.windows[id];
            let (opacity, dy) = animation(win, now);
            self.decor.draw_window_gpu(g, win, self.focused == Some(*id), self.decor_state, opacity, dy, content);
        }
        if let Some(s) = &self.switcher
            && s.bounds().intersects(&clip)
        {
            let key = (s.selected as u64) << 32 | s.windows.len() as u64 | 1 << 63;
            let t = match g.cached(Slot::Switcher, key) {
                Some(t) => t,
                None => {
                    let titles: Vec<&str> =
                        s.windows.iter().map(|id| self.windows.get(id).map_or("", |w| w.title.as_str())).collect();
                    let b = self.decor.switcher_bitmap(s, &titles);
                    g.upload(Slot::Switcher, key, &b)
                }
            };
            let at = s.bounds();
            g.image(t, Rect::new(0, 0, at.w, at.h), at, 1.0, false, None);
        } else if self.switcher.is_none() && g.has(Slot::Switcher) {
            g.forget(Slot::Switcher);
        }
        // Cursor on top.
        if self.cursor_rect.intersects(&clip)
            && let Some((b, hx, hy)) = self.decor.cursor(self.cursor_shape)
        {
            let slot = Slot::Cursor(self.cursor_shape as u32);
            let t = match g.cached(slot, 1) {
                Some(t) => t,
                None => g.upload(slot, 1, b),
            };
            let at = Rect::new(self.pointer.0 - hx, self.pointer.1 - hy, t.w, t.h);
            g.image(t, Rect::new(0, 0, t.w, t.h), at, 1.0, false, None);
        }
    }

    // ---- the GPU's arrival and departure -------------------------------------

    /// Starts using a GPU that was set up, once a picture waits for nothing
    /// (none asked for, none being drawn): first checking that what it
    /// draws reaches the display's memory.
    fn take_gpu(&mut self) {
        if self.gpu_ready.is_none() {
            return;
        }
        if !self.screen.flipping() {
            self.gpu_ready = None;
            return;
        }
        let Some(picture) = self.screen.idle_picture() else { return };
        let Some(mut g) = self.gpu_ready.take() else { return };
        let checked = g.check(&mut self.screen, picture);
        // The check drew into the picture's first pixels: they are drawn again
        // before it is shown.
        self.screen.spoil(picture, Rect::new(0, 0, 4, 1));
        match checked {
            Ok(()) => {
                println!("frames are drawn by the GPU ({}) from now on", g.renderer);
                self.gpu = Some(g);
            }
            Err(why) => println!("frames stay with the processor: {}", why),
        }
    }

    /// Stops using the GPU: frames are drawn by the processor from the next
    /// on, all of the screen first (the back buffer was not kept).
    pub(crate) fn drop_gpu(&mut self, why: &str) {
        if self.gpu.take().is_none() {
            return;
        }
        println!("{}: frames are drawn by the processor from now on", why);
        self.screen.not_drawn();
        let screen = self.screen_rect();
        self.damage.add(screen);
    }

    /// The GPU's fences moved: the frame it drew, if done, is asked for.
    pub(crate) fn gpu_signaled(&mut self, now: u64) {
        let Some((fence, since)) = self.screen.drawing() else { return };
        let done = self.gpu.as_ref().is_some_and(|g| g.signaled(fence));
        if done {
            self.screen.drawn(now);
        } else if now.saturating_sub(since) >= GPU_TIMEOUT_NS || self.gpu.as_ref().is_none_or(|g| g.lost()) {
            self.drop_gpu("the GPU did not draw a frame in time");
        }
    }

    /// When to look at the GPU's fences again without its event (a frame it
    /// draws times out).
    pub(crate) fn gpu_deadline(&self) -> Option<u64> {
        self.screen.drawing().map(|(_, since)| since + GPU_TIMEOUT_NS)
    }
}
