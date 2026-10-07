//! Signals.
//!
//! Veda delivers no asynchronous signals: other processes stop a program
//! by killing it (which is what `SIGKILL`, and `SIGINT` from the terminal,
//! do anyway). What remains is what a program does to itself — `raise`,
//! `abort`, `SIGPIPE` from writing to a closed pipe — and that is handled
//! here faithfully: dispositions (per process), the signal mask and pending
//! signals (per thread), and handlers called synchronously on the raising
//! thread once the signal is not blocked.
//!
//! A thread is known by its thread pointer: `%fs:0`, which the x86-64 TLS
//! ABI makes point at itself, unique while the thread lives.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};

use vrt::sync::Mutex;

use crate::error::SysResult;
use crate::linux::errno::{EAGAIN, EINTR, EINVAL};
use crate::linux::{Sigaction, Siginfo, Timespec, sig};
use crate::{process, time, user};

/// A thread's signal state.
#[derive(Default, Clone, Copy)]
struct ThreadSignals {
    mask: u64,
    pending: u64,
}

struct State {
    actions: [Sigaction; sig::NSIG as usize],
    threads: BTreeMap<usize, ThreadSignals>,
}

static STATE: Mutex<State> = Mutex::new(State {
    actions: [Sigaction { handler: sig::DFL, flags: 0, restorer: 0, mask: 0 }; sig::NSIG as usize],
    threads: BTreeMap::new(),
});

/// Thread pointers exist (the C library has set up thread-local storage).
static THREADS_READY: AtomicBool = AtomicBool::new(false);

/// From now on threads have thread pointers.
pub fn threads_ready() {
    THREADS_READY.store(true, Ordering::Release);
}

/// The calling thread's thread pointer (0 before there are any).
fn current() -> usize {
    if !THREADS_READY.load(Ordering::Acquire) {
        return 0;
    }
    let tp: usize;
    // SAFETY: with TLS set up, %fs:0 holds the thread pointer itself.
    unsafe { core::arch::asm!("mov {}, qword ptr fs:[0]", out(reg) tp, options(nostack, readonly, preserves_flags)) };
    tp
}

/// The calling thread has ended: forget its state.
pub fn thread_exited() {
    let tp = current();
    STATE.lock().threads.remove(&tp);
}

/// A thread with thread pointer `tp` is starting: it has a fresh state.
pub fn thread_started(tp: usize) {
    STATE.lock().threads.remove(&tp);
}

fn bit(s: u32) -> u64 {
    1 << (s - 1)
}

/// Signals that can be neither caught, ignored nor blocked.
const UNCATCHABLE: u64 = (1 << (sig::KILL - 1)) | (1 << (sig::STOP - 1));

fn valid(s: u32) -> bool {
    (1..sig::NSIG).contains(&s)
}

/// What a signal does when nothing handles it: true if it ends the process.
fn terminates(s: u32) -> bool {
    !matches!(s, sig::CHLD | sig::URG | sig::WINCH | sig::CONT | sig::STOP | sig::TSTP | sig::TTIN | sig::TTOU)
}

impl State {
    fn thread(&mut self, tp: usize) -> &mut ThreadSignals {
        self.threads.entry(tp).or_default()
    }
}

pub unsafe fn rt_sigaction(s: u32, act: usize, old: usize, size: usize) -> SysResult {
    if !valid(s) || size != 8 {
        return Err(EINVAL);
    }
    let new: Option<Sigaction> = if act != 0 {
        // SAFETY: the program passed a sigaction.
        Some(unsafe { user::read(act)? })
    } else {
        None
    };
    if new.is_some() && bit(s) & UNCATCHABLE != 0 {
        return Err(EINVAL);
    }
    let prev = {
        let mut st = STATE.lock();
        let prev = st.actions[s as usize];
        if let Some(a) = new {
            st.actions[s as usize] = a;
            // Ignoring a pending signal discards it.
            if a.handler == sig::IGN {
                for t in st.threads.values_mut() {
                    t.pending &= !bit(s);
                }
            }
        }
        prev
    };
    if old != 0 {
        // SAFETY: as above.
        unsafe { user::write(old, prev)? };
    }
    Ok(0)
}

pub unsafe fn rt_sigprocmask(how: u32, set: usize, old: usize, size: usize) -> SysResult {
    if size != 8 {
        return Err(EINVAL);
    }
    let new = if set != 0 {
        // SAFETY: the program passed a sigset.
        Some(unsafe { user::read::<u64>(set)? })
    } else {
        None
    };
    let tp = current();
    let prev = {
        let mut st = STATE.lock();
        let t = st.thread(tp);
        let prev = t.mask;
        if let Some(v) = new {
            t.mask = match how {
                sig::BLOCK => prev | v,
                sig::UNBLOCK => prev & !v,
                sig::SETMASK => v,
                _ => return Err(EINVAL),
            } & !UNCATCHABLE;
        }
        prev
    };
    if old != 0 {
        // SAFETY: as above.
        unsafe { user::write(old, prev)? };
    }
    deliver_pending();
    Ok(0)
}

pub unsafe fn rt_sigpending(set: usize) -> SysResult {
    let tp = current();
    let pending = STATE.lock().thread(tp).pending;
    // SAFETY: the program passed a sigset.
    unsafe { user::write(set, pending)? };
    Ok(0)
}

