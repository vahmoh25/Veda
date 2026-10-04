//! Persistence of the user's home directory on the home disk.
//!
//! The disk with serial number [`HOME_DISK_SERIAL`] holds snapshots of
//! `/home` in two slots, each a header sector followed by the encoded tree.
//! Saves alternate between the slots and loading picks the newest slot with
//! a valid checksum, so a crash or power loss in the middle of a save loses
//! at most that save. Files that still have their original sample contents
//! are stored as references into the system image rather than copies.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vipc::Bytes;
use vproto::block::{self as blk, block};
use vproto::fs::FsError;
use vrt::println;

use crate::tree::{self, Data, Kind, NodeId, Tree};

/// Serial number of the disk that holds the home directory.
pub const HOME_DISK_SERIAL: &str = "veda-home";
const MAGIC: [u8; 8] = *b"VHOMEFS1";
const SECTOR: usize = 512;
/// First sector of slot 0 (the start of the disk is left alone).
const SLOT0: u64 = 8;
/// How long to wait for the home disk's driver at boot.
const ATTACH_TIMEOUT_NS: u64 = 3_000_000_000;

// Snapshot records.
const R_DIR: u8 = 1;
const R_FILE: u8 = 2;
/// A file whose contents are a sample in the system image.
const R_SAMPLE_FILE: u8 = 3;
/// A sample that existed when the snapshot was taken (so that samples the
/// user deleted stay deleted, while samples added to the system image later
/// still appear).
const R_KNOWN_SAMPLE: u8 = 4;

/// CRC-32 (IEEE 802.3).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// The sample files in the system image (`samples/...`), which seed the
/// user's home directory.
pub struct Samples {
    /// Address of a sample's contents → its path in the system image.
    by_ptr: BTreeMap<usize, &'static str>,
    /// Path in the system image → contents.
    by_path: BTreeMap<&'static str, &'static [u8]>,
}

impl Samples {
    pub fn new(archive: &initrd::Archive<'static>) -> Samples {
        let mut by_ptr = BTreeMap::new();
        let mut by_path = BTreeMap::new();
        for f in archive.files().filter(|f| f.path.starts_with("samples/")) {
            if !f.data.is_empty() {
                by_ptr.insert(f.data.as_ptr() as usize, f.path);
            }
            by_path.insert(f.path, f.data);
        }
        Samples { by_ptr, by_path }
    }

    /// The sample `d` still is (an unmodified copy, which snapshots store as
    /// a reference): its path in the system image.
    pub fn source_of(&self, d: &Data) -> Option<&'static str> {
        match d {
            Data::Static(s) => self.by_ptr.get(&(s.as_ptr() as usize)).copied(),
            Data::Owned(_) => None,
        }
    }

    /// Where a sample goes in the home directory.
    fn home_path(src: &str) -> Option<String> {
        src.strip_prefix("samples/").map(|rest| format!("home/user/{rest}"))
    }

    /// Copies samples into the home directory: all of them, or those not in
    /// `known`. Existing files are never replaced.
    pub fn seed(&self, ram: &mut Tree, known: Option<&BTreeSet<String>>) {
        for (&src, &data) in &self.by_path {
            if known.is_some_and(|k| k.contains(src)) {
                continue;
            }
            if let Some(path) = Samples::home_path(src) {
                let comps = tree::components(&path).unwrap_or_default();
                let _ = ram.add_file(&comps, Data::Static(data));
            }
        }
    }
}

/// Where snapshot records go: [`Writer`] builds the encoding, [`Counter`]
/// only measures it, so the two can never disagree.
trait Sink {
    fn bytes(&mut self, b: &[u8]);
    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
    }
}

/// Little-endian encoder for snapshots.
struct Writer(Vec<u8>);

impl Sink for Writer {
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
}

/// Counts the bytes of a snapshot without building it.
struct Counter(usize);

impl Sink for Counter {
    fn bytes(&mut self, b: &[u8]) {
        self.0 += b.len();
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).ok()
    }
}

