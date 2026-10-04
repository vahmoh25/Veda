//! AC'97 driver: plays the audio service's mixed output and records the
//! microphone on an Intel 82801AA-compatible controller — VirtualBox's
//! "ICH AC97" and QEMU's `AC97`.
//!
//! * **The controller.** Two I/O BARs: the codec's mixer (NAM) and the bus
//!   master (NABM), whose PCM-out and PCM-in engines each walk a ring of 32
//!   buffer descriptors. After a cold reset and the codec's ready bit, the
//!   output is unmuted at full volume, the microphone becomes the recording
//!   source, and both converters run at 48 kHz (set explicitly where the
//!   codec has variable rate audio).
//! * **Playback.** Ten-millisecond buffers stay queued a few ahead of the
//!   one playing. The driver polls the engine's current index, counts the
//!   finished buffers into the ring's played position and refills them
//!   from the ring, padding with silence so the engine never runs dry while
//!   audio flows (a starvation while the service reports active streams
//!   counts as an underrun and deepens the queue). After a while without
//!   audio the engine stops; it starts again, from a reset, when audio
//!   arrives.
//! * **Recording.** While the audio service wants audio (the input ring's
//!   CAPTURE flag) the PCM-in engine fills its ring of buffers; the driver
//!   copies each finished one into the input ring, stamped with the capture
//!   clock, and hands it back to the engine.
//! * **No interrupts.** The 82801AA has no MSI; while an engine runs the
//!   driver polls every few milliseconds, otherwise it sleeps until the
//!   audio service has data or wants the microphone.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev, ring::flags};
use vproto::pci::pcidev;
use vrt::object::{Channel, Event, IoPorts};
use vrt::println;
use vrt::time::{Duration, now_ns};
use vvirtio::DmaBuffer;

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;

/// Buffer descriptors per engine (fixed by the hardware).
const ENTRIES: usize = 32;
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// One buffer: 10 ms.
const PERIOD_FRAMES: usize = (RATE / 100) as usize;
const PERIOD_SAMPLES: usize = PERIOD_FRAMES * CHANNELS;
const PERIOD_BYTES: usize = PERIOD_SAMPLES * 2;
/// The descriptor list comes first in an engine's DMA memory.
const BDL_BYTES: usize = ENTRIES * 8;
/// Buffers kept queued normally (60 ms), and at most after underruns.
const DEPTH: usize = 6;
const MAX_DEPTH: usize = 16;
/// Below this many queued buffers the driver pads with silence.
const MIN_QUEUED: usize = 2;
/// Buffers of audio needed before (re)starting playback.
const PREFILL: u32 = 2;
/// Stop the output engine after this long without audio.
const IDLE_STOP_NS: u64 = 1_500_000_000;
/// How often a running engine is looked at.
const POLL_NS: u64 = 4_000_000;

/// Mixer registers (NAM, 16-bit).
mod nam {
    pub const RESET: u16 = 0x00;
    pub const MASTER: u16 = 0x02;
    pub const HEADPHONE: u16 = 0x04;
    pub const MIC: u16 = 0x0E;
    pub const PCM_OUT: u16 = 0x18;
    pub const REC_SELECT: u16 = 0x1A;
    pub const REC_GAIN: u16 = 0x1C;
    pub const POWERDOWN: u16 = 0x26;
    pub const EXT_ID: u16 = 0x28;
    pub const EXT_CTRL: u16 = 0x2A;
    pub const DAC_RATE: u16 = 0x2C;
    pub const ADC_RATE: u16 = 0x32;
    /// Variable rate audio (EXT_ID, EXT_CTRL).
    pub const VRA: u16 = 1;
}

