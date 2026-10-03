//! Shared-memory packet rings: how frames travel between network drivers,
//! the Wi-Fi service and the network service.
//!
//! A [`Ring`] is a single-producer/single-consumer queue of fixed-size
//! slots in a VMO. Each slot holds one frame: a 16-byte header (length,
//! [`SlotMeta`]) and the payload. A [`Link`] pairs two rings (one per
//! direction) with two events so each side can sleep until the other has
//! produced data or freed space.
//!
//! # Robustness
//!
//! The peer is not trusted. Each side keeps its own position privately and
//! only publishes it; the peer's position is clamped to the ring's
//! capacity, slot lengths are checked against the slot size, and payloads
//! are copied out of shared memory before anyone parses them. A broken or
//! hostile peer can therefore only lose or garble its own frames.
//!
//! # Wake-ups
//!
//! Signalling an event costs a system call, so it happens only when the
//! other side is asleep: a consumer about to sleep sets `WAITING_DATA` in
//! the ring header and re-checks the ring; a producer that publishes a slot
//! and finds the flag set clears it and signals the consumer's event. A
//! producer facing a full ring does the same with `WAITING_SPACE`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use vipc::message;
use vrt::object::{Event, Vmo};
use vrt::vm::Mapping;

/// Layout of a ring VMO. All fields are little-endian and naturally
/// aligned; producer and consumer fields live on separate cache lines.
pub mod layout {
    /// `"VNRG"`.
    pub const MAGIC: u32 = u32::from_le_bytes(*b"VNRG");
    pub const VERSION: u32 = 1;
    pub const OFF_MAGIC: usize = 0;
    pub const OFF_VERSION: usize = 4;
    /// Number of slots (a power of two).
    pub const OFF_SLOTS: usize = 8;
    /// Bytes per slot, header included (a multiple of 64).
    pub const OFF_SLOT_SIZE: usize = 12;
    /// Producer: slots published (u64, running count).
    pub const OFF_WRITE: usize = 64;
    /// Producer: frames dropped because the ring was full (u64).
    pub const OFF_DROPPED: usize = 72;
    /// Consumer: slots consumed (u64, running count).
    pub const OFF_READ: usize = 128;
    /// Wake-up flags (u32, see [`flags`]); set by the side going to sleep,
    /// cleared by the side that wakes it.
    pub const OFF_FLAGS: usize = 192;
    /// Where the slots start.
    pub const DATA_OFFSET: usize = 4096;
    /// Bytes of each slot before the payload: `len: u32`, `kind: u16`,
    /// `flags: u16`, `meta: u64`.
    pub const SLOT_HEADER: usize = 16;
    pub const MIN_SLOTS: u32 = 8;
    pub const MAX_SLOTS: u32 = 4096;
    pub const MIN_SLOT_SIZE: u32 = 256;
    pub const MAX_SLOT_SIZE: u32 = 16384;
    /// Largest ring VMO a peer may hand over.
    pub const MAX_BYTES: usize = 16 << 20;

    /// Bits of the wake-up flag word.
    pub mod flags {
        /// The consumer is asleep waiting for data.
        pub const WAITING_DATA: u32 = 1;
        /// The producer is asleep waiting for free slots.
        pub const WAITING_SPACE: u32 = 2;
    }
}

/// What a slot carries (the `kind` field of [`SlotMeta`]).
pub mod kind {
    /// An Ethernet II frame (destination, source, EtherType, payload; no FCS).
    pub const ETHERNET: u16 = 1;
    /// An IEEE 802.11 MAC frame (no FCS). For received frames `meta` is a
    /// [`super::RxInfo`]; for transmitted ones a [`super::TxInfo`].
    pub const IEEE80211: u16 = 2;
    /// Transmit status of an 802.11 frame (radio to Wi-Fi service, no
    /// payload, `meta` is a [`super::TxStatus`]).
    pub const TX_STATUS: u16 = 3;
}

/// Per-slot metadata besides the payload length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotMeta {
    /// One of [`kind`].
    pub kind: u16,
    pub flags: u16,
    /// Kind-specific data.
    pub meta: u64,
}

impl SlotMeta {
    pub const fn ethernet() -> SlotMeta {
        SlotMeta { kind: kind::ETHERNET, flags: 0, meta: 0 }
    }
}

