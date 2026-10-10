//! The host controller itself: its registers, taking it over from the
//! firmware and resetting it, the command and event rings, interrupts, the
//! root hub ports, and the memory the controller reads contexts from.
//!
//! Nothing here waits for the controller beyond its start-up; the waiting
//! for commands and transfers is in `main.rs`.

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

use vabi::{WaitItem, map_flags, signals};
use vproto::pci::{self, DeviceInfo, pcidev};
use vrt::object::{Interrupt, Resource};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;
use vusb::xhci::{EndpointContext, SlotContext, Trb};
use vvirtio::DmaBuffer;

use crate::ring::{EventRing, Ring};

/// Capability registers (section 5.3).
mod cap {
    /// CAPLENGTH in bits 0-7, HCIVERSION in bits 16-31.
    pub const CAPLENGTH: usize = 0x00;
    pub const HCSPARAMS1: usize = 0x04;
    pub const HCSPARAMS2: usize = 0x08;
    pub const HCCPARAMS1: usize = 0x10;
    pub const DBOFF: usize = 0x14;
    pub const RTSOFF: usize = 0x18;
}

/// Operational registers, from CAPLENGTH (section 5.4).
mod op {
    pub const USBCMD: usize = 0x00;
    pub const USBSTS: usize = 0x04;
    pub const PAGESIZE: usize = 0x08;
    pub const CRCR: usize = 0x18;
    pub const DCBAAP: usize = 0x30;
    pub const CONFIG: usize = 0x38;
    pub const PORTSC: usize = 0x400;
}

/// The registers of interrupter 0, from RTSOFF (section 5.5).
mod ir {
    pub const IMAN: usize = 0x20;
    pub const IMOD: usize = 0x24;
    pub const ERSTSZ: usize = 0x28;
    pub const ERSTBA: usize = 0x30;
    pub const ERDP: usize = 0x38;
}

const HCC_AC64: u32 = 1 << 0;
const HCC_CSZ: u32 = 1 << 2;
const HCC_PPC: u32 = 1 << 3;
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const CMD_HSEE: u32 = 1 << 3;
const STS_HALTED: u32 = 1 << 0;
const STS_HSE: u32 = 1 << 2;
const STS_EINT: u32 = 1 << 3;
const STS_CNR: u32 = 1 << 11;
const STS_HCE: u32 = 1 << 12;
/// The USBSTS bits that are cleared by writing 1.
const STS_CHANGES: u32 = STS_HSE | STS_EINT | 1 << 4 | 1 << 10;
const CRCR_RCS: u64 = 1 << 0;
const CRCR_CA: u32 = 1 << 2;
const CRCR_CRR: u32 = 1 << 3;
const IMAN_IP: u32 = 1 << 0;
const IMAN_IE: u32 = 1 << 1;
const ERDP_EHB: u64 = 1 << 3;
/// Interrupt moderation: at most one interrupt per 250 µs (in 250 ns).
const IMOD_INTERVAL: u32 = 1000;

/// Extended capabilities (section 7).
const EXT_LEGACY: u8 = 1;
const EXT_PROTOCOL: u8 = 2;
/// USBLEGSUP: the firmware's and the OS's ownership semaphores.
const BIOS_OWNED: u32 = 1 << 16;
const OS_OWNED: u32 = 1 << 24;
/// USBLEGCTLSTS: the bits to keep (reserved) when turning the SMIs off,
/// and the SMI events, cleared by writing 1.
const LEGACY_KEEP: u32 = 0x7 << 1 | 0xFF << 5 | 0x7 << 17;
const LEGACY_EVENTS: u32 = 0x7 << 29;

/// Intel chipsets of the 7, 8 and 9 series route their USB ports to the
/// EHCI controllers until the OS moves them to xHCI.
const INTEL_SWITCHABLE: [u16; 5] = [0x1E31, 0x8C31, 0x9C31, 0x8CB1, 0x9CB1];
const INTEL_XUSB2PR: u16 = 0xD0;
const INTEL_USB2PRM: u16 = 0xD4;
const INTEL_USB3_PSSEN: u16 = 0xD8;
const INTEL_USB3PRM: u16 = 0xDC;

/// Endpoint states in an output endpoint context.
pub mod endpoint_state {
    pub const RUNNING: u8 = 1;
    pub const HALTED: u8 = 2;
}