/// Bus master registers (NABM).
mod nabm {
    /// The engines' register blocks.
    pub const PCM_IN: u16 = 0x00;
    pub const PCM_OUT: u16 = 0x10;
    pub const GLOB_CNT: u16 = 0x2C;
    pub const GLOB_STA: u16 = 0x30;
    /// Within an engine's block.
    pub const BDBAR: u16 = 0x00;
    pub const CIV: u16 = 0x04;
    pub const LVI: u16 = 0x05;
    pub const SR: u16 = 0x06;
    pub const CR: u16 = 0x0B;
    /// Status: halted; the last valid buffer completed, a buffer
    /// completed, FIFO error (the last three are cleared by writing 1).
    pub const SR_DCH: u16 = 0x01;
    pub const SR_CLEAR: u16 = 0x1C;
    /// Control: run, reset the engine's registers.
    pub const CR_RPBM: u8 = 0x01;
    pub const CR_RR: u8 = 0x02;
    /// Global control: release the AC-link from cold reset.
    pub const GLOB_COLD: u32 = 0x02;
    /// Global status: the primary codec is ready.
    pub const GLOB_PCR: u32 = 0x100;
}

/// One bus master engine (PCM out or PCM in).
struct Engine {
    io: IoPorts,
    base: u16,
}

impl Engine {
    /// Stops the engine and resets its registers (indices back to 0).
    fn reset(&self) {
        self.io.out8(self.base + nabm::CR, 0);
        self.io.out8(self.base + nabm::CR, nabm::CR_RR);
        let deadline = now_ns() + 10_000_000;
        while self.io.in8(self.base + nabm::CR) & nabm::CR_RR != 0 && now_ns() < deadline {}
        self.io.out16(self.base + nabm::SR, nabm::SR_CLEAR);
    }

    fn set_list(&self, phys: u32) {
        self.io.out32(self.base + nabm::BDBAR, phys);
    }

    /// The buffer being processed.
    fn current(&self) -> usize {
        (self.io.in8(self.base + nabm::CIV) as usize) % ENTRIES
    }

    /// The last buffer the engine may process.
    fn set_last(&self, i: usize) {
        self.io.out8(self.base + nabm::LVI, i as u8);
    }

    fn halted(&self) -> bool {
        self.io.in16(self.base + nabm::SR) & nabm::SR_DCH != 0
    }

    fn run(&self) {
        self.io.out16(self.base + nabm::SR, nabm::SR_CLEAR);
        self.io.out8(self.base + nabm::CR, nabm::CR_RPBM);
    }
}

/// An engine's DMA memory: the descriptor list, then its 32 buffers.
struct Buffers {
    dma: DmaBuffer,
}

impl Buffers {
    fn new(dma: &vrt::object::Resource) -> Result<Buffers, &'static str> {
        let dma = DmaBuffer::new(dma, BDL_BYTES + ENTRIES * PERIOD_BYTES).map_err(|_| "out of DMA memory")?;
        // The controller addresses only the first 4 GiB.
        if dma.phys() + dma.len() as u64 > u32::MAX as u64 {
            return Err("DMA memory above 4 GiB");
        }
        let b = Buffers { dma };
        for i in 0..ENTRIES {
            let mut entry = [0u8; 8];
            entry[..4].copy_from_slice(&(b.buffer_phys(i) as u32).to_le_bytes());
            entry[4..6].copy_from_slice(&(PERIOD_SAMPLES as u16).to_le_bytes());
            b.dma.write(i * 8, &entry);
        }
        Ok(b)
    }

    fn list_phys(&self) -> u32 {
        self.dma.phys() as u32
    }

    fn buffer_phys(&self, i: usize) -> u64 {
        self.dma.phys() + (BDL_BYTES + i * PERIOD_BYTES) as u64
    }

    /// Fills buffer `i` with `samples`, then silence.
    fn fill(&self, i: usize, samples: &[i16]) {
        let base = BDL_BYTES + i * PERIOD_BYTES;
        // SAFETY: i16 is little-endian on x86; the slice is initialised.
        let bytes = unsafe { core::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 2) };
        let n = bytes.len().min(PERIOD_BYTES);
        self.dma.write(base, &bytes[..n]);
        if n < PERIOD_BYTES {
            // SAFETY: inside this buffer, which the engine is not reading.
            unsafe { core::ptr::write_bytes(self.dma.ptr().add(base + n), 0, PERIOD_BYTES - n) };
        }
    }

    /// Reads buffer `i` into `out` (PERIOD_SAMPLES samples).
    fn read(&self, i: usize, out: &mut [i16]) {
        // SAFETY: the engine has finished writing this buffer.
        let bytes = unsafe { self.dma.bytes(BDL_BYTES + i * PERIOD_BYTES, PERIOD_BYTES) };
        for (o, b) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *o = i16::from_le_bytes(*b);
        }
    }
}