/// Reception details of an 802.11 frame, packed into [`SlotMeta::meta`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RxInfo {
    /// Channel number the frame arrived on.
    pub channel: u8,
    /// Band: 1 = 2.4 GHz, 2 = 5 GHz, 3 = 6 GHz.
    pub band: u8,
    /// Received signal strength in dBm.
    pub signal_dbm: i8,
    /// Noise floor in dBm (0 if unknown).
    pub noise_dbm: i8,
    /// Data rate in units of 500 kbit/s (0 if unknown).
    pub rate: u8,
}

impl RxInfo {
    pub fn pack(&self) -> u64 {
        u64::from_le_bytes([self.channel, self.band, self.signal_dbm as u8, self.noise_dbm as u8, self.rate, 0, 0, 0])
    }

    pub fn unpack(v: u64) -> RxInfo {
        let b = v.to_le_bytes();
        RxInfo { channel: b[0], band: b[1], signal_dbm: b[2] as i8, noise_dbm: b[3] as i8, rate: b[4] }
    }
}

/// Transmit request details of an 802.11 frame, packed into
/// [`SlotMeta::meta`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxInfo {
    /// Echoed back in the [`TxStatus`] (0 = no status wanted).
    pub id: u32,
    /// Data rate in units of 500 kbit/s (0 = the radio chooses).
    pub rate: u8,
    /// Maximum transmission attempts (0 = the radio's default).
    pub retries: u8,
    /// Do not wait for an acknowledgement (group-addressed frames).
    pub no_ack: bool,
}

impl TxInfo {
    pub fn pack(&self) -> u64 {
        let id = self.id.to_le_bytes();
        u64::from_le_bytes([id[0], id[1], id[2], id[3], self.rate, self.retries, self.no_ack as u8, 0])
    }

    pub fn unpack(v: u64) -> TxInfo {
        let b = v.to_le_bytes();
        TxInfo { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), rate: b[4], retries: b[5], no_ack: b[6] != 0 }
    }
}

/// The outcome of a transmission, packed into [`SlotMeta::meta`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxStatus {
    /// The [`TxInfo::id`] of the frame.
    pub id: u32,
    /// The receiver acknowledged the frame.
    pub acked: bool,
    /// Transmission attempts made.
    pub attempts: u8,
}

impl TxStatus {
    pub fn pack(&self) -> u64 {
        let id = self.id.to_le_bytes();
        u64::from_le_bytes([id[0], id[1], id[2], id[3], self.acked as u8, self.attempts, 0, 0])
    }

    pub fn unpack(v: u64) -> TxStatus {
        let b = v.to_le_bytes();
        TxStatus { id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]), acked: b[4] != 0, attempts: b[5] }
    }
}

/// Why a ring could not be created or mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    NoMemory,
    /// The peer's VMO has a bad header or size.
    BadHeader,
}

impl core::fmt::Display for RingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            RingError::NoMemory => "out of memory",
            RingError::BadHeader => "invalid ring",
        })
    }
}

/// Which side of a ring this process is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Producer,
    Consumer,
}

/// Bytes of a ring VMO with `slots` slots of `slot_size` bytes.
pub fn ring_bytes(slots: u32, slot_size: u32) -> usize {
    layout::DATA_OFFSET + slots as usize * slot_size as usize
}

/// One side of a shared packet ring.
pub struct Ring {
    base: *mut u8,
    slots: u32,
    slot_size: u32,
    role: Role,
    /// Our own running position (write count for the producer, read count
    /// for the consumer). The shared copy is only published, never read
    /// back.
    local: core::cell::Cell<u64>,
    /// Malformed frames skipped by the consumer.
    bad: core::cell::Cell<u64>,
    _map: Option<Mapping>,
}

// SAFETY: each side uses its ring from one thread at a time; shared fields
// are accessed atomically and payload bytes are copied.
unsafe impl Send for Ring {}

impl core::fmt::Debug for Ring {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Ring({:?}, {} x {} bytes)", self.role, self.slots, self.slot_size)
    }
}

