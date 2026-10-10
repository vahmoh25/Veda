//! The paravirtual platform Veda's guests run on.
//!
//! A guest finds no emulated hardware: no PIC, PIT, I/O APIC, RTC, serial
//! port or firmware code. It has processors with an x2APIC whose timer runs
//! on the TSC (`TSC-deadline` mode included), memory, ACPI tables that
//! describe its devices, and the platform's hypercalls, which it finds
//! through `cpuid`:
//!
//! * leaf [`CPUID_SIGNATURE`]: the highest platform leaf in `eax`, and
//!   [`SIGNATURE`] in `ebx`, `ecx` and `edx`;
//! * leaf [`CPUID_PLATFORM`]: the platform's features in `eax` (none yet)
//!   and the number of processors in `ebx`, whose local APIC ids are 0 to
//!   that number less one;
//! * leaf [`CPUID_TIMING`]: the TSC's frequency in `eax` and the local APIC
//!   timer's in `ebx`, in kHz.
//!
//! The TSC is the host's own (no offset, no scaling): invariant, and the
//! same on every processor.
//!
//! A hypercall is `vmcall` with the call's number in `rax` and its
//! arguments in `rbx`, `rcx`, `rdx`, `rsi` and `rdi`; the result comes back
//! in `rax`, negative for an error ([`error`]). The Linux guest's side of
//! all this is `arch/x86/kernel/cpu/veda.c`, `arch/x86/pci/veda.c` and
//! `arch/x86/include/asm/veda_para.h` in the Linux port (`ports/linux`).
//!
//! The ACPI tables (hardware-reduced ACPI: no fixed hardware, no SCI, no
//! MADT) are where the boot parameters say (`acpi_rsdp_addr`). They
//! describe the PCI root and its functions, the devices of the PC that the
//! guest has (its keyboard controller), and the interrupt lines those use:
//! the PC's global system interrupts (GSIs), which the guest routes to its
//! processors with [`hypercall::GSI`]. GPIO pins of the PC's that the
//! guest's devices are wired to are on GPIO controllers of the platform's
//! (`VEDA0001`), which [`hypercall::GPIO`] drives, by the PC's numbers for
//! the pins; their interrupts are lines too, GSIs from
//! [`PLATFORM_GSIS`] on.
//!
//! The PC's firmware variables that an operating system may read (UEFI's,
//! with runtime access, as they were when the PC started) the guest reads
//! with [`hypercall::FIRMWARE_VARIABLE`], which does what UEFI's
//! `GetVariable` and `GetNextVariableName` do; it cannot change them.
//!
//! PCI functions given to the guest are on its PCI segment 0, where the
//! platform puts them (bus 0); the guest finds them by reading their
//! configuration space, which it reaches only through hypercalls. Their
//! memory BARs hold the addresses the platform mapped them at, which stay.
//! Their MSIs and MSI-X the guest routes with a hypercall that gives back
//! the message the function must send; a function's INTx, where the PC
//! wires one, is a GSI the root's `_PRT` names. Their DMA
//! reaches the guest's memory, at its guest-physical addresses, and
//! nothing else.

/// The leaf of the platform's signature.
pub const CPUID_SIGNATURE: u32 = 0x4000_0000;
/// The leaf of the platform's features and processors.
pub const CPUID_PLATFORM: u32 = 0x4000_0001;
/// The leaf of the clocks' frequencies.
pub const CPUID_TIMING: u32 = 0x4000_0010;
/// The highest platform leaf.
pub const CPUID_MAX: u32 = CPUID_TIMING;

/// What `ebx`, `ecx` and `edx` of [`CPUID_SIGNATURE`] spell.
pub const SIGNATURE: [u8; 12] = *b"VedaVedaVeda";

