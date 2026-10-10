//! The virtual machine: its memory, its processors, its devices and its
//! hypercalls.
//!
//! Guest-physical memory is one VMO, mapped at 0 in the guest and here.
//! Its first 64 KiB hold what the kernel is entered with (a GDT, page
//! tables that map the first 4 GiB as they are, the boot parameters and
//! the command line) and stay reserved: the processors Linux starts later
//! start on the same page tables. As on a PC, the memory map leaves out
//! the legacy area below 1 MiB. The kernel goes where it prefers to be,
//! the initial RAM file system at the top of memory.
//!
//! The rest of the guest-physical address space:
//!
//! | Where | What |
//! |-------|------|
//! | 3 GiB to 4 GiB − 20 MiB | BARs of the guest's PCI functions |
//! | 64 GiB to 128 GiB | the bridge's window: VMOs guest programs map |
//! | 128 GiB to 256 GiB | BARs that do not fit below 4 GiB |

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::{VcpuExit, map_flags, vcpu_exit};
use vhv::linux::{self, E820_RAM, E820_RESERVED, MemoryRange};
use vhv::platform::{self, error, hypercall, power};
use vrt::object::{Channel, Guest, Resource, Vcpu, Vmo};
use vrt::println;
use vrt::sync::Mutex;
use vrt::vm::Mapping;

use crate::bridge::Bridge;
use crate::i8042::{self, I8042};
use crate::memory::GuestMemory;
use crate::pci::{Devices, Windows};

/// How the machine is made.
pub struct Config {
    pub memory_mib: u32,
    pub cpus: u32,
    /// Added to the kernel's command line (each option after a space).
    pub cmdline: String,
}

impl Default for Config {
    fn default() -> Config {
        Config { memory_mib: 256, cpus: 2, cmdline: String::new() }
    }
}

const MIB: u64 = 1 << 20;
const GDT_AT: u64 = 0x1000;
const PAGE_TABLES_AT: u64 = 0x2000;
const BOOT_PARAMS_AT: u64 = 0x8000;
const CMDLINE_AT: u64 = 0x9000;
/// The loader's structures, which stay reserved.
const LOW_RESERVED: u64 = 0x1_0000;
/// The legacy video and firmware area below 1 MiB.
const LEGACY_START: u64 = 0x9_F000;
const LEGACY_END: u64 = 0x10_0000;
/// The kernel's console is the platform's, from its first message on.
const CMDLINE: &str = "earlycon=veda console=hvc0 panic=-1 rdinit=/init";
/// The register of the boot parameters at entry.
const RSI: usize = 6;
/// Where PCI functions' BARs go: below 4 GiB (RAM ends before), above the
/// bridge's window.
const PCI_LOW: core::ops::Range<u64> = 0xC000_0000..0xFEC0_0000;
const PCI_HIGH: core::ops::Range<u64> = (128 << 30)..(256 << 30);

/// How the process ends, for whoever started it.
pub mod exit_code {
    /// Linux powered the machine off.
    pub const POWER_OFF: i32 = 0;
    /// Linux asked for a restart.
    pub const RESTART: i32 = 2;
    /// Linux crashed, or shut its processor down.
    pub const CRASHED: i32 = 3;
    /// The guest did what the platform does not allow, or could not run.
    pub const FAULT: i32 = 4;
}

pub struct Machine {
    guest: Guest,
    memory: GuestMemory,
    bridge: Bridge,
    devices: Devices,
    /// The keyboard controller, if the guest has it.
    i8042: Option<I8042>,
    /// The guest's first processor, which the bridge's interrupts go to.
    notify: Vcpu,
    /// The processors that have started (by APIC id), for the devices'
    /// interrupts.
    vcpus: Mutex<Vec<Option<Vcpu>>>,
    /// The console line each processor is writing (the kernel's messages
    /// and a program's output, written at once on two processors, stay
    /// apart).
    console: Mutex<Vec<Vec<u8>>>,
    /// I/O ports the guest touched (each reported once).
    ports: Mutex<Vec<u16>>,
}

