//! virtio-snd driver: plays the audio service's mixed output on a virtio
//! sound device (QEMU's `virtio-sound-pci`).
//!
//! * **Set-up.** Reads the device configuration (jacks, streams, channel
//!   maps), queries the PCM streams, picks the first output stream that
//!   supports S16 and configures it — 48 kHz stereo preferred, otherwise the
//!   closest supported format — with `SET_PARAMS` and `PREPARE`.
//! * **Attach.** Connects to the `audiodev` service (provided by the audio
//!   service), announces the format and receives a shared ring of mixed
//!   frames plus two events.
//! * **Playback.** Up to `depth` period-sized transfers stay queued on the
//!   tx queue. A transfer is one descriptor chain: `virtio_snd_pcm_xfer`
//!   plus the PCM data (device-readable), then `virtio_snd_pcm_status`
//!   (device-writable). When a transfer completes (MSI-X interrupt, or
//!   polling without MSI-X) its slot is refilled from the ring. If the ring
//!   runs dry while fewer than two transfers are queued, silence is queued
//!   so the device never underruns; a starvation while the service reports
//!   active streams counts as an underrun and deepens the queue. After a
//!   while without any audio the stream is stopped, and it is started again
//!   (after prefilling) as soon as data arrives.
//! * **Recording.** If the device has an input stream (QEMU with
//!   `streams=2`), it is attached as the audio service's input device and
//!   records while the service wants audio (see [`capture`]).

#![no_std]
#![no_main]

extern crate alloc;

mod capture;
mod device;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use vabi::{WaitItem, signals};
use vproto::audio::{DeviceFormat, Ring, Role, audiodev, ring::flags};

use capture::Recorder;
use vproto::pci::pcidev;
use vrt::object::{Channel, Event, Interrupt};
use vrt::println;
use vrt::time::now_ns;
use vvirtio::{DmaBuffer, Segment, Virtqueue};

use device::{DriverError, PcmConfig, S_OK, SndDevice};

vrt::entry!(main);

const PCIDEV_ROLE: u32 = vabi::startup::role::USER + 10;
/// Transfer slots (each needs two descriptors of the 64-entry tx queue).
const SLOTS: usize = 16;
/// Transfers kept queued normally (about 64 ms at 48 kHz).
const DEPTH: usize = 6;
/// Deepest queue after repeated underruns.
const MAX_DEPTH: usize = 12;
/// Below this many queued transfers the driver pads with silence.
const MIN_QUEUED: usize = 2;
/// Periods of data needed before (re)starting the stream.
const PREFILL_PERIODS: u32 = 2;
/// Stop the stream after this long without audio.
const IDLE_STOP_NS: u64 = 1_500_000_000;
const EVENT_BUFFERS: usize = 8;
const EVENT_SIZE: usize = 8;

/// Playback state of the output stream.
struct Player {
    snd: SndDevice,
    cfg: PcmConfig,
    tx: Virtqueue,
    irq: Option<Interrupt>,
    dma: DmaBuffer,
    /// Bytes per slot; the PCM follows a 4-byte header, the status sits at
    /// `status_off`.
    stride: usize,
    status_off: usize,
    slot_of_head: Vec<usize>,
    free: Vec<usize>,
    /// Queued transfers: (slot, frames taken from the ring).
    inflight: VecDeque<(usize, u32)>,
    running: bool,
    depth: usize,
    /// Ring frames whose transfers completed.
    played: u64,
    underruns: u32,
    starving: bool,
    /// Last time real audio was queued.
    last_audio_ns: u64,
    status_errors: u32,
    scratch: Vec<i16>,
    events: Virtqueue,
    event_buf: DmaBuffer,
    event_slot_of_head: Vec<usize>,
    /// The input stream, if the device has one.
    rec: Option<Recorder>,
}

impl Player {
    fn post_event_buffer(&mut self, slot: usize) {
        let seg = Segment {
            phys: self.event_buf.phys() + (slot * EVENT_SIZE) as u64,
            len: EVENT_SIZE as u32,
            device_writes: true,
        };
        if let Some(h) = self.events.push(&[seg]) {
            self.event_slot_of_head[h as usize] = slot;
        }
    }