struct Player {
    engine: Engine,
    buffers: Buffers,
    /// Frames taken from the ring into each buffer (the rest is silence).
    taken: [u32; ENTRIES],
    /// The next buffer to fill, and the oldest one still queued.
    next: usize,
    oldest: usize,
    queued: usize,
    running: bool,
    depth: usize,
    /// Ring frames whose buffers finished playing, and when that count
    /// last grew.
    played: u64,
    played_ns: u64,
    underruns: u32,
    starving: bool,
    last_audio_ns: u64,
    scratch: Vec<i16>,
}

impl Player {
    /// Queues one buffer: up to `frames` frames from the ring, padded with
    /// silence. Returns the frames taken.
    fn queue(&mut self, ring: &Ring, frames: u32) -> u32 {
        let want = (frames as usize).min(PERIOD_FRAMES);
        let n = ring.read(&mut self.scratch[..want * CHANNELS]);
        self.buffers.fill(self.next, &self.scratch[..n * CHANNELS]);
        self.taken[self.next] = n as u32;
        self.engine.set_last(self.next);
        self.next = (self.next + 1) % ENTRIES;
        self.queued += 1;
        if n > 0 {
            self.last_audio_ns = now_ns();
        }
        n as u32
    }

    /// Counts the buffers the engine finished. Returns true if any did.
    fn reap(&mut self) -> bool {
        if !self.running {
            return false;
        }
        // Everything before the current buffer has played; when the engine
        // halted, the current (last valid) one has too.
        let current = self.engine.current();
        let halted = self.engine.halted();
        let mut any = false;
        while self.queued > 0 && (self.oldest != current || halted) {
            if self.taken[self.oldest] > 0 {
                self.played += self.taken[self.oldest] as u64;
                self.played_ns = now_ns();
            }
            self.oldest = (self.oldest + 1) % ENTRIES;
            self.queued -= 1;
            any = true;
        }
        if halted {
            // Ran dry (or drained on purpose): start afresh next time.
            self.engine.reset();
            self.running = false;
            self.queued = 0;
        }
        any
    }

    fn start(&mut self, ring: &Ring) {
        self.engine.reset();
        self.engine.set_list(self.buffers.list_phys());
        self.next = 0;
        self.oldest = 0;
        self.queued = 0;
        while self.queued < self.depth && ring.filled() > 0 {
            let n = ring.filled().min(PERIOD_FRAMES as u32);
            self.queue(ring, n);
        }
        if self.queued == 0 {
            return;
        }
        self.engine.run();
        self.running = true;
        self.starving = false;
        self.last_audio_ns = now_ns();
    }

    /// Starts, refills or stops playback. Returns true if ring data was
    /// consumed or the state changed.
    fn pump(&mut self, ring: &Ring) -> bool {
        let active = ring.producer_flags() & flags::ACTIVE != 0;
        if !self.running {
            let filled = ring.filled();
            if filled == 0 || (active && filled < PERIOD_FRAMES as u32 * PREFILL) {
                return false;
            }
            self.start(ring);
            return true;
        }
        let idle = !active && ring.filled() == 0 && now_ns().saturating_sub(self.last_audio_ns) > IDLE_STOP_NS;
        if idle {
            // No refills: the engine plays what is queued and halts.
            return false;
        }
        let mut changed = false;
        while self.queued < self.depth {
            let filled = ring.filled();
            if filled >= PERIOD_FRAMES as u32 {
                self.queue(ring, PERIOD_FRAMES as u32);
                self.starving = false;
                changed = true;
                continue;
            }
            if self.queued >= MIN_QUEUED {
                break;
            }
            // About to run dry: queue what there is, and silence.
            if active && !self.starving {
                self.starving = true;
                self.underruns += 1;
                if self.depth < MAX_DEPTH {
                    self.depth += 1;
                }
                if self.underruns <= 10 || self.underruns.is_multiple_of(100) {
                    println!("underrun {} (queue depth now {})", self.underruns, self.depth);
                }
            }
            changed |= self.queue(ring, filled) > 0;
        }
        changed
    }

