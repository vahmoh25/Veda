//! Reading, writing and controlling descriptors.

use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE, WRITABLE};
use vabi::{WaitItem, startup};
use vrt::object::Socket;

use crate::error::{self, SysResult};
use crate::fd::{self, Description, Object};
use crate::linux::errno::*;
use crate::linux::{self, Flock, Iovec, Pollfd, fcntl, ioctl, o, poll};
use crate::{time, tty, user};

/// Reads into `buf` from `desc` (at `offset` for `pread`).
fn read_desc(desc: &Description, buf: &mut [u8], offset: Option<u64>) -> SysResult {
    if !desc.readable() {
        return Err(EBADF);
    }
    match (&desc.object, offset) {
        (Object::File(f), None) => f.read(buf),
        (Object::File(f), Some(at)) => f.pread(buf, at),
        (Object::Stream(_) | Object::Null | Object::Log, Some(_)) => Err(ESPIPE),
        (Object::Stream(s), None) => s.read(buf, desc.nonblocking()),
        (Object::Dir(_), _) => Err(EISDIR),
        (Object::Null | Object::Log, None) => Ok(0),
        // No events to read (Linux's are for displays).
        (Object::Drm(_) | Object::Dmabuf(_), _) => Err(EINVAL),
    }
}

/// Writes `data` to `desc` (at `offset` for `pwrite`).
fn write_desc(desc: &Description, data: &[u8], offset: Option<u64>) -> SysResult {
    if !desc.writable() {
        return Err(EBADF);
    }
    match (&desc.object, offset) {
        (Object::File(f), None) => f.write(data),
        (Object::File(f), Some(at)) => f.pwrite(data, at),
        (Object::Stream(_) | Object::Null | Object::Log, Some(_)) => Err(ESPIPE),
        (Object::Stream(s), None) => s.write(data, desc.nonblocking()),
        (Object::Dir(_), _) => Err(EISDIR),
        (Object::Null, None) => Ok(data.len()),
        (Object::Log, None) => {
            vrt::sys::debug_write(data);
            Ok(data.len())
        }
        (Object::Drm(_) | Object::Dmabuf(_), _) => Err(EINVAL),
    }
}

pub unsafe fn read(fd: i32, buf: usize, len: usize) -> SysResult {
    let desc = fd::get(fd)?;
    // SAFETY: the program passed a buffer of `len` bytes.
    read_desc(&desc, unsafe { user::slice_mut(buf, len)? }, None)
}

pub unsafe fn write(fd: i32, buf: usize, len: usize) -> SysResult {
    let desc = fd::get(fd)?;
    // SAFETY: as above.
    write_desc(&desc, unsafe { user::slice(buf, len)? }, None)
}

pub unsafe fn pread(fd: i32, buf: usize, len: usize, offset: i64) -> SysResult {
    let desc = fd::get(fd)?;
    let at = u64::try_from(offset).map_err(|_| EINVAL)?;
    // SAFETY: as above.
    read_desc(&desc, unsafe { user::slice_mut(buf, len)? }, Some(at))
}

pub unsafe fn pwrite(fd: i32, buf: usize, len: usize, offset: i64) -> SysResult {
    let desc = fd::get(fd)?;
    let at = u64::try_from(offset).map_err(|_| EINVAL)?;
    // SAFETY: as above.
    write_desc(&desc, unsafe { user::slice(buf, len)? }, Some(at))
}

/// The buffers of an `iovec` array.
///
/// # Safety
/// `iov` must be where the program keeps `count` iovecs.
pub(crate) unsafe fn iovecs(iov: usize, count: usize) -> Result<Vec<Iovec>, isize> {
    if count > 1024 {
        return Err(EINVAL);
    }
    // SAFETY: per the caller.
    (0..count).map(|i| unsafe { user::read::<Iovec>(iov + i * size_of::<Iovec>()) }).collect()
}

/// The bytes of the program's buffers `vecs`, one after another.
///
/// # Safety
/// `vecs` must describe buffers of the program's.
pub(crate) unsafe fn gather(vecs: &[Iovec]) -> Result<Vec<u8>, isize> {
    let mut all = Vec::new();
    for v in vecs {
        // SAFETY: per the caller.
        all.extend_from_slice(unsafe { user::slice(v.base as usize, v.len)? });
    }
    Ok(all)
}

