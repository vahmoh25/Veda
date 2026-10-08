//! `vkernel` — the Veda microkernel.
//!
//! The kernel provides only what cannot safely live in user space:
//! scheduling, address spaces, capability-based kernel objects (processes,
//! threads, channels, events, VMOs, interrupts, I/O ports, resources) and
//! interrupt delivery. Drivers, file systems, the window system and all
//! applications are user-space processes that talk over channels.
//!
//! See `docs/ARCHITECTURE.md` for the overall design.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod futex;
mod loader;
mod log;
mod mm;
mod object;
mod panic;
mod random;
mod sched;
mod sync;
mod syscall;
mod time;

use bootinfo::{BootInfo, MemoryKind};

use arch::{apic, cpu, gdt, idt, percpu, smp};
use sync::bkl;

#[global_allocator]
static HEAP: mm::heap::KernelHeap = mm::heap::KernelHeap::new();

/// Options parsed from the kernel command line (`BOOT.CFG`).
struct Options {
    max_cpus: usize,
    /// `tz=+02:00`: local time's offset from UTC, in seconds.
    tz: Option<i64>,
}

fn parse_cmdline(cmdline: &str) -> Options {
    let mut o = Options { max_cpus: percpu::MAX_CPUS, tz: None };
    for arg in cmdline.split_whitespace() {
        match arg.split_once('=') {
            Some(("log", "debug")) => log::set_level(log::Level::Debug),
            Some(("log", "warn")) => log::set_level(log::Level::Warn),
            Some(("smp", "off")) | Some(("cpus", "1")) => o.max_cpus = 1,
            Some(("cpus", n)) => o.max_cpus = n.parse().unwrap_or(o.max_cpus).clamp(1, percpu::MAX_CPUS),
            Some(("tz", z)) => o.tz = time::parse_utc_offset(z),
            _ => {}
        }
    }
    o
}

/// Logs how the loader painted its splash: a real PC's framebuffer is often
/// uncached, which shows as the picture being painted from the top down,
/// until something makes it write-combining.
fn log_splash(s: &bootinfo::SplashReport) {
    use bootinfo::{made_wc, memory_type};
    if s.paint_ticks == 0 {
        return;
    }
    let ms = |ticks: u64| {
        let us = time::ticks_to_ns(ticks as i64) / 1000;
        alloc::format!("{}.{} ms", us / 1000, us % 1000 / 100)
    };
    let found = memory_type::name(s.found);
    let how = match s.made_wc {
        made_wc::ALREADY => alloc::format!("the framebuffer is {}", found),
        made_wc::PAGE_ATTRIBUTES => alloc::format!(
            "the firmware's framebuffer is {}, painted write-combining through the loader's page tables (made in {})",
            found,
            ms(s.setup_ticks)
        ),
        _ => alloc::format!("the framebuffer is {}, and could not be painted write-combining", found),
    };
    kinfo!("boot: the loader painted its splash in {}; {}", ms(s.paint_ticks), how);
}

/// Kernel entry point, called by `vboot` (see the `bootinfo` crate for the
/// machine state at this point).
#[unsafe(no_mangle)]
pub extern "sysv64" fn kernel_entry(boot: &'static BootInfo) -> ! {
    arch::serial::init();
    // SAFETY: per-CPU structures of the BSP, set up exactly once.
    unsafe {
        let p = percpu::get(0);
        gdt::load(p.gdt.get(), p.tss.get());
        percpu::install(0, 0);
    }
    idt::init();
    idt::load();
    if !boot.is_valid() {
        panic!("invalid boot information (loader/kernel version mismatch)");
    }
    let features = cpu::detect_features();
    let opts = parse_cmdline(boot.cmdline());
    kinfo!("Veda kernel {} starting", env!("CARGO_PKG_VERSION"));
    kinfo!(
        "cpu: xsave={} avx={} x2apic={} tsc-deadline={} invariant-tsc={} smep={} smap={} 1g-pages={}",
        features.xsave,
        features.avx,
        features.x2apic,
        features.tsc_deadline,
        features.invariant_tsc,
        features.smep,
        features.smap,
        features.page_1g
    );

    // Memory: frame allocator, kernel address space, heap.
    // SAFETY: the loader built a valid memory map.
    let memmap = unsafe { boot.memory_map.as_slice() };
    mm::phys::init(memmap);
    mm::paging::init_kernel_space(boot);
    let reclaimed = mm::phys::reclaim(memmap, MemoryKind::LoaderReclaimable);
    let (total, free) = mm::phys::stats();
    kinfo!(
        "memory: {} MiB usable, {} MiB free ({} KiB reclaimed from the loader)",
        total >> 20,
        free >> 20,
        reclaimed >> 10
    );
    smp::init_this_cpu(0);
    panic::set_framebuffer(&boot.framebuffer);

    // Platform: ACPI, interrupt controllers, timers.
    acpi::init(boot.rsdp_phys);
    let acpi = acpi::ACPI.expect();
    apic::disable_pic();
    apic::select_mode(acpi.lapic_phys);
    apic::init_local();
    // SAFETY: recording the BSP's APIC id before any IPI is sent.
    unsafe { percpu::set_apic_id(0, apic::id()) };
    for io in &acpi.ioapics {
        apic::add_ioapic(io.phys, io.gsi_base);
    }
    if let Some(hpet) = acpi.hpet_phys {
        time::set_hpet(hpet);
    }
    // The boot processor's TSC as all processors' will be (see `time`).
    let firmware_adjust = time::reset_boot_tsc();
    time::calibrate_tsc();
    if let Some(adjust) = firmware_adjust {
        kinfo!("time: the firmware's adjustment of CPU 0's TSC ({} ns) set to 0", time::ticks_to_ns(adjust));
    }
    log_splash(&boot.splash);
    time::set_boot_time(&boot.boot_time, opts.tz);
    if features.tsc_deadline {
        apic::use_tsc_deadline(true);
    } else {
        apic::calibrate_timer(&time::busy_wait_ms);
    }
    kinfo!(
        "time: TSC {} MHz (by the {}), {} timer, {} mode APIC",
        time::tsc_hz() / 1_000_000,
        time::tsc_source(),
        if features.tsc_deadline { "TSC-deadline" } else { "one-shot" },
        if apic::is_x2apic() { "x2APIC" } else { "xAPIC" }
    );
    // How fast the processor runs: rated, and measured while busy.
    let rated = cpu::rated_mhz()
        .map_or(alloc::string::String::new(), |(base, top)| alloc::format!(", rated {} MHz (up to {} MHz)", base, top));
    let measured =
        cpu::measure_mhz(20_000).map_or(alloc::string::String::from("unknown"), |mhz| alloc::format!("{} MHz", mhz));
    let pstates = match cpu::hwp_range() {
        Some((lowest, highest)) => alloc::format!("hardware P-states on (performance {} to {})", lowest, highest),
        None => alloc::string::String::from("no hardware P-states: the firmware's speed"),
    };
    kinfo!("cpu: running at {} while busy{}; {}", measured, rated, pstates);

    random::init(boot.entropy());

    // Scheduler on the BSP; this boot context becomes CPU 0's idle thread.
    sched::init_cpu(sched::Thread::new_idle_current(0));
    percpu::get(0).online.store(true, core::sync::atomic::Ordering::Release);

    let aps = smp::start_aps(opts.max_cpus);
    kinfo!("smp: {} CPU(s) online", aps + 1);

    bkl::acquire();
    loader::spawn_init(boot);
    kinfo!("boot complete; handing the CPU to user space");
    sched::idle_loop()
}
