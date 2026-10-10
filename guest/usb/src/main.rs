//! `usbip` — the driver VM's host of the USB devices Veda lends
//! (`vproto::usb`): each is plugged into a port of Linux's USB/IP host
//! controller (`vhci_hcd`), and Linux's drivers drive it as a device on
//! the PC's own bus, while Veda's USB driver keeps the controller.
//!
//! vhci_hcd speaks USB/IP on a stream socket, as it does to a USB/IP server
//! across a network: here the socket's other end is this program, which
//! turns the URBs it reads into the lending protocol's requests and the
//! answers back into USB/IP's, the data in messages, a copy each way. A
//! device that goes away (unplugged, or Veda's driver gone) is unplugged
//! from its port; a device Linux lets go of (after an error) is given back,
//! and Veda resets it and lends it again. Isochronous transfers are not
//! lent yet: they fail.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};

use guest_sys::{POLLIN, PollFd, poll_many};
use vabi::signals;
use vipc::{Bytes, Decode};
use vproto::usb::{LEND, Lend, URB_ANSWER, URB_REQUEST, UrbAnswer, UrbRequest, UrbStatus, UsbDevice, UsbSpeed};
use vrt::object::Channel;

const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";
/// A vhci_hcd port's state when it is free (`VDEV_ST_NULL`).
const PORT_FREE: u32 = 4;

/// USB/IP's commands, directions and flags (`usbip_common.h`).
const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;
const DIR_IN: u32 = 1;
const URB_SHORT_NOT_OK: u32 = 0x1;
const URB_ZERO_PACKET: u32 = 0x40;
/// A header: the basic part, then the command's.
const HEADER: usize = 48;

/// Linux's errors, as URBs end with them.
const EPIPE: i32 = 32;
const EINVAL: i32 = 22;
const EPROTO: i32 = 71;
const EOVERFLOW: i32 = 75;
const ECONNRESET: i32 = 104;
const ESHUTDOWN: i32 = 108;
const EREMOTEIO: i32 = 121;

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const SOCK_CLOEXEC: i32 = 0o2_000_000;
const SYS_SOCKETPAIR: usize = 53;

/// The devices' numbers on their USB/IP connections.
static NEXT_DEVID: AtomicU32 = AtomicU32::new(1);

fn be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// A free port of vhci_hcd's for a device of `speed`: its number.
fn free_port(speed: UsbSpeed) -> Option<u32> {
    let hub = if matches!(speed, UsbSpeed::Super | UsbSpeed::SuperPlus) { "ss" } else { "hs" };
    let status = std::fs::read_to_string(format!("{VHCI}/status")).ok()?;
    status.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 3 && f[0] == hub && f[2].parse() == Ok(PORT_FREE)).then(|| f[1].parse().ok()).flatten()
    })
}

/// A connected pair of stream sockets.
fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: the kernel writes two descriptors into `fds`.
    unsafe {
        guest_sys::syscall(
            SYS_SOCKETPAIR,
            [AF_UNIX as usize, (SOCK_STREAM | SOCK_CLOEXEC) as usize, 0, fds.as_mut_ptr() as usize, 0],
        )?
    };
    // SAFETY: two new descriptors that nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// A transfer in flight: what its answer needs.
struct Submitted {
    is_in: bool,
    length: u32,
    short_not_ok: bool,
}

/// A device plugged into a port, while it lasts.
struct Plugged {
    socket: File,
    transfers: Channel,
    port: u32,
    name: String,
    submitted: BTreeMap<u32, Submitted>,
    /// Unlinks waiting for their answer: the transfer's, and the unlink's
    /// own number.
    unlinks: BTreeMap<u32, u32>,
    /// What came from the socket and is not a whole PDU yet.
    pending: Vec<u8>,
}

impl Plugged {
    /// Writes a PDU to vhci_hcd.
    fn send(&mut self, pdu: &[u8]) -> io::Result<()> {
        self.socket.write_all(pdu)
    }

    fn request(&self, r: UrbRequest) -> bool {
        vipc::send_event(&self.transfers, URB_REQUEST, r).is_ok()
    }

