//! `vboot` — the Veda UEFI boot loader.
//!
//! Responsibilities, in order:
//!
//! 1. read `\VEDA\BOOT.CFG`, `VKERNEL.EXE`, `INITRD.IMG` (and the optional
//!    `VKERNEL.SYM`) from the boot volume,
//! 2. pick and set a graphics mode (of the display's own shape, where the
//!    firmware gives its EDID) and clear the screen: it stays blank until
//!    the window system's splash fades in,
//! 3. load the kernel's PE sections into fresh physical pages,
//! 4. build the initial page tables (identity map, direct map, kernel image),
//! 5. read the firmware's variables that an operating system may read
//!    ([`bootinfo::variables`]),
//! 6. exit boot services, translate the firmware memory map into the
//!    [`bootinfo`] format, and
//! 7. switch to the new address space and jump to the kernel.

#![no_std]
#![no_main]

mod blank;
mod config;
mod memtype;
mod paging;
mod serial;
mod uefi;

use core::ffi::c_void;
use core::ptr;

use bootinfo::{
    BOOTINFO_MAGIC, BOOTINFO_VERSION, BootInfo, BootTime, Framebuffer, HHDM_BASE, KernelImage, MemoryKind, MemoryMap,
    MemoryRegion, PaintReport, PhysRegion, PixelFormat, SCREEN_MODES, ScreenReport, made_wc, memory_type, variables,
};
use uefi::{memory_type as mt, *};

const PAGE: u64 = 4096;
const BOOT_STACK_SIZE: u64 = 128 * 1024;
/// Most separately recorded allocations handed to the kernel.
const MAX_TAGS: usize = 256;
/// Most variables the loader takes from the firmware: more than any keeps,
/// and an end for one that lists a variable again and again.
const MAX_VARIABLES: usize = 4096;

/// The allocations handed to the kernel and what they hold.
///
/// Everything is allocated as ordinary loader data and told apart here,
/// not with the OS-defined memory types the UEFI specification allows:
/// some firmware mishandles those (VirtualBox's indexes a table with them
/// and crashes).
struct Tags {
    list: [(u64, u64, MemoryKind); MAX_TAGS],
    len: usize,
}

impl Tags {
    fn add(&mut self, base: u64, pages: u64, kind: MemoryKind) -> Result<()> {
        // Extend the last range when this one follows it.
        if let Some(last) = self.list[..self.len].last_mut()
            && last.2 == kind
            && last.0 + last.1 * PAGE == base
        {
            last.1 += pages;
            return Ok(());
        }
        let slot = self.list.get_mut(self.len).ok_or("too many boot allocations")?;
        *slot = (base, pages, kind);
        self.len += 1;
        Ok(())
    }
}

type Result<T> = core::result::Result<T, &'static str>;

/// Memory the loader reads into from the firmware, as much as that needs:
/// loader data, which the kernel takes back.
struct Buffer {
    base: u64,
    len: usize,
}

impl Buffer {
    /// `len` bytes, zeroed.
    fn new(fw: &Firmware, len: usize) -> Result<Buffer> {
        Ok(Buffer { base: fw.alloc_zeroed(len as u64, None)?, len })
    }

    /// A buffer of `len` bytes (or this one's, if more) holding what this
    /// one does.
    fn grown(&self, fw: &Firmware, len: usize) -> Result<Buffer> {
        let grown = Buffer::new(fw, len.max(self.len))?;
        // SAFETY: two of the loader's buffers, apart.
        unsafe { ptr::copy_nonoverlapping(self.base as *const u8, grown.base as *mut u8, self.len) };
        Ok(grown)
    }

    fn ptr<T>(&self) -> *mut T {
        self.base as *mut T
    }

    /// Its first `len` bytes.
    fn bytes(&self, len: usize) -> &[u8] {
        // SAFETY: the buffer's own memory.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, len.min(self.len)) }
    }

    /// The UTF-16 string it holds, without the NUL.
    fn string(&self) -> &[u16] {
        // SAFETY: the buffer's own memory, page-aligned.
        let units = unsafe { core::slice::from_raw_parts(self.base as *const u16, self.len / 2) };
        &units[..units.iter().position(|&u| u == 0).unwrap_or(units.len())]
    }
}

/// Thin safe-ish wrapper around the firmware tables.
struct Firmware {
    image: Handle,
    st: &'static SystemTable,
    bs: &'static BootServices,
    tags: core::cell::RefCell<Tags>,
}

impl Firmware {
    fn print(&self, msg: &str) {
        let mut buf = [0u16; 128];
        let mut n = 0;
        let out = self.st.con_out;
        let flush = |buf: &mut [u16; 128], n: &mut usize| {
            buf[*n] = 0;
            // SAFETY: ConOut is valid while boot services are active and
            // `buf` is NUL terminated.
            unsafe { ((*out).output_string)(out, buf.as_ptr()) };
            *n = 0;
        };
        for c in msg.chars() {
            if c == '\n' {
                buf[n] = '\r' as u16;
                n += 1;
            }
            buf[n] = if (c as u32) < 0x10000 { c as u16 } else { '?' as u16 };
            n += 1;
            if n >= 120 {
                flush(&mut buf, &mut n);
            }
        }
        flush(&mut buf, &mut n);
    }

