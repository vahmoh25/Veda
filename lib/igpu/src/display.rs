//! What the firmware leaves on the screen, and whether the driver may take
//! it over.
//!
//! The firmware (UEFI's GOP) sets a mode up: a pipe, a transcoder, a port
//! and the panel's link, and plane 1 showing its framebuffer. The driver
//! keeps all of that as it is (no mode setting, no link training, no
//! watermarks or display buffer to recompute) and only ever changes where
//! plane 1's picture is, to buffers of exactly the same size and format.
//! So it takes over only a plain picture: linear, 32 bits per pixel,
//! unrotated, shown from its first pixel, never flipped asynchronously.
//! Where the plane is on the pipe, and whether a scaler fits the picture
//! to the panel, stays as the firmware set it. Anything else, the driver
//! leaves alone, and the firmware's framebuffer stays in use.

use alloc::vec::Vec;
use core::fmt;

use crate::Mmio;
use crate::regs::{self, Pipe};

/// A pipe as the firmware left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeState {
    pub pipe: Pipe,
    /// Its transcoder is on.
    pub enabled: bool,
    /// The picture's size, as the planes give it to the pipe.
    pub source: (u32, u32),
    /// The mode: active and total pixels across and lines down.
    pub active: (u32, u32),
    pub total: (u32, u32),
    /// Its scalers (fitting the picture to the panel) that are on.
    pub scalers: u8,
    /// Panel self refresh (1 and 2).
    pub psr: bool,
    pub psr2: bool,
    /// Plane 1, the other planes that are on (by number), and where the
    /// cursor's picture is if it is on.
    pub plane: PlaneState,
    pub others: Vec<(u8, PlaneState)>,
    pub cursor: Option<u32>,
}

/// A plane's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaneState {
    pub ctl: u32,
    pub stride: u32,
    pub pos: u32,
    pub size: u32,
    pub offset: u32,
    pub surf: u32,
    pub surf_live: u32,
}

impl PlaneState {
    pub fn enabled(&self) -> bool {
        self.ctl & regs::PLANE_CTL_ENABLE != 0
    }

    /// Width and height.
    pub fn size(&self) -> (u32, u32) {
        ((self.size & 0xFFFF) + 1, (self.size >> 16) + 1)
    }

    /// Bytes from one row to the next.
    pub fn stride_bytes(&self) -> u32 {
        regs::plane_stride_bytes(self.stride)
    }

    /// Where the picture is in the GPU's address space.
    pub fn surface(&self) -> u32 {
        self.surf & regs::PLANE_SURF_MASK
    }

    /// The bytes the plane scans out from its surface on, in whole pages.
    pub fn bytes(&self) -> u64 {
        let lines = regs::plane_offset_y(self.offset) + self.size().1;
        (self.stride_bytes() as u64 * lines as u64).next_multiple_of(4096)
    }
}

impl PipeState {
    /// Reads `pipe`'s registers.
    pub fn read(mmio: &impl Mmio, pipe: Pipe) -> PipeState {
        let conf = mmio.read(regs::transconf(pipe));
        let src = mmio.read(regs::pipesrc(pipe));
        let (h, v) = (mmio.read(regs::htotal(pipe)), mmio.read(regs::vtotal(pipe)));
        let plane = |n| PlaneState {
            ctl: mmio.read(regs::plane_ctl(pipe, n)),
            stride: mmio.read(regs::plane_stride(pipe, n)),
            pos: mmio.read(regs::plane_pos(pipe, n)),
            size: mmio.read(regs::plane_size(pipe, n)),
            offset: mmio.read(regs::plane_offset(pipe, n)),
            surf: mmio.read(regs::plane_surf(pipe, n)),
            surf_live: mmio.read(regs::plane_surf_live(pipe, n)),
        };
        let others = (2..=regs::PLANES).map(|n| (n, plane(n))).filter(|(_, p)| p.enabled()).collect();
        let cursor = (mmio.read(regs::cursor_ctl(pipe)) & regs::CURSOR_MODE != 0)
            .then(|| mmio.read(regs::cursor_base(pipe)) & regs::PLANE_SURF_MASK);
        let mut scalers = 0;
        for s in 0..2 {
            if mmio.read(regs::scaler_ctl(pipe, s)) & regs::SCALER_ENABLE != 0 {
                scalers += 1;
            }
        }
        PipeState {
            pipe,
            enabled: conf & regs::TRANSCONF_ENABLE != 0,
            source: ((src >> 16) + 1, (src & 0xFFFF) + 1),
            active: ((h & 0xFFFF) + 1, (v & 0xFFFF) + 1),
            total: ((h >> 16) + 1, (v >> 16) + 1),
            scalers,
            psr: mmio.read(regs::psr_ctl(pipe)) & regs::PSR_ENABLE != 0,
            psr2: mmio.read(regs::psr2_ctl(pipe)) & regs::PSR_ENABLE != 0,
            plane: plane(1),
            others,
            cursor,
        }
    }

