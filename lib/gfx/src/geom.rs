//! Integer pixel geometry.

/// An integer rectangle (`w`/`h` are never negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w: if w < 0 { 0 } else { w }, h: if h < 0 { 0 } else { h } }
    }

    pub const fn from_size(w: i32, h: i32) -> Rect {
        Rect::new(0, 0, w, h)
    }

    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }

    /// Smallest rectangle containing both (empty rectangles are ignored).
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        Rect::new(x0, y0, self.right().max(o.right()) - x0, self.bottom().max(o.bottom()) - y0)
    }

    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, ..*self }
    }

    /// Grows (positive) or shrinks (negative) on every side.
    pub fn inflate(&self, d: i32) -> Rect {
        Rect::new(self.x - d, self.y - d, self.w + 2 * d, self.h + 2 * d)
    }

    pub fn inset(&self, left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect::new(self.x + left, self.y + top, self.w - left - right, self.h - top - bottom)
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    /// Centers a `w` x `h` rectangle inside this one.
    pub fn centered(&self, w: i32, h: i32) -> Rect {
        Rect::new(self.x + (self.w - w) / 2, self.y + (self.h - h) / 2, w, h)
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { self.w as i64 * self.h as i64 }
    }

    /// Splits off the top `h` pixels: (top, rest).
    pub fn split_top(&self, h: i32) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }

    /// Splits off the left `w` pixels: (left, rest).
    pub fn split_left(&self, w: i32) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }

    /// Splits off the bottom `h` pixels: (rest, bottom).
    pub fn split_bottom(&self, h: i32) -> (Rect, Rect) {
        let h = h.clamp(0, self.h);
        (Rect::new(self.x, self.y, self.w, self.h - h), Rect::new(self.x, self.bottom() - h, self.w, h))
    }

    /// Splits off the right `w` pixels: (rest, right).
    pub fn split_right(&self, w: i32) -> (Rect, Rect) {
        let w = w.clamp(0, self.w);
        (Rect::new(self.x, self.y, self.w - w, self.h), Rect::new(self.right() - w, self.y, w, self.h))
    }
}

/// A list of damaged rectangles that merges overlapping or nearby regions.
#[derive(Debug, Clone, Default)]
pub struct Damage {
    rects: alloc::vec::Vec<Rect>,
}

impl Damage {
    pub fn new() -> Damage {
        Damage::default()
    }

    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        let mut r = r;
        // Merge with everything it overlaps (or nearly touches), repeatedly.
        loop {
            let near = r.inflate(8);
            match self.rects.iter().position(|o| o.intersects(&near)) {
                Some(i) => r = r.union(&self.rects.swap_remove(i)),
                None => break,
            }
        }
        self.rects.push(r);
        // Too many fragments cost more than one big repaint.
        if self.rects.len() > 16 {
            let all = self.rects.iter().fold(Rect::default(), |a, b| a.union(b));
            self.rects.clear();
            self.rects.push(all);
        }
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }

    pub fn bounds(&self) -> Rect {
        self.rects.iter().fold(Rect::default(), |a, b| a.union(b))
    }

    pub fn take(&mut self) -> alloc::vec::Vec<Rect> {
        core::mem::take(&mut self.rects)
    }
}
