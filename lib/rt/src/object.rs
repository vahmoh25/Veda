//! Safe, owning wrappers around kernel object handles.
//!
//! Every wrapper closes its handle when dropped. Use `into_raw`/`from_raw`
//! to pass handles across FFI or protocol boundaries.

use alloc::vec::Vec;

use vabi::{Error, HandleBasicInfo, ProcessInfo, RawHandle, Rights, WaitItem, info_topic, nr};

use crate::sys::{self, call, call2};

/// An owned handle of any type.
#[derive(Debug)]
#[repr(transparent)]
pub struct Handle(RawHandle);

impl Handle {
    /// Takes ownership of a raw handle value.
    ///
    /// # Safety
    /// `raw` must be a valid handle owned by nobody else (or 0).
    pub const unsafe fn from_raw(raw: RawHandle) -> Handle {
        Handle(raw)
    }

    pub const fn invalid() -> Handle {
        Handle(vabi::INVALID_HANDLE)
    }

    pub fn is_valid(&self) -> bool {
        self.0 != vabi::INVALID_HANDLE
    }

    pub fn raw(&self) -> RawHandle {
        self.0
    }

    /// Releases ownership without closing.
    pub fn into_raw(self) -> RawHandle {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }

    /// Duplicates the handle, optionally with fewer rights.
    pub fn duplicate(&self, rights: Option<Rights>) -> Result<Handle, Error> {
        let r = rights.map(|r| r.0).unwrap_or(u32::MAX);
        call(nr::HANDLE_DUPLICATE, [self.0 as usize, r as usize, 0, 0, 0, 0]).map(|h| Handle(h as RawHandle))
    }

    /// Waits until any of `signals` is active or `deadline` (monotonic ns)
    /// passes. Returns the active signals.
    pub fn wait(&self, signals: u32, deadline: u64) -> Result<u32, Error> {
        call(nr::OBJECT_WAIT_ONE, [self.0 as usize, signals as usize, deadline as usize, 0, 0, 0]).map(|s| s as u32)
    }

    /// Sets/clears user signals on the object.
    pub fn signal(&self, clear: u32, set: u32) -> Result<(), Error> {
        call(nr::OBJECT_SIGNAL, [self.0 as usize, clear as usize, set as usize, 0, 0, 0]).map(|_| ())
    }

    pub fn basic_info(&self) -> Result<HandleBasicInfo, Error> {
        let mut info = HandleBasicInfo::default();
        call(
            nr::OBJECT_INFO,
            [
                self.0 as usize,
                info_topic::HANDLE_BASIC,
                &mut info as *mut _ as usize,
                core::mem::size_of_val(&info),
                0,
                0,
            ],
        )?;
        Ok(info)
    }

    pub fn koid(&self) -> u64 {
        self.basic_info().map(|i| i.koid).unwrap_or(0)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != vabi::INVALID_HANDLE {
            let _ = sys::handle_close(self.0);
        }
    }
}

macro_rules! wrapper {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        #[repr(transparent)]
        pub struct $name(pub Handle);

        impl $name {
            pub fn raw(&self) -> RawHandle {
                self.0.raw()
            }
            pub fn into_handle(self) -> Handle {
                self.0
            }
            pub fn from_handle(h: Handle) -> Self {
                $name(h)
            }
            pub fn wait(&self, signals: u32, deadline: u64) -> Result<u32, Error> {
                self.0.wait(signals, deadline)
            }
        }

        impl From<$name> for Handle {
            fn from(v: $name) -> Handle {
                v.0
            }
        }
    };
}

wrapper!(
    /// A channel endpoint.
    Channel
);
wrapper!(
    /// A socket endpoint: one end of a byte stream (see `vabi::nr::SOCKET_CREATE`).
    Socket
);
wrapper!(
    /// A memory object.
    Vmo
);
wrapper!(
    /// An event (signal word).
    Event
);
wrapper!(
    /// A process.
    Process
);
wrapper!(
    /// A thread.
    Thread
);
wrapper!(
    /// A hardware interrupt bound to this driver.
    Interrupt
);
wrapper!(
    /// A range of I/O ports.
    IoPorts
);
wrapper!(
    /// A hardware resource capability.
    Resource
);

