//! Network interfaces, for the drivers that carry an interface's frames to
//! Veda: switching one on and off, and a raw packet socket on one.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

const AF_PACKET: i32 = 17;
const SOCK_RAW: i32 = 3;
const SOCK_NONBLOCK: i32 = 0o4000;
const SOCK_CLOEXEC: i32 = 0o2_000_000;
/// `ETH_P_ALL`, in network byte order.
const ALL_FRAMES: u16 = 0x0003u16.to_be();
const SOL_SOCKET: i32 = 1;
const SO_RCVBUFFORCE: i32 = 33;
const SOL_PACKET: i32 = 263;
const PACKET_ADD_MEMBERSHIP: i32 = 1;
const PACKET_MR_ALLMULTI: u16 = 2;
const PACKET_IGNORE_OUTGOING: i32 = 23;
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFFLAGS: u32 = 0x8914;
const IFF_UP: i16 = 1;

/// Switches interface `name` on (`up`) or off.
pub fn set_up(name: &str, up: bool) -> io::Result<()> {
    // A packet socket of no protocol, which receives nothing: for the
    // requests only.
    let socket = crate::socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, 0)?;
    let fd = socket.as_raw_fd();
    let mut request = [0u8; 40];
    let n = name.len().min(15);
    request[..n].copy_from_slice(&name.as_bytes()[..n]);
    // SAFETY: an ifreq, which the kernel reads and writes.
    unsafe { crate::ioctl(fd, SIOCGIFFLAGS, request.as_mut_ptr() as usize)? };
    let flags = i16::from_le_bytes([request[16], request[17]]);
    let flags = if up { flags | IFF_UP } else { flags & !IFF_UP };
    request[16..18].copy_from_slice(&flags.to_le_bytes());
    // SAFETY: as above.
    unsafe { crate::ioctl(fd, SIOCSIFFLAGS, request.as_mut_ptr() as usize)? };
    Ok(())
}

/// A raw packet socket on interface `index`, not blocking: every frame it
/// receives (multicast too, which IPv6 needs), none of those it sends.
pub fn packet_socket(index: i32) -> io::Result<File> {
    let socket = crate::socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, ALL_FRAMES as i32)?;
    let fd = socket.as_raw_fd();
    // sockaddr_ll: family, protocol, interface; the rest unused.
    let mut address = [0u8; 20];
    address[0..2].copy_from_slice(&(AF_PACKET as u16).to_le_bytes());
    address[2..4].copy_from_slice(&ALL_FRAMES.to_le_bytes());
    address[4..8].copy_from_slice(&index.to_le_bytes());
    crate::bind(fd, &address)?;
    crate::setsockopt(fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &1i32.to_le_bytes())?;
    // packet_mreq: interface, type; no address.
    let mut membership = [0u8; 16];
    membership[0..4].copy_from_slice(&index.to_le_bytes());
    membership[4..6].copy_from_slice(&PACKET_MR_ALLMULTI.to_le_bytes());
    crate::setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &membership)?;
    // Room for bursts while the program is not running.
    let _ = crate::setsockopt(fd, SOL_SOCKET, SO_RCVBUFFORCE, &(4i32 << 20).to_le_bytes());
    Ok(File::from(socket))
}