    fn publish(&self, ring: &Ring) {
        ring.set_played(self.played, self.played_ns);
        ring.set_latency((self.queued * PERIOD_FRAMES) as u32);
        ring.set_underruns(self.underruns);
        ring.set_consumer_flags(if self.running { flags::RUNNING } else { 0 });
    }

    fn halt(&mut self) {
        self.engine.reset();
        self.running = false;
        self.queued = 0;
    }
}

struct Recorder {
    engine: Engine,
    buffers: Buffers,
    /// The next buffer to take from the engine.
    next: usize,
    running: bool,
    overruns: u32,
    scratch: Vec<i16>,
}

impl Recorder {
    /// Records while the audio service wants audio. Returns true if the
    /// state changed.
    fn follow(&mut self, ring: &Ring) -> bool {
        let wanted = ring.consumer_flags() & flags::CAPTURE != 0;
        if wanted == self.running {
            return false;
        }
        if wanted {
            self.start();
            println!("recording started");
        } else {
            self.engine.reset();
            self.running = false;
            println!("recording stopped");
        }
        true
    }

    fn start(&mut self) {
        self.engine.reset();
        self.engine.set_list(self.buffers.list_phys());
        // Every buffer but the one before the first is the engine's.
        self.engine.set_last(ENTRIES - 1);
        self.next = 0;
        self.engine.run();
        self.running = true;
    }

    /// Takes the finished buffers into the ring. Returns true if frames
    /// were delivered.
    fn reap(&mut self, ring: &Ring, data_event: &Event) -> bool {
        if !self.running {
            return false;
        }
        let current = self.engine.current();
        let halted = self.engine.halted();
        // The buffers before the current one are full; an engine that
        // halted filled every buffer it had (a whole lap).
        let full = if halted { ENTRIES } else { (current + ENTRIES - self.next) % ENTRIES };
        for _ in 0..full {
            self.buffers.read(self.next, &mut self.scratch);
            let written = ring.write(&self.scratch);
            if written < PERIOD_FRAMES {
                ring.set_overruns(ring.overruns().saturating_add((PERIOD_FRAMES - written) as u32));
            }
            ring.set_capture_clock(ring.write_pos(), now_ns());
            self.next = (self.next + 1) % ENTRIES;
        }
        let delivered = full > 0;
        if halted {
            // The engine caught up with buffers not yet taken.
            self.overruns += 1;
            if self.overruns <= 5 {
                println!("recording overrun; restarting the input");
            }
            self.start();
        } else {
            // Hand back what was taken: the engine may fill up to the
            // buffer before the next one to take.
            self.engine.set_last((self.next + ENTRIES - 1) % ENTRIES);
        }
        if delivered {
            let _ = data_event.signal();
        }
        delivered
    }
}

/// The controller, set up.
struct Device {
    player: Player,
    recorder: Option<Recorder>,
    /// devmgr's channel for the device: open while the driver runs.
    _pci: pcidev::Client,
}

/// Waits for `cond`, up to `ms` milliseconds.
fn wait_for(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = now_ns() + ms * 1_000_000;
    while now_ns() < deadline {
        if cond() {
            return true;
        }
        vrt::time::sleep(Duration::from_millis(2));
    }
    cond()
}

