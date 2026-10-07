//! The display engine of an Intel integrated GPU (display versions 12 and
//! 13), as a driver sees it through the GPU's first BAR: pipes with their
//! frame counters, planes whose registers take effect at the start of a
//! vertical blank (`PLANE_SURFLIVE` saying what is scanned out), the
//! interrupt registers as i915 describes them, and the global address
//! table in the BAR's upper half.
//!
//! The firmware's setup ([`Gop`]) is what UEFI leaves: a pipe on, plane 1
//! showing a linear framebuffer in stolen memory, mapped in the table (at
//! its bottom unless a test says otherwise). Time passes when a test says
//! so ([`DisplayEngine::vblank`]).
//!
//! The model also notes what a driver must not do here: change a plane's
//! setup (its format, size, stride, position: everything but where its
//! picture is), scan out memory the table does not map (or maps to a
//! scratch page), or remap the entries the firmware uses.

use std::cell::RefCell;
use std::collections::BTreeMap;

use vigpu::regs::{self, Pipe};

/// The first BAR's size (registers below, the table above).
pub const BAR_SIZE: u64 = 16 << 20;
/// `SNB_GMCH_CTRL` of a GPU with an 8 MiB table (4 GiB of addresses).
pub const GMCH_CTRL: u16 = 3 << 6;
/// Where stolen memory starts (the firmware's framebuffer is there).
pub const STOLEN: u64 = 0x7C00_0000;
/// The scratch page a firmware may point unused entries at.
pub const SCRATCH: u64 = 0x7BFF_F000;

/// What the firmware set up.
#[derive(Debug, Clone)]
pub struct Gop {
    /// The pipes it lit, all showing its framebuffer.
    pub pipes: Vec<Pipe>,
    /// Its framebuffer.
    pub width: u32,
    pub height: u32,
    /// The panel's mode (the pipe scales when it differs).
    pub panel: (u32, u32),
    /// Pixel format and tiling fields of plane 1 (`PLANE_CTL`).
    pub format: u32,
    pub tiling: u32,
    pub rgbx: bool,
    pub psr: bool,
    /// Where its framebuffer is in the GPU's address space.
    pub surface: u64,
    /// More of the table the firmware uses, after its framebuffer
    /// (address, pages).
    pub also_mapped: Vec<(u64, u64)>,
    /// Every other entry of the table maps one scratch page (rather than
    /// nothing).
    pub scratch: bool,
    /// Where the cursor's picture is, on the first pipe, if it is on.
    pub cursor: Option<u32>,
    /// A scaler fits plane 1 to the panel (rather than the pipe's picture:
    /// the pipe takes the panel's size from its planes).
    pub plane_scaling: bool,
    /// The line and column of its framebuffer plane 1 starts at
    /// (`PLANE_OFFSET`).
    pub offset: (u32, u32),
}

impl Gop {
    /// One pipe showing a `width` x `height` linear XRGB picture on a panel
    /// of `panel` pixels.
    pub fn plain(width: u32, height: u32, panel: (u32, u32)) -> Gop {
        Gop {
            pipes: vec![Pipe(0)],
            width,
            height,
            panel,
            format: regs::FORMAT_8888,
            tiling: 0,
            rgbx: false,
            psr: false,
            surface: 0,
            also_mapped: Vec::new(),
            scratch: false,
            cursor: None,
            plane_scaling: false,
            offset: (0, 0),
        }
    }

    /// Bytes from one framebuffer row to the next (the GOP's own: rows of
    /// whole 64-byte units).
    pub fn stride(&self) -> u32 {
        (self.width * 4).next_multiple_of(64)
    }
}

struct State {
    regs: BTreeMap<u32, u32>,
    /// Each plane's surface as scanned out: (pipe, plane) → address.
    live: BTreeMap<(u8, u8), u32>,
    table: BTreeMap<u64, u64>,
    /// What the entries not in `table` hold.
    default_pte: u64,
    /// The firmware's entries, which nobody may change.
    firmware_pages: Vec<u64>,
    flushes: u32,
    /// The interrupt line: raised and not yet seen by the test.
    interrupt: bool,
    notes: Vec<String>,
    /// Reads of `PLANE_SURFLIVE` after a vertical blank before the new
    /// surface shows there (a latch a moment after the blank's interrupt),
    /// and the surfaces on their way: (pipe, plane) → (surface, reads).
    latch_delay: u32,
    latching: BTreeMap<(u8, u8), (u32, u32)>,
}