impl Ring {
    /// Creates a ring in a new VMO. Returns our side and a VMO handle for
    /// the peer. `slots` is rounded up to a power of two and `slot_size` to
    /// a multiple of 64; both are clamped to the allowed ranges.
    pub fn create(slots: u32, slot_size: u32, role: Role) -> Result<(Ring, Vmo), RingError> {
        let slots = slots.clamp(layout::MIN_SLOTS, layout::MAX_SLOTS).next_power_of_two();
        let slot_size = slot_size.clamp(layout::MIN_SLOT_SIZE, layout::MAX_SLOT_SIZE).next_multiple_of(64);
        let len = ring_bytes(slots, slot_size).next_multiple_of(4096);
        let vmo = Vmo::create(len).map_err(|_| RingError::NoMemory)?;
        let peer = Vmo::from_handle(vmo.0.duplicate(None).map_err(|_| RingError::NoMemory)?);
        let map =
            Mapping::new(vmo, len, vabi::map_flags::READ | vabi::map_flags::WRITE).map_err(|_| RingError::NoMemory)?;
        // SAFETY: a fresh mapping of `len` bytes.
        let ring = unsafe { Ring::init_raw(map.as_ptr(), len, slots, slot_size, role) };
        Ok((Ring { _map: Some(map), ..ring }, peer))
    }

    /// Maps a ring the peer created, validating its header.
    pub fn map(vmo: Vmo, role: Role) -> Result<Ring, RingError> {
        let len = vmo.size().map_err(|_| RingError::BadHeader)?;
        if !(layout::DATA_OFFSET + layout::MIN_SLOTS as usize * layout::MIN_SLOT_SIZE as usize..=layout::MAX_BYTES)
            .contains(&len)
        {
            return Err(RingError::BadHeader);
        }
        let map =
            Mapping::new(vmo, len, vabi::map_flags::READ | vabi::map_flags::WRITE).map_err(|_| RingError::NoMemory)?;
        // SAFETY: the mapping is `len` bytes long.
        let ring = unsafe { Ring::attach_raw(map.as_ptr(), len, role)? };
        Ok(Ring { _map: Some(map), ..ring })
    }

    /// Initialises a ring header in raw memory.
    ///
    /// # Safety
    /// `base` must be valid for `len` bytes, 8-byte aligned, and outlive the
    /// ring; `len` must be at least [`ring_bytes`]`(slots, slot_size)`.
    pub unsafe fn init_raw(base: *mut u8, len: usize, slots: u32, slot_size: u32, role: Role) -> Ring {
        assert!(slots.is_power_of_two() && slot_size.is_multiple_of(64) && len >= ring_bytes(slots, slot_size));
        // SAFETY: the caller guarantees the memory.
        unsafe {
            core::ptr::write_bytes(base, 0, layout::DATA_OFFSET);
            let w = |off: usize, v: u32| (base.add(off) as *mut u32).write_volatile(v);
            w(layout::OFF_VERSION, layout::VERSION);
            w(layout::OFF_SLOTS, slots);
            w(layout::OFF_SLOT_SIZE, slot_size);
            fence(Ordering::Release);
            w(layout::OFF_MAGIC, layout::MAGIC);
        }
        Ring {
            base,
            slots,
            slot_size,
            role,
            local: core::cell::Cell::new(0),
            bad: core::cell::Cell::new(0),
            _map: None,
        }
    }

    /// Attaches to a ring header the peer initialised.
    ///
    /// # Safety
    /// `base` must be valid for `len` bytes, 8-byte aligned, and outlive the
    /// ring.
    pub unsafe fn attach_raw(base: *mut u8, len: usize, role: Role) -> Result<Ring, RingError> {
        if len < layout::DATA_OFFSET {
            return Err(RingError::BadHeader);
        }
        // SAFETY: inside the caller's memory.
        let r = |off: usize| unsafe { (base.add(off) as *const u32).read_volatile() };
        let (slots, slot_size) = (r(layout::OFF_SLOTS), r(layout::OFF_SLOT_SIZE));
        if r(layout::OFF_MAGIC) != layout::MAGIC
            || r(layout::OFF_VERSION) != layout::VERSION
            || !slots.is_power_of_two()
            || !(layout::MIN_SLOTS..=layout::MAX_SLOTS).contains(&slots)
            || !slot_size.is_multiple_of(64)
            || !(layout::MIN_SLOT_SIZE..=layout::MAX_SLOT_SIZE).contains(&slot_size)
            || len < ring_bytes(slots, slot_size)
        {
            return Err(RingError::BadHeader);
        }
        let ring = Ring {
            base,
            slots,
            slot_size,
            role,
            local: core::cell::Cell::new(0),
            bad: core::cell::Cell::new(0),
            _map: None,
        };
        // Continue from the position we published before (a restarted peer
        // attaching to a live ring), or from zero.
        let start = match role {
            Role::Producer => ring.u64_at(layout::OFF_WRITE).load(Ordering::Acquire),
            Role::Consumer => ring.u64_at(layout::OFF_READ).load(Ordering::Acquire),
        };
        ring.local.set(start);
        Ok(ring)
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        // SAFETY: header offsets are inside the mapping and 8-byte aligned.
        unsafe { &*(self.base.add(off) as *const AtomicU64) }
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        // SAFETY: header offsets are inside the mapping and 4-byte aligned.
        unsafe { &*(self.base.add(off) as *const AtomicU32) }
    }

