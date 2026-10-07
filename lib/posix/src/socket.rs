//! Sockets: `socketpair`, whose ends are kernel sockets as pipes are, and
//! what works on them: `send` and `recv` and their message forms, and
//! `shutdown`. There are no network sockets yet: `socket` fails with
//! `EAFNOSUPPORT`.

use vrt::object::Socket;

use crate::error::{self, SysResult};
use crate::fd::{self, Description, Object};
use crate::io;
use crate::linux::errno::*;
use crate::linux::{Msghdr, msg, o};
use crate::stream::Stream;
use crate::user;

/// `socketpair(AF_UNIX, SOCK_STREAM, ...)`: a connected pair.
///
/// # Safety
/// `out` must be where the program wants the two descriptors.
pub unsafe fn socketpair(domain: usize, kind: usize, out: usize) -> SysResult {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    const SOCK_NONBLOCK: usize = o::NONBLOCK as usize;
    const SOCK_CLOEXEC: usize = o::CLOEXEC as usize;
    if domain != AF_UNIX || kind & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != SOCK_STREAM {
        return Err(EAFNOSUPPORT);
    }
    let (a, b) = Socket::create().map_err(error::kernel)?;
    let status = o::RDWR | (kind & SOCK_NONBLOCK) as u32;
    let cloexec = kind & SOCK_CLOEXEC != 0;
    let x = fd::insert(Description::new(Object::Stream(Stream::new(a)), status), cloexec, 0)?;
    let y = match fd::insert(Description::new(Object::Stream(Stream::new(b)), status), cloexec, 0) {
        Ok(y) => y,
        Err(e) => {
            let _ = fd::remove(x);
            return Err(e);
        }
    };
    // SAFETY: per the caller.
    unsafe { user::write::<[i32; 2]>(out, [x, y])? };
    Ok(0)
}

/// The stream `fd` is, with what `flags` ask of a call on it.
fn stream(desc: &Description, flags: u32) -> Result<(&Stream, bool), isize> {
    let Object::Stream(s) = &desc.object else { return Err(ENOTSOCK) };
    // Out-of-band data does not exist on these streams, and bytes cannot be
    // looked at without taking them.
    if flags & (msg::OOB | msg::PEEK) != 0 {
        return Err(EOPNOTSUPP);
    }
    Ok((s, desc.nonblocking() || flags & msg::DONTWAIT != 0))
}

/// `sendto` (and `send`): the ends are connected, so there is no address.
///
/// # Safety
/// `buf` must be a buffer of `len` bytes of the program's.
pub unsafe fn sendto(fd: i32, buf: usize, len: usize, flags: u32, addr: usize) -> SysResult {
    let desc = fd::get(fd)?;
    let (s, nonblock) = stream(&desc, flags)?;
    if addr != 0 {
        return Err(EISCONN);
    }
    if !desc.writable() {
        return Err(EBADF);
    }
    // SAFETY: per the caller.
    s.send(unsafe { user::slice(buf, len)? }, nonblock, flags & msg::NOSIGNAL == 0)
}

/// `recvfrom` (and `recv`). The peer has no address: none is stored.
///
/// # Safety
/// `buf` must be a buffer of `len` bytes of the program's; `addrlen`, if
/// not 0, where it wants the address's length.
pub unsafe fn recvfrom(fd: i32, buf: usize, len: usize, flags: u32, addrlen: usize) -> SysResult {
    let desc = fd::get(fd)?;
    let (s, nonblock) = stream(&desc, flags)?;
    if !desc.readable() {
        return Err(EBADF);
    }
    // SAFETY: per the caller.
    let buf = unsafe { user::slice_mut(buf, len)? };
    let mut got = s.read(buf, nonblock)?;
    if flags & msg::WAITALL != 0 && !nonblock {
        // All of it, unless the stream ends (or fails) first.
        while got > 0 && got < buf.len() {
            match s.read(&mut buf[got..], false) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
        }
    }
    if addrlen != 0 {
        // SAFETY: per the caller.
        unsafe { user::write(addrlen, 0u32)? };
    }
    Ok(got)
}

/// `sendmsg`: the buffers in one piece; no address, no ancillary data
/// (descriptors cannot be sent yet).
///
/// # Safety
/// `message` must be the program's `struct msghdr`.
pub unsafe fn sendmsg(fd: i32, message: usize, flags: u32) -> SysResult {
    let desc = fd::get(fd)?;
    let (s, nonblock) = stream(&desc, flags)?;
    // SAFETY: per the caller.
    let m: Msghdr = unsafe { user::read(message)? };
    if m.name != 0 {
        return Err(EISCONN);
    }
    if m.controllen != 0 {
        return Err(EOPNOTSUPP);
    }
    if !desc.writable() {
        return Err(EBADF);
    }
    // SAFETY: the message's buffers are the program's.
    let data = unsafe { io::gather(&io::iovecs(m.iov, m.iovlen)?)? };
    s.send(&data, nonblock, flags & msg::NOSIGNAL == 0)
}

/// `recvmsg`: into the buffers in turn, as `readv`; no address, no
/// ancillary data.
///
/// # Safety
/// `message` must be the program's `struct msghdr`.
pub unsafe fn recvmsg(fd: i32, message: usize, flags: u32) -> SysResult {
    let desc = fd::get(fd)?;
    let (_, nonblock) = stream(&desc, flags)?;
    // SAFETY: per the caller.
    let mut m: Msghdr = unsafe { user::read(message)? };
    // SAFETY: the message's buffers are the program's.
    let got = unsafe { io::read_vectored(&desc, &io::iovecs(m.iov, m.iovlen)?, None, nonblock)? };
    m.namelen = 0;
    m.controllen = 0;
    m.flags = 0;
    // SAFETY: per the caller.
    unsafe { user::write(message, m)? };
    Ok(got)
}

/// `shutdown`: this end writes no more (`SHUT_WR`, `SHUT_RDWR`); the peer
/// reads to the end of what was sent. Stopping reading (`SHUT_RD`) changes
/// nothing here: what the peer sends can still be read.
pub fn shutdown(fd: i32, how: u32) -> SysResult {
    const SHUT_RD: u32 = 0;
    const SHUT_WR: u32 = 1;
    const SHUT_RDWR: u32 = 2;
    let desc = fd::get(fd)?;
    let Object::Stream(s) = &desc.object else { return Err(ENOTSOCK) };
    match how {
        SHUT_RD => Ok(0),
        SHUT_WR | SHUT_RDWR => s.socket.shutdown().map(|()| 0).map_err(error::kernel),
        _ => Err(EINVAL),
    }
}
