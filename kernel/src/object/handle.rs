//! Per-process handle tables.
//!
//! A handle value encodes a slot index and a generation counter, so a stale
//! handle value (closed and then reused slot) is detected instead of silently
//! referring to an unrelated object.

use alloc::vec::Vec;

use vabi::{Error, RawHandle, Rights};

use super::KObject;

/// Maximum handles per process.
pub const MAX_HANDLES: usize = 1 << 16;

/// An object reference plus the rights granted through it.
#[derive(Clone)]
pub struct Handle {
    pub object: KObject,
    pub rights: Rights,
}

struct Slot {
    generation: u16,
    entry: Option<Handle>,
}

#[derive(Default)]
pub struct HandleTable {
    slots: Vec<Slot>,
    free: Vec<u32>,
    count: usize,
}

fn encode(index: u32, generation: u16) -> RawHandle {
    ((generation as u32 & 0x7FF) << 20) | (index + 1)
}

fn decode(raw: RawHandle) -> Option<(u32, u16)> {
    let idx = raw & 0xF_FFFF;
    if idx == 0 || raw & 0x8000_0000 != 0 {
        return None;
    }
    Some((idx - 1, ((raw >> 20) & 0x7FF) as u16))
}

impl HandleTable {
    pub fn new() -> Self {
        HandleTable::default()
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn insert(&mut self, h: Handle) -> Result<RawHandle, Error> {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                if self.slots.len() >= MAX_HANDLES {
                    return Err(Error::LimitReached);
                }
                self.slots.push(Slot { generation: 0, entry: None });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.entry = Some(h);
        self.count += 1;
        Ok(encode(index, slot.generation))
    }

    fn slot(&self, raw: RawHandle) -> Result<&Slot, Error> {
        let (idx, generation) = decode(raw).ok_or(Error::BadHandle)?;
        match self.slots.get(idx as usize) {
            Some(s) if s.entry.is_some() && s.generation & 0x7FF == generation => Ok(s),
            _ => Err(Error::BadHandle),
        }
    }

    pub fn get(&self, raw: RawHandle) -> Result<&Handle, Error> {
        Ok(self.slot(raw)?.entry.as_ref().unwrap())
    }

    pub fn remove(&mut self, raw: RawHandle) -> Result<Handle, Error> {
        self.slot(raw)?;
        let idx = decode(raw).unwrap().0;
        let slot = &mut self.slots[idx as usize];
        let h = slot.entry.take().unwrap();
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(idx);
        self.count -= 1;
        Ok(h)
    }

    /// Removes every handle (process teardown). The caller drops the result
    /// after releasing its locks, because dropping objects can have effects
    /// (e.g. waking channel peers).
    pub fn take_all(&mut self) -> Vec<Handle> {
        let out: Vec<Handle> = self.slots.iter_mut().filter_map(|s| s.entry.take()).collect();
        self.slots.clear();
        self.free.clear();
        self.count = 0;
        out
    }
}
