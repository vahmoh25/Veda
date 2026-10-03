//! `vfs` — the Vindows file system service.
//!
//! Namespace:
//! * `/system` — the read-only system image (the initrd), and
//! * everything else — a writable in-memory file system holding the user's
//!   home directory and `/tmp`.
//!
//! The home directory starts with the sample files of the system image. If
//! a home disk is attached, `/home` is restored from it at boot and saved
//! back shortly after every change (see [`persist`]).
//!
//! Each client connection has its own file descriptor table.

#![no_std]
#![no_main]

extern crate alloc;

mod persist;
mod tree;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vipc::{Bytes, WaitSet};
use vproto::fs::{DirEntry, FsError, MAX_IO, Space, Stat, open_flags, vfs};
use vrt::object::{Channel, Vmo};
use vrt::println;

use persist::{Samples, Store};
use tree::{Data, Kind, NodeId, Tree};

vrt::entry!(main);

const SYSTEM: &str = "system";
/// Directories every home has.
const HOME_DIRS: [&str; 4] = ["home/user/Documents", "home/user/Pictures", "home/user/Music", "home/user/Desktop"];
/// Changes are saved once nothing changed for this long...
const SAVE_QUIET_NS: u64 = 500_000_000;
/// ... or at the latest this long after the first unsaved change.
const SAVE_MAX_DELAY_NS: u64 = 5_000_000_000;
/// Room reserved for the snapshot record of a new file or directory (its
/// header and path).
const RECORD_SLACK: usize = 512;

/// Which tree a path lives in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mount {
    System,
    Ram,
}

struct OpenFile {
    mount: Mount,
    node: NodeId,
    flags: u32,
    /// The file is in `/home`, so it counts against the home disk.
    home: bool,
}

struct Fs {
    system: Tree,
    ram: Tree,
    samples: Samples,
    /// The home disk, if one is attached.
    store: Option<Store>,
    /// Times of the first and the latest unsaved change.
    dirty: Option<(u64, u64)>,
}

impl Fs {
    /// Resolves a path to a tree and the components inside it.
    fn route<'p>(&self, path: &'p str) -> Result<(Mount, Vec<&'p str>), FsError> {
        let comps = tree::components(path)?;
        if comps.first() == Some(&SYSTEM) { Ok((Mount::System, comps[1..].to_vec())) } else { Ok((Mount::Ram, comps)) }
    }

    fn tree(&self, m: Mount) -> &Tree {
        match m {
            Mount::System => &self.system,
            Mount::Ram => &self.ram,
        }
    }

    fn tree_mut(&mut self, m: Mount) -> &mut Tree {
        match m {
            Mount::System => &mut self.system,
            Mount::Ram => &mut self.ram,
        }
    }

    /// Fails with `NoSpace` unless `/home` can grow by `extra` bytes and
    /// still fit on the home disk (without one there is nothing to check).
    fn reserve(&self, extra: usize) -> Result<(), FsError> {
        if extra == 0 {
            return Ok(());
        }
        match &self.store {
            Some(store) if persist::encoded_len(&self.ram, &self.samples) + extra > store.capacity() => {
                Err(FsError::NoSpace)
            }
            _ => Ok(()),
        }
    }

    /// How much more of the home disk a file takes once `d` becomes
    /// `new_len` bytes long (an unmodified sample is stored as a reference,
    /// so its first change stores the whole file).
    fn growth(&self, d: &Data, new_len: usize) -> usize {
        let stored = if self.samples.source_of(d).is_some() { 0 } else { d.bytes().len() };
        new_len.saturating_sub(stored)
    }

    /// Records a change to the writable file system.
    fn changed(&mut self) {
        if self.store.is_some() {
            let now = vrt::time::now_ns();
            let first = self.dirty.map_or(now, |(first, _)| first);
            self.dirty = Some((first, now));
        }
    }

    /// When unsaved changes are due to be saved.
    fn save_deadline(&self) -> u64 {
        match self.dirty {
            Some((first, last)) => (last + SAVE_QUIET_NS).min(first + SAVE_MAX_DELAY_NS),
            None => vabi::DEADLINE_INFINITE,
        }
    }

    /// Saves `/home` to the home disk if anything changed.
    fn save(&mut self) -> Result<(), FsError> {
        let Some(store) = &mut self.store else { return Ok(()) };
        if self.dirty.take().is_none() {
            return Ok(());
        }
        let data = persist::encode(&self.ram, &self.samples);
        let r = store.save(&data);
        if let Err(e) = r {
            println!("cannot save the home directory ({} bytes): {}", data.len(), e);
        }
        r
    }
}

