//! The GPU's engines, for the renderer: the `gem` service (`vgem`), which
//! Mesa's iris reaches through the POSIX layer's render node, on the render
//! and copy engines (`vigpu::render`).
//!
//! * **Bringing up**: forcewake for the GT and the render engine is taken
//!   and kept (Veda never lets the GT sleep: it enables no RC6), the
//!   engines are reset and set up as i915 does, and each runs its golden
//!   context. What fails is logged, with the engine's state, and the
//!   display goes on without the engines.
//! * **Memory**: contexts, rings and page tables are physically contiguous
//!   memory of the driver's, in a range of the global table set aside
//!   before the display's pictures were placed. Buffers are memory objects
//!   their client maps; the GPU reaches their pages through each address
//!   space's page tables. All of it is write-back: the GPU shares the
//!   processor's last-level cache, which keeps the two coherent.
//! * **Completion**: each submission writes its number into its engine's
//!   status page. The driver looks every millisecond while the GPU has
//!   work (the engines' interrupts stay off, so the display's are the only
//!   ones).
//! * **Clocks**: the GT runs at its highest frequency while it has work,
//!   and at its lowest once it has had none for a quarter of a second.

use alloc::vec;

use vigpu::Mmio;
use vigpu::gem::{EngineId, Gem, Hardware};
use vigpu::gt::{self, Frequencies, Topology};
use vigpu::gtregs;
use vigpu::render::{BufferMemory, Dma, Region, Render, Setup, Time};
use vproto::gem::{Engine, GemDevice, GemError};
use vrt::object::{Resource, Vmo};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vrt::vm::Mapping;

use crate::{Device, Registers};

/// Where the engines' room in the global table is looked for (clear of
/// what the display scans out), its size and alignment: their status
/// pages, and every context's image and ring.
pub const ROOM_FROM: u64 = 1 << 30;
pub const ROOM_BYTES: u64 = 256 << 20;
pub const ROOM_ALIGN: u64 = 2 << 20;
/// How often the engines are looked at while they have work.
const POLL_NS: u64 = 1_000_000;
/// How long the GT stays fast after its last work.
const IDLE_NS: u64 = 250_000_000;
/// The timestamp clock where the GT does not say (Gfx12's crystal).
const TIMESTAMP_HZ: u64 = 19_200_000;
/// The engines, by the device's index of them.
const ENGINE_NAMES: [&str; 2] = ["render", "copy"];

type Engines = Render<&'static Registers, Contiguous>;