    /// Why plane 1's picture cannot be taken over, if it cannot.
    fn check(&self) -> Result<(), Refusal> {
        let (pipe, ctl) = (self.pipe, self.plane.ctl);
        if !self.plane.enabled() {
            return Err(Refusal::PlaneOff(pipe));
        }
        if regs::plane_format(ctl) != regs::FORMAT_8888 {
            return Err(Refusal::Format(pipe, regs::plane_format(ctl)));
        }
        if regs::plane_tiling(ctl) != 0 {
            return Err(Refusal::Tiled(pipe));
        }
        if regs::plane_rotation(ctl) != 0 {
            return Err(Refusal::Rotated(pipe));
        }
        if ctl & regs::PLANE_CTL_ASYNC_FLIP != 0 {
            return Err(Refusal::AsyncFlips(pipe));
        }
        if self.plane.offset != 0 {
            return Err(Refusal::Offset(pipe));
        }
        let (w, _) = self.plane.size();
        if self.plane.stride_bytes() < w * 4 || !(self.plane.surface() as u64).is_multiple_of(regs::SURFACE_ALIGN) {
            return Err(Refusal::Layout(pipe));
        }
        Ok(())
    }
}

/// Every pipe of a display engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readout {
    pub pipes: Vec<PipeState>,
}

impl Readout {
    /// Reads the first `pipes` pipes.
    pub fn read(mmio: &impl Mmio, pipes: u8) -> Readout {
        Readout { pipes: (0..pipes).map(|p| PipeState::read(mmio, Pipe(p))).collect() }
    }

    /// What the driver may take over: the pipes showing the firmware's
    /// picture (one, or several showing the same one), if that picture is
    /// plain enough.
    pub fn takeover(&self) -> Result<Takeover, Refusal> {
        let on: Vec<&PipeState> = self.pipes.iter().filter(|p| p.enabled).collect();
        let first = on.first().ok_or(Refusal::NoPipe)?;
        for p in &on {
            p.check()?;
        }
        // One picture: shown by every pipe that is on, at the same size and
        // in the same layout (the firmware clones it to every screen).
        let surface = first.plane.surface();
        let same = |p: &&PipeState| {
            p.plane.surface() == surface
                && p.plane.size() == first.plane.size()
                && p.plane.stride == first.plane.stride
                && p.plane.ctl & regs::PLANE_CTL_ORDER_RGBX == first.plane.ctl & regs::PLANE_CTL_ORDER_RGBX
        };
        if !on.iter().all(same) {
            return Err(Refusal::SeveralPictures);
        }
        let (width, height) = first.plane.size();
        Ok(Takeover {
            pipes: on.iter().map(|p| p.pipe).collect(),
            width,
            height,
            stride: first.plane.stride_bytes(),
            rgbx: first.plane.ctl & regs::PLANE_CTL_ORDER_RGBX != 0,
            surface,
        })
    }

    /// What the display engine scans out now, as ranges of its address
    /// space (start, bytes): the pictures of every plane that is on, and
    /// the cursors'. The driver maps nothing over them.
    pub fn scanned_out(&self) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        for p in self.pipes.iter().filter(|p| p.enabled) {
            for plane in core::iter::once(&p.plane).chain(p.others.iter().map(|(_, s)| s)) {
                if plane.enabled() {
                    ranges.push((plane.surface() as u64, plane.bytes()));
                }
            }
            if let Some(base) = p.cursor {
                ranges.push((base as u64, regs::CURSOR_BYTES));
            }
        }
        ranges
    }
}

