//! The GPU's render node, `/dev/dri/renderD128`: Linux's i915 interface,
//! as Mesa's iris driver (in the renderer) uses it, carried out with the
//! GPU's driver over the GEM protocol (`vproto::gem`).
//!
//! What i915 keeps for an open file, the layer keeps: the handles of
//! buffers, syncobjs, contexts and address spaces, and each buffer's last
//! uses. The driver keeps what the GPU needs: the buffers' memory, the
//! address spaces and contexts, the submissions. So:
//!
//! * `GETPARAM` and `QUERY` are answered from the session's description
//!   of the GPU;
//! * a buffer is the driver's memory object, mapped where the program maps
//!   the offset `MMAP_OFFSET` gives for it;
//! * contexts and address spaces are made with the driver when first
//!   submitted to, their parameters set by then (i915 finishes its
//!   "proto-contexts" so too);
//! * a syncobj holds a point on an engine's timeline; waiting reads the
//!   session's fence page, and waits on its event while the point is
//!   ahead.
//!
//! Buffers are soft-pinned (iris places them all itself): relocations are
//! refused, as are sync files, buffers shared with other processes (PRIME,
//! flink), user memory (`USERPTR`) and timeline syncobjs, which i915 has
//! and iris on Veda does without.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use vabi::{WaitItem, map_flags, signals};
use vipc::IpcError;
use vproto::gem::{Engine, Exec, ExecBuffer, GemDevice, GemError, Point, class, gem};
use vrt::object::{Event, Vmo};
use vrt::sync::Mutex;
use vrt::vm::{self, Mapping};

use crate::error::SysResult;
use crate::linux::errno::*;
use crate::{time, user};

/// The render node's name.
pub const PATH: &str = "/dev/dri/renderD128";
/// Its device number: major 226 (DRM), minor 128 (the first render node).
pub const RDEV: u64 = (226 << 8) | 128;

/// The parts of an ioctl request (Linux's `_IOC` encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ioc {
    nr: u8,
    /// The bytes of its argument.
    size: usize,
    /// Whether the argument goes in, and whether it comes back.
    write: bool,
    read: bool,
}

impl Ioc {
    /// DRM's requests, type `'d'`.
    fn decode(request: u32) -> Option<Ioc> {
        if (request >> 8) & 0xFF != u32::from(b'd') {
            return None;
        }
        let dir = request >> 30;
        Some(Ioc {
            nr: request as u8,
            size: ((request >> 16) & 0x3FFF) as usize,
            write: dir & 1 != 0,
            read: dir & 2 != 0,
        })
    }
}

/// Request numbers: DRM's own, then i915's (from `DRM_COMMAND_BASE`).
mod nr {
    pub const VERSION: u8 = 0x00;
    pub const GET_MAGIC: u8 = 0x02;
    pub const GEM_CLOSE: u8 = 0x09;
    pub const GEM_FLINK: u8 = 0x0A;
    pub const GEM_OPEN: u8 = 0x0B;
    pub const GET_CAP: u8 = 0x0C;
    pub const SET_CLIENT_CAP: u8 = 0x0D;
    pub const PRIME_HANDLE_TO_FD: u8 = 0x2D;
    pub const PRIME_FD_TO_HANDLE: u8 = 0x2E;
    pub const SYNCOBJ_CREATE: u8 = 0xBF;
    pub const SYNCOBJ_DESTROY: u8 = 0xC0;
    pub const SYNCOBJ_HANDLE_TO_FD: u8 = 0xC1;
    pub const SYNCOBJ_FD_TO_HANDLE: u8 = 0xC2;
    pub const SYNCOBJ_WAIT: u8 = 0xC3;
    pub const SYNCOBJ_RESET: u8 = 0xC4;
    pub const SYNCOBJ_SIGNAL: u8 = 0xC5;
    pub const SYNCOBJ_TIMELINE_WAIT: u8 = 0xCA;
    pub const SYNCOBJ_QUERY: u8 = 0xCB;
    pub const SYNCOBJ_TRANSFER: u8 = 0xCC;
    pub const SYNCOBJ_TIMELINE_SIGNAL: u8 = 0xCD;

    const I915: u8 = 0x40;
    pub const GETPARAM: u8 = I915 + 0x06;
    pub const GEM_BUSY: u8 = I915 + 0x17;
    pub const GEM_CREATE: u8 = I915 + 0x1B;
    pub const GEM_MMAP: u8 = I915 + 0x1E;
    pub const GEM_SET_DOMAIN: u8 = I915 + 0x1F;
    pub const GEM_SET_TILING: u8 = I915 + 0x21;
    pub const GEM_GET_TILING: u8 = I915 + 0x22;
    pub const GEM_GET_APERTURE: u8 = I915 + 0x23;
    /// `MMAP_OFFSET`, and its older, shorter form `MMAP_GTT`.
    pub const GEM_MMAP_OFFSET: u8 = I915 + 0x24;
    pub const GEM_MADVISE: u8 = I915 + 0x26;
    pub const GEM_EXECBUFFER2: u8 = I915 + 0x29;
    pub const GEM_WAIT: u8 = I915 + 0x2C;
    /// `CONTEXT_CREATE`, and its longer form `CONTEXT_CREATE_EXT`.
    pub const GEM_CONTEXT_CREATE: u8 = I915 + 0x2D;
    pub const GEM_CONTEXT_DESTROY: u8 = I915 + 0x2E;
    pub const GEM_SET_CACHING: u8 = I915 + 0x2F;
    pub const GEM_GET_CACHING: u8 = I915 + 0x30;
    pub const REG_READ: u8 = I915 + 0x31;
    pub const GET_RESET_STATS: u8 = I915 + 0x32;
    pub const GEM_USERPTR: u8 = I915 + 0x33;
    pub const GEM_CONTEXT_GETPARAM: u8 = I915 + 0x34;
    pub const GEM_CONTEXT_SETPARAM: u8 = I915 + 0x35;
    pub const QUERY: u8 = I915 + 0x39;
    pub const GEM_VM_CREATE: u8 = I915 + 0x3A;
    pub const GEM_VM_DESTROY: u8 = I915 + 0x3B;
    pub const GEM_CREATE_EXT: u8 = I915 + 0x3C;

    /// Veda's: the device's PCI identity, which libdrm reads from sysfs on
    /// Linux (`DRM_IOCTL_VEDA_PCI_INFO` in the renderer's `xf86drm.h`).
    pub const VEDA_PCI_INFO: u8 = 0xFE;
}

mod cap {
    pub const PRIME: u64 = 0x5;
    pub const TIMESTAMP_MONOTONIC: u64 = 0x6;
    pub const SYNCOBJ: u64 = 0x13;
    pub const SYNCOBJ_TIMELINE: u64 = 0x14;
}

mod syncobj {
    pub const CREATE_SIGNALED: u32 = 1;
    pub const WAIT_ALL: u32 = 1 << 0;
    pub const WAIT_FOR_SUBMIT: u32 = 1 << 1;
    pub const WAIT_AVAILABLE: u32 = 1 << 2;
    pub const WAIT_DEADLINE: u32 = 1 << 3;
}

/// `I915_PARAM_*`.
mod param {
    pub const CHIPSET_ID: i32 = 4;
    pub const HAS_GEM: i32 = 5;
    pub const NUM_FENCES_AVAIL: i32 = 6;
    pub const HAS_EXECBUF2: i32 = 9;
    pub const HAS_BSD: i32 = 10;
    pub const HAS_BLT: i32 = 11;
    pub const HAS_RELAXED_DELTA: i32 = 15;
    pub const HAS_GEN7_SOL_RESET: i32 = 16;
    pub const HAS_LLC: i32 = 17;
    pub const HAS_ALIASING_PPGTT: i32 = 18;
    pub const HAS_WAIT_TIMEOUT: i32 = 19;
    pub const HAS_VEBOX: i32 = 22;
    pub const HAS_EXEC_NO_RELOC: i32 = 25;
    pub const HAS_EXEC_HANDLE_LUT: i32 = 26;
    pub const MMAP_VERSION: i32 = 30;
    pub const REVISION: i32 = 32;
    pub const SUBSLICE_TOTAL: i32 = 33;
    pub const EU_TOTAL: i32 = 34;
    pub const HAS_EXEC_SOFTPIN: i32 = 37;
    pub const MMAP_GTT_VERSION: i32 = 40;
    pub const HAS_SCHEDULER: i32 = 41;
    pub const HAS_EXEC_ASYNC: i32 = 43;
    pub const HAS_EXEC_FENCE: i32 = 44;
    pub const HAS_EXEC_CAPTURE: i32 = 45;
    pub const SLICE_MASK: i32 = 46;
    pub const SUBSLICE_MASK: i32 = 47;
    pub const HAS_EXEC_BATCH_FIRST: i32 = 48;
    pub const HAS_EXEC_FENCE_ARRAY: i32 = 49;
    pub const HAS_CONTEXT_ISOLATION: i32 = 50;
    pub const CS_TIMESTAMP_FREQUENCY: i32 = 51;
    pub const HAS_EXEC_TIMELINE_FENCES: i32 = 55;
    pub const HAS_USERPTR_PROBE: i32 = 56;
    pub const PXP_STATUS: i32 = 58;
}