/// Physically contiguous memory: the engines' own.
struct Contiguous(&'static Resource);

/// A block of it, mapped for the driver.
struct Block {
    map: Mapping,
    phys: u64,
}

impl Region for Block {
    fn phys(&self) -> u64 {
        self.phys
    }

    fn words(&mut self) -> &mut [u32] {
        // SAFETY: the mapping's memory, page-aligned, the driver's for as
        // long as the block lives (what the GPU does to it is the
        // hardware's, and read volatile where it matters).
        unsafe { core::slice::from_raw_parts_mut(self.map.as_ptr() as *mut u32, self.map.len() / 4) }
    }
}

impl Dma for Contiguous {
    type Region = Block;
    /// A buffer's pages are kept by the driver's handle of its memory.
    type Buffer = Vmo;

    fn alloc(&mut self, pages: u32) -> Option<Block> {
        let bytes = pages as usize * 4096;
        let vmo = Vmo::create_contiguous(self.0, bytes).ok()?;
        let phys = vmo.phys_addr(0).ok()?;
        let map = Mapping::new(vmo, bytes, vabi::map_flags::READ | vabi::map_flags::WRITE).ok()?;
        Some(Block { map, phys })
    }
}

/// The system's clock, and sleeping.
struct SystemTime;

impl Time for SystemTime {
    fn now(&self) -> u64 {
        now_ns()
    }

    fn delay(&self, ns: u64) {
        vrt::time::sleep(Duration::from_nanos(ns));
    }
}

/// The GT's frequency: the highest while it has work, the lowest once it
/// has had none for a while ([`IDLE_NS`]).
struct Clocks {
    range: Frequencies,
    fast: bool,
    /// Since when it has had no work, while fast.
    idle_since: Option<u64>,
    /// Whether what it ran at was logged (the first time it slowed down).
    told: bool,
}

impl Clocks {
    fn work(&mut self, regs: &Registers) {
        self.idle_since = None;
        if !self.fast {
            gt::request_frequency(regs, self.range.max);
            self.fast = true;
        }
    }

    fn idle(&mut self, regs: &Registers, now: u64) {
        if !self.fast {
            return;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.saturating_sub(since) < IDLE_NS {
            return;
        }
        if !self.told {
            // What the power unit granted: the one sign the requests work.
            println!(
                "GT: ran at {} MHz (asked for {}); {} MHz while idle",
                Frequencies::mhz(gt::current_frequency(regs)),
                Frequencies::mhz(self.range.max),
                Frequencies::mhz(self.range.min)
            );
            self.told = true;
        }
        gt::request_frequency(regs, self.range.min);
        self.fast = false;
        self.idle_since = None;
    }

    /// When [`idle`](Self::idle) next has something to do.
    fn deadline(&self) -> Option<u64> {
        self.idle_since.map(|t| t + IDLE_NS)
    }
}

/// The render engine's timestamp counter (its two halves read as i915
/// does, the upper again in case the lower carried into it).
fn timestamp(regs: &Registers) -> u64 {
    let at = gtregs::ring_timestamp(gtregs::RENDER_BASE);
    let mut high = regs.read(at + 4);
    for _ in 0..2 {
        let low = regs.read(at);
        let again = regs.read(at + 4);
        if again == high {
            return u64::from(high) << 32 | u64::from(low);
        }
        high = again;
    }
    u64::from(high) << 32 | u64::from(regs.read(at))
}

/// The GPU's engines as the `gem` service drives them.
struct Gt {
    dev: &'static Device,
    clocks: Clocks,
}

impl vgem::Driver for Gt {
    type Hardware = Engines;

    fn buffer(&mut self, size: u64) -> Result<(BufferMemory<Vmo>, Vmo), GemError> {
        let vmo = Vmo::create(size as usize).map_err(|_| GemError::NoMemory)?;
        // Every page now: the GPU may reach any of them.
        let mut pages = vec![0u64; (size / 4096) as usize];
        vmo.pages(&self.dev.dma, 0, &mut pages).map_err(|_| GemError::NoMemory)?;
        let theirs = vgem::client_handle(&vmo)?;
        Ok((BufferMemory { pages, keep: vmo, uncached: false }, theirs))
    }

    fn import(&mut self, memory: Vmo) -> Result<(BufferMemory<Vmo>, u64), GemError> {
        // A display's picture (the display's half of this driver made it):
        // its pages, kept while the GPU may use them, reached uncached.
        let size = memory.size().map_err(|_| GemError::Invalid)? as u64;
        let mut pages = vec![0u64; (size / 4096) as usize];
        memory.pages(&self.dev.dma, 0, &mut pages).map_err(|_| GemError::Invalid)?;
        Ok((BufferMemory { pages, keep: memory, uncached: true }, size))
    }

    fn timestamp(&mut self) -> u64 {
        timestamp(self.dev.regs)
    }

    fn submitting(&mut self) {
        // Fast before it runs.
        self.clocks.work(self.dev.regs);
    }

    fn look(&mut self, gem: &mut Gem<Engines>, now: u64) {
        let events = gem.hardware_mut().poll(now);
        for (e, state) in &events.hangs {
            let name = ENGINE_NAMES.get(*e).copied().unwrap_or("?");
            println!("GT: the {} engine hung and was reset (its context is lost): {}", name, state);
        }
        if gem.hardware().busy() {
            self.clocks.work(self.dev.regs);
        } else {
            self.clocks.idle(self.dev.regs, now);
        }
    }

    fn deadline(&self, gem: &Gem<Engines>, now: u64) -> u64 {
        if gem.hardware().pending() {
            return now + POLL_NS;
        }
        self.clocks.deadline().unwrap_or(vabi::DEADLINE_INFINITE)
    }
}

/// What the GPU is, as the render node tells Mesa.
fn describe(dev: &Device, topology: &Topology, timestamp_hz: u64, engines: &[EngineId]) -> GemDevice {
    let memory = vrt::object::system_info().map_or(4 << 30, |i| i.total_memory);
    GemDevice {
        vendor: dev.info.vendor,
        device: dev.info.device,
        subvendor: dev.subsystem.0,
        subdevice: dev.subsystem.1,
        revision: dev.info.revision,
        domain: 0,
        bus: dev.info.bus,
        dev: dev.info.slot,
        func: dev.info.function,
        slice_mask: topology.slice_mask,
        subslice_masks: vec![topology.dss_mask],
        eu_mask: topology.eu_mask,
        timestamp_hz,
        vm_size: vigpu::ppgtt::SIZE,
        memory,
        engines: engines.iter().map(|e| Engine { class: e.class, instance: e.instance }).collect(),
        name: dev.name.clone(),
    }
}

/// Brings the engines up in the global table's range `room` (start,
/// bytes), and serves `gem` on them; returns only if they cannot be.
pub fn run(dev: &'static Device, room: (u64, u64)) {
    let regs = dev.regs;
    for d in [gtregs::FORCEWAKE_GT, gtregs::FORCEWAKE_RENDER] {
        gt::forcewake_reset(regs, d);
    }
    for d in [gtregs::FORCEWAKE_GT, gtregs::FORCEWAKE_RENDER] {
        if !gt::forcewake_get(regs, d) {
            println!("GT: the {} domain did not wake up (forcewake): no 3D", d.name);
            return;
        }
    }
    let topology = Topology::read(regs);
    let (timestamp_hz, said) = match gt::timestamp_hz(regs) {
        Some(hz) => (hz, ""),
        None => (TIMESTAMP_HZ, " (assumed)"),
    };
    let range = Frequencies::read(regs);
    let was = gt::current_frequency(regs);
    gt::manual_frequency(regs);
    println!(
        "GT: {} execution units in {} dual subslices, timestamps at {} kHz{}, {} to {} MHz (at {} now); \
         contexts at {:#x}",
        topology.eus(),
        topology.dss_mask.count_ones(),
        timestamp_hz / 1000,
        said,
        Frequencies::mhz(range.min),
        Frequencies::mhz(range.max),
        Frequencies::mhz(was),
        room.0
    );
    let setup = Setup { gtt: dev.gtt, ggtt_start: room.0, ggtt_bytes: room.1, topology, interrupts: false };
    let started = now_ns();
    let render = match Render::new(regs, Contiguous(&dev.dma), setup, &SystemTime) {
        Ok(r) => r,
        Err(e) => {
            println!("GT: not brought up: {}; no 3D", e);
            return;
        }
    };
    println!(
        "GT: the render and copy engines ran their golden contexts ({} ms to bring up)",
        (now_ns() - started) / 1_000_000
    );
    let device = describe(dev, &topology, timestamp_hz, render.engines());
    let clocks = Clocks { range, fast: false, idle_since: None, told: false };
    vgem::serve(device, render, Gt { dev, clocks });
}
