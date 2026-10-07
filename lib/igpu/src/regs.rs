//! Registers of the display engine (display versions 12 and 13) and of the
//! GPU's global address table, by the names Linux's i915 driver gives them
//! (`intel_display_regs.h`, `skl_universal_plane_regs.h`, `i915_irq.c`,
//! `intel_ggtt.c`). Offsets are in the GPU's first BAR.

/// A display pipe: A is 0. On display 12 and 13, transcoder N feeds pipe N.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pipe(pub u8);

impl Pipe {
    pub fn name(self) -> char {
        (b'A' + self.0) as char
    }

    /// Between pipe A's registers and this pipe's (and between
    /// transcoders').
    const fn step(self) -> u32 {
        self.0 as u32 * 0x1000
    }
}

impl core::fmt::Display for Pipe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "pipe {}", self.name())
    }
}

// ---- transcoders and pipes ---------------------------------------------

/// `TRANSCONF`: the transcoder (and so its pipe) is on.
pub const fn transconf(p: Pipe) -> u32 {
    0x70008 + p.step()
}
pub const TRANSCONF_ENABLE: u32 = 1 << 31;
pub const TRANSCONF_STATE_ENABLE: u32 = 1 << 30;

/// `PIPESRC`: the size of the picture the pipe takes from its planes
/// (width − 1 in bits 31:16, height − 1 in 15:0).
pub const fn pipesrc(p: Pipe) -> u32 {
    0x6001C + p.step()
}

/// `PIPE_FRMCOUNT_G4X`: frames counted, at the start of each vertical blank.
pub const fn frame_count(p: Pipe) -> u32 {
    0x70040 + p.step()
}

/// `TRANS_HTOTAL` and `TRANS_VTOTAL`: total − 1 in bits 31:16, active − 1
/// in 15:0.
pub const fn htotal(p: Pipe) -> u32 {
    0x60000 + p.step()
}
pub const fn vtotal(p: Pipe) -> u32 {
    0x6000C + p.step()
}
/// `TRANS_VBLANK`: the vertical blank's end − 1 in bits 31:16, its start
/// − 1 in 15:0 (in lines).
pub const fn vblank(p: Pipe) -> u32 {
    0x60010 + p.step()
}

/// `PIPEDSL`: the line the pipe is scanning out. It counts at the start of
/// each line, one behind: as the vertical blank begins it still reads the
/// blank's first line − 1.
pub const fn scanline(p: Pipe) -> u32 {
    0x70000 + p.step()
}
pub const SCANLINE_MASK: u32 = 0xF_FFFF;

/// `TRANS_PSR_CTL` and `TRANS_PSR2_CTL`: panel self refresh (bit 31 on).
pub const fn psr_ctl(p: Pipe) -> u32 {
    0x60800 + p.step()
}
pub const fn psr2_ctl(p: Pipe) -> u32 {
    0x60900 + p.step()
}
pub const PSR_ENABLE: u32 = 1 << 31;

/// `PS_CTRL`: the pipe's scalers (`PS_SCALER_EN`, bit 31), two per pipe.
pub const fn scaler_ctl(p: Pipe, scaler: u8) -> u32 {
    0x68180 + p.0 as u32 * 0x800 + scaler as u32 * 0x100
}
pub const SCALER_ENABLE: u32 = 1 << 31;

// ---- universal planes (1 is the primary) --------------------------------

/// Universal planes per pipe that a firmware might leave on.
pub const PLANES: u8 = 7;

/// `PLANE_CTL` of `plane` (1-based) on `pipe`; the plane's other registers
/// follow at fixed distances.
pub const fn plane_ctl(p: Pipe, plane: u8) -> u32 {
    0x70180 + p.step() + (plane as u32 - 1) * 0x100
}
pub const fn plane_stride(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x08
}
pub const fn plane_pos(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x0C
}
pub const fn plane_size(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x10
}
/// `PLANE_SURF`: where the plane's picture is in the GPU's address space.
/// Writing it arms every plane register written before: they all take
/// effect at the start of the next vertical blank.
pub const fn plane_surf(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x1C
}
pub const fn plane_offset(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x24
}
/// `PLANE_SURFLIVE`: the surface the plane is scanning out now.
pub const fn plane_surf_live(p: Pipe, plane: u8) -> u32 {
    plane_ctl(p, plane) + 0x2C
}

