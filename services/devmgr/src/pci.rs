//! PCI configuration space access (port 0xCF8/0xCFC mechanism), bus
//! enumeration, and resetting a function.

use alloc::vec::Vec;
use core::ops::Range;

use vproto::pci::{Bar, DeviceInfo};
use vrt::object::IoPorts;
use vrt::time::{Duration, sleep};

const CAP_POWER_MANAGEMENT: u8 = 0x01;
const CAP_PCI_EXPRESS: u8 = 0x10;
/// PCI Express: Device Capabilities (function-level reset), Device Control
/// (initiate it), Device Status (transactions pending).
const DEVCAP: u16 = 4;
const DEVCAP_FLR: u32 = 1 << 28;
const DEVCTL: u16 = 8;
const DEVCTL_FLR: u32 = 1 << 15;
const DEVSTA: u16 = 0xA;
const DEVSTA_TRANSACTIONS_PENDING: u32 = 1 << 5;
/// Power management: Control/Status (the power state, and whether going
/// back to D0 keeps the function's state).
const PMCSR: u16 = 4;
const PMCSR_STATE: u32 = 0b11;
const PMCSR_D3HOT: u32 = 0b11;
const PMCSR_NO_SOFT_RESET: u32 = 1 << 3;
const COMMAND_DECODE: u32 = 0b11;
const COMMAND_BUS_MASTER: u32 = 1 << 2;

/// A memory BAR, as sized; address 0 where the firmware placed it nowhere.
#[derive(Debug, Clone, Copy)]
pub struct MemoryBar {
    pub index: u8,
    pub address: u64,
    pub size: u64,
    pub is64: bool,
}

/// Where the firmware placed a function (`ConfigSpace::placement`).
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    bars: [u32; 6],
    /// Its power management capability.
    pm: Option<u16>,
}

pub struct ConfigSpace {
    ports: IoPorts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
}

impl Address {
    /// The function's requester id: what its DMA and interrupts carry.
    pub fn requester_id(&self) -> u16 {
        (self.bus as u16) << 8 | (self.slot as u16 & 0x1F) << 3 | (self.function as u16 & 7)
    }
}

impl ConfigSpace {
    pub fn new(ports: IoPorts) -> ConfigSpace {
        ConfigSpace { ports }
    }

    fn select(&self, a: Address, offset: u16) {
        let addr = 0x8000_0000u32
            | (a.bus as u32) << 16
            | (a.slot as u32) << 11
            | (a.function as u32) << 8
            | (offset as u32 & 0xFC);
        self.ports.out32(0xCF8, addr);
    }

    pub fn read32(&self, a: Address, offset: u16) -> u32 {
        self.select(a, offset);
        self.ports.in32(0xCFC)
    }

    pub fn read(&self, a: Address, offset: u16, width: u8) -> u32 {
        let v = self.read32(a, offset & !3);
        let shift = (offset & 3) * 8;
        match width {
            1 => (v >> shift) & 0xFF,
            2 => (v >> shift) & 0xFFFF,
            _ => v,
        }
    }

    pub fn write32(&self, a: Address, offset: u16, value: u32) {
        self.select(a, offset);
        self.ports.out32(0xCFC, value);
    }

    pub fn write(&self, a: Address, offset: u16, width: u8, value: u32) {
        if width == 4 {
            return self.write32(a, offset, value);
        }
        let shift = (offset & 3) * 8;
        let mask = if width == 1 { 0xFFu32 } else { 0xFFFF } << shift;
        let old = self.read32(a, offset & !3);
        self.write32(a, offset & !3, (old & !mask) | ((value << shift) & mask));
    }