    pub fn slots(&self) -> u32 {
        self.slots
    }

    /// Largest payload a slot can hold.
    pub fn max_payload(&self) -> usize {
        self.slot_size as usize - layout::SLOT_HEADER
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Slots published by the producer (the peer's value clamped on the
    /// consumer side).
    fn write_pos(&self) -> u64 {
        match self.role {
            Role::Producer => self.local.get(),
            Role::Consumer => {
                let r = self.local.get();
                let w = self.u64_at(layout::OFF_WRITE).load(Ordering::Acquire);
                r + w.wrapping_sub(r).min(self.slots as u64)
            }
        }
    }

    /// Slots consumed (the peer's value clamped on the producer side; a
    /// bogus value makes the ring look full, never overfull).
    fn read_pos(&self) -> u64 {
        match self.role {
            Role::Consumer => self.local.get(),
            Role::Producer => {
                let w = self.local.get();
                let r = self.u64_at(layout::OFF_READ).load(Ordering::Acquire);
                w - w.wrapping_sub(r).min(self.slots as u64).min(w)
            }
        }
    }

    /// Frames waiting to be consumed.
    pub fn queued(&self) -> u32 {
        (self.write_pos() - self.read_pos()) as u32
    }

    /// Free slots (producer side).
    pub fn free(&self) -> u32 {
        self.slots - self.queued()
    }

    pub fn is_empty(&self) -> bool {
        self.queued() == 0
    }

    fn slot(&self, pos: u64) -> *mut u8 {
        let index = (pos & (self.slots as u64 - 1)) as usize;
        // SAFETY: index < slots; the slot area lies inside the mapping.
        unsafe { self.base.add(layout::DATA_OFFSET + index * self.slot_size as usize) }
    }

    /// Producer: appends a frame. Returns `false` (and counts a drop) if
    /// the ring is full or the payload does not fit a slot.
    pub fn push(&self, meta: SlotMeta, payload: &[u8]) -> bool {
        self.push_with(meta, payload.len(), |buf| buf.copy_from_slice(payload))
    }

    /// Producer: appends a frame of `len` bytes written by `fill` straight
    /// into the slot.
    pub fn push_with(&self, meta: SlotMeta, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        debug_assert_eq!(self.role, Role::Producer);
        if len > self.max_payload() || self.free() == 0 {
            self.u64_at(layout::OFF_DROPPED).fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let w = self.local.get();
        let slot = self.slot(w);
        // SAFETY: the slot is ours until we publish it (the consumer never
        // reads past the published write position), and `len` fits.
        unsafe {
            let payload = core::slice::from_raw_parts_mut(slot.add(layout::SLOT_HEADER), len);
            fill(payload);
            (slot as *mut u32).write_volatile(len as u32);
            (slot.add(4) as *mut u16).write_volatile(meta.kind);
            (slot.add(6) as *mut u16).write_volatile(meta.flags);
            (slot.add(8) as *mut u64).write_unaligned(meta.meta);
        }
        self.local.set(w + 1);
        self.u64_at(layout::OFF_WRITE).store(w + 1, Ordering::Release);
        true
    }

    /// Consumer: takes the next frame, copying its payload into `buf`.
    /// Returns its metadata and length. Frames longer than `buf` or than a
    /// slot are skipped (counted in [`Ring::bad_frames`]); at most one
    /// ring's worth per call, so a hostile producer cannot keep the consumer
    /// busy inside one call.
    pub fn pop(&self, buf: &mut [u8]) -> Option<(SlotMeta, usize)> {
        debug_assert_eq!(self.role, Role::Consumer);
        for _ in 0..self.slots {
            if self.is_empty() {
                return None;
            }
            let r = self.local.get();
            let slot = self.slot(r);
            // SAFETY: published slot inside the mapping; values are checked
            // before use and the payload is copied out.
            let (len, meta) = unsafe {
                let len = (slot as *const u32).read_volatile() as usize;
                let meta = SlotMeta {
                    kind: (slot.add(4) as *const u16).read_volatile(),
                    flags: (slot.add(6) as *const u16).read_volatile(),
                    meta: (slot.add(8) as *const u64).read_unaligned(),
                };
                (len, meta)
            };
            let ok = len <= self.max_payload() && len <= buf.len();
            if ok {
                // SAFETY: `len` fits both the slot and `buf`.
                unsafe { core::ptr::copy_nonoverlapping(slot.add(layout::SLOT_HEADER), buf.as_mut_ptr(), len) };
            }
            self.local.set(r + 1);
            self.u64_at(layout::OFF_READ).store(r + 1, Ordering::Release);
            if ok {
                return Some((meta, len));
            }
            self.bad.set(self.bad.get() + 1);
        }
        None
    }

    /// Consumer: malformed frames skipped.
    pub fn bad_frames(&self) -> u64 {
        self.bad.get()
    }

    /// Producer: frames dropped because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.u64_at(layout::OFF_DROPPED).load(Ordering::Relaxed)
    }

    /// Sets one of our wake-up flags (see the module docs).
    fn set_flag(&self, flag: u32) {
        self.u32_at(layout::OFF_FLAGS).fetch_or(flag, Ordering::SeqCst);
    }

    fn clear_flag(&self, flag: u32) {
        self.u32_at(layout::OFF_FLAGS).fetch_and(!flag, Ordering::SeqCst);
    }

    /// Clears `flag` if set; returns whether it was set (the peer is asleep
    /// and must be woken).
    fn take_flag(&self, flag: u32) -> bool {
        self.u32_at(layout::OFF_FLAGS).fetch_and(!flag, Ordering::SeqCst) & flag != 0
    }
}

/// One end of a bidirectional frame link: a ring we consume, a ring we
/// produce, the event we sleep on and the event that wakes the peer.
pub struct Link {
    rx: Ring,
    tx: Ring,
    wake: Event,
    peer_wake: Event,
}

impl core::fmt::Debug for Link {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Link(rx {:?}, tx {:?})", self.rx, self.tx)
    }
}

message! {
    /// The handles that make up a link, as handed from the side that
    /// creates it (a driver) to the side that attaches (the stack).
    #[derive(Debug)]
    pub struct LinkEndpoints {
        /// Frames from the device to the stack.
        pub to_stack: Vmo,
        /// Frames from the stack to the device.
        pub to_device: Vmo,
        /// The stack sleeps on this event.
        pub stack_wake: Event,
        /// The device sleeps on this event.
        pub device_wake: Event,
    }
}

fn dup_event(e: &Event) -> Result<Event, RingError> {
    e.0.duplicate(None).map(Event::from_handle).map_err(|_| RingError::NoMemory)
}

impl Link {
    /// Creates a link on the device side (the side that owns the device's
    /// frames). Returns our end and the endpoints to hand to the stack.
    pub fn create(slots: u32, slot_size: u32) -> Result<(Link, LinkEndpoints), RingError> {
        let (tx, to_stack) = Ring::create(slots, slot_size, Role::Producer)?;
        let (rx, to_device) = Ring::create(slots, slot_size, Role::Consumer)?;
        let wake = Event::create().map_err(|_| RingError::NoMemory)?;
        let peer_wake = Event::create().map_err(|_| RingError::NoMemory)?;
        let ends =
            LinkEndpoints { to_stack, to_device, stack_wake: dup_event(&peer_wake)?, device_wake: dup_event(&wake)? };
        Ok((Link { rx, tx, wake, peer_wake }, ends))
    }

