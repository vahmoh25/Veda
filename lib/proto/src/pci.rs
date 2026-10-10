//! The per-device PCI protocol served by `devmgr`.
//!
//! A driver is started with a channel speaking this protocol for exactly one
//! device: it can read and write that device's configuration space, map its
//! BARs, enable it, and allocate MSI interrupts and DMA memory (or have the
//! line its INTx is wired to). It also
//! learns what the firmware describes below the device (ACPI: the
//! amplifiers on an SPI controller, say), and drives the GPIO pins those
//! devices are wired to, and has their interrupts — those pins only.
//! Nothing else.

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
    /// An I/O APIC input (a GSI), and how it signals.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct InterruptLine {
        pub gsi: u32,
        pub level: bool,
        pub active_low: bool,
    }
}

message! {
    /// Memory the firmware keeps for a function (an RMRR of the IOMMU's
    /// table: a GPU's stolen memory, which it and the firmware's
    /// framebuffer use): where it is, and a VMO of it.
    #[derive(Debug)]
    pub struct ReservedMemory {
        pub base: u64,
        pub size: u64,
        pub memory: Vmo,
    }
}

message! {
    /// What the firmware gives an Intel integrated GPU's driver: its
    /// OpRegion (the memory its configuration space's ASLS names: the
    /// display's description and the firmware's mailboxes), and its raw VBT
    /// if the OpRegion keeps that outside itself (empty if not).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OpRegion {
        pub opregion: Vec<u8>,
        pub vbt: Vec<u8>,
    }
}

message! {
    /// The registers of the PC's host bridge (00:00.0) that an Intel
    /// integrated GPU's driver reads: its ids, class and subsystem (the
    /// dwords at 0x00, 0x08 and 0x2C), its MCHBAR (0x48) and its graphics
    /// control (the dword at 0x50).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HostBridge {
        pub ids: u32,
        pub class: u32,
        pub subsystem: u32,
        pub mchbar: u64,
        pub graphics_control: u32,
    }
}