    /// Reads and sizes the six BARs of a type-0 header.
    fn bars(&self, a: Address) -> Vec<Bar> {
        let mut bars = Vec::new();
        let command = self.read(a, 0x04, 2);
        // Disable decoding while probing sizes.
        self.write(a, 0x04, 2, command & !0b11);
        let mut i = 0u8;
        while i < 6 {
            let off = 0x10 + i as u16 * 4;
            let orig = self.read32(a, off);
            self.write32(a, off, 0xFFFF_FFFF);
            let probe = self.read32(a, off);
            self.write32(a, off, orig);
            if orig & 1 == 1 {
                let size = (!(probe & 0xFFFF_FFFC)).wrapping_add(1) & 0xFFFF;
                if probe != 0 && size != 0 {
                    bars.push(Bar {
                        index: i,
                        io: true,
                        address: (orig & 0xFFFF_FFFC) as u64,
                        size: size as u64,
                        prefetchable: false,
                    });
                }
                i += 1;
                continue;
            }
            let is64 = (orig >> 1) & 0b11 == 0b10;
            let mut address = (orig & 0xFFFF_FFF0) as u64;
            let mut size_mask = (probe & 0xFFFF_FFF0) as u64;
            if is64 && i < 5 {
                let off_hi = off + 4;
                let orig_hi = self.read32(a, off_hi);
                self.write32(a, off_hi, 0xFFFF_FFFF);
                let probe_hi = self.read32(a, off_hi);
                self.write32(a, off_hi, orig_hi);
                address |= (orig_hi as u64) << 32;
                size_mask |= (probe_hi as u64) << 32;
            } else {
                size_mask |= 0xFFFF_FFFF_0000_0000;
            }
            let size = (!size_mask).wrapping_add(1);
            if probe != 0 && size != 0 {
                bars.push(Bar { index: i, io: false, address, size, prefetchable: orig & 0x8 != 0 });
            }
            i += if is64 { 2 } else { 1 };
        }
        self.write(a, 0x04, 2, command);
        bars
    }

    pub fn info(&self, a: Address) -> DeviceInfo {
        let id = self.read32(a, 0);
        let class = self.read32(a, 0x08);
        let irq = self.read32(a, 0x3C);
        let header = self.read(a, 0x0E, 1) & 0x7F;
        DeviceInfo {
            vendor: id as u16,
            device: (id >> 16) as u16,
            class: (class >> 24) as u8,
            subclass: (class >> 16) as u8,
            prog_if: (class >> 8) as u8,
            revision: class as u8,
            bus: a.bus,
            slot: a.slot,
            function: a.function,
            irq_line: irq as u8,
            irq_pin: (irq >> 8) as u8,
            // A BAR the firmware placed nowhere (and devmgr could not
            // place) is no driver's.
            bars: if header == 0 { self.bars(a).into_iter().filter(|b| b.address != 0).collect() } else { Vec::new() },
        }
    }

    /// Whether the function is a PCI-to-PCI bridge.
    pub fn is_bridge(&self, a: Address) -> bool {
        self.read(a, 0x0E, 1) & 0x7F == 1
    }

    /// The memory a bridge forwards to its side: its memory window and its
    /// prefetchable one (end exclusive), those it has open.
    pub fn bridge_windows(&self, a: Address) -> Vec<Range<u64>> {
        let mut windows = Vec::new();
        let (base, limit) = (self.read(a, 0x20, 2) as u64, self.read(a, 0x22, 2) as u64);
        let (start, end) = ((base & 0xFFF0) << 16, ((limit & 0xFFF0) << 16) + 0x10_0000);
        if start < end {
            windows.push(start..end);
        }
        let (base, limit) = (self.read(a, 0x24, 2) as u64, self.read(a, 0x26, 2) as u64);
        let (base_hi, limit_hi) =
            if base & 0xF == 1 { (self.read32(a, 0x28) as u64, self.read32(a, 0x2C) as u64) } else { (0, 0) };
        let start = (base_hi << 32) | ((base & 0xFFF0) << 16);
        let end = ((limit_hi << 32) | ((limit & 0xFFF0) << 16)) + 0x10_0000;
        if start < end {
            windows.push(start..end);
        }
        windows
    }