    /// The PDUs that came whole: their requests go to Veda. `false` once
    /// the device is gone.
    fn take_requests(&mut self) -> io::Result<bool> {
        loop {
            if self.pending.len() < HEADER {
                return Ok(true);
            }
            let h = &self.pending[..HEADER];
            let (command, seqnum, direction, ep) = (be(h, 0), be(h, 4), be(h, 12), be(h, 16));
            match command {
                CMD_SUBMIT => {
                    let (flags, length, packets) = (be(h, 20), be(h, 24), be(h, 32) as i32);
                    let is_in = direction == DIR_IN;
                    let mut setup = [0u8; 8];
                    setup.copy_from_slice(&h[40..48]);
                    let iso = if packets > 0 { packets as usize * 16 } else { 0 };
                    let out = if is_in { 0 } else { length as usize };
                    if self.pending.len() < HEADER + out + iso {
                        return Ok(true);
                    }
                    let data = self.pending[HEADER..HEADER + out].to_vec();
                    self.pending.drain(..HEADER + out + iso);
                    if packets > 0 {
                        // Isochronous: not lent yet.
                        self.send(&ret_submit(seqnum, -EINVAL, &[]))?;
                        continue;
                    }
                    let short_not_ok = flags & URB_SHORT_NOT_OK != 0;
                    self.submitted.insert(seqnum, Submitted { is_in, length, short_not_ok });
                    let endpoint = ep as u8 & 0x0F | if is_in { 0x80 } else { 0 };
                    let zero_packet = flags & URB_ZERO_PACKET != 0;
                    let submit =
                        UrbRequest::Submit { id: seqnum, endpoint, setup, length, data: Bytes(data), zero_packet };
                    if !self.request(submit) {
                        return Ok(false);
                    }
                }
                CMD_UNLINK => {
                    let target = be(h, 20);
                    self.pending.drain(..HEADER);
                    self.unlinks.insert(target, seqnum);
                    if !self.request(UrbRequest::Unlink { id: target }) {
                        return Ok(false);
                    }
                }
                other => return Err(io::Error::other(format!("USB/IP command {other} from vhci_hcd"))),
            }
        }
    }

    /// An answer from Veda: to vhci_hcd.
    fn take_answer(&mut self, answer: UrbAnswer) -> io::Result<()> {
        match answer {
            UrbAnswer::Done { id, status, actual, data } => {
                let Some(s) = self.submitted.remove(&id) else { return Ok(()) };
                let error = match status {
                    UrbStatus::Ok if s.is_in && s.short_not_ok && actual < s.length => -EREMOTEIO,
                    UrbStatus::Ok => 0,
                    UrbStatus::Stall => -EPIPE,
                    UrbStatus::Error => -EPROTO,
                    UrbStatus::Overflow => -EOVERFLOW,
                    UrbStatus::Cancelled => -ECONNRESET,
                    UrbStatus::Gone => -ESHUTDOWN,
                    UrbStatus::Unsupported => -EINVAL,
                };
                let mut pdu = ret_submit(id, error, if s.is_in { &data.0 } else { &[] });
                // The actual length, of what was sent too.
                pdu[24..28].copy_from_slice(&(actual as i32).to_be_bytes());
                self.send(&pdu)
            }
            UrbAnswer::Unlinked { id, cancelled } => {
                let Some(seqnum) = self.unlinks.remove(&id) else { return Ok(()) };
                if cancelled {
                    self.submitted.remove(&id);
                }
                let mut pdu = vec![0u8; HEADER];
                pdu[0..4].copy_from_slice(&RET_UNLINK.to_be_bytes());
                pdu[4..8].copy_from_slice(&seqnum.to_be_bytes());
                let status = if cancelled { -ECONNRESET } else { 0 };
                pdu[20..24].copy_from_slice(&status.to_be_bytes());
                self.send(&pdu)
            }
        }
    }
}

/// A RET_SUBMIT of transfer `seqnum`, which ended with `status` and
/// received `data`.
fn ret_submit(seqnum: u32, status: i32, data: &[u8]) -> Vec<u8> {
    let mut pdu = vec![0u8; HEADER];
    pdu[0..4].copy_from_slice(&RET_SUBMIT.to_be_bytes());
    pdu[4..8].copy_from_slice(&seqnum.to_be_bytes());
    pdu[20..24].copy_from_slice(&status.to_be_bytes());
    pdu[24..28].copy_from_slice(&(data.len() as i32).to_be_bytes());
    pdu.extend_from_slice(data);
    pdu
}

