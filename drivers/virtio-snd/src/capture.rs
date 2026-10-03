//! Recording from the device's input stream.
//!
//! The receive queue holds up to `SLOTS` period-sized transfers. A transfer
//! is one descriptor chain: `virtio_snd_pcm_xfer` (device-readable), the PCM
//! buffer and `virtio_snd_pcm_status` (both device-writable). Each completed
//! transfer is copied into the audio service's input ring, stamped with the
//! time it arrived, and queued again. The stream runs only while the
//! service wants audio (the ring's CAPTURE flag), so the host's microphone
//! is not opened while nobody listens.

use alloc::vec;
use alloc::vec::Vec;

use vproto::audio::{Ring, ring::flags};
use vrt::object::{Event, Interrupt};
use vrt::println;
use vrt::time::now_ns;
use vvirtio::{DmaBuffer, Segment, Virtqueue};

use crate::device::{DriverError, PcmConfig, S_OK, SndDevice};

/// Receive transfers (each needs three descriptors).
pub const SLOTS: usize = 8;

pub struct Recorder {
    pub cfg: PcmConfig,
    rx: Virtqueue,
    pub irq: Option<Interrupt>,
    dma: DmaBuffer,
    stride: usize,
    status_off: usize,
    slot_of_head: Vec<usize>,
    free: Vec<usize>,
    /// Transfers the device holds.
    inflight: usize,
    pub running: bool,
    status_errors: u32,
    scratch: Vec<i16>,
}

impl Recorder {
    pub fn new(
        snd: &mut SndDevice,
        rx: Virtqueue,
        cfg: PcmConfig,
        irq: Option<Interrupt>,
    ) -> Result<Recorder, DriverError> {
        let status_off = (4 + cfg.period_bytes()).next_multiple_of(8);
        let stride = (status_off + 8).next_multiple_of(64);
        let dma = DmaBuffer::new(snd.dev.dma(), stride * SLOTS)?;
        let qsize = rx.size() as usize;
        Ok(Recorder {
            cfg,
            rx,
            irq,
            dma,
            stride,
            status_off,
            slot_of_head: vec![0; qsize],
            free: (0..SLOTS.min(qsize / 3)).rev().collect(),
            inflight: 0,
            running: false,
            status_errors: 0,
            scratch: vec![0; cfg.period_frames as usize * cfg.channels as usize],
        })
    }

    /// Hands every free buffer to the device.
    fn post_free(&mut self) {
        let period = self.cfg.period_bytes();
        let mut posted = false;
        while let Some(slot) = self.free.pop() {
            let base = slot * self.stride;
            self.dma.write(base, &self.cfg.stream.to_le_bytes());
            self.dma.write(base + self.status_off, &[0xFF; 8]);
            let phys = self.dma.phys() + base as u64;
            let chain = [
                Segment { phys, len: 4, device_writes: false },
                Segment { phys: phys + 4, len: period as u32, device_writes: true },
                Segment { phys: phys + self.status_off as u64, len: 8, device_writes: true },
            ];
            match self.rx.push(&chain) {
                Some(head) => {
                    self.slot_of_head[head as usize] = slot;
                    self.inflight += 1;
                    posted = true;
                }
                None => {
                    self.free.push(slot);
                    break;
                }
            }
        }
        if posted {
            self.rx.notify();
        }
    }

    /// Starts or stops the stream to follow the service's CAPTURE flag.
    /// Returns true if the state changed.
    pub fn follow(&mut self, snd: &mut SndDevice, ring: &Ring) -> bool {
        let wanted = ring.consumer_flags() & flags::CAPTURE != 0;
        if wanted == self.running {
            return false;
        }
        if wanted {
            self.post_free();
            match snd.start(self.cfg.stream) {
                Ok(()) => {
                    self.running = true;
                    println!("recording started");
                }
                Err(e) => println!("cannot start recording: {}", e),
            }
        } else {
            match snd.stop(self.cfg.stream) {
                Ok(()) => println!("recording stopped"),
                Err(e) => println!("cannot stop recording: {}", e),
            }
            self.running = false;
        }
        true
    }

    /// Takes completed transfers into the ring. Returns true if frames were
    /// delivered.
    pub fn reap(&mut self, ring: &Ring, data_event: &Event) -> bool {
        let ch = self.cfg.channels as usize;
        let frame_bytes = self.cfg.frame_bytes();
        let period = self.cfg.period_frames as usize;
        let mut delivered = false;
        while let Some((head, len)) = self.rx.pop_used() {
            let slot = self.slot_of_head[head as usize];
            self.inflight = self.inflight.saturating_sub(1);
            let base = slot * self.stride;
            // SAFETY: the device has finished writing this transfer.
            let st = unsafe { self.dma.bytes(base + self.status_off, 4) };
            let status = u32::from_le_bytes([st[0], st[1], st[2], st[3]]);
            // The used length counts the PCM written plus the status.
            let frames = ((len as usize).saturating_sub(8) / frame_bytes).min(period);
            if status != S_OK {
                self.status_errors += 1;
                if self.status_errors <= 5 {
                    println!("capture transfer failed with status {:#x}", status);
                }
            } else if frames > 0 && self.running {
                // SAFETY: inside this slot's PCM area, written by the device.
                let pcm = unsafe { self.dma.bytes(base + 4, frames * frame_bytes) };
                for (i, s) in pcm.chunks_exact(2).enumerate() {
                    self.scratch[i] = i16::from_le_bytes([s[0], s[1]]);
                }
                let written = ring.write(&self.scratch[..frames * ch]);
                if written < frames {
                    ring.set_overruns(ring.overruns().saturating_add((frames - written) as u32));
                }
                ring.set_capture_clock(ring.write_pos(), now_ns());
                delivered = true;
            }
            self.free.push(slot);
        }
        if self.running {
            self.post_free();
        }
        if delivered {
            let _ = data_event.signal();
        }
        delivered
    }

    /// Stops and releases the stream when the audio service goes away, then
    /// prepares it for the next attach.
    pub fn halt(&mut self, snd: &mut SndDevice) {
        if self.running {
            let _ = snd.stop(self.cfg.stream);
            self.running = false;
        }
        let _ = snd.release(self.cfg.stream);
        let deadline = now_ns() + 200_000_000;
        while self.inflight > 0 && now_ns() < deadline {
            while let Some((head, _)) = self.rx.pop_used() {
                let slot = self.slot_of_head[head as usize];
                self.inflight = self.inflight.saturating_sub(1);
                self.free.push(slot);
            }
            vrt::time::sleep(vrt::time::Duration::from_millis(2));
        }
        if let Err(e) = snd.set_params(&self.cfg).and_then(|_| snd.prepare(self.cfg.stream)) {
            println!("cannot prepare the input stream again: {}", e);
        }
    }
}
