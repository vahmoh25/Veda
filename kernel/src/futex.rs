//! Futexes: the kernel half of user-space mutexes and condition variables.
//!
//! A futex is identified by (process, virtual address). `wait` blocks only if
//! the 32-bit word at the address still holds the expected value, which
//! closes the race between user space checking a lock word and sleeping.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

use vabi::Error;

use crate::mm::user;
use crate::sched::{self, Thread, WakeReason};
use crate::sync::SpinLock;

static QUEUES: SpinLock<BTreeMap<(u64, u64), VecDeque<Arc<Thread>>>> = SpinLock::new(BTreeMap::new());

fn key(addr: u64) -> Result<(u64, u64), Error> {
    if addr % 4 != 0 {
        return Err(Error::InvalidArgs);
    }
    let cur = sched::current();
    let p = cur.process.as_ref().ok_or(Error::BadState)?;
    Ok((p.koid, addr))
}

pub fn wait(addr: u64, expected: u32, deadline: Option<u64>) -> Result<(), Error> {
    let k = key(addr)?;
    let value: u32 = user::read(addr)?;
    if value != expected {
        return Err(Error::ShouldWait);
    }
    let cur = sched::current();
    QUEUES.lock().entry(k).or_default().push_back(cur.clone());
    let reason = sched::block(deadline);
    // Remove ourselves if we were not dequeued by a waker.
    {
        let mut q = QUEUES.lock();
        if let Some(list) = q.get_mut(&k) {
            list.retain(|t| t.koid != cur.koid);
            if list.is_empty() {
                q.remove(&k);
            }
        }
    }
    match reason {
        WakeReason::TimedOut => Err(Error::TimedOut),
        WakeReason::Killed => Err(Error::Canceled),
        _ => Ok(()),
    }
}

pub fn wake(addr: u64, count: usize) -> Result<usize, Error> {
    let k = key(addr)?;
    let mut woken = alloc::vec::Vec::new();
    {
        let mut q = QUEUES.lock();
        if let Some(list) = q.get_mut(&k) {
            while woken.len() < count {
                match list.pop_front() {
                    Some(t) => woken.push(t),
                    None => break,
                }
            }
            if list.is_empty() {
                q.remove(&k);
            }
        }
    }
    let n = woken.len();
    for t in woken {
        sched::wake(&t, WakeReason::Signaled);
    }
    Ok(n)
}