/// PORTSC bits (section 5.4.8).
pub mod portsc {
    /// Current connect status.
    pub const CCS: u32 = 1 << 0;
    /// Port enabled.
    pub const PED: u32 = 1 << 1;
    /// Port reset.
    pub const PR: u32 = 1 << 4;
    /// Port power.
    pub const PP: u32 = 1 << 9;
    /// Connect status change.
    pub const CSC: u32 = 1 << 17;
    /// Warm port reset change.
    pub const WRC: u32 = 1 << 19;
    /// Port reset change.
    pub const PRC: u32 = 1 << 21;
    /// Warm port reset (USB 3 ports).
    pub const WPR: u32 = 1 << 31;
    /// The change bits, cleared by writing 1.
    pub const CHANGES: u32 = 0x7F << 17;
    /// The bits a write must carry over: the link state (only written
    /// with its strobe), power, the indicators and the wake enables.
    /// Everything else is a no-op as 0, and PED would disable the port
    /// as 1.
    pub const KEEP: u32 = 0xF << 5 | PP | 0x3 << 14 | 0x7 << 25;

    /// The port speed ID.
    pub fn speed(v: u32) -> u8 {
        ((v >> 10) & 0xF) as u8
    }
}

/// The memory-mapped registers.
pub struct Registers {
    _map: Mapping,
    base: *mut u8,
    len: usize,
    op: usize,
    rt: usize,
    db: usize,
}

impl Registers {
    /// The 32-bit register at `off` (all ones outside the window, as for a
    /// device that is gone).
    fn read(&self, off: usize) -> u32 {
        if !off.is_multiple_of(4) || off + 4 > self.len {
            return u32::MAX;
        }
        // SAFETY: an aligned register inside the mapped BAR.
        unsafe { read_volatile(self.base.add(off) as *const u32) }
    }

    fn write(&self, off: usize, v: u32) {
        if off.is_multiple_of(4) && off + 4 <= self.len {
            // SAFETY: an aligned register inside the mapped BAR.
            unsafe { write_volatile(self.base.add(off) as *mut u32, v) }
        }
    }

    /// A 64-bit register, as two 32-bit writes, low half first.
    fn write64(&self, off: usize, v: u64) {
        self.write(off, v as u32);
        self.write(off + 4, (v >> 32) as u32);
    }

    fn op(&self, off: usize) -> u32 {
        self.read(self.op + off)
    }

    fn set_op(&self, off: usize, v: u32) {
        self.write(self.op + off, v)
    }

    /// Waits up to `ms` milliseconds for `done`.
    fn wait(&self, ms: u64, mut done: impl FnMut(&Registers) -> bool) -> bool {
        let end = now_ns() + ms * 1_000_000;
        loop {
            if done(self) {
                return true;
            }
            if now_ns() > end {
                return false;
            }
            vrt::time::sleep(Duration::from_millis(1));
        }
    }

    /// The extended capabilities: (offset, ID).
    fn extended_capabilities(&self, hccparams1: u32) -> Vec<(usize, u8)> {
        let mut caps = Vec::new();
        let mut at = (hccparams1 >> 16) as usize * 4;
        // A list longer than this is not a list.
        for _ in 0..64 {
            if at == 0 || at + 4 > self.len {
                break;
            }
            let v = self.read(at);
            caps.push((at, v as u8));
            let next = ((v >> 8) & 0xFF) as usize * 4;
            if next == 0 {
                break;
            }
            at += next;
        }
        caps
    }
}

/// A port of the root hub.
#[derive(Debug, Clone, Copy, Default)]
pub struct RootPort {
    /// A USB 3 port (SuperSpeed and faster); otherwise USB 2.
    pub usb3: bool,
}

/// A running host controller.
pub struct Controller {
    regs: Registers,
    dma: Resource,
    below_4g: bool,
    irq: Option<Interrupt>,
    /// How the controller interrupts: "MSI", "MSI-X" or "polling".
    pub interrupts: &'static str,
    /// HCIVERSION (0x0100 = 1.0).
    pub version: u16,
    pub max_slots: u8,
    /// Bytes per context: 32, or 64 on controllers that want it.
    context_size: usize,
    pub ports: Vec<RootPort>,
    dcbaa: DmaBuffer,
    _scratchpads: Option<(DmaBuffer, DmaBuffer)>,
    commands: Ring,
    events: EventRing,
    /// The input context of the command being issued (one at a time).
    input: DmaBuffer,
    /// The data of the control transfer being made (one at a time).
    pub control_buffer: DmaBuffer,
}