impl Machine {
    /// Makes the machine, gives it the PCI functions of `devices` (their
    /// `pcidev` channels) and the keyboard controller, loads Linux and
    /// starts its first processor.
    pub fn start(
        hypervisor: &Resource,
        kernel: &[u8],
        initramfs: &[u8],
        devices: Vec<Channel>,
        i8042: Option<I8042>,
        config: &Config,
    ) -> Result<Arc<Machine>, String> {
        let size = config.memory_mib as u64 * MIB;
        if size > PCI_LOW.start {
            return Err(format!("the guest can have at most {} MiB", PCI_LOW.start / MIB));
        }
        let image = linux::parse(kernel).map_err(|e| format!("the kernel: {e}"))?;
        let vmo = Vmo::create_committed(size as usize).map_err(|e| format!("no memory for the guest: {e}"))?;
        let memory = Mapping::new(vmo, size as usize, map_flags::READ | map_flags::WRITE)
            .map_err(|e| format!("cannot map the guest's memory: {e}"))?;
        let memory = GuestMemory::new(memory);
        let guest = Guest::create(hypervisor, config.cpus).map_err(|e| format!("no virtual machine: {e}"))?;
        guest
            .map(memory.vmo(), 0, size as usize, 0, map_flags::READ | map_flags::WRITE | map_flags::EXECUTE)
            .map_err(|e| format!("cannot give the guest its memory: {e}"))?;
        let devices = if devices.is_empty() {
            Devices::none()
        } else {
            Devices::attach(&guest, devices, &mut Windows { low: PCI_LOW, high: PCI_HIGH })
        };

        // The kernel where it prefers to be, the initial RAM file system at
        // the top, between them the room the kernel unpacks itself into.
        let kernel_at = image.preferred_address.max(LEGACY_END);
        let initrd_at = (size - initramfs.len() as u64) & !0xFFF;
        if kernel_at + image.init_size.max(image.protected.len() as u64) > initrd_at {
            return Err(format!("{} MiB is too little memory for the guest", config.memory_mib));
        }
        let mut cmdline = String::from(CMDLINE);
        cmdline.push_str(&devices.host_options());
        if i8042.is_some() {
            cmdline.push_str(" veda.i8042");
        }
        cmdline.push_str(config.cmdline.trim_end());
        if cmdline.len() > image.cmdline_size as usize || cmdline.len() >= 4096 {
            return Err(String::from("the kernel's command line is too long"));
        }
        cmdline.push('\0');
        let ranges = [
            MemoryRange { start: 0, len: LOW_RESERVED, kind: E820_RESERVED },
            MemoryRange { start: LOW_RESERVED, len: LEGACY_START - LOW_RESERVED, kind: E820_RAM },
            MemoryRange { start: LEGACY_START, len: LEGACY_END - LEGACY_START, kind: E820_RESERVED },
            MemoryRange { start: LEGACY_END, len: size - LEGACY_END, kind: E820_RAM },
        ];
        let boot = linux::Boot {
            kernel: &image,
            kernel_at,
            cmdline_at: CMDLINE_AT,
            initrd: Some((initrd_at, initramfs.len() as u64)),
            memory: &ranges,
        };
        let gdt: Vec<u8> = linux::GDT.iter().flat_map(|e| e.to_le_bytes()).collect();
        let tables = linux::identity_page_tables(PAGE_TABLES_AT);
        let mut loaded = memory.write(GDT_AT, &gdt);
        for (i, t) in tables.iter().enumerate() {
            let bytes: Vec<u8> = t.iter().flat_map(|e| e.to_le_bytes()).collect();
            loaded &= memory.write(PAGE_TABLES_AT + i as u64 * 0x1000, &bytes);
        }
        loaded &= memory.write(BOOT_PARAMS_AT, &linux::boot_params(&boot));
        loaded &= memory.write(CMDLINE_AT, cmdline.as_bytes());
        loaded &= memory.write(kernel_at, image.protected);
        loaded &= memory.write(initrd_at, initramfs);
        if !loaded {
            return Err(String::from("the kernel does not fit the guest's memory"));
        }

        let mut state = linux::long_mode(linux::entry(kernel_at), PAGE_TABLES_AT, GDT_AT);
        state.gprs[RSI] = BOOT_PARAMS_AT;
        let vcpu = guest.create_vcpu(0, &state).map_err(|e| format!("no virtual processor: {e}"))?;
        let duplicate = |v: &Vcpu| v.0.duplicate(None).map(Vcpu::from_handle).map_err(|e| format!("{e}"));
        let notify = duplicate(&vcpu)?;
        let mut vcpus: Vec<Option<Vcpu>> = (0..config.cpus).map(|_| None).collect();
        vcpus[0] = Some(duplicate(&vcpu)?);
        let machine = Arc::new(Machine {
            guest,
            memory,
            bridge: Bridge::new().map_err(|e| format!("no bridge: {e}"))?,
            devices,
            i8042,
            notify,
            vcpus: Mutex::new(vcpus),
            console: Mutex::new((0..config.cpus).map(|_| Vec::new()).collect()),
            ports: Mutex::new(Vec::new()),
        });
        // The bridge's waits and the guest's registry, on threads of their
        // own.
        let m = machine.clone();
        vrt::thread::Builder::new()
            .name("bridge-waits")
            .spawn(move || m.bridge.run_waits(&m.memory, &m.notify))
            .map_err(|e| format!("no thread for the bridge: {e}"))?;
        let m = machine.clone();
        vrt::thread::Builder::new()
            .name("registry")
            .spawn(move || m.bridge.registry().run())
            .map_err(|e| format!("no thread for the registry: {e}"))?;
        println!(
            "starting Linux ({} KiB, initramfs {} KiB) with {} MiB and {} processors",
            kernel.len() / 1024,
            initramfs.len() / 1024,
            config.memory_mib,
            config.cpus
        );
        machine.spawn(0, vcpu)?;
        Ok(machine)
    }