    /// Allocates loader-data pages; `tag` records what they will hold for
    /// the kernel (`None`: scratch the kernel may reuse).
    fn alloc_pages(&self, pages: u64, tag: Option<MemoryKind>) -> Result<u64> {
        let mut addr = 0u64;
        // SAFETY: plain firmware call with valid out-pointer.
        let s = unsafe { (self.bs.allocate_pages)(AllocateType::AnyPages, mt::LOADER_DATA, pages as usize, &mut addr) };
        if is_error(s) {
            return Err("out of memory");
        }
        if let Some(kind) = tag {
            self.tags.borrow_mut().add(addr, pages, kind)?;
        }
        Ok(addr)
    }

    fn alloc_zeroed(&self, bytes: u64, tag: Option<MemoryKind>) -> Result<u64> {
        let pages = bytes.div_ceil(PAGE);
        let addr = self.alloc_pages(pages, tag)?;
        // SAFETY: freshly allocated, identity-mapped pages.
        unsafe { ptr::write_bytes(addr as *mut u8, 0, (pages * PAGE) as usize) };
        Ok(addr)
    }

    fn protocol<T>(&self, handle: Handle, guid: &Guid) -> Result<*mut T> {
        let mut iface: *mut c_void = ptr::null_mut();
        // SAFETY: valid handle and GUID; the firmware fills `iface`.
        let s = unsafe { (self.bs.handle_protocol)(handle, guid, &mut iface) };
        if is_error(s) || iface.is_null() { Err("protocol not supported") } else { Ok(iface.cast()) }
    }

    fn locate<T>(&self, guid: &Guid) -> Result<*mut T> {
        let mut iface: *mut c_void = ptr::null_mut();
        // SAFETY: as above.
        let s = unsafe { (self.bs.locate_protocol)(guid, ptr::null_mut(), &mut iface) };
        if is_error(s) || iface.is_null() { Err("protocol not found") } else { Ok(iface.cast()) }
    }

    /// The own size of the display on graphics output `gop`, as its EDID
    /// gives it (the firmware's active one, else the one it discovered), if
    /// the firmware gives one there.
    fn display_size(&self, gop: *mut GraphicsOutput) -> Option<(u32, u32)> {
        let (mut count, mut handles) = (0usize, ptr::null_mut::<Handle>());
        // SAFETY: the firmware allocates the list of handles.
        let s = unsafe {
            (self.bs.locate_handle_buffer)(
                BY_PROTOCOL,
                &GRAPHICS_OUTPUT_PROTOCOL,
                ptr::null_mut(),
                &mut count,
                &mut handles,
            )
        };
        if is_error(s) || handles.is_null() {
            return None;
        }
        // SAFETY: `count` handles at `handles`.
        let list = unsafe { core::slice::from_raw_parts(handles, count) };
        let output = list.iter().find(|&&h| self.protocol::<GraphicsOutput>(h, &GRAPHICS_OUTPUT_PROTOCOL) == Ok(gop));
        let size = output.and_then(|&h| {
            [EDID_ACTIVE_PROTOCOL, EDID_DISCOVERED_PROTOCOL].iter().find_map(|guid| {
                let edid: *mut Edid = self.protocol(h, guid).ok()?;
                // SAFETY: the firmware's EDID protocol: `size_of_edid` bytes at
                // `edid`.
                let (size, at) = unsafe { ((*edid).size_of_edid as usize, (*edid).edid) };
                if at.is_null() {
                    return None;
                }
                preferred_timing(unsafe { core::slice::from_raw_parts(at, size) })
            })
        });
        // SAFETY: the list the firmware allocated, no longer used.
        unsafe { (self.bs.free_pool)(handles.cast()) };
        size
    }

    /// Opens the root directory of the volume this loader was started from.
    fn boot_volume(&self) -> Result<*mut FileProtocol> {
        let loaded: *mut LoadedImage = self.protocol(self.image, &LOADED_IMAGE_PROTOCOL)?;
        // SAFETY: the firmware returned a valid LoadedImage protocol.
        let device = unsafe { (*loaded).device_handle };
        let fs: *mut SimpleFileSystem = self.protocol(device, &SIMPLE_FILE_SYSTEM_PROTOCOL)?;
        let mut root = ptr::null_mut();
        // SAFETY: valid protocol pointer.
        let s = unsafe { ((*fs).open_volume)(fs, &mut root) };
        if is_error(s) { Err("cannot open boot volume") } else { Ok(root) }
    }