/// Reads from `desc` into the program's buffers `vecs` in turn (`readv`,
/// `recvmsg`). From a stream as Linux reads a pipe, waiting (unless
/// `nonblock`) only while nothing has come, and from the terminal in
/// canonical mode one line at most. An error after some bytes came ends it
/// with those.
///
/// # Safety
/// `vecs` must describe buffers of the program's.
pub(crate) unsafe fn read_vectored(
    desc: &Description,
    vecs: &[Iovec],
    mut at: Option<u64>,
    nonblock: bool,
) -> SysResult {
    if !desc.readable() {
        return Err(EBADF);
    }
    let lines = matches!(&desc.object, Object::Stream(s) if s.reads_lines());
    let mut total = 0;
    for v in vecs {
        if v.len == 0 {
            continue;
        }
        // SAFETY: per the caller.
        let buf = unsafe { user::slice_mut(v.base as usize, v.len)? };
        let read = match &desc.object {
            // After the first buffer, only what is there already.
            Object::Stream(s) if at.is_none() => s.read(buf, nonblock || total > 0),
            _ => read_desc(desc, buf, at),
        };
        let n = match read {
            Ok(n) => n,
            Err(_) if total > 0 => break,
            Err(e) => return Err(e),
        };
        total += n;
        at = at.map(|a| a + n as u64);
        if n < v.len || (lines && buf[n - 1] == b'\n') {
            break;
        }
    }
    Ok(total)
}

pub unsafe fn readv(fd: i32, iov: usize, count: usize, offset: Option<i64>) -> SysResult {
    let desc = fd::get(fd)?;
    let at = offset.map(|o| u64::try_from(o).map_err(|_| EINVAL)).transpose()?;
    // SAFETY: the program passed `count` iovecs, each a buffer of its own.
    unsafe { read_vectored(&desc, &iovecs(iov, count)?, at, desc.nonblocking()) }
}

pub unsafe fn writev(fd: i32, iov: usize, count: usize, offset: Option<i64>) -> SysResult {
    let desc = fd::get(fd)?;
    let mut at = offset.map(|o| u64::try_from(o).map_err(|_| EINVAL)).transpose()?;
    // SAFETY: as above.
    let vecs = unsafe { iovecs(iov, count)? };
    // One write for streams, so that the pieces stay together.
    if matches!(desc.object, Object::Stream(_) | Object::Log) && vecs.len() > 1 {
        // SAFETY: as above.
        return write_desc(&desc, &unsafe { gather(&vecs)? }, at);
    }
    let mut total = 0;
    for v in vecs {
        // SAFETY: as above.
        let buf = unsafe { user::slice(v.base as usize, v.len)? };
        let n = match write_desc(&desc, buf, at) {
            Ok(n) => n,
            // What was written counts.
            Err(_) if total > 0 => break,
            Err(e) => return Err(e),
        };
        total += n;
        at = at.map(|a| a + n as u64);
        if n < v.len {
            break;
        }
    }
    Ok(total)
}

pub fn lseek(fd: i32, offset: i64, whence: u32) -> SysResult {
    let desc = fd::get(fd)?;
    match &desc.object {
        Object::File(f) => f.seek(offset, whence).map(|o| o as usize),
        Object::Dir(d) => d.seek(offset, whence).map(|o| o as usize),
        Object::Stream(_) => Err(ESPIPE),
        Object::Null | Object::Log | Object::Drm(_) => Ok(0),
        // Its size, from the end (how Mesa learns it), as Linux has it.
        Object::Dmabuf(vmo) => match whence {
            linux::seek::END => Ok(vmo.size().map_err(error::kernel)?),
            _ => Ok(0),
        },
    }
}

pub fn close(fd: i32) -> SysResult {
    fd::remove(fd).map(|_| 0)
}

pub fn dup(fd: i32) -> SysResult {
    let desc = fd::get(fd)?;
    fd::insert(desc, false, 0).map(|n| n as usize)
}

pub fn dup3(old: i32, new: i32, flags: u32, dup2: bool) -> SysResult {
    if flags & !o::CLOEXEC != 0 {
        return Err(EINVAL);
    }
    let desc = fd::get(old)?;
    if old == new {
        return if dup2 { Ok(new as usize) } else { Err(EINVAL) };
    }
    fd::replace(new, desc, flags & o::CLOEXEC != 0)?;
    Ok(new as usize)
}

