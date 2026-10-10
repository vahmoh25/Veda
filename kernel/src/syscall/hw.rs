//! Hardware access for user-space drivers: resources, I/O ports and
//! interrupts.

use vabi::{Error, MsiInfo, RawHandle, Rights, irq_flags, resource_kind};

use super::{SysResult, get_interrupt, get_ioports, get_resource, insert, ok};
use crate::mm::user;
use crate::object::KObject;
use crate::object::interrupt::Interrupt;
use crate::object::ioport::IoPorts;
use crate::object::resource::Resource;

pub fn resource_create(parent: RawHandle, kind: usize, base: u64, size: u64) -> SysResult {
    let p = get_resource(parent, Rights::DUPLICATE)?;
    if kind > resource_kind::LAST || !p.permits(kind, base, size) {
        return Err(Error::AccessDenied);
    }
    let rights = Rights(Rights::BASIC.0 | Rights::DUPLICATE.0);
    ok(insert(KObject::Resource(Resource::new(kind, base, size)), rights)? as usize)
}

pub fn ioport_create(res: RawHandle, base: usize, count: usize) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    if count == 0 || base + count > 0x10000 || !r.permits(resource_kind::IOPORT, base as u64, count as u64) {
        return Err(Error::AccessDenied);
    }
    let rights = Rights(Rights::BASIC.0 | Rights::READ.0 | Rights::WRITE.0);
    ok(insert(KObject::IoPorts(IoPorts::new(base as u16, count as u16)), rights)? as usize)
}

pub fn ioport_read(raw: RawHandle, port: usize, width: usize) -> SysResult {
    let io = get_ioports(raw, Rights::READ)?;
    ok(io.read(port as u16, width)? as usize)
}

pub fn ioport_write(raw: RawHandle, port: usize, width: usize, value: usize) -> SysResult {
    let io = get_ioports(raw, Rights::WRITE)?;
    io.write(port as u16, width, value as u32)?;
    ok(0)
}

pub fn irq_create(res: RawHandle, irq: usize, flags: usize) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    let (gsi, level, active_low) = if flags & irq_flags::ISA != 0 {
        if irq > 15 {
            return Err(Error::InvalidArgs);
        }
        crate::acpi::isa_irq_to_gsi(irq as u8)
    } else {
        (irq as u32, flags & irq_flags::LEVEL != 0, flags & irq_flags::ACTIVE_LOW != 0)
    };
    if !r.permits(resource_kind::IRQ, gsi as u64, 1) {
        return Err(Error::AccessDenied);
    }
    let i = Interrupt::new_gsi(gsi, level, active_low).ok_or(Error::AlreadyExists)?;
    let rights = Rights(Rights::BASIC.0 | Rights::WRITE.0);
    ok(insert(KObject::Interrupt(i), rights)? as usize)
}

pub fn irq_ack(raw: RawHandle) -> SysResult {
    get_interrupt(raw, Rights::WRITE)?.ack();
    ok(0)
}

pub fn msi_create(res: RawHandle, device: usize, out: usize) -> SysResult {
    let r = get_resource(res, Rights::NONE)?;
    let device = u16::try_from(device).map_err(|_| Error::InvalidArgs)?;
    if !r.permits(resource_kind::PCI, device as u64, 1) {
        return Err(Error::AccessDenied);
    }
    let (i, (address, data)) = Interrupt::new_msi(device).ok_or(Error::LimitReached)?;
    let info = MsiInfo { address, data, vector: i.vector as u32 };
    let rights = Rights(Rights::BASIC.0 | Rights::WRITE.0);
    let h = insert(KObject::Interrupt(i), rights)?;
    user::write(out as u64, &info)?;
    ok(h as usize)
}
