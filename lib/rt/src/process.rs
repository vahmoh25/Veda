//! Creating processes from PE images.
//!
//! Process creation is done entirely from user space with kernel primitives:
//! create an empty process, map the image's sections into it from a VMO,
//! give it a stack, create its first thread and start it with a bootstrap
//! channel carrying the startup message.

use alloc::vec::Vec;

use vabi::startup;
use vabi::{Error, map_flags, nr};

use crate::object::{Channel, Handle, Process, Thread, Vmo};
use crate::sys::call;
use crate::vm;

/// Stack size of a new process's main thread.
pub const MAIN_STACK: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    BadImage(vpe::PeError),
    Kernel(Error),
}

impl From<Error> for SpawnError {
    fn from(e: Error) -> Self {
        SpawnError::Kernel(e)
    }
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SpawnError::BadImage(e) => write!(f, "invalid executable: {e}"),
            SpawnError::Kernel(e) => write!(f, "{e}"),
        }
    }
}

/// What to give a new process.
pub struct Spawn<'a> {
    pub name: &'a str,
    pub args: Vec<&'a str>,
    pub env: Vec<&'a str>,
    /// Handles to pass, tagged with roles (`vabi::startup::role`).
    pub handles: Vec<(u32, Handle)>,
}

impl<'a> Spawn<'a> {
    pub fn new(name: &'a str) -> Spawn<'a> {
        Spawn { name, args: alloc::vec![name], env: Vec::new(), handles: Vec::new() }
    }

    pub fn arg(mut self, a: &'a str) -> Self {
        self.args.push(a);
        self
    }

    pub fn env(mut self, kv: &'a str) -> Self {
        self.env.push(kv);
        self
    }

    pub fn handle(mut self, role: u32, h: Handle) -> Self {
        self.handles.push((role, h));
        self
    }

    /// Loads `image` (a Vindows PE executable) into a new process and starts it.
    pub fn start(self, image: &[u8]) -> Result<Process, SpawnError> {
        let pe = vpe::PeImage::parse(image).map_err(SpawnError::BadImage)?;
        pe.check_no_imports().map_err(SpawnError::BadImage)?;

        let h = call(nr::PROCESS_CREATE, [self.name.as_ptr() as usize, self.name.len(), 0, 0, 0, 0])?;
        // SAFETY: fresh handle from the kernel.
        let process = Process(unsafe { Handle::from_raw(h as u32) });

        // Image: one VMO, mapped section by section with its permissions.
        let base = pe.image_base() as usize;
        let size = pe.size_of_image() as usize;
        let vmo = Vmo::create(size)?;
        vmo.write(0, pe.header_bytes())?;
        vm::map(Some(&process), &vmo, 0, (pe.size_of_headers() as usize).next_multiple_of(4096), base, map_flags::READ | map_flags::FIXED)?;
        for s in pe.sections().filter(|s| !s.discardable() && s.virtual_size > 0) {
            vmo.write(s.virtual_address as usize, s.data)?;
            let mut flags = map_flags::READ | map_flags::FIXED;
            if s.writable() {
                flags |= map_flags::WRITE;
            }
            if s.executable() {
                flags |= map_flags::EXECUTE;
            }
            let len = (s.virtual_size as usize).next_multiple_of(4096);
            vm::map(Some(&process), &vmo, s.virtual_address as usize, len, base + s.virtual_address as usize, flags)?;
        }

        let stack = Vmo::create(MAIN_STACK)?;
        let stack_base = vm::map(Some(&process), &stack, 0, MAIN_STACK, 0, map_flags::READ | map_flags::WRITE)?;

        let t = call(nr::THREAD_CREATE, [process.raw() as usize, "main".as_ptr() as usize, 4, 0, 0, 0])?;
        // SAFETY: fresh handle from the kernel.
        let thread = Thread(unsafe { Handle::from_raw(t as u32) });

        let (ours, theirs) = Channel::create()?;
        let roles: Vec<u32> = self.handles.iter().map(|(r, _)| *r).collect();
        let mut bytes = Vec::new();
        startup::encode(&self.args, &self.env, &roles, &mut |b| bytes.extend_from_slice(b));
        ours.write(&bytes, self.handles.into_iter().map(|(_, h)| h).collect())?;
        drop(ours);

        let entry = pe.entry_point() as usize;
        let rsp = stack_base + MAIN_STACK - 72;
        call(nr::PROCESS_START, [process.raw() as usize, thread.raw() as usize, entry, rsp, theirs.into_handle().into_raw() as usize, 0])?;
        Ok(process)
    }
}