    /// The function's memory BARs, sized, those the firmware left unplaced
    /// (at 0) among them.
    pub fn memory_bars(&self, a: Address) -> Vec<MemoryBar> {
        if self.read(a, 0x0E, 1) & 0x7F != 0 {
            return Vec::new();
        }
        self.bars(a)
            .into_iter()
            .filter(|b| !b.io)
            .map(|b| {
                let is64 = (self.read32(a, 0x10 + 4 * b.index as u16) >> 1) & 0b11 == 0b10;
                MemoryBar { index: b.index, address: b.address, size: b.size, is64 }
            })
            .collect()
    }

    /// Takes the function's memory BARs out of where the firmware placed
    /// them, and its memory decoding off: as a firmware that places them
    /// nowhere leaves it (for tests).
    pub fn forget_placement(&self, a: Address) {
        let command = self.read(a, 0x04, 2);
        self.write(a, 0x04, 2, command & !0b10);
        for bar in self.memory_bars(a) {
            self.set_bar(a, bar.index, 0, bar.is64);
        }
    }

    /// Places memory BAR `index` at `address`.
    pub fn set_bar(&self, a: Address, index: u8, address: u64, is64: bool) {
        let at = 0x10 + 4 * index as u16;
        self.write32(a, at, address as u32);
        if is64 {
            self.write32(a, at + 4, (address >> 32) as u32);
        }
    }

    /// Where the firmware placed the function: its BARs, as their registers
    /// read (to put back when it loses them).
    pub fn placement(&self, a: Address) -> Placement {
        let mut bars = [0u32; 6];
        for (i, b) in bars.iter_mut().enumerate() {
            *b = self.read32(a, 0x10 + 4 * i as u16);
        }
        Placement { bars, pm: self.capability(a, CAP_POWER_MANAGEMENT) }
    }

    /// The function's power state (0 for D0 to 3 for D3hot), if it has the
    /// power management capability.
    pub fn power_state(&self, a: Address) -> Option<u32> {
        self.capability(a, CAP_POWER_MANAGEMENT).map(|at| self.read(a, at + PMCSR, 2) & PMCSR_STATE)
    }

    /// A driver's write of `value` (`width` bytes) at `offset`. A function
    /// woken from D3hot to D0 resets, unless it says it does not (No Soft
    /// Reset), and its BARs with it: once it is awake (10 ms), they are
    /// put back where the firmware placed them (`placement`). True if they
    /// had to be.
    pub fn driver_write(&self, a: Address, placement: &Placement, offset: u16, width: u8, value: u32) -> bool {
        let wakes = placement.pm.is_some_and(|at| {
            offset == at + PMCSR && value & PMCSR_STATE == 0 && self.read(a, at + PMCSR, 2) & PMCSR_STATE == PMCSR_D3HOT
        });
        self.write(a, offset, width, value);
        if !wakes {
            return false;
        }
        sleep(Duration::from_millis(10));
        let lost = (0..6).any(|i| self.read32(a, 0x10 + 4 * i as u16) != placement.bars[i]);
        if lost {
            for (i, &bar) in placement.bars.iter().enumerate() {
                self.write32(a, 0x10 + 4 * i as u16, bar);
            }
        }
        lost
    }

    /// Where capability `id` is in the function's list, if it has it.
    pub fn capability(&self, a: Address, id: u8) -> Option<u16> {
        if self.read(a, 0x06, 2) & (1 << 4) == 0 {
            return None;
        }
        let mut at = (self.read(a, 0x34, 1) & 0xFC) as u16;
        // At most 48 entries fit in 256 bytes.
        for _ in 0..48 {
            if at == 0 {
                return None;
            }
            if self.read(a, at, 1) as u8 == id {
                return Some(at);
            }
            at = (self.read(a, at + 1, 1) & 0xFC) as u16;
        }
        None
    }

