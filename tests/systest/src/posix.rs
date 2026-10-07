//! Tests of what C programs stand on: sockets, memory protection, private
//! memory, the thread exit futex, the VFS's open-file connections, ending a
//! job, and the C programs themselves (`/system/tests/c`, built by the cross
//! toolchain).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vabi::signals::{PEER_CLOSED, PEER_WRITE_DISABLED, READABLE};
use vabi::startup::role;
use vabi::{Error, Rights, map_flags, nr};
use vipc::Bytes;
use vproto::fs::{FsError, file, open_flags, seek, vfs};
use vrt::object::{Process, Socket, Vmo};
use vrt::process::Spawn;
use vrt::sys::call;
use vrt::vm;

use crate::{TestResult, check, vfs_client};

fn s<E: core::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Sockets: a byte stream in both directions, duplicated endpoints, the
/// end of the stream, all-or-nothing small writes.
pub fn test_sockets() -> TestResult {
    let (a, b) = Socket::create().map_err(s)?;
    check(a.write(b"ping").map_err(s)? == 4, "write")?;
    let mut buf = [0u8; 16];
    check(b.read(&mut buf).map_err(s)? == 4 && &buf[..4] == b"ping", "read")?;
    check(b.read(&mut buf) == Err(Error::ShouldWait), "empty socket waits")?;
    b.write(b"pong").map_err(s)?;
    check(a.read(&mut buf).map_err(s)? == 4 && &buf[..4] == b"pong", "other direction")?;

    // A duplicate keeps the endpoint open; the peer sees the end only when
    // every handle is closed.
    let a2 = a.duplicate(None).map_err(s)?;
    drop(a);
    a2.write(b"x").map_err(s)?;
    check(b.wait(PEER_CLOSED, 0).is_err(), "not closed while a duplicate lives")?;
    drop(a2);
    check(b.read(&mut buf).map_err(s)? == 1, "data written before closing arrives")?;
    check(b.read(&mut buf) == Err(Error::PeerClosed), "then the end of the stream")?;

    // Shutting down one direction leaves the other working.
    let (c, d) = Socket::create().map_err(s)?;
    c.write(b"last").map_err(s)?;
    c.shutdown().map_err(s)?;
    check(c.write(b"more") == Err(Error::BadState), "no writes after shutdown")?;
    check(d.wait(PEER_WRITE_DISABLED, 0).is_ok(), "peer told")?;
    check(d.read(&mut buf).map_err(s)? == 4, "buffered data still read")?;
    check(d.read(&mut buf) == Err(Error::PeerClosed), "then the end")?;
    d.write(b"back").map_err(s)?;
    check(c.read(&mut buf).map_err(s)? == 4, "the other direction still works")?;

    // Fill a socket: small writes then go in whole or not at all.
    let (c, d) = Socket::create().map_err(s)?;
    let big = alloc::vec![7u8; 64 * 1024];
    let mut written = 0;
    loop {
        match c.write(&big) {
            Ok(n) => written += n,
            Err(Error::ShouldWait) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    check(written == 256 * 1024, "capacity")?;
    let info = d.info().map_err(s)?;
    check(info.readable == 256 * 1024 && c.info().map_err(s)?.writable == 0, "socket info")?;
    let mut sink = alloc::vec![0u8; 3000];
    d.read(&mut sink).map_err(s)?;
    check(c.write(&[1u8; 4096]) == Err(Error::ShouldWait), "a 4096-byte write is not split")?;
    check(c.write(&[1u8; 2000]).map_err(s)? == 2000, "a smaller one fits")?;
    check(d.wait(READABLE, 0).is_ok(), "readable")
}

/// `vm_protect` works on parts of mappings and never grants more than the
/// handle the VMO was mapped with; ranges that do not fit in user space are
/// refused.
pub fn test_memory_protection() -> TestResult {
    let vmo = Vmo::create(4 * 4096).map_err(s)?;
    let addr = vm::map(None, &vmo, 0, 4 * 4096, 0, map_flags::READ | map_flags::WRITE).map_err(s)?;
    // SAFETY: our fresh, writable mapping.
    unsafe { core::ptr::write_volatile((addr + 4096 * 3) as *mut u8, 9) };
    vm::protect(None, addr + 4096, 4096, map_flags::READ).map_err(|e| format!("partial protect: {e}"))?;
    vm::protect(None, addr + 4096, 4096, map_flags::READ | map_flags::WRITE).map_err(s)?;
    // SAFETY: as above (writable again).
    unsafe { core::ptr::write_volatile((addr + 4096) as *mut u8, 1) };
    vm::protect(None, addr + 2 * 4096, 4096, 0).map_err(|e| format!("protect none: {e}"))?;
    vm::unmap(None, addr, 4 * 4096).map_err(s)?;
    check(vm::protect(None, addr, 4096, map_flags::READ) == Err(Error::NotFound), "protecting unmapped memory fails")?;

    // A read-only handle maps read-only and stays so.
    let ro =
        Vmo::from_handle(vmo.0.duplicate(Some(Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::MAP.0))).map_err(s)?);
    let at = vm::map(None, &ro, 0, 4096, 0, map_flags::READ).map_err(s)?;
    let r = vm::protect(None, at, 4096, map_flags::READ | map_flags::WRITE);
    vm::unmap(None, at, 4096).map_err(s)?;
    check(r == Err(Error::AccessDenied), "write access beyond the handle's rights is refused")?;

    // Addresses and lengths a program may get wrong, or forge: refused,
    // never wrapped around.
    let rw = map_flags::READ | map_flags::WRITE;
    let page = vm::allocate(None, 4096, 0, rw | map_flags::COMMIT).map_err(s)?;
    let high = vm::allocate(None, 8192, usize::MAX & !4095, rw).map_err(s)?;
    vm::unmap(None, high, 8192).map_err(s)?;
    check(high + 8192 <= vabi::USER_SPACE_END, "a hint beyond user space is not taken")?;
    let wrap = vm::map(None, &vmo, 0, usize::MAX, page, map_flags::READ | map_flags::FIXED);
    check(wrap.is_err(), "a length that wraps is refused")?;
    for r in [
        vm::unmap(None, page, usize::MAX),
        vm::protect(None, page, usize::MAX, map_flags::READ),
        vm::decommit(None, page, usize::MAX),
    ] {
        check(r == Err(Error::InvalidArgs), "a range that wraps is refused")?;
    }
    // SAFETY: still our mapping, writable.
    unsafe { core::ptr::write_volatile(page as *mut u8, 1) };
    vm::unmap(None, page, 4096).map_err(s)
}

/// Private memory (`vm_allocate`): its pages are freed as soon as they are
/// unmapped or decommitted, and decommitted pages read as zeros.
pub fn test_private_memory() -> TestResult {
    const PAGE: usize = vabi::PAGE_SIZE;
    let rw = map_flags::READ | map_flags::WRITE;
    let resident = || vrt::object::process_self_info().map(|i| i.memory_bytes as usize).map_err(s);
    let addr = vm::allocate(None, 16 * PAGE, 0, rw).map_err(s)?;
    let untouched = resident()?;
    for i in 0..16 {
        // SAFETY: our fresh, writable mapping.
        unsafe { core::ptr::write_volatile((addr + i * PAGE) as *mut u8, i as u8 + 1) };
    }
    let touched = resident()?;
    check(touched >= untouched + 16 * PAGE, "pages are committed when touched")?;
    vm::unmap(None, addr + 4 * PAGE, 4 * PAGE).map_err(s)?;
    check(touched.saturating_sub(resident()?) >= 4 * PAGE, "unmapped pages are freed")?;
    vm::decommit(None, addr + 8 * PAGE, 4 * PAGE).map_err(s)?;
    check(touched.saturating_sub(resident()?) >= 8 * PAGE, "decommitted pages are freed")?;
    // SAFETY: both pages are still mapped, readable.
    let (dropped, kept) = unsafe {
        (
            core::ptr::read_volatile((addr + 8 * PAGE) as *const u8),
            core::ptr::read_volatile((addr + 12 * PAGE) as *const u8),
        )
    };
    check(dropped == 0 && kept == 13, "decommitted pages read as zeros, the others keep their contents")?;
    // A reservation gives pages back: inaccessible memory is decommitted too.
    vm::protect(None, addr + 12 * PAGE, 4 * PAGE, 0).map_err(s)?;
    vm::decommit(None, addr + 12 * PAGE, 4 * PAGE).map_err(|e| format!("decommit inaccessible pages: {e}"))?;
    check(vm::decommit(None, addr, 8 * PAGE) == Err(Error::NotFound), "a range with a hole is refused")?;
    vm::unmap(None, addr, 16 * PAGE).map_err(s)?;

    // Memory a VMO handle reaches keeps its contents.
    let vmo = Vmo::create(PAGE).map_err(s)?;
    let at = vm::map(None, &vmo, 0, PAGE, 0, rw).map_err(s)?;
    let r = vm::decommit(None, at, PAGE);
    vm::unmap(None, at, PAGE).map_err(s)?;
    check(r == Err(Error::NotSupported), "memory a VMO backs is not decommitted")
}

/// Starts a copy of systest that does `what` (see `main`), as its child.
fn start_copy(what: &str) -> Result<Process, String> {
    let fs = vfs_client()?;
    let path = "/system/bin/systest.exe";
    let (vmo, size) = fs.read_file(path.into()).map_err(s)?.map_err(|e| format!("{path}: {e}"))?;
    let image = vm::Mapping::new(vmo, size as usize, map_flags::READ).map_err(s)?;
    let registry = vproto::with_registry(|r| r.clone_registry())
        .map_err(|e| format!("{e:?}"))?
        .map_err(s)?
        .map_err(|e| format!("{e:?}"))?;
    let spawn = Spawn::new("systest").path(path).arg(what).handle(role::REGISTRY, registry.into_handle());
    // SAFETY: a read-only mapping of the VFS's copy of the file.
    spawn.start(unsafe { image.as_slice() }).map_err(|e| format!("{path}: {e}"))
}

/// The live process that `parent` started, if any.
fn child_of(parent: u64) -> Option<u64> {
    let mut list = [vabi::ProcessInfo::default(); 256];
    let n = vrt::object::process_list(&mut list).ok()?.min(list.len());
    list[..n].iter().find(|p| p.parent_koid == parent && p.state == vabi::process_state::RUNNING).map(|p| p.koid)
}

/// Whether process `koid` still runs.
fn running(koid: u64) -> bool {
    let mut list = [vabi::ProcessInfo::default(); 256];
    let n = vrt::object::process_list(&mut list).unwrap_or(0).min(list.len());
    list[..n].iter().any(|p| p.koid == koid && p.state == vabi::process_state::RUNNING)
}

/// Killing a job (`kill_tree`, as the Terminal's Ctrl+C does) ends the
/// processes its leader started too.
pub fn test_kill_job() -> TestResult {
    let leader = start_copy("job")?;
    let koid = leader.0.koid();
    let deadline = vrt::time::deadline_after(vrt::time::Duration::from_secs(20));
    let member = loop {
        if let Some(c) = child_of(koid) {
            break c;
        }
        if vrt::time::now_ns() > deadline {
            let _ = leader.kill_tree();
            return Err("the job's leader started nothing".into());
        }
        vrt::time::sleep(vrt::time::Duration::from_millis(20));
    };
    leader.kill_tree().map_err(s)?;
    leader.join(deadline).map_err(s)?;
    while running(member) {
        if vrt::time::now_ns() > deadline {
            return Err("the process the leader started still runs".into());
        }
        vrt::time::sleep(vrt::time::Duration::from_millis(20));
    }
    Ok(())
}

/// `systest job`: the leader of [`test_kill_job`]'s job, which starts a
/// copy that waits and waits itself.
pub fn job() -> i32 {
    match start_copy("wait") {
        Ok(_member) => {
            wait();
            0
        }
        Err(e) => {
            vrt::println!("job: {e}");
            1
        }
    }
}

/// `systest wait`: waits until it is killed (a minute at most).
pub fn wait() {
    vrt::time::sleep(vrt::time::Duration::from_secs(60));
}

/// A thread's exit futex word is cleared, and its waiter woken, once the
/// thread has ended.
pub fn test_exit_futex() -> TestResult {
    static WORD: AtomicU32 = AtomicU32::new(1);
    let t = vrt::thread::spawn(|| {
        let _ = call(nr::THREAD_SET_EXIT_FUTEX, [WORD.as_ptr() as usize, 0, 0, 0, 0, 0]);
    });
    let deadline = vrt::time::deadline_after(vrt::time::Duration::from_secs(5));
    while WORD.load(Ordering::Acquire) != 0 {
        match call(nr::FUTEX_WAIT, [WORD.as_ptr() as usize, 1, deadline as usize, 0, 0, 0]) {
            Ok(_) | Err(Error::ShouldWait) => {}
            Err(e) => return Err(format!("futex wait: {e}")),
        }
    }
    t.join().map_err(s)
}

fn open_file(fs: &vfs::Client, path: &str, flags: u32) -> Result<file::Client, String> {
    let (ch, _) = fs.open_file(path.into(), flags).map_err(s)?.map_err(|e| format!("open {path}: {e}"))?;
    Ok(file::Client::new(ch))
}

/// The VFS's open files: a shared offset across duplicated connections,
/// files that outlive their names, renames that replace, exclusive
/// creation, devices.
pub fn test_vfs_open_files() -> TestResult {
    let fs = vfs_client()?;
    let path = "/tmp/systest-open.txt";
    let f = open_file(&fs, path, open_flags::READ | open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE)?;
    f.write(Bytes(b"hello world".to_vec())).map_err(s)?.map_err(s)?;
    let g = file::Client::new(f.duplicate().map_err(s)?.map_err(s)?);
    check(g.seek(0, seek::CURRENT).map_err(s)?.map_err(s)? == 11, "duplicates share the offset")?;
    g.seek(6, seek::SET).map_err(s)?.map_err(s)?;
    check(f.read(5).map_err(s)?.map_err(s)?.0 == b"world", "reading at the shared offset")?;
    let st = f.stat().map_err(s)?.map_err(s)?;
    check(st.size == 11 && st.inode != 0 && !st.executable, "stat")?;

    // Removed while open: still there for those who have it.
    fs.remove(path.into()).map_err(s)?.map_err(s)?;
    check(fs.stat(path.into()).map_err(s)? == Err(FsError::NotFound), "name gone")?;
    check(f.read_at(0, 5).map_err(s)?.map_err(s)?.0 == b"hello", "contents still readable")?;
    f.write_at(0, Bytes(b"HELLO".to_vec())).map_err(s)?.map_err(s)?;
    check(g.read_at(0, 5).map_err(s)?.map_err(s)?.0 == b"HELLO", "and writable")?;
    drop((f, g));

    // Exclusive creation and replacing renames.
    let a = "/tmp/systest-a.txt";
    let b = "/tmp/systest-b.txt";
    for (p, text) in [(a, "A"), (b, "B")] {
        let h = open_file(&fs, p, open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE)?;
        h.write(Bytes(text.as_bytes().to_vec())).map_err(s)?.map_err(s)?;
    }
    let excl = fs.open_file(a.into(), open_flags::WRITE | open_flags::CREATE | open_flags::EXCLUSIVE).map_err(s)?;
    check(matches!(excl, Err(FsError::Exists)), "exclusive creation of an existing file fails")?;
    check(fs.rename(a.into(), b.into()).map_err(s)? == Err(FsError::Exists), "rename does not replace")?;
    fs.replace(a.into(), b.into()).map_err(s)?.map_err(s)?;
    let h = open_file(&fs, b, open_flags::READ)?;
    check(h.read(10).map_err(s)?.map_err(s)?.0 == b"A", "replace moved the file over the other")?;
    fs.remove(b.into()).map_err(s)?.map_err(s)?;

    // Devices.
    let null = open_file(&fs, "/dev/null", open_flags::READ | open_flags::WRITE)?;
    check(null.write(Bytes(alloc::vec![1; 100])).map_err(s)?.map_err(s)? == 100, "/dev/null swallows")?;
    check(null.read(10).map_err(s)?.map_err(s)?.0.is_empty(), "/dev/null is empty")?;
    let st = fs.stat("/dev/zero".into()).map_err(s)?.map_err(s)?;
    check(st.char_device && !st.is_dir, "/dev/zero is a device")?;
    let full = open_file(&fs, "/dev/full", open_flags::WRITE)?;
    check(full.write(Bytes(alloc::vec![0; 1])).map_err(s)? == Err(FsError::NoSpace), "/dev/full is full")?;
    let names: Vec<String> = fs.read_dir("/dev".into()).map_err(s)?.map_err(s)?.into_iter().map(|e| e.name).collect();
    check(names.iter().any(|n| n == "urandom"), "/dev lists its devices")
}

/// Runs a C program with its output on a socket: the exit code and what it
/// wrote.
fn run_c(path: &str, args: &[&str], cwd: &str) -> Result<(i64, String), String> {
    let fs = vfs_client()?;
    let (vmo, size) = fs.read_file(path.into()).map_err(s)?.map_err(|e| format!("{path}: {e}"))?;
    let image = vm::Mapping::new(vmo, size as usize, map_flags::READ).map_err(s)?;
    let (ours, theirs) = Socket::create().map_err(s)?;
    let registry = vproto::with_registry(|r| r.clone_registry())
        .map_err(|e| format!("{e:?}"))?
        .map_err(s)?
        .map_err(|e| format!("{e:?}"))?;
    let mut spawn = Spawn::new("c-test")
        .path(path)
        .cwd(cwd)
        .env("HOME=/home/user")
        .env("PATH=/system/bin")
        .handle(role::FD + 1, theirs.duplicate(None).map_err(s)?.into_handle())
        .handle(role::FD + 2, theirs.into_handle())
        .handle(role::REGISTRY, registry.into_handle());
    for a in args {
        spawn = spawn.arg(a);
    }
    // SAFETY: a read-only mapping of the VFS's copy of the file.
    let process: Process = spawn.start(unsafe { image.as_slice() }).map_err(|e| format!("{path}: {e}"))?;
    let deadline = vrt::time::deadline_after(vrt::time::Duration::from_secs(120));
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match ours.read(&mut buf) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(Error::ShouldWait) => {
                if ours.wait(READABLE | PEER_CLOSED, deadline) == Err(Error::TimedOut) {
                    let _ = process.kill();
                    return Err(format!("{path} did not finish: {}", String::from_utf8_lossy(&out)));
                }
            }
            Err(_) => break,
        }
    }
    let code = process.join(deadline).map_err(s)?;
    Ok((code, String::from_utf8_lossy(&out).into_owned()))
}