    /// Logs device events (jack changes, xruns) and reposts their buffers.
    fn poll_events(&mut self) {
        let mut any = false;
        while let Some((head, _)) = self.events.pop_used() {
            let slot = self.event_slot_of_head[head as usize];
            // SAFETY: the device finished writing this event.
            let e = unsafe { self.event_buf.bytes(slot * EVENT_SIZE, EVENT_SIZE) };
            let code = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
            let data = u32::from_le_bytes([e[4], e[5], e[6], e[7]]);
            match code {
                device::EVT_JACK_CONNECTED => println!("jack {} connected", data),
                device::EVT_JACK_DISCONNECTED => println!("jack {} disconnected", data),
                device::EVT_PCM_XRUN => println!("device reports an xrun on stream {}", data),
                device::EVT_PCM_PERIOD_ELAPSED => {}
                _ => println!("event {:#x} ({})", code, data),
            }
            self.post_event_buffer(slot);
            any = true;
        }
        if any {
            self.events.notify();
        }
    }

    fn queued_frames(&self) -> u32 {
        self.inflight.len() as u32 * self.cfg.period_frames
    }

    /// Collects completed transfers. Returns true if any completed.
    fn reap(&mut self) -> bool {
        let mut any = false;
        while let Some((head, _len)) = self.tx.pop_used() {
            let slot = self.slot_of_head[head as usize];
            if let Some(i) = self.inflight.iter().position(|&(s, _)| s == slot) {
                let (_, frames) = self.inflight.remove(i).unwrap_or((slot, 0));
                self.played += frames as u64;
            }
            // SAFETY: the device has written the status of this transfer.
            let st = unsafe { self.dma.bytes(slot * self.stride + self.status_off, 4) };
            let status = u32::from_le_bytes([st[0], st[1], st[2], st[3]]);
            if status != S_OK {
                self.status_errors += 1;
                if self.status_errors <= 5 {
                    println!("transfer failed with status {:#x}", status);
                }
            }
            self.free.push(slot);
            any = true;
        }
        any
    }

    /// Queues one period: `frames` frames from the ring, padded with
    /// silence. Returns the frames actually taken from the ring.
    fn queue(&mut self, ring: &Ring, frames: u32) -> u32 {
        let Some(slot) = self.free.pop() else { return 0 };
        let ch = self.cfg.channels as usize;
        let period = self.cfg.period_frames as usize;
        let want = (frames as usize).min(period);
        let n = ring.read(&mut self.scratch[..want * ch]);
        let base = slot * self.stride;
        self.dma.write(base, &self.cfg.stream.to_le_bytes());
        // SAFETY: `scratch` holds `n * ch` initialised samples; i16 is
        // little-endian on x86.
        let pcm = unsafe { core::slice::from_raw_parts(self.scratch.as_ptr() as *const u8, n * ch * 2) };
        self.dma.write(base + 4, pcm);
        if n < period {
            let start = base + 4 + n * ch * 2;
            let len = (period - n) * ch * 2;
            // SAFETY: inside this slot's PCM area, which the device is not
            // reading (the slot is free).
            unsafe { core::ptr::write_bytes(self.dma.ptr().add(start), 0, len) };
        }
        self.dma.write(base + self.status_off, &[0xFF; 8]);
        let phys = self.dma.phys() + base as u64;
        let chain = [
            Segment { phys, len: (4 + period * ch * 2) as u32, device_writes: false },
            Segment { phys: phys + self.status_off as u64, len: 8, device_writes: true },
        ];
        match self.tx.push(&chain) {
            Some(head) => {
                self.slot_of_head[head as usize] = slot;
                self.inflight.push_back((slot, n as u32));
            }
            None => {
                // Cannot happen with SLOTS * 2 <= queue size; keep the slot.
                self.free.push(slot);
            }
        }
        if n > 0 {
            self.last_audio_ns = now_ns();
        }
        n as u32
    }