/// `I915_CONTEXT_PARAM_*`.
mod ctx_param {
    pub const BAN_PERIOD: u64 = 0x1;
    pub const GTT_SIZE: u64 = 0x3;
    pub const NO_ERROR_CAPTURE: u64 = 0x4;
    pub const BANNABLE: u64 = 0x5;
    pub const PRIORITY: u64 = 0x6;
    pub const RECOVERABLE: u64 = 0x8;
    pub const VM: u64 = 0x9;
    pub const ENGINES: u64 = 0xA;
    pub const PERSISTENCE: u64 = 0xB;
    pub const RINGSIZE: u64 = 0xC;
    pub const PROTECTED_CONTENT: u64 = 0xD;
}

/// `I915_EXEC_*` and `EXEC_OBJECT_*`.
mod exec {
    pub const RING_MASK: u64 = 0x3F;
    pub const RENDER: u64 = 1;
    pub const BSD: u64 = 2;
    pub const BLT: u64 = 3;
    pub const VEBOX: u64 = 4;
    pub const CONSTANTS_MASK: u64 = 3 << 6;
    pub const GEN7_SOL_RESET: u64 = 1 << 8;
    pub const IS_PINNED: u64 = 1 << 10;
    pub const NO_RELOC: u64 = 1 << 11;
    pub const HANDLE_LUT: u64 = 1 << 12;
    pub const BSD_SHIFT: u32 = 13;
    pub const BSD_MASK: u64 = 3 << BSD_SHIFT;
    pub const BATCH_FIRST: u64 = 1 << 18;
    pub const FENCE_ARRAY: u64 = 1 << 19;
    /// What the layer carries out (sync files, the secure and resource
    /// streamer bits, extensions are refused).
    pub const KNOWN: u64 = RING_MASK
        | CONSTANTS_MASK
        | GEN7_SOL_RESET
        | IS_PINNED
        | NO_RELOC
        | HANDLE_LUT
        | BSD_MASK
        | BATCH_FIRST
        | FENCE_ARRAY;

    pub const FENCE_WAIT: u32 = 1 << 0;
    pub const FENCE_SIGNAL: u32 = 1 << 1;

    pub const OBJECT_NEEDS_FENCE: u64 = 1 << 0;
    pub const OBJECT_NEEDS_GTT: u64 = 1 << 1;
    pub const OBJECT_WRITE: u64 = 1 << 2;
    pub const OBJECT_SUPPORTS_48B_ADDRESS: u64 = 1 << 3;
    pub const OBJECT_PINNED: u64 = 1 << 4;
    pub const OBJECT_ASYNC: u64 = 1 << 6;
    pub const OBJECT_CAPTURE: u64 = 1 << 7;
    pub const OBJECT_KNOWN: u64 = OBJECT_NEEDS_FENCE
        | OBJECT_NEEDS_GTT
        | OBJECT_WRITE
        | OBJECT_SUPPORTS_48B_ADDRESS
        | OBJECT_PINNED
        | OBJECT_ASYNC
        | OBJECT_CAPTURE;
}

/// `DRM_I915_QUERY_*`.
mod query {
    pub const TOPOLOGY_INFO: u64 = 1;
    pub const ENGINE_INFO: u64 = 2;
    pub const MEMORY_REGIONS: u64 = 4;
    pub const HWCONFIG_BLOB: u64 = 5;
    pub const GEOMETRY_SUBSLICES: u64 = 6;
}

/// i915's extension names: of `GEM_CREATE_EXT`, and of
/// `CONTEXT_CREATE_EXT`.
mod ext {
    pub const CREATE_MEMORY_REGIONS: u32 = 0;
    pub const CREATE_PROTECTED_CONTENT: u32 = 1;
    pub const CREATE_SET_PAT: u32 = 2;
    pub const CONTEXT_SETPARAM: u32 = 0;
    /// The longest chain followed.
    pub const MAX_CHAIN: usize = 64;
}

const PAGE: u64 = vabi::PAGE_SIZE as u64;
/// The bits of a GPU address (48: four levels of page tables).
const ADDRESS_BITS: u32 = 48;
/// The render engine's timestamp register (`RCS_TIMESTAMP`), which
/// `REG_READ` reads.
const RCS_TIMESTAMP: u64 = 0x2358;
/// The most buffers a submission takes.
const MAX_EXEC_BUFFERS: u32 = 1 << 16;
/// A mapping's offset: the buffer's handle above, the offset into it
/// below (`MMAP_OFFSET` gives the buffer's start).
const OFFSET_SHIFT: u32 = 32;

