//! Byte streams: pipes and the terminal, both socket endpoints.

use vabi::Error;
use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE, WRITABLE};
use vrt::object::Socket;

use crate::linux::errno::{EAGAIN, EPIPE};
use crate::{error, signal, tty};

pub struct Stream {
    pub socket: Socket,
    /// The endpoint's kernel object id (the inode number of the pipe).
    pub koid: u64,
}

impl Stream {
    pub fn new(socket: Socket) -> Stream {
        let koid = socket.0.koid();
        Stream { socket, koid }
    }

    /// Whether this is the terminal.
    pub fn is_tty(&self) -> bool {
        tty::socket_koid() == Some(self.koid)
    }

    /// Reads what is there (waiting for something unless `nonblock`); 0 at
    /// the end of the stream. From the terminal, the read ends as its modes
    /// say: in canonical mode at the end of a line, as on a Unix terminal.
    pub fn read(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, isize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_tty() {
            match tty::input() {
                tty::Input::Lines => return self.read_line(buf, nonblock),
                tty::Input::Bytes { min, time } if !nonblock => return self.read_timed(buf, min, time),
                tty::Input::Bytes { .. } => {}
            }
        }
        loop {
            match self.socket.read(buf) {
                Ok(n) => return Ok(n),
                Err(Error::PeerClosed) => return Ok(0),
                Err(Error::ShouldWait) if nonblock => return Err(EAGAIN),
                Err(Error::ShouldWait) => self.wait_readable(vabi::DEADLINE_INFINITE)?,
                Err(e) => return Err(error::kernel(e)),
            }
        }
    }

    /// Whether a read ends at the end of a line (the terminal in canonical
    /// mode).
    pub fn reads_lines(&self) -> bool {
        self.is_tty() && matches!(tty::input(), tty::Input::Lines)
    }

    /// Canonical input: up to the end of the line and not a byte further,
    /// which stays for the next read, by this program or another (the
    /// Terminal sends each line once it has been edited). A byte at a time:
    /// people type slowly.
    fn read_line(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, isize> {
        let mut got = 0;
        while got < buf.len() {
            match self.socket.read(&mut buf[got..got + 1]) {
                Ok(_) => {
                    got += 1;
                    if buf[got - 1] == b'\n' {
                        break;
                    }
                }
                // The end of the input: what there is of the line.
                Err(Error::PeerClosed) => break,
                Err(Error::ShouldWait) if nonblock => return if got > 0 { Ok(got) } else { Err(EAGAIN) },
                Err(Error::ShouldWait) => self.wait_readable(vabi::DEADLINE_INFINITE)?,
                Err(_) if got > 0 => break,
                Err(e) => return Err(error::kernel(e)),
            }
        }
        Ok(got)
    }

    /// Non-canonical input as `VMIN` and `VTIME` ask (POSIX): `min` bytes
    /// at least (not more than fit), with at most `time` tenths of a second
    /// between them; with `min` 0, whatever comes within `time` (nothing
    /// waited for when that is 0 too).
    fn read_timed(&self, buf: &mut [u8], min: u8, time: u8) -> Result<usize, isize> {
        let min = (min as usize).min(buf.len());
        let interval = vrt::time::Duration::from_millis(100 * time as u64);
        let mut deadline = vrt::time::deadline_after(interval);
        let mut got = 0;
        loop {
            match self.socket.read(&mut buf[got..]) {
                Ok(n) => {
                    got += n;
                    if got >= min.max(1) {
                        return Ok(got);
                    }
                    // The time between bytes starts again.
                    deadline = vrt::time::deadline_after(interval);
                }
                Err(Error::PeerClosed) => return Ok(got),
                Err(Error::ShouldWait) => {}
                Err(_) if got > 0 => return Ok(got),
                Err(e) => return Err(error::kernel(e)),
            }
            let wait_until = match (min, time) {
                (0, 0) => return Ok(got),
                // Until the first byte, the time does not run.
                (_, 0) => vabi::DEADLINE_INFINITE,
                (m, _) if m > 0 && got == 0 => vabi::DEADLINE_INFINITE,
                _ => deadline,
            };
            match self.socket.wait(READABLE | PEER_CLOSED | PEER_WRITE_DISABLED, wait_until) {
                Ok(_) => {}
                Err(Error::TimedOut) => return Ok(got),
                Err(e) => return Err(error::kernel(e)),
            }
        }
    }

    fn wait_readable(&self, deadline: u64) -> Result<(), isize> {
        self.socket.wait(READABLE | PEER_CLOSED | PEER_WRITE_DISABLED, deadline).map(|_| ()).map_err(error::kernel)
    }

    /// Writes `data`: all of it, waiting for room, unless `nonblock` (then
    /// what fits). Writing to a stream nobody reads raises `SIGPIPE`; what
    /// was written before that, or before an error, is still counted.
    pub fn write(&self, data: &[u8], nonblock: bool) -> Result<usize, isize> {
        self.send(data, nonblock, true)
    }

    /// [`write`](Self::write), raising `SIGPIPE` or not (`MSG_NOSIGNAL`).
    pub fn send(&self, data: &[u8], nonblock: bool, sigpipe: bool) -> Result<usize, isize> {
        let mut done = 0;
        // An error after some bytes went: those are the result.
        let failed = |done: usize, e: isize| if done > 0 { Ok(done) } else { Err(e) };
        while done < data.len() {
            match self.socket.write(&data[done..]) {
                Ok(n) => done += n,
                Err(Error::ShouldWait) if nonblock => return failed(done, EAGAIN),
                Err(Error::ShouldWait) => {
                    if let Err(e) = self.socket.wait(WRITABLE | PEER_CLOSED, vabi::DEADLINE_INFINITE) {
                        return failed(done, error::kernel(e));
                    }
                }
                Err(Error::PeerClosed | Error::BadState) => {
                    if sigpipe {
                        signal::raise(crate::linux::sig::PIPE);
                    }
                    return failed(done, EPIPE);
                }
                Err(e) => return failed(done, error::kernel(e)),
            }
        }
        Ok(done)
    }

    /// Bytes that can be read without waiting.
    pub fn available(&self) -> usize {
        self.socket.info().map_or(0, |i| i.readable as usize)
    }

    /// Whether a read would not wait: `(readable, hung up, writable)`.
    pub fn readiness(&self) -> (bool, bool, bool) {
        let s = self.socket.wait(READABLE | WRITABLE | PEER_CLOSED | PEER_WRITE_DISABLED, 0).unwrap_or(0);
        let hup = s & (PEER_CLOSED | PEER_WRITE_DISABLED) != 0;
        (s & READABLE != 0, hup, s & WRITABLE != 0)
    }
}
