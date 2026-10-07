//! Creating processes from PE and ELF images.
//!
//! Process creation is done entirely from user space with kernel primitives:
//! create an empty process, map the image into it from a VMO, give it a
//! stack, create its first thread and start it with a bootstrap channel
//! carrying the startup message (`vabi::startup`).
//!
//! * **PE** images are Veda's own (Rust) programs: linked at a fixed base
//!   without imports. Their arguments and environment travel in the startup
//!   message.
//! * **ELF** images are POSIX programs (C, built by Veda's GCC): static
//!   executables for the System V ABI, position-independent or not. Their
//!   arguments, environment and auxiliary vector are laid out on their
//!   stack as on Linux (`velf::initial_stack`); the startup message carries
//!   the working directory and the handles (registry, file descriptors,
//!   terminal).

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::startup;
use vabi::{Error, map_flags, nr};

use crate::object::{Channel, Handle, Process, Thread, Vmo};
use crate::sys::call;
use crate::vm;

/// Stack size of a new PE process's main thread.
pub const MAIN_STACK: usize = 1 << 20;
/// Stack size of an ELF program's main thread (as Linux's default limit),
/// unless the program asks for more.
pub const ELF_STACK: usize = 8 << 20;
/// The largest main-thread stack an ELF program may ask for.
pub const ELF_STACK_MAX: usize = 512 << 20;
/// Inaccessible pages below an ELF program's stack, so that an overflow
/// faults instead of running into other memory.
pub const STACK_GUARD: usize = 64 << 10;
/// Where position-independent ELF executables are loaded.
pub const PIE_BASE: u64 = 0x0000_5555_0000_0000;
/// The user and group POSIX programs run as.
pub const UID: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// Neither a PE nor an ELF executable.
    NotExecutable,
    BadPe(vpe::PeError),
    BadElf(velf::ElfError),
    /// The arguments and environment do not fit (in the startup message of
    /// a PE program, on the stack of an ELF one).
    TooLarge,
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
            SpawnError::NotExecutable => f.write_str("not an executable"),
            SpawnError::BadPe(e) => write!(f, "invalid executable: {e}"),
            SpawnError::BadElf(e) => write!(f, "invalid executable: {e}"),
            SpawnError::TooLarge => f.write_str("the argument list is too long"),
            SpawnError::Kernel(e) => write!(f, "{e}"),
        }
    }
}

/// The formats [`Spawn::start`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pe,
    Elf,
}

/// The executable format of `image`, judged by its first bytes.
pub fn format_of(image: &[u8]) -> Option<Format> {
    if image.starts_with(b"MZ") {
        Some(Format::Pe)
    } else if velf::is_elf(image) {
        Some(Format::Elf)
    } else {
        None
    }
}

/// What to give a new process.
pub struct Spawn<'a> {
    /// Process name (shown by the task manager).
    pub name: &'a str,
    /// Arguments; the first is the program name.
    pub args: Vec<&'a [u8]>,
    /// `KEY=VALUE` strings.
    pub env: Vec<&'a [u8]>,
    /// Absolute working directory (empty: `/`).
    pub cwd: &'a str,
    /// The path the program was loaded from (`AT_EXECFN`), if known.
    pub path: &'a str,
    /// Handles to pass, tagged with roles (`vabi::startup::role`).
    pub handles: Vec<(u32, Handle)>,
}