/// The platform's hypercalls (`rax`).
pub mod hypercall {
    /// Writes to the console: `rbx` holds the number of bytes (at most
    /// 32), `rcx`, `rdx`, `rsi` and `rdi` the bytes (the first in the low
    /// byte of `rcx`). Registers rather than memory, so that the guest can
    /// write at any moment of its life, however early or broken.
    pub const CONSOLE_WRITE: u64 = 1;
    /// Starts processor `rbx` (its local APIC id) at `rcx`, in 64-bit mode
    /// with interrupts off, on page tables that map the first 4 GiB of
    /// guest-physical memory as they are. A processor starts once.
    pub const START_CPU: u64 = 2;
    /// Powers the machine off (`rbx` = [`super::power::OFF`]), restarts it
    /// ([`super::power::RESTART`]) or reports that the guest crashed
    /// ([`super::power::CRASHED`]). Does not return.
    pub const POWER: u64 = 3;
    /// The time of day: nanoseconds since 1970 (UTC).
    pub const WALLCLOCK: u64 = 4;
    /// An operation of the bridge ([`crate::bridge::HYPERCALL`]).
    pub const BRIDGE: u64 = crate::bridge::HYPERCALL;
    /// Reads `rdx` bytes (1, 2 or 4) at offset `rcx` of the configuration
    /// space of PCI function `rbx` (bus << 8 | device << 3 | function):
    /// the value, all ones where there is nothing.
    pub const PCI_CONFIG_READ: u64 = 6;
    /// Writes `rsi` (`rdx` bytes) at offset `rcx` of the configuration
    /// space of PCI function `rbx`.
    pub const PCI_CONFIG_WRITE: u64 = 7;
    /// Routes MSI `rcx` of PCI function `rbx` (an MSI-X entry, or one of
    /// MSI's vectors) to vector `rsi` of the processor with APIC id `rdx`,
    /// and gives the message the function must send for it: the address
    /// (its low 32 bits; the high ones are zero) in the low half, the data
    /// in the high half. Routing the same MSI again moves it; the message
    /// stays the same.
    pub const PCI_MSI: u64 = 8;
    /// Routes GSI `rbx` (below [`GSIS`]; one the ACPI tables name) to
    /// vector `rdx` of the processor with APIC id `rcx`, or to nothing
    /// (vector 0), which masks the line. Routing one again moves it. An
    /// edge needs no end-of-interrupt; a level-triggered line is masked
    /// when it fires until the processor's end-of-interrupt of its vector
    /// (as with an I/O APIC). An edge that came while the line was masked
    /// is raised when it is routed again.
    pub const GSI: u64 = 9;
    /// Operation `rdx` ([`super::gpio`]) on pin `rcx` (the PC's number, as
    /// the ACPI tables give it) of the platform's GPIO controller `rbx`
    /// (its `_UID`), with `rsi` for a level. Returns the level, whether the
    /// pin is an output, or 0; [`super::error::INVALID`] if the guest has
    /// no such pin, or the PC would not do it.
    pub const GPIO: u64 = 10;
    /// Operation `rbx` ([`super::variable`]) on the PC's firmware
    /// variables, with the [`super::Variable`] request at guest-physical
    /// address `rcx`. Returns 0; [`super::error::NOT_FOUND`] if there is no
    /// such variable (or none after it); [`super::error::TOO_SMALL`] if a
    /// buffer is too small for what goes there (the request then holds the
    /// size it needs); [`super::error::INVALID`] if the request is not in
    /// the guest's memory, or what it names is not a variable's name.
    pub const FIRMWARE_VARIABLE: u64 = 11;
}

/// What [`hypercall::FIRMWARE_VARIABLE`] does.
pub mod variable {
    /// UEFI's `GetVariable`: copies the data of the variable `name` (of
    /// vendor `guid`) to `data`, and sets `data_size` to its size and
    /// `attributes` to its attributes (with [`super::error::TOO_SMALL`]
    /// too).
    pub const GET: u64 = 0;
    /// UEFI's `GetNextVariableName`: replaces `name` and `guid` with those
    /// of the variable after them (the first after the empty name), and
    /// sets `name_size` to its name's size, the NUL included.
    pub const NEXT: u64 = 1;
}

