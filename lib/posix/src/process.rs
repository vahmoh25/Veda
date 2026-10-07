//! Processes: ids, starting programs (`posix_spawn`), waiting for them,
//! `kill` and exit.
//!
//! There is no `fork`: Veda creates a process from a program image. A
//! process id is the process's kernel object id (as [`thread::tid_of`]
//! keeps thread ids); the processes this one started are its children, the
//! only ones it can wait for or signal.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use vabi::startup::role;
use vabi::{WaitItem, signals};
use vrt::object::{Handle, Process, Vmo};
use vrt::process::{Spawn, SpawnError};
use vrt::sync::Mutex;

use crate::error::{self, SysResult};
use crate::fd::{self, Description, Object};
use crate::linux::errno::*;
use crate::linux::{Rusage, Siginfo, Timeval, sig, wait};
use crate::thread::tid_of;
use crate::{fs, path, tty, user, vfs};

static PID: AtomicI32 = AtomicI32::new(0);

/// This process's id.
pub fn pid() -> i32 {
    let p = PID.load(Ordering::Relaxed);
    if p != 0 {
        return p;
    }
    let p = vrt::object::process_self_info().map_or(1, |i| tid_of(i.koid));
    PID.store(p, Ordering::Relaxed);
    p
}

/// The id of the process that started this one.
pub fn ppid() -> i32 {
    vrt::object::process_self_info().map_or(0, |i| if i.parent_koid == 0 { 0 } else { tid_of(i.parent_koid) })
}

/// The processes this one started and has not waited for.
static CHILDREN: Mutex<BTreeMap<i32, Process>> = Mutex::new(BTreeMap::new());

/// Ends the process with `code` (`exit`).
pub fn exit(code: i32) -> ! {
    vrt::sys::process_exit((code & 0xff) as i64)
}

/// Ends the process as killed by signal `s`.
pub fn exit_by_signal(s: u32) -> ! {
    vrt::sys::process_exit(vabi::EXIT_CODE_SIGNALED - s as i64)
}

/// A wait status (`WIFEXITED`, `WIFSIGNALED`) for an exit code.
fn wait_status(code: i64) -> i32 {
    match vabi::exit_signal(code) {
        Some(s) => s as i32,
        None => ((code & 0xff) as i32) << 8,
    }
}

// --- posix_spawn ---------------------------------------------------------

/// musl's `struct fdop` (a file action of `posix_spawn`).
#[repr(C)]
struct FdOp {
    next: *const FdOp,
    prev: *const FdOp,
    cmd: i32,
    fd: i32,
    srcfd: i32,
    oflag: i32,
    mode: u32,
    // The path follows: `char path[]`, right after `mode` (before the
    // padding that rounds the struct's size up).
}

/// Where the path of a file action starts.
const FDOP_PATH: usize = core::mem::offset_of!(FdOp, mode) + size_of::<u32>();

const FDOP_CLOSE: i32 = 1;
const FDOP_DUP2: i32 = 2;
const FDOP_OPEN: i32 = 3;
const FDOP_CHDIR: i32 = 4;
const FDOP_FCHDIR: i32 = 5;

/// The descriptors and directory of a process about to start.
struct ChildState {
    fds: BTreeMap<i32, (Arc<Description>, bool)>,
    cwd: String,
}

impl ChildState {
    /// Applies one file action, as the child would before running.
    ///
    /// # Safety
    /// `op` must be one of musl's file actions.
    unsafe fn apply(&mut self, op: &FdOp) -> Result<(), isize> {
        // SAFETY: the path follows the action.
        let name = || unsafe { user::cstr(op as *const FdOp as usize + FDOP_PATH) };
        match op.cmd {
            FDOP_CLOSE => {
                self.fds.remove(&op.fd);
            }
            FDOP_DUP2 => {
                let (d, _) = self.fds.get(&op.srcfd).cloned().ok_or(EBADF)?;
                self.fds.insert(op.fd, (d, false));
            }
            FDOP_OPEN => {
                let r = path::resolve(&self.cwd, name()?)?;
                let d = fs::open_resolved(&r, op.oflag as u32)?;
                self.fds.insert(op.fd, (d, false));
            }
            FDOP_CHDIR => {
                let r = path::resolve(&self.cwd, name()?)?;
                if !vfs::stat(&r.path)?.is_dir {
                    return Err(ENOTDIR);
                }
                self.cwd = r.path;
            }
            FDOP_FCHDIR => {
                let (d, _) = self.fds.get(&op.fd).ok_or(EBADF)?;
                let Object::Dir(dir) = &d.object else { return Err(ENOTDIR) };
                self.cwd = dir.path.clone();
            }
            _ => return Err(EINVAL),
        }
        Ok(())
    }

