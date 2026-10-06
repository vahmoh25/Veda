//! The boot splash: a dark gradient with the Veda logo (OS1's white ring
//! from "Her"), drawn directly into the GOP framebuffer while the system
//! loads. The picture is `vsplash`'s, which the window system draws the
//! same way when it takes the screen over and animates it.

pub struct Surface {
    pub base: *mut u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// `true` if the framebuffer stores R in the low byte.
    pub rgb: bool,
}

impl Surface {
    fn put(&self, x: u32, y: u32, c: u32) {
        let c = if self.rgb { (c & 0xFF00FF00) | ((c >> 16) & 0xFF) | ((c & 0xFF) << 16) } else { c };
        // SAFETY: callers keep (x, y) inside the framebuffer.
        unsafe { self.base.add((y * self.stride + x) as usize).write_volatile(c) }
    }
}

/// Paints the splash over the whole screen.
pub fn draw(s: &Surface) {
    vsplash::draw(s.width, s.height, |x, y, c| s.put(x, y, c));
}