pub const PLANE_CTL_ENABLE: u32 = 1 << 31;
/// The pixel format (`PLANE_CTL_FORMAT_MASK_ICL`, bits 27:23); 8 is
/// `PLANE_CTL_FORMAT_XRGB_8888` (4 in Skylake's bits 27:24).
pub const fn plane_format(ctl: u32) -> u32 {
    (ctl >> 23) & 0x1F
}
pub const FORMAT_8888: u32 = 8;
/// Red in the low byte (`PLANE_CTL_ORDER_RGBX`): XBGR rather than XRGB.
pub const PLANE_CTL_ORDER_RGBX: u32 = 1 << 20;
/// Tiling (`PLANE_CTL_TILED_MASK`, bits 12:10; 0 is linear).
pub const fn plane_tiling(ctl: u32) -> u32 {
    (ctl >> 10) & 0x7
}
pub const PLANE_CTL_ASYNC_FLIP: u32 = 1 << 9;
/// Rotation (bits 1:0; 0 is none).
pub const fn plane_rotation(ctl: u32) -> u32 {
    ctl & 0x3
}
/// `PLANE_SURF_ADDR_MASK`.
pub const PLANE_SURF_MASK: u32 = 0xFFFF_F000;
/// The stride of a linear plane is in 64-byte units (bits 11:0).
pub const fn plane_stride_bytes(stride: u32) -> u32 {
    (stride & 0xFFF) * 64
}

/// The first line of the picture the plane shows (`PLANE_OFFSET_Y`, bits
/// 28:16).
pub const fn plane_offset_y(offset: u32) -> u32 {
    (offset >> 16) & 0x1FFF
}

/// Linear planes' surfaces start on 256 KiB in the GPU's address space
/// (`intel_linear_alignment`).
pub const SURFACE_ALIGN: u64 = 256 * 1024;

// ---- the cursor ------------------------------------------------------------

/// `CURCNTR`: the pipe's cursor plane, on while any of its mode bits
/// (`MCURSOR_MODE_MASK`) is set.
pub const fn cursor_ctl(p: Pipe) -> u32 {
    0x70080 + p.step()
}
pub const CURSOR_MODE: u32 = 0x27;
/// `CURBASE`: where the cursor's picture is in the GPU's address space.
pub const fn cursor_base(p: Pipe) -> u32 {
    0x70084 + p.step()
}
/// The largest cursor picture: 256 x 256 pixels of 4 bytes.
pub const CURSOR_BYTES: u64 = 256 * 256 * 4;

// ---- interrupts ----------------------------------------------------------

/// `GEN11_GFX_MSTR_IRQ`: the GPU's master interrupt control and status.
pub const MASTER_IRQ: u32 = 0x190010;
/// `GEN11_MASTER_IRQ`: interrupts on.
pub const MASTER_IRQ_ENABLE: u32 = 1 << 31;
/// `GEN11_DISPLAY_IRQ`: the display engine has something.
pub const MASTER_IRQ_DISPLAY: u32 = 1 << 16;

/// `GEN11_DISPLAY_INT_CTL`: the display engine's interrupt control.
pub const DISPLAY_INT_CTL: u32 = 0x44200;
pub const DISPLAY_IRQ_ENABLE: u32 = 1 << 31;
/// `GEN8_DE_PIPE_IRQ(pipe)`: a pipe has interrupts pending.
pub const fn display_int_pipe(p: Pipe) -> u32 {
    1 << (16 + p.0 as u32)
}
/// The engine's interrupt sources in `DISPLAY_INT_CTL` (bits 23:16: the
/// pipes, then its ports, miscellany and hot plugging).
pub const DISPLAY_SOURCES: u32 = 0x00FF_0000;

/// `GEN8_DE_PIPE_ISR`, `IMR`, `IIR`, `IER`.
pub const fn pipe_isr(p: Pipe) -> u32 {
    0x44400 + 0x10 * p.0 as u32
}
pub const fn pipe_imr(p: Pipe) -> u32 {
    pipe_isr(p) + 0x4
}
pub const fn pipe_iir(p: Pipe) -> u32 {
    pipe_isr(p) + 0x8
}
pub const fn pipe_ier(p: Pipe) -> u32 {
    pipe_isr(p) + 0xC
}
/// `GEN8_PIPE_VBLANK`.
pub const PIPE_VBLANK: u32 = 1 << 0;
/// `GEN9_PIPE_PLANE1_FAULT`: plane 1 read memory its address table does
/// not map; `GEN8_PIPE_FIFO_UNDERRUN`: the pipe ran out of pixels to send.
/// Either means the screen showed garbage.
pub const PIPE_PLANE1_FAULT: u32 = 1 << 7;
pub const PIPE_FIFO_UNDERRUN: u32 = 1 << 31;

// ---- the global address table --------------------------------------------

/// `GFX_FLSH_CNTL_GEN6`: written after the table changes, so that the
/// GPU's walkers see it.
pub const GFX_FLUSH_CNTL: u32 = 0x101008;
pub const GFX_FLUSH_CNTL_EN: u32 = 1 << 0;
/// A table entry maps a page when bit 0 (`GEN8_PAGE_PRESENT`) is set; the
/// page's physical address is in the bits above 11.
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// ---- PCI configuration space ----------------------------------------------

/// `SNB_GMCH_CTRL` (16 bits): the size of the stolen memory and of the table.
pub const PCI_GMCH_CTRL: u16 = 0x50;
/// The table's size: `1 << GGMS` MiB of entries (`BDW_GMCH_GGMS`, bits 7:6).
pub const fn gmch_table_mib(ctl: u16) -> u64 {
    match (ctl >> 6) & 0x3 {
        0 => 0,
        g => 1 << g,
    }
}
