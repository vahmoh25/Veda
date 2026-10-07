//! Memory objects and address-space management.

use vabi::{Error, RawHandle, Rights, cache_policy, dma_flags, map_flags, resource_kind, vmo_flags};

use super::{SysResult, get_resource, get_vmo, handle, insert, ok, target_process};
use crate::mm::aspace::Perms;
use crate::mm::paging::Cache;
use crate::mm::user;
use crate::mm::vmo::Vmo;
use crate::object::KObject;

const VMO_RIGHTS: Rights =
    Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::EXECUTE.0 | Rights::GET_INFO.0);

pub fn vmo_create(size: usize, flags: usize) -> SysResult {
    let vmo = Vmo::new_anonymous(size as u64).ok_or(Error::InvalidArgs)?;
    if flags & vmo_flags::COMMIT != 0 {
        for off in (0..vmo.size()).step_by(4096) {
            vmo.page(off, true).ok_or(Error::NoMemory)?;
        }
    }
    ok(insert(KObject::Vmo(vmo), VMO_RIGHTS)? as usize)
}

pub fn vmo_read(raw: RawHandle, offset: usize, buf: usize, len: usize) -> SysResult {
    let vmo = get_vmo(raw, Rights::READ)?;
    let mut tmp = user::buffer(len.min(256 * 1024))?;
    let mut done = 0;
    while done < len {
        let n = (len - done).min(tmp.len());
        if !vmo.read((offset + done) as u64, &mut tmp[..n]) {
            return Err(Error::OutOfRange);
        }
        user::copy_to_user((buf + done) as u64, &tmp[..n])?;
        done += n;
    }
    ok(len)
}

pub fn vmo_write(raw: RawHandle, offset: usize, buf: usize, len: usize) -> SysResult {
    let vmo = get_vmo(raw, Rights::WRITE)?;
    let mut tmp = user::buffer(len.min(256 * 1024))?;
    let mut done = 0;
    while done < len {
        let n = (len - done).min(tmp.len());
        user::copy_from_user(&mut tmp[..n], (buf + done) as u64)?;
        if !vmo.write((offset + done) as u64, &tmp[..n]) {
            return Err(Error::OutOfRange);
        }
        done += n;
    }
    ok(len)
}

pub fn vmo_get_size(raw: RawHandle) -> SysResult {
    ok(get_vmo(raw, Rights::NONE)?.size() as usize)
}

pub fn vmo_create_physical(res: RawHandle, paddr: usize, size: usize, cache: usize) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    if size == 0 || !r.permits(resource_kind::MMIO, paddr as u64, size as u64) {
        return Err(Error::AccessDenied);
    }
    let cache = match cache {
        cache_policy::WRITE_BACK => Cache::WriteBack,
        cache_policy::WRITE_COMBINING => Cache::WriteCombining,
        cache_policy::UNCACHED => Cache::Uncached,
        _ => return Err(Error::InvalidArgs),
    };
    // The pages the range touches (the root resource permits any range).
    let span = (size as u64)
        .checked_add(paddr as u64 & 0xFFF)
        .and_then(crate::mm::checked_page_align_up)
        .filter(|&span| span <= crate::mm::vmo::MAX_VMO_SIZE)
        .ok_or(Error::InvalidArgs)?;
    let vmo = Vmo::new_physical(paddr as u64, span, cache);
    let rights = Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::GET_INFO.0);
    ok(insert(KObject::Vmo(vmo), rights)? as usize)
}

pub fn vmo_create_contiguous(res: RawHandle, size: usize, flags: usize) -> SysResult {
    if flags & !dma_flags::BELOW_4G != 0 {
        return Err(Error::InvalidArgs);
    }
    let r = get_resource(res, Rights::NONE)?;
    if !r.permits(resource_kind::DMA, 0, 0) && !r.permits(resource_kind::DMA, r.base, 0) {
        return Err(Error::AccessDenied);
    }
    let limit = if flags & dma_flags::BELOW_4G != 0 { 1 << 32 } else { u64::MAX };
    let vmo = Vmo::new_contiguous(size as u64, limit).ok_or(Error::NoMemory)?;
    ok(insert(KObject::Vmo(vmo), VMO_RIGHTS)? as usize)
}

pub fn vmo_phys_addr(raw: RawHandle, offset: usize) -> SysResult {
    let vmo = get_vmo(raw, Rights::NONE)?;
    ok(vmo.phys_addr(offset as u64).ok_or(Error::NotSupported)? as usize)
}

pub fn vm_map(proc: RawHandle, vmo: RawHandle, offset: usize, len: usize, addr: usize, flags: usize) -> SysResult {
    let p = target_process(proc)?;
    let perms = Perms::from_map_flags(flags);
    // x86 has no write-only or execute-only pages: a page that can be
    // accessed at all can be read, so any access takes the READ right.
    let mut needed = Rights::MAP;
    if !perms.none() {
        needed = needed | Rights::READ;
    }
    if perms.write {
        needed = needed | Rights::WRITE;
    }
    if perms.exec {
        needed = needed | Rights::EXECUTE;
    }
    let h = handle(vmo, needed)?;
    let KObject::Vmo(vmo) = h.object else { return Err(Error::WrongType) };
    // What vm_protect may later grant: no more than the handle allows.
    let readable = h.rights.contains(Rights::READ);
    let max = Perms {
        read: readable,
        write: readable && h.rights.contains(Rights::WRITE),
        exec: readable && h.rights.contains(Rights::EXECUTE),
    };
    let aspace = p.aspace().ok_or(Error::BadState)?;
    let at = aspace.map(
        vmo,
        offset as u64,
        len as u64,
        addr as u64,
        flags & map_flags::FIXED != 0,
        perms,
        max,
        flags & map_flags::COMMIT != 0,
    )?;
    ok(at as usize)
}

pub fn vm_allocate(proc: RawHandle, len: usize, addr: usize, flags: usize) -> SysResult {
    let p = target_process(proc)?;
    let aspace = p.aspace().ok_or(Error::BadState)?;
    let at = aspace.allocate(
        len as u64,
        addr as u64,
        flags & map_flags::FIXED != 0,
        Perms::from_map_flags(flags),
        flags & map_flags::COMMIT != 0,
    )?;
    ok(at as usize)
}

pub fn vm_decommit(proc: RawHandle, addr: usize, len: usize) -> SysResult {
    let p = target_process(proc)?;
    p.aspace().ok_or(Error::BadState)?.decommit(addr as u64, len as u64)?;
    ok(0)
}

pub fn vm_unmap(proc: RawHandle, addr: usize, len: usize) -> SysResult {
    let p = target_process(proc)?;
    p.aspace().ok_or(Error::BadState)?.unmap(addr as u64, len as u64)?;
    ok(0)
}

pub fn vm_protect(proc: RawHandle, addr: usize, len: usize, flags: usize) -> SysResult {
    let p = target_process(proc)?;
    p.aspace().ok_or(Error::BadState)?.protect(addr as u64, len as u64, Perms::from_map_flags(flags))?;
    ok(0)
}