    /// Attaches the stack side to endpoints received from a device.
    pub fn attach(ends: LinkEndpoints) -> Result<Link, RingError> {
        Ok(Link {
            rx: Ring::map(ends.to_stack, Role::Consumer)?,
            tx: Ring::map(ends.to_device, Role::Producer)?,
            wake: ends.stack_wake,
            peer_wake: ends.device_wake,
        })
    }

    /// The event this side sleeps on (add it to the wait set).
    pub fn wake_event(&self) -> &Event {
        &self.wake
    }

    /// Largest frame the peer accepts.
    pub fn max_frame(&self) -> usize {
        self.tx.max_payload()
    }

    /// Queues a frame for the peer, waking it if it sleeps. Returns `false`
    /// if the frame was dropped (ring full or frame too large).
    pub fn send(&self, meta: SlotMeta, frame: &[u8]) -> bool {
        self.send_with(meta, frame.len(), |b| b.copy_from_slice(frame))
    }

    /// Like [`Link::send`], with the frame written straight into the ring.
    pub fn send_with(&self, meta: SlotMeta, len: usize, fill: impl FnOnce(&mut [u8])) -> bool {
        let ok = self.tx.push_with(meta, len, fill);
        if ok {
            fence(Ordering::SeqCst);
            if self.tx.take_flag(layout::flags::WAITING_DATA) {
                let _ = self.peer_wake.signal();
            }
        }
        ok
    }

