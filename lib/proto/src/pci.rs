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