pub fn close_range(first: u32, last: u32, flags: u32) -> SysResult {
    const CLOEXEC: u32 = 4;
    if first > last || flags & !CLOEXEC != 0 {
        return Err(EINVAL);
    }
    if flags & CLOEXEC != 0 {
        fd::set_cloexec_range(first as usize, last as usize);
    } else {
        fd::remove_range(first as usize, last as usize);
    }
    Ok(0)
}

pub fn pipe2(fds: usize, flags: u32) -> SysResult {
    if flags & !(o::CLOEXEC | o::NONBLOCK) != 0 {
        return Err(EINVAL);
    }
    let (a, b) = Socket::create().map_err(error::kernel)?;
    let status = flags & o::NONBLOCK;
    // A pipe: one end only reads, the other only writes.
    let reader = Description::new(Object::Stream(crate::stream::Stream::new(a)), o::RDONLY | status);
    let writer = Description::new(Object::Stream(crate::stream::Stream::new(b)), o::WRONLY | status);
    let cloexec = flags & o::CLOEXEC != 0;
    let r = fd::insert(reader, cloexec, 0)?;
    let w = match fd::insert(writer, cloexec, 0) {
        Ok(w) => w,
        Err(e) => {
            let _ = fd::remove(r);
            return Err(e);
        }
    };
    // SAFETY: the program passed an `int[2]`.
    unsafe { user::write::<[i32; 2]>(fds, [r, w])? };
    Ok(0)
}

pub unsafe fn fcntl(fd: i32, cmd: u32, arg: usize) -> SysResult {
    match cmd {
        fcntl::DUPFD | fcntl::DUPFD_CLOEXEC => {
            let desc = fd::get(fd)?;
            fd::insert(desc, cmd == fcntl::DUPFD_CLOEXEC, arg).map(|n| n as usize)
        }
        // Whether `arg` refers to the same open file description.
        fcntl::DUPFD_QUERY => Ok(Arc::ptr_eq(&fd::get(fd)?, &fd::get(arg as i32)?) as usize),
        fcntl::GETFD => Ok(if fd::cloexec(fd)? { fcntl::FD_CLOEXEC as usize } else { 0 }),
        fcntl::SETFD => fd::set_cloexec(fd, arg as u32 & fcntl::FD_CLOEXEC != 0).map(|_| 0),
        fcntl::GETFL => Ok(fd::get(fd)?.flags() as usize),
        fcntl::SETFL => {
            let desc = fd::get(fd)?;
            let append = arg as u32 & o::APPEND != 0;
            if let Object::File(f) = &desc.object
                && append != (desc.flags() & o::APPEND != 0)
            {
                f.set_append(append)?;
            }
            desc.set_status(arg as u32);
            Ok(0)
        }
        // There are no locks between processes: every lock is granted.
        fcntl::GETLK => {
            fd::get(fd)?;
            // SAFETY: the program passed a `struct flock`.
            let mut l: Flock = unsafe { user::read(arg)? };
            l.kind = fcntl::F_UNLCK;
            // SAFETY: as above.
            unsafe { user::write(arg, l)? };
            Ok(0)
        }
        fcntl::SETLK | fcntl::SETLKW => fd::get(fd).map(|_| 0),
        fcntl::GETOWN | fcntl::SETOWN => Ok(0),
        _ => Err(EINVAL),
    }
}

pub unsafe fn ioctl(fd: i32, request: u32, arg: usize) -> SysResult {
    let desc = fd::get(fd)?;
    match request {
        ioctl::FIONBIO => {
            // SAFETY: the program passed an `int`.
            let on: i32 = unsafe { user::read(arg)? };
            let flags = if on != 0 { desc.flags() | o::NONBLOCK } else { desc.flags() & !o::NONBLOCK };
            desc.set_status(flags);
            Ok(0)
        }
        ioctl::FIONREAD => {
            let n = match &desc.object {
                Object::Stream(s) => s.available(),
                Object::File(f) => {
                    let size = f.stat()?.size;
                    let at = f.seek(0, linux::seek::CUR)?;
                    size.saturating_sub(at) as usize
                }
                _ => 0,
            };
            // SAFETY: the program passed an `int`.
            unsafe { user::write(arg, n.min(i32::MAX as usize) as i32)? };
            Ok(0)
        }
        ioctl::FIOCLEX => fd::set_cloexec(fd, true).map(|_| 0),
        ioctl::FIONCLEX => fd::set_cloexec(fd, false).map(|_| 0),
        _ => match &desc.object {
            // SAFETY: per the request.
            Object::Drm(drm) => unsafe { drm.ioctl(request, arg) },
            // SAFETY: as above.
            _ => unsafe { tty::ioctl(fd, request, arg) },
        },
    }
}