    /// Reads a whole file into newly allocated pages (recorded as `tag`).
    /// Returns `None` if the file does not exist.
    fn read_file(&self, root: *mut FileProtocol, path: &str, tag: Option<MemoryKind>) -> Result<Option<PhysRegion>> {
        let mut name = [0u16; 64];
        for (i, c) in path.encode_utf16().enumerate().take(63) {
            name[i] = c;
        }
        let mut file = ptr::null_mut();
        // SAFETY: `root` is an open directory and `name` is NUL terminated.
        let s = unsafe { ((*root).open)(root, &mut file, name.as_ptr(), FILE_MODE_READ, 0) };
        if s == NOT_FOUND {
            return Ok(None);
        }
        if is_error(s) {
            return Err("cannot open file");
        }
        #[repr(C, align(8))]
        struct InfoBuf([u8; 512]);
        let mut info = InfoBuf([0; 512]);
        let mut info_size = info.0.len();
        // SAFETY: `file` is open; the buffer is large enough for a short name.
        let s = unsafe { ((*file).get_info)(file, &FILE_INFO, &mut info_size, info.0.as_mut_ptr()) };
        if is_error(s) {
            return Err("cannot stat file");
        }
        // SAFETY: the firmware wrote an EFI_FILE_INFO into the aligned buffer.
        let size = unsafe { (*(info.0.as_ptr() as *const FileInfo)).file_size };
        let base = self.alloc_zeroed(size.max(1), tag)?;
        let mut done = 0u64;
        while done < size {
            let mut chunk = (size - done).min(16 << 20) as usize;
            // SAFETY: reading into our freshly allocated buffer.
            let s = unsafe { ((*file).read)(file, &mut chunk, (base + done) as *mut u8) };
            if is_error(s) || chunk == 0 {
                return Err("file read failed");
            }
            done += chunk as u64;
        }
        // SAFETY: closing an open file handle.
        unsafe { ((*file).close)(file) };
        Ok(Some(PhysRegion { base, size }))
    }

    fn config_table(&self, guid: &Guid) -> Option<u64> {
        // SAFETY: the configuration table has `number_of_table_entries` entries.
        let tables =
            unsafe { core::slice::from_raw_parts(self.st.configuration_table, self.st.number_of_table_entries) };
        tables.iter().find(|t| t.vendor_guid == *guid).map(|t| t.vendor_table as u64)
    }

    /// Random bytes from `EFI_RNG_PROTOCOL` for the kernel's generator.
    /// Returns the buffer and the number of valid bytes (0 if the firmware
    /// has no RNG; the kernel then relies on CPU and timing sources).
    fn entropy(&self) -> ([u8; bootinfo::ENTROPY_MAX], usize) {
        let mut buf = [0u8; bootinfo::ENTROPY_MAX];
        let Ok(rng) = self.locate::<RngProtocol>(&RNG_PROTOCOL) else { return (buf, 0) };
        // SAFETY: a valid protocol instance and a buffer of the stated size.
        let s = unsafe { ((*rng).get_rng)(rng, ptr::null(), buf.len(), buf.as_mut_ptr()) };
        if is_error(s) { ([0; bootinfo::ENTROPY_MAX], 0) } else { (buf, buf.len()) }
    }

    /// Calls `f` with each of the firmware's variables that an operating
    /// system may read while it runs (those with runtime access): its
    /// vendor, its attributes, its name (UTF-16, without the NUL) and its
    /// data.
    fn for_each_variable(&self, mut f: impl FnMut(&Guid, u32, &[u16], &[u8])) -> Result<()> {
        // SAFETY: the runtime services are valid during boot.
        let rt = unsafe { &*self.st.runtime_services };
        // Zeroed: the empty name, from which the list starts.
        let mut name = Buffer::new(self, PAGE as usize)?;
        let mut data = Buffer::new(self, PAGE as usize)?;
        let mut guid = Guid(0, 0, 0, [0; 8]);
        for _ in 0..MAX_VARIABLES {
            let mut size = name.len;
            // SAFETY: `name` holds, in `size` bytes, the name the firmware
            // gave last (or the empty one).
            match unsafe { (rt.get_next_variable_name)(&mut size, name.ptr(), &mut guid) } {
                SUCCESS => {}
                NOT_FOUND => return Ok(()),
                BUFFER_TOO_SMALL => {
                    name = name.grown(self, size)?;
                    continue;
                }
                _ => return Err("the firmware cannot list its variables"),
            }
            let (mut attributes, mut size) = (0, data.len);
            // SAFETY: a name the firmware gave, and a buffer of `size` bytes.
            let mut status = unsafe { (rt.get_variable)(name.ptr(), &guid, &mut attributes, &mut size, data.ptr()) };
            if status == BUFFER_TOO_SMALL
                && let Ok(bigger) = Buffer::new(self, size)
            {
                data = bigger;
                size = data.len;
                // SAFETY: as above.
                status = unsafe { (rt.get_variable)(name.ptr(), &guid, &mut attributes, &mut size, data.ptr()) };
            }
            // One it cannot read (or find the memory for) is left out.
            if status == SUCCESS && attributes & variables::attributes::RUNTIME_ACCESS != 0 {
                f(&guid, attributes, name.string(), data.bytes(size));
            }
        }
        Err("the firmware lists too many variables")
    }

