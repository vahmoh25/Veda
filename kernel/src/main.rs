#![no_std]
#![no_main]

use bootinfo::BootInfo;
use core::arch::asm;
use core::fmt::Write;

struct Serial;
impl Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            unsafe {
                while { let v: u8; asm!("in al, dx", out("al") v, in("dx") 0x3FDu16); v } & 0x20 == 0 {}
                asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
            }
        }
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn kernel_entry(info: &'static BootInfo) -> ! {
    let _ = writeln!(Serial, "vkernel: hello! bootinfo valid={} cmdline='{}'\r", info.is_valid(), info.cmdline());
    let mm = unsafe { info.memory_map.as_slice() };
    let usable: u64 = mm.iter().filter(|r| r.kind == bootinfo::MemoryKind::Usable).map(|r| r.pages * 4096).sum();
    let _ = writeln!(Serial, "vkernel: {} regions, {} MiB usable, fb {}x{}\r", mm.len(), usable >> 20, info.framebuffer.width, info.framebuffer.height);
    loop { unsafe { asm!("hlt") } }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
