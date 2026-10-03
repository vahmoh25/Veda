//! The boot handoff protocol between `vboot` (the UEFI loader) and `vkernel`.
//!
//! When the loader jumps to the kernel entry point, the CPU is in 64-bit long
//! mode with interrupts disabled and paging enabled using page tables built by
//! the loader. Those tables contain:
//!
//! * an identity map of low physical memory (so the loader can survive the
//!   switch; the kernel drops it as soon as it installs its own tables),
//! * a direct map of all physical RAM and the low 4 GiB at [`HHDM_BASE`],
//! * the kernel image at its link address, with per-section permissions.
//!
//! The entry point is `extern "sysv64" fn(&'static BootInfo) -> !`. Every
//! pointer stored inside [`BootInfo`] is a *virtual* address inside the direct
//! map, so the kernel can use it immediately.
//!
//! All structures are `#[repr(C)]` and contain only plain data, because the
//! loader and kernel are separate binaries that must agree on the layout.

#![no_std]

/// Virtual base address of the higher-half direct map of physical memory.
pub const HHDM_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Link address of the kernel image.
pub const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// `BootInfo::magic` value (`"VINDBOOT"` in little-endian).
pub const BOOTINFO_MAGIC: u64 = u64::from_le_bytes(*b"VINDBOOT");

/// Version of this protocol. Bump on any layout change.
pub const BOOTINFO_VERSION: u32 = 2;

/// Maximum length of the kernel command line, in bytes.
pub const CMDLINE_MAX: usize = 256;

/// Size of the entropy seed the loader passes to the kernel.
pub const ENTROPY_MAX: usize = 64;

/// Everything the kernel needs to know about the machine at boot.
#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    /// Must equal [`BOOTINFO_MAGIC`].
    pub magic: u64,
    /// Must equal [`BOOTINFO_VERSION`].
    pub version: u32,
    /// Size of this structure in bytes (sanity check).
    pub size: u32,
    /// Virtual address at which physical address 0 is mapped.
    pub hhdm_base: u64,
    /// Highest physical address (exclusive) covered by the direct map.
    pub hhdm_limit: u64,
    /// Physical memory layout, sorted by start address, non-overlapping.
    pub memory_map: MemoryMap,
    /// The loaded kernel image.
    pub kernel: KernelImage,
    /// Linear framebuffer configured through UEFI GOP.
    pub framebuffer: Framebuffer,
    /// The initial ramdisk (a `VINITRD` archive), see the `initrd` crate.
    pub initrd: PhysRegion,
    /// Optional kernel symbol table produced at build time (may be empty).
    pub symbols: PhysRegion,
    /// Physical address of the ACPI RSDP, or 0 if the firmware has none.
    pub rsdp_phys: u64,
    /// Wall-clock time read from the firmware just before handoff.
    pub boot_time: BootTime,
    /// Stack the kernel was entered on (physical range).
    pub boot_stack: PhysRegion,
    /// Kernel command line (UTF-8, not NUL terminated).
    pub cmdline: [u8; CMDLINE_MAX],
    /// Number of valid bytes in [`BootInfo::cmdline`].
    pub cmdline_len: u32,
    /// Number of valid bytes in [`BootInfo::entropy`].
    pub entropy_len: u32,
    /// Random bytes from the firmware's `EFI_RNG_PROTOCOL` (if it has one),
    /// which seed the kernel's random number generator.
    pub entropy: [u8; ENTROPY_MAX],
}

impl BootInfo {
    /// Returns the command line as a string slice (empty if not valid UTF-8).
    pub fn cmdline(&self) -> &str {
        let len = (self.cmdline_len as usize).min(CMDLINE_MAX);
        core::str::from_utf8(&self.cmdline[..len]).unwrap_or("")
    }

    /// The firmware entropy seed (empty if the firmware had none).
    pub fn entropy(&self) -> &[u8] {
        &self.entropy[..(self.entropy_len as usize).min(ENTROPY_MAX)]
    }

