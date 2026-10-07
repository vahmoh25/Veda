//! File descriptors.
//!
//! A descriptor refers to an open file description ([`Description`]),
//! shared by the descriptors `dup` makes and, through the handles behind
//! it, by other processes:
//!
//! | Object | Behind it | Passed to children as |
//! |--------|-----------|-----------------------|
//! | [`Object::File`] | a `file` connection to the VFS (which keeps the offset) | a duplicated connection |
//! | [`Object::Dir`] | a directory's path | (not passed) |
//! | [`Object::Stream`] | a socket endpoint: a pipe or the terminal | a duplicated handle |
//! | [`Object::Null`] | nothing: reads end at once, writes vanish | (not passed) |
//! | [`Object::Log`] | the kernel log (output of programs without a terminal) | (not passed) |
//!
//! The table itself is per process; descriptors carry only the close-on-exec
//! flag.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vrt::sync::Mutex;

use crate::file::{Directory, VfsFile};
use crate::linux::errno::{EBADF, EMFILE};
use crate::linux::o;
use crate::stream::Stream;

/// Descriptors per process (`RLIMIT_NOFILE`).
pub const MAX_FDS: usize = 1024;

pub enum Object {
    File(VfsFile),
    Dir(Directory),
    Stream(Stream),
    Null,
    Log,
}

/// An open file description.
pub struct Description {
    pub object: Object,
    /// The access mode and status flags (`O_APPEND`, `O_NONBLOCK`).
    flags: AtomicU32,
}

impl Description {
    pub fn new(object: Object, flags: u32) -> Arc<Description> {
        Arc::new(Description { object, flags: AtomicU32::new(flags & STATUS_FLAGS) })
    }

    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// Changes the flags `fcntl(F_SETFL)` may change.
    pub fn set_status(&self, flags: u32) {
        let keep = self.flags() & !SETTABLE;
        self.flags.store(keep | (flags & SETTABLE), Ordering::Relaxed);
    }

    pub fn readable(&self) -> bool {
        self.flags() & o::ACCMODE != o::WRONLY
    }

    pub fn writable(&self) -> bool {
        self.flags() & o::ACCMODE != o::RDONLY
    }

    pub fn nonblocking(&self) -> bool {
        self.flags() & o::NONBLOCK != 0
    }
}

/// The flags a description keeps.
const STATUS_FLAGS: u32 = o::ACCMODE | o::APPEND | o::NONBLOCK | o::PATH;
/// The flags `F_SETFL` changes.
const SETTABLE: u32 = o::APPEND | o::NONBLOCK;

#[derive(Clone)]
struct Slot {
    desc: Arc<Description>,
    cloexec: bool,
}

pub struct Table {
    slots: Vec<Option<Slot>>,
}

static TABLE: Mutex<Table> = Mutex::new(Table { slots: Vec::new() });

/// The description behind `fd`.
pub fn get(fd: i32) -> Result<Arc<Description>, isize> {
    let t = TABLE.lock();
    usize::try_from(fd).ok().and_then(|i| t.slots.get(i)).and_then(|s| s.as_ref()).map(|s| s.desc.clone()).ok_or(EBADF)
}

/// Installs `desc` at the lowest free descriptor at least `min`.
pub fn insert(desc: Arc<Description>, cloexec: bool, min: usize) -> Result<i32, isize> {
    let mut t = TABLE.lock();
    let free = (min..MAX_FDS).find(|&i| t.slots.get(i).is_none_or(|s| s.is_none())).ok_or(EMFILE)?;
    if t.slots.len() <= free {
        t.slots.resize(free + 1, None);
    }
    t.slots[free] = Some(Slot { desc, cloexec });
    Ok(free as i32)
}

/// Installs `desc` at `fd`, closing what was there.
pub fn replace(fd: i32, desc: Arc<Description>, cloexec: bool) -> Result<(), isize> {
    let i = usize::try_from(fd).ok().filter(|&i| i < MAX_FDS).ok_or(EBADF)?;
    let old = {
        let mut t = TABLE.lock();
        if t.slots.len() <= i {
            t.slots.resize(i + 1, None);
        }
        t.slots[i].replace(Slot { desc, cloexec })
    };
    // Closing may talk to a service: not under the lock.
    drop(old);
    Ok(())
}

/// Closes `fd`.
pub fn remove(fd: i32) -> Result<(), isize> {
    let old = {
        let mut t = TABLE.lock();
        usize::try_from(fd).ok().and_then(|i| t.slots.get_mut(i)).and_then(|s| s.take()).ok_or(EBADF)?
    };
    drop(old);
    Ok(())
}

/// Closes every descriptor in `first..=last` (`close_range`).
pub fn remove_range(first: usize, last: usize) {
    let old: Vec<Slot> = {
        let mut t = TABLE.lock();
        let end = t.slots.len().min(last.saturating_add(1));
        (first.min(end)..end).filter_map(|i| t.slots[i].take()).collect()
    };
    drop(old);
}

/// Sets close-on-exec on every descriptor in `first..=last`.
pub fn set_cloexec_range(first: usize, last: usize) {
    let mut t = TABLE.lock();
    let end = t.slots.len().min(last.saturating_add(1));
    for s in t.slots[first.min(end)..end].iter_mut().flatten() {
        s.cloexec = true;
    }
}

pub fn cloexec(fd: i32) -> Result<bool, isize> {
    let t = TABLE.lock();
    usize::try_from(fd).ok().and_then(|i| t.slots.get(i)).and_then(|s| s.as_ref()).map(|s| s.cloexec).ok_or(EBADF)
}

pub fn set_cloexec(fd: i32, on: bool) -> Result<(), isize> {
    let mut t = TABLE.lock();
    let s = usize::try_from(fd).ok().and_then(|i| t.slots.get_mut(i)).and_then(|s| s.as_mut()).ok_or(EBADF)?;
    s.cloexec = on;
    Ok(())
}

/// Every open descriptor: `(fd, description, close-on-exec)`.
pub fn snapshot() -> Vec<(i32, Arc<Description>, bool)> {
    let t = TABLE.lock();
    t.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i as i32, s.desc.clone(), s.cloexec))).collect()
}
