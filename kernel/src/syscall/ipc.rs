//! Handles, waiting, channels and events.

use alloc::vec::Vec;

use vabi::signals::{SIGNALED, USER_ALL};
use vabi::{Error, HandleBasicInfo, RawHandle, Rights, WaitItem, info_topic};

use super::{SysResult, current_process, deadline, get_channel, handle, insert, ok};
use crate::mm::user;
use crate::object::channel::{self, Message};
use crate::object::event::Event;
use crate::object::handle::Handle;
use crate::object::{KObject, new_koid};
use crate::sched::{self, WakeReason};

const CHANNEL_RIGHTS: Rights = Rights(
    Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::WAIT.0 | Rights::SIGNAL.0 | Rights::GET_INFO.0,
);

pub fn handle_close(raw: RawHandle) -> SysResult {
    let h = current_process()?.handles.lock().remove(raw)?;
    drop(h);
    ok(0)
}

fn reduce(rights: Rights, requested: u32) -> Result<Rights, Error> {
    if requested == u32::MAX {
        return Ok(rights);
    }
    let r = Rights(requested);
    if !rights.contains(r) {
        return Err(Error::AccessDenied);
    }
    Ok(r)
}

pub fn handle_duplicate(raw: RawHandle, rights: u32) -> SysResult {
    let h = handle(raw, Rights::DUPLICATE)?;
    let rights = reduce(h.rights, rights)?;
    ok(insert(h.object, rights)? as usize)
}

pub fn handle_replace(raw: RawHandle, rights: u32) -> SysResult {
    let p = current_process()?;
    let mut table = p.handles.lock();
    let h = table.get(raw)?.clone();
    let rights = reduce(h.rights, rights)?;
    table.remove(raw)?;
    ok(table.insert(Handle { object: h.object, rights })? as usize)
}

pub fn object_info(raw: RawHandle, topic: usize, buf: usize, len: usize) -> SysResult {
    let h = handle(raw, Rights::NONE)?;
    let write = |bytes: &[u8]| -> SysResult {
        if len < bytes.len() {
            return Err(Error::BufferTooSmall);
        }
        user::copy_to_user(buf as u64, bytes)?;
        ok(bytes.len())
    };
    fn as_bytes<T>(v: &T) -> &[u8] {
        // SAFETY: plain #[repr(C)] ABI structures.
        unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
    }
    match topic {
        info_topic::HANDLE_BASIC => {
            let info = HandleBasicInfo {
                koid: h.object.koid(),
                object_type: h.object.object_type() as u32,
                rights: h.rights.0,
            };
            write(as_bytes(&info))
        }
        info_topic::PROCESS => match &h.object {
            KObject::Process(p) => write(as_bytes(&p.info())),
            _ => Err(Error::WrongType),
        },
        info_topic::VMO => match &h.object {
            KObject::Vmo(v) => {
                let info =
                    vabi::VmoInfo { size: v.size(), committed_bytes: v.committed_bytes(), flags: 0, _reserved: 0 };
                write(as_bytes(&info))
            }
            _ => Err(Error::WrongType),
        },
        info_topic::THREAD => match &h.object {
            KObject::Thread(t) => {
                let s = t.sched.lock();
                let info = vabi::ThreadInfo {
                    koid: t.koid,
                    state: s.state as u32,
                    priority: s.priority as u32,
                    cpu_time_ns: t.cpu_time_ns.load(core::sync::atomic::Ordering::Relaxed),
                };
                drop(s);
                write(as_bytes(&info))
            }
            _ => Err(Error::WrongType),
        },
        _ => Err(Error::InvalidArgs),
    }
}

pub fn object_signal(raw: RawHandle, clear: u32, set: u32) -> SysResult {
    let h = handle(raw, Rights::SIGNAL)?;
    let allowed = match h.object {
        KObject::Event(_) => USER_ALL | SIGNALED,
        KObject::Channel(_) => USER_ALL,
        _ => return Err(Error::NotSupported),
    };
    if (clear | set) & !allowed != 0 {
        return Err(Error::InvalidArgs);
    }
    h.object.signals().ok_or(Error::NotSupported)?.update(clear, set);
    ok(0)
}

/// Shared implementation of the wait calls. Returns the number of satisfied
/// items; `observed` is filled for every item.
fn wait_objects(objects: &[KObject], masks: &[u32], observed: &mut [u32], d: u64) -> Result<usize, Error> {
    let cur = sched::current();
    let wait_id = new_koid();
    loop {
        let mut satisfied = 0;
        for (i, o) in objects.iter().enumerate() {
            observed[i] = o.signals().map(|s| s.active()).unwrap_or(0);
            if observed[i] & masks[i] != 0 {
                satisfied += 1;
            }
        }
        if satisfied > 0 {
            return Ok(satisfied);
        }
        if d == 0 {
            return Err(Error::TimedOut);
        }
        for (i, o) in objects.iter().enumerate() {
            if let Some(s) = o.signals() {
                s.add_waiter(wait_id, cur.clone(), masks[i]);
            }
        }
        let reason = sched::block(deadline(d));
        for o in objects {
            if let Some(s) = o.signals() {
                s.remove_waiter(wait_id);
            }
        }
        match reason {
            WakeReason::Killed => return Err(Error::Canceled),
            WakeReason::TimedOut => {
                // Report the final state, then time out.
                for (i, o) in objects.iter().enumerate() {
                    observed[i] = o.signals().map(|s| s.active()).unwrap_or(0);
                }
                return if objects.iter().zip(masks).zip(observed.iter()).any(|((_, m), o)| o & m != 0) {
                    Ok(1)
                } else {
                    Err(Error::TimedOut)
                };
            }
            _ => {}
        }
    }
}