message! {
    /// Where a function's INTx goes, as the firmware routes it (`_PRT`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct IntxLine {
        /// The function's pin: 1 for INTA# to 4.
        pub pin: u8,
        /// The I/O APIC input (GSI) it is wired to, and how.
        pub gsi: u32,
        pub level: bool,
        pub active_low: bool,
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
        /// Compatible ids (`PNP0C50`).
        pub cids: Vec<String>,
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
        /// `restriction`: 0 either way, 1 input only, 2 output only. An
        /// interrupt is an edge or level-triggered (`edge`), `polarity` 0
        /// active high (or rising), 1 active low (or falling), 2 both
        /// edges, and may wake the machine (`wake`); `debounce` is in
        /// hundredths of a millisecond. The controllers of connections are
        /// absolute paths where the name resolves (as written where it
        /// does not).
        4 => Gpio {
            interrupt: bool,
            pins: Vec<u16>,
            controller: String,
            pull: u8,
            restriction: u8,
            shared: bool,
            edge: bool,
            polarity: u8,
            wake: bool,
            debounce: u16,
        },
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
        6 => I2c { controller: String, address: u16, speed_hz: u32, ten_bit: bool },
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
        /// The ports of an I/O BAR (older devices').
        8 => fn map_io_bar(index: u8) -> Result<IoPorts, PciError>;
        /// The devices the firmware describes below this one; none if it
        /// describes none (or there are no ACPI tables).
        9 => fn acpi_devices() -> Vec<AcpiDevice>;
        /// The level of GPIO pin `index` of ACPI device `device` (its index
        /// in `acpi_devices`), counting the pins of its `Gpio` resources in
        /// order: what it drives, if it is an output; what it reads, made
        /// an input if it is neither.
        10 => fn gpio_read(device: u32, index: u32) -> Result<bool, PciError>;
        /// Drives GPIO pin `index` of ACPI device `device` high or low,
        /// making it an output if it is not one.
        11 => fn gpio_write(device: u32, index: u32, high: bool) -> Result<(), PciError>;
        /// The resource that names this function (a PCI resource of its
        /// requester id), for giving it to a virtual machine: only the
        /// driver VM's devices have one (`Denied`).
        12 => fn device_resource() -> Result<Resource, PciError>;
        /// The interrupt line the function's INTx is wired to (`NotFound`:
        /// it has none, or the firmware does not say where it goes). Every
        /// function on a line shares the one interrupt; a driver that
        /// takes it leaves the function's MSIs off.
        13 => fn intx() -> Result<(Interrupt, IntxLine), PciError>;
        /// What `_DSM` of ACPI device `device` (its index in
        /// `acpi_devices`) answers function `function` of `uuid` (its 16
        /// bytes, as `ToUUID` makes them) at `revision`, without arguments:
        /// an integer (`NotFound` if it has no `_DSM` or answers anything
        /// else).
        14 => fn acpi_dsm(device: u32, uuid: Vec<u8>, revision: u64, function: u64) -> Result<u64, PciError>;
        /// Constant object `name` (`_DSD`, an I2C controller's `FMCN`) of
        /// ACPI device `device`, or of the function's own device with
        /// `device` `u32::MAX`, as AML (`NotFound` if it has none;
        /// `Unsupported` if it is no constant: integers, strings, buffers
        /// and packages of them).
        15 => fn acpi_data(device: u32, name: String) -> Result<Vec<u8>, PciError>;
        /// The interrupt line of interrupt `index` of ACPI device `device`
        /// (counting the interrupts of its `Irq` resources in order), as
        /// `intx` gives a function's: shared by whoever else is on it.
        16 => fn acpi_interrupt(device: u32, index: u32) -> Result<(Interrupt, InterruptLine), PciError>;
        /// Makes GPIO pin `index` of ACPI device `device` an input.
        17 => fn gpio_input(device: u32, index: u32) -> Result<(), PciError>;
        /// Whether GPIO pin `index` of ACPI device `device` drives its level
        /// (an output) rather than reading it.
        18 => fn gpio_is_output(device: u32, index: u32) -> Result<bool, PciError>;
        /// The interrupt of GPIO pin `index` of ACPI device `device`, whose
        /// connection is an interrupt (`GpioInt`; `NotFound` if it is
        /// not), the pin set up to interrupt as the connection says: raised
        /// when the pin does — an edge, or level-triggered, raised (and the
        /// pin masked) until it is ended (acknowledged, or the guest's
        /// end-of-interrupt). `Busy` if the firmware keeps the pin.
        19 => fn gpio_interrupt(device: u32, index: u32) -> Result<Interrupt, PciError>;
        /// The firmware's tables that describe the function's hardware
        /// rather than the machine's, whole, as the firmware has them: an
        /// Intel audio controller's NHLT (the links of its DSP, and the
        /// microphones and ports on them). None if there are none.
        20 => fn acpi_tables() -> Vec<Vec<u8>>;
        /// The memory the firmware keeps for the function, which a guest
        /// it is given to must have where the PC has it (its IOMMU domain
        /// maps it there): only the driver VM's functions get it
        /// (`Denied`).
        21 => fn reserved_memory() -> Result<Vec<ReservedMemory>, PciError>;
        /// An Intel integrated GPU's OpRegion, as the firmware has it
        /// (`NotFound`: it has none; `Unsupported`: the function is no
        /// Intel GPU).
        22 => fn opregion() -> Result<OpRegion, PciError>;
        /// The PC's host bridge's registers that an Intel integrated GPU's
        /// driver reads (`Unsupported`: the function is no Intel GPU).
        23 => fn host_bridge() -> Result<HostBridge, PciError>;
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

/// Sets up entry 0 of the MSI-X table of a device with the MSI-X
/// capability for every interrupt cause it has that names entry 0 (a
/// controller's queues can each name one), and returns the interrupt to
/// wait on (`None`: no MSI-X).
pub fn enable_msix(pci: &pcidev::Client) -> Option<Interrupt> {
    let at = find_capability(pci, cap::MSI_X)?;
    let control = config_read(pci, at + 2, 2)? as u16;
    let table = config_read(pci, at + 4, 4)?;
    let vmo = pci.map_bar((table & 7) as u8).ok()?.ok()?;
    let offset = (table & !7) as usize;
    let size = vmo.size().ok()?;
    if offset + 16 > size {
        return None;
    }
    let (irq, msi) = pci.alloc_msi().ok()?.ok()?;
    let map = vrt::vm::Mapping::new(vmo, size, vabi::map_flags::READ | vabi::map_flags::WRITE).ok()?;
    // SAFETY: entry 0 of the table, inside the mapped BAR; the device reads
    // it, so the stores must reach it as they are.
    unsafe {
        let entry = map.as_ptr().add(offset) as *mut u32;
        core::ptr::write_volatile(entry, msi.address as u32);
        core::ptr::write_volatile(entry.add(1), (msi.address >> 32) as u32);
        core::ptr::write_volatile(entry.add(2), msi.data);
        core::ptr::write_volatile(entry.add(3), 0);
    }
    // Enabled, the function's mask off.
    let enabled = ((control | 0x8000) & !0x4000) as u32;
    matches!(pci.config_write(at + 2, 2, enabled), Ok(Ok(()))).then_some(irq)
}