/// Whether a path is in `/home`, which is kept on the home disk.
fn in_home(m: Mount, comps: &[&str]) -> bool {
    m == Mount::Ram && comps.first() == Some(&"home")
}

/// The length of a path from its components.
fn path_len(comps: &[&str]) -> usize {
    comps.iter().map(|c| c.len() + 1).sum()
}

/// One client connection.
struct Session<'a> {
    fs: &'a mut Fs,
    fds: &'a mut BTreeMap<u32, OpenFile>,
}

const MAX_FDS: usize = 256;

impl vfs::Server for Session<'_> {
    fn open(&mut self, path: String, flags: u32) -> Result<u32, FsError> {
        let (m, comps) = self.fs.route(&path)?;
        let home = in_home(m, &comps);
        let writing = flags & (open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE | open_flags::APPEND) != 0;
        if writing && self.fs.tree(m).read_only {
            return Err(FsError::ReadOnly);
        }
        let mut changed = false;
        let node = match self.fs.tree(m).lookup(&comps) {
            Ok(n) => n,
            Err(FsError::NotFound) if flags & open_flags::CREATE != 0 => {
                if home {
                    self.fs.reserve(RECORD_SLACK)?;
                }
                changed = true;
                self.fs.tree_mut(m).create(&comps, false)?
            }
            Err(e) => return Err(e),
        };
        let tree = self.fs.tree_mut(m);
        match &mut tree.node_mut(node).unwrap().kind {
            Kind::Dir(_) => return Err(FsError::IsDir),
            Kind::File(d) if flags & open_flags::TRUNCATE != 0 => {
                *d = Data::Owned(Vec::new());
                tree.touch(node);
                changed = true;
            }
            Kind::File(_) => {}
        }
        if self.fds.len() >= MAX_FDS {
            return Err(FsError::TooMany);
        }
        if changed {
            self.fs.changed();
        }
        let fd = (1..).find(|fd| !self.fds.contains_key(fd)).unwrap();
        self.fds.insert(fd, OpenFile { mount: m, node, flags, home });
        Ok(fd)
    }

    fn close(&mut self, fd: u32) -> Result<(), FsError> {
        self.fds.remove(&fd).map(|_| ()).ok_or(FsError::BadFd)
    }

    fn read(&mut self, fd: u32, offset: u64, len: u32) -> Result<Bytes, FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        let Some(tree::Node { kind: Kind::File(d), .. }) = self.fs.tree(f.mount).node(f.node) else {
            return Err(FsError::BadFd);
        };
        let bytes = d.bytes();
        let start = (offset as usize).min(bytes.len());
        let end = (start + len.min(MAX_IO) as usize).min(bytes.len());
        Ok(Bytes(bytes[start..end].to_vec()))
    }

    fn write(&mut self, fd: u32, offset: u64, data: Bytes) -> Result<u32, FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        if f.flags & (open_flags::WRITE | open_flags::APPEND) == 0 {
            return Err(FsError::BadFd);
        }
        let (m, node, append, home) = (f.mount, f.node, f.flags & open_flags::APPEND != 0, f.home);
        if home && let Some(tree::Node { kind: Kind::File(d), .. }) = self.fs.tree(m).node(node) {
            let at = if append { d.bytes().len() } else { offset as usize };
            self.fs.reserve(self.fs.growth(d, d.bytes().len().max(at.saturating_add(data.0.len()))))?;
        }
        let tree = self.fs.tree_mut(m);
        let Some(tree::Node { kind: Kind::File(d), .. }) = tree.node_mut(node) else { return Err(FsError::BadFd) };
        let v = d.make_mut();
        let at = if append { v.len() } else { offset as usize };
        if at > 1 << 31 {
            return Err(FsError::NoSpace);
        }
        if v.len() < at + data.0.len() {
            v.resize(at + data.0.len(), 0);
        }
        v[at..at + data.0.len()].copy_from_slice(&data.0);
        tree.touch(node);
        self.fs.changed();
        Ok(data.0.len() as u32)
    }

    fn stat(&mut self, path: String) -> Result<Stat, FsError> {
        let (m, comps) = self.fs.route(&path)?;
        let t = self.fs.tree(m);
        Ok(t.stat(t.lookup(&comps)?))
    }

    fn read_dir(&mut self, path: String) -> Result<Vec<DirEntry>, FsError> {
        let (m, comps) = self.fs.route(&path)?;
        let t = self.fs.tree(m);
        let mut entries = t.list(t.lookup(&comps)?)?;
        if m == Mount::Ram && comps.is_empty() {
            entries.push(DirEntry { name: SYSTEM.into(), is_dir: true, size: 0, modified: 0 });
        }
        // Directories first, then case-insensitive name order.
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(entries)
    }

    fn mkdir(&mut self, path: String) -> Result<(), FsError> {
        let (m, comps) = self.fs.route(&path)?;
        if in_home(m, &comps) {
            self.fs.reserve(RECORD_SLACK)?;
        }
        self.fs.tree_mut(m).create(&comps, true)?;
        self.fs.changed();
        Ok(())
    }

    fn remove(&mut self, path: String) -> Result<(), FsError> {
        let (m, comps) = self.fs.route(&path)?;
        self.fs.tree_mut(m).remove(&comps)?;
        self.fs.changed();
        Ok(())
    }

    fn rename(&mut self, from: String, to: String) -> Result<(), FsError> {
        let (m1, c1) = self.fs.route(&from)?;
        let (m2, c2) = self.fs.route(&to)?;
        if m1 != m2 {
            return Err(FsError::Invalid);
        }
        if in_home(m2, &c2) {
            let t = self.fs.tree(m1);
            let (bytes, nodes) = t.subtree_size(t.lookup(&c1)?);
            // Within /home only the paths in the records change; moving in
            // from elsewhere (/tmp) brings the contents along.
            let extra = if in_home(m1, &c1) {
                nodes * path_len(&c2).saturating_sub(path_len(&c1))
            } else {
                bytes + nodes * RECORD_SLACK
            };
            self.fs.reserve(extra)?;
        }
        self.fs.tree_mut(m1).rename(&c1, &c2)?;
        self.fs.changed();
        Ok(())
    }

    fn read_file(&mut self, path: String) -> Result<(Vmo, u64), FsError> {
        let (m, comps) = self.fs.route(&path)?;
        let t = self.fs.tree(m);
        let Some(tree::Node { kind, .. }) = t.node(t.lookup(&comps)?) else { return Err(FsError::NotFound) };
        let Kind::File(d) = kind else { return Err(FsError::IsDir) };
        let bytes = d.bytes();
        let vmo = Vmo::create(bytes.len().max(1)).map_err(|_| FsError::NoSpace)?;
        vmo.write(0, bytes).map_err(|_| FsError::Io)?;
        Ok((vmo, bytes.len() as u64))
    }

    fn write_file(&mut self, path: String, data: Vmo, len: u64) -> Result<(), FsError> {
        let (m, comps) = self.fs.route(&path)?;
        if len > 1 << 30 {
            return Err(FsError::NoSpace);
        }
        if in_home(m, &comps) {
            let t = self.fs.tree(m);
            let extra = match t.lookup(&comps).ok().and_then(|n| t.node(n)) {
                Some(tree::Node { kind: Kind::File(d), .. }) => self.fs.growth(d, len as usize),
                _ => len as usize + RECORD_SLACK,
            };
            self.fs.reserve(extra)?;
        }
        let mut buf = alloc::vec![0u8; len as usize];
        data.read(0, &mut buf).map_err(|_| FsError::Invalid)?;
        let tree = self.fs.tree_mut(m);
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        let node = match tree.lookup(&comps) {
            Ok(n) => n,
            Err(FsError::NotFound) => tree.create(&comps, false)?,
            Err(e) => return Err(e),
        };
        match &mut tree.node_mut(node).unwrap().kind {
            Kind::File(d) => *d = Data::Owned(buf),
            Kind::Dir(_) => return Err(FsError::IsDir),
        }
        tree.touch(node);
        self.fs.changed();
        Ok(())
    }

    fn truncate(&mut self, fd: u32, len: u64) -> Result<(), FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        let (m, node, home) = (f.mount, f.node, f.home);
        if home && let Some(tree::Node { kind: Kind::File(d), .. }) = self.fs.tree(m).node(node) {
            self.fs.reserve(self.fs.growth(d, len.min(1 << 31) as usize))?;
        }
        let tree = self.fs.tree_mut(m);
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        let Some(tree::Node { kind: Kind::File(d), .. }) = tree.node_mut(node) else { return Err(FsError::BadFd) };
        d.make_mut().resize(len.min(1 << 31) as usize, 0);
        self.fs.changed();
        Ok(())
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.fs.save()
    }

    fn space(&mut self, path: String) -> Result<Space, FsError> {
        let (m, comps) = self.fs.route(&path)?;
        let t = self.fs.tree(m);
        t.lookup(&comps)?;
        let (bytes, _) = t.subtree_size(tree::ROOT);
        Ok(match (m, &self.fs.store) {
            (Mount::System, _) => Space { total: bytes as u64, used: bytes as u64, persistent: true },
            (Mount::Ram, Some(store)) if in_home(m, &comps) => Space {
                total: store.capacity() as u64,
                used: persist::encoded_len(&self.fs.ram, &self.fs.samples) as u64,
                persistent: true,
            },
            // Kept in memory: as much as the free memory allows.
            (Mount::Ram, _) => {
                let free = vrt::object::system_info().map(|i| i.free_memory).unwrap_or(0);
                Space { total: bytes as u64 + free, used: bytes as u64, persistent: false }
            }
        })
    }
}