    /// The handles that carry the child's descriptors.
    fn handles(&self) -> Result<Vec<(u32, Handle)>, isize> {
        let mut out = Vec::new();
        for (&n, (d, cloexec)) in &self.fds {
            if *cloexec || n < 0 || n as u32 >= role::MAX_FDS {
                continue;
            }
            let h = match &d.object {
                // The access the descriptor has and no more: the read end of
                // a pipe cannot be written in the child either.
                Object::Stream(s) => {
                    let mut rights = s.socket.0.basic_info().map_err(error::kernel)?.rights;
                    if !d.readable() {
                        rights &= !vabi::Rights::READ.0;
                    }
                    if !d.writable() {
                        rights &= !vabi::Rights::WRITE.0;
                    }
                    s.socket.0.duplicate(Some(vabi::Rights(rights))).map_err(error::kernel)?
                }
                Object::File(f) => f.duplicate()?.into_handle(),
                // Directories, the empty input and the log stay behind
                // (the child gets the defaults for 0, 1 and 2).
                _ => continue,
            };
            out.push((role::FD + n as u32, h));
        }
        Ok(out)
    }
}

/// The `errno` for a failure to start a program.
fn spawn_errno(e: SpawnError) -> isize {
    match e {
        SpawnError::NotExecutable | SpawnError::BadPe(_) | SpawnError::BadElf(_) => ENOEXEC,
        SpawnError::TooLarge => E2BIG,
        SpawnError::Kernel(vabi::Error::NoMemory) => ENOMEM,
        SpawnError::Kernel(e) => error::kernel(e),
    }
}

/// Starts the program at `name` with the descriptors and working directory
/// left by the file actions `ops`, which are carried out first, as the
/// child would (a relative `name` is found from the directory they leave).
///
/// # Safety
/// The arguments are those of musl's `posix_spawn`.
unsafe fn spawn(res: usize, name: usize, ops: usize, argv: usize, envp: usize) -> Result<(), isize> {
    let mut child = ChildState {
        fds: fd::snapshot().into_iter().map(|(n, d, cloexec)| (n, (d, cloexec))).collect(),
        cwd: vfs::cwd(),
    };
    if ops != 0 {
        // The list is built backwards: start from its tail.
        let mut op = ops as *const FdOp;
        // SAFETY: musl's list of file actions.
        unsafe {
            while !(*op).next.is_null() {
                op = (*op).next;
            }
            while !op.is_null() {
                child.apply(&*op)?;
                op = (*op).prev;
            }
        }
    }
    // SAFETY: the program passed a path.
    let resolved = path::resolve(&child.cwd, unsafe { user::cstr(name)? })?;
    let path = resolved.path;
    let st = vfs::stat(&path)?;
    if st.is_dir || !st.executable {
        return Err(EACCES);
    }

    let mut handles = child.handles()?;
    // A registry connection of its own; the terminal's state.
    let registry = vproto::with_registry(|r| r.clone_registry()).map_err(|_| EIO)?;
    match registry {
        Ok(Ok(ch)) => handles.push((role::REGISTRY, ch.into_handle())),
        _ => return Err(EIO),
    }
    if let Some(vmo) = tty::vmo_for_child() {
        handles.push((role::TERMINAL, vmo.into_handle()));
    }
    if handles.len() > vabi::CHANNEL_MAX_HANDLES {
        return Err(EMFILE);
    }

    // The program's image.
    let (vmo, size): (Vmo, u64) = error::ipc(vfs::with(|c| c.read_file(path.clone()))?)?;
    let image = vrt::vm::Mapping::new(vmo, (size as usize).max(1), vabi::map_flags::READ).map_err(error::kernel)?;
    // SAFETY: a read-only mapping of the VMO the VFS made for us alone.
    let bytes = unsafe { &image.as_slice()[..size as usize] };

    // SAFETY: the program passed NULL-terminated arrays.
    let (args, env) = unsafe { (user::cstr_array(argv), user::cstr_array(envp)) };
    let name = path::file_name(&path);
    let name = &name[..name.floor_char_boundary(vabi::NAME_MAX)];
    let mut spawn = Spawn::new(name).path(&path).cwd(&child.cwd);
    spawn.args = args;
    spawn.env = env;
    spawn.handles = handles;
    let process = spawn.start(bytes).map_err(spawn_errno)?;
    let pid = tid_of(process.0.koid());
    CHILDREN.lock().insert(pid, process);
    if res != 0 {
        // SAFETY: the program passed where to store the id.
        unsafe { user::write(res, pid)? };
    }
    Ok(())
}

/// musl's `posix_spawn` without the `PATH` search (see the C library's
/// `src/process/posix_spawn.c`). Returns 0 or an `errno` value.
///
/// # Safety
/// The arguments are those of `posix_spawn`, with `ops` the list of file
/// actions and `attr` never null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __veda_spawn(
    res: usize,
    path: usize,
    ops: usize,
    _attr: usize,
    argv: usize,
    envp: usize,
) -> i32 {
    // SAFETY: per the caller.
    match unsafe { spawn(res, path, ops, argv, envp) } {
        Ok(()) => 0,
        Err(e) => e as i32,
    }
}

// --- waiting ---------------------------------------------------------------

/// The children `pid` selects (`wait4`'s convention: -1 or a process
/// group means any).
fn select_children(pid: i32) -> Result<Vec<(i32, Process)>, isize> {
    let children = CHILDREN.lock();
    let picked: Vec<(i32, Process)> = children
        .iter()
        .filter(|&(&p, _)| pid <= 0 || p == pid)
        .filter_map(|(&p, proc)| proc.0.duplicate(None).ok().map(|h| (p, Process(h))))
        .collect();
    if picked.is_empty() {
        return Err(ECHILD);
    }
    Ok(picked)
}