/// Encodes `/home` of the RAM file system.
pub fn encode(ram: &Tree, samples: &Samples) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    snapshot(ram, samples, &mut w);
    w.0
}

/// The size of [`encode`]'s result, computed without copying any data.
pub fn encoded_len(ram: &Tree, samples: &Samples) -> usize {
    let mut c = Counter(0);
    snapshot(ram, samples, &mut c);
    c.0
}

fn snapshot(ram: &Tree, samples: &Samples, out: &mut impl Sink) {
    for src in samples.by_path.keys() {
        out.u8(R_KNOWN_SAMPLE);
        out.str(src);
    }
    if let Ok(home) = ram.lookup(&["home"]) {
        walk(ram, home, "home", samples, out);
    }
}

/// Writes the records of a subtree, parents before children.
fn walk(t: &Tree, id: NodeId, path: &str, samples: &Samples, out: &mut impl Sink) {
    let Some(node) = t.node(id) else { return };
    match &node.kind {
        Kind::Dir(children) => {
            out.u8(R_DIR);
            out.str(path);
            out.u64(node.modified);
            for (name, &child) in children {
                walk(t, child, &format!("{path}/{name}"), samples, out);
            }
        }
        Kind::File(d) => match samples.source_of(d) {
            Some(src) => {
                out.u8(R_SAMPLE_FILE);
                out.str(path);
                out.u64(node.modified);
                out.str(src);
            }
            None => {
                out.u8(R_FILE);
                out.str(path);
                out.u64(node.modified);
                out.u32(d.bytes().len() as u32);
                out.bytes(d.bytes());
            }
        },
    }
}

/// Rebuilds `/home` in an empty RAM file system from a snapshot.
pub fn decode(data: &[u8], ram: &mut Tree, samples: &Samples) -> Option<()> {
    let mut r = Reader { data, pos: 0 };
    let mut known = BTreeSet::new();
    while r.pos < data.len() {
        let kind = r.u8()?;
        if kind == R_KNOWN_SAMPLE {
            known.insert(r.str()?.to_string());
            continue;
        }
        let path = r.str()?;
        let modified = r.u64()?;
        let comps = tree::components(path).ok()?;
        let id = match kind {
            R_DIR => ram.mkdir_all(&comps).ok(),
            R_FILE => {
                let len = r.u32()? as usize;
                let bytes = r.take(len)?;
                ram.add_file(&comps, Data::Owned(bytes.to_vec())).ok()
            }
            R_SAMPLE_FILE => {
                let src = r.str()?;
                // A sample since removed from the system image is dropped.
                samples.by_path.get(src).and_then(|&d| ram.add_file(&comps, Data::Static(d)).ok())
            }
            _ => return None,
        };
        if let Some(n) = id.and_then(|id| ram.node_mut(id)) {
            n.modified = modified;
        }
    }
    samples.seed(ram, Some(&known));
    Some(())
}

/// The home disk.
pub struct Store {
    disk: block::Client,
    sectors: u64,
    generation: u64,
    /// The slot the next save goes to.
    next_slot: u64,
}

fn io(e: impl core::fmt::Debug) -> FsError {
    println!("home disk: {:?}", e);
    FsError::Io
}

impl Store {
    /// Connects to the home disk, waiting a few seconds for its driver.
    /// Returns `None` if there is no usable home disk.
    pub fn connect() -> Option<Store> {
        let name = blk::service_name(HOME_DISK_SERIAL);
        let start = vrt::time::now_ns();
        loop {
            let names = vproto::with_registry(|r| r.list()).ok().and_then(|r| r.ok()).unwrap_or_default();
            if names.contains(&name) {
                break;
            }
            if vrt::time::now_ns() - start > ATTACH_TIMEOUT_NS {
                return None;
            }
            vrt::time::sleep(vrt::time::Duration::from_millis(20));
        }
        let disk = block::Client::new(vproto::connect(&name).ok()?);
        let info = disk.info().ok()?;
        if info.read_only || info.sector_size != SECTOR as u32 || info.sectors < 2 * (SLOT0 + 64) {
            println!(
                "home disk unusable ({} sectors of {} bytes{})",
                info.sectors,
                info.sector_size,
                if info.read_only { ", read-only" } else { "" }
            );
            return None;
        }
        Some(Store { disk, sectors: info.sectors, generation: 0, next_slot: 0 })
    }