/// A writable tree with `/tmp` and an empty `/home`.
fn fresh_ram() -> Tree {
    let mut ram = Tree::new(false);
    let _ = ram.mkdir_all(&["tmp"]);
    let _ = ram.mkdir_all(&["home", "user"]);
    ram
}

/// Builds the file systems: the system image from the initrd, and the home
/// directory from the home disk or, failing that, the samples.
fn build(archive: &initrd::Archive<'static>) -> Fs {
    let mut system = Tree::new(true);
    for f in archive.files() {
        let comps = tree::components(f.path).unwrap_or_default();
        let _ = system.add_file(&comps, Data::Static(f.data));
    }
    let samples = Samples::new(archive);
    let mut store = Store::connect();
    let mut ram = fresh_ram();
    let restored = match store.as_mut().and_then(|s| s.load()) {
        Some(snapshot) => {
            if persist::decode(&snapshot, &mut ram, &samples).is_some() {
                true
            } else {
                println!("the home snapshot is unreadable; starting from the samples");
                ram = fresh_ram();
                false
            }
        }
        None => false,
    };
    if !restored {
        samples.seed(&mut ram, None);
    }
    for dir in HOME_DIRS {
        let comps = tree::components(dir).unwrap_or_default();
        let _ = ram.mkdir_all(&comps);
    }
    let home = match (&store, restored) {
        (None, _) => "in memory (no home disk)",
        (Some(_), true) => "restored from the home disk",
        (Some(_), false) => "new, on the home disk",
    };
    println!("home directory {}", home);
    Fs { system, ram, samples, store, dirty: None }
}