    /// Starts, refills or stops the stream. Returns true if ring data was
    /// consumed or the state changed.
    fn pump(&mut self, ring: &Ring) -> bool {
        let period = self.cfg.period_frames;
        let active = ring.producer_flags() & flags::ACTIVE != 0;
        let mut changed = false;
        if !self.running {
            let filled = ring.filled();
            if filled == 0 || (active && filled < period * PREFILL_PERIODS) {
                return false;
            }
            while self.inflight.len() < self.depth && ring.filled() > 0 && !self.free.is_empty() {
                let n = ring.filled().min(period);
                self.queue(ring, n);
            }
            self.tx.notify();
            match self.snd.start(self.cfg.stream) {
                Ok(()) => {
                    self.running = true;
                    self.starving = false;
                    self.last_audio_ns = now_ns();
                }
                Err(e) => println!("cannot start the stream: {}", e),
            }
            return true;
        }
        let idle = !active && ring.filled() == 0 && now_ns().saturating_sub(self.last_audio_ns) > IDLE_STOP_NS;
        if idle {
            // Let the queue drain, then stop the stream.
            if self.inflight.is_empty() {
                if let Err(e) = self.snd.stop(self.cfg.stream) {
                    println!("cannot stop the stream: {}", e);
                }
                self.running = false;
                changed = true;
            }
            return changed;
        }
        let mut queued = false;
        while self.inflight.len() < self.depth && !self.free.is_empty() {
            let filled = ring.filled();
            if filled >= period {
                self.queue(ring, period);
                self.starving = false;
                queued = true;
                changed = true;
                continue;
            }
            if self.inflight.len() >= MIN_QUEUED {
                break;
            }
            // About to run dry: queue what there is plus silence.
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
            let took = self.queue(ring, filled);
            changed |= took > 0;
            queued = true;
        }
        if queued {
            self.tx.notify();
        }
        changed
    }

    /// Publishes our progress in the ring header.
    fn publish(&self, ring: &Ring) {
        ring.set_played(self.played, now_ns());
        ring.set_latency(self.queued_frames());
        ring.set_underruns(self.underruns);
        ring.set_consumer_flags(if self.running { flags::RUNNING } else { 0 });
    }

    /// Resets the stream when the audio service detaches: STOP, then RELEASE
    /// (the device completes every outstanding transfer), then PREPARE so
    /// that the stream is ready for the next attach.
    fn halt(&mut self) {
        if self.running {
            if let Err(e) = self.snd.stop(self.cfg.stream) {
                println!("cannot stop the stream: {}", e);
            }
            self.running = false;
        }
        if let Err(e) = self.snd.release(self.cfg.stream) {
            println!("cannot release the stream: {}", e);
        }
        let deadline = now_ns() + 200_000_000;
        while !self.inflight.is_empty() && now_ns() < deadline {
            self.reap();
            vrt::time::sleep(vrt::time::Duration::from_millis(2));
        }
        // Slots the device never returned are lost; start afresh with the
        // ones we have.
        self.inflight.clear();
        if let Err(e) = self.snd.set_params(&self.cfg).and_then(|_| self.snd.prepare(self.cfg.stream)) {
            println!("cannot prepare the stream again: {}", e);
        }
        if let Some(rec) = &mut self.rec {
            rec.halt(&mut self.snd);
        }
    }
}

/// The input side of an attachment.
struct InputSide {
    ring: Ring,
    data_event: Event,
    wake_event: Event,
}

/// Attaches the input stream on the same connection.
fn attach_input(client: &audiodev::Client, cfg: &PcmConfig) -> Option<InputSide> {
    let format = DeviceFormat {
        name: "virtio-snd".into(),
        rate: cfg.rate,
        channels: cfg.channels as u32,
        period_frames: cfg.period_frames,
        max_periods: 0,
    };
    match client.attach_input(format) {
        Ok(Ok(link)) => match Ring::map(link.ring, Role::Producer) {
            Ok(ring) => Some(InputSide { ring, data_event: link.data_event, wake_event: link.wake_event }),
            Err(_) => {
                println!("bad input ring");
                None
            }
        },
        Ok(Err(e)) => {
            println!("the audio service refused the input device: {}", e);
            None
        }
        Err(_) => None,
    }
}

/// Connects to the audio service and attaches the device.
fn attach(
    cfg: &PcmConfig,
    input: Option<&PcmConfig>,
) -> Result<(Channel, Ring, Event, Event, Option<InputSide>), &'static str> {
    let ch = vproto::connect(audiodev::NAME).map_err(|_| "no registry")?;
    let client = audiodev::Client::new(ch);
    let format = DeviceFormat {
        name: "virtio-snd".into(),
        rate: cfg.rate,
        channels: cfg.channels as u32,
        period_frames: cfg.period_frames,
        max_periods: MAX_DEPTH as u32,
    };
    let link = client.attach(format).map_err(|_| "the audio service went away")?.map_err(|_| "attach refused")?;
    let ring = Ring::map(link.ring, Role::Consumer).map_err(|_| "bad ring")?;
    if ring.rate() != cfg.rate || ring.channels() != cfg.channels as u32 {
        return Err("ring format does not match the device");
    }
    let input = input.and_then(|icfg| attach_input(&client, icfg));
    Ok((client.into_channel(), ring, link.data_event, link.space_event, input))
}