    /// Free slots for sending.
    pub fn send_space(&self) -> u32 {
        self.tx.free()
    }

    /// Takes the next frame from the peer, waking the peer if it was waiting
    /// for space.
    pub fn recv(&self, buf: &mut [u8]) -> Option<(SlotMeta, usize)> {
        let r = self.rx.pop(buf);
        if r.is_some() {
            fence(Ordering::SeqCst);
            if self.rx.take_flag(layout::flags::WAITING_SPACE) {
                let _ = self.peer_wake.signal();
            }
        }
        r
    }

    /// Frames waiting to be received.
    pub fn pending(&self) -> u32 {
        self.rx.queued()
    }

    /// Call before sleeping on [`Link::wake_event`]: clears the event and
    /// asks the peer to wake us for new frames (and, with
    /// `want_space`, when sending room frees up). Returns `false` if there
    /// is already work to do, in which case do not sleep.
    pub fn prepare_wait(&self, want_space: bool) -> bool {
        let _ = self.wake.clear();
        self.rx.set_flag(layout::flags::WAITING_DATA);
        if want_space {
            self.tx.set_flag(layout::flags::WAITING_SPACE);
        }
        fence(Ordering::SeqCst);
        let busy = !self.rx.is_empty() || (want_space && self.tx.free() > 0);
        if busy {
            self.finish_wait();
        }
        !busy
    }

    /// Call after waking: withdraws the wake-up requests.
    pub fn finish_wait(&self) {
        self.rx.clear_flag(layout::flags::WAITING_DATA);
        self.tx.clear_flag(layout::flags::WAITING_SPACE);
    }

    /// Frames the peer had to drop because our receive ring was full.
    pub fn peer_dropped(&self) -> u64 {
        self.rx.dropped()
    }

    /// Frames we dropped because the peer's ring was full.
    pub fn dropped(&self) -> u64 {
        self.tx.dropped()
    }

