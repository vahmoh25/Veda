//! The kernel's cryptographically secure random number generator, behind
//! the `random` system call.
//!
//! Output comes from a ChaCha20 stream whose key is replaced after every
//! request (`ventropy::Generator`), so earlier output cannot be
//! reconstructed from a later state. The key is derived with BLAKE2s from
//! an entropy pool that collects:
//!
//! * the seed the loader obtained from the firmware's `EFI_RNG_PROTOCOL`
//!   (backed by virtio-rng under QEMU),
//! * RDSEED and RDRAND output when the CPU has them,
//! * timing jitter measured at boot,
//! * the arrival times of device interrupts (folded in at every reseed).
//!
//! The generator is reseeded at boot, then every [`RESEED_INTERVAL_NS`] or
//! after [`RESEED_BYTES`] of output, whichever comes first.

use ventropy::{FastPool, Generator};

use crate::arch::cpu;
use crate::sync::SpinLock;
use crate::time;

/// Reseed at least this often...
const RESEED_INTERVAL_NS: u64 = 10_000_000_000;
/// ...and after this much output.
const RESEED_BYTES: u64 = 16 << 20;

struct State {
    generator: Generator,
    last_reseed_ns: u64,
    bytes_since_reseed: u64,
}

static RNG: SpinLock<State> =
    SpinLock::new(State { generator: Generator::new(), last_reseed_ns: 0, bytes_since_reseed: 0 });

/// Interrupt timestamps, cheap to update on every device interrupt.
static IRQ_POOL: SpinLock<FastPool> = SpinLock::new(FastPool::new());

/// Mixes up to 32 words of hardware randomness; returns how many were read.
fn mix_hardware(g: &mut Generator, read: fn() -> Option<u64>, words: usize) -> usize {
    let mut n = 0;
    for _ in 0..words {
        match read() {
            Some(v) => {
                g.mix(&v.to_le_bytes());
                n += 1;
            }
            None => break,
        }
    }
    n
}

/// Measures timing jitter: how long short busy loops take varies with
/// caches, interrupts in the host and emulation, and the low bits of the
/// differences are unpredictable.
fn mix_jitter(g: &mut Generator, samples: usize) {
    let mut acc = 0u64;
    let mut prev = cpu::rdtsc();
    let mut buf = [0u8; 64];
    let mut filled = 0;
    for i in 0..samples {
        // A data-dependent loop length makes the timing vary further.
        for _ in 0..(16 + (acc & 31)) {
            acc = acc.rotate_left(7) ^ cpu::rdtsc();
            core::hint::spin_loop();
        }
        let now = cpu::rdtsc();
        let delta = now.wrapping_sub(prev);
        prev = now;
        buf[filled..filled + 2].copy_from_slice(&((delta as u16) ^ (i as u16)).to_le_bytes());
        filled += 2;
        if filled == buf.len() {
            g.mix(&buf);
            filled = 0;
        }
    }
    g.mix(&buf[..filled]);
    g.mix(&acc.to_le_bytes());
}

/// Seeds the generator. Called once at boot, before user space runs.
pub fn init(firmware_seed: &[u8]) {
    let mut st = RNG.lock();
    let g = &mut st.generator;
    let mut sources: [&str; 4] = [""; 4];
    let mut count = 0;
    if !firmware_seed.is_empty() {
        g.mix(firmware_seed);
        sources[count] = "firmware RNG";
        count += 1;
    }
    if mix_hardware(g, cpu::rdseed, 8) > 0 {
        sources[count] = "RDSEED";
        count += 1;
    }
    if mix_hardware(g, cpu::rdrand, 8) > 0 {
        sources[count] = "RDRAND";
        count += 1;
    }
    mix_jitter(g, 256);
    g.mix(&time::now_ns().to_le_bytes());
    g.mix(&time::realtime_ns().to_le_bytes());
    sources[count] = "timing jitter";
    count += 1;
    g.reseed();
    st.last_reseed_ns = time::now_ns();
    drop(st);
    if count == 1 {
        crate::kwarn!("random: no hardware entropy source; seeded from timing jitter only");
    } else {
        crate::kinfo!("random: seeded from {}", sources[..count].join(", "));
    }
}

/// Records the arrival of a device interrupt (BKL held, interrupts off).
pub fn add_interrupt_timing(vector: u8) {
    IRQ_POOL.lock().add(cpu::rdtsc(), vector as u64);
}

/// Fills `out` with cryptographically secure random bytes.
pub fn fill(out: &mut [u8]) {
    let now = time::now_ns();
    let mut st = RNG.lock();
    if now.saturating_sub(st.last_reseed_ns) >= RESEED_INTERVAL_NS || st.bytes_since_reseed >= RESEED_BYTES {
        let irq = {
            let mut pool = IRQ_POOL.lock();
            let count = pool.count();
            (pool.drain(), count)
        };
        let g = &mut st.generator;
        g.mix(&irq.0);
        g.mix(&irq.1.to_le_bytes());
        mix_hardware(g, cpu::rdseed, 4);
        mix_hardware(g, cpu::rdrand, 4);
        g.mix(&cpu::rdtsc().to_le_bytes());
        g.reseed();
        st.last_reseed_ns = now;
        st.bytes_since_reseed = 0;
    }
    st.generator.fill(out);
    st.bytes_since_reseed += out.len() as u64;
}
