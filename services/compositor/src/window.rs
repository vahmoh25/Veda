//! Windows as tracked by the compositor.

use alloc::string::String;

use vgfx::Rect;
use vproto::display::{Cursor, WindowKind, WindowState};
use vrt::object::Channel;
use vrt::vm::Mapping;

use crate::state::Snap;

/// Height of the title bar of decorated windows.
pub const TITLE_HEIGHT: i32 = 36;
/// Corner radius of decorated windows.
pub const CORNER_RADIUS: i32 = 10;
/// Invisible margin around a window that acts as a resize handle.
pub const RESIZE_MARGIN: i32 = 6;
/// Width of each caption button.
pub const BUTTON_WIDTH: i32 = 46;

/// Client-provided pixel buffers.
pub struct Buffers {
    pub map: Mapping,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub count: u8,
}

impl Buffers {
    /// Pixels of buffer `index`.
    pub fn pixels(&self, index: u8) -> &[u32] {
        let per = (self.stride * self.height) as usize;
        let start = per * index.min(self.count - 1) as usize;
        // SAFETY: the mapping holds `count` buffers of `stride * height`
        // pixels (validated at attach time). The client may write them
        // concurrently, which can only produce visual tearing, never memory
        // unsafety, as the mapping stays valid while we hold it.
        unsafe { core::slice::from_raw_parts((self.map.as_ptr() as *const u32).add(start), per) }
    }
}

/// An animation affecting how a window is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimKind {
    Open,
    Close,
    Minimize,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anim {
    pub kind: AnimKind,
    pub start: u64,
    pub duration: u64,
}

impl Anim {
    /// Progress 0.0 → 1.0 with ease-out.
    pub fn progress(&self, now: u64) -> f32 {
        let t = ((now.saturating_sub(self.start)) as f32 / self.duration as f32).clamp(0.0, 1.0);
        1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
    }

    pub fn done(&self, now: u64) -> bool {
        now >= self.start + self.duration
    }
}

/// Which part of a window the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Client,
    Title,
    Minimize,
    Maximize,
    Close,
    /// Resize edge: (dx, dy) each -1, 0 or 1.
    Edge(i8, i8),
}

pub struct Window {
    pub id: u32,
    pub client: u64,
    pub kind: WindowKind,
    pub title: String,
    pub app_id: String,
    pub state: WindowState,
    /// Client area in screen coordinates.
    pub client_rect: Rect,
    /// Client rectangle to return to when un-maximising.
    pub restore_rect: Rect,
    pub min_w: i32,
    pub min_h: i32,
    pub resizable: bool,
    pub events: Channel,
    pub buffers: Option<Buffers>,
    pub current: Option<u8>,
    /// A FrameDone is owed for this buffer after the next composite.
    pub frame_owed: Option<u8>,
    pub cursor: Cursor,
    pub anim: Option<Anim>,
    /// The client asked to close; destroy after the close animation.
    pub closing: bool,
    /// Snapped to half of the screen (dragging restores `restore_rect`).
    pub snapped: Option<Snap>,
    /// Placed automatically in this cascade slot and not moved by the user
    /// since: the window is placed again when the work area changes.
    pub auto_slot: Option<i32>,
}

impl Window {
    pub fn decorated(&self) -> bool {
        self.kind == WindowKind::Normal && self.state != WindowState::Fullscreen
    }

    /// Outer rectangle (client area plus title bar).
    pub fn frame(&self) -> Rect {
        if self.decorated() {
            let c = self.client_rect;
            Rect::new(c.x, c.y - TITLE_HEIGHT, c.w, c.h + TITLE_HEIGHT)
        } else {
            self.client_rect
        }
    }

    /// Area that may be painted, including the shadow.
    pub fn paint_bounds(&self) -> Rect {
        if self.decorated() {
            let f = self.frame();
            Rect::new(f.x - 30, f.y - 26, f.w + 60, f.h + 64)
        } else if self.kind == WindowKind::Popup || self.kind == WindowKind::Notification {
            self.frame().inflate(28).translate(0, 4)
        } else {
            self.frame()
        }
    }

    pub fn title_rect(&self) -> Rect {
        let f = self.frame();
        Rect::new(f.x, f.y, f.w, TITLE_HEIGHT)
    }

    pub fn button_rect(&self, part: Part) -> Rect {
        let t = self.title_rect();
        let i = match part {
            Part::Close => 1,
            Part::Maximize => 2,
            Part::Minimize => 3,
            _ => return Rect::default(),
        };
        Rect::new(t.right() - BUTTON_WIDTH * i, t.y, BUTTON_WIDTH, TITLE_HEIGHT)
    }

    pub fn visible(&self) -> bool {
        self.state != WindowState::Minimized || self.anim.is_some()
    }

    /// Classifies a screen point; `None` if the point misses the window.
    pub fn hit(&self, x: i32, y: i32) -> Option<Part> {
        let f = self.frame();
        if self.decorated() && self.resizable && self.state == WindowState::Normal {
            let grip = f.inflate(RESIZE_MARGIN);
            if grip.contains(x, y) && !f.inset(2, 2, 2, 2).contains(x, y) {
                let dx = if x < f.x + 4 {
                    -1
                } else if x >= f.right() - 4 {
                    1
                } else {
                    0
                };
                let dy = if y < f.y + 4 {
                    -1
                } else if y >= f.bottom() - 4 {
                    1
                } else {
                    0
                };
                if dx != 0 || dy != 0 {
                    return Some(Part::Edge(dx, dy));
                }
            }
        }
        if !f.contains(x, y) {
            return None;
        }
        if self.decorated() && y < self.client_rect.y {
            for p in [Part::Close, Part::Maximize, Part::Minimize] {
                if self.button_rect(p).contains(x, y) && (p != Part::Maximize || self.resizable) {
                    return Some(p);
                }
            }
            return Some(Part::Title);
        }
        Some(Part::Client)
    }
}