impl<'a> Spawn<'a> {
    pub fn new(name: &'a str) -> Spawn<'a> {
        Spawn { name, args: alloc::vec![name.as_bytes()], env: Vec::new(), cwd: "", path: "", handles: Vec::new() }
    }

    pub fn arg(self, a: &'a str) -> Self {
        self.arg_bytes(a.as_bytes())
    }

    /// An argument that need not be UTF-8 (ELF programs take any bytes but
    /// NUL; PE programs get invalid UTF-8 replaced).
    pub fn arg_bytes(mut self, a: &'a [u8]) -> Self {
        self.args.push(a);
        self
    }

    pub fn env(self, kv: &'a str) -> Self {
        self.env_bytes(kv.as_bytes())
    }

    pub fn env_bytes(mut self, kv: &'a [u8]) -> Self {
        self.env.push(kv);
        self
    }

    pub fn cwd(mut self, dir: &'a str) -> Self {
        self.cwd = dir;
        self
    }

    pub fn path(mut self, path: &'a str) -> Self {
        self.path = path;
        self
    }

    pub fn handle(mut self, role: u32, h: Handle) -> Self {
        self.handles.push((role, h));
        self
    }

    /// Loads `image` (a PE or ELF executable) into a new process and
    /// starts it.
    pub fn start(self, image: &[u8]) -> Result<Process, SpawnError> {
        match format_of(image) {
            Some(Format::Pe) => self.start_pe(image),
            Some(Format::Elf) => self.start_elf(image),
            None => Err(SpawnError::NotExecutable),
        }
    }

    fn create_process(&self) -> Result<(Process, Thread), SpawnError> {
        let h = call(nr::PROCESS_CREATE, [self.name.as_ptr() as usize, self.name.len(), 0, 0, 0, 0])?;
        // SAFETY: fresh handle from the kernel.
        let process = Process(unsafe { Handle::from_raw(h as u32) });
        let t = call(nr::THREAD_CREATE, [process.raw() as usize, "main".as_ptr() as usize, 4, 0, 0, 0])?;
        // SAFETY: fresh handle from the kernel.
        let thread = Thread(unsafe { Handle::from_raw(t as u32) });
        Ok((process, thread))
    }

    /// Sends the startup message and starts the main thread at `entry`.
    fn launch(
        self,
        process: Process,
        thread: Thread,
        entry: usize,
        rsp: usize,
        args: &[&str],
        env: &[&str],
    ) -> Result<Process, SpawnError> {
        let roles: Vec<u32> = self.handles.iter().map(|(r, _)| *r).collect();
        let msg = startup::Startup { args, env, cwd: self.cwd, roles: &roles };
        if msg.encoded_len() > vabi::CHANNEL_MAX_BYTES || roles.len() > vabi::CHANNEL_MAX_HANDLES {
            return Err(SpawnError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut |b| bytes.extend_from_slice(b));
        let (ours, theirs) = Channel::create()?;
        ours.write(&bytes, self.handles.into_iter().map(|(_, h)| h).collect())?;
        drop(ours);
        call(
            nr::PROCESS_START,
            [process.raw() as usize, thread.raw() as usize, entry, rsp, theirs.into_handle().into_raw() as usize, 0],
        )?;
        Ok(process)
    }

    fn start_pe(self, image: &[u8]) -> Result<Process, SpawnError> {
        let pe = vpe::PeImage::parse(image).map_err(SpawnError::BadPe)?;
        pe.check_no_imports().map_err(SpawnError::BadPe)?;
        let (process, thread) = self.create_process()?;

        // Image: one VMO, mapped section by section with its permissions.
        let base = pe.image_base() as usize;
        let size = pe.size_of_image() as usize;
        let vmo = Vmo::create(size)?;
        vmo.write(0, pe.header_bytes())?;
        vm::map(
            Some(&process),
            &vmo,
            0,
            (pe.size_of_headers() as usize).next_multiple_of(4096),
            base,
            map_flags::READ | map_flags::FIXED,
        )?;
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
        let rsp = stack_base + MAIN_STACK - 72;

        // The startup message carries text.
        fn text<'b>(v: &[&'b [u8]]) -> Vec<Cow<'b, str>> {
            v.iter().map(|b| String::from_utf8_lossy(b)).collect()
        }
        let (args, env) = (text(&self.args), text(&self.env));
        let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
        let env: Vec<&str> = env.iter().map(|s| s.as_ref()).collect();
        let entry = pe.entry_point() as usize;
        self.launch(process, thread, entry, rsp, &args, &env)
    }

    fn start_elf(self, image: &[u8]) -> Result<Process, SpawnError> {
        let elf = velf::Elf::parse(image).map_err(SpawnError::BadElf)?;
        let (lo, hi) = elf.span();
        // Where the image goes: at its addresses, or, if it is
        // position-independent, at PIE_BASE. `bias` moves an address of the
        // file there (downwards too: the arithmetic wraps).
        let base = if elf.is_position_independent() { PIE_BASE } else { lo };
        let bias = base.wrapping_sub(lo);
        let end = base.checked_add(hi - lo);
        if base < vabi::USER_SPACE_START as u64 || end.is_none_or(|e| e > vabi::USER_SPACE_END as u64) {
            return Err(SpawnError::BadElf(velf::ElfError::BadSegment));
        }
        let (process, thread) = self.create_process()?;

        // Image: one VMO holding every segment at its offset in the span
        // (zero elsewhere, which is the bss), mapped page run by page run.
        let vmo = Vmo::create((hi - lo) as usize)?;
        for s in elf.segments() {
            vmo.write((s.vaddr - lo) as usize, s.data)?;
        }
        for run in velf::page_runs(&elf) {
            let mut flags = map_flags::FIXED;
            for (on, flag) in [(run.perms.read, map_flags::READ), (run.perms.write, map_flags::WRITE)] {
                if on {
                    flags |= flag;
                }
            }
            if run.perms.exec {
                flags |= map_flags::EXECUTE | map_flags::READ;
            }
            let at = run.start.wrapping_add(bias) as usize;
            vm::map(Some(&process), &vmo, (run.start - lo) as usize, run.len as usize, at, flags)?;
        }

        // Stack: guard pages, then the stack proper, its top holding the
        // arguments, environment and auxiliary vector.
        let size = elf.stack_size().map_or(ELF_STACK, |s| (s as usize).clamp(ELF_STACK, ELF_STACK_MAX));
        let size = size.next_multiple_of(vabi::PAGE_SIZE);
        let stack = Vmo::create(STACK_GUARD + size)?;
        let base = vm::map(Some(&process), &stack, 0, STACK_GUARD + size, 0, map_flags::READ | map_flags::WRITE)?;
        vm::protect(Some(&process), base, STACK_GUARD, 0)?;
        let top = (base + STACK_GUARD + size) as u64;
        let mut random = [0u8; 16];
        crate::object::random_bytes(&mut random);
        let aux = velf::AuxInfo {
            phdr: elf.phdr_vaddr().wrapping_add(bias),
            phnum: elf.phnum() as u64,
            entry: elf.entry().wrapping_add(bias),
            hwcap: cpu_features(),
            uid: UID,
            gid: UID,
            random,
            execfn: self.path.as_bytes(),
        };
        // Like Linux, a quarter of the stack at most for the arguments.
        let img = velf::initial_stack(top, &self.args, &self.env, &aux, size / 4).map_err(|e| match e {
            velf::ElfError::TooLarge => SpawnError::TooLarge,
            e => SpawnError::BadElf(e),
        })?;
        stack.write(STACK_GUARD + size - img.bytes.len(), &img.bytes)?;

        let entry = elf.entry().wrapping_add(bias) as usize;
        self.launch(process, thread, entry, img.sp as usize, &[], &[])
    }
}

/// The CPU's feature flags as `AT_HWCAP` reports them on x86-64
/// (`CPUID.1:EDX`).
fn cpu_features() -> u64 {
    core::arch::x86_64::__cpuid(1).edx as u64
}
