//! virtio 1.x over PCI ("modern" interface) for user-space drivers.
//!
//! * [`Device`] locates the virtio capabilities through the `pcidev`
//!   protocol, maps the BARs, performs the status/feature handshake and sets
//!   up MSI-X vectors.
//! * [`Virtqueue`] is a split virtqueue living in physically contiguous DMA
//!   memory.
//! * [`DmaBuffer`] is a mapped, physically contiguous buffer for payloads.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use vabi::map_flags;
use vproto::pci::{cap, pcidev};
use vrt::object::{Interrupt, Resource, Vmo};
use vrt::vm::Mapping;

/// Device status bits.
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const FAILED: u8 = 128;
}

/// Feature bit every virtio 1.x driver negotiates.
pub const F_VERSION_1: u64 = 1 << 32;
/// The device's memory accesses go through the platform's IOMMU: taken
/// whenever offered (a device that offers it may refuse a driver that does
/// not), since the addresses Veda gives devices are what its IOMMU
/// translates.
pub const F_ACCESS_PLATFORM: u64 = 1 << 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioError {
    /// Talking to devmgr failed.
    Ipc,
    /// A required virtio capability is missing.
    MissingCapability,
    /// The device rejected our feature set.
    FeaturesRejected,
    /// The requested queue does not exist or is too small.
    BadQueue,
    /// Out of memory or mapping failed.
    NoMemory,
    /// No MSI-X support.
    NoMsix,
}

/// A mapped, physically contiguous DMA buffer.
pub struct DmaBuffer {
    _vmo: Vmo,
    map: Mapping,
    phys: u64,
}

impl DmaBuffer {
    pub fn new(dma: &Resource, len: usize) -> Result<DmaBuffer, VirtioError> {
        let len = len.next_multiple_of(4096);
        DmaBuffer::mapped(Vmo::create_contiguous(dma, len).map_err(|_| VirtioError::NoMemory)?, len)
    }

    /// A buffer below 4 GiB, for devices limited to 32-bit addresses.
    pub fn new_below_4g(dma: &Resource, len: usize) -> Result<DmaBuffer, VirtioError> {
        let len = len.next_multiple_of(4096);
        DmaBuffer::mapped(Vmo::create_contiguous_below_4g(dma, len).map_err(|_| VirtioError::NoMemory)?, len)
    }

    fn mapped(vmo: Vmo, len: usize) -> Result<DmaBuffer, VirtioError> {
        let phys = vmo.phys_addr(0).map_err(|_| VirtioError::NoMemory)?;
        let map_vmo = Vmo::from_handle(vmo.0.duplicate(None).map_err(|_| VirtioError::NoMemory)?);
        let map = Mapping::new(map_vmo, len, map_flags::READ | map_flags::WRITE).map_err(|_| VirtioError::NoMemory)?;
        Ok(DmaBuffer { _vmo: vmo, map, phys })
    }

    pub fn phys(&self) -> u64 {
        self.phys
    }

    pub fn ptr(&self) -> *mut u8 {
        self.map.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// # Safety
    /// The device must not be writing the buffer concurrently.
    pub unsafe fn bytes(&self, offset: usize, len: usize) -> &[u8] {
        // SAFETY: within the mapping; synchronisation is the caller's job.
        unsafe { core::slice::from_raw_parts(self.ptr().add(offset), len) }
    }

    pub fn write(&self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= self.len());
        // SAFETY: bounds checked above.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr().add(offset), data.len()) };
    }
}

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// One element of a descriptor chain: (physical address, length, device
/// writes it).
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub phys: u64,
    pub len: u32,
    pub device_writes: bool,
}

/// A split virtqueue.
pub struct Virtqueue {
    pub index: u16,
    size: u16,
    mem: DmaBuffer,
    avail_off: usize,
    used_off: usize,
    free: Vec<u16>,
    /// Length of the chain starting at each head (0: not an outstanding
    /// head).
    chain_len: Vec<u16>,
    /// Our own copy of each descriptor's `next` link, so freeing a chain
    /// never trusts memory the device can write.
    next: Vec<u16>,
    last_used: u16,
    notify: *mut u16,
}