    /// Resets the function to what it was at power-on, but where the
    /// firmware placed it: a function-level reset if it has one (PCI
    /// Express), else going through D3hot if that resets it. Its header
    /// (BARs, expansion ROM, interrupt line) and its PCI Express device
    /// control are put back, its decoding enabled and its DMA not. Says how,
    /// or `None` if it cannot be reset.
    pub fn reset(&self, a: Address) -> Option<&'static str> {
        let header: Vec<u32> = (0..16).map(|i| self.read32(a, i * 4)).collect();
        let command = header[1] & 0xFFFF;
        let express = self.capability(a, CAP_PCI_EXPRESS);
        let devctl = express.map(|at| self.read(a, at + DEVCTL, 2));
        // Its DMA stops first.
        self.write(a, 0x04, 2, command & !COMMAND_BUS_MASTER);
        let how = if let Some(at) = express.filter(|&at| self.read32(a, at + DEVCAP) & DEVCAP_FLR != 0) {
            // What it has started finishes (up to 100 ms) before the reset,
            // which takes up to 100 ms.
            for _ in 0..10 {
                if self.read(a, at + DEVSTA, 2) & DEVSTA_TRANSACTIONS_PENDING == 0 {
                    break;
                }
                sleep(Duration::from_millis(10));
            }
            self.write(a, at + DEVCTL, 2, devctl.unwrap_or(0) | DEVCTL_FLR);
            sleep(Duration::from_millis(100));
            "function-level reset"
        } else if let Some(at) = self
            .capability(a, CAP_POWER_MANAGEMENT)
            .filter(|&at| self.read(a, at + PMCSR, 2) & PMCSR_NO_SOFT_RESET == 0)
        {
            let pmcsr = self.read(a, at + PMCSR, 2) & !PMCSR_STATE;
            self.write(a, at + PMCSR, 2, pmcsr | PMCSR_D3HOT);
            sleep(Duration::from_millis(10));
            self.write(a, at + PMCSR, 2, pmcsr);
            sleep(Duration::from_millis(10));
            "reset through D3hot"
        } else {
            self.write(a, 0x04, 2, command & COMMAND_DECODE);
            return None;
        };
        // It answers again within a second.
        for _ in 0..100 {
            if self.read32(a, 0) & 0xFFFF != 0xFFFF {
                break;
            }
            sleep(Duration::from_millis(10));
        }
        // Cache line size and latency timer, the BARs, the expansion ROM,
        // the interrupt line; then the command.
        for i in [3usize, 4, 5, 6, 7, 8, 9, 12, 15] {
            self.write32(a, i as u16 * 4, header[i]);
        }
        if let (Some(at), Some(ctl)) = (express, devctl) {
            self.write(a, at + DEVCTL, 2, ctl & !DEVCTL_FLR);
        }
        self.write(a, 0x04, 2, command & COMMAND_DECODE);
        Some(how)
    }

    /// Enumerates every function on every bus reachable from bus 0.
    pub fn scan(&self) -> Vec<Address> {
        let mut found = Vec::new();
        let mut buses: Vec<u8> = alloc::vec![0];
        let mut seen = [false; 256];
        while let Some(bus) = buses.pop() {
            if seen[bus as usize] {
                continue;
            }
            seen[bus as usize] = true;
            for slot in 0..32u8 {
                for function in 0..8u8 {
                    let a = Address { bus, slot, function };
                    let id = self.read32(a, 0);
                    if id & 0xFFFF == 0xFFFF {
                        if function == 0 {
                            break;
                        }
                        continue;
                    }
                    found.push(a);
                    let header = self.read(a, 0x0E, 1);
                    // PCI-to-PCI bridge: scan the secondary bus.
                    if header & 0x7F == 1 {
                        buses.push(self.read(a, 0x19, 1) as u8);
                    }
                    if function == 0 && header & 0x80 == 0 {
                        break;
                    }
                }
            }
        }
        found
    }
}