    /// The firmware's variables that an operating system may read (those
    /// with runtime access), as `bootinfo::variables` records in memory of
    /// their own; none if it has none.
    fn variables(&self) -> Result<PhysRegion> {
        // How much memory they take, then the records.
        let mut size = variables::HEADER_SIZE;
        self.for_each_variable(|_, _, name, data| {
            size = size.saturating_add(variables::record_size(name.len(), data.len()).unwrap_or(usize::MAX));
        })?;
        if size == variables::HEADER_SIZE {
            return Ok(PhysRegion::EMPTY);
        }
        let base = self.alloc_zeroed(size as u64, Some(MemoryKind::FirmwareVariables))?;
        // SAFETY: freshly allocated, identity-mapped memory of `size` bytes.
        let out = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, size) };
        let mut records = variables::Writer::new(out).ok_or("no memory for the variables")?;
        let (mut count, mut more) = (0, 0);
        self.for_each_variable(|guid, attributes, name, data| {
            if records.push(&guid.to_bytes(), attributes, name, data) {
                count += 1;
            } else {
                more += 1;
            }
        })?;
        let size = records.finish();
        log!("variables: {} that an operating system may read ({} bytes)", count, size);
        if more > 0 {
            log!("variables: {} more the second time the firmware listed them, left out", more);
        }
        Ok(PhysRegion { base, size: size as u64 })
    }

    fn boot_time(&self) -> BootTime {
        let mut t = Time::default();
        // SAFETY: runtime services are valid during boot.
        let s = unsafe { ((*self.st.runtime_services).get_time)(&mut t, ptr::null_mut()) };
        if is_error(s) {
            return BootTime::default();
        }
        BootTime {
            year: t.year,
            month: t.month,
            day: t.day,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
            valid: 1,
            utc_offset_minutes: if t.time_zone == UNSPECIFIED_TIMEZONE { i16::MAX } else { -t.time_zone },
            _pad: [0; 6],
        }
    }
}

/// Selects and activates a graphics mode ([`choose_mode`]): the
/// framebuffer, and the modes there were, for the kernel's log.
fn setup_graphics(fw: &Firmware, preferred: Option<(u32, u32)>) -> Result<(Framebuffer, ScreenReport)> {
    let gop: *mut GraphicsOutput = fw.locate(&GRAPHICS_OUTPUT_PROTOCOL)?;
    // SAFETY: valid GOP instance from the firmware.
    let gop_ref = unsafe { &mut *gop };
    // SAFETY: the mode structure of a valid GOP.
    let (max_mode, entered) = unsafe { ((*gop_ref.mode).max_mode, (*gop_ref.mode).mode) };
    let mut report = ScreenReport { chosen: u8::MAX, entered: u8::MAX, ..ScreenReport::default() };
    // The modes of 32-bit pixels: their numbers and sizes.
    let mut numbers = [0u32; SCREEN_MODES];
    let mut sizes = [(0u32, 0u32); SCREEN_MODES];
    let mut count = 0;
    for m in 0..max_mode {
        if count == SCREEN_MODES {
            break;
        }
        let mut size = 0usize;
        let mut info: *mut GraphicsModeInfo = ptr::null_mut();
        // SAFETY: querying a mode index below max_mode.
        if is_error(unsafe { (gop_ref.query_mode)(gop, m, &mut size, &mut info) }) || info.is_null() {
            continue;
        }
        let info = unsafe { &*info };
        if info.pixel_format != PIXEL_BGR_RESERVED_8BIT && info.pixel_format != PIXEL_RGB_RESERVED_8BIT {
            continue;
        }
        let (w, h) = (info.horizontal_resolution, info.vertical_resolution);
        if m == entered {
            report.entered = count as u8;
        }
        (numbers[count], sizes[count]) = (m, (w, h));
        report.modes[count] = [w.min(u16::MAX as u32) as u16, h.min(u16::MAX as u32) as u16];
        count += 1;
    }
    report.count = count as u8;
    let own = fw.display_size(gop);
    if let Some((w, h)) = own {
        report.own = [w.min(u16::MAX as u32) as u16, h.min(u16::MAX as u32) as u16];
    }
    if let Some(i) = choose_mode(&sizes[..count], preferred.unwrap_or((1280, 800)), own) {
        report.chosen = i as u8;
        // SAFETY: valid mode number.
        unsafe { (gop_ref.set_mode)(gop, numbers[i]) };
    }
    // SAFETY: the mode structure is valid after SetMode.
    let mode = unsafe { &*gop_ref.mode };
    let info = unsafe { &*mode.info };
    let framebuffer = Framebuffer {
        phys_base: mode.frame_buffer_base,
        size: mode.frame_buffer_size as u64,
        width: info.horizontal_resolution,
        height: info.vertical_resolution,
        stride: info.pixels_per_scan_line,
        format: if info.pixel_format == PIXEL_RGB_RESERVED_8BIT { PixelFormat::Rgbx } else { PixelFormat::Bgrx },
    };
    Ok((framebuffer, report))
}