/// A message read from a channel.
#[derive(Debug, Default)]
pub struct Message {
    pub bytes: Vec<u8>,
    pub handles: Vec<Handle>,
}

impl Channel {
    pub fn create() -> Result<(Channel, Channel), Error> {
        let mut out = [0u32; 2];
        call(nr::CHANNEL_CREATE, [out.as_mut_ptr() as usize, 0, 0, 0, 0, 0])?;
        // SAFETY: the kernel just gave us both handles.
        Ok(unsafe { (Channel(Handle::from_raw(out[0])), Channel(Handle::from_raw(out[1]))) })
    }

    /// Sends bytes and transfers ownership of `handles` (consumed even on
    /// error, mirroring the kernel's semantics).
    pub fn write(&self, bytes: &[u8], handles: Vec<Handle>) -> Result<(), Error> {
        let raws: Vec<RawHandle> = handles.into_iter().map(Handle::into_raw).collect();
        call(
            nr::CHANNEL_WRITE,
            [self.raw() as usize, bytes.as_ptr() as usize, bytes.len(), raws.as_ptr() as usize, raws.len(), 0],
        )
        .map(|_| ())
    }

    /// Reads one message into the given buffers. On `BufferTooSmall`, returns
    /// the required sizes in the error payload via [`Channel::read`].
    pub fn read_into(
        &self,
        bytes: &mut [u8],
        handles: &mut [RawHandle],
    ) -> Result<(usize, usize), (Error, usize, usize)> {
        let mut actual = [0u32; 2];
        let r = call(
            nr::CHANNEL_READ,
            [
                self.raw() as usize,
                bytes.as_mut_ptr() as usize,
                bytes.len(),
                handles.as_mut_ptr() as usize,
                handles.len(),
                actual.as_mut_ptr() as usize,
            ],
        );
        match r {
            Ok(_) => Ok((actual[0] as usize, actual[1] as usize)),
            Err(e) => Err((e, actual[0] as usize, actual[1] as usize)),
        }
    }

    /// Reads one message of any size without blocking.
    pub fn read(&self) -> Result<Message, Error> {
        let mut bytes = alloc::vec![0u8; 4096];
        let mut raws = alloc::vec![0 as RawHandle; 16];
        loop {
            match self.read_into(&mut bytes, &mut raws) {
                Ok((n, h)) => {
                    bytes.truncate(n);
                    // SAFETY: the kernel transferred these handles to us.
                    let handles = raws[..h].iter().map(|&r| unsafe { Handle::from_raw(r) }).collect();
                    return Ok(Message { bytes, handles });
                }
                Err((Error::BufferTooSmall, nb, nh)) => {
                    bytes.resize(nb.max(bytes.len()), 0);
                    raws.resize(nh.max(raws.len()), 0);
                }
                Err((e, _, _)) => return Err(e),
            }
        }
    }

    /// Blocks until a message arrives (or the peer closes), then reads it.
    pub fn read_blocking(&self, deadline: u64) -> Result<Message, Error> {
        loop {
            match self.read() {
                Err(Error::ShouldWait) => {
                    let s = self.wait(vabi::signals::READABLE | vabi::signals::PEER_CLOSED, deadline)?;
                    if s & vabi::signals::READABLE == 0 && s & vabi::signals::PEER_CLOSED != 0 {
                        return Err(Error::PeerClosed);
                    }
                }
                other => return other,
            }
        }
    }
}

impl Socket {
    /// A connected pair of endpoints.
    pub fn create() -> Result<(Socket, Socket), Error> {
        let mut out = [0u32; 2];
        call(nr::SOCKET_CREATE, [out.as_mut_ptr() as usize, 0, 0, 0, 0, 0])?;
        // SAFETY: the kernel just gave us both handles.
        Ok(unsafe { (Socket(Handle::from_raw(out[0])), Socket(Handle::from_raw(out[1]))) })
    }

