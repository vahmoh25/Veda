//! `systest` — integration tests that run inside Vindows.
//!
//! Each test prints `systest: <name> ... ok|FAILED: reason`; the final line is
//! `systest: PASS` or `systest: FAIL (n failed)`. `cargo xtask test` boots the
//! system with `init.systest` on the kernel command line and checks the log.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vipc::Bytes;
use vproto::fs::{FsError, open_flags, vfs};
use vproto::launcher;
use vrt::object::{Channel, Event, Vmo};
use vrt::println;
use vrt::sync::{Condvar, Mutex};

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
    let (vmo, len) = fs.read_file("/system/etc/version".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    let mut buf = alloc::vec![0u8; len as usize];
    vmo.read(0, &mut buf).map_err(|e| e.to_string())?;
    check(buf.starts_with(b"Vindows"), "version file contents")?;
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
    fs.rename(path.clone(), "/home/user/Documents/renamed.txt".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
    check(fs.stat(path).map_err(|e| e.to_string())? == Err(FsError::NotFound), "old name gone after rename")?;
    fs.remove("/home/user/Documents/renamed.txt".into()).map_err(|e| e.to_string())?.map_err(|e| e.to_string())
}

fn test_launcher() -> TestResult {
    let ch = vproto::connect(launcher::NAME).map_err(|e| alloc::format!("{e:?}"))?;
    let l = launcher::Client::new(ch);
    let tasks = l.tasks().map_err(|e| e.to_string())?;
    check(tasks.iter().any(|t| t.name == "init"), "init is running")?;
    check(tasks.iter().any(|t| t.name == "vfs"), "vfs is running")
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

fn main() -> i32 {
    println!("systest: starting");
    let tests: [(&str, fn() -> TestResult); 5] = [
        ("ipc primitives", test_ipc_primitives),
        ("threads and locks", test_threads_and_locks),
        ("vfs system image", test_vfs_system_image),
        ("vfs read/write", test_vfs_read_write),
        ("launcher", test_launcher),
    ];
    let mut failed = 0;
    for (name, f) in tests {
        match f() {
            Ok(()) => println!("systest: {} ... ok", name),
            Err(e) => {
                failed += 1;
                println!("systest: {} ... FAILED: {}", name, e);
            }
        }
    }
    if failed == 0 {
        println!("systest: PASS");
        0
    } else {
        println!("systest: FAIL ({} failed)", failed);
        1
    }
}