/// A [`hypercall::FIRMWARE_VARIABLE`] request, in the guest's memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Variable {
    /// The variable's vendor (a GUID, as UEFI lays it out).
    pub guid: [u8; 16],
    /// Where its name is (guest-physical): UTF-16, NUL-terminated, in a
    /// buffer of `name_size` bytes.
    pub name: u64,
    pub name_size: u64,
    /// Where its data goes ([`variable::GET`]), a buffer of `data_size`
    /// bytes.
    pub data: u64,
    pub data_size: u64,
    /// Its attributes (UEFI's), from [`variable::GET`].
    pub attributes: u32,
    pub reserved: u32,
}

// The layout Linux's side (`asm/veda_para.h`) has.
const _: () = assert!(core::mem::size_of::<Variable>() == 56);

/// GSIs are below this: the guest's IRQs of the same numbers.
pub const GSIS: u32 = 512;
/// GSIs below this are the PC's own; from it, lines the platform makes
/// (the interrupts of GPIO pins).
pub const PLATFORM_GSIS: u32 = 256;

/// What [`hypercall::GPIO`] does with a pin.
pub mod gpio {
    /// The level: what the pin drives if it is an output, else what it
    /// reads (made an input if it is neither).
    pub const READ: u64 = 0;
    /// Drives `rsi`'s level (0 low, else high), making the pin an output.
    pub const WRITE: u64 = 1;
    /// Makes the pin an input.
    pub const INPUT: u64 = 2;
    /// 1 if the pin is an output, 0 if an input.
    pub const DIRECTION: u64 = 3;
}

/// The message of a [`hypercall::PCI_MSI`], as its result holds it.
pub fn msi_result(address: u64, data: u32) -> u64 {
    (address & 0xFFFF_FFFF) | ((data as u64) << 32)
}

/// What [`hypercall::POWER`] asks for.
pub mod power {
    pub const OFF: u64 = 0;
    pub const RESTART: u64 = 1;
    pub const CRASHED: u64 = 2;
}

/// Errors of hypercalls, as `rax` holds them.
pub mod error {
    /// No such hypercall.
    pub const UNKNOWN: u64 = -1i64 as u64;
    /// An argument was invalid.
    pub const INVALID: u64 = -2i64 as u64;
    /// There is no such thing.
    pub const NOT_FOUND: u64 = -3i64 as u64;
    /// A buffer is too small for what goes there.
    pub const TOO_SMALL: u64 = -4i64 as u64;
}

/// The bytes of a [`hypercall::CONSOLE_WRITE`], from its registers.
pub fn console_bytes(count: u64, regs: [u64; 4]) -> ([u8; 32], usize) {
    let mut out = [0u8; 32];
    for (i, r) in regs.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&r.to_le_bytes());
    }
    (out, count.min(32) as usize)
}

/// The registers of [`SIGNATURE`]: `ebx`, `ecx`, `edx`.
pub fn signature_registers() -> [u32; 3] {
    let w = |i: usize| u32::from_le_bytes([SIGNATURE[i], SIGNATURE[i + 1], SIGNATURE[i + 2], SIGNATURE[i + 3]]);
    [w(0), w(4), w(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_bytes_come_in_order() {
        let regs = [u64::from_le_bytes(*b"hello, w"), u64::from_le_bytes(*b"orld\n\0\0\0"), 0, 0];
        let (bytes, n) = console_bytes(13, regs);
        assert_eq!(&bytes[..n], b"hello, world\n");
        assert_eq!(console_bytes(99, regs).1, 32);
    }

    #[test]
    fn msi_results_hold_address_and_data() {
        assert_eq!(msi_result(0xFEE0_00B0, 0), 0xFEE0_00B0);
        assert_eq!(msi_result(0xFEE0_1000, 0x41), 0x41_FEE0_1000);
    }

    #[test]
    fn signature_spells_veda() {
        let [b, c, d] = signature_registers();
        let mut s = [0u8; 12];
        s[..4].copy_from_slice(&b.to_le_bytes());
        s[4..8].copy_from_slice(&c.to_le_bytes());
        s[8..].copy_from_slice(&d.to_le_bytes());
        assert_eq!(&s, b"VedaVedaVeda");
    }
}