    /// Returns `true` if the magic, version and size fields are what this
    /// build of the protocol expects.
    pub fn is_valid(&self) -> bool {
        self.magic == BOOTINFO_MAGIC
            && self.version == BOOTINFO_VERSION
            && self.size as usize == core::mem::size_of::<BootInfo>()
    }
}

/// A physical address range.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhysRegion {
    pub base: u64,
    pub size: u64,
}

impl PhysRegion {
    pub const EMPTY: PhysRegion = PhysRegion { base: 0, size: 0 };

    pub const fn end(&self) -> u64 {
        self.base + self.size
    }

    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }
}

/// Where the kernel image lives in physical and virtual memory.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelImage {
    pub phys_base: u64,
    pub virt_base: u64,
    pub size: u64,
}

/// The physical memory map, an array of [`MemoryRegion`]s.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryMap {
    /// Virtual (direct-map) address of the first entry.
    pub entries: *const MemoryRegion,
    pub len: u64,
}

impl MemoryMap {
    /// # Safety
    /// `entries` must point to `len` valid regions that outlive the slice.
    pub unsafe fn as_slice(&self) -> &[MemoryRegion] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: guaranteed by the caller (the loader built this array).
        unsafe { core::slice::from_raw_parts(self.entries, self.len as usize) }
    }
}

/// One contiguous range of physical memory.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub base: u64,
    pub pages: u64,
    pub kind: MemoryKind,
    pub _pad: u32,
}

impl MemoryRegion {
    pub const PAGE_SIZE: u64 = 4096;

    pub const fn end(&self) -> u64 {
        self.base + self.pages * Self::PAGE_SIZE
    }
}

/// How a physical memory range may be used by the kernel.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM.
    Usable = 1,
    /// Firmware reserved; never touch.
    Reserved = 2,
    /// ACPI tables; reclaimable once the kernel has parsed them.
    AcpiReclaimable = 3,
    /// ACPI non-volatile storage; never touch.
    AcpiNvs = 4,
    /// Memory-mapped I/O.
    Mmio = 5,
    /// Loader code and scratch data. Free once the kernel no longer needs the
    /// loader's identity map (i.e. after it has switched page tables).
    LoaderReclaimable = 6,
    /// The kernel image.
    Kernel = 7,
    /// The initial ramdisk.
    Initrd = 8,
    /// Boot info, memory map, boot stack and the loader-built page tables.
    BootData = 9,
    /// Defective or otherwise unusable RAM.
    Unusable = 10,
    /// Kernel symbol table.
    Symbols = 11,
}

impl MemoryKind {
    /// RAM that the kernel may hand out once boot-time data is consumed.
    pub const fn is_reclaimable_ram(self) -> bool {
        matches!(self, MemoryKind::Usable | MemoryKind::LoaderReclaimable)
    }
}

/// Pixel layout of a 32-bit-per-pixel framebuffer.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Byte order B, G, R, X in memory (`0xXXRRGGBB` as a little-endian u32).
    Bgrx = 1,
    /// Byte order R, G, B, X in memory.
    Rgbx = 2,
}

/// A linear framebuffer.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Framebuffer {
    /// Physical address of the first pixel; 0 if no framebuffer is present.
    pub phys_base: u64,
    /// Size of the framebuffer memory in bytes.
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels (not bytes) per scan line.
    pub stride: u32,
    pub format: PixelFormat,
}

/// Calendar time as reported by the firmware's real-time clock.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BootTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 1 if the fields above are valid.
    pub valid: u8,
    /// Offset from UTC in minutes, or `i16::MAX` if unknown (assume UTC).
    pub utc_offset_minutes: i16,
    pub _pad: [u8; 6],
}

// The layouts are shared between two separately compiled binaries: catch
// accidental changes at compile time.
const _: () = {
    assert!(core::mem::size_of::<MemoryRegion>() == 24);
    assert!(core::mem::size_of::<Framebuffer>() == 32);
    assert!(core::mem::size_of::<BootTime>() == 16);
};
