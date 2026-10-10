//! The bridge, the monitor's side: the guest's handles, and the operations
//! on them (`vhv::bridge` describes them).
//!
//! The guest's handles are numbers that this table gives meaning to: each
//! stands for a handle of this process that the guest was given or made.
//! Operations are the system calls of their names, carried out on those
//! handles; handles that messages carry move between the table and Veda's
//! channels. A wait that cannot finish at once goes on in the waiter
//! thread (`run_waits`), which writes what it observed into the guest's
//! request when it finishes, reports it in the notification ring, and
//! raises the guest's interrupt. Pending waits name the guest's handles
//! as the guest does, and are resolved against the table at each round:
//! a handle the guest closes meanwhile ends its wait (`BadHandle`).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::mem::ManuallyDrop;

use vabi::{Error, RawHandle, Rights, WaitItem, map_flags, signals};
use vhv::bridge::{self as b, op};
use vrt::object::{Channel, Event, Guest, Handle, Vcpu, Vmo};
use vrt::sync::Mutex;

use crate::memory::{GuestMemory, Plain};
use crate::registry::Registry;

/// The most handles a guest holds at a time.
const MAX_HANDLES: usize = 16384;
/// The most bytes a VMO the guest makes may have.
const MAX_VMO: u64 = 1 << 30;
/// VMOs the guest maps go in this range of its physical address space,
/// beyond its RAM.
const WINDOW_BASE: u64 = 64 << 30;
const WINDOW_SIZE: u64 = 64 << 30;

pub struct Bridge {
    table: Mutex<Table>,
    waits: Mutex<Waits>,
    /// Signaled when the waits change, for the waiter thread.
    wake: Event,
    windows: Mutex<Windows>,
    registry: Registry,
}

/// The guest's handles, by the numbers it knows them by.
struct Table {
    next: u32,
    handles: BTreeMap<u32, Handle>,
}

impl Table {
    fn insert(&mut self, h: Handle) -> Result<u32, Error> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(Error::LimitReached);
        }
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            if !self.handles.contains_key(&self.next) {
                self.handles.insert(self.next, h);
                return Ok(self.next);
            }
        }
    }

    fn raw(&self, id: u32) -> Result<RawHandle, Error> {
        self.handles.get(&id).map(Handle::raw).ok_or(Error::BadHandle)
    }

    /// `items` with this process's handles for the guest's.
    fn resolve(&self, items: &[WaitItem]) -> Result<Vec<WaitItem>, Error> {
        items.iter().map(|i| Ok(WaitItem { handle: self.raw(i.handle)?, ..*i })).collect()
    }
}

/// A wait that goes on: its items (with the guest's handle numbers), and
/// where they are in the guest.
#[derive(Clone)]
struct Pending {
    key: u64,
    items_gpa: u64,
    items: Vec<WaitItem>,
    deadline: u64,
}

struct Waits {
    pending: Vec<Pending>,
    /// Finished waits the ring has had no room for yet.
    done: VecDeque<b::Completion>,
    /// The ring (guest-physical) and the interrupt that announces it.
    ring: Option<(u64, u8)>,
    head: u32,
}

/// The ranges of the window that VMOs are mapped at.
struct Windows {
    free: Vec<(u64, u64)>,
    used: BTreeMap<u64, u64>,
}

impl Windows {
    fn take(&mut self, len: u64) -> Option<u64> {
        let i = self.free.iter().position(|&(_, l)| l >= len)?;
        let (start, l) = self.free[i];
        if l == len {
            self.free.remove(i);
        } else {
            self.free[i] = (start + len, l - len);
        }
        self.used.insert(start, len);
        Some(start)
    }

    fn give_back(&mut self, start: u64) -> Option<u64> {
        let len = self.used.remove(&start)?;
        self.free.push((start, len));
        self.free.sort_unstable();
        // Neighbours join again.
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free.len());
        for &(s, l) in &self.free {
            match merged.last_mut() {
                Some((ms, ml)) if *ms + *ml == s => *ml += l,
                _ => merged.push((s, l)),
            }
        }
        self.free = merged;
        Some(len)
    }
}

/// A handle of the table, seen as an object of type `T` (not closed when
/// the view goes).
fn view<T>(raw: RawHandle, wrap: fn(Handle) -> T) -> ManuallyDrop<T> {
    // SAFETY: the table owns the handle; the view never closes it.
    ManuallyDrop::new(wrap(unsafe { Handle::from_raw(raw) }))
}

