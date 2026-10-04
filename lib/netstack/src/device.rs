//! The smoltcp device behind each interface: two frame queues. The
//! network service moves frames between these queues and the driver's
//! shared ring; tests connect two stacks' queues directly.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Most frames queued per direction; beyond that frames are dropped (as a
/// full hardware queue would).
pub const QUEUE_LIMIT: usize = 512;

/// A frame queue pair implementing smoltcp's `Device`.
pub struct QueueDevice {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: VecDeque<Vec<u8>>,
    medium: Medium,
    /// Largest frame (Ethernet: IP MTU + 14).
    mtu: usize,
    /// Transmitted frames come straight back (the loopback interface).
    loopback: bool,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

/// The largest received frame accepted (what a device link slot holds).
const MAX_RX_FRAME: usize = 16384;

impl QueueDevice {
    pub fn new(medium: Medium, mtu: usize, loopback: bool) -> QueueDevice {
        QueueDevice { rx: VecDeque::new(), tx: VecDeque::new(), medium, mtu, loopback, rx_dropped: 0, tx_dropped: 0 }
    }

    /// Queues a received frame. Returns `false` if it was dropped.
    ///
    /// Frames may exceed the MTU: a hypervisor bridged to a host whose
    /// network adapter merges TCP segments (receive segment coalescing)
    /// passes on frames several times the MTU, valid in every other way.
    pub fn push_rx(&mut self, frame: &[u8]) -> bool {
        if self.rx.len() >= QUEUE_LIMIT || frame.len() > self.mtu.max(MAX_RX_FRAME) {
            self.rx_dropped += 1;
            return false;
        }
        self.rx.push_back(frame.to_vec());
        true
    }
}

pub struct RxToken(Vec<u8>);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

pub struct TxToken<'a>(&'a mut QueueDevice);

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = alloc::vec![0u8; len];
        let r = f(&mut frame);
        let dev = self.0;
        let queue = if dev.loopback { &mut dev.rx } else { &mut dev.tx };
        if queue.len() < QUEUE_LIMIT {
            queue.push_back(frame);
        } else {
            dev.tx_dropped += 1;
        }
        r
    }
}

impl phy::Device for QueueDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxToken(frame), TxToken(self)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        let queue = if self.loopback { &self.rx } else { &self.tx };
        if queue.len() >= QUEUE_LIMIT { None } else { Some(TxToken(self)) }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = self.medium;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = None;
        let mut checksum = ChecksumCapabilities::default();
        if self.loopback {
            // Loopback frames never leave memory: skip checksums both ways.
            checksum = ChecksumCapabilities::ignored();
        } else {
            checksum.ipv4 = Checksum::Both;
            checksum.udp = Checksum::Both;
            checksum.tcp = Checksum::Both;
            checksum.icmpv4 = Checksum::Both;
            checksum.icmpv6 = Checksum::Both;
        }
        caps.checksum = checksum;
        caps
    }
}
