//! The per-device PCI protocol served by `devmgr`.
//!
//! A driver is started with a channel speaking this protocol for exactly one
//! device: it can read and write that device's configuration space, map its
//! BARs, enable it, and allocate MSI interrupts and DMA memory — and nothing
//! else.

use alloc::vec::Vec;
use vipc::{enumeration, message, protocol};
use vrt::object::{Interrupt, Resource, Vmo};

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Bar {
        pub index: u8,
        /// 0 = memory, 1 = I/O.
        pub io: bool,
        pub address: u64,
        pub size: u64,
        pub prefetchable: bool,
    }
}

message! {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DeviceInfo {
        pub vendor: u16,
        pub device: u16,
        pub class: u8,
        pub subclass: u8,
        pub prog_if: u8,
        pub revision: u8,
        pub bus: u8,
        pub slot: u8,
        pub function: u8,
        pub irq_line: u8,
        pub irq_pin: u8,
        pub bars: Vec<Bar>,
    }
}

message! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MsiAddress {
        pub address: u64,
        pub data: u32,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PciError {
        BadOffset = 1,
        NoSuchBar = 2,
        Denied = 3,
        NoResources = 4,
    }
}

protocol! {
    /// Access to one PCI function.
    pub mod pcidev = "pcidev" {
        1 => fn info() -> DeviceInfo;
        2 => fn config_read(offset: u16, width: u8) -> Result<u32, PciError>;
        3 => fn config_write(offset: u16, width: u8, value: u32) -> Result<(), PciError>;
        /// Maps a memory BAR (uncached physical VMO).
        4 => fn map_bar(index: u8) -> Result<Vmo, PciError>;
        /// Enables memory decoding and, optionally, bus mastering (DMA).
        5 => fn enable(bus_master: bool) -> Result<(), PciError>;
        /// Allocates an MSI/MSI-X interrupt; program the returned address and
        /// data into the device.
        6 => fn alloc_msi() -> Result<(Interrupt, MsiAddress), PciError>;
        /// A resource that allows allocating DMA (physically contiguous) memory.
        7 => fn dma_resource() -> Result<Resource, PciError>;
    }
}

/// Well-known capability ids.
pub mod cap {
    pub const MSI: u8 = 0x05;
    pub const VENDOR: u8 = 0x09;
    pub const MSI_X: u8 = 0x11;
}

fn config_read(pci: &pcidev::Client, off: u16, width: u8) -> Option<u32> {
    pci.config_read(off, width).ok()?.ok()
}

/// Finds a capability in the device's capability list.
pub fn find_capability(pci: &pcidev::Client, id: u8) -> Option<u16> {
    let mut at = (config_read(pci, 0x34, 1)? & 0xFC) as u16;
    // The list is at most 48 entries long in 256 bytes of space.
    for _ in 0..48 {
        if at == 0 {
            return None;
        }
        if config_read(pci, at, 1)? as u8 == id {
            return Some(at);
        }
        at = (config_read(pci, at + 1, 1)? & 0xFC) as u16;
    }
    None
}

/// Sets up a single MSI vector for every interrupt cause of a device with
/// the MSI capability, and returns the interrupt to wait on (`None`: no
/// MSI; poll the device instead).
pub fn enable_msi(pci: &pcidev::Client) -> Option<Interrupt> {
    let at = find_capability(pci, cap::MSI)?;
    let control = config_read(pci, at + 2, 2)? as u16;
    let (irq, msi) = pci.alloc_msi().ok()?.ok()?;
    let ok = |r: Result<Result<(), PciError>, vipc::IpcError>| matches!(r, Ok(Ok(())));
    let mut good = ok(pci.config_write(at + 4, 4, msi.address as u32));
    // 64-bit capable devices have an upper address register before the data.
    let data_at = if control & (1 << 7) != 0 {
        good &= ok(pci.config_write(at + 8, 4, (msi.address >> 32) as u32));
        at + 12
    } else {
        at + 8
    };
    good &= ok(pci.config_write(data_at, 2, msi.data & 0xFFFF));
    // Enable, with a single message.
    good &= ok(pci.config_write(at + 2, 2, ((control & !(0x7 << 4)) | 1) as u32));
    good.then_some(irq)
}