/// The display engine, with the firmware's picture on it.
pub struct DisplayEngine {
    state: RefCell<State>,
    pipes: u8,
}

const PAGE: u64 = 4096;

impl DisplayEngine {
    /// An engine of `pipes` pipes as `gop` left it.
    pub fn new(pipes: u8, gop: &Gop) -> DisplayEngine {
        let mut regs = BTreeMap::new();
        let mut live = BTreeMap::new();
        for &p in &gop.pipes {
            regs.insert(regs::transconf(p), regs::TRANSCONF_ENABLE | regs::TRANSCONF_STATE_ENABLE);
            let (pw, ph) = gop.panel;
            let (sw, sh) = if gop.plane_scaling { gop.panel } else { (gop.width, gop.height) };
            regs.insert(regs::pipesrc(p), ((sw - 1) << 16) | (sh - 1));
            regs.insert(regs::htotal(p), ((pw + 160 - 1) << 16) | (pw - 1));
            regs.insert(regs::vtotal(p), ((ph + 40 - 1) << 16) | (ph - 1));
            regs.insert(regs::vblank(p), ((ph + 40 - 1) << 16) | (ph - 1));
            if gop.panel != (gop.width, gop.height) {
                regs.insert(regs::scaler_ctl(p, 0), regs::SCALER_ENABLE);
            }
            if gop.psr {
                regs.insert(regs::psr_ctl(p), regs::PSR_ENABLE);
            }
            let mut ctl = regs::PLANE_CTL_ENABLE | (gop.format << 23) | (gop.tiling << 10);
            if gop.rgbx {
                ctl |= regs::PLANE_CTL_ORDER_RGBX;
            }
            regs.insert(regs::plane_ctl(p, 1), ctl);
            regs.insert(regs::plane_stride(p, 1), gop.stride() / 64);
            regs.insert(regs::plane_size(p, 1), ((gop.height - 1) << 16) | (gop.width - 1));
            regs.insert(regs::plane_offset(p, 1), (gop.offset.0 << 16) | gop.offset.1);
            regs.insert(regs::plane_surf(p, 1), gop.surface as u32);
            live.insert((p.0, 1), gop.surface as u32);
        }
        if let (Some(cursor), Some(&p)) = (gop.cursor, gop.pipes.first()) {
            regs.insert(regs::cursor_ctl(p), regs::CURSOR_MODE);
            regs.insert(regs::cursor_base(p), cursor);
        }
        // The firmware's framebuffer, in stolen memory; and whatever else
        // it maps.
        let base = (BAR_SIZE / 2) as u32;
        let mut table = BTreeMap::new();
        let mut firmware_pages = Vec::new();
        let fb_pages = (gop.stride() as u64 * gop.height as u64).div_ceil(PAGE);
        for page in gop.surface / PAGE..gop.surface / PAGE + fb_pages {
            table.insert(base as u64 + page * 8, (STOLEN + page * PAGE) | regs::PTE_PRESENT);
            firmware_pages.push(page);
        }
        if let Some(cursor) = gop.cursor {
            let first = cursor as u64 / PAGE;
            for page in first..first + regs::CURSOR_BYTES / PAGE {
                table.insert(base as u64 + page * 8, (STOLEN + page * PAGE) | regs::PTE_PRESENT);
                firmware_pages.push(page);
            }
        }
        for &(address, pages) in &gop.also_mapped {
            for page in address / PAGE..address / PAGE + pages {
                table.insert(base as u64 + page * 8, (STOLEN + page * PAGE) | regs::PTE_PRESENT);
                firmware_pages.push(page);
            }
        }
        DisplayEngine {
            state: RefCell::new(State {
                regs,
                live,
                table,
                default_pte: if gop.scratch { SCRATCH | regs::PTE_PRESENT } else { 0 },
                firmware_pages,
                flushes: 0,
                interrupt: false,
                notes: Vec::new(),
                latch_delay: 0,
                latching: BTreeMap::new(),
            }),
            pipes,
        }
    }

    fn table_base(&self) -> u32 {
        (BAR_SIZE / 2) as u32
    }