/// Sends signal `s` to the calling thread.
pub fn raise(s: u32) {
    if !valid(s) {
        return;
    }
    let tp = current();
    {
        let mut st = STATE.lock();
        if st.actions[s as usize].handler == sig::IGN && bit(s) & UNCATCHABLE == 0 {
            return;
        }
        st.thread(tp).pending |= bit(s);
    }
    deliver_pending();
}

/// Acts on the calling thread's pending signals that are not blocked.
pub fn deliver_pending() {
    let tp = current();
    loop {
        let (s, action) = {
            let mut st = STATE.lock();
            let t = st.thread(tp);
            let ready = t.pending & !t.mask;
            if ready == 0 {
                return;
            }
            let s = ready.trailing_zeros() + 1;
            t.pending &= !bit(s);
            let action = st.actions[s as usize];
            match action.handler {
                sig::IGN => continue,
                sig::DFL => (s, None),
                _ => {
                    if action.flags & sig::SA_RESETHAND != 0 {
                        st.actions[s as usize].handler = sig::DFL;
                    }
                    (s, Some(action))
                }
            }
        };
        match action {
            None => {
                if terminates(s) {
                    process::exit_by_signal(s);
                }
            }
            Some(a) => call_handler(tp, s, &a),
        }
    }
}

/// Runs a handler on this thread, with the signal (and the handler's mask)
/// blocked while it runs.
fn call_handler(tp: usize, s: u32, a: &Sigaction) {
    let saved = {
        let mut st = STATE.lock();
        let t = st.thread(tp);
        let saved = t.mask;
        let mut block = a.mask;
        if a.flags & sig::SA_NODEFER == 0 {
            block |= bit(s);
        }
        t.mask |= block & !UNCATCHABLE;
        saved
    };
    if a.flags & sig::SA_SIGINFO != 0 {
        let mut info = Siginfo { signo: s as i32, pid: process::pid(), uid: crate::vfs::UID, ..Default::default() };
        // A context of zeros: nothing interrupted the thread.
        let mut context = [0u64; 128];
        // SAFETY: the program installed this three-argument handler.
        let f: extern "C" fn(i32, *mut Siginfo, *mut u64) = unsafe { core::mem::transmute(a.handler) };
        f(s as i32, &mut info, context.as_mut_ptr());
    } else {
        // SAFETY: the program installed this handler.
        let f: extern "C" fn(i32) = unsafe { core::mem::transmute(a.handler) };
        f(s as i32);
    }
    STATE.lock().thread(tp).mask = saved;
}

/// Waits for a signal: none ever comes from outside.
pub fn pause_forever() -> ! {
    loop {
        vrt::time::sleep_until(vabi::DEADLINE_INFINITE);
    }
}

pub unsafe fn rt_sigsuspend(mask: usize) -> SysResult {
    // SAFETY: the program passed a sigset.
    let m: u64 = unsafe { user::read(mask)? };
    let tp = current();
    let (saved, had) = {
        let mut st = STATE.lock();
        let t = st.thread(tp);
        let saved = t.mask;
        t.mask = m & !UNCATCHABLE;
        (saved, t.pending & !t.mask != 0)
    };
    deliver_pending();
    STATE.lock().thread(tp).mask = saved;
    if had {
        return Err(EINTR);
    }
    pause_forever()
}

/// `sigtimedwait`: takes a pending signal of `set`, waiting until `timeout`.
pub unsafe fn rt_sigtimedwait(set: usize, info: usize, timeout: usize) -> SysResult {
    // SAFETY: the program passed a sigset.
    let want: u64 = unsafe { user::read(set)? };
    let tp = current();
    let taken = {
        let mut st = STATE.lock();
        let t = st.thread(tp);
        let ready = t.pending & want;
        (ready != 0).then(|| {
            let s = ready.trailing_zeros() + 1;
            t.pending &= !bit(s);
            s
        })
    };
    if let Some(s) = taken {
        if info != 0 {
            // SAFETY: the program passed a siginfo.
            unsafe { user::write(info, Siginfo { signo: s as i32, pid: process::pid(), ..Default::default() })? };
        }
        return Ok(s as usize);
    }
    if timeout == 0 {
        pause_forever();
    }
    // SAFETY: the program passed a timespec.
    let t: Timespec = unsafe { user::read(timeout)? };
    vrt::time::sleep_until(time::monotonic_ns().saturating_add(t.to_ns().ok_or(EINVAL)?));
    Err(EAGAIN)
}

/// `kill`/`tkill`/`tgkill` aimed at this process.
pub fn send_self(s: u32) -> SysResult {
    if s == 0 {
        return Ok(0);
    }
    if !valid(s) {
        return Err(EINVAL);
    }
    raise(s);
    Ok(0)
}

pub unsafe fn sigaltstack(old: usize) -> SysResult {
    if old != 0 {
        // stack_t { ss_sp, ss_flags = SS_DISABLE, ss_size }.
        // SAFETY: the program passed a stack_t.
        unsafe { user::write::<[usize; 3]>(old, [0, 2, 0])? };
    }
    Ok(0)
}