/// Size of the control transfer buffer: the largest descriptor.
pub const CONTROL_BUFFER: usize = 64 * 1024;

impl Controller {
    /// Takes the controller over from the firmware, resets it and starts
    /// it.
    pub fn start(pci: &pcidev::Client, info: &DeviceInfo) -> Result<Controller, &'static str> {
        if !matches!(pci.enable(true), Ok(Ok(()))) {
            return Err("cannot enable the device");
        }
        let Ok(Ok(dma)) = pci.dma_resource() else { return Err("no DMA resource") };
        let Ok(Ok(vmo)) = pci.map_bar(0) else { return Err("cannot map the registers") };
        let len = vmo.size().map_err(|_| "cannot map the registers")?;
        let map = Mapping::new(vmo, len, map_flags::READ | map_flags::WRITE).map_err(|_| "cannot map the registers")?;
        let mut regs = Registers { base: map.as_ptr(), _map: map, len, op: 0, rt: 0, db: 0 };
        let cap0 = regs.read(cap::CAPLENGTH);
        let (hcs1, hcs2, hcc1) = (regs.read(cap::HCSPARAMS1), regs.read(cap::HCSPARAMS2), regs.read(cap::HCCPARAMS1));
        if cap0 == u32::MAX || (cap0 & 0xFF) < 0x20 {
            return Err("the controller does not answer");
        }
        regs.op = (cap0 & 0xFF) as usize;
        regs.rt = (regs.read(cap::RTSOFF) & !0x1F) as usize;
        regs.db = (regs.read(cap::DBOFF) & !0x3) as usize;
        let (max_slots, max_ports) = (hcs1 as u8, (hcs1 >> 24) as u8);
        if max_slots == 0
            || max_ports == 0
            || regs.op + op::PORTSC + 0x10 * max_ports as usize > len
            || regs.rt + ir::ERDP + 8 > len
            || regs.db + 4 * (max_slots as usize + 1) > len
        {
            return Err("the registers do not fit their window");
        }

        take_from_firmware(&regs, hcc1);
        route_intel_ports(pci, info);
        if !regs.wait(1000, |r| r.op(op::USBSTS) & STS_CNR == 0) {
            return Err("the controller does not become ready");
        }
        regs.set_op(op::USBCMD, regs.op(op::USBCMD) & !CMD_RUN);
        if !regs.wait(100, |r| r.op(op::USBSTS) & STS_HALTED != 0) {
            return Err("the controller does not stop");
        }
        regs.set_op(op::USBCMD, CMD_RESET);
        // Some controllers must not be touched right after a reset.
        vrt::time::sleep(Duration::from_millis(1));
        if !regs.wait(2000, |r| r.op(op::USBCMD) & CMD_RESET == 0 && r.op(op::USBSTS) & STS_CNR == 0) {
            return Err("the controller does not reset");
        }
        if regs.op(op::PAGESIZE) & 1 == 0 {
            return Err("4 KiB pages are not supported");
        }

        // Without 64-bit addressing, everything it reads must be below
        // 4 GiB.
        let below_4g = hcc1 & HCC_AC64 == 0;
        let alloc = |len: usize| {
            if below_4g { DmaBuffer::new_below_4g(&dma, len) } else { DmaBuffer::new(&dma, len) }
                .map_err(|_| "out of DMA memory")
        };
        let dcbaa = alloc(4096)?;
        let scratchpad_count = (((hcs2 >> 21) & 0x1F) << 5 | hcs2 >> 27) as usize;
        let scratchpads = if scratchpad_count > 0 {
            let array = alloc(scratchpad_count * 8)?;
            let pages = alloc(scratchpad_count * 4096)?;
            for i in 0..scratchpad_count {
                array.write(i * 8, &(pages.phys() + (i * 4096) as u64).to_le_bytes());
            }
            dcbaa.write(0, &array.phys().to_le_bytes());
            Some((array, pages))
        } else {
            None
        };
        let commands = Ring::new(alloc(4096)?);
        let events = EventRing::new(alloc(4096)?, alloc(4096)?);
        let input = alloc(4096)?;
        let control_buffer = alloc(CONTROL_BUFFER)?;

