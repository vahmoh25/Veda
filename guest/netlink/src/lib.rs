//! Netlink for the driver VM's programs: a socket of a netlink family,
//! messages built and parsed with their attributes, and generic netlink's
//! families (nl80211, mac80211_hwsim) found by name.
//!
//! One socket carries a program's requests and the events of the groups it
//! joined: [`Socket::request`] collects the replies to a request (until its
//! acknowledgement, or the end of a dump) and keeps the events that come
//! meanwhile for [`Socket::events`], in their order.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub const NETLINK_ROUTE: i32 = 0;
pub const NETLINK_GENERIC: i32 = 16;

const AF_NETLINK: i32 = 16;
const SOCK_RAW: i32 = 3;
const SOCK_NONBLOCK: i32 = 0o4000;
const SOCK_CLOEXEC: i32 = 0o2_000_000;
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;
const NETLINK_CAP_ACK: i32 = 10;
const NETLINK_EXT_ACK: i32 = 11;
const SOL_SOCKET: i32 = 1;
const SO_RCVBUFFORCE: i32 = 33;

pub const NLM_F_REQUEST: u16 = 1;
pub const NLM_F_MULTI: u16 = 2;
pub const NLM_F_ACK: u16 = 4;
pub const NLM_F_DUMP: u16 = 0x300;
const ENOBUFS: i32 = 105;
const NLMSG_ERROR: u16 = 2;
/// An error's flags: its request is cut to its header; attributes follow.
const NLM_F_CAPPED: u16 = 0x100;
const NLM_F_ACK_TLVS: u16 = 0x200;
const NLMSGERR_ATTR_MSG: u16 = 1;
const NLMSG_DONE: u16 = 3;
const NLA_TYPE_MASK: u16 = 0x3FFF;
const NLA_F_NESTED: u16 = 0x8000;

const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

const HEADER: usize = 16;
const GENL_HEADER: usize = 4;

fn align(n: usize) -> usize {
    n.next_multiple_of(4)
}

/// A generic netlink message being built: its header, then attributes.
pub struct Builder {
    buf: Vec<u8>,
    nests: Vec<usize>,
}

impl Builder {
    /// A message of generic netlink family `family`: command `cmd`.
    pub fn genl(family: u16, flags: u16, cmd: u8, version: u8) -> Builder {
        let mut buf = vec![0u8; HEADER + GENL_HEADER];
        buf[4..6].copy_from_slice(&family.to_ne_bytes());
        buf[6..8].copy_from_slice(&(flags | NLM_F_REQUEST).to_ne_bytes());
        buf[HEADER] = cmd;
        buf[HEADER + 1] = version;
        Builder { buf, nests: Vec::new() }
    }

    pub fn attr(mut self, kind: u16, payload: &[u8]) -> Builder {
        let len = 4 + payload.len();
        self.buf.extend_from_slice(&(len as u16).to_ne_bytes());
        self.buf.extend_from_slice(&kind.to_ne_bytes());
        self.buf.extend_from_slice(payload);
        self.buf.resize(align(self.buf.len()), 0);
        self
    }

    pub fn flag(self, kind: u16) -> Builder {
        self.attr(kind, &[])
    }

    pub fn u8(self, kind: u16, v: u8) -> Builder {
        self.attr(kind, &[v])
    }

    pub fn u16(self, kind: u16, v: u16) -> Builder {
        self.attr(kind, &v.to_ne_bytes())
    }

    pub fn u32(self, kind: u16, v: u32) -> Builder {
        self.attr(kind, &v.to_ne_bytes())
    }

    pub fn u64(self, kind: u16, v: u64) -> Builder {
        self.attr(kind, &v.to_ne_bytes())
    }

    /// A string, with its terminating NUL.
    pub fn string(self, kind: u16, s: &str) -> Builder {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.attr(kind, &v)
    }

    /// Starts a nested attribute; [`Builder::end`] ends it.
    pub fn nest(mut self, kind: u16) -> Builder {
        self.nests.push(self.buf.len());
        self.buf.extend_from_slice(&[0, 0]);
        self.buf.extend_from_slice(&(kind | NLA_F_NESTED).to_ne_bytes());
        self
    }

    pub fn end(mut self) -> Builder {
        if let Some(at) = self.nests.pop() {
            let len = (self.buf.len() - at) as u16;
            self.buf[at..at + 2].copy_from_slice(&len.to_ne_bytes());
        }
        self
    }

    fn with_flags(mut self, flags: u16) -> Builder {
        let f = u16::from_ne_bytes([self.buf[6], self.buf[7]]) | flags;
        self.buf[6..8].copy_from_slice(&f.to_ne_bytes());
        self
    }

    fn finish(mut self, seq: u32) -> Vec<u8> {
        let len = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&len.to_ne_bytes());
        self.buf[8..12].copy_from_slice(&seq.to_ne_bytes());
        self.buf
    }
}

