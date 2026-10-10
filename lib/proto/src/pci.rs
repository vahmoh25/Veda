//! The per-device PCI protocol served by `devmgr`.
//!
//! A driver is started with a channel speaking this protocol for exactly one
//! device: it can read and write that device's configuration space, map its
//! BARs, enable it, and allocate MSI interrupts and DMA memory. It also
//! learns what the firmware describes below the device (ACPI: the
//! amplifiers on an SPI controller, say), and drives the GPIO pins those
//! devices are wired to — those pins only. Nothing else.

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{enumeration, message, protocol, union};
use vrt::object::{Interrupt, IoPorts, Resource, Vmo};

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

message! {
    /// A device the firmware (ACPI) describes below a PCI function: what
    /// it is, and the resources it uses.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AcpiDevice {
        /// Its path in the ACPI namespace (`\_SB.PC00.SPI1.SPK1`).
        pub path: String,
        /// Hardware id (`CSC3551`); empty if none.
        pub hid: String,
        /// Unique id; empty if none.
        pub uid: String,
        /// Subsystem id: the board's, for devices that need per-board
        /// settings (`10431F62`); empty if none.
        pub sub: String,
        /// `_STA` (bit 0: present).
        pub status: u32,
        pub resources: Vec<AcpiResource>,
        /// Why its resources could not be read; empty if they could.
        pub resource_error: String,
    }
}

union! {
    /// One resource an ACPI device uses.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum AcpiResource {
        1 => Memory { base: u64, length: u64 },
        2 => Io { base: u16, length: u16 },
        3 => Irq { irqs: Vec<u32>, edge: bool, active_low: bool, shared: bool },
        /// A GPIO connection: an input or output (`GpioIo`), or an
        /// interrupt (`GpioInt`). `pull`: 0 default, 1 up, 2 down, 3 none;
        /// `restriction`: 0 either way, 1 input only, 2 output only.
        4 => Gpio { interrupt: bool, pins: Vec<u16>, controller: String, pull: u8, restriction: u8, shared: bool },
        /// The device's connection to an SPI controller.
        5 => Spi {
            controller: String,
            chip_select: u16,
            speed_hz: u32,
            bits: u8,
            cpol: bool,
            cpha: bool,
            cs_active_high: bool,
        },
        6 => I2c { controller: String, address: u16, speed_hz: u32 },
        /// Something else (its descriptor type).
        7 => Other { kind: u8 },
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PciError {
        BadOffset = 1,
        NoSuchBar = 2,
        Denied = 3,
        NoResources = 4,
        /// No such ACPI device or GPIO connection.
        NotFound = 5,
        /// A GPIO controller devmgr does not drive.
        Unsupported = 6,
        /// The pin belongs to the firmware, or its settings are locked.
        Busy = 7,
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
        /// Enables memory and I/O decoding and, optionally, bus mastering
        /// (DMA).
        5 => fn enable(bus_master: bool) -> Result<(), PciError>;
        /// Allocates an MSI/MSI-X interrupt; program the returned address and
        /// data into the device.
        6 => fn alloc_msi() -> Result<(Interrupt, MsiAddress), PciError>;
        /// A resource that allows allocating DMA (physically contiguous) memory.
        7 => fn dma_resource() -> Result<Resource, PciError>;
        /// The ports of an I/O BAR (older devices such as AC'97 sound).
        8 => fn map_io_bar(index: u8) -> Result<IoPorts, PciError>;
        /// The devices the firmware describes below this one; none if it
        /// describes none (or there are no ACPI tables).
        9 => fn acpi_devices() -> Vec<AcpiDevice>;
        /// The level of GPIO connection `index` (counting the device's
        /// `Gpio` resources in order) of ACPI device `device` (its index in
        /// `acpi_devices`).
        10 => fn gpio_read(device: u32, index: u32) -> Result<bool, PciError>;
        /// Drives GPIO connection `index` of ACPI device `device` high or
        /// low, making the pin an output if it is not one.
        11 => fn gpio_write(device: u32, index: u32, high: bool) -> Result<(), PciError>;
        /// The resource that names this function (a PCI resource of its
        /// requester id), for giving it to a virtual machine: only the
        /// driver VM's devices have one (`Denied`).
        12 => fn device_resource() -> Result<Resource, PciError>;
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