        regs.set_op(op::CONFIG, max_slots as u32);
        regs.write64(regs.op + op::DCBAAP, dcbaa.phys());
        regs.write64(regs.op + op::CRCR, commands.base() | CRCR_RCS);
        regs.write(regs.rt + ir::ERSTSZ, 1);
        regs.write64(regs.rt + ir::ERDP, events.dequeue_pointer());
        regs.write64(regs.rt + ir::ERSTBA, events.table());
        regs.write(regs.rt + ir::IMOD, IMOD_INTERVAL);
        let (irq, interrupts) = match enable_msix(pci, &regs) {
            Some(irq) => (Some(irq), "MSI-X"),
            None => match pci::enable_msi(pci) {
                Some(irq) => (Some(irq), "MSI"),
                None => (None, "polling"),
            },
        };
        regs.write(regs.rt + ir::IMAN, IMAN_IP | if irq.is_some() { IMAN_IE } else { 0 });
        regs.set_op(op::USBSTS, STS_CHANGES);
        regs.set_op(op::USBCMD, CMD_RUN | CMD_HSEE | if irq.is_some() { CMD_INTE } else { 0 });
        if !regs.wait(100, |r| r.op(op::USBSTS) & STS_HALTED == 0) {
            return Err("the controller does not start");
        }

