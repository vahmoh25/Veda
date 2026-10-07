//! Composition: damaged regions are redrawn bottom to top into the back
//! buffer, which is then copied to the screen (`screen`).

use alloc::vec::Vec;

use vgfx::Canvas;
use vproto::display::WindowEvent;

use crate::decor;
use crate::state::{Compositor, Drag};
use crate::window::AnimKind;

impl Compositor {
    // ---- rendering ---------------------------------------------------------

    pub(crate) fn animating(&self) -> bool {
        self.startup.is_some() || self.windows.values().any(|w| w.anim.is_some())
    }

    /// Whether a frame is to be composed (when the screen is ready for one).
    pub(crate) fn wants_frame(&self) -> bool {
        !self.damage.is_empty() || self.animating() || self.screen.needs_frame()
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
                let (opacity, dy) = match win.anim {
                    Some(a) => {
                        let p = a.progress(now);
                        match a.kind {
                            AnimKind::Open | AnimKind::Restore => (p, ((1.0 - p) * 14.0) as i32),
                            AnimKind::Close => (1.0 - p, (p * 10.0) as i32),
                            AnimKind::Minimize => (1.0 - p, (p * 40.0) as i32),
                        }
                    }
                    None => (1.0, 0),
                };
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
        // Clients may now draw their next frame.
        let owed: Vec<(u32, u8)> =
            self.windows.iter_mut().filter_map(|(id, w)| w.frame_owed.take().map(|b| (*id, b))).collect();
        for (id, b) in owed {
            self.send(id, WindowEvent::FrameDone { shown: b });
        }
        if self.startup.is_some() {
            let ready = self.desktop_ready();
            if let Some(s) = &mut self.startup
                && !s.frame(&mut self.screen, now, ready)
            {
                // Over: from now on the composed screen, as it is.
                self.startup = None;
                self.screen.flush(screen);
            }
        }
        self.screen.end_frame(real);
        self.last_frame = real;
    }
}