/// Waits for one of the children `pid` selects to end; returns its id and
/// exit code, or `None` if `nohang` and none has.
fn wait_child(pid: i32, nohang: bool, reap: bool) -> Result<Option<(i32, i64)>, isize> {
    let children = select_children(pid)?;
    let mut items: Vec<WaitItem> = children
        .iter()
        .map(|(_, p)| WaitItem { handle: p.raw(), signals: signals::TERMINATED, observed: 0, _reserved: 0 })
        .collect();
    let deadline = if nohang { 0 } else { vabi::DEADLINE_INFINITE };
    match vrt::object::wait_many(&mut items, deadline) {
        Ok(_) => {}
        Err(vabi::Error::TimedOut) => return Ok(None),
        Err(e) => return Err(error::kernel(e)),
    }
    let Some(i) = items.iter().position(|it| it.observed & signals::TERMINATED != 0) else { return Ok(None) };
    let (p, proc) = &children[i];
    let code = proc.info().map_err(error::kernel)?.exit_code;
    if reap {
        CHILDREN.lock().remove(p);
    }
    Ok(Some((*p, code)))
}

pub unsafe fn wait4(pid: i32, status: usize, options: u32, rusage: usize) -> SysResult {
    if options & !(wait::NOHANG | wait::UNTRACED | 8) != 0 {
        return Err(EINVAL);
    }
    let Some((p, code)) = wait_child(pid, options & wait::NOHANG != 0, true)? else { return Ok(0) };
    if status != 0 {
        // SAFETY: the program passed an int.
        unsafe { user::write(status, wait_status(code))? };
    }
    if rusage != 0 {
        // SAFETY: the program passed a struct rusage.
        unsafe { user::write(rusage, Rusage::default())? };
    }
    Ok(p as usize)
}

pub unsafe fn waitid(kind: u32, id: i32, info: usize, options: u32) -> SysResult {
    let pid = match kind {
        wait::P_ALL => -1,
        wait::P_PID => id,
        wait::P_PGID => -1,
        _ => return Err(EINVAL),
    };
    if options & wait::EXITED == 0 {
        return Err(EINVAL);
    }
    let reap = options & wait::NOWAIT == 0;
    let found = wait_child(pid, options & wait::NOHANG != 0, reap)?;
    let si = match found {
        None => Siginfo::default(),
        Some((p, code)) => {
            let (si_code, status) = match vabi::exit_signal(code) {
                Some(s) => (wait::CLD_KILLED, s as i32),
                None => (wait::CLD_EXITED, (code & 0xff) as i32),
            };
            Siginfo { signo: sig::CHLD as i32, code: si_code, pid: p, uid: vfs::UID, status, ..Default::default() }
        }
    };
    if info != 0 {
        // SAFETY: the program passed a siginfo_t.
        unsafe { user::write(info, si)? };
    }
    Ok(0)
}

/// `kill`: this process, or a child (which ends at once, whatever the
/// signal, except signal 0 which only checks that it exists).
pub fn kill(pid: i32, s: u32) -> SysResult {
    if s >= sig::NSIG {
        return Err(EINVAL);
    }
    if pid == self::pid() || pid == 0 || pid == -1 {
        return crate::signal::send_self(s);
    }
    let children = CHILDREN.lock();
    let proc = children.get(&pid.abs()).ok_or(ESRCH)?;
    if s != 0 {
        proc.kill().map_err(error::kernel)?;
    }
    Ok(0)
}

/// `getrusage`: CPU time of this process (`RUSAGE_SELF`) or none.
pub unsafe fn getrusage(who: i32, buf: usize) -> SysResult {
    let mut r = Rusage::default();
    if who == 0 {
        let ns = vrt::object::process_self_info().map_or(0, |i| i.cpu_time_ns);
        r.utime = Timeval { sec: (ns / 1_000_000_000) as i64, usec: ((ns % 1_000_000_000) / 1000) as i64 };
    }
    // SAFETY: the program passed a struct rusage.
    unsafe { user::write(buf, r)? };
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_actions_are_laid_out_as_musl_has_them() {
        // musl's struct fdop: two pointers, five ints, then `char path[]`.
        assert_eq!(FDOP_PATH, 36);
        assert_eq!(size_of::<FdOp>(), 40);
    }

    #[test]
    fn wait_statuses() {
        // WIFEXITED, WEXITSTATUS
        assert_eq!(wait_status(0), 0);
        assert_eq!(wait_status(3), 3 << 8);
        assert_eq!(wait_status(256 + 7), 7 << 8);
        // WIFSIGNALED, WTERMSIG
        assert_eq!(wait_status(vabi::EXIT_CODE_SIGNALED - 6), 6);
        assert_eq!(wait_status(vabi::EXIT_CODE_CRASHED), 11);
        assert_eq!(wait_status(vabi::EXIT_CODE_KILLED), 9);
    }
}