    /// A vertical blank begins on `pipe`: armed plane registers take
    /// effect, the frame counter counts, and the interrupt is raised if
    /// the driver asked for it.
    pub fn vblank(&self, pipe: Pipe) {
        let mut s = self.state.borrow_mut();
        for plane in 1..=regs::PLANES {
            let surf = s.regs.get(&regs::plane_surf(pipe, plane)).copied().unwrap_or(0) & regs::PLANE_SURF_MASK;
            let enabled = s.regs.get(&regs::plane_ctl(pipe, plane)).copied().unwrap_or(0) & regs::PLANE_CTL_ENABLE != 0;
            if enabled && s.live.get(&(pipe.0, plane)) != Some(&surf) {
                // The plane scans its new picture out: all of it must be
                // mapped, and not to the scratch page.
                let stride =
                    regs::plane_stride_bytes(s.regs.get(&regs::plane_stride(pipe, plane)).copied().unwrap_or(0));
                let size = s.regs.get(&regs::plane_size(pipe, plane)).copied().unwrap_or(0);
                let bytes = stride as u64 * ((size >> 16) + 1) as u64;
                let base = self.table_base() as u64;
                for page in (surf as u64 / PAGE)..(surf as u64 + bytes).div_ceil(PAGE) {
                    let pte = s.table.get(&(base + page * 8)).copied().unwrap_or(s.default_pte);
                    if pte & regs::PTE_PRESENT == 0 || pte & regs::PTE_ADDR_MASK == SCRATCH {
                        let note = format!(
                            "{pipe} plane {plane} scans out {surf:#x}, whose page {page:#x} is unmapped or scratch"
                        );
                        s.notes.push(note);
                        break;
                    }
                }
                if s.latch_delay == 0 {
                    s.live.insert((pipe.0, plane), surf);
                } else {
                    let reads = s.latch_delay;
                    s.latching.insert((pipe.0, plane), (surf, reads));
                }
            }
        }
        let count = s.regs.entry(regs::frame_count(pipe)).or_insert(0);
        *count = count.wrapping_add(1);
        // The line counter, one behind, still reads the line before the
        // blank's first.
        let blank_start = s.regs.get(&regs::vblank(pipe)).copied().unwrap_or(0) & 0xFFFF;
        s.regs.insert(regs::scanline(pipe), blank_start);
        drop(s);
        self.event(pipe, regs::PIPE_VBLANK);
    }

    /// From now on, the new surface shows in `PLANE_SURFLIVE` only after it
    /// was read `reads` times following the blank (the registers taking the
    /// new picture a moment after the blank's interrupt).
    pub fn delay_latch(&self, reads: u32) {
        self.state.borrow_mut().latch_delay = reads;
    }

    /// The pipe scans line `line` out now (`PIPEDSL`).
    pub fn set_line(&self, pipe: Pipe, line: u32) {
        self.state.borrow_mut().regs.insert(regs::scanline(pipe), line);
    }

    /// `events` happen on `pipe` (a fault, say: plane 1 read unmapped
    /// memory). Those the mask lets through are latched; enabled ones
    /// interrupt.
    pub fn event(&self, pipe: Pipe, events: u32) {
        let mut s = self.state.borrow_mut();
        let imr = s.regs.get(&regs::pipe_imr(pipe)).copied().unwrap_or(!0);
        *s.regs.entry(regs::pipe_iir(pipe)).or_insert(0) |= events & !imr;
        if self.pending(&s) {
            s.interrupt = true;
        }
    }

    /// Whether the GPU's interrupt is asserted: an enabled event latched,
    /// display interrupts and the master on.
    fn pending(&self, s: &State) -> bool {
        let master = s.regs.get(&regs::MASTER_IRQ).copied().unwrap_or(0) & regs::MASTER_IRQ_ENABLE != 0;
        let display = s.regs.get(&regs::DISPLAY_INT_CTL).copied().unwrap_or(0) & regs::DISPLAY_IRQ_ENABLE != 0;
        master && display && self.display_sources(s) != 0
    }

    /// The pipes with an enabled event latched, as `DISPLAY_INT_CTL` shows
    /// them.
    fn display_sources(&self, s: &State) -> u32 {
        let mut bits = 0;
        for p in 0..self.pipes {
            let p = Pipe(p);
            let iir = s.regs.get(&regs::pipe_iir(p)).copied().unwrap_or(0);
            let ier = s.regs.get(&regs::pipe_ier(p)).copied().unwrap_or(0);
            if iir & ier != 0 {
                bits |= regs::display_int_pipe(p);
            }
        }
        bits
    }

    /// Takes the interrupt, if one was raised since the last call.
    pub fn take_interrupt(&self) -> bool {
        core::mem::take(&mut self.state.borrow_mut().interrupt)
    }

