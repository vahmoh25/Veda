//! Virtual machines: guests and their virtual processors.

use vabi::{Error, RawHandle, Rights, VcpuExit, VcpuState, map_flags, resource_kind};

use super::{SysResult, get_interrupt, get_resource, get_vmo, handle, insert, ok};
use crate::hv::ept::Access;
use crate::hv::{Guest, Vcpu};
use crate::mm::user;
use crate::object::KObject;

fn get_guest(raw: RawHandle, rights: Rights) -> Result<alloc::sync::Arc<Guest>, Error> {
    match handle(raw, rights)?.object {
        KObject::Guest(g) => Ok(g),
        _ => Err(Error::WrongType),
    }
}

fn get_vcpu(raw: RawHandle, rights: Rights) -> Result<alloc::sync::Arc<Vcpu>, Error> {
    match handle(raw, rights)?.object {
        KObject::Vcpu(v) => Ok(v),
        _ => Err(Error::WrongType),
    }
}

pub fn guest_create(res: RawHandle, cpus: usize) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    if !r.permits(resource_kind::HYPERVISOR, 0, 0) {
        return Err(Error::AccessDenied);
    }
    crate::hv::vmx::caps().map_err(|_| Error::NotSupported)?;
    let guest = Guest::new(u32::try_from(cpus).map_err(|_| Error::InvalidArgs)?)?;
    let rights = Rights(Rights::BASIC.0 | Rights::WRITE.0 | Rights::MANAGE.0);
    ok(insert(KObject::Guest(guest), rights)? as usize)
}

pub fn guest_map(guest: RawHandle, vmo: RawHandle, offset: usize, len: usize, gpa: usize, flags: usize) -> SysResult {
    let g = get_guest(guest, Rights::WRITE)?;
    if flags & !(map_flags::READ | map_flags::WRITE | map_flags::EXECUTE) != 0 || flags & map_flags::READ == 0 {
        return Err(Error::InvalidArgs);
    }
    let access = Access { read: true, write: flags & map_flags::WRITE != 0, execute: flags & map_flags::EXECUTE != 0 };
    // The guest may do with the memory only what the handle allows.
    let mut needed = Rights(Rights::MAP.0 | Rights::READ.0);
    if access.write {
        needed = needed | Rights::WRITE;
    }
    if access.execute {
        needed = needed | Rights::EXECUTE;
    }
    let vmo = get_vmo(vmo, needed)?;
    g.map(vmo, offset as u64, len as u64, gpa as u64, access)?;
    ok(0)
}

pub fn guest_unmap(guest: RawHandle, gpa: usize, len: usize) -> SysResult {
    get_guest(guest, Rights::WRITE)?.unmap(gpa as u64, len as u64)?;
    ok(0)
}

pub fn vcpu_create(guest: RawHandle, id: usize, state: usize) -> SysResult {
    let g = get_guest(guest, Rights::WRITE)?;
    let state: VcpuState = user::read(state as u64)?;
    let id = u32::try_from(id).map_err(|_| Error::InvalidArgs)?;
    let vcpu = Vcpu::new(g, id, state)?;
    let rights = Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::SIGNAL.0);
    ok(insert(KObject::Vcpu(vcpu), rights)? as usize)
}

pub fn vcpu_run(vcpu: RawHandle, exit: usize) -> SysResult {
    let v = get_vcpu(vcpu, Rights::WRITE)?;
    let mut e: VcpuExit = user::read(exit as u64)?;
    v.run(&mut e)?;
    user::write(exit as u64, &e)?;
    ok(0)
}

pub fn vcpu_interrupt(vcpu: RawHandle, vector: usize) -> SysResult {
    let v = get_vcpu(vcpu, Rights::SIGNAL)?;
    v.interrupt(u8::try_from(vector).map_err(|_| Error::InvalidArgs)?);
    ok(0)
}

pub fn vcpu_read_state(vcpu: RawHandle, out: usize) -> SysResult {
    let v = get_vcpu(vcpu, Rights::READ)?;
    let state = v.read_state()?;
    user::write(out as u64, &state)?;
    ok(0)
}

pub fn guest_attach_device(guest: RawHandle, res: RawHandle, device: usize) -> SysResult {
    let g = get_guest(guest, Rights::MANAGE)?;
    let r = get_resource(res, Rights::NONE)?;
    let device = u16::try_from(device).map_err(|_| Error::InvalidArgs)?;
    if !r.permits(resource_kind::PCI, device as u64, 1) {
        return Err(Error::AccessDenied);
    }
    g.attach_device(device)?;
    ok(0)
}

pub fn vcpu_bind_interrupt(vcpu: RawHandle, interrupt: RawHandle, vector: usize) -> SysResult {
    let v = get_vcpu(vcpu, Rights::SIGNAL)?;
    let irq = get_interrupt(interrupt, Rights::WRITE)?;
    let vector = u8::try_from(vector).ok().filter(|&v| v >= 32).ok_or(Error::InvalidArgs)?;
    if !irq.bind(&v, vector) {
        return Err(Error::NotSupported);
    }
    ok(0)
}