    /// Runs virtual processor `id` on a thread of its own.
    fn spawn(self: &Arc<Self>, id: u32, vcpu: Vcpu) -> Result<(), String> {
        let machine = self.clone();
        vrt::thread::Builder::new()
            .name("vcpu")
            .spawn(move || machine.run(id, vcpu))
            .map(|_| ())
            .map_err(|e| format!("no thread for processor {id}: {e}"))
    }

    /// The machine runs until a processor ends it.
    pub fn wait(&self) -> i32 {
        loop {
            vrt::time::sleep(vrt::time::Duration::from_secs(3600));
        }
    }

    fn run(self: Arc<Self>, id: u32, vcpu: Vcpu) {
        let mut exit = VcpuExit::default();
        loop {
            if let Err(e) = vcpu.run(&mut exit) {
                self.stop(&format!("processor {id} cannot run: {e}"), exit_code::FAULT);
            }
            match exit.reason {
                vcpu_exit::HYPERCALL => exit.data[0] = self.hypercall(id, &exit.data),
                vcpu_exit::IO => self.port(&mut exit),
                vcpu_exit::MEMORY => self.stop(
                    &format!(
                        "processor {id} reached unmapped memory at {:#x} (access {}, rip {:#x})",
                        exit.data[0], exit.data[1], exit.data[2]
                    ),
                    exit_code::FAULT,
                ),
                vcpu_exit::SHUTDOWN => self.stop(&format!("processor {id} shut down"), exit_code::CRASHED),
                _ => {
                    let state = vcpu.state().map(|s| format!("{s:x?}")).unwrap_or_default();
                    self.stop(
                        &format!(
                            "processor {id} failed (exit {:#x}, {:#x}, rip {:#x}): {state}",
                            exit.data[0], exit.data[1], exit.data[2]
                        ),
                        exit_code::FAULT,
                    )
                }
            }
        }
    }