/// The picture the driver takes over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    /// The pipes showing it; the first one paces the flips.
    pub pipes: Vec<Pipe>,
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next.
    pub stride: u32,
    /// Red in the low byte of each pixel (otherwise blue: `0xXXRRGGBB`).
    pub rgbx: bool,
    /// Where the firmware's picture is in the GPU's address space.
    pub surface: u32,
}

impl Takeover {
    /// Bytes in one picture, rounded up to whole pages.
    pub fn picture_bytes(&self) -> u64 {
        (self.stride as u64 * self.height as u64).next_multiple_of(4096)
    }

    /// Shows the picture at `surface` (an offset in the GPU's address
    /// space) from the start of the next vertical blank on every pipe.
    pub fn show(&self, mmio: &impl Mmio, surface: u32) {
        for &p in &self.pipes {
            mmio.write(regs::plane_surf(p, 1), surface);
        }
    }

    /// The picture the first pipe is scanning out now.
    pub fn live(&self, mmio: &impl Mmio) -> u32 {
        mmio.read(regs::plane_surf_live(self.pipes[0], 1)) & regs::PLANE_SURF_MASK
    }

    /// Whether every pipe scans out the picture at `surface` now.
    pub fn shows(&self, mmio: &impl Mmio, surface: u32) -> bool {
        self.pipes.iter().all(|&p| mmio.read(regs::plane_surf_live(p, 1)) & regs::PLANE_SURF_MASK == surface)
    }

    /// The first pipe's frame counter.
    pub fn frames(&self, mmio: &impl Mmio) -> u32 {
        mmio.read(regs::frame_count(self.pipes[0]))
    }
}

/// Why the driver leaves the screen to the firmware's framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No pipe is on.
    NoPipe,
    /// Plane 1 of a pipe that is on is off.
    PlaneOff(Pipe),
    /// Not 32 bits per pixel (the format's code).
    Format(Pipe, u32),
    Tiled(Pipe),
    Rotated(Pipe),
    AsyncFlips(Pipe),
    /// The plane shows its picture from a point inside it (`PLANE_OFFSET`).
    Offset(Pipe),
    /// The stride is too small, or the surface misplaced.
    Layout(Pipe),
    /// Pipes that are on show different pictures.
    SeveralPictures,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NoPipe => write!(f, "no pipe is on"),
            Refusal::PlaneOff(p) => write!(f, "{p} is on but its plane 1 is off"),
            Refusal::Format(p, code) => write!(f, "{p} shows a picture that is not 32 bits per pixel (format {code})"),
            Refusal::Tiled(p) => write!(f, "{p} shows a tiled picture"),
            Refusal::Rotated(p) => write!(f, "{p} shows a rotated picture"),
            Refusal::AsyncFlips(p) => write!(f, "{p} flips asynchronously"),
            Refusal::Offset(p) => write!(f, "{p}'s plane 1 shows its picture from a point inside it"),
            Refusal::Layout(p) => write!(f, "{p}'s picture is laid out oddly in memory"),
            Refusal::SeveralPictures => write!(f, "the pipes that are on show different pictures"),
        }
    }
}

impl fmt::Display for PipeState {
    /// One line for the log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.enabled {
            return write!(f, "{}: off", self.pipe);
        }
        let (active, total, source) = (self.active, self.total, self.source);
        write!(f, "{}: {}x{} active of {}x{}", self.pipe, active.0, active.1, total.0, total.1)?;
        write!(f, ", picture {}x{}", source.0, source.1)?;
        if self.scalers > 0 {
            write!(f, ", scaled")?;
        }
        if self.psr2 {
            write!(f, ", PSR2")?;
        } else if self.psr {
            write!(f, ", PSR")?;
        }
        if !self.others.is_empty() {
            write!(f, ", other planes on")?;
        }
        if self.cursor.is_some() {
            write!(f, ", cursor on")?;
        }
        let (w, h) = self.plane.size();
        write!(f, "; plane 1 {} {}x{}", if self.plane.enabled() { "on" } else { "off" }, w, h)?;
        if self.plane.pos != 0 {
            write!(f, " at ({}, {})", self.plane.pos & 0x1FFF, (self.plane.pos >> 16) & 0x1FFF)?;
        }
        write!(
            f,
            ", stride {} format {} tiling {}, surface {:#x}",
            self.plane.stride_bytes(),
            regs::plane_format(self.plane.ctl),
            regs::plane_tiling(self.plane.ctl),
            self.plane.surface()
        )
    }
}