    /// Writes as much of `data` as fits without blocking and returns how
    /// much that was (`ShouldWait` if nothing fits; writes of at most
    /// `vabi::SOCKET_ATOMIC_WRITE` bytes go in whole or not at all).
    pub fn write(&self, data: &[u8]) -> Result<usize, Error> {
        call(nr::SOCKET_WRITE, [self.raw() as usize, data.as_ptr() as usize, data.len(), 0, 0, 0])
    }

    /// Reads what is buffered, up to `buf.len()` bytes, without blocking.
    /// `PeerClosed` marks the end of the stream.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
        call(nr::SOCKET_READ, [self.raw() as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0])
    }

    /// Writes all of `data`, waiting for room as needed.
    pub fn write_all(&self, mut data: &[u8]) -> Result<(), Error> {
        use vabi::signals::{PEER_CLOSED, WRITABLE};
        while !data.is_empty() {
            match self.write(data) {
                Ok(n) => data = &data[n..],
                Err(Error::ShouldWait) => {
                    self.wait(WRITABLE | PEER_CLOSED, vabi::DEADLINE_INFINITE)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads at least one byte, waiting until there is one; `Ok(0)` at the
    /// end of the stream.
    pub fn read_blocking(&self, buf: &mut [u8]) -> Result<usize, Error> {
        use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE};
        loop {
            match self.read(buf) {
                Err(Error::ShouldWait) => {
                    self.wait(READABLE | PEER_CLOSED | PEER_WRITE_DISABLED, vabi::DEADLINE_INFINITE)?;
                }
                Err(Error::PeerClosed) => return Ok(0),
                other => return other,
            }
        }
    }

    /// Stops writing from this endpoint (the peer reads the end of the
    /// stream once it has read what is buffered).
    pub fn shutdown(&self) -> Result<(), Error> {
        call(nr::SOCKET_SHUTDOWN, [self.raw() as usize, 0, 0, 0, 0, 0]).map(|_| ())
    }

    /// Bytes buffered for reading here, and bytes a write would accept now.
    pub fn info(&self) -> Result<vabi::SocketInfo, Error> {
        let mut info = vabi::SocketInfo::default();
        call(
            nr::OBJECT_INFO,
            [self.raw() as usize, info_topic::SOCKET, &mut info as *mut _ as usize, size_of_val(&info), 0, 0],
        )?;
        Ok(info)
    }

    /// Another handle to the same endpoint, with all rights or `rights`.
    pub fn duplicate(&self, rights: Option<Rights>) -> Result<Socket, Error> {
        self.0.duplicate(rights).map(Socket)
    }
}

impl Vmo {
    pub fn create(size: usize) -> Result<Vmo, Error> {
        call(nr::VMO_CREATE, [size, 0, 0, 0, 0, 0]).map(|h| Vmo(Handle(h as RawHandle)))
    }

    pub fn create_committed(size: usize) -> Result<Vmo, Error> {
        call(nr::VMO_CREATE, [size, vabi::vmo_flags::COMMIT, 0, 0, 0, 0]).map(|h| Vmo(Handle(h as RawHandle)))
    }

    pub fn create_physical(res: &Resource, paddr: u64, size: usize, cache: usize) -> Result<Vmo, Error> {
        call(nr::VMO_CREATE_PHYSICAL, [res.raw() as usize, paddr as usize, size, cache, 0, 0])
            .map(|h| Vmo(Handle(h as RawHandle)))
    }

    pub fn create_contiguous(res: &Resource, size: usize) -> Result<Vmo, Error> {
        call(nr::VMO_CREATE_CONTIGUOUS, [res.raw() as usize, size, 0, 0, 0, 0]).map(|h| Vmo(Handle(h as RawHandle)))
    }

    /// Like [`Vmo::create_contiguous`], below 4 GiB (for devices that can
    /// only address 32 bits).
    pub fn create_contiguous_below_4g(res: &Resource, size: usize) -> Result<Vmo, Error> {
        call(nr::VMO_CREATE_CONTIGUOUS, [res.raw() as usize, size, vabi::dma_flags::BELOW_4G, 0, 0, 0])
            .map(|h| Vmo(Handle(h as RawHandle)))
    }

    pub fn size(&self) -> Result<usize, Error> {
        call(nr::VMO_GET_SIZE, [self.raw() as usize, 0, 0, 0, 0, 0])
    }

    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<(), Error> {
        call(nr::VMO_READ, [self.raw() as usize, offset, buf.as_mut_ptr() as usize, buf.len(), 0, 0]).map(|_| ())
    }

    pub fn write(&self, offset: usize, buf: &[u8]) -> Result<(), Error> {
        call(nr::VMO_WRITE, [self.raw() as usize, offset, buf.as_ptr() as usize, buf.len(), 0, 0]).map(|_| ())
    }

    pub fn phys_addr(&self, offset: usize) -> Result<u64, Error> {
        call(nr::VMO_PHYS_ADDR, [self.raw() as usize, offset, 0, 0, 0, 0]).map(|a| a as u64)
    }

    /// Maps the VMO into the current process; see [`crate::vm::map`].
    pub fn map(&self, offset: usize, len: usize, flags: usize) -> Result<usize, Error> {
        crate::vm::map(None, self, offset, len, 0, flags)
    }
}

impl Event {
    pub fn create() -> Result<Event, Error> {
        call(nr::EVENT_CREATE, [0; 6]).map(|h| Event(Handle(h as RawHandle)))
    }

    pub fn signal(&self) -> Result<(), Error> {
        self.0.signal(0, vabi::signals::SIGNALED)
    }

    pub fn clear(&self) -> Result<(), Error> {
        self.0.signal(vabi::signals::SIGNALED, 0)
    }
}

impl Process {
    pub fn info(&self) -> Result<ProcessInfo, Error> {
        let mut info = ProcessInfo::default();
        call(
            nr::OBJECT_INFO,
            [
                self.raw() as usize,
                info_topic::PROCESS,
                &mut info as *mut _ as usize,
                core::mem::size_of_val(&info),
                0,
                0,
            ],
        )?;
        Ok(info)
    }

    pub fn kill(&self) -> Result<(), Error> {
        call(nr::PROCESS_KILL, [self.raw() as usize, 0, 0, 0, 0, 0]).map(|_| ())
    }

    /// Kills the process and every process it started that still runs
    /// (and those they started): its job, as a terminal's Ctrl+C ends it.
    pub fn kill_tree(&self) -> Result<(), Error> {
        call(nr::PROCESS_KILL, [self.raw() as usize, vabi::kill_flags::DESCENDANTS, 0, 0, 0, 0]).map(|_| ())
    }

    /// Waits for the process to terminate and returns its exit code.
    pub fn join(&self, deadline: u64) -> Result<i64, Error> {
        self.wait(vabi::signals::TERMINATED, deadline)?;
        Ok(self.info()?.exit_code)
    }
}

impl Interrupt {
    pub fn create(res: &Resource, irq: usize, flags: usize) -> Result<Interrupt, Error> {
        call(nr::IRQ_CREATE, [res.raw() as usize, irq, flags, 0, 0, 0]).map(|h| Interrupt(Handle(h as RawHandle)))
    }

    /// Allocates an MSI vector; returns the interrupt and the address/data
    /// pair to program into the device.
    pub fn create_msi(res: &Resource) -> Result<(Interrupt, vabi::MsiInfo), Error> {
        let mut info = vabi::MsiInfo::default();
        let h = call(nr::MSI_CREATE, [res.raw() as usize, &mut info as *mut _ as usize, 0, 0, 0, 0])?;
        Ok((Interrupt(Handle(h as RawHandle)), info))
    }

    /// Re-arms the interrupt after servicing the device.
    pub fn ack(&self) -> Result<(), Error> {
        call(nr::IRQ_ACK, [self.raw() as usize, 0, 0, 0, 0, 0]).map(|_| ())
    }

    /// Blocks until the interrupt fires.
    pub fn wait_irq(&self, deadline: u64) -> Result<(), Error> {
        self.wait(vabi::signals::SIGNALED, deadline).map(|_| ())
    }
}

impl IoPorts {
    pub fn create(res: &Resource, base: u16, count: u16) -> Result<IoPorts, Error> {
        call(nr::IOPORT_CREATE, [res.raw() as usize, base as usize, count as usize, 0, 0, 0])
            .map(|h| IoPorts(Handle(h as RawHandle)))
    }

    pub fn in8(&self, port: u16) -> u8 {
        call(nr::IOPORT_READ, [self.raw() as usize, port as usize, 1, 0, 0, 0]).unwrap_or(0xFF) as u8
    }

    pub fn in16(&self, port: u16) -> u16 {
        call(nr::IOPORT_READ, [self.raw() as usize, port as usize, 2, 0, 0, 0]).unwrap_or(0xFFFF) as u16
    }

    pub fn in32(&self, port: u16) -> u32 {
        call(nr::IOPORT_READ, [self.raw() as usize, port as usize, 4, 0, 0, 0]).unwrap_or(u32::MAX as usize) as u32
    }

    pub fn out8(&self, port: u16, v: u8) {
        let _ = call(nr::IOPORT_WRITE, [self.raw() as usize, port as usize, 1, v as usize, 0, 0]);
    }

    pub fn out16(&self, port: u16, v: u16) {
        let _ = call(nr::IOPORT_WRITE, [self.raw() as usize, port as usize, 2, v as usize, 0, 0]);
    }

    pub fn out32(&self, port: u16, v: u32) {
        let _ = call(nr::IOPORT_WRITE, [self.raw() as usize, port as usize, 4, v as usize, 0, 0]);
    }
}

impl Resource {
    /// Derives a narrower resource (see `vabi::resource_kind`).
    pub fn create(&self, kind: usize, base: u64, size: u64) -> Result<Resource, Error> {
        call(nr::RESOURCE_CREATE, [self.raw() as usize, kind, base as usize, size as usize, 0, 0])
            .map(|h| Resource(Handle(h as RawHandle)))
    }

    pub fn duplicate(&self) -> Result<Resource, Error> {
        self.0.duplicate(None).map(Resource)
    }
}

/// Waits on several objects at once. Returns the number of satisfied items;
/// each item's `observed` field is filled in.
pub fn wait_many(items: &mut [WaitItem], deadline: u64) -> Result<usize, Error> {
    call(nr::OBJECT_WAIT_MANY, [items.as_mut_ptr() as usize, items.len(), deadline as usize, 0, 0, 0])
}

/// Reads the kernel log starting at `offset`; returns bytes read and the next
/// offset.
pub fn log_read(offset: u64, buf: &mut [u8]) -> Result<(usize, u64), Error> {
    call2(nr::LOG_READ, [offset as usize, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0])
        .map(|(n, next)| (n, next as u64))
}

/// Information about the calling process.
pub fn process_self_info() -> Result<ProcessInfo, Error> {
    let mut info = ProcessInfo::default();
    let len = core::mem::size_of_val(&info);
    call(
        nr::OBJECT_INFO,
        [vabi::INVALID_HANDLE as usize, info_topic::PROCESS, &mut info as *mut _ as usize, len, 0, 0],
    )?;
    Ok(info)
}

/// Lists processes (up to `out.len()`); returns the total number.
pub fn process_list(out: &mut [ProcessInfo]) -> Result<usize, Error> {
    call(nr::PROCESS_LIST, [out.as_mut_ptr() as usize, out.len(), 0, 0, 0, 0])
}

pub fn system_info() -> Result<vabi::SystemInfo, Error> {
    let mut info = vabi::SystemInfo::default();
    call(nr::SYSTEM_INFO, [&mut info as *mut _ as usize, 0, 0, 0, 0, 0])?;
    Ok(info)
}

pub fn power(resource: &Resource, action: usize) -> Result<(), Error> {
    call(nr::SYSTEM_POWER, [resource.raw() as usize, action, 0, 0, 0, 0]).map(|_| ())
}

pub fn random_bytes(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(4096) {
        let _ = call(nr::RANDOM, [chunk.as_mut_ptr() as usize, chunk.len(), 0, 0, 0, 0]);
    }
}

pub fn process_open(resource: &Resource, koid: u64) -> Result<Process, Error> {
    call(nr::PROCESS_OPEN, [resource.raw() as usize, koid as usize, 0, 0, 0, 0])
        .map(|h| Process(Handle(h as RawHandle)))
}
