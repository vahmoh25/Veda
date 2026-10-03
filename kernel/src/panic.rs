//! Kernel panics and fatal CPU exceptions.
//!
//! A panic stops every CPU, prints a report to the serial console and paints
//! a "stop screen" on the framebuffer.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::entry::TrapFrame;
use crate::arch::{apic, cpu, idt, percpu};
use crate::log::emergency;

static PANICKING: AtomicBool = AtomicBool::new(false);

/// Framebuffer used for the stop screen (virtual address, geometry).
static FB_ADDR: AtomicU64 = AtomicU64::new(0);
static FB_GEOMETRY: AtomicU64 = AtomicU64::new(0); // width | height << 20 | stride << 40

pub fn set_framebuffer(fb: &bootinfo::Framebuffer) {
    if fb.phys_base == 0 {
        return;
    }
    FB_ADDR.store(crate::mm::phys_to_virt(fb.phys_base), Ordering::Relaxed);
    FB_GEOMETRY.store(fb.width as u64 | (fb.height as u64) << 20 | (fb.stride as u64) << 40, Ordering::Relaxed);
}

fn halt_other_cpus() {
    let me = percpu::cpu_id_or_boot();
    for p in percpu::online() {
        if p.cpu_id != me {
            apic::send_ipi(p.apic_id, idt::HALT_VECTOR as u32);
        }
    }
}

/// Paints the stop screen: a deep blue background with a white frowning
/// face, drawn without any font so it works in any state.
fn paint_stop_screen() {
    let base = FB_ADDR.load(Ordering::Relaxed);
    let g = FB_GEOMETRY.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }
    let (w, h, stride) = ((g & 0xFFFFF) as usize, ((g >> 20) & 0xFFFFF) as usize, (g >> 40) as usize);
    let px = base as *mut u32;
    let put = |x: usize, y: usize, c: u32| {
        if x < w && y < h {
            // SAFETY: inside the framebuffer.
            unsafe { px.add(y * stride + x).write_volatile(c) };
        }
    };
    for y in 0..h {
        for x in 0..w {
            put(x, y, 0x0010_3C8C);
        }
    }
    // ":(" â€” two eyes and a frown, scaled to the screen.
    let s = (h / 120).max(2);
    let (ox, oy) = (w / 10, h / 5);
    for dy in 0..6 * s {
        for dx in 0..6 * s {
            put(ox + dx, oy + dy, 0xFFFF_FFFF);
            put(ox + dx, oy + 14 * s + dy, 0xFFFF_FFFF);
        }
    }
    let (cx, cy, r) = (ox + 30 * s, oy + 22 * s, 16 * s);
    for a in 0..720 {
        // Upper half of a circle approximated without floats.
        let t = a as i64 - 360;
        let x = cx as i64 + (t * r as i64) / 360;
        let yy = r as i64 * r as i64 - (x - cx as i64) * (x - cx as i64);
        let mut ry = 0i64;
        while (ry + 1) * (ry + 1) <= yy {
            ry += 1;
        }
        for th in 0..(2 * s as i64) {
            put(x as usize, (cy as i64 - ry + th) as usize, 0xFFFF_FFFF);
        }
    }
}

fn enter_panic() -> bool {
    cpu::disable_interrupts();
    if PANICKING.swap(true, Ordering::SeqCst) {
        // Another CPU is already panicking.
        cpu::halt_forever();
    }
    halt_other_cpus();
    true
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    enter_panic();
    emergency(format_args!("\n\n*** VINDOWS KERNEL PANIC on CPU {} ***\n{}\n", percpu::cpu_id_or_boot(), info));
    if let Some(t) = crate::sched::try_current() {
        emergency(format_args!("current thread: {} of {}\n", t.koid, t.process_name()));
    }
    paint_stop_screen();
    cpu::halt_forever();
}

fn dump_frame(frame: &TrapFrame) {
    emergency(format_args!(
        "  rip {:#018x}  rsp {:#018x}  rflags {:#x}\n  cs {:#x}  ss {:#x}  error {:#x}\n",
        frame.rip, frame.rsp, frame.rflags, frame.cs, frame.ss, frame.error_code
    ));
    emergency(format_args!(
        "  rax {:#018x} rbx {:#018x} rcx {:#018x} rdx {:#018x}\n  rsi {:#018x} rdi {:#018x} rbp {:#018x}\n",
        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp
    ));
    emergency(format_args!(
        "  r8  {:#018x} r9  {:#018x} r10 {:#018x} r11 {:#018x}\n  r12 {:#018x} r13 {:#018x} r14 {:#018x} r15 {:#018x}\n",
        frame.r8, frame.r9, frame.r10, frame.r11, frame.r12, frame.r13, frame.r14, frame.r15
    ));
    // Walk the frame-pointer chain for a crude backtrace.
    emergency(format_args!("  backtrace:"));
    let mut rbp = frame.rbp;
    for _ in 0..16 {
        if rbp < 0xFFFF_8000_0000_0000 || rbp % 8 != 0 {
            break;
        }
        // SAFETY: best effort on a kernel stack; a bad chain only ends the walk
        // early because we check the address range first.
        let (next, ret) = unsafe { (*(rbp as *const u64), *((rbp + 8) as *const u64)) };
        emergency(format_args!(" {:#x}", ret));
        if next <= rbp {
            break;
        }
        rbp = next;
    }
    emergency(format_args!("\n"));
}

/// A CPU exception raised by kernel code.
pub fn kernel_exception(frame: &TrapFrame) -> ! {
    enter_panic();
    let v = frame.vector as usize;
    emergency(format_args!(
        "\n\n*** VINDOWS KERNEL PANIC: {} (vector {}) in kernel mode on CPU {} rip={:#x} cr2={:#x} rsp={:#x} err={:#x} ***\n",
        idt::EXCEPTION_NAMES.get(v).copied().unwrap_or("?"),
        v,
        percpu::cpu_id_or_boot(),
        frame.rip,
        cpu::read_cr2(),
        frame.rsp,
        frame.error_code
    ));
    if v == 14 {
        let addr = cpu::read_cr2();
        let what = if crate::mm::kvirt::is_stack_guard(addr) { " (kernel stack overflow?)" } else { "" };
        emergency(format_args!("  faulting address {:#x}{}\n", addr, what));
    }
    dump_frame(frame);
    paint_stop_screen();
    cpu::halt_forever();
}

/// Non-maskable interrupt: only expected as part of a panic.
pub fn nmi(frame: &TrapFrame) -> ! {
    if PANICKING.load(Ordering::SeqCst) {
        cpu::halt_forever();
    }
    enter_panic();
    emergency(format_args!("\n\n*** VINDOWS KERNEL PANIC: unexpected NMI ***\n"));
    dump_frame(frame);
    paint_stop_screen();
    cpu::halt_forever();
}
