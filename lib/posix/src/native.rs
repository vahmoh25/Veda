//! Veda's own interfaces, for C programs that are Veda services or talk to
//! them directly (`<veda/ipc.h>`): handles, channels and the service
//! registry, waiting, memory objects and events. They are the kernel's
//! calls, but for the registry's, which go through `vproto`.
//!
//! Every call returns 0 (or a count) when it succeeds and a negative Veda
//! error number (`vabi::Error`) when it fails. Handles are the kernel's;
//! the descriptors of the POSIX side know nothing of them.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::mem::ManuallyDrop;

use vabi::{Error, RawHandle, WaitItem, nr};
use vrt::object::{Channel, Handle};
use vrt::sys::call;

fn fail(e: Error) -> c_int {
    -(e as c_int)
}

fn done(r: Result<usize, Error>) -> c_int {
    match r {
        Ok(_) => 0,
        Err(e) => fail(e),
    }
}

/// Stores a new handle where the caller asked.
///
/// # Safety
/// `out` is writable.
unsafe fn give(out: *mut RawHandle, r: Result<usize, Error>) -> c_int {
    match r {
        Ok(h) => {
            // SAFETY: the caller's pointer.
            unsafe { *out = h as RawHandle };
            0
        }
        Err(e) => fail(e),
    }
}

/// A channel the caller keeps owning.
fn borrowed(h: RawHandle) -> ManuallyDrop<Channel> {
    // SAFETY: never dropped, so never closed here.
    ManuallyDrop::new(Channel::from_handle(unsafe { Handle::from_raw(h) }))
}

/// A name from C, if it is UTF-8.
///
/// # Safety
/// `name` is NUL-terminated.
unsafe fn name<'a>(name: *const c_char) -> Option<&'a str> {
    // SAFETY: as the caller promises.
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

/// Closes a handle.
#[unsafe(no_mangle)]
pub extern "C" fn veda_close(h: RawHandle) -> c_int {
    done(vrt::sys::handle_close(h))
}

/// Duplicates a handle with `rights` (`~0`: those it has).
///
/// # Safety
/// `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_duplicate(h: RawHandle, rights: u32, out: *mut RawHandle) -> c_int {
    // SAFETY: as the caller promises.
    unsafe { give(out, call(nr::HANDLE_DUPLICATE, [h as usize, rights as usize, 0, 0, 0, 0])) }
}

/// Registers the program as the provider of service `name`; connections
/// to it arrive on `*listener` ([`veda_service_accept`]).
///
/// # Safety
/// `name` is NUL-terminated; `listener` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_service_register(name_: *const c_char, listener: *mut RawHandle) -> c_int {
    // SAFETY: as the caller promises.
    let Some(n) = (unsafe { name(name_) }) else { return fail(Error::InvalidArgs) };
    match vproto::register(n) {
        // SAFETY: as the caller promises.
        Ok(l) => unsafe { give(listener, Ok(l.0.into_raw() as usize)) },
        // Taken by another, or reserved for a system service.
        Err(_) => fail(Error::AccessDenied),
    }
}

/// Takes a connection waiting on a listener: its channel goes to
/// `*channel`. Fails with `ShouldWait` if none waits.
///
/// # Safety
/// `channel` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_service_accept(listener: RawHandle, channel: *mut RawHandle) -> c_int {
    match vproto::accept(&borrowed(listener)) {
        // SAFETY: as the caller promises.
        Some(ch) => unsafe { give(channel, Ok(ch.0.into_raw() as usize)) },
        None => fail(Error::ShouldWait),
    }
}

/// Connects to service `name`; the connection's channel goes to
/// `*channel`. Connections to a service not registered yet wait for it.
///
/// # Safety
/// `name` is NUL-terminated; `channel` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_service_connect(name_: *const c_char, channel: *mut RawHandle) -> c_int {
    // SAFETY: as the caller promises.
    let Some(n) = (unsafe { name(name_) }) else { return fail(Error::InvalidArgs) };
    match vproto::connect(n) {
        // SAFETY: as the caller promises.
        Ok(ch) => unsafe { give(channel, Ok(ch.0.into_raw() as usize)) },
        Err(vproto::ServiceError::NotFound) => fail(Error::NotFound),
        Err(vproto::ServiceError::Unavailable) => fail(Error::PeerClosed),
    }
}

