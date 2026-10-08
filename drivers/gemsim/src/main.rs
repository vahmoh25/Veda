//! `gemsim` — a test stand-in: an Intel GPU that runs nothing.
//!
//! It serves the GEM protocol (`vproto::gem`) as `intel-gpu` does for the
//! GPU, with the same service (`vgem`), as the Iris Xe of Alder Lake-P
//! (8086:46A6, the test laptop's): buffers are memory, address spaces and
//! contexts are kept (`vigpu::gem`), and every submission completes at
//! once without running. So the renderer's iris starts, compiles shaders
//! and builds its batches in QEMU, which has no such GPU; what it draws is
//! never drawn.
//!
//! Started on request (`run=gemsim`), with the renderer
//! (`run=renderer:iris`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;

use vigpu::gem::{EngineId, Gem, GemError as ModelError, Hardware};
use vproto::gem::{Engine, GemDevice, GemError, class};
use vrt::object::Vmo;

vrt::entry!(main);

const ENGINES: [EngineId; 2] =
    [EngineId { class: class::RENDER, instance: 0 }, EngineId { class: class::COPY, instance: 0 }];
/// The command streamers' timestamp clock (Gfx12: 19.2 MHz).
const TIMESTAMP_HZ: u64 = 19_200_000;

/// A buffer's memory.
struct Memory {
    _vmo: Vmo,
    size: u64,
}

/// A GPU whose engines are done with whatever they are given.
struct Nothing {
    done: [u64; 2],
}

impl Hardware for Nothing {
    type Memory = Memory;
    type Space = ();
    /// A context is its engine, nothing more.
    type Context = usize;

    fn engines(&self) -> &[EngineId] {
        &ENGINES
    }

    fn space_size(&self) -> u64 {
        1 << 48
    }

    fn size(&self, m: &Memory) -> u64 {
        m.size
    }

    fn new_space(&mut self) -> Result<(), ModelError> {
        Ok(())
    }

    fn drop_space(&mut self, _: ()) {}

    fn bind(&mut self, _: &mut (), _: u64, _: &Memory) -> Result<(), ModelError> {
        Ok(())
    }

    fn unbind(&mut self, _: &mut (), _: u64, _: u64) {}

    fn new_context(&mut self, engine: usize, _: &()) -> Result<usize, ModelError> {
        Ok(engine)
    }

    fn drop_context(&mut self, _: usize) {}

    fn run(&mut self, engine: &mut usize, _: &(), _: u64, _: u32, seqno: u64) {
        self.done[*engine] = seqno;
    }

    fn completed(&self, engine: usize) -> u64 {
        self.done[engine]
    }
}

/// What it says it is.
struct Sim;

impl vgem::Driver for Sim {
    type Hardware = Nothing;

    fn buffer(&mut self, size: u64) -> Result<(Memory, Vmo), GemError> {
        let vmo = Vmo::create(size as usize).map_err(|_| GemError::NoMemory)?;
        let theirs = vgem::client_handle(&vmo)?;
        Ok((Memory { _vmo: vmo, size }, theirs))
    }

    fn import(&mut self, memory: Vmo) -> Result<(Memory, u64), GemError> {
        let size = memory.size().map_err(|_| GemError::Invalid)? as u64;
        Ok((Memory { _vmo: memory, size }, size))
    }

    fn timestamp(&mut self) -> u64 {
        // The clock's ticks since boot.
        (u128::from(vrt::time::now_ns()) * u128::from(TIMESTAMP_HZ) / 1_000_000_000) as u64
    }

    fn look(&mut self, _: &mut Gem<Nothing>, _: u64) {}

    fn deadline(&self, _: &Gem<Nothing>, _: u64) -> u64 {
        // Everything is done the moment it is submitted.
        vabi::DEADLINE_INFINITE
    }
}

fn device() -> GemDevice {
    GemDevice {
        vendor: 0x8086,
        device: 0x46A6,
        subvendor: 0x8086,
        subdevice: 0x46A6,
        revision: 0x0C,
        domain: 0,
        bus: 0,
        dev: 2,
        func: 0,
        // One slice of six dual subslices, sixteen units each: 96.
        slice_mask: 1,
        subslice_masks: vec![0x3F],
        eu_mask: 0xFFFF,
        timestamp_hz: TIMESTAMP_HZ,
        vm_size: 1 << 48,
        memory: 4 << 30,
        engines: ENGINES.iter().map(|e| Engine { class: e.class, instance: e.instance }).collect(),
        name: String::from("Intel Alder Lake-P (gemsim: runs nothing)"),
    }
}

fn main() -> i32 {
    vgem::serve(device(), Nothing { done: [0; 2] }, Sim);
    1
}