pub fn ftruncate(fd: i32, len: i64) -> SysResult {
    let desc = fd::get(fd)?;
    let len = u64::try_from(len).map_err(|_| EINVAL)?;
    match &desc.object {
        Object::File(f) if desc.writable() => f.truncate(len).map(|_| 0),
        Object::Dir(_) => Err(EISDIR),
        _ => Err(EINVAL),
    }
}

/// What `poll` reports for one descriptor right now, and how to wait for
/// more if it is a stream.
fn poll_one(p: &Pollfd) -> (i16, Option<(Arc<Description>, u32)>) {
    let Ok(desc) = fd::get(p.fd) else { return (poll::NVAL, None) };
    let want_in = p.events & (poll::IN | poll::RDNORM) != 0;
    let want_out = p.events & (poll::OUT | poll::WRNORM) != 0;
    match &desc.object {
        Object::Stream(s) => {
            let (readable, hup, writable) = s.readiness();
            let mut r = 0;
            if want_in && readable {
                r |= p.events & (poll::IN | poll::RDNORM);
            }
            if want_out && writable && !hup {
                r |= p.events & (poll::OUT | poll::WRNORM);
            }
            if hup {
                r |= if desc.writable() && !desc.readable() { poll::ERR } else { poll::HUP };
            }
            let mut signals = PEER_CLOSED | PEER_WRITE_DISABLED;
            if want_in {
                signals |= READABLE;
            }
            if want_out {
                signals |= WRITABLE;
            }
            (r, Some((desc.clone(), signals)))
        }
        // Files and the rest never make a reader or writer wait.
        _ => (p.events & (poll::IN | poll::RDNORM | poll::OUT | poll::WRNORM), None),
    }
}

/// `poll`: waits until a descriptor is ready or `deadline` (monotonic ns,
/// `None` = forever) passes.
pub unsafe fn poll(fds: usize, count: usize, deadline: Option<u64>) -> SysResult {
    if count > fd::MAX_FDS {
        return Err(EINVAL);
    }
    let read_all = || -> Result<Vec<Pollfd>, isize> {
        // SAFETY: the program passed `count` pollfds.
        (0..count).map(|i| unsafe { user::read::<Pollfd>(fds + i * size_of::<Pollfd>()) }).collect()
    };
    let mut pfds = read_all()?;
    loop {
        let mut ready = 0;
        let mut waits: Vec<(Arc<Description>, u32)> = Vec::new();
        for p in pfds.iter_mut() {
            if p.fd < 0 {
                p.revents = 0;
                continue;
            }
            let (r, wait) = poll_one(p);
            p.revents = r;
            if r != 0 {
                ready += 1;
            }
            waits.extend(wait);
        }
        let now = time::monotonic_ns();
        if ready > 0 || deadline.is_some_and(|d| now >= d) {
            for (i, p) in pfds.iter().enumerate() {
                // SAFETY: as above.
                unsafe { user::write(fds + i * size_of::<Pollfd>(), *p)? };
            }
            return Ok(ready);
        }
        if waits.is_empty() {
            // Nothing can become ready: just wait out the time.
            match deadline {
                Some(d) => vrt::time::sleep_until(d),
                None => crate::signal::pause_forever(),
            }
            continue;
        }
        let mut items: Vec<WaitItem> = waits
            .iter()
            .filter_map(|(d, sig)| match &d.object {
                Object::Stream(s) => {
                    Some(WaitItem { handle: s.socket.raw(), signals: *sig, observed: 0, _reserved: 0 })
                }
                _ => None,
            })
            .collect();
        match vrt::object::wait_many(&mut items, deadline.unwrap_or(vabi::DEADLINE_INFINITE)) {
            Ok(_) | Err(vabi::Error::TimedOut) => {}
            Err(e) => return Err(error::kernel(e)),
        }
        pfds = read_all()?;
    }
}