    /// Serves processor `id`'s hypercall: the call in `data[0]`, its
    /// arguments after.
    fn hypercall(self: &Arc<Self>, id: u32, data: &[u64; 7]) -> u64 {
        match data[0] {
            hypercall::CONSOLE_WRITE => {
                self.console_write(id, data[1], [data[2], data[3], data[4], data[5]]);
                0
            }
            hypercall::START_CPU => self.start_cpu(data[1], data[2]),
            hypercall::POWER => match data[1] {
                power::OFF => self.stop("Linux powered the machine off", exit_code::POWER_OFF),
                power::RESTART => self.stop("Linux restarts the machine", exit_code::RESTART),
                power::CRASHED => self.stop("Linux crashed", exit_code::CRASHED),
                _ => error::INVALID,
            },
            hypercall::WALLCLOCK => vrt::time::utc_time_ns(),
            vhv::bridge::HYPERCALL => self.bridge.call(&self.memory, &self.guest, data[1] as u32, data[2]),
            hypercall::PCI_CONFIG_READ => self.devices.config_read(data[1], data[2], data[3]),
            hypercall::PCI_CONFIG_WRITE => self.devices.config_write(data[1], data[2], data[3], data[4]),
            hypercall::PCI_MSI => match self.vcpus.lock().get(data[3] as usize) {
                Some(Some(vcpu)) => self.devices.msi(data[1], data[2], vcpu, data[4]),
                _ => error::INVALID,
            },
            hypercall::ISA_IRQ => match (&self.i8042, self.vcpus.lock().get(data[2] as usize)) {
                (Some(i8042), Some(Some(vcpu))) if i8042.route(data[1], vcpu, data[3]) => 0,
                _ => error::INVALID,
            },
            _ => error::UNKNOWN,
        }
    }

    /// Starts processor `id` in 64-bit mode at `rip`.
    fn start_cpu(self: &Arc<Self>, id: u64, rip: u64) -> u64 {
        let mut vcpus = self.vcpus.lock();
        let Some(slot) = vcpus.get_mut(id as usize).filter(|s| s.is_none()) else { return error::INVALID };
        let state = linux::long_mode(rip, PAGE_TABLES_AT, GDT_AT);
        let made = self.guest.create_vcpu(id as u32, &state).and_then(|v| Ok((v.0.duplicate(None)?, v)));
        let vcpu = match made {
            Ok((copy, v)) => {
                *slot = Some(Vcpu::from_handle(copy));
                v
            }
            Err(e) => {
                println!("no virtual processor {}: {}", id, e);
                return error::INVALID;
            }
        };
        drop(vcpus);
        match self.spawn(id as u32, vcpu) {
            Ok(()) => 0,
            Err(e) => {
                println!("{}", e);
                error::INVALID
            }
        }
    }

    /// Collects processor `id`'s console output into lines, for the log.
    fn console_write(&self, id: u32, count: u64, regs: [u64; 4]) {
        let (bytes, n) = platform::console_bytes(count, regs);
        let mut lines = self.console.lock();
        let Some(line) = lines.get_mut(id as usize) else { return };
        for &b in &bytes[..n] {
            match b {
                b'\n' => {
                    println!("linux: {}", String::from_utf8_lossy(line));
                    line.clear();
                }
                b'\r' => {}
                _ if line.len() < 1024 => line.push(b),
                _ => {}
            }
        }
    }

    /// The platform has no I/O ports but the keyboard controller's, if the
    /// guest has it: elsewhere reads find nothing, writes go nowhere. Each
    /// port is reported the first time.
    fn port(&self, exit: &mut VcpuExit) {
        let port = exit.data[0] as u16;
        if let Some(i8042) = &self.i8042
            && matches!(port, i8042::DATA | i8042::COMMAND)
            && exit.data[1] == 1
        {
            if exit.data[2] == 1 {
                i8042.write(port, exit.data[3] as u8);
            } else {
                exit.data[3] = i8042.read(port) as u64;
            }
            return;
        }
        let mut seen = self.ports.lock();
        if !seen.contains(&port) && seen.len() < 32 {
            seen.push(port);
            println!("Linux touched I/O port {:#x}, which does not exist", port);
        }
        exit.data[3] = u64::MAX;
    }

    /// Ends the machine (and this process).
    fn stop(&self, why: &str, code: i32) -> ! {
        for line in self.console.lock().iter().filter(|l| !l.is_empty()) {
            println!("linux: {}", String::from_utf8_lossy(line));
        }
        println!("{}", why);
        vrt::sys::process_exit(code as i64)
    }
}
