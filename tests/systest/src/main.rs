//! `systest` — integration tests that run inside Veda.
//!
//! Each test prints `<name> ... ok|FAILED: reason` and the final line is `PASS`
//! or `FAIL (n failed)`; the kernel log prefixes every line with the program
//! name, so the serial log reads `systest: PASS`. `cargo xtask test` boots the
//! system with `systest` on the kernel command line and checks the log.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vipc::Bytes;
use vproto::fs::{FsError, open_flags, vfs};
use vproto::init::TaskEvent;
use vproto::launcher;
use vrt::object::{Channel, Event, Vmo};
use vrt::println;
use vrt::sync::{Condvar, Mutex};

mod posix;

vrt::entry!(main);

type TestResult = Result<(), String>;

fn check(cond: bool, what: &str) -> TestResult {
    if cond { Ok(()) } else { Err(what.to_string()) }
}

fn vfs_client() -> Result<vfs::Client, String> {
    vproto::connect(vfs::NAME).map(vfs::Client::new).map_err(|e| alloc::format!("connect vfs: {e:?}"))
}

fn test_vfs_system_image() -> TestResult {
    let fs = vfs_client()?;
    let entries = fs.read_dir("/system/bin".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(entries.iter().any(|e| e.name == "init.exe"), "init.exe listed in /system/bin")?;
    let st = fs.stat("/system/etc/version".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(!st.is_dir && st.size > 0 && st.read_only, "version file stat")?;
    let (vmo, len) =
        fs.read_file("/system/etc/version".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let mut buf = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut buf).map_err(|e| e.to_string())?;
    check(buf.starts_with(b"Veda"), "version file contents")?;
    let denied = fs.open("/system/etc/version".into(), open_flags::WRITE).map_err(|e| e.to_string())?;
    check(denied == Err(FsError::ReadOnly), "system image is read-only")
}

fn test_vfs_read_write() -> TestResult {
    let fs = vfs_client()?;
    let path: String = "/home/user/Documents/systest.txt".into();
    let fd = fs
        .open(path.clone(), open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let n = fs.write(fd, 0, Bytes(data.clone())).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(n as usize == data.len(), "write length")?;
    fs.close(fd).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let fd = fs.open(path.clone(), open_flags::READ).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let back = fs.read(fd, 0, 20_000).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(back.0 == data, "read back what was written")?;
    fs.rename(path.clone(), "/home/user/Documents/renamed.txt".into())
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    check(fs.stat(path).map_err(|e| e.to_string())? == Err(FsError::NotFound), "old name gone after rename")?;
    fs.remove("/home/user/Documents/renamed.txt".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())
}

/// The home directory reports its space, and a file that would not fit on
/// the home disk is refused instead of being lost at the next save.
fn test_vfs_space() -> TestResult {
    let fs = vfs_client()?;
    let home = fs.space("/home/user".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(home.total > 0 && home.used <= home.total, "home space reported")?;
    let system = fs.space("/system/bin".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(system.used > 0 && system.used == system.total, "system image space reported")?;
    if !home.persistent {
        return Ok(()); // No home disk: memory is the only limit.
    }
    let path: String = "/home/user/Documents/too-big.bin".into();
    let len = home.total - home.used + 1;
    let vmo = Vmo::create(len as usize).map_err(|e| e.to_string())?;
    let r = fs.write_file(path.clone(), vmo, len).map_err(|e| e.to_string())?;
    check(r == Err(FsError::NoSpace), "a file larger than the free space is refused")?;
    check(fs.stat(path).map_err(|e| e.to_string())? == Err(FsError::NotFound), "nothing was created")
}

fn test_launcher() -> TestResult {
    let ch = vproto::connect(launcher::NAME).map_err(|e| alloc::format!("{e:?}"))?;
    let l = launcher::Client::new(ch);
    let tasks = l.tasks().map_err(|e| e.to_string())?;
    check(tasks.iter().any(|t| t.name == "init"), "init is running")?;
    check(tasks.iter().any(|t| t.name == "vfs"), "vfs is running")
}

/// The supervisor in `init` restarts crashed system services: once the
/// desktop is up, kill the compositor, then wait for a new one to answer
/// display requests and for the desktop shell (which loses its windows with
/// it) to come back.
fn test_service_restart() -> TestResult {
    use vproto::display::display;
    let l = launcher::Client::new(vproto::connect(launcher::NAME).map_err(|e| alloc::format!("{e:?}"))?);
    let koid_of = |l: &launcher::Client, name: &str| -> Result<Option<u64>, String> {
        Ok(l.tasks().map_err(|e| e.to_string())?.iter().find(|t| t.name == name).map(|t| t.koid))
    };
    // A shell registers its service after creating its windows.
    let shell_up = || {
        let names = vproto::with_registry(|r| r.list()).ok().and_then(|r| r.ok()).unwrap_or_default();
        names.iter().any(|n| n == "shell")
    };
    let wait = || vrt::time::sleep(vrt::time::Duration::from_millis(200));
    // The compositor goes with the desktop on the screen, as in use. (With
    // hardware virtualisation, systest gets here before the shell has made
    // its windows.)
    let start = vrt::time::Instant::now();
    while !shell_up() {
        if start.elapsed().as_millis() > 40_000 {
            return Err("the desktop shell did not start".into());
        }
        wait();
    }
    let shell_before = koid_of(&l, "shell")?;
    let old = koid_of(&l, "compositor")?.ok_or("the compositor is not running")?;
    l.kill(old).map_err(|e| e.to_string())?.map_err(|e| alloc::format!("kill: {e:?}"))?;
    let start = vrt::time::Instant::now();
    loop {
        if start.elapsed().as_millis() > 20_000 {
            return Err("the compositor was not restarted".into());
        }
        wait();
        if koid_of(&l, "compositor")?.is_some_and(|k| k != old) {
            break;
        }
    }
    // The registry queues this connection until the new compositor
    // registers, so the call waits for it to come up.
    let d = display::Client::new(vproto::connect(display::NAME).map_err(|e| alloc::format!("{e:?}"))?);
    let info = d.screen_info().map_err(|e| e.to_string())?;
    check(info.width > 0 && info.height > 0, "restarted compositor reports the screen")?;
    loop {
        if start.elapsed().as_millis() > 40_000 {
            return Err("the desktop shell did not come back".into());
        }
        let shell = koid_of(&l, "shell")?;
        if shell.is_some() && shell != shell_before && shell_up() {
            return Ok(());
        }
        wait();
    }
}

/// Applications that crash are reported to launcher watchers (the shell
/// shows a notification): start a copy of this program that faults.
fn test_crash_report() -> TestResult {
    let l = launcher::Client::new(vproto::connect(launcher::NAME).map_err(|e| alloc::format!("{e:?}"))?);
    let events = l.watch().map_err(|e| e.to_string())?.map_err(|e| alloc::format!("watch: {e:?}"))?;
    println!("crash report: starting a copy of systest that faults on purpose (invalid opcode)");
    let koid = l
        .launch("/system/bin/systest.exe".into(), alloc::vec!["crash".into()])
        .map_err(|e| e.to_string())?
        .map_err(|e| alloc::format!("launch: {e:?}"))?;
    let deadline = vrt::time::deadline_after(vrt::time::Duration::from_secs(10));
    let msg = events.read_blocking(deadline).map_err(|e| alloc::format!("no crash report: {e}"))?;
    let (_, event) = vipc::decode_event::<TaskEvent>(msg).map_err(|e| e.to_string())?;
    check(event == TaskEvent::Crashed { koid, name: "systest".into() }, "the report names the crashed process")
}

/// `systest crash`: the faulting child of [`test_crash_report`].
fn crash() -> ! {
    // SAFETY: `ud2` raises an invalid-opcode exception on purpose; the
    // kernel ends the process.
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

fn test_threads_and_locks() -> TestResult {
    let counter = Arc::new(Mutex::new(0u64));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let c = counter.clone();
        handles.push(vrt::thread::spawn(move || {
            for _ in 0..2000 {
                *c.lock() += 1;
            }
        }));
    }
    for h in handles {
        h.join().map_err(|e| e.to_string())?;
    }
    check(*counter.lock() == 8000, "mutex-protected counter")?;

    // Condition variable handshake between two threads.
    let state = Arc::new((Mutex::new(false), Condvar::new()));
    let s2 = state.clone();
    let waker = vrt::thread::spawn(move || {
        vrt::time::sleep(vrt::time::Duration::from_millis(20));
        *s2.0.lock() = true;
        s2.1.notify_all();
    });
    let mut ready = state.0.lock();
    while !*ready {
        ready = state.1.wait(ready);
    }
    drop(ready);
    waker.join().map_err(|e| e.to_string())
}

fn test_ipc_primitives() -> TestResult {
    // Channel with a VMO handle travelling through it.
    let (a, b) = Channel::create().map_err(|e| e.to_string())?;
    let vmo = Vmo::create(8192).map_err(|e| e.to_string())?;
    vmo.write(4096, b"shared").map_err(|e| e.to_string())?;
    a.write(b"vmo", alloc::vec![vmo.into_handle()]).map_err(|e| e.to_string())?;
    let mut msg = b.read().map_err(|e| e.to_string())?;
    check(msg.handles.len() == 1, "handle received")?;
    let got = Vmo::from_handle(msg.handles.pop().unwrap());
    let mut buf = [0u8; 6];
    got.read(4096, &mut buf).map_err(|e| e.to_string())?;
    check(&buf == b"shared", "VMO contents shared")?;
    drop(a);
    check(b.read().err() == Some(vabi::Error::PeerClosed), "peer closed detected")?;

    // Events wake waiters on another thread.
    let ev = Arc::new(Event::create().map_err(|e| e.to_string())?);
    let hits = Arc::new(AtomicU32::new(0));
    let (ev2, hits2) = (ev.clone(), hits.clone());
    let t = vrt::thread::spawn(move || {
        if ev2.wait(vabi::signals::SIGNALED, vrt::time::deadline_after(vrt::time::Duration::from_secs(5))).is_ok() {
            hits2.fetch_add(1, Ordering::SeqCst);
        }
    });
    vrt::time::sleep(vrt::time::Duration::from_millis(10));
    ev.signal().map_err(|e| e.to_string())?;
    t.join().map_err(|e| e.to_string())?;
    check(hits.load(Ordering::SeqCst) == 1, "event woke the waiter")?;

    // Timeouts expire.
    let start = vrt::time::Instant::now();
    let r = ev.0.wait(vabi::signals::USER_0, vrt::time::deadline_after(vrt::time::Duration::from_millis(30)));
    check(r == Err(vabi::Error::TimedOut) && start.elapsed().as_millis() >= 25, "wait timeout")
}

/// A named test.
type Test = (&'static str, fn() -> TestResult);

fn main() -> i32 {
    match vrt::env::args().get(1).map(|a| a.as_str()) {
        Some("crash") => crash(),
        Some("job") => return posix::job(),
        Some("wait") => {
            posix::wait();
            return 0;
        }
        _ => {}
    }
    println!("starting");
    let tests: [Test; 15] = [
        ("ipc primitives", test_ipc_primitives),
        ("sockets", posix::test_sockets),
        ("memory protection", posix::test_memory_protection),
        ("private memory", posix::test_private_memory),
        ("thread exit futex", posix::test_exit_futex),
        ("threads and locks", test_threads_and_locks),
        ("vfs system image", test_vfs_system_image),
        ("vfs read/write", test_vfs_read_write),
        ("vfs space", test_vfs_space),
        ("vfs open files", posix::test_vfs_open_files),
        ("c programs", posix::test_c_programs),
        ("killing a job", posix::test_kill_job),
        ("launcher", test_launcher),
        ("crash report", test_crash_report),
        ("service restart", test_service_restart),
    ];
    let mut failed = 0;
    for (name, f) in tests {
        match f() {
            Ok(()) => println!("{} ... ok", name),
            Err(e) => {
                failed += 1;
                println!("{} ... FAILED: {}", name, e);
            }
        }
    }
    if failed == 0 {
        println!("PASS");
        0
    } else {
        println!("FAIL ({} failed)", failed);
        1
    }
}