pub fn wait_one(raw: RawHandle, signals: u32, d: u64) -> SysResult {
    let h = handle(raw, Rights::WAIT)?;
    if h.object.signals().is_none() {
        return Err(Error::NotSupported);
    }
    let mut observed = [0u32];
    match wait_objects(core::slice::from_ref(&h.object), &[signals], &mut observed, d) {
        Ok(_) => ok(observed[0] as usize),
        Err(e) => Err(e),
    }
}

pub fn wait_many(items_ptr: usize, count: usize, d: u64) -> SysResult {
    if count == 0 || count > vabi::WAIT_MANY_MAX {
        return Err(Error::InvalidArgs);
    }
    let size = core::mem::size_of::<WaitItem>();
    let bytes = user::read_vec(items_ptr as u64, count * size, vabi::WAIT_MANY_MAX * size)?;
    let mut items: Vec<WaitItem> = bytes
        .chunks_exact(size)
        // SAFETY: WaitItem is plain data valid for any bit pattern.
        .map(|c| unsafe { core::ptr::read_unaligned(c.as_ptr() as *const WaitItem) })
        .collect();
    let mut objects = Vec::with_capacity(count);
    for it in &items {
        let h = handle(it.handle, Rights::WAIT)?;
        if h.object.signals().is_none() {
            return Err(Error::NotSupported);
        }
        objects.push(h.object);
    }
    let masks: Vec<u32> = items.iter().map(|i| i.signals).collect();
    let mut observed = alloc::vec![0u32; count];
    let result = wait_objects(&objects, &masks, &mut observed, d);
    for (it, o) in items.iter_mut().zip(&observed) {
        it.observed = *o;
    }
    // SAFETY: viewing plain data as bytes.
    let out = unsafe { core::slice::from_raw_parts(items.as_ptr() as *const u8, count * size) };
    user::copy_to_user(items_ptr as u64, out)?;
    result.map(|n| (n, None))
}

pub fn channel_create(out: usize) -> SysResult {
    let (a, b) = channel::create();
    let p = current_process()?;
    let mut table = p.handles.lock();
    let ha = table.insert(Handle { object: KObject::Channel(a), rights: CHANNEL_RIGHTS })?;
    let hb = match table.insert(Handle { object: KObject::Channel(b), rights: CHANNEL_RIGHTS }) {
        Ok(h) => h,
        Err(e) => {
            let _ = table.remove(ha);
            return Err(e);
        }
    };
    drop(table);
    user::write(out as u64, &[ha, hb])?;
    ok(0)
}

pub fn channel_write(raw: RawHandle, bytes: usize, n_bytes: usize, handles_ptr: usize, n_handles: usize) -> SysResult {
    let ch = get_channel(raw, Rights::WRITE)?;
    let data = user::read_vec(bytes as u64, n_bytes, vabi::CHANNEL_MAX_BYTES)?;
    let raws = user::read_u32s(handles_ptr as u64, n_handles, vabi::CHANNEL_MAX_HANDLES)?;
    // Validate every handle before moving any of them.
    let p = current_process()?;
    let mut moved = Vec::with_capacity(raws.len());
    {
        let mut table = p.handles.lock();
        for (i, &r) in raws.iter().enumerate() {
            if r == raw || raws[..i].contains(&r) {
                return Err(Error::InvalidArgs);
            }
            let h = table.get(r)?;
            if !h.rights.contains(Rights::TRANSFER) {
                return Err(Error::AccessDenied);
            }
        }
        for &r in &raws {
            moved.push(table.remove(r)?);
        }
    }
    // Handles are consumed even if the write fails (like Zircon); the
    // returned message is dropped here, closing them.
    match ch.write(Message { data, handles: moved }) {
        Ok(()) => ok(0),
        Err((e, msg)) => {
            drop(msg);
            Err(e)
        }
    }
}

pub fn channel_read(
    raw: RawHandle,
    bytes: usize,
    bytes_cap: usize,
    handles_ptr: usize,
    handles_cap: usize,
    actual: usize,
) -> SysResult {
    let ch = get_channel(raw, Rights::READ)?;
    let p = current_process()?;
    // Validate the output buffers up front so a dequeued message is never
    // lost to a bad pointer.
    let aspace = p.aspace().ok_or(Error::BadState)?;
    let check = |ptr: usize, len: usize| aspace.ensure_range(ptr as u64, len as u64, true);
    if !check(actual, 8)
        || !check(bytes, bytes_cap.min(vabi::CHANNEL_MAX_BYTES))
        || !check(handles_ptr, handles_cap * 4)
    {
        return Err(Error::Fault);
    }
    let msg = match ch.read(bytes_cap, handles_cap) {
        Ok(m) => m,
        Err(e) => {
            if e.error == Error::BufferTooSmall {
                user::write(actual as u64, &[e.bytes as u32, e.handles as u32])?;
            }
            return Err(e.error);
        }
    };
    let mut values = Vec::with_capacity(msg.handles.len());
    {
        let mut table = p.handles.lock();
        for h in msg.handles {
            match table.insert(h) {
                Ok(v) => values.push(v),
                // Out of handle slots: the remaining handles are dropped.
                Err(_) => values.push(vabi::INVALID_HANDLE),
            }
        }
    }
    user::copy_to_user(bytes as u64, &msg.data)?;
    let hv: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    user::copy_to_user(handles_ptr as u64, &hv)?;
    user::write(actual as u64, &[msg.data.len() as u32, values.len() as u32])?;
    ok(msg.data.len())
}

pub fn event_create() -> SysResult {
    let rights = Rights(Rights::BASIC.0 | Rights::SIGNAL.0);
    ok(insert(KObject::Event(Event::new()), rights)? as usize)
}