fn setup() -> Result<Device, &'static str> {
    let h = vrt::env::take_handle(PCIDEV_ROLE).ok_or("no device")?;
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let info = pci.info().map_err(|_| "devmgr went away")?;
    let bar = |index: u8| info.bars.iter().find(|b| b.index == index && b.io).map(|b| b.address as u16);
    let (Some(nam_base), Some(nabm_base)) = (bar(0), bar(1)) else {
        return Err("the controller has no I/O BARs");
    };
    let ports = |index: u8| -> Result<IoPorts, &'static str> {
        pci.map_io_bar(index).map_err(|_| "devmgr went away")?.map_err(|_| "no access to the I/O ports")
    };
    let mixer = ports(0)?;
    let master = ports(1)?;
    pci.enable(true).map_err(|_| "devmgr went away")?.map_err(|_| "cannot enable the device")?;
    let dma = pci.dma_resource().map_err(|_| "devmgr went away")?.map_err(|_| "no DMA memory")?;

    // The AC-link out of cold reset, and the codec ready.
    master.out32(nabm_base + nabm::GLOB_CNT, nabm::GLOB_COLD);
    if !wait_for(1000, || master.in32(nabm_base + nabm::GLOB_STA) & nabm::GLOB_PCR != 0) {
        return Err("the codec did not become ready");
    }
    mixer.out16(nam_base + nam::RESET, 0);
    if !wait_for(200, || mixer.in16(nam_base + nam::POWERDOWN) & 0x0F == 0x0F) {
        println!("the codec's converters report not ready; going on");
    }
    // Output unmuted at full volume; the microphone records, unmuted.
    mixer.out16(nam_base + nam::MASTER, 0x0000);
    mixer.out16(nam_base + nam::HEADPHONE, 0x0000);
    mixer.out16(nam_base + nam::PCM_OUT, 0x0808);
    mixer.out16(nam_base + nam::MIC, 0x0008);
    mixer.out16(nam_base + nam::REC_SELECT, 0x0000);
    mixer.out16(nam_base + nam::REC_GAIN, 0x0000);
    let vra = mixer.in16(nam_base + nam::EXT_ID) & nam::VRA != 0;
    if vra {
        let ctrl = mixer.in16(nam_base + nam::EXT_CTRL);
        mixer.out16(nam_base + nam::EXT_CTRL, ctrl | nam::VRA);
        mixer.out16(nam_base + nam::DAC_RATE, RATE as u16);
        mixer.out16(nam_base + nam::ADC_RATE, RATE as u16);
        let (dac, adc) = (mixer.in16(nam_base + nam::DAC_RATE), mixer.in16(nam_base + nam::ADC_RATE));
        if dac as u32 != RATE || adc as u32 != RATE {
            println!("the codec runs at {} Hz out, {} Hz in rather than {} Hz", dac, adc, RATE);
        }
    }

    let out_ports = ports(1)?;
    let player = Player {
        engine: Engine { io: out_ports, base: nabm_base + nabm::PCM_OUT },
        buffers: Buffers::new(&dma)?,
        taken: [0; ENTRIES],
        next: 0,
        oldest: 0,
        queued: 0,
        running: false,
        depth: DEPTH,
        played: 0,
        played_ns: 0,
        underruns: 0,
        starving: false,
        last_audio_ns: 0,
        scratch: vec![0; PERIOD_SAMPLES],
    };
    player.engine.reset();
    let recorder = match Buffers::new(&dma) {
        Ok(buffers) => {
            let r = Recorder {
                engine: Engine { io: master, base: nabm_base + nabm::PCM_IN },
                buffers,
                next: 0,
                running: false,
                overruns: 0,
                scratch: vec![0; PERIOD_SAMPLES],
            };
            r.engine.reset();
            Some(r)
        }
        Err(e) => {
            println!("no recording: {}", e);
            None
        }
    };
    println!(
        "AC'97 at ports {:#x}/{:#x}: {} Hz stereo out{}, polled",
        nam_base,
        nabm_base,
        RATE,
        if recorder.is_some() { " and in" } else { "" }
    );
    Ok(Device { player, recorder, _pci: pci })
}

