//! `init` — the first user-space process (bring-up version).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use vabi::startup::role;
use vrt::{Channel, Vmo, println};

vrt::entry!(main);

fn main() -> i32 {
    println!("init: hello from user space! args={:?}", vrt::env::args());
    println!("init: startup handle roles {:?}", vrt::env::handle_roles());

    let initrd = Vmo::from_handle(vrt::env::take_handle(role::INITRD).expect("no initrd handle"));
    let size = initrd.size().unwrap();
    let addr = initrd.map(0, size, vabi::map_flags::READ).unwrap();
    // SAFETY: the initrd mapping is read-only and lives as long as init.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    let archive = initrd::Archive::open(bytes).expect("bad initrd");
    for f in archive.files() {
        println!("init: initrd file {} ({} bytes)", f.path, f.data.len());
    }

    // Channel round trip with a transferred handle.
    let (a, b) = Channel::create().unwrap();
    let (c, _d) = Channel::create().unwrap();
    a.write(b"ping", alloc::vec![c.into_handle()]).unwrap();
    let msg = b.read().unwrap();
    println!("init: channel says {:?} with {} handle(s)", core::str::from_utf8(&msg.bytes), msg.handles.len());

    // Heap and threads.
    let v: Vec<u64> = (0..100_000).collect();
    let sum: u64 = v.iter().sum();
    let worker = vrt::thread::spawn(move || (0..1000u64).map(|x| x * x).sum::<u64>());
    println!("init: heap sum {} thread result {}", sum, worker.join().unwrap());
    println!("init: bring-up OK");
    loop {
        vrt::time::sleep(vrt::time::Duration::from_secs(3600));
    }
}