/// The mode to set of `modes` (their widths and heights) for a screen of
/// about the `preferred` size: of the display's own shape (its own size
/// `own`, if the firmware gives it), as wide as the preferred size or up
/// to an eighth narrower, the widest such (1920x1200 for 1920x1080 on a
/// 16:10 panel). The firmware then scales the picture evenly over the
/// whole panel, as the display's driver does the window system's later:
/// the screen keeps its shape when the driver takes over, and has no bars.
/// Else the preferred size itself; else the largest within it; else the
/// smallest beyond it.
fn choose_mode(modes: &[(u32, u32)], (pw, ph): (u32, u32), own: Option<(u32, u32)>) -> Option<usize> {
    let shape = |(w, h): (u32, u32), (ow, oh): (u32, u32)| w as u64 * oh as u64 == h as u64 * ow as u64;
    if let Some((ow, oh)) = own
        && !shape((pw, ph), (ow, oh))
    {
        let fits = |&(w, h): &(u32, u32)| shape((w, h), (ow, oh)) && w <= pw && w * 8 >= pw * 7 && w <= ow && h <= oh;
        let widest = modes.iter().enumerate().filter(|(_, m)| fits(m)).max_by_key(|(_, (w, _))| *w);
        if let Some((i, _)) = widest {
            return Some(i);
        }
    }
    let mut best: Option<(usize, u64)> = None;
    for (i, &(w, h)) in modes.iter().enumerate() {
        // Exact match wins; otherwise prefer the closest size not exceeding
        // the preferred one.
        let score = if (w, h) == (pw, ph) {
            0
        } else if w <= pw && h <= ph {
            1 + ((pw - w) as u64 * ph as u64 + (ph - h) as u64 * pw as u64)
        } else {
            u64::MAX / 2 + (w as u64 * h as u64)
        };
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((i, score));
        }
    }
    best.map(|(i, _)| i)
}

/// The size of an EDID's preferred timing (its first detailed timing
/// descriptor): the display's own resolution.
fn preferred_timing(edid: &[u8]) -> Option<(u32, u32)> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let d = &edid[54..72];
    // A timing has a pixel clock; a descriptor of another kind has none.
    if d[0] == 0 && d[1] == 0 {
        return None;
    }
    let w = d[2] as u32 | (d[4] as u32 & 0xF0) << 4;
    let h = d[5] as u32 | (d[7] as u32 & 0xF0) << 4;
    (w > 0 && h > 0).then_some((w, h))
}

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reading the timestamp counter.
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

/// Clears the screen write-combining if the framebuffer is not (see
/// `blank`); how it went, for the kernel's log.
fn clear_screen(fw: &Firmware, fb: &Framebuffer) -> PaintReport {
    let mut report = PaintReport::default();
    if fb.phys_base == 0 {
        return report;
    }
    let surface =
        blank::Surface { base: fb.phys_base as *mut u32, width: fb.width, height: fb.height, stride: fb.stride };
    let start = rdtsc();
    report.found = memtype::memory_type(fb.phys_base);
    if report.found == memory_type::WRITE_COMBINING {
        blank::clear(&surface);
        report.painted = report.found;
        report.made_wc = made_wc::ALREADY;
        report.paint_ticks = (rdtsc() - start).max(1);
        return report;
    }
    let painted = ram_limit(fw)
        .and_then(|limit| blank::clear_write_combining(&mut ScratchFrames(fw), &surface, limit, supports_1g_pages()));
    match painted {
        Ok(ticks) => {
            report.painted = memory_type::WRITE_COMBINING;
            report.made_wc = made_wc::PAGE_ATTRIBUTES;
            report.paint_ticks = ticks.max(1);
            report.setup_ticks = (rdtsc() - start).saturating_sub(ticks);
        }
        Err(e) => {
            log!("cannot clear the screen write-combining: {e}");
            let painting = rdtsc();
            blank::clear(&surface);
            report.painted = report.found;
            report.made_wc = made_wc::NOT;
            report.paint_ticks = (rdtsc() - painting).max(1);
        }
    }
    log!(
        "screen cleared in {} ticks: the framebuffer is {}, painted {}",
        report.paint_ticks,
        memory_type::name(report.found),
        memory_type::name(report.painted)
    );
    report
}

/// Page allocator for the page tables the screen is cleared through
/// (scratch the kernel reuses).
struct ScratchFrames<'a>(&'a Firmware);

impl paging::FrameSource for ScratchFrames<'_> {
    fn alloc_zeroed_page(&mut self) -> Result<u64> {
        self.0.alloc_zeroed(PAGE, None)
    }
}

/// The end of what the direct map covers: all RAM, and at least the low
/// 4 GiB (the MMIO hole), to a GiB.
fn ram_limit(fw: &Firmware) -> Result<u64> {
    let (bytes, desc_size) = memory_map_size(fw);
    let capacity = bytes + 16 * desc_size;
    let map = fw.alloc_zeroed(capacity as u64, None)?;
    ram_limit_of(fw, map, capacity)
}

/// [`ram_limit`], reading the memory map into `map` (`capacity` bytes).
fn ram_limit_of(fw: &Firmware, map: u64, capacity: usize) -> Result<u64> {
    let mut limit = 4u64 << 30;
    let (mut size, mut key, mut ds, mut ver) = (capacity, 0usize, 0usize, 0u32);
    // SAFETY: a buffer of `capacity` bytes.
    let s = unsafe { (fw.bs.get_memory_map)(&mut size, map as *mut MemoryDescriptor, &mut key, &mut ds, &mut ver) };
    if is_error(s) || ds == 0 {
        return Err("GetMemoryMap failed");
    }
    for i in 0..size / ds {
        // SAFETY: within the returned map.
        let d = unsafe { &*((map as usize + i * ds) as *const MemoryDescriptor) };
        let end = d.physical_start + d.number_of_pages * PAGE;
        if d.ty != mt::MMIO && d.ty != mt::RESERVED {
            limit = limit.max(end);
        }
    }
    Ok(limit.next_multiple_of(1 << 30))
}