fn status(r: Result<(), Error>) -> u32 {
    match r {
        Ok(()) => b::OK,
        Err(e) => e as u32,
    }
}

impl Bridge {
    pub fn new() -> Result<Bridge, Error> {
        Ok(Bridge {
            table: Mutex::new(Table { next: 0, handles: BTreeMap::new() }),
            waits: Mutex::new(Waits { pending: Vec::new(), done: VecDeque::new(), ring: None, head: 0 }),
            wake: Event::create()?,
            windows: Mutex::new(Windows { free: alloc::vec![(WINDOW_BASE, WINDOW_SIZE)], used: BTreeMap::new() }),
            registry: Registry::new()?,
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Carries out operation `op` with its request at `gpa`: 0, or the
    /// platform's error if the request is not in the guest's memory.
    pub fn call(&self, memory: &GuestMemory, guest: &Guest, op: u32, gpa: u64) -> u64 {
        fn request<T: Plain>(memory: &GuestMemory, gpa: u64, f: impl FnOnce(&mut T)) -> u64 {
            let Some(mut r) = memory.read_obj::<T>(gpa) else { return vhv::platform::error::INVALID };
            f(&mut r);
            if memory.write_obj(gpa, &r) { 0 } else { vhv::platform::error::INVALID }
        }
        match op {
            op::SETUP => request(memory, gpa, |r: &mut b::Setup| r.status = status(self.setup(memory, r))),
            op::CLOSE => request(memory, gpa, |r: &mut b::Close| {
                r.status = status(self.table.lock().handles.remove(&r.handle).map(drop).ok_or(Error::BadHandle))
            }),
            op::DUPLICATE => request(memory, gpa, |r: &mut b::Duplicate| r.status = status(self.duplicate(r))),
            op::OBJECT_INFO => request(memory, gpa, |r: &mut b::ObjectInfo| r.status = status(self.object_info(r))),
            op::SIGNAL => request(memory, gpa, |r: &mut b::Signal| {
                let table = self.table.lock();
                r.status = status(table.raw(r.handle).and_then(|raw| view(raw, |h| h).signal(r.clear, r.set)));
            }),
            op::WAIT => request(memory, gpa, |r: &mut b::Wait| {
                if let Err(e) = self.wait(memory, r) {
                    r.status = e as u32;
                }
            }),
            op::CANCEL => request(memory, gpa, |r: &mut b::Cancel| {
                let mut waits = self.waits.lock();
                let before = waits.pending.len();
                waits.pending.retain(|p| p.key != r.key);
                r.status = if waits.pending.len() < before { b::OK } else { Error::NotFound as u32 };
            }),
            op::CHANNEL_CREATE => request(memory, gpa, |r: &mut b::ChannelCreate| {
                r.status = status(self.channel_create(r));
            }),
            op::CHANNEL_WRITE => {
                request(memory, gpa, |r: &mut b::ChannelWrite| r.status = status(self.channel_write(memory, r)))
            }
            op::CHANNEL_READ => {
                request(memory, gpa, |r: &mut b::ChannelRead| r.status = status(self.channel_read(memory, r)))
            }
            op::EVENT_CREATE => request(memory, gpa, |r: &mut b::EventCreate| {
                r.status = status(Event::create().and_then(|e| self.insert(e.into_handle())).map(|id| r.out = id));
            }),
            op::VMO_CREATE => request(memory, gpa, |r: &mut b::Vmo| r.status = status(self.vmo_create(r))),
            op::VMO_SIZE => request(memory, gpa, |r: &mut b::Vmo| {
                let table = self.table.lock();
                let size = table.raw(r.handle).and_then(|raw| view(raw, Vmo::from_handle).size());
                r.status = status(size.map(|s| r.size = s as u64));
            }),
            op::VMO_READ | op::VMO_WRITE => request(memory, gpa, |r: &mut b::VmoCopy| {
                r.status = status(self.vmo_copy(memory, r, op == op::VMO_WRITE));
            }),
            op::VMO_MAP => request(memory, gpa, |r: &mut b::VmoMap| r.status = status(self.vmo_map(guest, r))),
            op::VMO_UNMAP => request(memory, gpa, |r: &mut b::VmoMap| {
                let len = self.windows.lock().used.get(&r.gpa).copied();
                r.status = status(match len {
                    Some(len) if len == r.len => guest.unmap(r.gpa, len as usize).map(|_| {
                        self.windows.lock().give_back(r.gpa);
                    }),
                    _ => Err(Error::NotFound),
                });
            }),
            op::BOOTSTRAP => request(memory, gpa, |r: &mut b::Bootstrap| r.status = status(self.bootstrap(r))),
            op::CLOCK => request(memory, gpa, |r: &mut b::Clock| {
                r.status = status(vrt::time::clock_info().map(|info| r.info = info));
            }),
            _ => vhv::platform::error::UNKNOWN,
        }
    }

    fn insert(&self, h: Handle) -> Result<u32, Error> {
        self.table.lock().insert(h)
    }

    fn setup(&self, memory: &GuestMemory, r: &b::Setup) -> Result<(), Error> {
        if !r.ring.is_multiple_of(4096) || !memory.contains(r.ring, 4096) || !(32..=255).contains(&r.vector) {
            return Err(Error::InvalidArgs);
        }
        let mut waits = self.waits.lock();
        waits.ring = Some((r.ring, r.vector as u8));
        waits.head = 0;
        Ok(())
    }

    fn duplicate(&self, r: &mut b::Duplicate) -> Result<(), Error> {
        let mut table = self.table.lock();
        let raw = table.raw(r.handle)?;
        let rights = (r.rights != 0).then_some(Rights(r.rights));
        let h = view(raw, |h| h).duplicate(rights)?;
        r.out = table.insert(h)?;
        Ok(())
    }

    fn object_info(&self, r: &mut b::ObjectInfo) -> Result<(), Error> {
        let table = self.table.lock();
        let info = view(table.raw(r.handle)?, |h| h).basic_info()?;
        r.koid = info.koid;
        r.object_type = info.object_type;
        r.rights = info.rights;
        Ok(())
    }

    fn channel_create(&self, r: &mut b::ChannelCreate) -> Result<(), Error> {
        let (a, c) = Channel::create()?;
        let mut table = self.table.lock();
        let first = table.insert(a.into_handle())?;
        match table.insert(c.into_handle()) {
            Ok(second) => {
                r.out = [first, second];
                Ok(())
            }
            Err(e) => {
                table.handles.remove(&first);
                Err(e)
            }
        }
    }

    fn channel_write(&self, memory: &GuestMemory, r: &mut b::ChannelWrite) -> Result<(), Error> {
        if r.bytes_len > b::MAX_BYTES || r.handles_len > b::MAX_HANDLES {
            return Err(Error::InvalidArgs);
        }
        let mut bytes = alloc::vec![0u8; r.bytes_len as usize];
        let mut ids = alloc::vec![0u8; r.handles_len as usize * 4];
        if !memory.read(r.bytes, &mut bytes) || !memory.read(r.handles, &mut ids) {
            return Err(Error::Fault);
        }
        let ids: Vec<u32> = ids.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        let mut table = self.table.lock();
        let channel = table.raw(r.handle)?;
        // Every handle checked before any moves, as the kernel does.
        for (i, id) in ids.iter().enumerate() {
            if *id == r.handle || ids[..i].contains(id) {
                return Err(Error::InvalidArgs);
            }
            table.raw(*id)?;
        }
        let handles: Vec<Handle> = ids.iter().filter_map(|id| table.handles.remove(id)).collect();
        r.consumed = 1;
        view(channel, Channel::from_handle).write(&bytes, handles)
    }

    fn channel_read(&self, memory: &GuestMemory, r: &mut b::ChannelRead) -> Result<(), Error> {
        let (cap_bytes, cap_handles) = (r.bytes_capacity.min(b::MAX_BYTES), r.handles_capacity.min(b::MAX_HANDLES));
        // The guest's buffers first: a message read is never lost to them.
        if !memory.contains(r.bytes, cap_bytes as u64) || !memory.contains(r.handles, cap_handles as u64 * 4) {
            return Err(Error::Fault);
        }
        let mut bytes = alloc::vec![0u8; cap_bytes as usize];
        let mut raws = alloc::vec![0 as RawHandle; cap_handles as usize];
        let mut table = self.table.lock();
        let channel = view(table.raw(r.handle)?, Channel::from_handle);
        match channel.read_into(&mut bytes, &mut raws) {
            Ok((n, h)) => {
                // SAFETY: the kernel gave us these handles.
                let received: Vec<Handle> = raws[..h].iter().map(|&raw| unsafe { Handle::from_raw(raw) }).collect();
                let mut ids = Vec::with_capacity(h);
                for handle in received {
                    // A handle that does not fit the table is dropped.
                    ids.push(table.insert(handle).unwrap_or(0));
                }
                let id_bytes: Vec<u8> = ids.iter().flat_map(|id| id.to_le_bytes()).collect();
                memory.write(r.bytes, &bytes[..n]);
                memory.write(r.handles, &id_bytes);
                r.bytes_len = n as u32;
                r.handles_len = h as u32;
                Ok(())
            }
            Err((e, n, h)) => {
                r.bytes_len = n as u32;
                r.handles_len = h as u32;
                Err(e)
            }
        }
    }

    fn vmo_create(&self, r: &mut b::Vmo) -> Result<(), Error> {
        if r.size == 0 || r.size > MAX_VMO {
            return Err(Error::InvalidArgs);
        }
        let vmo = if r.flags & vabi::vmo_flags::COMMIT as u32 != 0 {
            Vmo::create_committed(r.size as usize)?
        } else {
            Vmo::create(r.size as usize)?
        };
        r.handle = self.insert(vmo.into_handle())?;
        Ok(())
    }

    fn vmo_copy(&self, memory: &GuestMemory, r: &b::VmoCopy, write: bool) -> Result<(), Error> {
        if r.len > b::MAX_COPY {
            return Err(Error::InvalidArgs);
        }
        let mut data = alloc::vec![0u8; r.len as usize];
        let table = self.table.lock();
        let vmo = view(table.raw(r.handle)?, Vmo::from_handle);
        if write {
            if !memory.read(r.buffer, &mut data) {
                return Err(Error::Fault);
            }
            vmo.write(r.offset as usize, &data)
        } else {
            if !memory.contains(r.buffer, r.len) {
                return Err(Error::Fault);
            }
            vmo.read(r.offset as usize, &mut data)?;
            memory.write(r.buffer, &data);
            Ok(())
        }
    }

    fn vmo_map(&self, guest: &Guest, r: &mut b::VmoMap) -> Result<(), Error> {
        if r.len == 0 || !r.len.is_multiple_of(4096) || !r.offset.is_multiple_of(4096) {
            return Err(Error::InvalidArgs);
        }
        let flags = (r.flags as usize & map_flags::WRITE) | map_flags::READ;
        let table = self.table.lock();
        let vmo = view(table.raw(r.handle)?, Vmo::from_handle);
        let gpa = self.windows.lock().take(r.len).ok_or(Error::NoMemory)?;
        if let Err(e) = guest.map(&vmo, r.offset as usize, r.len as usize, gpa, flags) {
            self.windows.lock().give_back(gpa);
            return Err(e);
        }
        r.gpa = gpa;
        Ok(())
    }

    fn bootstrap(&self, r: &mut b::Bootstrap) -> Result<(), Error> {
        if r.role != vabi::startup::role::REGISTRY {
            return Err(Error::NotFound);
        }
        r.out = self.insert(self.registry.connect()?.into_handle())?;
        Ok(())
    }

    /// A wait: finished at once if it can be, else it goes on in the
    /// waiter thread ([`b::PENDING`]).
    fn wait(&self, memory: &GuestMemory, r: &mut b::Wait) -> Result<(), Error> {
        if r.count == 0 || r.count > b::MAX_WAIT_ITEMS {
            return Err(Error::InvalidArgs);
        }
        let size = core::mem::size_of::<WaitItem>() as u64;
        let mut items: Vec<WaitItem> = Vec::with_capacity(r.count as usize);
        for i in 0..r.count as u64 {
            items.push(memory.read_obj::<WaitItem>(r.items + i * size).ok_or(Error::Fault)?);
        }
        let mut resolved = self.table.lock().resolve(&items)?;
        let polled = vrt::object::wait_many(&mut resolved, 0);
        let now = vrt::time::now_ns();
        match polled {
            Ok(n) if n > 0 => return finish(memory, r, &resolved, n as u32, b::OK),
            Ok(_) | Err(Error::TimedOut) if r.deadline <= now => {
                return finish(memory, r, &resolved, 0, Error::TimedOut as u32);
            }
            Ok(_) | Err(Error::TimedOut) => {}
            Err(e) => return Err(e),
        }
        // Waits that are pending, or finished and not yet in the ring, are
        // at most what the ring holds: a guest that takes nothing from it
        // cannot make the monitor (Veda's memory) keep more.
        let mut waits = self.waits.lock();
        if waits.pending.len() + waits.done.len() >= b::ring::CAPACITY as usize {
            return Err(Error::LimitReached);
        }
        waits.pending.push(Pending { key: r.key, items_gpa: r.items, items, deadline: r.deadline });
        drop(waits);
        r.status = b::PENDING;
        self.wake.signal()
    }

    /// The waiter thread: waits on every pending wait at once (and on the
    /// event that says they changed), finishes those whose signals came,
    /// whose deadline passed or whose handles went, and reports them.
    pub fn run_waits(&self, memory: &GuestMemory, notify: &Vcpu) -> ! {
        loop {
            // The waits as they are now (never with the table's lock: a
            // wait takes the two the other way round).
            let snapshot: Vec<Pending> = self.waits.lock().pending.clone();
            let mut items =
                alloc::vec![WaitItem { handle: self.wake.raw(), signals: signals::SIGNALED, ..WaitItem::default() }];
            let mut spans = BTreeMap::new();
            let mut gone = Vec::new();
            let mut deadline = u64::MAX;
            {
                let table = self.table.lock();
                for p in &snapshot {
                    match table.resolve(&p.items) {
                        Ok(resolved) => {
                            spans.insert(p.key, (items.len(), resolved.len()));
                            items.extend(resolved);
                            deadline = deadline.min(p.deadline);
                        }
                        Err(_) => gone.push(p.key),
                    }
                }
            }
            if gone.is_empty() {
                // (A handle closed meanwhile fails the whole wait: the
                // next round finds it gone.)
                let _ = vrt::object::wait_many(&mut items, deadline);
            }
            let _ = self.wake.clear();
            let now = vrt::time::now_ns();
            let mut waits = self.waits.lock();
            for mut p in core::mem::take(&mut waits.pending) {
                let status = if gone.contains(&p.key) {
                    Error::BadHandle as u32
                } else {
                    // A wait that came meanwhile was not waited on yet.
                    let Some(&(at, len)) = spans.get(&p.key) else {
                        waits.pending.push(p);
                        continue;
                    };
                    let observed = &items[at..at + len];
                    if !observed.iter().any(|o| o.observed & o.signals != 0) && now < p.deadline {
                        waits.pending.push(p);
                        continue;
                    }
                    for (mine, seen) in p.items.iter_mut().zip(observed) {
                        mine.observed = seen.observed;
                    }
                    if p.items.iter().any(|i| i.observed & i.signals != 0) { b::OK } else { Error::TimedOut as u32 }
                };
                let satisfied = p.items.iter().filter(|i| i.observed & i.signals != 0).count() as u32;
                let size = core::mem::size_of::<WaitItem>() as u64;
                for (i, item) in p.items.iter().enumerate() {
                    // Only the observed signals go back; the guest's
                    // handle numbers stay as it wrote them.
                    memory.write_obj(p.items_gpa + i as u64 * size + 8, &item.observed);
                }
                waits.done.push_back(b::Completion { key: p.key, status, satisfied });
            }
            if self.post(&mut waits, memory)
                && let Some((_, vector)) = waits.ring
            {
                let _ = notify.interrupt(vector);
            }
        }
    }

    /// Moves finished waits into the ring as far as it has room; whether
    /// any moved.
    fn post(&self, waits: &mut Waits, memory: &GuestMemory) -> bool {
        let Some((ring, _)) = waits.ring else { return false };
        let mut moved = false;
        while let Some(c) = waits.done.front().copied() {
            let tail = memory.read_obj::<u32>(ring + b::ring::TAIL as u64).unwrap_or(0);
            if waits.head.wrapping_sub(tail) >= b::ring::CAPACITY {
                break;
            }
            let slot = (waits.head % b::ring::CAPACITY) as u64;
            memory.write_obj(ring + b::ring::ENTRIES as u64 + slot * b::ring::ENTRY_SIZE as u64, &c);
            waits.head = waits.head.wrapping_add(1);
            memory.write_obj(ring + b::ring::HEAD as u64, &waits.head);
            waits.done.pop_front();
            moved = true;
        }
        moved
    }
}

/// Finishes a wait at once: what was observed goes back into the guest's
/// items.
fn finish(memory: &GuestMemory, r: &mut b::Wait, items: &[WaitItem], satisfied: u32, st: u32) -> Result<(), Error> {
    let size = core::mem::size_of::<WaitItem>() as u64;
    for (i, item) in items.iter().enumerate() {
        if !memory.write_obj(r.items + i as u64 * size + 8, &item.observed) {
            return Err(Error::Fault);
        }
    }
    r.satisfied = satisfied;
    r.status = st;
    Ok(())
}