    /// The surface `pipe`'s plane 1 scans out.
    pub fn live(&self, pipe: Pipe) -> u32 {
        self.state.borrow().live.get(&(pipe.0, 1)).copied().unwrap_or(0)
    }

    /// How often the driver made the GPU drop its view of the table.
    pub fn flushes(&self) -> u32 {
        self.state.borrow().flushes
    }

    /// What the driver did that it must not.
    pub fn notes(&self) -> Vec<String> {
        self.state.borrow().notes.clone()
    }

    /// The physical page entry `page` maps, if any.
    pub fn mapped(&self, page: u64) -> Option<u64> {
        let s = self.state.borrow();
        let pte = s.table.get(&(self.table_base() as u64 + page * 8)).copied().unwrap_or(s.default_pte);
        (pte & regs::PTE_PRESENT != 0).then_some(pte & regs::PTE_ADDR_MASK)
    }

    /// Whether `offset` is one of a plane's setup registers, which the
    /// driver keeps as the firmware set them.
    fn plane_setup(offset: u32) -> Option<(Pipe, u8)> {
        for p in 0..4 {
            for plane in 1..=regs::PLANES {
                let ctl = regs::plane_ctl(Pipe(p), plane);
                if (ctl..ctl + 0x100).contains(&offset)
                    && offset != regs::plane_surf(Pipe(p), plane)
                    && offset != regs::plane_surf_live(Pipe(p), plane)
                {
                    return Some((Pipe(p), plane));
                }
            }
        }
        None
    }
}

impl vigpu::Mmio for DisplayEngine {
    fn read(&self, offset: u32) -> u32 {
        for p in 0..self.pipes {
            for plane in 1..=regs::PLANES {
                if offset == regs::plane_surf_live(Pipe(p), plane) {
                    // A surface on its way shows after so many reads.
                    let mut s = self.state.borrow_mut();
                    if let Some((surf, reads)) = s.latching.get(&(p, plane)).copied() {
                        if reads <= 1 {
                            s.latching.remove(&(p, plane));
                            s.live.insert((p, plane), surf);
                        } else {
                            s.latching.insert((p, plane), (surf, reads - 1));
                        }
                    }
                    return s.live.get(&(p, plane)).copied().unwrap_or(0);
                }
            }
        }
        let s = self.state.borrow();
        let reg = |o: u32| s.regs.get(&o).copied().unwrap_or(0);
        if offset == regs::MASTER_IRQ {
            let mut v = reg(offset) & regs::MASTER_IRQ_ENABLE;
            if self.display_sources(&s) != 0 {
                v |= regs::MASTER_IRQ_DISPLAY;
            }
            return v;
        }
        if offset == regs::DISPLAY_INT_CTL {
            return (reg(offset) & regs::DISPLAY_IRQ_ENABLE) | self.display_sources(&s);
        }
        reg(offset)
    }

    fn write(&self, offset: u32, value: u32) {
        let mut s = self.state.borrow_mut();
        if let Some((pipe, plane)) = Self::plane_setup(offset) {
            let old = s.regs.get(&offset).copied().unwrap_or(0);
            if old != value {
                s.notes.push(format!("{pipe} plane {plane}: register {offset:#x} changed from {old:#x} to {value:#x}"));
            }
        }
        if offset == regs::GFX_FLUSH_CNTL && value & regs::GFX_FLUSH_CNTL_EN != 0 {
            s.flushes += 1;
            return;
        }
        let is_iir = (0..self.pipes).any(|p| offset == regs::pipe_iir(Pipe(p)));
        if is_iir {
            // Write one to clear.
            let v = s.regs.entry(offset).or_insert(0);
            *v &= !value;
        } else {
            s.regs.insert(offset, value);
        }
        let raised = self.pending(&s);
        s.interrupt |= raised;
    }

    fn read64(&self, offset: u32) -> u64 {
        assert!(offset >= self.table_base(), "64-bit read of a register: {offset:#x}");
        let s = self.state.borrow();
        s.table.get(&(offset as u64)).copied().unwrap_or(s.default_pte)
    }

    fn write64(&self, offset: u32, value: u64) {
        assert!(offset >= self.table_base(), "64-bit write of a register: {offset:#x}");
        let mut s = self.state.borrow_mut();
        let page = (offset - self.table_base()) as u64 / 8;
        if s.firmware_pages.contains(&page) {
            s.notes.push(format!("the firmware's table entry for page {page:#x} was changed"));
        }
        s.table.insert(offset as u64, value);
    }
}