    fn slot_start(&self, slot: u64) -> u64 {
        if slot == 0 { SLOT0 } else { self.sectors / 2 }
    }

    /// Bytes of snapshot data a slot can hold.
    pub fn capacity(&self) -> usize {
        ((self.sectors / 2 - SLOT0 - 1) as usize) * SECTOR
    }

    fn read(&self, lba: u64, bytes: usize) -> Result<Vec<u8>, FsError> {
        let mut out = Vec::with_capacity(bytes.next_multiple_of(SECTOR));
        let mut lba = lba;
        while out.len() < bytes {
            let chunk = (bytes - out.len()).min(blk::MAX_TRANSFER as usize).div_ceil(SECTOR);
            let Bytes(data) = self.disk.read(lba, chunk as u32).map_err(io)?.map_err(io)?;
            out.extend_from_slice(&data);
            lba += chunk as u64;
        }
        out.truncate(bytes);
        Ok(out)
    }

    fn write(&self, lba: u64, data: &[u8]) -> Result<(), FsError> {
        let mut lba = lba;
        for chunk in data.chunks(blk::MAX_TRANSFER as usize) {
            let mut buf = chunk.to_vec();
            buf.resize(chunk.len().next_multiple_of(SECTOR), 0);
            let sectors = (buf.len() / SECTOR) as u64;
            self.disk.write(lba, Bytes(buf)).map_err(io)?.map_err(io)?;
            lba += sectors;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), FsError> {
        self.disk.flush().map_err(io)?.map_err(io)
    }

    /// Reads the newest valid snapshot, if any.
    pub fn load(&mut self) -> Option<Vec<u8>> {
        let mut best: Option<(u64, u64, Vec<u8>)> = None;
        for slot in 0..2 {
            let start = self.slot_start(slot);
            let Ok(h) = self.read(start, SECTOR) else { continue };
            if h[0..8] != MAGIC {
                continue;
            }
            let generation = u64::from_le_bytes(h[8..16].try_into().unwrap());
            let len = u64::from_le_bytes(h[16..24].try_into().unwrap()) as usize;
            let crc = u32::from_le_bytes(h[24..28].try_into().unwrap());
            if len > self.capacity() || best.as_ref().is_some_and(|(g, ..)| *g >= generation) {
                continue;
            }
            match self.read(start + 1, len) {
                Ok(data) if crc32(&data) == crc => best = Some((generation, slot, data)),
                _ => println!("home snapshot in slot {} is damaged; ignoring it", slot),
            }
        }
        let (generation, slot, data) = best?;
        self.generation = generation;
        self.next_slot = 1 - slot;
        Some(data)
    }

    /// Writes a snapshot into the older slot: data first, then its header.
    pub fn save(&mut self, data: &[u8]) -> Result<(), FsError> {
        if data.len() > self.capacity() {
            return Err(FsError::NoSpace);
        }
        let start = self.slot_start(self.next_slot);
        self.write(start + 1, data)?;
        self.flush()?;
        let mut header = [0u8; SECTOR];
        header[0..8].copy_from_slice(&MAGIC);
        header[8..16].copy_from_slice(&(self.generation + 1).to_le_bytes());
        header[16..24].copy_from_slice(&(data.len() as u64).to_le_bytes());
        header[24..28].copy_from_slice(&crc32(data).to_le_bytes());
        self.write(start, &header)?;
        self.flush()?;
        self.generation += 1;
        self.next_slot = 1 - self.next_slot;
        Ok(())
    }
}
