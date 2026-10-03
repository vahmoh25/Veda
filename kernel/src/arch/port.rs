//! x86 I/O port access.

use core::arch::asm;

/// Reads a byte from an I/O port.
///
/// # Safety
/// Port I/O can have arbitrary side effects on devices.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: forwarded to the caller.
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    // SAFETY: forwarded to the caller.
    unsafe { asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    // SAFETY: forwarded to the caller.
    unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn outb(port: u16, v: u8) {
    // SAFETY: forwarded to the caller.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn outw(port: u16, v: u16) {
    // SAFETY: forwarded to the caller.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn outl(port: u16, v: u32) {
    // SAFETY: forwarded to the caller.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) };
}