    /// Malformed frames skipped while receiving.
    pub fn bad_frames(&self) -> u64 {
        self.rx.bad_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    /// A ring pair over plain host memory: (producer, consumer).
    fn host_ring(slots: u32, slot_size: u32) -> (Vec<u64>, Ring, Ring) {
        let len = ring_bytes(slots, slot_size);
        let mut mem = vec![0u64; len / 8];
        let base = mem.as_mut_ptr() as *mut u8;
        // SAFETY: `mem` outlives both rings within each test.
        let p = unsafe { Ring::init_raw(base, len, slots, slot_size, Role::Producer) };
        let c = unsafe { Ring::attach_raw(base, len, Role::Consumer).unwrap() };
        (mem, p, c)
    }

    #[test]
    fn frames_round_trip_in_order() {
        let (_mem, p, c) = host_ring(8, 256);
        let mut buf = [0u8; 2048];
        for i in 0..20u8 {
            let frame = vec![i; 10 + i as usize];
            assert!(p.push(SlotMeta { kind: kind::ETHERNET, flags: i as u16, meta: i as u64 * 3 }, &frame));
            let (meta, len) = c.pop(&mut buf).unwrap();
            assert_eq!(len, frame.len());
            assert_eq!(&buf[..len], &frame[..]);
            assert_eq!(meta, SlotMeta { kind: kind::ETHERNET, flags: i as u16, meta: i as u64 * 3 });
        }
        assert!(c.pop(&mut buf).is_none());
    }

    #[test]
    fn full_ring_drops_and_counts() {
        let (_mem, p, c) = host_ring(8, 256);
        for _ in 0..8 {
            assert!(p.push(SlotMeta::ethernet(), &[1, 2, 3]));
        }
        assert_eq!(p.free(), 0);
        assert!(!p.push(SlotMeta::ethernet(), &[4]));
        assert_eq!(p.dropped(), 1);
        let mut buf = [0u8; 64];
        assert!(c.pop(&mut buf).is_some());
        assert!(p.push(SlotMeta::ethernet(), &[5]));
        // Oversized payloads never enter the ring.
        assert!(!p.push(SlotMeta::ethernet(), &[0u8; 241]));
        assert_eq!(p.dropped(), 2);
    }

    #[test]
    fn hostile_positions_and_lengths_are_contained() {
        let (mem, p, c) = host_ring(8, 256);
        let base = mem.as_ptr() as *mut u8;
        assert!(p.push(SlotMeta::ethernet(), &[7; 5]));
        // A peer claims to have written a billion frames: at most `slots`
        // are visible.
        // SAFETY: writing the shared header in host memory.
        unsafe { (base.add(layout::OFF_WRITE) as *mut u64).write(1_000_000_000) };
        assert_eq!(c.queued(), 8);
        // And it puts a huge length into a slot: the frame is skipped.
        unsafe { (base.add(layout::DATA_OFFSET + 256) as *mut u32).write(0xFFFF_FFFF) };
        // The consumer keeps seeing a full ring (as if the device sent
        // frames without end), so callers bound their work per poll; every
        // call returns promptly and stays inside the ring.
        let mut buf = [0u8; 300];
        let mut got = 0;
        for _ in 0..32 {
            if c.pop(&mut buf).is_some() {
                got += 1;
            }
            assert!(c.queued() <= 8);
        }
        assert!(got >= 28);
        assert!(c.bad_frames() >= 1);
        // A slot of nothing but bad lengths: one call skips at most a ring's
        // worth and returns.
        for i in 0..8 {
            unsafe { (base.add(layout::DATA_OFFSET + i * 256) as *mut u32).write(0xFFFF_FFFF) };
        }
        let before = c.bad_frames();
        assert!(c.pop(&mut buf).is_none());
        assert_eq!(c.bad_frames() - before, 8);
        // A consumer claiming to be far ahead makes the ring look full to
        // the producer, never more than full.
        unsafe { (base.add(layout::OFF_READ) as *mut u64).write(u64::MAX) };
        assert!(p.free() <= 8);
    }

    #[test]
    fn attach_rejects_bad_headers() {
        let len = ring_bytes(8, 256);
        let mut mem = vec![0u64; len / 8];
        let base = mem.as_mut_ptr() as *mut u8;
        // SAFETY: host memory of `len` bytes.
        assert!(unsafe { Ring::attach_raw(base, len, Role::Consumer) }.is_err());
        let _ = unsafe { Ring::init_raw(base, len, 8, 256, Role::Producer) };
        unsafe { (base.add(layout::OFF_SLOTS) as *mut u32).write(1 << 20) };
        assert!(unsafe { Ring::attach_raw(base, len, Role::Consumer) }.is_err());
        unsafe { (base.add(layout::OFF_SLOTS) as *mut u32).write(8) };
        assert!(unsafe { Ring::attach_raw(base, len - 1, Role::Consumer) }.is_err());
        assert!(unsafe { Ring::attach_raw(base, len, Role::Consumer) }.is_ok());
    }

    #[test]
    fn wake_flags() {
        let (_mem, p, c) = host_ring(8, 256);
        c.set_flag(layout::flags::WAITING_DATA);
        assert!(p.take_flag(layout::flags::WAITING_DATA));
        assert!(!p.take_flag(layout::flags::WAITING_DATA));
        c.set_flag(layout::flags::WAITING_SPACE);
        c.clear_flag(layout::flags::WAITING_SPACE);
        assert!(!p.take_flag(layout::flags::WAITING_SPACE));
    }

    #[test]
    fn metadata_packing() {
        let rx = RxInfo { channel: 11, band: 1, signal_dbm: -67, noise_dbm: -95, rate: 108 };
        assert_eq!(RxInfo::unpack(rx.pack()), rx);
        let tx = TxInfo { id: 0xDEAD_BEEF, rate: 2, retries: 7, no_ack: true };
        assert_eq!(TxInfo::unpack(tx.pack()), tx);
        let st = TxStatus { id: 42, acked: true, attempts: 3 };
        assert_eq!(TxStatus::unpack(st.pack()), st);
    }
}