/// Plugs a lent device into a free port and says so to Veda: its thread
/// serves it from then on. A device that cannot be plugged in is refused.
fn plug(device: UsbDevice, transfers: Channel) {
    let name = if device.name.is_empty() {
        format!("{:04x}:{:04x}", device.vendor, device.product)
    } else {
        device.name.clone()
    };
    let socket = (|| {
        let port = free_port(device.speed).ok_or_else(|| format!("no free port of {:?} speed", device.speed))?;
        let (ours, theirs) = socketpair().map_err(|e| format!("no socket: {e}"))?;
        let devid = NEXT_DEVID.fetch_add(1, Ordering::Relaxed);
        let attach = format!("{port} {} {devid} {}", theirs.as_raw_fd(), device.speed as u32);
        std::fs::write(format!("{VHCI}/attach"), attach).map_err(|e| format!("port {port}: {e}"))?;
        // vhci_hcd holds its end now.
        Ok::<_, String>((port, ours))
    })();
    let (port, ours) = match socket {
        Ok(s) => s,
        Err(e) => {
            println!("usbip: {name}: cannot plug it in ({e})");
            let _ = vipc::send_event(&transfers, URB_REQUEST, UrbRequest::Refused {});
            return;
        }
    };
    println!("usbip: {name} ({:04x}:{:04x}, {}) on port {port}", device.vendor, device.product, device.location);
    if vipc::send_event(&transfers, URB_REQUEST, UrbRequest::Taken {}).is_err() {
        let _ = std::fs::write(format!("{VHCI}/detach"), port.to_string());
        return;
    }
    let plugged = Plugged {
        socket: File::from(ours),
        transfers,
        port,
        name,
        submitted: BTreeMap::new(),
        unlinks: BTreeMap::new(),
        pending: Vec::new(),
    };
    std::thread::spawn(move || serve(plugged));
}

/// Carries a device's transfers until it goes away.
fn serve(mut d: Plugged) {
    let ended = run(&mut d);
    // Veda's side gone: the device leaves its port; Linux's: the device goes
    // back to Veda (the channel closes with `d`).
    let _ = std::fs::write(format!("{VHCI}/detach"), d.port.to_string());
    match ended {
        Ok(()) => println!("usbip: {}: gone from port {}", d.name, d.port),
        Err(e) => println!("usbip: {}: let go of ({e})", d.name),
    }
}

fn run(d: &mut Plugged) -> io::Result<()> {
    let watch = vrt::guest::watch(d.transfers.raw(), signals::READABLE | signals::PEER_CLOSED)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut watch = File::from(watch);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Veda's answers.
        loop {
            match d.transfers.read() {
                Ok(mut msg) => {
                    if let Ok((h, mut dec)) = vipc::open(&mut msg)
                        && h.ordinal == URB_ANSWER
                        && let Ok(a) = UrbAnswer::decode(&mut dec)
                    {
                        d.take_answer(a)?;
                    }
                }
                Err(vabi::Error::ShouldWait) => break,
                Err(_) => return Ok(()),
            }
        }
        let mut fds = [PollFd::new(d.socket.as_raw_fd(), POLLIN), PollFd::new(watch.as_raw_fd(), POLLIN)];
        poll_many(&mut fds, -1)?;
        if fds[1].revents & POLLIN != 0 {
            // Taken: the next poll waits for the channel again.
            let _ = watch.read(&mut [0u8; 4]);
        }
        if fds[0].revents != 0 {
            let n = d.socket.read(&mut buf)?;
            if n == 0 {
                return Err(io::Error::other("its port was closed"));
            }
            d.pending.extend_from_slice(&buf[..n]);
            if !d.take_requests()? {
                return Ok(());
            }
        }
    }
}

fn main() {
    let listener = match vproto::register(vproto::usb::NAME) {
        Ok(l) => l,
        Err(e) => {
            println!("usbip: cannot provide the service: {e:?}");
            return;
        }
    };
    println!("usbip: ready for Veda's USB devices");
    loop {
        let _ = listener.wait(signals::READABLE | signals::PEER_CLOSED, vabi::DEADLINE_INFINITE);
        while let Some(connection) = vproto::accept(&listener) {
            // One event: a device lent (sent with the connection).
            let _ = connection.wait(signals::READABLE | signals::PEER_CLOSED, vrt::time::now_ns() + 2_000_000_000);
            match connection.read().map(vipc::decode_event::<Lend>) {
                Ok(Ok((LEND, lend))) => plug(lend.device, lend.transfers),
                Ok(_) => println!("usbip: a connection that lends nothing"),
                Err(e) => println!("usbip: nothing lent: {e:?}"),
            }
        }
    }
}