/// Plays from an attached ring (and records into the input ring) until
/// the audio service goes away.
fn serve(
    p: &mut Player,
    link: &Channel,
    ring: &Ring,
    data_event: &Event,
    space_event: &Event,
    input: Option<&InputSide>,
) {
    loop {
        let now = now_ns();
        let mut deadline = match (p.running, p.irq.is_some()) {
            (true, true) => now + 25_000_000,
            (true, false) => now + 2_000_000,
            (false, _) => now + 500_000_000,
        };
        let recording = p.rec.as_ref().is_some_and(|r| r.running);
        if recording {
            let polled = p.rec.as_ref().is_some_and(|r| r.irq.is_none());
            deadline = deadline.min(now + if polled { 5_000_000 } else { 25_000_000 });
        }
        let fallback = data_event.raw();
        let mut items = [
            WaitItem { handle: link.raw(), signals: signals::PEER_CLOSED, ..Default::default() },
            WaitItem { handle: data_event.raw(), signals: signals::SIGNALED, ..Default::default() },
            WaitItem {
                handle: p.irq.as_ref().map(|i| i.raw()).unwrap_or(fallback),
                signals: signals::SIGNALED,
                ..Default::default()
            },
            WaitItem {
                handle: input.map(|i| i.wake_event.raw()).unwrap_or(fallback),
                signals: signals::SIGNALED,
                ..Default::default()
            },
            WaitItem {
                handle: p.rec.as_ref().and_then(|r| r.irq.as_ref()).map(|i| i.raw()).unwrap_or(fallback),
                signals: signals::SIGNALED,
                ..Default::default()
            },
        ];
        let _ = vrt::object::wait_many(&mut items, deadline);
        if items[0].observed & signals::PEER_CLOSED != 0 {
            return;
        }
        if let Some(irq) = &p.irq
            && items[2].observed & signals::SIGNALED != 0
        {
            // Re-arm before draining so no completion is lost.
            let _ = irq.ack();
        }
        if let Some(irq) = p.rec.as_ref().and_then(|r| r.irq.as_ref())
            && items[4].observed & signals::SIGNALED != 0
        {
            let _ = irq.ack();
        }
        let _ = data_event.clear();
        let mut changed = p.reap();
        changed |= p.pump(ring);
        p.publish(ring);
        p.poll_events();
        if changed {
            let _ = space_event.signal();
        }
        if let (Some(rec), Some(input)) = (&mut p.rec, input) {
            let _ = input.wake_event.clear();
            rec.follow(&mut p.snd, &input.ring);
            rec.reap(&input.ring, &input.data_event);
        }
    }
}