// SAFETY: the raw pointer refers to device memory owned by this queue.
unsafe impl Send for Virtqueue {}

impl Virtqueue {
    fn new(index: u16, size: u16, dma: &Resource, notify: *mut u16) -> Result<Virtqueue, VirtioError> {
        let desc_bytes = 16 * size as usize;
        let avail_off = desc_bytes;
        let avail_bytes = 6 + 2 * size as usize;
        let used_off = (avail_off + avail_bytes).next_multiple_of(4096);
        let used_bytes = 6 + 8 * size as usize;
        let mem = DmaBuffer::new(dma, used_off + used_bytes)?;
        Ok(Virtqueue {
            index,
            size,
            mem,
            avail_off,
            used_off,
            free: (0..size).rev().collect(),
            chain_len: alloc::vec![0; size as usize],
            next: alloc::vec![0; size as usize],
            last_used: 0,
            notify,
        })
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    fn desc(&self, i: u16) -> *mut Desc {
        // SAFETY: i < size, inside the descriptor table.
        unsafe { (self.mem.ptr() as *mut Desc).add(i as usize) }
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        // SAFETY: inside the avail ring.
        unsafe { self.mem.ptr().add(self.avail_off + 2) as *mut u16 }
    }

    fn avail_ring(&self, i: u16) -> *mut u16 {
        // SAFETY: i < size.
        unsafe { self.mem.ptr().add(self.avail_off + 4 + 2 * i as usize) as *mut u16 }
    }

    fn used_idx(&self) -> u16 {
        // SAFETY: inside the used ring; written by the device.
        unsafe { read_volatile(self.mem.ptr().add(self.used_off + 2) as *const u16) }
    }

    fn used_elem(&self, i: u16) -> (u32, u32) {
        // SAFETY: i < size.
        unsafe {
            let p = self.mem.ptr().add(self.used_off + 4 + 8 * i as usize) as *const u32;
            (read_volatile(p), read_volatile(p.add(1)))
        }
    }

    /// Number of free descriptors.
    pub fn free_descriptors(&self) -> usize {
        self.free.len()
    }

    /// Posts a descriptor chain. Returns its head index (the token reported
    /// back by [`Virtqueue::pop_used`]).
    pub fn push(&mut self, chain: &[Segment]) -> Option<u16> {
        if chain.is_empty() || chain.len() > self.free.len() {
            return None;
        }
        let ids: Vec<u16> = (0..chain.len()).map(|_| self.free.pop().unwrap()).collect();
        for (i, seg) in chain.iter().enumerate() {
            let mut flags = if seg.device_writes { DESC_F_WRITE } else { 0 };
            let next = if i + 1 < chain.len() {
                flags |= DESC_F_NEXT;
                ids[i + 1]
            } else {
                0
            };
            self.next[ids[i] as usize] = next;
            // SAFETY: descriptor owned by us until the device uses it.
            unsafe { write_volatile(self.desc(ids[i]), Desc { addr: seg.phys, len: seg.len, flags, next }) };
        }
        let head = ids[0];
        self.chain_len[head as usize] = chain.len() as u16;
        // SAFETY: the avail ring belongs to the driver.
        unsafe {
            let idx = read_volatile(self.avail_idx_ptr());
            write_volatile(self.avail_ring(idx % self.size), head);
            fence(Ordering::SeqCst);
            write_volatile(self.avail_idx_ptr(), idx.wrapping_add(1));
        }
        Some(head)
    }

    /// Tells the device new buffers are available.
    pub fn notify(&self) {
        fence(Ordering::SeqCst);
        // SAFETY: the notify register of this queue (device MMIO).
        unsafe { write_volatile(self.notify, self.index) };
    }

    /// Takes the next completed chain: (head, bytes written by the device).
    /// The chain's descriptors are returned to the free list. Entries naming
    /// a descriptor that is not the head of an outstanding chain (a device
    /// bug) are skipped.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        loop {
            fence(Ordering::SeqCst);
            if self.last_used == self.used_idx() {
                return None;
            }
            let (id, len) = self.used_elem(self.last_used % self.size);
            self.last_used = self.last_used.wrapping_add(1);
            if id >= self.size as u32 || self.chain_len[id as usize] == 0 {
                continue;
            }
            return Some(self.release(id as u16, len));
        }
    }

    fn release(&mut self, head: u16, len: u32) -> (u16, u32) {
        let count = core::mem::take(&mut self.chain_len[head as usize]);
        let mut cur = head;
        for i in 0..count {
            self.free.push(cur);
            if i + 1 < count {
                cur = self.next[cur as usize];
            }
        }
        (head, len)
    }
}