/// The input side of an attachment.
struct InputSide {
    ring: Ring,
    data_event: Event,
    wake_event: Event,
}

fn format(max_periods: u32) -> DeviceFormat {
    DeviceFormat {
        name: "ac97".into(),
        rate: RATE,
        channels: CHANNELS as u32,
        period_frames: PERIOD_FRAMES as u32,
        max_periods,
    }
}

/// Connects to the audio service and attaches the device (and its input).
fn attach(with_input: bool) -> Result<(Channel, Ring, Event, Event, Option<InputSide>), &'static str> {
    let ch = vproto::connect(audiodev::NAME).map_err(|_| "no registry")?;
    let client = audiodev::Client::new(ch);
    let link =
        client.attach(format(MAX_DEPTH as u32)).map_err(|_| "the audio service went away")?.map_err(|_| "refused")?;
    let ring = Ring::map(link.ring, Role::Consumer).map_err(|_| "bad ring")?;
    if ring.rate() != RATE || ring.channels() != CHANNELS as u32 {
        return Err("ring format does not match the device");
    }
    let input = if with_input {
        match client.attach_input(format(0)) {
            Ok(Ok(l)) => Ring::map(l.ring, Role::Producer).ok().map(|ring| InputSide {
                ring,
                data_event: l.data_event,
                wake_event: l.wake_event,
            }),
            Ok(Err(e)) => {
                println!("the audio service refused the input: {}", e);
                None
            }
            Err(_) => None,
        }
    } else {
        None
    };
    Ok((client.into_channel(), ring, link.data_event, link.space_event, input))
}

/// Plays (and records) until the audio service goes away.
fn serve(
    dev: &mut Device,
    link: &Channel,
    ring: &Ring,
    data_event: &Event,
    space_event: &Event,
    input: Option<&InputSide>,
) {
    loop {
        let recording = dev.recorder.as_ref().is_some_and(|r| r.running);
        let now = now_ns();
        let deadline = if dev.player.running || recording { now + POLL_NS } else { now + 500_000_000 };
        let fallback = data_event.raw();
        let mut items = [
            WaitItem { handle: link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
            WaitItem { handle: data_event.raw(), signals: signals::SIGNALED, ..Default::default() },
            WaitItem {
                handle: input.map(|i| i.wake_event.raw()).unwrap_or(fallback),
                signals: signals::SIGNALED,
                ..Default::default()
            },
        ];
        let _ = vrt::object::wait_many(&mut items, deadline);
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
        let _ = data_event.clear();
        let mut changed = dev.player.reap();
        changed |= dev.player.pump(ring);
        dev.player.publish(ring);
        if changed {
            let _ = space_event.signal();
        }
        if let (Some(rec), Some(input)) = (&mut dev.recorder, input) {
            let _ = input.wake_event.clear();
            rec.follow(&input.ring);
            rec.reap(&input.ring, &input.data_event);
        }
    }
}

/// Attaches to the audio service and plays until it goes away, forever.
fn run(mut dev: Device) {
    loop {
        match attach(dev.recorder.is_some()) {
            Ok((link, ring, data_event, space_event, input)) => {
                println!("attached to the audio service{}", if input.is_some() { " (with recording)" } else { "" });
                dev.player.played = ring.read_pos();
                dev.player.played_ns = 0;
                serve(&mut dev, &link, &ring, &data_event, &space_event, input.as_ref());
                println!("the audio service went away");
                dev.player.halt();
                if let Some(r) = &mut dev.recorder {
                    r.engine.reset();
                    r.running = false;
                }
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn main() -> i32 {
    let dev = match setup() {
        Ok(d) => d,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    // Refilling the device is time-critical: run above normal programs.
    let worker = vrt::thread::Builder::new().name("playback").priority(vabi::priority::HIGH).spawn(move || run(dev));
    match worker {
        Ok(handle) => {
            let _ = handle.join();
            0
        }
        Err(e) => {
            println!("cannot start the playback thread: {}", e);
            1
        }
    }
}