/// The attributes of a message or a nested attribute.
#[derive(Clone, Copy)]
pub struct Attrs<'a>(pub &'a [u8]);

impl<'a> Attrs<'a> {
    /// The first attribute of `kind`.
    pub fn get(&self, kind: u16) -> Option<&'a [u8]> {
        self.iter().find(|(k, _)| *k == kind).map(|(_, v)| v)
    }

    pub fn has(&self, kind: u16) -> bool {
        self.get(kind).is_some()
    }

    pub fn nested(&self, kind: u16) -> Option<Attrs<'a>> {
        self.get(kind).map(Attrs)
    }

    pub fn u8(&self, kind: u16) -> Option<u8> {
        self.get(kind)?.first().copied()
    }

    pub fn u16(&self, kind: u16) -> Option<u16> {
        Some(u16::from_ne_bytes(self.get(kind)?.get(..2)?.try_into().ok()?))
    }

    pub fn u32(&self, kind: u16) -> Option<u32> {
        Some(u32::from_ne_bytes(self.get(kind)?.get(..4)?.try_into().ok()?))
    }

    pub fn u64(&self, kind: u16) -> Option<u64> {
        Some(u64::from_ne_bytes(self.get(kind)?.get(..8)?.try_into().ok()?))
    }

    pub fn string(&self, kind: u16) -> Option<String> {
        let v = self.get(kind)?;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        Some(String::from_utf8_lossy(&v[..end]).into_owned())
    }

    /// Every attribute: (kind, payload).
    pub fn iter(&self) -> impl Iterator<Item = (u16, &'a [u8])> + 'a {
        let mut rest = self.0;
        std::iter::from_fn(move || {
            if rest.len() < 4 {
                return None;
            }
            let len = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
            let kind = u16::from_ne_bytes([rest[2], rest[3]]) & NLA_TYPE_MASK;
            if len < 4 || len > rest.len() {
                return None;
            }
            let payload = &rest[4..len];
            rest = &rest[align(len).min(rest.len())..];
            Some((kind, payload))
        })
    }
}

/// A message received.
#[derive(Debug, Clone)]
pub struct Received {
    /// The family (generic netlink) or message type.
    pub kind: u16,
    pub flags: u16,
    pub seq: u32,
    /// Generic netlink's command.
    pub cmd: u8,
    /// The attributes.
    pub payload: Vec<u8>,
}

impl Received {
    pub fn attrs(&self) -> Attrs<'_> {
        Attrs(&self.payload)
    }
}

/// A netlink socket.
pub struct Socket {
    fd: OwnedFd,
    seq: u32,
    events: VecDeque<Received>,
    buf: Vec<u8>,
    /// Why the last request failed, as the kernel said.
    reason: Option<String>,
}

impl Socket {
    /// A socket of netlink `protocol`, not blocking.
    pub fn open(protocol: i32) -> io::Result<Socket> {
        let fd = guest_sys::socket(AF_NETLINK, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, protocol)?;
        // sockaddr_nl: family, pad, pid 0 (the kernel's choice), groups 0.
        let mut address = [0u8; 12];
        address[0..2].copy_from_slice(&(AF_NETLINK as u16).to_ne_bytes());
        guest_sys::bind(fd.as_raw_fd(), &address)?;
        // Errors with their reasons, without the request they answer.
        let _ = guest_sys::setsockopt(fd.as_raw_fd(), SOL_NETLINK, NETLINK_EXT_ACK, &1i32.to_ne_bytes());
        let _ = guest_sys::setsockopt(fd.as_raw_fd(), SOL_NETLINK, NETLINK_CAP_ACK, &1i32.to_ne_bytes());
        let _ = guest_sys::setsockopt(fd.as_raw_fd(), SOL_SOCKET, SO_RCVBUFFORCE, &(4i32 << 20).to_ne_bytes());
        Ok(Socket { fd, seq: 1, events: VecDeque::new(), buf: vec![0u8; 64 * 1024], reason: None })
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Receives the events of multicast group `group` from now on.
    pub fn join(&self, group: u32) -> io::Result<()> {
        guest_sys::setsockopt(self.fd(), SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &group.to_ne_bytes())
    }

    /// Sends `msg` without waiting for anything.
    pub fn send(&mut self, msg: Builder) -> io::Result<u32> {
        self.seq = self.seq.wrapping_add(1).max(1);
        let bytes = msg.finish(self.seq);
        let n = guest_sys::write(self.fd(), &bytes)?;
        if n != bytes.len() {
            return Err(io::Error::other("netlink message cut short"));
        }
        Ok(self.seq)
    }

    /// The messages of one datagram (none if there is none).
    fn receive(&mut self) -> io::Result<Vec<Received>> {
        let n = loop {
            match guest_sys::read(self.fd(), &mut self.buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Vec::new()),
                // The socket overflowed: events were lost, which a program
                // that waits for them finds by their absence.
                Err(e) if e.raw_os_error() == Some(ENOBUFS) => {}
                Err(e) => return Err(e),
            }
        };
        let mut out = Vec::new();
        let mut rest = &self.buf[..n];
        while rest.len() >= HEADER {
            let len = u32::from_ne_bytes(rest[0..4].try_into().unwrap()) as usize;
            if len < HEADER || len > rest.len() {
                break;
            }
            let kind = u16::from_ne_bytes([rest[4], rest[5]]);
            let flags = u16::from_ne_bytes([rest[6], rest[7]]);
            let seq = u32::from_ne_bytes(rest[8..12].try_into().unwrap());
            let body = &rest[HEADER..len];
            let (cmd, payload) = match kind {
                NLMSG_ERROR | NLMSG_DONE => (0, body.to_vec()),
                _ if body.len() >= GENL_HEADER => (body[0], body[GENL_HEADER..].to_vec()),
                _ => (0, body.to_vec()),
            };
            out.push(Received { kind, flags, seq, cmd, payload });
            rest = &rest[align(len).min(rest.len())..];
        }
        Ok(out)
    }