// ---- The arguments ---------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Version {
    major: i32,
    minor: i32,
    patch: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Handle {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjCreate {
    handle: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjWait {
    handles: u64,
    /// Absolute, on the monotonic clock.
    timeout: i64,
    count: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout: i64,
    count: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
    deadline: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjArray {
    handles: u64,
    count: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjTimelineArray {
    handles: u64,
    points: u64,
    count: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SyncobjTransfer {
    src_handle: u32,
    dst_handle: u32,
    src_point: u64,
    dst_point: u64,
    flags: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct PciInfo {
    domain: u16,
    bus: u8,
    dev: u8,
    func: u8,
    revision: u8,
    vendor: u16,
    device: u16,
    subvendor: u16,
    subdevice: u16,
    pad: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Getparam {
    param: i32,
    pad: u32,
    /// Where the `int` goes.
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GemCreate {
    size: u64,
    handle: u32,
    /// `GEM_CREATE_EXT`'s flags (`GEM_CREATE` has padding there).
    flags: u32,
    /// `GEM_CREATE_EXT`'s extensions (past `GEM_CREATE`'s end).
    extensions: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct UserExtension {
    next: u64,
    name: u32,
    flags: u32,
    rsvd: [u32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct MemoryRegions {
    base: UserExtension,
    pad: u32,
    count: u32,
    regions: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GemMmap {
    handle: u32,
    pad: u32,
    offset: u64,
    size: u64,
    addr: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GemMmapOffset {
    handle: u32,
    pad: u32,
    offset: u64,
    /// `MMAP_OFFSET`'s (`MMAP_GTT` ends before: 0, a GTT mapping).
    flags: u64,
    extensions: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SetDomain {
    handle: u32,
    read_domains: u32,
    write_domain: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Busy {
    handle: u32,
    busy: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct GemWait {
    handle: u32,
    flags: u32,
    /// Relative; negative waits forever. The time left comes back.
    timeout: i64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Madvise {
    handle: u32,
    madv: u32,
    retained: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Caching {
    handle: u32,
    caching: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Aperture {
    size: u64,
    available: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Execbuffer2 {
    buffers: u64,
    buffer_count: u32,
    batch_start: u32,
    batch_len: u32,
    dr1: u32,
    dr4: u32,
    num_cliprects: u32,
    /// The fences, with `FENCE_ARRAY`.
    cliprects: u64,
    flags: u64,
    /// The context (its low 32 bits).
    rsvd1: u64,
    rsvd2: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ExecObject2 {
    handle: u32,
    relocation_count: u32,
    relocs: u64,
    alignment: u64,
    /// Where the buffer is, in canonical form (bit 47 copied above).
    offset: u64,
    flags: u64,
    rsvd1: u64,
    rsvd2: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ExecFence {
    handle: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ContextCreate {
    ctx_id: u32,
    /// `CONTEXT_CREATE_EXT`'s (`CONTEXT_CREATE` ends before).
    flags: u32,
    extensions: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ContextParam {
    ctx_id: u32,
    size: u32,
    param: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct SetparamExtension {
    base: UserExtension,
    param: ContextParam,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct VmControl {
    extensions: u64,
    flags: u32,
    vm_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct RegRead {
    offset: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct ResetStats {
    ctx_id: u32,
    flags: u32,
    reset_count: u32,
    batch_active: u32,
    batch_pending: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Query {
    num_items: u32,
    flags: u32,
    items: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct QueryItem {
    id: u64,
    /// In: the buffer's bytes (0: how many are needed). Out: the bytes, or
    /// `-errno`.
    length: i32,
    flags: u32,
    data: u64,
}

/// The argument of a request as the struct `T`: the bytes the program
/// passed, zeros past them (Linux's DRM core copies them so, which lets
/// shorter and longer versions of a struct meet).
///
/// # Safety
/// `arg` must point at the request's bytes.
unsafe fn load<T: Copy + Default>(arg: usize, ioc: Ioc) -> Result<T, isize> {
    let mut v = T::default();
    if ioc.write {
        let n = ioc.size.min(size_of::<T>());
        // SAFETY: per the caller.
        let src = unsafe { user::slice(arg, n)? };
        // SAFETY: `v` has `size_of::<T>()` bytes; plain data.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), (&raw mut v).cast::<u8>(), n) };
    }
    Ok(v)
}

/// Gives the program back the argument of a request that returns it.
///
/// # Safety
/// As [`load`].
unsafe fn store<T: Copy>(arg: usize, ioc: Ioc, v: &T) -> Result<(), isize> {
    if ioc.read {
        let n = ioc.size.min(size_of::<T>());
        // SAFETY: per the caller.
        let dst = unsafe { user::slice_mut(arg, n)? };
        // SAFETY: `v` has `size_of::<T>()` bytes.
        unsafe { core::ptr::copy_nonoverlapping((v as *const T).cast::<u8>(), dst.as_mut_ptr(), n) };
    }
    Ok(())
}

/// Runs `f` on a request's argument, and gives the argument back, after a
/// failure too (as Linux does: `SET_TILING` and `GEM_WAIT` say so).
///
/// # Safety
/// As [`load`].
unsafe fn with<T: Copy + Default>(arg: usize, ioc: Ioc, f: impl FnOnce(&mut T) -> Result<(), isize>) -> SysResult {
    // SAFETY: per the caller.
    let mut v: T = unsafe { load(arg, ioc)? };
    let r = f(&mut v);
    // SAFETY: as above.
    unsafe { store(arg, ioc, &v)? };
    r.map(|()| 0)
}

/// `count` values the program passed at `p`.
///
/// # Safety
/// `p` must point at `count` `T`s.
unsafe fn read_array<T: Copy>(p: u64, count: usize) -> Result<Vec<T>, isize> {
    // SAFETY: per the caller.
    (0..count).map(|i| unsafe { user::read::<T>(p as usize + i * size_of::<T>()) }).collect()
}

/// Copies a string into the program's buffer of `*len` bytes (if it gave
/// one) and tells it the string's length, as Linux's `drm_copy_field`.
///
/// # Safety
/// `p` must be null or point at `*len` writable bytes.
unsafe fn copy_field(p: u64, len: &mut u64, s: &[u8]) -> Result<(), isize> {
    let n = usize::try_from(*len).unwrap_or(usize::MAX).min(s.len());
    if n > 0 && p != 0 {
        // SAFETY: per the caller.
        unsafe { user::slice_mut(p as usize, n)? }.copy_from_slice(&s[..n]);
    }
    *len = s.len() as u64;
    Ok(())
}

/// The canonical form of a GPU address: its top bit copied above.
fn canonical(address: u64) -> u64 {
    let shift = 64 - ADDRESS_BITS;
    (((address << shift) as i64) >> shift) as u64
}

// ---- What the layer keeps --------------------------------------------------

/// Handle numbers as Linux gives them: the lowest free, from 1.
#[derive(Debug, Default)]
struct Ids {
    free: BTreeSet<u32>,
    /// The highest given out.
    top: u32,
}

impl Ids {
    fn take(&mut self) -> Result<u32, isize> {
        if let Some(n) = self.free.pop_first() {
            return Ok(n);
        }
        self.top = self.top.checked_add(1).ok_or(ENOSPC)?;
        Ok(self.top)
    }

    fn give(&mut self, n: u32) {
        if n != self.top {
            self.free.insert(n);
            return;
        }
        self.top -= 1;
        while self.top > 0 && self.free.remove(&self.top) {
            self.top -= 1;
        }
    }
}

struct Buffer {
    /// The driver's handle for it.
    driver: u32,
    vmo: Vmo,
    size: u64,
    /// Its last submission on each of the device's engines, and the last
    /// that wrote it.
    reads: Vec<u64>,
    write: Option<Point>,
    caching: u32,
}

/// What a syncobj holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fence {
    /// Nothing yet (made unsignaled, or reset).
    Unset,
    Signaled,
    /// A submission, done when its engine has passed it.
    At(Point),
}

/// An address space as iris sees it.
struct Space {
    /// The driver's, made when a context in it is first submitted to.
    driver: Option<u32>,
    /// Its i915 handles and the contexts in it.
    refs: u32,
}

struct Context {
    space: u32,
    /// Its engine map (`CONTEXT_PARAM_ENGINES`); without one, i915's
    /// rings (`I915_EXEC_RENDER`, ...) choose among the device's engines.
    engines: Option<Vec<Engine>>,
    /// The driver's, made on the first submission.
    driver: Option<u32>,
    priority: i64,
    recoverable: bool,
    bannable: bool,
}

impl Context {
    fn new(space: u32) -> Context {
        Context { space, engines: None, driver: None, priority: 0, recoverable: true, bannable: true }
    }
}

struct State {
    gem: gem::Client,
    buffers: BTreeMap<u32, Buffer>,
    buffer_ids: Ids,
    syncobjs: BTreeMap<u32, Fence>,
    syncobj_ids: Ids,
    /// Context 0 is the file's default context.
    contexts: BTreeMap<u32, Context>,
    context_ids: Ids,
    /// i915's address space handles: the space each names.
    vms: BTreeMap<u32, u32>,
    vm_ids: Ids,
    /// Space 0 is the default context's.
    spaces: BTreeMap<u32, Space>,
    next_space: u32,
}

/// The `errno` for a call to the driver.
fn call<T>(r: Result<Result<T, GemError>, IpcError>) -> Result<T, isize> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(GemError::Invalid)) => Err(EINVAL),
        Ok(Err(GemError::NoMemory)) => Err(ENOMEM),
        Ok(Err(GemError::Lost)) | Err(_) => Err(EIO),
        Ok(Err(GemError::Unsupported)) => Err(ENODEV),
    }
}

impl State {
    fn buffer(&mut self, handle: u32) -> Result<&mut Buffer, isize> {
        self.buffers.get_mut(&handle).ok_or(ENOENT)
    }

    fn context(&mut self, id: u32) -> Result<&mut Context, isize> {
        self.contexts.get_mut(&id).ok_or(ENOENT)
    }

    fn new_space(&mut self) -> u32 {
        self.next_space += 1;
        self.spaces.insert(self.next_space, Space { driver: None, refs: 1 });
        self.next_space
    }

    fn hold_space(&mut self, id: u32) {
        if let Some(s) = self.spaces.get_mut(&id) {
            s.refs += 1;
        }
    }

    /// Lets go of a space: forgotten, the driver's with it, at the last.
    fn release_space(&mut self, id: u32) {
        let Some(s) = self.spaces.get_mut(&id) else { return };
        s.refs -= 1;
        if s.refs == 0
            && let Some(s) = self.spaces.remove(&id)
            && let Some(d) = s.driver
        {
            let _ = self.gem.vm_destroy(d);
        }
    }

    /// A new i915 handle for space `id`.
    fn vm_handle(&mut self, id: u32) -> Result<u32, isize> {
        let vm = self.vm_ids.take()?;
        self.vms.insert(vm, id);
        self.hold_space(id);
        Ok(vm)
    }

    /// The driver's context for context `id`, made (with its address space)
    /// at the first submission.
    fn driver_context(&mut self, id: u32, device: &GemDevice) -> Result<u32, isize> {
        let ctx = self.context(id)?;
        if let Some(d) = ctx.driver {
            return Ok(d);
        }
        let engines = ctx.engines.clone().unwrap_or_else(|| device.engines.clone());
        let sid = ctx.space;
        let space = self.spaces.get_mut(&sid).ok_or(EINVAL)?;
        let vm = match space.driver {
            Some(vm) => vm,
            None => {
                let vm = call(self.gem.vm_create())?;
                space.driver = Some(vm);
                vm
            }
        };
        let d = call(self.gem.context_create(vm, engines))?;
        self.context(id)?.driver = Some(d);
        Ok(d)
    }
}

/// The render node, open: a session with the GPU's driver.
pub struct Drm {
    device: GemDevice,
    /// The engines' completed numbers (the session's fence page).
    fences: Mapping,
    /// Signaled by the driver as they move, and by the layer as syncobjs
    /// get submissions.
    progress: Event,
    /// The connection's channel, closed if the driver goes.
    channel: vabi::RawHandle,
    state: Mutex<State>,
    /// The threads waiting on `progress` (see [`Drm::wait`]).
    waiters: Mutex<u32>,
}

/// Opens the render node: a session with the GPU's driver (`ENOENT` if
/// there is none: no GPU iris drives).
pub fn open() -> Result<Drm, isize> {
    let channel = vproto::connect(gem::NAME).map_err(|_| ENOENT)?;
    let gem = gem::Client::new(channel);
    let session = call(gem.open())?;
    let engines = session.device.engines.len();
    if engines == 0 || engines * 8 > PAGE as usize {
        return Err(EIO);
    }
    let fences = Mapping::new(session.fences, PAGE as usize, map_flags::READ).map_err(crate::error::kernel)?;
    let channel = gem.channel().raw();
    let mut spaces = BTreeMap::new();
    // The default context's space, which nothing lets go of.
    spaces.insert(0, Space { driver: None, refs: 1 });
    let mut contexts = BTreeMap::new();
    contexts.insert(0, Context::new(0));
    Ok(Drm {
        device: session.device,
        fences,
        progress: session.progress,
        channel,
        state: Mutex::new(State {
            gem,
            buffers: BTreeMap::new(),
            buffer_ids: Ids::default(),
            syncobjs: BTreeMap::new(),
            syncobj_ids: Ids::default(),
            contexts,
            context_ids: Ids::default(),
            vms: BTreeMap::new(),
            vm_ids: Ids::default(),
            spaces,
            next_space: 0,
        }),
        waiters: Mutex::new(0),
    })
}

impl Drm {
    /// Carries out the ioctl `request`.
    ///
    /// # Safety
    /// `arg` must be what the request says: its argument (and the memory
    /// that points at).
    pub unsafe fn ioctl(&self, request: u32, arg: usize) -> SysResult {
        let Some(ioc) = Ioc::decode(request) else { return Err(ENOTTY) };
        // SAFETY (whole match): per the caller; each request's argument is
        // the struct its handler takes.
        unsafe {
            match ioc.nr {
                nr::VERSION => with(arg, ioc, |v: &mut Version| self.version(v)),
                nr::GET_CAP => with(arg, ioc, |c: &mut GetCap| self.get_cap(c)),
                nr::GEM_CLOSE => with(arg, ioc, |c: &mut Handle| self.gem_close(c.handle)),
                // Render nodes are not authenticated and name nothing
                // globally.
                nr::GET_MAGIC | nr::GEM_FLINK | nr::GEM_OPEN => Err(EACCES),
                nr::SET_CLIENT_CAP => Err(EINVAL),
                nr::PRIME_HANDLE_TO_FD | nr::PRIME_FD_TO_HANDLE => Err(EOPNOTSUPP),
                nr::SYNCOBJ_CREATE => with(arg, ioc, |c: &mut SyncobjCreate| self.syncobj_create(c)),
                nr::SYNCOBJ_DESTROY => with(arg, ioc, |c: &mut Handle| self.syncobj_destroy(c)),
                nr::SYNCOBJ_HANDLE_TO_FD | nr::SYNCOBJ_FD_TO_HANDLE => Err(EOPNOTSUPP),
                nr::SYNCOBJ_WAIT => with(arg, ioc, |w: &mut SyncobjWait| {
                    let handles = read_array::<u32>(w.handles, w.count as usize)?;
                    w.first_signaled = self.syncobj_wait(&handles, w.timeout, w.flags)?;
                    Ok(())
                }),
                nr::SYNCOBJ_TIMELINE_WAIT => with(arg, ioc, |w: &mut SyncobjTimelineWait| {
                    let handles = read_array::<u32>(w.handles, w.count as usize)?;
                    if read_array::<u64>(w.points, w.count as usize)?.iter().any(|&p| p != 0) {
                        return Err(EINVAL);
                    }
                    w.first_signaled = self.syncobj_wait(&handles, w.timeout, w.flags)?;
                    Ok(())
                }),
                nr::SYNCOBJ_RESET | nr::SYNCOBJ_SIGNAL => with(arg, ioc, |a: &mut SyncobjArray| {
                    if a.pad != 0 {
                        return Err(EINVAL);
                    }
                    let handles = read_array::<u32>(a.handles, a.count as usize)?;
                    let fence = if ioc.nr == nr::SYNCOBJ_SIGNAL { Fence::Signaled } else { Fence::Unset };
                    self.syncobj_set(&handles, fence)
                }),
                nr::SYNCOBJ_TIMELINE_SIGNAL => with(arg, ioc, |a: &mut SyncobjTimelineArray| {
                    let handles = read_array::<u32>(a.handles, a.count as usize)?;
                    if a.flags != 0 || read_array::<u64>(a.points, a.count as usize)?.iter().any(|&p| p != 0) {
                        return Err(EINVAL);
                    }
                    self.syncobj_set(&handles, Fence::Signaled)
                }),
                nr::SYNCOBJ_QUERY => with(arg, ioc, |a: &mut SyncobjTimelineArray| {
                    // Binary syncobjs are all at point 0.
                    let handles = read_array::<u32>(a.handles, a.count as usize)?;
                    let s = self.state.lock();
                    if handles.iter().any(|h| !s.syncobjs.contains_key(h)) {
                        return Err(ENOENT);
                    }
                    for i in 0..handles.len() {
                        user::write::<u64>(a.points as usize + i * 8, 0)?;
                    }
                    Ok(())
                }),
                nr::SYNCOBJ_TRANSFER => with(arg, ioc, |t: &mut SyncobjTransfer| self.syncobj_transfer(t)),
                nr::VEDA_PCI_INFO => with(arg, ioc, |p: &mut PciInfo| {
                    let d = &self.device;
                    *p = PciInfo {
                        domain: d.domain,
                        bus: d.bus,
                        dev: d.dev,
                        func: d.func,
                        revision: d.revision,
                        vendor: d.vendor,
                        device: d.device,
                        subvendor: d.subvendor,
                        subdevice: d.subdevice,
                        pad: 0,
                    };
                    Ok(())
                }),
                nr::GETPARAM => with(arg, ioc, |g: &mut Getparam| {
                    let v = self.getparam(g.param)?;
                    user::write::<i32>(g.value as usize, v)
                }),
                nr::QUERY => with(arg, ioc, |q: &mut Query| self.query(q)),
                nr::GEM_CREATE | nr::GEM_CREATE_EXT => with(arg, ioc, |c: &mut GemCreate| {
                    if ioc.nr == nr::GEM_CREATE {
                        c.flags = 0;
                        c.extensions = 0;
                    }
                    self.gem_create(c)
                }),
                nr::GEM_MMAP_OFFSET => with(arg, ioc, |m: &mut GemMmapOffset| self.mmap_offset(m)),
                nr::GEM_MMAP => with(arg, ioc, |m: &mut GemMmap| self.gem_mmap(m)),
                nr::GEM_SET_DOMAIN => with(arg, ioc, |d: &mut SetDomain| self.set_domain(d)),
                nr::GEM_BUSY => with(arg, ioc, |b: &mut Busy| self.busy(b)),
                nr::GEM_WAIT => with(arg, ioc, |w: &mut GemWait| self.gem_wait(w)),
                nr::GEM_MADVISE => with(arg, ioc, |m: &mut Madvise| {
                    if m.madv > 1 {
                        return Err(EINVAL);
                    }
                    self.state.lock().buffer(m.handle)?;
                    // Nothing is ever purged.
                    m.retained = 1;
                    Ok(())
                }),
                nr::GEM_SET_CACHING => with(arg, ioc, |c: &mut Caching| {
                    if c.caching > 2 {
                        return Err(EINVAL);
                    }
                    self.state.lock().buffer(c.handle)?.caching = c.caching;
                    Ok(())
                }),
                nr::GEM_GET_CACHING => with(arg, ioc, |c: &mut Caching| {
                    c.caching = self.state.lock().buffer(c.handle)?.caching;
                    Ok(())
                }),
                // No fence registers (as on i915's newer GPUs): tiling is
                // the driver's business, in its surface states.
                nr::GEM_SET_TILING | nr::GEM_GET_TILING => Err(EOPNOTSUPP),
                nr::GEM_GET_APERTURE => with(arg, ioc, |a: &mut Aperture| {
                    *a = Aperture { size: self.device.memory, available: self.device.memory };
                    Ok(())
                }),
                nr::GEM_USERPTR => Err(ENODEV),
                nr::GEM_EXECBUFFER2 => with(arg, ioc, |e: &mut Execbuffer2| self.execbuffer(e)),
                nr::GEM_CONTEXT_CREATE => with(arg, ioc, |c: &mut ContextCreate| self.context_create(c)),
                nr::GEM_CONTEXT_DESTROY => with(arg, ioc, |c: &mut Handle| self.context_destroy(c.handle)),
                nr::GEM_CONTEXT_GETPARAM => with(arg, ioc, |p: &mut ContextParam| self.context_getparam(p)),
                nr::GEM_CONTEXT_SETPARAM => with(arg, ioc, |p: &mut ContextParam| {
                    let mut s = self.state.lock();
                    self.context_setparam(&mut s, p.ctx_id, p)
                }),
                nr::GEM_VM_CREATE => with(arg, ioc, |v: &mut VmControl| {
                    if v.extensions != 0 || v.flags != 0 {
                        return Err(EINVAL);
                    }
                    let mut s = self.state.lock();
                    let space = s.new_space();
                    let r = s.vm_handle(space);
                    // The handle holds it now.
                    s.release_space(space);
                    v.vm_id = r?;
                    Ok(())
                }),
                nr::GEM_VM_DESTROY => with(arg, ioc, |v: &mut VmControl| {
                    if v.extensions != 0 || v.flags != 0 {
                        return Err(EINVAL);
                    }
                    let mut s = self.state.lock();
                    let space = s.vms.remove(&v.vm_id).ok_or(ENOENT)?;
                    s.vm_ids.give(v.vm_id);
                    s.release_space(space);
                    Ok(())
                }),
                nr::REG_READ => with(arg, ioc, |r: &mut RegRead| {
                    // The timestamp alone (its 64-bit read: the low bit).
                    if r.offset & !1 != RCS_TIMESTAMP {
                        return Err(EINVAL);
                    }
                    r.value = call(self.state.lock().gem.timestamp())?;
                    Ok(())
                }),
                nr::GET_RESET_STATS => with(arg, ioc, |r: &mut ResetStats| {
                    if r.flags != 0 {
                        return Err(EINVAL);
                    }
                    let mut s = self.state.lock();
                    let driver = s.context(r.ctx_id)?.driver;
                    let (active, pending) = match driver {
                        Some(d) => call(s.gem.reset_stats(d))?,
                        None => (0, 0),
                    };
                    r.reset_count = 0;
                    r.batch_active = active;
                    r.batch_pending = pending;
                    Ok(())
                }),
                _ => Err(EINVAL),
            }
        }
    }

    fn version(&self, v: &mut Version) -> Result<(), isize> {
        // i915's.
        v.major = 1;
        v.minor = 6;
        v.patch = 0;
        // SAFETY: the program gave buffers of the lengths it says.
        unsafe {
            copy_field(v.name, &mut v.name_len, b"i915")?;
            copy_field(v.date, &mut v.date_len, b"20201103")?;
            copy_field(v.desc, &mut v.desc_len, b"Intel Graphics")?;
        }
        Ok(())
    }

    fn get_cap(&self, c: &mut GetCap) -> Result<(), isize> {
        c.value = match c.capability {
            cap::PRIME | cap::SYNCOBJ_TIMELINE => 0,
            cap::TIMESTAMP_MONOTONIC | cap::SYNCOBJ => 1,
            _ => return Err(EINVAL),
        };
        Ok(())
    }

    fn getparam(&self, p: i32) -> Result<i32, isize> {
        let d = &self.device;
        let has = |c: u16| d.engines.iter().any(|e| e.class == c) as i32;
        let subslices: u32 = d.subslice_masks.iter().map(|m| m.count_ones()).sum();
        Ok(match p {
            param::CHIPSET_ID => i32::from(d.device),
            param::REVISION => i32::from(d.revision),
            param::HAS_BSD => has(class::VIDEO),
            param::HAS_BLT => has(class::COPY),
            param::HAS_VEBOX => has(class::VIDEO_ENHANCE),
            param::NUM_FENCES_AVAIL | param::HAS_SCHEDULER | param::HAS_EXEC_FENCE => 0,
            param::HAS_EXEC_TIMELINE_FENCES | param::HAS_USERPTR_PROBE => 0,
            param::HAS_GEM
            | param::HAS_EXECBUF2
            | param::HAS_RELAXED_DELTA
            | param::HAS_GEN7_SOL_RESET
            | param::HAS_LLC
            | param::HAS_WAIT_TIMEOUT
            | param::HAS_EXEC_NO_RELOC
            | param::HAS_EXEC_HANDLE_LUT
            | param::HAS_EXEC_SOFTPIN
            | param::HAS_EXEC_ASYNC
            | param::HAS_EXEC_CAPTURE
            | param::HAS_EXEC_BATCH_FIRST
            | param::HAS_EXEC_FENCE_ARRAY
            | param::MMAP_VERSION => 1,
            // Full per-process address spaces.
            param::HAS_ALIASING_PPGTT => 2,
            // `MMAP_OFFSET`, but not of part of a buffer.
            param::MMAP_GTT_VERSION => 4,
            param::SUBSLICE_TOTAL => subslices as i32,
            param::EU_TOTAL => (subslices * d.eu_mask.count_ones()) as i32,
            param::SLICE_MASK => d.slice_mask as i32,
            param::SUBSLICE_MASK => d.subslice_masks.first().copied().unwrap_or(0) as i32,
            // Every context its own: the classes isolated.
            param::HAS_CONTEXT_ISOLATION => d.engines.iter().fold(0, |m, e| m | 1 << e.class),
            param::CS_TIMESTAMP_FREQUENCY => i32::try_from(d.timestamp_hz).unwrap_or(i32::MAX),
            // No protected content.
            param::PXP_STATUS => return Err(ENODEV),
            _ => return Err(EINVAL),
        })
    }

    // ---- Queries ----------------------------------------------------------

    fn query(&self, q: &mut Query) -> Result<(), isize> {
        if q.flags != 0 {
            return Err(EINVAL);
        }
        for i in 0..q.num_items as usize {
            let at = q.items as usize + i * size_of::<QueryItem>();
            // SAFETY: the program passed `num_items` items.
            let mut item: QueryItem = unsafe { user::read(at)? };
            match self.query_data(&item) {
                Ok(data) => {
                    let len = i32::try_from(data.len()).map_err(|_| EINVAL)?;
                    if item.length == 0 {
                        item.length = len;
                    } else if item.length < len {
                        item.length = -(EINVAL as i32);
                    } else {
                        // SAFETY: the program's buffer of `length` bytes.
                        unsafe { user::slice_mut(item.data as usize, data.len())? }.copy_from_slice(&data);
                        item.length = len;
                    }
                }
                Err(e) => item.length = -(e as i32),
            }
            // SAFETY: as above.
            unsafe { user::write(at, item)? };
        }
        Ok(())
    }

    fn query_data(&self, item: &QueryItem) -> Result<Vec<u8>, isize> {
        if item.flags != 0 {
            return Err(EINVAL);
        }
        match item.id {
            // On these GPUs every subslice draws.
            query::TOPOLOGY_INFO | query::GEOMETRY_SUBSLICES => Ok(topology(&self.device)),
            query::ENGINE_INFO => Ok(engine_info(&self.device)),
            query::MEMORY_REGIONS => Ok(memory_regions(&self.device)),
            query::HWCONFIG_BLOB => call(self.state.lock().gem.hwconfig()).map(|b| b.0),
            _ => Err(EINVAL),
        }
    }

    // ---- Buffers ----------------------------------------------------------

    fn gem_create(&self, c: &mut GemCreate) -> Result<(), isize> {
        if c.flags & !1 != 0 {
            return Err(EINVAL);
        }
        let mut next = c.extensions;
        for _ in 0..ext::MAX_CHAIN {
            if next == 0 {
                break;
            }
            // SAFETY: the program's chain of extensions.
            let e: UserExtension = unsafe { user::read(next as usize)? };
            match e.name {
                ext::CREATE_MEMORY_REGIONS => {
                    // SAFETY: as above.
                    let r: MemoryRegions = unsafe { user::read(next as usize)? };
                    if r.count == 0 || r.pad != 0 {
                        return Err(EINVAL);
                    }
                    // System memory, the only region (class 0, instance 0).
                    // SAFETY: the program passed `count` regions.
                    if unsafe { read_array::<u32>(r.regions, r.count as usize)? }.iter().any(|&r| r != 0) {
                        return Err(EINVAL);
                    }
                }
                ext::CREATE_PROTECTED_CONTENT => return Err(ENODEV),
                // The driver chooses how the GPU caches.
                ext::CREATE_SET_PAT => {}
                _ => return Err(EINVAL),
            }
            next = e.next;
        }
        if next != 0 {
            return Err(E2BIG);
        }
        let size = c.size.checked_next_multiple_of(PAGE).filter(|&s| s > 0).ok_or(EINVAL)?;
        let mut s = self.state.lock();
        let (driver, vmo) = call(s.gem.create(size, 0))?;
        let handle = match s.buffer_ids.take() {
            Ok(h) => h,
            Err(e) => {
                let _ = s.gem.close(driver);
                return Err(e);
            }
        };
        let engines = self.device.engines.len();
        s.buffers.insert(handle, Buffer { driver, vmo, size, reads: vec![0; engines], write: None, caching: 1 });
        c.handle = handle;
        c.size = size;
        Ok(())
    }

    fn gem_close(&self, handle: u32) -> Result<(), isize> {
        let mut s = self.state.lock();
        let b = s.buffers.remove(&handle).ok_or(EINVAL)?;
        s.buffer_ids.give(handle);
        // The driver keeps it while the GPU uses it; mappings keep the
        // memory.
        let _ = s.gem.close(b.driver);
        Ok(())
    }

    fn mmap_offset(&self, m: &mut GemMmapOffset) -> Result<(), isize> {
        const WC: u64 = 1;
        const WB: u64 = 2;
        const UC: u64 = 3;
        if m.extensions != 0 || m.pad != 0 {
            return Err(EINVAL);
        }
        // Mapped as the memory is (write-back: coherent with the GPU,
        // which shares the processor's last-level cache); no aperture, and
        // no memory of the GPU's own.
        if !matches!(m.flags, WC | WB | UC) {
            return Err(ENODEV);
        }
        self.state.lock().buffer(m.handle)?;
        m.offset = u64::from(m.handle) << OFFSET_SHIFT;
        Ok(())
    }

    /// `GEM_MMAP`, the older way: the layer maps it.
    fn gem_mmap(&self, m: &mut GemMmap) -> Result<(), isize> {
        if m.flags & !1 != 0 || m.pad != 0 {
            return Err(EINVAL);
        }
        self.state.lock().buffer(m.handle)?;
        let size = usize::try_from(m.size).map_err(|_| EINVAL)?;
        if m.offset >> OFFSET_SHIFT != 0 {
            return Err(EINVAL);
        }
        let offset = (u64::from(m.handle) << OFFSET_SHIFT) | m.offset;
        m.addr = self.map(offset, size, 0, map_flags::READ | map_flags::WRITE)? as u64;
        Ok(())
    }

    /// Maps part of a buffer (`mmap` of the render node at `offset`):
    /// `flags` are the kernel's.
    pub fn map(&self, offset: u64, len: usize, hint: usize, flags: usize) -> SysResult {
        let handle = u32::try_from(offset >> OFFSET_SHIFT).map_err(|_| EINVAL)?;
        let inner = offset & ((1 << OFFSET_SHIFT) - 1);
        if !inner.is_multiple_of(PAGE) || len == 0 {
            return Err(EINVAL);
        }
        let s = self.state.lock();
        let b = s.buffers.get(&handle).ok_or(EINVAL)?;
        if inner.checked_add(len as u64).is_none_or(|end| end > b.size) {
            return Err(EINVAL);
        }
        vm::map(None, &b.vmo, inner as usize, len, hint, flags).map_err(|e| match e {
            vabi::Error::AlreadyExists => EEXIST,
            vabi::Error::NoMemory | vabi::Error::OutOfRange => ENOMEM,
            e => crate::error::kernel(e),
        })
    }

    /// The points a buffer's last uses are at: its writer, or (`all`) its
    /// readers too.
    fn uses(&self, b: &Buffer, all: bool) -> Vec<Point> {
        let mut out: Vec<Point> = b.write.into_iter().collect();
        if all {
            out.extend(
                b.reads.iter().enumerate().filter(|&(_, &n)| n > 0).map(|(e, &n)| Point { engine: e as u32, seqno: n }),
            );
        }
        out.retain(|&p| !self.passed(p));
        out
    }

    fn set_domain(&self, d: &SetDomain) -> Result<(), isize> {
        // The processor's domains (CPU, GTT, WC), each alone if written.
        const CPU_DOMAINS: u32 = 0x01 | 0x40 | 0x80;
        if d.read_domains & !CPU_DOMAINS != 0 || (d.write_domain != 0 && d.write_domain != d.read_domains) {
            return Err(EINVAL);
        }
        let points = {
            let mut s = self.state.lock();
            let b = s.buffer(d.handle)?;
            // Reading waits for the GPU's writes, writing for all it does.
            let b = &*b;
            self.uses(b, d.write_domain != 0)
        };
        self.wait(vabi::DEADLINE_INFINITE, || Ok(points.iter().all(|&p| self.passed(p))))
    }

    fn busy(&self, b: &mut Busy) -> Result<(), isize> {
        let mut s = self.state.lock();
        let buf = s.buffer(b.handle)?;
        let class = |e: u32| self.device.engines.get(e as usize).map_or(0, |e| u32::from(e.class));
        // i915's encoding: the writer's class + 1 below, the readers'
        // classes as bits above (the writer's among them).
        let mut busy = 0;
        for (e, &n) in buf.reads.iter().enumerate() {
            let p = Point { engine: e as u32, seqno: n };
            if n > 0 && !self.passed(p) {
                busy |= 0x10000 << class(p.engine);
            }
        }
        if let Some(w) = buf.write
            && !self.passed(w)
        {
            busy |= (class(w.engine) + 1) | (0x10000 << class(w.engine));
        }
        b.busy = busy;
        Ok(())
    }

    fn gem_wait(&self, w: &mut GemWait) -> Result<(), isize> {
        if w.flags != 0 {
            return Err(EINVAL);
        }
        let points = {
            let mut s = self.state.lock();
            let b = s.buffer(w.handle)?;
            let b = &*b;
            self.uses(b, true)
        };
        let start = time::monotonic_ns();
        let deadline = if w.timeout < 0 { vabi::DEADLINE_INFINITE } else { start.saturating_add(w.timeout as u64) };
        let r = self.wait(deadline, || Ok(points.iter().all(|&p| self.passed(p))));
        if w.timeout > 0 {
            w.timeout = deadline.saturating_sub(time::monotonic_ns()) as i64;
        }
        r
    }

    // ---- Submissions ------------------------------------------------------

    /// The device's engine a ring of i915's names (in a context without an
    /// engine map).
    fn ring_engine(&self, flags: u64) -> Result<u32, isize> {
        let (c, instance) = match flags & exec::RING_MASK {
            0 | exec::RENDER => (class::RENDER, 0),
            exec::BLT => (class::COPY, 0),
            exec::BSD => (class::VIDEO, ((flags & exec::BSD_MASK) >> exec::BSD_SHIFT).saturating_sub(1) as u16),
            exec::VEBOX => (class::VIDEO_ENHANCE, 0),
            _ => return Err(EINVAL),
        };
        let e = Engine { class: c, instance };
        self.device.engines.iter().position(|&d| d == e).map(|i| i as u32).ok_or(EINVAL)
    }

    fn execbuffer(&self, e: &Execbuffer2) -> Result<(), isize> {
        if e.flags & !exec::KNOWN != 0 || e.buffer_count == 0 || e.buffer_count > MAX_EXEC_BUFFERS {
            return Err(EINVAL);
        }
        // SAFETY: the program passed `buffer_count` objects.
        let mut objects = unsafe { read_array::<ExecObject2>(e.buffers, e.buffer_count as usize)? };
        let fences = if e.flags & exec::FENCE_ARRAY != 0 {
            // SAFETY: and `num_cliprects` fences.
            unsafe { read_array::<ExecFence>(e.cliprects, e.num_cliprects as usize)? }
        } else if e.num_cliprects != 0 {
            return Err(EINVAL);
        } else {
            Vec::new()
        };
        if e.flags & exec::BATCH_FIRST == 0 {
            // The batch is the last object; the driver takes it first.
            objects.rotate_right(1);
        }
        for o in &objects {
            let address = o.offset & ((1 << ADDRESS_BITS) - 1);
            // Placed by the program, with nothing to relocate.
            if o.relocation_count != 0
                || o.flags & !exec::OBJECT_KNOWN != 0
                || o.flags & exec::OBJECT_PINNED == 0
                || canonical(address) != o.offset
                || !address.is_multiple_of(PAGE)
            {
                return Err(EINVAL);
            }
        }
        let ctx_id = e.rsvd1 as u32;

        let mut s = self.state.lock();
        let engine_map = s.context(ctx_id)?.engines.clone();
        // The engine: by the context's index of it (an engine map), or by
        // the device's (rings, the driver's context has all engines).
        let engine = match &engine_map {
            Some(map) => {
                let i = e.flags & exec::RING_MASK;
                if i as usize >= map.len() {
                    return Err(EINVAL);
                }
                i as u32
            }
            None => self.ring_engine(e.flags)?,
        };
        let device_engine = match &engine_map {
            Some(map) => self.device.engines.iter().position(|&d| d == map[engine as usize]).ok_or(EINVAL)?,
            None => engine as usize,
        };

        // What must pass first: the fences waited for, and (i915's implicit
        // synchronisation) a buffer's last writer, and its readers if this
        // writes it, unless the program keeps track itself (`ASYNC`).
        let mut need = vec![0u64; self.device.engines.len()];
        let mut wait_for = |p: Point| {
            if let Some(n) = need.get_mut(p.engine as usize) {
                *n = (*n).max(p.seqno);
            }
        };
        let mut seen = BTreeSet::new();
        let mut buffers = Vec::with_capacity(objects.len());
        for o in &objects {
            if !seen.insert(o.handle) {
                return Err(EINVAL);
            }
            let b = s.buffers.get(&o.handle).ok_or(ENOENT)?;
            let write = o.flags & exec::OBJECT_WRITE != 0;
            if o.flags & exec::OBJECT_ASYNC == 0 {
                for p in self.uses(b, write) {
                    wait_for(p);
                }
            }
            buffers.push(ExecBuffer { handle: b.driver, address: o.offset & ((1 << ADDRESS_BITS) - 1), write });
        }
        let mut signals = Vec::new();
        for f in &fences {
            if f.flags & !(exec::FENCE_WAIT | exec::FENCE_SIGNAL) != 0 {
                return Err(EINVAL);
            }
            let fence = *s.syncobjs.get(&f.handle).ok_or(ENOENT)?;
            if f.flags & exec::FENCE_WAIT != 0 {
                match fence {
                    Fence::Unset => return Err(EINVAL),
                    Fence::Signaled => {}
                    Fence::At(p) => wait_for(p),
                }
            }
            if f.flags & exec::FENCE_SIGNAL != 0 {
                signals.push(f.handle);
            }
        }
        let waits = need
            .iter()
            .enumerate()
            .map(|(i, &n)| Point { engine: i as u32, seqno: n })
            .filter(|&p| p.seqno > 0 && !self.passed(p))
            .collect();

        let batch_start = e.batch_start;
        let batch_size = s.buffers.get(&objects[0].handle).map_or(0, |b| b.size);
        let batch_len = match e.batch_len {
            0 => u32::try_from(batch_size.saturating_sub(u64::from(batch_start))).map_err(|_| EINVAL)? & !7,
            n => n,
        };
        let context = s.driver_context(ctx_id, &self.device)?;
        let exec = Exec { context, engine, buffers, batch_start, batch_len, waits };
        let point = call(s.gem.execute(exec))?;
        if point.engine as usize != device_engine {
            return Err(EIO);
        }
        for o in &objects {
            if let Some(b) = s.buffers.get_mut(&o.handle) {
                b.reads[device_engine] = point.seqno;
                if o.flags & exec::OBJECT_WRITE != 0 {
                    b.write = Some(point);
                }
            }
        }
        for h in &signals {
            s.syncobjs.insert(*h, Fence::At(point));
        }
        drop(s);
        if !signals.is_empty() {
            // Threads waiting for these to be submitted.
            let _ = self.progress.signal();
        }
        Ok(())
    }

    // ---- Contexts and address spaces ------------------------------------

    fn context_create(&self, c: &mut ContextCreate) -> Result<(), isize> {
        const USE_EXTENSIONS: u32 = 1 << 0;
        const SINGLE_TIMELINE: u32 = 1 << 1;
        if c.flags & !(USE_EXTENSIONS | SINGLE_TIMELINE) != 0 {
            return Err(EINVAL);
        }
        let mut s = self.state.lock();
        let id = s.context_ids.take()?;
        // A new context has a new space, until it is given another.
        let space = s.new_space();
        s.contexts.insert(id, Context::new(space));
        let r = if c.flags & USE_EXTENSIONS != 0 { self.context_extensions(&mut s, id, c.extensions) } else { Ok(()) };
        if let Err(e) = r {
            drop(s);
            let _ = self.context_destroy(id);
            return Err(e);
        }
        c.ctx_id = id;
        Ok(())
    }

    /// Applies `CONTEXT_CREATE_EXT`'s extensions to context `id`.
    fn context_extensions(&self, s: &mut State, id: u32, mut next: u64) -> Result<(), isize> {
        for _ in 0..ext::MAX_CHAIN {
            if next == 0 {
                return Ok(());
            }
            // SAFETY: the program's chain of extensions.
            let e: SetparamExtension = unsafe { user::read(next as usize)? };
            if e.base.name != ext::CONTEXT_SETPARAM || e.param.ctx_id != 0 {
                return Err(EINVAL);
            }
            self.context_setparam(s, id, &e.param)?;
            next = e.base.next;
        }
        Err(E2BIG)
    }

    fn context_destroy(&self, id: u32) -> Result<(), isize> {
        if id == 0 {
            return Err(ENOENT);
        }
        let mut s = self.state.lock();
        let ctx = s.contexts.remove(&id).ok_or(ENOENT)?;
        s.context_ids.give(id);
        if let Some(d) = ctx.driver {
            let _ = s.gem.context_destroy(d);
        }
        s.release_space(ctx.space);
        Ok(())
    }

    fn context_setparam(&self, s: &mut State, id: u32, p: &ContextParam) -> Result<(), isize> {
        let made = s.context(id)?.driver.is_some();
        match p.param {
            ctx_param::PRIORITY => {
                let v = p.value as i64;
                if !(-1023..=1023).contains(&v) {
                    return Err(EINVAL);
                }
                s.context(id)?.priority = v;
            }
            ctx_param::RECOVERABLE => s.context(id)?.recoverable = p.value != 0,
            ctx_param::BANNABLE => s.context(id)?.bannable = p.value != 0,
            // Taken as given: nothing the driver does differs.
            ctx_param::NO_ERROR_CAPTURE | ctx_param::PERSISTENCE | ctx_param::RINGSIZE | ctx_param::BAN_PERIOD => {}
            ctx_param::PROTECTED_CONTENT if p.value == 0 => {}
            ctx_param::PROTECTED_CONTENT => return Err(ENODEV),
            // What the driver's context is made with: before it is.
            ctx_param::VM => {
                if made {
                    return Err(EINVAL);
                }
                let vm = u32::try_from(p.value).map_err(|_| ENOENT)?;
                let space = *s.vms.get(&vm).ok_or(ENOENT)?;
                s.hold_space(space);
                let old = core::mem::replace(&mut s.context(id)?.space, space);
                s.release_space(old);
            }
            ctx_param::ENGINES => {
                if made {
                    return Err(EINVAL);
                }
                let engines = if p.size == 0 { None } else { Some(self.engine_map(p.value, p.size)?) };
                s.context(id)?.engines = engines;
            }
            _ => return Err(EINVAL),
        }
        Ok(())
    }

    /// An engine map (`i915_context_param_engines`): no extensions, then
    /// the engines by class and instance.
    fn engine_map(&self, p: u64, size: u32) -> Result<Vec<Engine>, isize> {
        let size = size as usize;
        if size < 8 || !(size - 8).is_multiple_of(4) || (size - 8) / 4 > 64 {
            return Err(EINVAL);
        }
        // SAFETY: the program passed `size` bytes.
        let extensions: u64 = unsafe { user::read(p as usize)? };
        if extensions != 0 {
            return Err(EINVAL);
        }
        // SAFETY: as above.
        let raw = unsafe { read_array::<[u16; 2]>(p + 8, (size - 8) / 4)? };
        let engines: Vec<Engine> = raw.iter().map(|&[class, instance]| Engine { class, instance }).collect();
        if engines.is_empty() || engines.iter().any(|e| !self.device.engines.contains(e)) {
            return Err(EINVAL);
        }
        Ok(engines)
    }

    fn context_getparam(&self, p: &mut ContextParam) -> Result<(), isize> {
        let mut s = self.state.lock();
        let ctx = s.context(p.ctx_id)?;
        p.size = 0;
        p.value = match p.param {
            ctx_param::GTT_SIZE => self.device.vm_size,
            ctx_param::PRIORITY => ctx.priority as u64,
            ctx_param::RECOVERABLE => ctx.recoverable as u64,
            ctx_param::BANNABLE => ctx.bannable as u64,
            ctx_param::PERSISTENCE => 1,
            ctx_param::NO_ERROR_CAPTURE | ctx_param::PROTECTED_CONTENT => 0,
            // A new handle for the context's space.
            ctx_param::VM => {
                let space = ctx.space;
                u64::from(s.vm_handle(space)?)
            }
            _ => return Err(EINVAL),
        };
        Ok(())
    }

    // ---- Syncobjs ---------------------------------------------------------

    fn syncobj_create(&self, c: &mut SyncobjCreate) -> Result<(), isize> {
        if c.flags & !syncobj::CREATE_SIGNALED != 0 {
            return Err(EINVAL);
        }
        let mut s = self.state.lock();
        let h = s.syncobj_ids.take()?;
        let fence = if c.flags != 0 { Fence::Signaled } else { Fence::Unset };
        s.syncobjs.insert(h, fence);
        c.handle = h;
        Ok(())
    }

    fn syncobj_destroy(&self, c: &Handle) -> Result<(), isize> {
        if c.pad != 0 {
            return Err(EINVAL);
        }
        let mut s = self.state.lock();
        s.syncobjs.remove(&c.handle).ok_or(EINVAL)?;
        s.syncobj_ids.give(c.handle);
        Ok(())
    }

    fn syncobj_set(&self, handles: &[u32], fence: Fence) -> Result<(), isize> {
        let mut s = self.state.lock();
        if handles.iter().any(|h| !s.syncobjs.contains_key(h)) {
            return Err(ENOENT);
        }
        for h in handles {
            s.syncobjs.insert(*h, fence);
        }
        Ok(())
    }

    fn syncobj_transfer(&self, t: &SyncobjTransfer) -> Result<(), isize> {
        // Between binary syncobjs alone.
        if t.src_point != 0 || t.dst_point != 0 || t.flags != 0 || t.pad != 0 {
            return Err(EINVAL);
        }
        let mut s = self.state.lock();
        let fence = *s.syncobjs.get(&t.src_handle).ok_or(ENOENT)?;
        if fence == Fence::Unset {
            return Err(EINVAL);
        }
        let dst = s.syncobjs.get_mut(&t.dst_handle).ok_or(ENOENT)?;
        *dst = fence;
        Ok(())
    }

    /// Waits for the syncobjs `handles` (all, or with `WAIT_ALL` unset,
    /// one), until `timeout` (absolute, monotonic ns: `ETIME` then). Those
    /// without a fence are an error, or with `WAIT_FOR_SUBMIT` are waited
    /// for to get one. Returns the index of one signaled.
    fn syncobj_wait(&self, handles: &[u32], timeout: i64, flags: u32) -> Result<u32, isize> {
        use syncobj::*;
        if flags & !(WAIT_ALL | WAIT_FOR_SUBMIT | WAIT_AVAILABLE | WAIT_DEADLINE) != 0 || handles.is_empty() {
            return Err(EINVAL);
        }
        // The fences waited for, as they are now (as Linux takes them); a
        // syncobj without one is looked at again until it has one.
        let mut fences: Vec<Fence> = {
            let s = self.state.lock();
            handles.iter().map(|h| s.syncobjs.get(h).copied().ok_or(ENOENT)).collect::<Result<_, _>>()?
        };
        if flags & WAIT_FOR_SUBMIT == 0 && fences.contains(&Fence::Unset) {
            return Err(EINVAL);
        }
        let deadline = u64::try_from(timeout).unwrap_or(0);
        let mut first = 0;
        self.wait(deadline, || {
            if fences.contains(&Fence::Unset) {
                let s = self.state.lock();
                for (f, h) in fences.iter_mut().zip(handles) {
                    if *f == Fence::Unset {
                        // A syncobj destroyed meanwhile never gets one.
                        *f = s.syncobjs.get(h).copied().unwrap_or(Fence::Unset);
                    }
                }
            }
            let done = |f: &Fence| match *f {
                Fence::Unset => false,
                Fence::Signaled => true,
                Fence::At(p) => flags & WAIT_AVAILABLE != 0 || self.passed(p),
            };
            let signaled = fences.iter().position(done);
            if let Some(i) = signaled {
                first = i as u32;
            }
            Ok(if flags & WAIT_ALL != 0 { fences.iter().all(done) } else { signaled.is_some() })
        })?;
        Ok(first)
    }

    // ---- Waiting ----------------------------------------------------------

    /// The last number the device's engine `e` completed.
    fn completed(&self, e: u32) -> u64 {
        if e as usize >= self.device.engines.len() {
            return u64::MAX;
        }
        // SAFETY: the fence page: a u64 per engine, written by the driver.
        unsafe { core::ptr::read_volatile((self.fences.as_ptr() as *const u64).add(e as usize)) }
    }

    fn passed(&self, p: Point) -> bool {
        self.completed(p.engine) >= p.seqno
    }

    /// Waits until `done` holds (it is asked again whenever the engines'
    /// numbers move or a syncobj gets a submission) or `deadline`
    /// (monotonic ns; 0 looks once) passes: `ETIME` then, `EIO` if the
    /// driver went away.
    ///
    /// The session's event is signaled by every change. A thread that
    /// waits alone clears it before it looks; with others waiting it is
    /// left for them, and a thread that finds it set but nothing done
    /// sleeps a moment rather than spin.
    fn wait(&self, deadline: u64, mut done: impl FnMut() -> Result<bool, isize>) -> Result<(), isize> {
        if done()? {
            return Ok(());
        }
        struct Waiting<'a>(&'a Mutex<u32>);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                *self.0.lock() -= 1;
            }
        }
        *self.waiters.lock() += 1;
        let _waiting = Waiting(&self.waiters);
        loop {
            if time::monotonic_ns() >= deadline {
                return if done()? { Ok(()) } else { Err(ETIME) };
            }
            let alone = {
                let w = self.waiters.lock();
                if *w == 1 {
                    let _ = self.progress.clear();
                }
                *w == 1
            };
            if done()? {
                return Ok(());
            }
            if !alone && self.progress.0.wait(signals::SIGNALED, 0).is_ok() {
                vrt::time::sleep_until(deadline.min(time::monotonic_ns() + 1_000_000));
            } else {
                let mut items = [
                    WaitItem { handle: self.progress.0.raw(), signals: signals::SIGNALED, observed: 0, _reserved: 0 },
                    WaitItem { handle: self.channel, signals: signals::PEER_CLOSED, observed: 0, _reserved: 0 },
                ];
                match vrt::object::wait_many(&mut items, deadline) {
                    Ok(_) | Err(vabi::Error::TimedOut) => {}
                    Err(e) => return Err(crate::error::kernel(e)),
                }
                if items[1].observed & signals::PEER_CLOSED != 0 {
                    return Err(EIO);
                }
            }
            if done()? {
                return Ok(());
            }
        }
    }
}

// ---- What the GPU is -------------------------------------------------------

/// `QUERY_TOPOLOGY_INFO`: the slices, the subslices of each, the units of
/// each subslice.
fn topology(d: &GemDevice) -> Vec<u8> {
    let max_slices = (32 - d.slice_mask.leading_zeros()).max(1) as usize;
    let max_subslices = d.subslice_masks.iter().map(|m| 32 - m.leading_zeros()).max().unwrap_or(0).max(1) as usize;
    let eus = (32 - d.eu_mask.leading_zeros()).max(1) as usize;
    let subslice_offset = max_slices.div_ceil(8);
    let subslice_stride = max_subslices.div_ceil(8);
    let eu_offset = subslice_offset + max_slices * subslice_stride;
    let eu_stride = eus.div_ceil(8);
    let mut data = vec![0u8; eu_offset + max_slices * max_subslices * eu_stride];
    for s in 0..max_slices {
        if d.slice_mask & (1 << s) == 0 {
            continue;
        }
        data[s / 8] |= 1 << (s % 8);
        let mask = d.subslice_masks.get(s).copied().unwrap_or(0);
        for ss in 0..max_subslices {
            if mask & (1 << ss) == 0 {
                continue;
            }
            data[subslice_offset + s * subslice_stride + ss / 8] |= 1 << (ss % 8);
            for eu in (0..eus).filter(|&eu| d.eu_mask & (1 << eu) != 0) {
                data[eu_offset + (s * max_subslices + ss) * eu_stride + eu / 8] |= 1 << (eu % 8);
            }
        }
    }
    let mut out = Vec::with_capacity(16 + data.len());
    for v in [0, max_slices, max_subslices, eus, subslice_offset, subslice_stride, eu_offset, eu_stride] {
        out.extend_from_slice(&(v as u16).to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// `QUERY_ENGINE_INFO`: the engines (each 56 bytes: class, instance,
/// flags, capabilities, logical instance).
fn engine_info(d: &GemDevice) -> Vec<u8> {
    let mut out = vec![0u8; 16 + 56 * d.engines.len()];
    out[..4].copy_from_slice(&(d.engines.len() as u32).to_le_bytes());
    for (i, e) in d.engines.iter().enumerate() {
        let at = 16 + 56 * i;
        out[at..at + 2].copy_from_slice(&e.class.to_le_bytes());
        out[at + 2..at + 4].copy_from_slice(&e.instance.to_le_bytes());
        // Its logical instance (the flag says it has one): its physical.
        out[at + 8] = 1;
        out[at + 24..at + 26].copy_from_slice(&e.instance.to_le_bytes());
    }
    out
}

/// `QUERY_MEMORY_REGIONS`: system memory alone (88 bytes a region).
fn memory_regions(d: &GemDevice) -> Vec<u8> {
    let mut out = vec![0u8; 16 + 88];
    out[..4].copy_from_slice(&1u32.to_le_bytes());
    let r = 16;
    // Class 0 (system), instance 0; probed, unallocated, and the same for
    // what the processor sees.
    for at in [r + 8, r + 16, r + 24, r + 32] {
        out[at..at + 8].copy_from_slice(&d.memory.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adl() -> GemDevice {
        GemDevice {
            vendor: 0x8086,
            device: 0x46A6,
            subvendor: 0,
            subdevice: 0,
            revision: 0x0C,
            domain: 0,
            bus: 0,
            dev: 2,
            func: 0,
            slice_mask: 1,
            subslice_masks: vec![0x3F],
            eu_mask: 0xFFFF,
            timestamp_hz: 19_200_000,
            vm_size: 1 << 48,
            memory: 4 << 30,
            engines: vec![Engine { class: class::RENDER, instance: 0 }, Engine { class: class::COPY, instance: 0 }],
            name: "test".into(),
        }
    }

    #[test]
    fn requests() {
        // DRM_IOCTL_I915_GEM_EXECBUFFER2: _IOW('d', 0x69, 64 bytes).
        assert_eq!(Ioc::decode(0x4040_6469), Some(Ioc { nr: nr::GEM_EXECBUFFER2, size: 64, write: true, read: false }));
        // DRM_IOCTL_I915_GETPARAM: _IOWR('d', 0x46, 16 bytes).
        assert_eq!(Ioc::decode(0xC010_6446), Some(Ioc { nr: nr::GETPARAM, size: 16, write: true, read: true }));
        // DRM_IOCTL_VERSION: _IOWR('d', 0, 64 bytes).
        assert_eq!(Ioc::decode(0xC040_6400).map(|i| (i.nr, i.size)), Some((nr::VERSION, 64)));
        // A terminal's.
        assert_eq!(Ioc::decode(0x5401), None);
    }

    #[test]
    fn argument_sizes() {
        // As Linux's structs.
        assert_eq!(size_of::<Version>(), 64);
        assert_eq!(size_of::<SyncobjWait>(), 40);
        assert_eq!(size_of::<SyncobjTimelineWait>(), 48);
        assert_eq!(size_of::<SyncobjTransfer>(), 32);
        assert_eq!(size_of::<PciInfo>(), 16);
        assert_eq!(size_of::<Getparam>(), 16);
        assert_eq!(size_of::<UserExtension>(), 32);
        assert_eq!(size_of::<MemoryRegions>(), 48);
        assert_eq!(size_of::<GemMmap>(), 40);
        assert_eq!(size_of::<GemMmapOffset>(), 32);
        assert_eq!(size_of::<Execbuffer2>(), 64);
        assert_eq!(size_of::<ExecObject2>(), 56);
        assert_eq!(size_of::<ContextCreate>(), 16);
        assert_eq!(size_of::<ContextParam>(), 24);
        assert_eq!(size_of::<SetparamExtension>(), 56);
        assert_eq!(size_of::<ResetStats>(), 24);
        assert_eq!(size_of::<QueryItem>(), 24);
    }

    #[test]
    fn canonical_addresses() {
        assert_eq!(canonical(0x1000), 0x1000);
        assert_eq!(canonical(0x7FFF_FFFF_F000), 0x7FFF_FFFF_F000);
        assert_eq!(canonical(0x8000_0000_0000), 0xFFFF_8000_0000_0000);
        assert_eq!(canonical(0xFFFF_FFFF_F000), 0xFFFF_FFFF_FFFF_F000);
    }

    #[test]
    fn handle_numbers() {
        let mut ids = Ids::default();
        assert_eq!((ids.take(), ids.take(), ids.take()), (Ok(1), Ok(2), Ok(3)));
        ids.give(2);
        assert_eq!(ids.take(), Ok(2));
        ids.give(3);
        ids.give(2);
        // Back to one in use.
        assert_eq!(ids.top, 1);
        assert!(ids.free.is_empty());
        assert_eq!(ids.take(), Ok(2));
    }

    #[test]
    fn topology_of_alder_lake() {
        let t = topology(&adl());
        let word = |i: usize| u16::from_le_bytes([t[2 * i], t[2 * i + 1]]);
        // One slice of six subslices of sixteen units.
        assert_eq!((word(1), word(2), word(3)), (1, 6, 16));
        // Subslices at 1, a byte each; units at 2, two bytes each.
        assert_eq!((word(4), word(5), word(6), word(7)), (1, 1, 2, 2));
        let data = &t[16..];
        assert_eq!(data.len(), 2 + 6 * 2);
        assert_eq!(&data[..2], &[1, 0x3F]);
        assert!(data[2..].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn engines_and_memory() {
        let e = engine_info(&adl());
        assert_eq!(e.len(), 16 + 2 * 56);
        assert_eq!(e[0], 2);
        // The copy engine: class 1, instance 0.
        assert_eq!(&e[16 + 56..16 + 56 + 4], &[1, 0, 0, 0]);
        let m = memory_regions(&adl());
        assert_eq!(m.len(), 104);
        assert_eq!(u64::from_le_bytes(m[24..32].try_into().unwrap()), 4 << 30);
    }
}
