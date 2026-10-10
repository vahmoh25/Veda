//! The PC's firmware variables, which the guest reads.
//!
//! Veda's loader read the firmware's variables that an operating system
//! may read before Veda started (`bootinfo::variables`), and devmgr hands
//! them over. The guest reads them with the platform's hypercall
//! (`vhv::platform::hypercall::FIRMWARE_VARIABLE`), which does what UEFI's
//! `GetVariable` and `GetNextVariableName` do; it cannot change them.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use bootinfo::variables::{Name, Variables};
use vhv::platform::{Variable, error, variable};
use vrt::object::Vmo;
use vrt::println;

use crate::memory::GuestMemory;

/// The most bytes of a name the monitor reads: Linux's longest name, 1024
/// UTF-16 code units, and its NUL.
const MAX_NAME: u64 = 2050;
/// How many of the guest's asks for variables the PC does not have are
/// logged (a program may ask for any).
const MISSES_LOGGED: u32 = 16;

#[derive(Default)]
pub struct FirmwareVariables {
    list: Variables<'static>,
    /// Which variables the guest has read (each is logged the first time:
    /// efivarfs reads a variable again at every read of its file).
    read: Vec<AtomicBool>,
    misses: AtomicU32,
}

impl FirmwareVariables {
    /// The variables devmgr handed over, mapped for the rest of the
    /// process's life (none if they are not records of theirs).
    pub fn map(vmo: Vmo) -> FirmwareVariables {
        let mapped = vmo.size().and_then(|size| Ok((vmo.map(0, size, vabi::map_flags::READ)?, size)));
        let Ok((addr, size)) = mapped else {
            println!("cannot map the PC's firmware variables");
            return FirmwareVariables::default();
        };
        // SAFETY: a read-only mapping that lives as long as the process,
        // of memory the kernel never changes.
        let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
        match Variables::parse(bytes) {
            Some(list) => {
                println!("the PC's firmware variables: {}", list.len());
                let read = (0..list.len()).map(|_| AtomicBool::new(false)).collect();
                FirmwareVariables { list, read, misses: AtomicU32::new(0) }
            }
            None => {
                println!("the PC's firmware variables are not what the loader writes: the guest gets none");
                FirmwareVariables::default()
            }
        }
    }

    /// Carries out operation `op` ([`variable`]) with the request at `gpa`.
    pub fn call(&self, memory: &GuestMemory, op: u64, gpa: u64) -> u64 {
        let Some(mut request) = memory.read_obj::<Variable>(gpa) else { return error::INVALID };
        let result = match op {
            variable::GET => self.get(memory, &mut request),
            variable::NEXT => self.next(memory, &mut request),
            _ => return error::INVALID,
        };
        if matches!(result, 0 | error::TOO_SMALL) && !memory.write_obj(gpa, &request) {
            return error::INVALID;
        }
        result
    }

    /// UEFI's `GetVariable`.
    fn get(&self, memory: &GuestMemory, request: &mut Variable) -> u64 {
        let Some(name) = name(memory, request) else { return error::INVALID };
        let Some((index, found)) = self.list.iter().enumerate().find(|(_, v)| v.is(&request.guid, &name)) else {
            if self.misses.fetch_add(1, Ordering::Relaxed) < MISSES_LOGGED {
                let wanted = Name { name: &name, guid: &request.guid };
                println!("the guest asked for the firmware variable {wanted}, which the PC does not have");
            }
            return error::NOT_FOUND;
        };
        let size = found.data.len() as u64;
        request.attributes = found.attributes;
        if request.data_size < size {
            request.data_size = size;
            return error::TOO_SMALL;
        }
        if !memory.write(request.data, found.data) {
            return error::INVALID;
        }
        request.data_size = size;
        if !self.read[index].swap(true, Ordering::Relaxed) {
            println!("the guest read the firmware variable {} ({} bytes)", found, size);
        }
        0
    }

    /// UEFI's `GetNextVariableName`.
    fn next(&self, memory: &GuestMemory, request: &mut Variable) -> u64 {
        let Some(name) = name(memory, request) else { return error::INVALID };
        let mut list = self.list.iter();
        if !name.is_empty() && !list.any(|v| v.is(&request.guid, &name)) {
            return error::INVALID;
        }
        let Some(next) = list.next() else { return error::NOT_FOUND };
        let size = next.name.len() as u64 + 2;
        if request.name_size < size {
            request.name_size = size;
            return error::TOO_SMALL;
        }
        let with_nul = [next.name, &[0, 0]].concat();
        if !memory.write(request.name, &with_nul) {
            return error::INVALID;
        }
        request.name_size = size;
        request.guid = next.guid;
        0
    }
}

/// The name a request holds (UTF-16LE, without the NUL); `None` if its
/// buffer is not the guest's memory or holds no NUL.
fn name(memory: &GuestMemory, request: &Variable) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; (request.name_size.min(MAX_NAME) & !1) as usize];
    if !memory.read(request.name, &mut buffer) {
        return None;
    }
    let nul = buffer.as_chunks::<2>().0.iter().position(|u| *u == [0, 0])?;
    buffer.truncate(nul * 2);
    Some(buffer)
}