fn main() -> i32 {
    let Some(initrd_vmo) = vrt::env::take_handle(vabi::startup::role::INITRD).map(Vmo::from_handle) else {
        println!("no initrd handle");
        return 1;
    };
    let size = initrd_vmo.size().unwrap_or(0);
    let Ok(addr) = initrd_vmo.map(0, size, vabi::map_flags::READ) else {
        println!("cannot map the initrd");
        return 1;
    };
    // SAFETY: the mapping is read-only and lives as long as the process.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    let Ok(archive) = initrd::Archive::open(bytes) else {
        println!("the initrd is corrupt");
        return 1;
    };
    let mut fs = build(&archive);

    let Ok(listener) = vproto::register(vfs::NAME) else {
        println!("cannot register the vfs service");
        return 1;
    };
    println!("serving /system ({} files) and /home", archive.len());

    struct Client {
        channel: Channel,
        fds: BTreeMap<u32, OpenFile>,
    }
    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let mut next_id = 1u64;
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE | signals::PEER_CLOSED, 0);
        for (&id, c) in &clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, id);
        }
        let Ok(ready) = ws.wait(fs.save_deadline()) else { continue };
        for (key, observed) in ready {
            if key == 0 {
                while let Some(ch) = vproto::accept(&listener) {
                    clients.insert(next_id, Client { channel: ch, fds: BTreeMap::new() });
                    next_id += 1;
                }
                continue;
            }
            let Some(client) = clients.get_mut(&key) else { continue };
            if observed & signals::READABLE != 0 {
                while let Ok(msg) = client.channel.read() {
                    let mut session = Session { fs: &mut fs, fds: &mut client.fds };
                    match vfs::dispatch(&mut session, msg) {
                        Ok(reply) => {
                            let _ = reply.send(&client.channel);
                        }
                        Err(e) => println!("bad request: {}", e),
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                clients.remove(&key);
            }
        }
        if vrt::time::now_ns() >= fs.save_deadline() {
            let _ = fs.save();
        }
    }
}