/// Page allocator for the page-table builder (boot data pages).
struct TableFrames<'a>(&'a Firmware);

impl paging::FrameSource for TableFrames<'_> {
    fn alloc_zeroed_page(&mut self) -> Result<u64> {
        self.0.alloc_zeroed(PAGE, Some(MemoryKind::BootData))
    }
}

/// Loads the kernel's sections into physically contiguous pages.
fn load_kernel(fw: &Firmware, file: &[u8]) -> Result<(vpe::PeImage<'static>, KernelImage)> {
    // SAFETY: `file` lives in pages that are never freed.
    let file: &'static [u8] = unsafe { core::slice::from_raw_parts(file.as_ptr(), file.len()) };
    let pe = vpe::PeImage::parse(file).map_err(|_| "kernel is not a valid PE32+ image")?;
    pe.check_no_imports().map_err(|_| "kernel has DLL imports")?;
    if pe.image_base() != bootinfo::KERNEL_BASE {
        return Err("kernel is not linked at KERNEL_BASE");
    }
    let size = (pe.size_of_image() as u64).next_multiple_of(PAGE);
    let phys = fw.alloc_zeroed(size, Some(MemoryKind::Kernel))?;
    // SAFETY: the destination was just allocated with `size` bytes and the
    // parser verified every section lies inside `size_of_image`.
    unsafe {
        let hdr = pe.header_bytes();
        ptr::copy_nonoverlapping(hdr.as_ptr(), phys as *mut u8, hdr.len());
        for s in pe.sections() {
            ptr::copy_nonoverlapping(s.data.as_ptr(), (phys + s.virtual_address as u64) as *mut u8, s.data.len());
        }
    }
    Ok((pe, KernelImage { phys_base: phys, virt_base: pe.image_base(), size }))
}

/// Maps the kernel image with per-section permissions (W^X).
fn map_kernel<F: paging::FrameSource>(
    pt: &mut paging::PageTables<F>,
    pe: &vpe::PeImage,
    k: &KernelImage,
) -> Result<()> {
    use paging::{GLOBAL, NO_EXECUTE, WRITABLE};
    let mut off = 0;
    while off < k.size {
        let rva = off as u32;
        let mut flags = GLOBAL | NO_EXECUTE;
        for s in pe.sections() {
            let end = s.virtual_address + s.virtual_size.next_multiple_of(PAGE as u32);
            if rva >= s.virtual_address && rva < end {
                if s.writable() {
                    flags |= WRITABLE;
                }
                if s.executable() {
                    flags &= !NO_EXECUTE;
                }
            }
        }
        pt.map_4k(k.virt_base + off, k.phys_base + off, flags)?;
        off += PAGE;
    }
    Ok(())
}

fn convert_type(ty: u32) -> MemoryKind {
    match ty {
        mt::CONVENTIONAL | mt::BOOT_SERVICES_CODE | mt::BOOT_SERVICES_DATA => MemoryKind::Usable,
        mt::LOADER_CODE | mt::LOADER_DATA => MemoryKind::LoaderReclaimable,
        mt::ACPI_RECLAIM => MemoryKind::AcpiReclaimable,
        mt::ACPI_NVS => MemoryKind::AcpiNvs,
        mt::MMIO | mt::MMIO_PORT_SPACE => MemoryKind::Mmio,
        mt::UNUSABLE => MemoryKind::Unusable,
        _ => MemoryKind::Reserved,
    }
}

/// Size information for the firmware memory map.
fn memory_map_size(fw: &Firmware) -> (usize, usize) {
    let (mut size, mut key, mut desc_size, mut ver) = (0usize, 0usize, 0usize, 0u32);
    // SAFETY: a size query with a null buffer is explicitly allowed.
    unsafe { (fw.bs.get_memory_map)(&mut size, ptr::null_mut(), &mut key, &mut desc_size, &mut ver) };
    (size, desc_size.max(core::mem::size_of::<MemoryDescriptor>()))
}

fn read_msr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reading an architectural MSR.
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}

fn write_msr(msr: u32, value: u64) {
    // SAFETY: writing an architectural MSR with a valid value.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack))
    };
}

fn supports_1g_pages() -> bool {
    let edx: u32;
    // SAFETY: CPUID is always available on x86-64. rbx is reserved by LLVM,
    // so it is saved manually.
    unsafe {
        core::arch::asm!("mov {tmp}, rbx", "cpuid", "mov rbx, {tmp}", tmp = out(reg) _,
            inout("eax") 0x8000_0001u32 => _, inout("ecx") 0 => _, out("edx") edx, options(nostack));
    }
    edx & (1 << 26) != 0
}

