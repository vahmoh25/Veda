//! Flips: which of the driver's pictures is on the screen, which is to be
//! next, and when the screen's vertical blanks come.
//!
//! A flip writes the planes' surface registers; the display engine takes
//! the new picture at the start of the next vertical blank, and from then
//! on scans it out (`PLANE_SURFLIVE` says which it is). A request that
//! comes too late for one blank is carried out at the next.

use alloc::vec::Vec;

use crate::Mmio;
use crate::display::Takeover;
use crate::regs::{self, Pipe};

/// The pictures and the flips between them.
pub struct Flipper {
    takeover: Takeover,
    /// Each picture's address in the GPU's address space.
    surfaces: Vec<u32>,
    /// The picture on the screen (`None`: still the firmware's).
    shown: Option<usize>,
    /// The picture asked for, and the request's number.
    pending: Option<(usize, u32)>,
}

impl Flipper {
    pub fn new(takeover: Takeover, surfaces: Vec<u32>) -> Flipper {
        Flipper { takeover, surfaces, shown: None, pending: None }
    }

    pub fn takeover(&self) -> &Takeover {
        &self.takeover
    }

    /// Each picture's address in the GPU's address space.
    pub fn surfaces(&self) -> &[u32] {
        &self.surfaces
    }

    /// The picture on the screen, if it is one of the driver's.
    pub fn shown(&self) -> Option<usize> {
        self.shown
    }

    /// Whether a flip is waiting for a vertical blank.
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Asks for picture `index` from the next vertical blank, as request
    /// `seq`. A later request replaces one not yet carried out. False for a
    /// picture that does not exist.
    pub fn queue(&mut self, mmio: &impl Mmio, index: usize, seq: u32) -> bool {
        let Some(&surface) = self.surfaces.get(index) else { return false };
        self.takeover.show(mmio, surface);
        self.pending = Some((index, seq));
        true
    }

    /// After a vertical blank began (or while polling): the request the
    /// screen now shows, if the pending one is (on every pipe).
    pub fn completed(&mut self, mmio: &impl Mmio) -> Option<(usize, u32)> {
        let (index, seq) = self.pending?;
        if !self.takeover.shows(mmio, self.surfaces[index]) {
            return None;
        }
        self.pending = None;
        self.shown = Some(index);
        Some((index, seq))
    }

    /// Shows the firmware's picture again from the next vertical blank,
    /// forgetting any request.
    pub fn restore(&mut self, mmio: &impl Mmio) {
        self.takeover.show(mmio, self.takeover.surface);
        self.pending = None;
        self.shown = None;
    }
}

/// A pipe's lines: where its vertical blank starts and how many make a
/// frame. From the line the pipe is scanning out, they tell when the
/// frame's vertical blank began, however late the driver looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lines {
    pub blank_start: u32,
    pub total: u32,
}

impl Lines {
    /// `pipe`'s, if its mode makes sense.
    pub fn read(mmio: &impl Mmio, pipe: Pipe) -> Option<Lines> {
        let blank_start = (mmio.read(regs::vblank(pipe)) & 0xFFFF) + 1;
        let total = (mmio.read(regs::vtotal(pipe)) >> 16) + 1;
        (blank_start < total).then_some(Lines { blank_start, total })
    }

    /// When the last vertical blank began, if the pipe is at `line`
    /// (`PIPEDSL`) at `now` and a frame takes `period` (ns). `now` itself
    /// if that cannot be told.
    pub fn blank_before(&self, line: u32, now: u64, period: u64) -> u64 {
        if period == 0 || line >= self.total {
            return now;
        }
        let since = (line + 1 + self.total - self.blank_start) % self.total;
        now.saturating_sub(since as u64 * period / self.total as u64)
    }
}

/// The screen's rhythm, from the times its vertical blanks began.
#[derive(Debug, Clone, Copy, Default)]
pub struct Vsync {
    /// The frame counter and time of the last vertical blank seen.
    last: Option<(u32, u64)>,
    /// The time between two blanks (ns), smoothed; 0 until known.
    period: u64,
}

impl Vsync {
    pub fn new(period: u64) -> Vsync {
        Vsync { last: None, period }
    }

    pub fn period(&self) -> u64 {
        self.period
    }

    /// A vertical blank began at `now` (ns), the frame counter then at
    /// `frames`. Blanks missed in between (while nothing listened) are
    /// counted out.
    pub fn blank(&mut self, frames: u32, now: u64) {
        if let Some((f, t)) = self.last {
            let n = frames.wrapping_sub(f) as u64;
            // Only consecutive, plausible readings refine the period: 1
            // to 4 frames apart, 20 to 250 frames a second.
            if (1..=4).contains(&n) && now > t {
                let each = (now - t) / n;
                if (4_000_000..=50_000_000).contains(&each) {
                    self.period = if self.period == 0 { each } else { (self.period * 7 + each) / 8 };
                }
            }
        }
        self.last = Some((frames, now));
    }

    /// When the last vertical blank began (ns), if one was seen.
    pub fn last(&self) -> Option<u64> {
        self.last.map(|(_, t)| t)
    }

    /// When the first vertical blank after `now` should begin, if the
    /// rhythm is known.
    pub fn next_after(&self, now: u64) -> Option<u64> {
        let (_, t) = self.last?;
        if self.period == 0 {
            return None;
        }
        Some(t + (now.saturating_sub(t) / self.period + 1) * self.period)
    }

    /// The number of the last vertical blank seen.
    pub fn frames(&self) -> Option<u32> {
        self.last.map(|(f, _)| f)
    }
}