        let ports = port_protocols(&regs, hcc1, max_ports);
        let controller = Controller {
            regs,
            dma,
            below_4g,
            irq,
            interrupts,
            version: (cap0 >> 16) as u16,
            max_slots,
            context_size: if hcc1 & HCC_CSZ != 0 { 64 } else { 32 },
            ports,
            dcbaa,
            _scratchpads: scratchpads,
            commands,
            events,
            input,
            control_buffer,
        };
        if hcc1 & HCC_PPC != 0 {
            // Ports with power switches start unpowered after a reset.
            for port in 1..=max_ports {
                controller.write_portsc(port, portsc::PP);
            }
            vrt::time::sleep(Duration::from_millis(20));
        }
        Ok(controller)
    }

    /// Whether the controller still works (no host system or controller
    /// error).
    pub fn healthy(&self) -> bool {
        let sts = self.regs.op(op::USBSTS);
        sts != u32::MAX && sts & (STS_HSE | STS_HCE) == 0
    }

    // Memory.

    /// A zeroed DMA buffer the controller can reach.
    pub fn alloc(&self, len: usize) -> Option<DmaBuffer> {
        if self.below_4g { DmaBuffer::new_below_4g(&self.dma, len) } else { DmaBuffer::new(&self.dma, len) }.ok()
    }

    /// A new transfer ring.
    pub fn ring(&self) -> Option<Ring> {
        self.alloc(4096).map(Ring::new)
    }

    /// Room for a device context (32 contexts).
    pub fn alloc_device_context(&self) -> Option<DmaBuffer> {
        self.alloc(32 * self.context_size)
    }

    /// Points the controller at a slot's device context (0: none).
    pub fn set_device_context(&self, slot: u8, address: u64) {
        self.dcbaa.write(slot as usize * 8, &address.to_le_bytes());
    }

    // The input context: control context, slot context, then endpoint
    // contexts by device context index.

    pub fn input_context(&self) -> u64 {
        self.input.phys()
    }

    /// Clears the input context and sets which contexts the next command
    /// adds (bit 0 the slot, bit `i` endpoint context `i`).
    pub fn input_reset(&self, add: u32) {
        self.input.write(0, &[0; 33 * 64]);
        write_dwords(&self.input, 0, &vusb::xhci::input_control(add));
    }

    /// As [`Controller::input_reset`], for a Configure Endpoint that also
    /// drops the endpoint contexts of `drop`.
    pub fn input_change(&self, drop: u32, add: u32) {
        self.input.write(0, &[0; 33 * 64]);
        write_dwords(&self.input, 0, &vusb::xhci::input_control_change(drop, add));
    }

    pub fn input_slot(&self, slot: &SlotContext) {
        write_dwords(&self.input, self.context_size, &slot.to_dwords());
    }

    pub fn input_endpoint(&self, index: u8, endpoint: &EndpointContext) {
        write_dwords(&self.input, (index as usize + 1) * self.context_size, &endpoint.to_dwords());
    }

    /// Copies an endpoint context from a device context into the input
    /// context, with another maximum packet size (Evaluate Context of
    /// endpoint 0).
    pub fn input_endpoint_with_packet_size(&self, device: &DmaBuffer, index: u8, max_packet_size: u16) {
        let mut d = [0u32; 5];
        for (i, dword) in d.iter_mut().enumerate() {
            *dword = read_dword(device, index as usize * self.context_size + i * 4);
        }
        d[1] = (d[1] & 0xFFFF) | (max_packet_size as u32) << 16;
        write_dwords(&self.input, (index as usize + 1) * self.context_size, &d);
    }

    /// The slot context of a device context, as the controller keeps it.
    pub fn device_slot(&self, device: &DmaBuffer) -> SlotContext {
        SlotContext::from_dwords([0, 1, 2, 3].map(|i| read_dword(device, i * 4)))
    }

    /// The state of an endpoint, from a device context.
    pub fn endpoint_state(&self, device: &DmaBuffer, index: u8) -> u8 {
        (read_dword(device, index as usize * self.context_size) & 7) as u8
    }

    /// Where a stopped endpoint stands on its ring, from a device context.
    pub fn endpoint_dequeue(&self, device: &DmaBuffer, index: u8) -> u64 {
        let at = index as usize * self.context_size;
        (read_dword(device, at + 8) as u64 | (read_dword(device, at + 12) as u64) << 32) & !0xF
    }

    // Commands, doorbells and events.

    /// Puts a command on the command ring and rings for it; returns the
    /// address its completion event names.
    pub fn submit_command(&mut self, trb: Trb) -> u64 {
        let at = self.commands.push(trb);
        self.ring_doorbell(0, 0);
        at
    }

    /// Aborts the command being executed (it takes too long).
    pub fn abort_command(&self) {
        let crcr = self.regs.op + op::CRCR;
        self.regs.write(crcr, CRCR_CA);
        self.regs.write(crcr + 4, 0);
        if !self.regs.wait(5000, |r| r.op(op::CRCR) & CRCR_CRR == 0) {
            println!("the command ring does not stop");
        }
    }

    /// Tells the controller about new TRBs: doorbell 0 for commands, the
    /// slot's for its endpoints (`target`: the device context index).
    pub fn ring_doorbell(&self, slot: u8, target: u8) {
        // The TRBs must be in memory before the controller looks.
        fence(Ordering::SeqCst);
        self.regs.write(self.regs.db + 4 * slot as usize, target as u32);
    }

    /// Waits for an interrupt until `deadline` (without interrupts, for a
    /// moment).
    pub fn wait(&self, deadline: u64) {
        match &self.irq {
            Some(irq) => {
                let _ = irq.wait_irq(deadline);
                let _ = irq.ack();
            }
            None => vrt::time::sleep_until(deadline.min(now_ns() + POLL_INTERVAL_NS)),
        }
        // Acknowledge, so the controller interrupts for the next events.
        self.regs.set_op(op::USBSTS, STS_EINT);
        self.regs.write(self.regs.rt + ir::IMAN, IMAN_IP | if self.irq.is_some() { IMAN_IE } else { 0 });
    }

    /// Waits as [`Controller::wait`] does, or until one of `extra`'s
    /// signals (which it then says).
    pub fn wait_also(&self, extra: &mut [WaitItem], deadline: u64) {
        if extra.is_empty() {
            return self.wait(deadline);
        }
        let mut items: Vec<WaitItem> = Vec::with_capacity(extra.len() + 1);
        let deadline = match &self.irq {
            Some(irq) => {
                items.push(WaitItem { handle: irq.raw(), signals: signals::SIGNALED, ..Default::default() });
                deadline
            }
            None => deadline.min(now_ns() + POLL_INTERVAL_NS),
        };
        let first = items.len();
        items.extend_from_slice(extra);
        let _ = vrt::object::wait_many(&mut items, deadline);
        if let Some(irq) = &self.irq
            && items[0].observed & signals::SIGNALED != 0
        {
            let _ = irq.ack();
        }
        for (e, i) in extra.iter_mut().zip(&items[first..]) {
            e.observed = i.observed;
        }
        self.regs.set_op(op::USBSTS, STS_EINT);
        self.regs.write(self.regs.rt + ir::IMAN, IMAN_IP | if self.irq.is_some() { IMAN_IE } else { 0 });
    }

    /// The next event, if any.
    pub fn next_event(&mut self) -> Option<Trb> {
        self.events.pop()
    }

    /// Tells the controller how far the events have been read.
    pub fn events_done(&self) {
        self.regs.write64(self.regs.rt + ir::ERDP, self.events.dequeue_pointer() | ERDP_EHB);
    }

    // Root hub ports (from 1).

    pub fn portsc(&self, port: u8) -> u32 {
        self.regs.op(op::PORTSC + 0x10 * (port as usize - 1))
    }

    /// Writes `bits` (actions, or change bits to clear) to a port's PORTSC,
    /// keeping its settings.
    pub fn write_portsc(&self, port: u8, bits: u32) {
        let at = op::PORTSC + 0x10 * (port as usize - 1);
        self.regs.set_op(at, (self.regs.op(at) & portsc::KEEP) | bits);
    }
}

