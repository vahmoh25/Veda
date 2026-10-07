//! Processes and threads.

use core::sync::atomic::Ordering;

use vabi::{Error, ProcessInfo, RawHandle, Rights, resource_kind};

use super::{SysResult, current_process, get_process, get_resource, get_thread, insert, ok, read_str, target_process};
use crate::mm::user;
use crate::object::KObject;
use crate::object::handle::Handle;
use crate::object::process::{self, Process};
use crate::sched::{self, State, Thread};

const PROCESS_RIGHTS: Rights = Rights(Rights::BASIC.0 | Rights::MANAGE.0 | Rights::GET_INFO.0);
const THREAD_RIGHTS: Rights = Rights(Rights::BASIC.0 | Rights::MANAGE.0 | Rights::GET_INFO.0);

fn is_user_address(a: usize) -> bool {
    (vabi::USER_SPACE_START..vabi::USER_SPACE_END).contains(&a)
}

pub fn process_create(name: usize, len: usize) -> SysResult {
    let name = read_str(name, len, vabi::NAME_MAX * 4)?;
    let parent = current_process()?;
    let p = Process::new(&name, parent.koid).ok_or(Error::NoMemory)?;
    ok(insert(KObject::Process(p), PROCESS_RIGHTS)? as usize)
}

fn start_thread(t: &Thread, entry: usize, stack: usize, arg0: u64, arg1: u64) -> Result<(), Error> {
    if !is_user_address(entry) || !is_user_address(stack) {
        return Err(Error::InvalidArgs);
    }
    if t.state() != State::New {
        return Err(Error::BadState);
    }
    t.prepare_user_entry(entry as u64, stack as u64, arg0, arg1);
    Ok(())
}

pub fn process_start(
    proc: RawHandle,
    thread: RawHandle,
    entry: usize,
    stack: usize,
    arg: RawHandle,
    arg1: usize,
) -> SysResult {
    let p = get_process(proc, Rights::MANAGE)?;
    let t = get_thread(thread, Rights::MANAGE)?;
    if !t.process.as_ref().is_some_and(|tp| tp.koid == p.koid) {
        return Err(Error::InvalidArgs);
    }
    if !is_user_address(entry) || !is_user_address(stack) || t.state() != State::New {
        return Err(Error::InvalidArgs);
    }
    // Move the bootstrap handle into the new process.
    let child_handle = if arg != vabi::INVALID_HANDLE {
        let h: Handle = current_process()?.handles.lock().remove(arg)?;
        p.handles.lock().insert(h)?
    } else {
        vabi::INVALID_HANDLE
    };
    start_thread(&t, entry, stack, child_handle as u64, arg1 as u64)?;
    p.mark_started();
    sched::make_ready(&t);
    ok(0)
}

pub fn process_exit(code: i64) -> SysResult {
    let p = current_process()?;
    p.kill(code);
    sched::exit_current()
}

/// Ends the process; with `kill_flags::DESCENDANTS`, the processes it
/// started (and so on) as well: its handle manages its whole job.
pub fn process_kill(raw: RawHandle, flags: usize) -> SysResult {
    if flags & !vabi::kill_flags::DESCENDANTS != 0 {
        return Err(Error::InvalidArgs);
    }
    let p = get_process(raw, Rights::MANAGE)?;
    let mut victims = alloc::vec![p.clone()];
    if flags & vabi::kill_flags::DESCENDANTS != 0 {
        victims.extend(process::descendants(p.koid));
    }
    let me = current_process()?.koid;
    for v in &victims {
        v.kill(vabi::EXIT_CODE_KILLED);
    }
    if victims.iter().any(|v| v.koid == me) {
        sched::exit_current();
    }
    ok(0)
}

pub fn thread_create(proc: RawHandle, name: usize, len: usize) -> SysResult {
    let p = target_process(proc)?;
    let name = read_str(name, len, vabi::NAME_MAX * 4)?;
    let t = Thread::new_user(&p, &name).ok_or(Error::NoMemory)?;
    if !p.add_thread(t.clone()) {
        return Err(Error::BadState);
    }
    ok(insert(KObject::Thread(t), THREAD_RIGHTS)? as usize)
}

pub fn thread_start(raw: RawHandle, entry: usize, stack: usize, arg0: usize, arg1: usize) -> SysResult {
    let t = get_thread(raw, Rights::MANAGE)?;
    start_thread(&t, entry, stack, arg0 as u64, arg1 as u64)?;
    if let Some(p) = &t.process {
        p.mark_started();
    }
    sched::make_ready(&t);
    ok(0)
}

pub fn thread_set_exit_futex(addr: usize) -> SysResult {
    if addr != 0 && (!is_user_address(addr) || !addr.is_multiple_of(4)) {
        return Err(Error::InvalidArgs);
    }
    sched::current().exit_futex.store(addr as u64, Ordering::Relaxed);
    ok(0)
}

/// Ends the calling thread. Its exit futex word (if any) is cleared and a
/// waiter woken now that the thread is off its user stack for good; a bad
/// address is ignored, as there is no one left to report it to.
pub fn thread_exit() -> SysResult {
    let addr = sched::current().exit_futex.swap(0, Ordering::Relaxed);
    if addr != 0 && user::write(addr, &0u32).is_ok() {
        let _ = crate::futex::wake(addr, 1);
    }
    sched::exit_current()
}

pub fn thread_set_priority(raw: RawHandle, prio: usize) -> SysResult {
    let t = get_thread(raw, Rights::MANAGE)?;
    if prio == 0 || prio > vabi::priority::REALTIME {
        return Err(Error::InvalidArgs);
    }
    t.sched.lock().priority = prio as u8;
    ok(0)
}

pub fn thread_set_fs_base(value: usize) -> SysResult {
    if value != 0 && !is_user_address(value) {
        return Err(Error::InvalidArgs);
    }
    let cur = sched::current();
    // SAFETY: the current thread's context, under the BKL.
    unsafe { (*cur.ctx_ptr()).fs_base = value as u64 };
    // SAFETY: loading a user-space address into FS.base.
    unsafe { crate::arch::cpu::wrmsr(crate::arch::cpu::MSR_FS_BASE, value as u64) };
    ok(0)
}

pub fn process_list(buf: usize, cap: usize) -> SysResult {
    let all = process::all();
    let size = core::mem::size_of::<ProcessInfo>();
    for (i, p) in all.iter().take(cap).enumerate() {
        user::write((buf + i * size) as u64, &p.info())?;
    }
    ok(all.len())
}

pub fn process_open(res: RawHandle, koid: u64) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    if !r.permits(resource_kind::PROCESS, 0, 0) {
        return Err(Error::AccessDenied);
    }
    let p = process::find(koid).ok_or(Error::NotFound)?;
    ok(insert(KObject::Process(p), PROCESS_RIGHTS)? as usize)
}