/// C and C++ programs built by the cross toolchain run, linked at a fixed
/// address and position-independent: the smallest one, then the checks of
/// the C library and the POSIX layer (`tests/c/posix.c`) and of the C++
/// library (`tests/c/cxx.cc`).
pub fn test_c_programs() -> TestResult {
    let fs = vfs_client()?;
    if fs.stat("/system/tests/c/hello".into()).map_err(s)?.is_err() {
        vrt::println!("c programs: none in this image (no cross toolchain was built)");
        return Ok(());
    }
    // Each program linked at a fixed address and position-independent.
    for variant in ["", "-pie"] {
        let hello = format!("hello{variant}");
        let (code, out) = run_c(&format!("/system/tests/c/{hello}"), &["one", "two"], "/tmp")?;
        vrt::println!("{hello}: {}", out.trim_end());
        check(code == 42, &format!("{hello} exited with {code}"))?;
        let greeting = out.contains("Hello from C on Veda! argc=3 argv[0]=c-test") && out.contains("cwd=/tmp");
        check(greeting, &format!("{hello}'s output"))?;

        // The checks of the C library and the POSIX layer, and of the C++
        // library.
        for checks in ["posix", "cxx"] {
            let program = format!("{checks}{variant}");
            let dir = format!("/tmp/{program}-test");
            let _ = fs.mkdir(dir.clone());
            let (code, out) = run_c(&format!("/system/tests/c/{program}"), &[], &dir)?;
            for line in out.lines().filter(|l| !l.starts_with("ok - ")) {
                vrt::println!("{program}: {line}");
            }
            let passed = code == 0 && out.contains("PASS: 0 failed");
            check(passed, &format!("{program} checks failed (exit code {code})"))?;
        }
    }
    Ok(())
}