/// Location of a virtio structure inside a BAR.
#[derive(Debug, Clone, Copy)]
struct CapLocation {
    bar: u8,
    offset: u32,
    length: u32,
}

/// A virtio PCI device.
pub struct Device {
    pub pci: pcidev::Client,
    bars: Vec<(u8, Mapping)>,
    common: *mut u8,
    notify_base: *mut u8,
    notify_mult: u32,
    isr: *mut u8,
    device_cfg: *mut u8,
    device_cfg_len: u32,
    dma: Resource,
    msix_table: Option<*mut u32>,
    msix_cap: u16,
    msix_count: u16,
    vectors_used: u16,
}

// SAFETY: the raw pointers refer to MMIO mappings owned by the device.
unsafe impl Send for Device {}

fn pci_read(pci: &pcidev::Client, off: u16, width: u8) -> Result<u32, VirtioError> {
    match pci.config_read(off, width) {
        Ok(Ok(v)) => Ok(v),
        _ => Err(VirtioError::Ipc),
    }
}

impl Device {
    /// Locates the capabilities, maps the BARs and enables the device.
    pub fn new(pci: pcidev::Client) -> Result<Device, VirtioError> {
        match pci.enable(true) {
            Ok(Ok(())) => {}
            _ => return Err(VirtioError::Ipc),
        }
        let dma = match pci.dma_resource() {
            Ok(Ok(r)) => r,
            _ => return Err(VirtioError::Ipc),
        };
        let mut common = None;
        let mut notify = None;
        let mut notify_mult = 0;
        let mut isr = None;
        let mut device = None;
        let mut msix_cap = 0u16;
        let mut capp = (pci_read(&pci, 0x34, 1)? & 0xFC) as u16;
        let mut guard = 0;
        while capp != 0 && guard < 48 {
            guard += 1;
            let id = pci_read(&pci, capp, 1)? as u8;
            let next = (pci_read(&pci, capp + 1, 1)? & 0xFC) as u16;
            if id == cap::VENDOR {
                let cfg_type = pci_read(&pci, capp + 3, 1)? as u8;
                let loc = CapLocation {
                    bar: pci_read(&pci, capp + 4, 1)? as u8,
                    offset: pci_read(&pci, capp + 8, 4)?,
                    length: pci_read(&pci, capp + 12, 4)?,
                };
                match cfg_type {
                    1 if common.is_none() => common = Some(loc),
                    2 if notify.is_none() => {
                        notify = Some(loc);
                        notify_mult = pci_read(&pci, capp + 16, 4)?;
                    }
                    3 if isr.is_none() => isr = Some(loc),
                    4 if device.is_none() => device = Some(loc),
                    _ => {}
                }
            } else if id == cap::MSI_X {
                msix_cap = capp;
            }
            capp = next;
        }
        let (Some(common), Some(notify), Some(isr)) = (common, notify, isr) else {
            return Err(VirtioError::MissingCapability);
        };

        let mut dev = Device {
            pci,
            bars: Vec::new(),
            common: core::ptr::null_mut(),
            notify_base: core::ptr::null_mut(),
            notify_mult,
            isr: core::ptr::null_mut(),
            device_cfg: core::ptr::null_mut(),
            device_cfg_len: device.map(|d| d.length).unwrap_or(0),
            dma,
            msix_table: None,
            msix_cap,
            msix_count: 0,
            vectors_used: 0,
        };
        dev.common = dev.locate(common)?;
        dev.notify_base = dev.locate(notify)?;
        dev.isr = dev.locate(isr)?;
        if let Some(d) = device {
            dev.device_cfg = dev.locate(d)?;
        }
        if msix_cap != 0 {
            let control = pci_read(&dev.pci, msix_cap + 2, 2)?;
            let table = pci_read(&dev.pci, msix_cap + 4, 4)?;
            let base = dev.bar_ptr((table & 7) as u8)?;
            // SAFETY: the table lies inside the mapped BAR.
            dev.msix_table = Some(unsafe { base.add((table & !7) as usize) } as *mut u32);
            dev.msix_count = (control as u16 & 0x7FF) + 1;
        }
        Ok(dev)
    }