fn setup() -> Result<Player, DriverError> {
    let Some(h) = vrt::env::take_handle(PCIDEV_ROLE) else {
        return Err(DriverError::NoConfig);
    };
    let pci = pcidev::Client::new(Channel::from_handle(h));
    let mut snd = SndDevice::new(pci)?;
    println!("{} jack(s), {} stream(s), {} channel map(s)", snd.jacks, snd.streams, snd.chmaps);
    let tx = snd.dev.setup_queue(device::TXQ, (SLOTS * 2) as u16)?;
    let mut events = snd.dev.setup_queue(device::EVENTQ, EVENT_BUFFERS as u16)?;
    // Queues must exist before DRIVER_OK; whether there is an input stream
    // to use the receive queue for is known only afterwards.
    let rx = snd.dev.setup_queue(device::RXQ, (capture::SLOTS * 3) as u16).ok();
    let irq = snd.dev.msix_vector(Some(device::TXQ)).ok();
    let rx_irq = if rx.is_some() { snd.dev.msix_vector(Some(device::RXQ)).ok() } else { None };
    snd.dev.driver_ok();

    let event_buf = DmaBuffer::new(snd.dev.dma(), EVENT_BUFFERS * EVENT_SIZE)?;
    let mut event_slot_of_head = vec![0usize; events.size() as usize];
    for i in 0..EVENT_BUFFERS.min(events.size() as usize) {
        let seg =
            Segment { phys: event_buf.phys() + (i * EVENT_SIZE) as u64, len: EVENT_SIZE as u32, device_writes: true };
        if let Some(h) = events.push(&[seg]) {
            event_slot_of_head[h as usize] = i;
        }
    }
    events.notify();

    let infos = snd.pcm_info()?;
    for i in &infos {
        println!(
            "virtio-snd: stream {}: {}, {}-{} channels, formats {:#x}, rates {:#x}, features {:#x}",
            i.id,
            if i.direction == device::D_OUTPUT { "output" } else { "input" },
            i.channels_min,
            i.channels_max,
            i.formats,
            i.rates,
            i.features
        );
    }
    match snd.jack_info() {
        Ok(jacks) => {
            for (nid, connected) in jacks {
                println!("jack (HDA nid {}) {}", nid, if connected { "connected" } else { "unplugged" });
            }
        }
        Err(e) => println!("jack info unavailable: {}", e),
    }
    match snd.chmap_info() {
        Ok(maps) => {
            for (dir, ch) in maps {
                println!("channel map ({} channels, {})", ch, if dir == device::D_OUTPUT { "output" } else { "input" });
            }
        }
        Err(e) => println!("channel maps unavailable: {}", e),
    }
    let cfg =
        infos.iter().find_map(|i| device::choose_config(i, MAX_DEPTH as u32)).ok_or(DriverError::NoOutputStream)?;
    snd.set_params(&cfg)?;
    snd.prepare(cfg.stream)?;

    let period_bytes = cfg.period_bytes();
    let status_off = (4 + period_bytes).next_multiple_of(8);
    let stride = (status_off + 8).next_multiple_of(64);
    let dma = DmaBuffer::new(snd.dev.dma(), stride * SLOTS)?;
    println!(
        "virtio-snd: output stream {} ready: {} Hz, {} channel(s), S16, {} frames per period, {} interrupts",
        cfg.stream,
        cfg.rate,
        cfg.channels,
        cfg.period_frames,
        if irq.is_some() { "MSI-X" } else { "polled, no" }
    );
    let rec = match (rx, infos.iter().find_map(device::choose_input_config)) {
        (Some(rx), Some(icfg)) => match snd.set_params(&icfg).and_then(|_| snd.prepare(icfg.stream)) {
            Ok(()) => match Recorder::new(&mut snd, rx, icfg, rx_irq) {
                Ok(r) => {
                    println!(
                        "virtio-snd: input stream {} ready: {} Hz, {} channel(s), S16, {} frames per period",
                        icfg.stream, icfg.rate, icfg.channels, icfg.period_frames
                    );
                    Some(r)
                }
                Err(e) => {
                    println!("no recording: {}", e);
                    None
                }
            },
            Err(e) => {
                println!("cannot configure the input stream: {}", e);
                None
            }
        },
        _ => None,
    };
    let qsize = tx.size() as usize;
    Ok(Player {
        snd,
        cfg,
        tx,
        irq,
        dma,
        stride,
        status_off,
        slot_of_head: vec![0; qsize],
        free: (0..SLOTS.min(qsize / 2)).rev().collect(),
        inflight: VecDeque::new(),
        running: false,
        depth: DEPTH,
        played: 0,
        underruns: 0,
        starving: false,
        last_audio_ns: 0,
        status_errors: 0,
        scratch: vec![0; cfg.period_frames as usize * cfg.channels as usize],
        events,
        event_buf,
        event_slot_of_head,
        rec,
    })
}

/// Attaches to the audio service and plays until it goes away, forever.
fn run(mut player: Player) {
    loop {
        let input_cfg = player.rec.as_ref().map(|r| r.cfg);
        match attach(&player.cfg, input_cfg.as_ref()) {
            Ok((link, ring, data_event, space_event, input)) => {
                println!("attached to the audio service{}", if input.is_some() { " (with recording)" } else { "" });
                player.played = ring.read_pos();
                serve(&mut player, &link, &ring, &data_event, &space_event, input.as_ref());
                println!("the audio service went away");
                player.halt();
            }
            Err(e) => {
                println!("cannot attach: {}", e);
                vrt::time::sleep(vrt::time::Duration::from_secs(2));
            }
        }
    }
}

fn main() -> i32 {
    let player = match setup() {
        Ok(p) => p,
        Err(e) => {
            println!("{}", e);
            return 1;
        }
    };
    // Refilling the device is time-critical: run above normal programs.
    let worker = vrt::thread::Builder::new().name("playback").priority(vabi::priority::HIGH).spawn(move || run(player));
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