/// Polling period without interrupts.
const POLL_INTERVAL_NS: u64 = 4_000_000;

fn write_dwords(buffer: &DmaBuffer, at: usize, dwords: &[u32]) {
    for (i, d) in dwords.iter().enumerate() {
        buffer.write(at + i * 4, &d.to_le_bytes());
    }
}

fn read_dword(buffer: &DmaBuffer, at: usize) -> u32 {
    assert!(at + 4 <= buffer.len());
    // SAFETY: inside the buffer; the controller writes contexts a dword at
    // a time.
    unsafe { read_volatile(buffer.ptr().add(at) as *const u32) }
}

/// Asks the firmware to let go of the controller (it may be driving the
/// keyboard for the boot menus, through SMIs), and stops its SMIs.
fn take_from_firmware(regs: &Registers, hccparams1: u32) {
    let Some(at) = regs.extended_capabilities(hccparams1).into_iter().find(|c| c.1 == EXT_LEGACY).map(|c| c.0) else {
        return;
    };
    let v = regs.read(at);
    if v & BIOS_OWNED != 0 {
        regs.write(at, v | OS_OWNED);
        if !regs.wait(1000, |r| r.read(at) & BIOS_OWNED == 0) {
            println!("the firmware does not hand the controller over; taking it");
            regs.write(at, (regs.read(at) & !BIOS_OWNED) | OS_OWNED);
        }
    }
    let control = regs.read(at + 4);
    regs.write(at + 4, (control & LEGACY_KEEP) | LEGACY_EVENTS);
}

/// Moves the USB ports that Intel 7 to 9 series chipsets share with their
/// EHCI controllers over to xHCI, as far as the firmware allows.
fn route_intel_ports(pci: &pcidev::Client, info: &DeviceInfo) {
    if info.vendor != 0x8086 || !INTEL_SWITCHABLE.contains(&info.device) {
        return;
    }
    for (mask, enable) in [(INTEL_USB3PRM, INTEL_USB3_PSSEN), (INTEL_USB2PRM, INTEL_XUSB2PR)] {
        if let Ok(Ok(allowed)) = pci.config_read(mask, 4) {
            let _ = pci.config_write(enable, 4, allowed);
        }
    }
}

/// Programs MSI-X vector 0 (when the table is in the register BAR).
fn enable_msix(pci: &pcidev::Client, regs: &Registers) -> Option<Interrupt> {
    let at = pci::find_capability(pci, pci::cap::MSI_X)?;
    let control = pci.config_read(at + 2, 2).ok()?.ok()?;
    let table = pci.config_read(at + 4, 4).ok()?.ok()?;
    let entry = (table & !7) as usize;
    if table & 7 != 0 || entry + 16 > regs.len {
        return None;
    }
    let (irq, msi) = pci.alloc_msi().ok()?.ok()?;
    regs.write(entry, msi.address as u32);
    regs.write(entry + 4, (msi.address >> 32) as u32);
    regs.write(entry + 8, msi.data);
    regs.write(entry + 12, 0);
    // Enable, and clear the function mask.
    pci.config_write(at + 2, 2, (control | 0x8000) & !0x4000).ok()?.ok()?;
    Some(irq)
}

/// Which root ports are USB 3, from the supported protocol capabilities.
fn port_protocols(regs: &Registers, hccparams1: u32, max_ports: u8) -> Vec<RootPort> {
    let mut ports = vec![RootPort::default(); max_ports as usize];
    for (at, id) in regs.extended_capabilities(hccparams1) {
        if id != EXT_PROTOCOL {
            continue;
        }
        let major = regs.read(at) >> 24;
        let range = regs.read(at + 8);
        let (first, count) = ((range & 0xFF) as usize, ((range >> 8) & 0xFF) as usize);
        for port in ports.iter_mut().skip(first.saturating_sub(1)).take(count) {
            port.usb3 = major >= 3;
        }
    }
    ports
}