fn boot(fw: &Firmware) -> Result<core::convert::Infallible> {
    // SAFETY: disabling the 5 minute firmware watchdog.
    unsafe { (fw.bs.set_watchdog_timer)(0, 0, 0, ptr::null()) };

    let root = fw.boot_volume()?;
    let cfg = match fw.read_file(root, "\\VEDA\\BOOT.CFG", None)? {
        Some(r) => {
            // SAFETY: the file was read into r.base with r.size bytes.
            let bytes = unsafe { core::slice::from_raw_parts(r.base as *const u8, r.size as usize) };
            config::Config::parse(core::str::from_utf8(bytes).unwrap_or(""))
        }
        None => config::Config::default(),
    };

    let (framebuffer, screen) = setup_graphics(fw, cfg.resolution)?;
    log!(
        "framebuffer {}x{} stride {} at {:#x}",
        framebuffer.width,
        framebuffer.height,
        framebuffer.stride,
        framebuffer.phys_base
    );
    let paint = clear_screen(fw, &framebuffer);

    let kernel_file = fw.read_file(root, "\\VEDA\\VKERNEL.EXE", None)?.ok_or("\\VEDA\\VKERNEL.EXE not found")?;
    // SAFETY: the kernel file was read into kernel_file.base.
    let kernel_bytes = unsafe { core::slice::from_raw_parts(kernel_file.base as *const u8, kernel_file.size as usize) };
    let (pe, kernel) = load_kernel(fw, kernel_bytes)?;
    log!("kernel: {} KiB at phys {:#x}, entry {:#x}", kernel.size / 1024, kernel.phys_base, pe.entry_point());

    let initrd =
        fw.read_file(root, "\\VEDA\\INITRD.IMG", Some(MemoryKind::Initrd))?.ok_or("\\VEDA\\INITRD.IMG not found")?;
    log!("initrd: {} KiB at {:#x}", initrd.size / 1024, initrd.base);
    let symbols = fw.read_file(root, "\\VEDA\\VKERNEL.SYM", Some(MemoryKind::Symbols))?.unwrap_or(PhysRegion::EMPTY);

    let rsdp_phys = fw.config_table(&ACPI_20_TABLE).or_else(|| fw.config_table(&ACPI_10_TABLE)).unwrap_or(0);
    let boot_time = fw.boot_time();
    let (entropy, entropy_len) = fw.entropy();
    log!("entropy: {} bytes from the firmware RNG", entropy_len);
    // The system starts without them if need be.
    let firmware_variables = fw.variables().unwrap_or_else(|e| {
        log!("variables: none for the operating system: {}", e);
        PhysRegion::EMPTY
    });

    // Allocate everything the kernel will receive before taking the final
    // memory map.
    let boot_info_phys = fw.alloc_zeroed(PAGE, Some(MemoryKind::BootData))?;
    let stack = fw.alloc_zeroed(BOOT_STACK_SIZE, Some(MemoryKind::BootData))?;
    let (map_bytes, desc_size) = memory_map_size(fw);
    // Head-room for the descriptors created by the allocations below.
    let map_capacity = map_bytes + 16 * desc_size;
    let raw_map = fw.alloc_zeroed(map_capacity as u64, None)?;
    // Each recorded allocation can split a firmware region in up to three.
    let max_entries = map_capacity / desc_size + 2 * MAX_TAGS;
    let regions =
        fw.alloc_zeroed((max_entries * core::mem::size_of::<MemoryRegion>()) as u64, Some(MemoryKind::BootData))?;

    // The direct map covers all RAM and at least the low 4 GiB (MMIO hole).
    let phys_limit = ram_limit_of(fw, raw_map, map_capacity)?;

    let mut frames = TableFrames(fw);
    let mut pt = paging::PageTables::new(&mut frames, supports_1g_pages())?;
    // Identity map (slot 0) and direct map (slot 256) share the same tables;
    // the direct map is additionally non-executable.
    pt.map_linear(0, phys_limit, paging::WRITABLE)?;
    pt.alias_pml4_slot(256, 0, paging::NO_EXECUTE);
    // The identity map must not be global: the kernel will drop it.
    map_kernel(&mut pt, &pe, &kernel)?;
    let cr3 = pt.pml4;
    log!("page tables at {:#x}, direct map limit {:#x}", cr3, phys_limit);

    // ExitBootServices: no firmware calls (including printing) after this.
    let mut map_size;
    let mut desc_size_out = 0usize;
    let mut attempts = 0;
    loop {
        map_size = map_capacity;
        let (mut key, mut ver) = (0usize, 0u32);
        // SAFETY: valid buffer; see above.
        let s = unsafe {
            (fw.bs.get_memory_map)(
                &mut map_size,
                raw_map as *mut MemoryDescriptor,
                &mut key,
                &mut desc_size_out,
                &mut ver,
            )
        };
        if is_error(s) {
            return Err("GetMemoryMap failed");
        }
        // SAFETY: the map key is current.
        let s = unsafe { (fw.bs.exit_boot_services)(fw.image, key) };
        if s == SUCCESS {
            break;
        }
        attempts += 1;
        if attempts > 4 {
            return Err("ExitBootServices failed");
        }
    }

    // Translate, sort and merge the firmware map; loader data is split by
    // the recorded allocations.
    let mut tags = fw.tags.borrow_mut();
    let tag_count = tags.len;
    tags.list[..tag_count].sort_unstable_by_key(|t| t.0);
    let out = regions as *mut MemoryRegion;
    let mut count = 0usize;
    let mut push = |base: u64, end: u64, kind: MemoryKind| {
        if end > base && count < max_entries {
            let r = MemoryRegion { base, pages: (end - base) / PAGE, kind, _pad: 0 };
            // SAFETY: `count < max_entries`, the capacity of `regions`.
            unsafe { out.add(count).write(r) };
            count += 1;
        }
    };
    for i in 0..map_size / desc_size_out {
        // SAFETY: within the returned map.
        let d = unsafe { &*((raw_map as usize + i * desc_size_out) as *const MemoryDescriptor) };
        if d.number_of_pages == 0 {
            continue;
        }
        let (start, end) = (d.physical_start, d.physical_start + d.number_of_pages * PAGE);
        let kind = convert_type(d.ty);
        if d.ty != mt::LOADER_DATA {
            push(start, end, kind);
            continue;
        }
        let mut cur = start;
        for &(base, pages, tag) in &tags.list[..tag_count] {
            let (ts, te) = (base.max(cur), (base + pages * PAGE).min(end));
            if ts >= te {
                continue;
            }
            push(cur, ts, kind);
            push(ts, te, tag);
            cur = te;
        }
        push(cur, end, kind);
    }
    // SAFETY: `count` regions were initialised above.
    let slice = unsafe { core::slice::from_raw_parts_mut(out, count) };
    slice.sort_unstable_by_key(|r| r.base);
    let mut merged = 0usize;
    for i in 0..count {
        let r = slice[i];
        if merged > 0 && slice[merged - 1].kind == r.kind && slice[merged - 1].end() == r.base {
            slice[merged - 1].pages += r.pages;
        } else {
            slice[merged] = r;
            merged += 1;
        }
    }

    let info = boot_info_phys as *mut BootInfo;
    let mut cmdline = [0u8; bootinfo::CMDLINE_MAX];
    cmdline[..cfg.cmdline_len].copy_from_slice(&cfg.cmdline[..cfg.cmdline_len]);
    // SAFETY: the BootInfo page was allocated and zeroed above.
    unsafe {
        info.write(BootInfo {
            magic: BOOTINFO_MAGIC,
            version: BOOTINFO_VERSION,
            size: core::mem::size_of::<BootInfo>() as u32,
            hhdm_base: HHDM_BASE,
            hhdm_limit: phys_limit,
            memory_map: MemoryMap { entries: (HHDM_BASE + regions) as *const MemoryRegion, len: merged as u64 },
            kernel,
            framebuffer,
            initrd,
            symbols,
            rsdp_phys,
            boot_time,
            boot_stack: PhysRegion { base: stack, size: BOOT_STACK_SIZE },
            cmdline,
            cmdline_len: cfg.cmdline_len as u32,
            entropy_len: entropy_len as u32,
            entropy,
            paint,
            screen,
            firmware_variables,
        });
    }
    log!("handing over to the kernel ({} memory regions)", merged);

    // Enable no-execute support (required by the NX bits in our tables) and
    // supervisor write protection, then switch address space and jump.
    const IA32_EFER: u32 = 0xC000_0080;
    write_msr(IA32_EFER, read_msr(IA32_EFER) | (1 << 11));
    let entry = pe.entry_point();
    let stack_top = HHDM_BASE + stack + BOOT_STACK_SIZE;
    let info_virt = HHDM_BASE + boot_info_phys;
    // SAFETY: enabling supervisor write protection (CR0.WP) with interrupts
    // masked; the firmware's page tables are still active and writable.
    unsafe {
        core::arch::asm!("cli", "mov {tmp}, cr0", "or {tmp}, 0x10000", "mov cr0, {tmp}", tmp = out(reg) _, options(nostack));
    }
    // SAFETY: the new tables identity-map this code, map the stack and the
    // kernel, and the kernel entry follows the documented contract.
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rsp, {stack}",
            "xor ebp, ebp",
            "push 0",               // fake return address keeps the ABI alignment
            "jmp {entry}",
            cr3 = in(reg) cr3,
            stack = in(reg) stack_top,
            entry = in(reg) entry,
            in("rdi") info_virt,
            options(noreturn)
        );
    }
}

#[unsafe(export_name = "efi_main")]
extern "efiapi" fn efi_main(image: Handle, st: *mut SystemTable) -> Status {
    serial::init();
    log!("Veda boot loader {}", env!("CARGO_PKG_VERSION"));
    // SAFETY: the firmware passes a valid system table.
    let st: &'static SystemTable = unsafe { &*st };
    // SAFETY: boot services are valid until ExitBootServices.
    let fw = Firmware {
        image,
        st,
        bs: unsafe { &*st.boot_services },
        tags: core::cell::RefCell::new(Tags { list: [(0, 0, MemoryKind::Reserved); MAX_TAGS], len: 0 }),
    };
    let Err(err) = boot(&fw);
    log!("fatal: {err}");
    fw.print("\nVeda could not start: ");
    fw.print(err);
    fw.print("\n");
    loop {
        // SAFETY: stall is always safe to call during boot services.
        unsafe { (fw.bs.stall)(1_000_000) };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("panic: {info}");
    loop {
        core::hint::spin_loop();
    }
}
