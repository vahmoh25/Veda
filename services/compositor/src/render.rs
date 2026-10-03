//! The screen and composition: damaged regions are redrawn bottom to top
//! into a back buffer, which is then copied to the framebuffer.

use alloc::vec::Vec;

use vgfx::{Bitmap, Canvas, Rect};
use vproto::display::WindowEvent;
use vrt::vm::Mapping;

use crate::decor;
use crate::state::{Compositor, Drag};
use crate::window::AnimKind;

/// The framebuffer and the back buffer we compose into.
pub(crate) struct Screen {
    pub(crate) fb: Mapping,
    pub(crate) pitch: usize,
    pub(crate) rgb: bool,
    pub(crate) back: Bitmap,
}

impl Screen {
    pub(crate) fn rect(&self) -> Rect {
        self.back.rect()
    }

    /// Copies a region of the back buffer to the framebuffer.
    pub(crate) fn flush(&mut self, r: Rect) {
        let r = r.intersect(&self.rect());
        let w = self.back.width;
        let fb = self.fb.as_ptr();
        for y in r.y..r.bottom() {
            let src = &self.back.pixels[(y * w + r.x) as usize..(y * w + r.right()) as usize];
            // SAFETY: the framebuffer mapping covers `pitch * height` bytes.
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (fb.add(y as usize * self.pitch) as *mut u32).add(r.x as usize),
                    r.w as usize,
                )
            };
            if self.rgb {
                for (d, &s) in dst.iter_mut().zip(src) {
                    *d = (s & 0xFF00_FF00) | ((s >> 16) & 0xFF) | ((s & 0xFF) << 16);
                }
            } else {
                dst.copy_from_slice(src);
            }
        }
    }
}

impl Compositor {
    // ---- rendering ---------------------------------------------------------

    pub(crate) fn animating(&self) -> bool {
        self.windows.values().any(|w| w.anim.is_some())
    }

    pub(crate) fn composite(&mut self) {
        let now = vrt::time::now_ns();
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
            self.screen.flush(r);
        }
        // Clients may now draw their next frame.
        let owed: Vec<(u32, u8)> =
            self.windows.iter_mut().filter_map(|(id, w)| w.frame_owed.take().map(|b| (*id, b))).collect();
        for (id, b) in owed {
            self.send(id, WindowEvent::FrameDone { shown: b });
        }
        self.last_frame = now;
    }
}