/// Sends a message: `len` bytes and `count` handles, which move to the
/// receiver (and are closed if the message cannot be sent).
///
/// # Safety
/// `bytes` and `handles` hold `len` bytes and `count` handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_channel_write(
    ch: RawHandle,
    bytes: *const c_void,
    len: usize,
    handles: *const RawHandle,
    count: usize,
) -> c_int {
    done(call(nr::CHANNEL_WRITE, [ch as usize, bytes as usize, len, handles as usize, count, 0]))
}

/// Reads a message without waiting: up to `cap` bytes and `hcap` handles.
/// `*len` and `*count` get its size; if that is more than fits, it fails
/// with `BufferTooSmall` and stays queued. Fails with `ShouldWait` if no
/// message is queued, `PeerClosed` if none will come.
///
/// # Safety
/// The buffers hold `cap` bytes and `hcap` handles; `len` and `count` are
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_channel_read(
    ch: RawHandle,
    bytes: *mut c_void,
    cap: usize,
    len: *mut usize,
    handles: *mut RawHandle,
    hcap: usize,
    count: *mut usize,
) -> c_int {
    let mut actual = [0u32; 2];
    let r = call(
        nr::CHANNEL_READ,
        [ch as usize, bytes as usize, cap, handles as usize, hcap, actual.as_mut_ptr() as usize],
    );
    // SAFETY: as the caller promises.
    unsafe {
        *len = actual[0] as usize;
        *count = actual[1] as usize;
    }
    done(r)
}

/// Waits until one of `count` items has one of its signals active, or the
/// deadline (`veda_now_ns`'s clock; `~0`: none) passes. Each item's
/// `observed` gets the signals active. Returns how many items are ready: 0
/// when the deadline passed.
///
/// # Safety
/// `items` holds `count` items.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_wait(items: *mut WaitItem, count: usize, deadline: u64) -> c_int {
    if count == 0 || count > vabi::WAIT_MANY_MAX {
        return fail(Error::InvalidArgs);
    }
    // SAFETY: as the caller promises.
    let items = unsafe { core::slice::from_raw_parts_mut(items, count) };
    match vrt::object::wait_many(items, deadline) {
        Ok(n) => n as c_int,
        Err(Error::TimedOut) => 0,
        Err(e) => fail(e),
    }
}

/// The monotonic clock, in nanoseconds since boot.
#[unsafe(no_mangle)]
pub extern "C" fn veda_now_ns() -> u64 {
    vrt::time::now_ns()
}

/// Creates a memory object of `size` bytes, zero-filled.
///
/// # Safety
/// `vmo` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_vmo_create(size: u64, vmo: *mut RawHandle) -> c_int {
    // SAFETY: as the caller promises.
    unsafe { give(vmo, call(nr::VMO_CREATE, [size as usize, 0, 0, 0, 0, 0])) }
}

/// Maps `size` bytes of a memory object from `offset` (`VEDA_MAP_READ`,
/// `VEDA_MAP_WRITE`); the address goes to `*addr`.
///
/// # Safety
/// `addr` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_vmo_map(
    vmo: RawHandle,
    offset: u64,
    size: u64,
    flags: u32,
    addr: *mut *mut c_void,
) -> c_int {
    let flags = flags as usize & (vabi::map_flags::READ | vabi::map_flags::WRITE);
    let r = call(nr::VM_MAP, [vabi::INVALID_HANDLE as usize, vmo as usize, offset as usize, size as usize, 0, flags]);
    match r {
        Ok(a) => {
            // SAFETY: as the caller promises.
            unsafe { *addr = a as *mut c_void };
            0
        }
        Err(e) => fail(e),
    }
}

/// Unmaps what [`veda_vmo_map`] mapped.
#[unsafe(no_mangle)]
pub extern "C" fn veda_vmo_unmap(addr: *mut c_void, size: u64) -> c_int {
    done(call(nr::VM_UNMAP, [vabi::INVALID_HANDLE as usize, addr as usize, size as usize, 0, 0, 0]))
}

/// Creates an event (signaled with `VEDA_SIGNALED`).
///
/// # Safety
/// `event` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn veda_event_create(event: *mut RawHandle) -> c_int {
    // SAFETY: as the caller promises.
    unsafe { give(event, call(nr::EVENT_CREATE, [0; 6])) }
}

/// Clears, then sets, signals of an object: an event's `VEDA_SIGNALED`, or
/// the user signals of any.
#[unsafe(no_mangle)]
pub extern "C" fn veda_object_signal(h: RawHandle, clear: u32, set: u32) -> c_int {
    done(call(nr::OBJECT_SIGNAL, [h as usize, clear as usize, set as usize, 0, 0, 0]))
}