/// `select`/`pselect6` through `poll`.
pub unsafe fn select(n: i32, readfds: usize, writefds: usize, exceptfds: usize, deadline: Option<u64>) -> SysResult {
    let n = usize::try_from(n).map_err(|_| EINVAL)?.min(fd::MAX_FDS);
    let words = n.div_ceil(64);
    // SAFETY: the program passed fd_sets of at least `n` bits (or nulls).
    let load = |p: usize| -> Result<Vec<u64>, isize> {
        if p == 0 {
            return Ok(alloc::vec![0; words]);
        }
        (0..words).map(|i| unsafe { user::read::<u64>(p + i * 8) }).collect()
    };
    let (r, w, e) = (load(readfds)?, load(writefds)?, load(exceptfds)?);
    let has = |set: &[u64], i: usize| set[i / 64] & (1 << (i % 64)) != 0;
    let mut pfds = Vec::new();
    for i in 0..n {
        let mut events = 0;
        if has(&r, i) {
            events |= poll::IN;
        }
        if has(&w, i) {
            events |= poll::OUT;
        }
        if has(&e, i) {
            events |= poll::PRI;
        }
        if events != 0 {
            pfds.push(Pollfd { fd: i as i32, events, revents: 0 });
        }
    }
    // Poll on a copy in our own memory.
    // SAFETY: `pfds` is a live array of pollfds.
    let ready = unsafe { poll(pfds.as_mut_ptr() as usize, pfds.len(), deadline)? };
    let _ = ready;
    if pfds.iter().any(|p| p.revents & poll::NVAL != 0) {
        return Err(EBADF);
    }
    let mut out = (alloc::vec![0u64; words], alloc::vec![0u64; words], alloc::vec![0u64; words]);
    let mut count = 0;
    for p in &pfds {
        let i = p.fd as usize;
        let bit = 1u64 << (i % 64);
        if p.revents & (poll::IN | poll::HUP | poll::ERR) != 0 && has(&r, i) {
            out.0[i / 64] |= bit;
            count += 1;
        }
        if p.revents & (poll::OUT | poll::ERR) != 0 && has(&w, i) {
            out.1[i / 64] |= bit;
            count += 1;
        }
        if p.revents & poll::PRI != 0 && has(&e, i) {
            out.2[i / 64] |= bit;
            count += 1;
        }
    }
    for (p, set) in [(readfds, &out.0), (writefds, &out.1), (exceptfds, &out.2)] {
        if p != 0 {
            for (i, word) in set.iter().enumerate() {
                // SAFETY: as above.
                unsafe { user::write(p + i * 8, *word)? };
            }
        }
    }
    Ok(count)
}

/// Installs the descriptors a process was started with: role `FD + n`
/// handles are sockets (pipes, the terminal) or `file` connections.
pub fn adopt_startup_fds() {
    for role in vrt::env::handle_roles() {
        let Some(n) = startup::role::fd(role) else { continue };
        let Some(h) = vrt::env::take_handle(role) else { continue };
        let Ok(info) = h.basic_info() else { continue };
        let desc = if info.object_type == vabi::ObjectType::Socket as u32 {
            // Its access is what the handle allows.
            let mode = match (info.rights & vabi::Rights::READ.0 != 0, info.rights & vabi::Rights::WRITE.0 != 0) {
                (true, true) => o::RDWR,
                (false, true) => o::WRONLY,
                _ => o::RDONLY,
            };
            Description::new(Object::Stream(crate::stream::Stream::new(Socket::from_handle(h))), mode)
        } else if info.object_type == vabi::ObjectType::Channel as u32 {
            let f = crate::file::VfsFile::new(vrt::object::Channel::from_handle(h));
            let mode = f.open_flags().map(crate::file::access_mode).unwrap_or(o::RDWR);
            let append = f.open_flags().is_ok_and(|fl| fl & vproto::fs::open_flags::APPEND != 0);
            Description::new(Object::File(f), mode | if append { o::APPEND } else { 0 })
        } else {
            continue;
        };
        let _ = fd::replace(n as i32, desc, false);
    }
    // Standard streams nobody gave: input is empty, output goes to the
    // kernel log.
    for (n, object) in [(0, Object::Null), (1, Object::Log), (2, Object::Log)] {
        if fd::get(n).is_err() {
            let _ = fd::replace(n, Description::new(object, o::RDWR), false);
        }
    }
}