    fn bar_ptr(&mut self, bar: u8) -> Result<*mut u8, VirtioError> {
        if let Some((_, m)) = self.bars.iter().find(|(b, _)| *b == bar) {
            return Ok(m.as_ptr());
        }
        let vmo = match self.pci.map_bar(bar) {
            Ok(Ok(v)) => v,
            _ => return Err(VirtioError::MissingCapability),
        };
        let size = vmo.size().map_err(|_| VirtioError::NoMemory)?;
        let m = Mapping::new(vmo, size, map_flags::READ | map_flags::WRITE).map_err(|_| VirtioError::NoMemory)?;
        let p = m.as_ptr();
        self.bars.push((bar, m));
        Ok(p)
    }

    fn locate(&mut self, loc: CapLocation) -> Result<*mut u8, VirtioError> {
        let base = self.bar_ptr(loc.bar)?;
        // SAFETY: the capability lies inside the mapped BAR.
        Ok(unsafe { base.add(loc.offset as usize) })
    }

    fn c8(&self, off: usize) -> *mut u8 {
        // SAFETY: inside the common configuration structure.
        unsafe { self.common.add(off) }
    }

    fn w8(&self, off: usize, v: u8) {
        // SAFETY: MMIO register.
        unsafe { write_volatile(self.c8(off), v) }
    }
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: MMIO register.
        unsafe { read_volatile(self.c8(off)) }
    }
    fn w16(&self, off: usize, v: u16) {
        // SAFETY: MMIO register.
        unsafe { write_volatile(self.c8(off) as *mut u16, v) }
    }
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: MMIO register.
        unsafe { read_volatile(self.c8(off) as *const u16) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: MMIO register.
        unsafe { write_volatile(self.c8(off) as *mut u32, v) }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: MMIO register.
        unsafe { read_volatile(self.c8(off) as *const u32) }
    }
    fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }

    pub fn status(&self) -> u8 {
        self.r8(20)
    }

    pub fn set_status(&self, s: u8) {
        self.w8(20, s);
    }

    /// Resets the device and performs the feature handshake, accepting the
    /// intersection of the device's features and `wanted` (plus VERSION_1
    /// and ACCESS_PLATFORM).
    pub fn initialize(&self, wanted: u64) -> Result<u64, VirtioError> {
        // A device whose status reads 0 is in its reset state already (the
        // firmware resets its devices when it hands over). Resetting it
        // again is not only redundant: QEMU's 3D virtio-gpu resets on its
        // main loop and holds the vCPU that asked until it has, which now
        // and then deadlocks the whole machine.
        if self.status() != 0 {
            self.set_status(0);
            while self.status() != 0 {
                core::hint::spin_loop();
            }
        }
        self.set_status(status::ACKNOWLEDGE);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER);
        self.w32(0, 0);
        let lo = self.r32(4) as u64;
        self.w32(0, 1);
        let hi = self.r32(4) as u64;
        let offered = lo | hi << 32;
        let accepted = offered & (wanted | F_VERSION_1 | F_ACCESS_PLATFORM);
        if accepted & F_VERSION_1 == 0 {
            self.set_status(status::FAILED);
            return Err(VirtioError::FeaturesRejected);
        }
        self.w32(8, 0);
        self.w32(12, accepted as u32);
        self.w32(8, 1);
        self.w32(12, (accepted >> 32) as u32);
        self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK);
        if self.status() & status::FEATURES_OK == 0 {
            self.set_status(status::FAILED);
            return Err(VirtioError::FeaturesRejected);
        }
        Ok(accepted)
    }

    /// Creates queue `index` with up to `max_size` entries.
    pub fn setup_queue(&self, index: u16, max_size: u16) -> Result<Virtqueue, VirtioError> {
        self.w16(22, index);
        let dev_size = self.r16(24);
        if dev_size == 0 {
            return Err(VirtioError::BadQueue);
        }
        let size = dev_size.min(max_size).max(1);
        self.w16(24, size);
        let notify_off = self.r16(30) as usize;
        // SAFETY: inside the notification structure.
        let notify = unsafe { self.notify_base.add(notify_off * self.notify_mult as usize) } as *mut u16;
        let q = Virtqueue::new(index, size, &self.dma, notify)?;
        self.w64(32, q.mem.phys());
        self.w64(40, q.mem.phys() + q.avail_off as u64);
        self.w64(48, q.mem.phys() + q.used_off as u64);
        self.w16(28, 1);
        Ok(q)
    }

    /// Allocates an MSI-X vector, binds it to `queue` (or to configuration
    /// changes when `None`), and returns the interrupt to wait on.
    pub fn msix_vector(&mut self, queue: Option<u16>) -> Result<Interrupt, VirtioError> {
        let table = self.msix_table.ok_or(VirtioError::NoMsix)?;
        if self.vectors_used >= self.msix_count {
            return Err(VirtioError::NoMsix);
        }
        let (irq, msi) = match self.pci.alloc_msi() {
            Ok(Ok(v)) => v,
            _ => return Err(VirtioError::NoMsix),
        };
        let v = self.vectors_used;
        self.vectors_used += 1;
        // SAFETY: entry `v` of the MSI-X table in the mapped BAR.
        unsafe {
            let e = table.add(v as usize * 4);
            write_volatile(e, msi.address as u32);
            write_volatile(e.add(1), (msi.address >> 32) as u32);
            write_volatile(e.add(2), msi.data);
            write_volatile(e.add(3), 0); // unmask
        }
        // Enable MSI-X (clear the function mask).
        let control = pci_read(&self.pci, self.msix_cap + 2, 2)?;
        let _ = self.pci.config_write(self.msix_cap + 2, 2, (control | 0x8000) & !0x4000);
        match queue {
            Some(q) => {
                self.w16(22, q);
                self.w16(26, v);
                if self.r16(26) != v {
                    return Err(VirtioError::NoMsix);
                }
            }
            None => self.w16(16, v),
        }
        Ok(irq)
    }

    /// Final step of initialisation.
    pub fn driver_ok(&self) {
        self.set_status(status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK);
    }

    /// Reads and clears the ISR status (legacy interrupt acknowledgement).
    pub fn isr(&self) -> u8 {
        // SAFETY: ISR status register.
        unsafe { read_volatile(self.isr) }
    }

    pub fn dma(&self) -> &Resource {
        &self.dma
    }

    pub fn cfg_read8(&self, off: u32) -> u8 {
        assert!(off < self.device_cfg_len);
        // SAFETY: inside the device-specific configuration.
        unsafe { read_volatile(self.device_cfg.add(off as usize)) }
    }

    pub fn cfg_write8(&self, off: u32, v: u8) {
        assert!(off < self.device_cfg_len);
        // SAFETY: inside the device-specific configuration.
        unsafe { write_volatile(self.device_cfg.add(off as usize), v) }
    }

    pub fn cfg_read16(&self, off: u32) -> u16 {
        assert!(off + 2 <= self.device_cfg_len);
        // SAFETY: inside the device-specific configuration.
        unsafe { read_volatile(self.device_cfg.add(off as usize) as *const u16) }
    }

    pub fn cfg_read32(&self, off: u32) -> u32 {
        assert!(off + 4 <= self.device_cfg_len);
        // SAFETY: inside the device-specific configuration.
        unsafe { read_volatile(self.device_cfg.add(off as usize) as *const u32) }
    }

    pub fn cfg_read64(&self, off: u32) -> u64 {
        self.cfg_read32(off) as u64 | (self.cfg_read32(off + 4) as u64) << 32
    }
}