    /// Sends `msg` (asking for an acknowledgement) and returns its replies:
    /// the messages of a dump, or a reply before the acknowledgement. The
    /// events that come meanwhile are kept for [`Socket::events`]. When it
    /// fails, [`Socket::reason`] may say why.
    pub fn request(&mut self, msg: Builder) -> io::Result<Vec<Received>> {
        self.reason = None;
        let seq = self.send(msg.with_flags(NLM_F_ACK))?;
        let mut replies = Vec::new();
        loop {
            let batch = self.receive()?;
            if batch.is_empty() {
                // Nothing yet: wait for the kernel's answer (which it always
                // gives at once, but for a dump's next part).
                if guest_sys::poll(self.fd(), guest_sys::POLLIN, 5000)? == 0 {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                continue;
            }
            for m in batch {
                if m.seq != seq {
                    self.events.push_back(m);
                    continue;
                }
                match m.kind {
                    // The acknowledgement, or the end of a dump: an error
                    // is negative (a few requests answer a number, such as
                    // the index of what they made).
                    NLMSG_ERROR | NLMSG_DONE => {
                        let status = i32::from_ne_bytes(m.payload.get(0..4).unwrap_or(&[0; 4]).try_into().unwrap());
                        if status >= 0 {
                            return Ok(replies);
                        }
                        if m.kind == NLMSG_ERROR && m.flags & NLM_F_ACK_TLVS != 0 {
                            // After the request's header (all of the request,
                            // if not capped): the reason, among others.
                            let request =
                                u32::from_ne_bytes(m.payload.get(4..8).unwrap_or(&[0; 4]).try_into().unwrap());
                            let skip = 4 + if m.flags & NLM_F_CAPPED != 0 { HEADER } else { request as usize };
                            let tlvs = Attrs(m.payload.get(align(skip)..).unwrap_or(&[]));
                            self.reason = tlvs.string(NLMSGERR_ATTR_MSG);
                        }
                        return Err(io::Error::from_raw_os_error(-status));
                    }
                    _ => replies.push(m),
                }
            }
        }
    }

    /// The events received (kept while waiting for replies, then those
    /// waiting on the socket), in order.
    pub fn events(&mut self) -> io::Result<Vec<Received>> {
        loop {
            let batch = self.receive()?;
            if batch.is_empty() {
                break;
            }
            self.events.extend(batch.into_iter().filter(|m| m.kind != NLMSG_ERROR && m.kind != NLMSG_DONE));
        }
        Ok(self.events.drain(..).collect())
    }

    /// Whether events wait in the program (they would not wake a poll).
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// Why the last request failed, if the kernel said.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// A generic netlink family: its id, and its multicast groups.
#[derive(Debug, Clone)]
pub struct Family {
    pub id: u16,
    pub groups: Vec<(String, u32)>,
}

impl Family {
    /// Finds family `name` (`ENOENT` if the kernel has none).
    pub fn find(socket: &mut Socket, name: &str) -> io::Result<Family> {
        let replies = socket
            .request(Builder::genl(GENL_ID_CTRL, 0, CTRL_CMD_GETFAMILY, 1).string(CTRL_ATTR_FAMILY_NAME, name))?;
        let reply = replies.first().ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let a = reply.attrs();
        let id = a.u16(CTRL_ATTR_FAMILY_ID).ok_or_else(|| io::Error::other("no family id"))?;
        let groups = a
            .nested(CTRL_ATTR_MCAST_GROUPS)
            .map(|g| {
                g.iter()
                    .filter_map(|(_, entry)| {
                        let e = Attrs(entry);
                        Some((e.string(CTRL_ATTR_MCAST_GRP_NAME)?, e.u32(CTRL_ATTR_MCAST_GRP_ID)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Family { id, groups })
    }

    /// The id of multicast group `name`.
    pub fn group(&self, name: &str) -> Option<u32> {
        self.groups.iter().find(|(n, _)| n == name).map(|&(_, id)| id)
    }
}
