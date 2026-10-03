//! Starts the first user process, `init`, from the initrd.
//!
//! This is the only program the kernel loads itself. `init` receives the
//! root resource and handles to the boot modules; every other process is
//! created from user space.

use alloc::sync::Arc;
use alloc::vec::Vec;

use bootinfo::BootInfo;
use vabi::startup::{self, role};
use vabi::{KernelBootInfo, Rights};

use crate::mm::aspace::Perms;
use crate::mm::paging::Cache;
use crate::mm::vmo::Vmo;
use crate::mm::{page_align_up, phys_to_virt};
use crate::object::channel::{self, Message};
use crate::object::handle::Handle;
use crate::object::process::Process;
use crate::object::resource::Resource;
use crate::object::KObject;
use crate::sched::{self, Thread};

const INIT_PATH: &str = "bin/init.exe";
const STACK_SIZE: u64 = 1 << 20;

fn perms_of(s: &vpe::Section) -> Perms {
    Perms { read: true, write: s.writable(), exec: s.executable() }
}

pub fn spawn_init(boot: &BootInfo) {
    // SAFETY: the initrd lies in RAM covered by the direct map and is never
    // freed.
    let initrd = unsafe {
        core::slice::from_raw_parts(phys_to_virt(boot.initrd.base) as *const u8, boot.initrd.size as usize)
    };
    let archive = initrd::Archive::open(initrd).expect("the initrd is corrupt");
    crate::kinfo!("initrd: {} files, {} KiB", archive.len(), initrd.len() / 1024);
    let file = archive.find(INIT_PATH).expect("bin/init.exe is missing from the initrd");
    let pe = vpe::PeImage::parse(file.data).expect("init.exe is not a valid PE32+ image");
    pe.check_no_imports().expect("init.exe has DLL imports");

    let process = Process::new("init", 0).expect("out of memory creating init");
    let aspace = process.aspace().unwrap();

    // Copy the image into a VMO and map each section with its permissions.
    let base = pe.image_base();
    let image = Vmo::new_anonymous(pe.size_of_image() as u64).expect("init image too large");
    assert!(image.write(0, pe.header_bytes()));
    let ro = Perms { read: true, write: false, exec: false };
    aspace
        .map(image.clone(), 0, page_align_up(pe.size_of_headers() as u64), base, true, ro, false)
        .expect("mapping init headers");
    for s in pe.sections().filter(|s| !s.discardable() && s.virtual_size > 0) {
        assert!(image.write(s.virtual_address as u64, s.data));
        aspace
            .map(
                image.clone(),
                s.virtual_address as u64,
                page_align_up(s.virtual_size as u64),
                base + s.virtual_address as u64,
                true,
                perms_of(&s),
                false,
            )
            .expect("mapping an init section");
    }

    let stack = Vmo::new_anonymous(STACK_SIZE).unwrap();
    let rw = Perms { read: true, write: true, exec: false };
    let stack_base = aspace.map(stack, 0, STACK_SIZE, 0, false, rw, false).expect("mapping init's stack");

    // Boot modules handed to init.
    let fb = &boot.framebuffer;
    let mut cmdline = [0u8; 256];
    cmdline[..boot.cmdline_len as usize].copy_from_slice(&boot.cmdline[..boot.cmdline_len as usize]);
    let info = KernelBootInfo {
        framebuffer_width: fb.width,
        framebuffer_height: fb.height,
        framebuffer_pitch: fb.stride * 4,
        framebuffer_format: fb.format as u32,
        framebuffer_size: fb.size,
        initrd_size: boot.initrd.size,
        cmdline,
        cmdline_len: boot.cmdline_len,
        cpu_count: crate::arch::percpu::online().count() as u32,
    };
    let info_vmo = Vmo::new_anonymous(4096).unwrap();
    // SAFETY: viewing a plain #[repr(C)] struct as bytes.
    let info_bytes = unsafe {
        core::slice::from_raw_parts(&info as *const KernelBootInfo as *const u8, core::mem::size_of::<KernelBootInfo>())
    };
    assert!(info_vmo.write(0, info_bytes));

    let read_only = Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::MAP.0 | Rights::GET_INFO.0);
    let read_write = Rights(read_only.0 | Rights::WRITE.0);
    let mut handles: Vec<Handle> = Vec::new();
    let mut roles: Vec<u32> = Vec::new();
    handles.push(Handle {
        object: KObject::Resource(Resource::root()),
        rights: Rights(Rights::BASIC.0 | Rights::DUPLICATE.0),
    });
    roles.push(role::ROOT_RESOURCE);
    handles.push(Handle {
        object: KObject::Vmo(Vmo::new_physical(boot.initrd.base, boot.initrd.size, Cache::WriteBack)),
        rights: read_only,
    });
    roles.push(role::INITRD);
    if fb.phys_base != 0 {
        handles.push(Handle {
            object: KObject::Vmo(Vmo::new_physical(fb.phys_base, fb.size, Cache::WriteCombining)),
            rights: read_write,
        });
        roles.push(role::FRAMEBUFFER);
    }
    handles.push(Handle { object: KObject::Vmo(info_vmo), rights: read_only });
    roles.push(role::BOOT_INFO);

    let mut data = Vec::new();
    startup::encode(&["init"], &[], &roles, &mut |b| data.extend_from_slice(b));
    let (kernel_end, init_end) = channel::create();
    if kernel_end.write(Message { data, handles }).is_err() {
        panic!("could not send init its startup message");
    }
    drop(kernel_end);
    let channel_rights = Rights(Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::WAIT.0);
    let bootstrap = process
        .handles
        .lock()
        .insert(Handle { object: KObject::Channel(init_end), rights: channel_rights })
        .expect("inserting init's bootstrap handle");

    let thread = Thread::new_user(&process, "main").expect("creating init's thread");
    assert!(process.add_thread(thread.clone()));
    // Leave the Win64 shadow space above the (fake) return address.
    let rsp = stack_base + STACK_SIZE - 72;
    thread.prepare_user_entry(pe.entry_point(), rsp, bootstrap as u64, 0);
    process.mark_started();
    sched::make_ready(&thread);
    crate::kinfo!("started init (process {}), entry {:#x}", process.koid, pe.entry_point());
    keep_alive(process);
}

/// The kernel keeps a reference to init so it can report if init dies.
static INIT: crate::sync::SpinLock<Option<Arc<Process>>> = crate::sync::SpinLock::new(None);

fn keep_alive(p: Arc<Process>) {
    *INIT.lock() = Some(p);
}
