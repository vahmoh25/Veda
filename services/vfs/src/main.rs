//! `vfs` — the Veda file system service.
//!
//! Namespace:
//! * `/system` — the read-only system image (the initrd),
//! * `/dev` — the devices (`null`, `zero`, `full`, `random`, `urandom`;
//!   see [`dev`]), and
//! * everything else — a writable in-memory file system holding the user's
//!   home directory and `/tmp`.
//!
//! The home directory starts with the sample files of the system image. If
//! a home disk is attached, `/home` is restored from it at boot and saved
//! back shortly after every change (see [`persist`]).
//!
//! Files are open in one of two ways. Each client connection has its own
//! table of descriptors (`vfs::open`), read and written at explicit
//! offsets. And `vfs::open_file` gives a file a connection of its own (the
//! `file` protocol): an open file description in the POSIX sense, with an
//! offset shared by every connection duplicated from it, which can be
//! passed to other processes. As in POSIX, a file removed while open stays
//! readable and writable through what has it open, and goes away with the
//! last of them.
//!
//! **Private directories.** `/home/.private/<service>` belongs to the
//! system service of that name (as the registry identifies the client):
//! only it can see and change what is inside, and `.private` does not
//! appear in listings of `/home` for anyone else. Services keep secrets and
//! personal data there (the agent its Deepgram key and what it remembers
//! about the user), on the home disk like everything in `/home`.

#![no_std]
#![no_main]

extern crate alloc;

mod dev;
mod persist;
mod tree;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use vabi::signals;
use vipc::{Bytes, WaitSet};
use vproto::fs::{DirEntry, FsError, MAX_IO, Space, Stat, device, file, open_flags, seek, vfs};
use vproto::init::ClientIdentity;
use vrt::object::{Channel, Vmo};
use vrt::println;

use dev::Dev;
use persist::{Samples, Store};
use tree::{Data, Kind, NodeId, Tree};

vrt::entry!(main);

const SYSTEM: &str = "system";
const DEV: &str = "dev";
/// Directories every home has.
const HOME_DIRS: [&str; 4] = ["home/user/Documents", "home/user/Pictures", "home/user/Music", "home/user/Desktop"];
/// Changes are saved once nothing changed for this long...
const SAVE_QUIET_NS: u64 = 500_000_000;
/// ... or at the latest this long after the first unsaved change.
const SAVE_MAX_DELAY_NS: u64 = 5_000_000_000;
/// Room reserved for the snapshot record of a new file or directory (its
/// header and path).
const RECORD_SLACK: usize = 512;
/// Largest file in the writable file system.
const MAX_FILE: u64 = 1 << 31;
/// Memory the writable file system leaves free for the rest of the system.
const MEMORY_RESERVE: u64 = 64 << 20;
/// Descriptors per client connection (`vfs::open`).
const MAX_FDS: usize = 256;
/// File connections (`vfs::open_file`) of all clients together.
const MAX_FILE_CONNS: usize = 8192;

/// Which tree a path lives in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mount {
    System,
    Ram,
    Dev,
}

/// A descriptor of the `vfs::open` kind.
struct OpenFile {
    mount: Mount,
    node: NodeId,
    flags: u32,
    /// The file is in `/home`, so it counts against the home disk.
    home: bool,
}

/// What an open file description refers to.
#[derive(Clone, Copy)]
enum Target {
    Node(Mount, NodeId),
    Dev(Dev),
}

/// An open file description (behind every connection of the `file` kind
/// duplicated from one `open_file`).
struct Description {
    target: Target,
    flags: u32,
    offset: u64,
    home: bool,
}

type Shared = Rc<RefCell<Description>>;

struct Fs {
    system: Tree,
    ram: Tree,
    samples: Samples,
    /// The home disk, if one is attached.
    store: Option<Store>,
    /// Times of the first and the latest unsaved change.
    dirty: Option<(u64, u64)>,
    /// How many descriptors and descriptions have each node open.
    open: BTreeMap<(Mount, NodeId), u32>,
    /// Nodes removed while open, forgotten when the last closes.
    orphans: BTreeSet<(Mount, NodeId)>,
}

impl Fs {
    /// Resolves a path to a tree and the components inside it.
    fn route<'p>(&self, path: &'p str) -> Result<(Mount, Vec<&'p str>), FsError> {
        let comps = tree::components(path)?;
        match comps.first() {
            Some(&SYSTEM) => Ok((Mount::System, comps[1..].to_vec())),
            Some(&DEV) => Ok((Mount::Dev, comps[1..].to_vec())),
            _ => Ok((Mount::Ram, comps)),
        }
    }

    /// The tree of a mount (`/dev` has none: nothing there can change).
    fn tree(&self, m: Mount) -> Result<&Tree, FsError> {
        match m {
            Mount::System => Ok(&self.system),
            Mount::Ram => Ok(&self.ram),
            Mount::Dev => Err(FsError::ReadOnly),
        }
    }

    fn tree_mut(&mut self, m: Mount) -> Result<&mut Tree, FsError> {
        match m {
            Mount::System => Ok(&mut self.system),
            Mount::Ram => Ok(&mut self.ram),
            Mount::Dev => Err(FsError::ReadOnly),
        }
    }

    /// Something opened `node`.
    fn hold(&mut self, m: Mount, node: NodeId) {
        *self.open.entry((m, node)).or_default() += 1;
    }

    /// Something that had `node` open closed it.
    fn release(&mut self, m: Mount, node: NodeId) {
        let Some(n) = self.open.get_mut(&(m, node)) else { return };
        *n -= 1;
        if *n == 0 {
            self.open.remove(&(m, node));
            if self.orphans.remove(&(m, node))
                && let Ok(t) = self.tree_mut(m)
            {
                t.forget(node);
            }
        }
    }

    /// Disposes of a node taken out of its tree: now, or when the last
    /// thing that has it open closes it.
    fn dispose(&mut self, m: Mount, node: NodeId) {
        if self.open.contains_key(&(m, node)) {
            self.orphans.insert((m, node));
        } else if let Ok(t) = self.tree_mut(m) {
            t.forget(node);
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

    /// Whether a file may grow by `more` bytes in memory, where every
    /// writable file is kept: the system keeps [`MEMORY_RESERVE`] free for
    /// everything else. Beyond that, `NoSpace`, as a full disk would say.
    fn memory_for(&self, more: usize) -> Result<(), FsError> {
        if more == 0 {
            return Ok(());
        }
        let free = vrt::object::system_info().map_or(0, |i| i.free_memory);
        if (more as u64).saturating_add(MEMORY_RESERVE) > free {
            return Err(FsError::NoSpace);
        }
        Ok(())
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

    /// Reads up to `len` bytes of an open file at `offset`.
    fn read_node(&self, m: Mount, node: NodeId, offset: u64, len: u32) -> Result<Vec<u8>, FsError> {
        let Some(tree::Node { kind: Kind::File(d), .. }) = self.tree(m)?.node(node) else {
            return Err(FsError::BadFd);
        };
        let bytes = d.bytes();
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + len.min(MAX_IO) as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    /// Writes `data` into an open file at `offset` (at its end with
    /// `append`); returns where the data went.
    fn write_node(
        &mut self,
        m: Mount,
        node: NodeId,
        offset: u64,
        append: bool,
        home: bool,
        data: &[u8],
    ) -> Result<u64, FsError> {
        if self.tree(m)?.read_only {
            return Err(FsError::ReadOnly);
        }
        let Some(tree::Node { kind: Kind::File(d), .. }) = self.tree(m)?.node(node) else {
            return Err(FsError::BadFd);
        };
        let at = if append { d.bytes().len() as u64 } else { offset };
        let end = at.checked_add(data.len() as u64).ok_or(FsError::NoSpace)?;
        if end > MAX_FILE {
            return Err(FsError::NoSpace);
        }
        if home {
            self.reserve(self.growth(d, d.bytes().len().max(end as usize)))?;
        }
        self.memory_for((end as usize).saturating_sub(d.bytes().len()))?;
        let tree = self.tree_mut(m)?;
        let Some(tree::Node { kind: Kind::File(d), .. }) = tree.node_mut(node) else { return Err(FsError::BadFd) };
        let v = d.make_mut();
        let (at, end) = (at as usize, end as usize);
        if v.len() < end {
            v.try_reserve_exact(end - v.len()).map_err(|_| FsError::NoSpace)?;
            v.resize(end, 0);
        }
        v[at..end].copy_from_slice(data);
        tree.touch(node);
        self.changed();
        Ok(at as u64)
    }

    /// Sets the length of an open file.
    fn truncate_node(&mut self, m: Mount, node: NodeId, home: bool, len: u64) -> Result<(), FsError> {
        if self.tree(m)?.read_only {
            return Err(FsError::ReadOnly);
        }
        if len > MAX_FILE {
            return Err(FsError::NoSpace);
        }
        let Some(tree::Node { kind: Kind::File(d), .. }) = self.tree(m)?.node(node) else {
            return Err(FsError::BadFd);
        };
        if home {
            self.reserve(self.growth(d, len as usize))?;
        }
        self.memory_for((len as usize).saturating_sub(d.bytes().len()))?;
        let tree = self.tree_mut(m)?;
        let Some(tree::Node { kind: Kind::File(d), .. }) = tree.node_mut(node) else { return Err(FsError::BadFd) };
        let v = d.make_mut();
        let len = len as usize;
        if v.len() < len {
            v.try_reserve_exact(len - v.len()).map_err(|_| FsError::NoSpace)?;
        }
        v.resize(len, 0);
        tree.touch(node);
        self.changed();
        Ok(())
    }

    /// Finds or creates the file at `comps` for opening with `flags`
    /// (truncating it if asked to). Returns it and whether anything changed.
    fn open_node(&mut self, m: Mount, comps: &[&str], flags: u32, home: bool) -> Result<NodeId, FsError> {
        let writing = flags & (open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE) != 0;
        if writing && self.tree(m)?.read_only {
            return Err(FsError::ReadOnly);
        }
        let mut changed = false;
        let node = match self.tree(m)?.lookup(comps) {
            Ok(_) if flags & open_flags::CREATE != 0 && flags & open_flags::EXCLUSIVE != 0 => {
                return Err(FsError::Exists);
            }
            Ok(n) => n,
            Err(FsError::NotFound) if flags & open_flags::CREATE != 0 => {
                if home {
                    self.reserve(RECORD_SLACK)?;
                }
                changed = true;
                self.tree_mut(m)?.create(comps, false)?
            }
            Err(e) => return Err(e),
        };
        let tree = self.tree_mut(m)?;
        match &mut tree.node_mut(node).unwrap().kind {
            Kind::Dir(_) => return Err(FsError::IsDir),
            Kind::File(d) if flags & open_flags::TRUNCATE != 0 => {
                if !d.bytes().is_empty() {
                    *d = Data::Owned(Vec::new());
                    changed = true;
                }
                tree.touch(node);
            }
            Kind::File(_) => {}
        }
        if changed {
            self.changed();
        }
        Ok(node)
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

/// The directory holding the services' private directories.
const PRIVATE: &str = ".private";

/// File connections created while handling a request, for the service
/// loop to wait on.
type NewFiles = Vec<(Channel, Shared)>;

/// One client connection.
struct Session<'a> {
    fs: &'a mut Fs,
    fds: &'a mut BTreeMap<u32, OpenFile>,
    /// Who the client is (decides access to private directories).
    who: &'a ClientIdentity,
    new_files: &'a mut NewFiles,
    /// File connections open now (all clients).
    file_conns: usize,
}

impl Session<'_> {
    /// Refuses paths inside another service's private directory (and the
    /// private directory itself to programs that are not services).
    fn guard(&self, m: Mount, comps: &[&str]) -> Result<(), FsError> {
        if m != Mount::Ram || comps.len() < 2 || comps[0] != "home" || comps[1] != PRIVATE {
            return Ok(());
        }
        let allowed = self.who.service && comps.get(2).is_none_or(|owner| *owner == self.who.name);
        if allowed { Ok(()) } else { Err(FsError::Denied) }
    }

    /// Routes and guards a path.
    fn resolve<'p>(&self, path: &'p str) -> Result<(Mount, Vec<&'p str>), FsError> {
        let (m, comps) = self.fs.route(path)?;
        self.guard(m, &comps)?;
        Ok((m, comps))
    }

    /// Renames `from` to `to`, replacing what is there if asked to.
    fn move_node(&mut self, from: &str, to: &str, replace: bool) -> Result<(), FsError> {
        let (m1, c1) = self.resolve(from)?;
        let (m2, c2) = self.resolve(to)?;
        if m1 == Mount::Ram && c1 == ["home", PRIVATE] {
            return Err(FsError::Denied);
        }
        if m1 != m2 {
            return Err(FsError::Invalid);
        }
        if in_home(m2, &c2) {
            let t = self.fs.tree(m1)?;
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
        if let Some(old) = self.fs.tree_mut(m1)?.rename(&c1, &c2, replace)? {
            self.fs.dispose(m1, old);
        }
        self.fs.changed();
        Ok(())
    }
}

impl vfs::Server for Session<'_> {
    fn open(&mut self, path: String, flags: u32) -> Result<u32, FsError> {
        let (m, comps) = self.resolve(&path)?;
        if m == Mount::Dev {
            return Err(FsError::Invalid);
        }
        if self.fds.len() >= MAX_FDS {
            return Err(FsError::TooMany);
        }
        let home = in_home(m, &comps);
        let node = self.fs.open_node(m, &comps, flags, home)?;
        self.fs.hold(m, node);
        let fd = (1..).find(|fd| !self.fds.contains_key(fd)).unwrap();
        self.fds.insert(fd, OpenFile { mount: m, node, flags, home });
        Ok(fd)
    }

    fn close(&mut self, fd: u32) -> Result<(), FsError> {
        let f = self.fds.remove(&fd).ok_or(FsError::BadFd)?;
        self.fs.release(f.mount, f.node);
        Ok(())
    }

    fn read(&mut self, fd: u32, offset: u64, len: u32) -> Result<Bytes, FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        self.fs.read_node(f.mount, f.node, offset, len).map(Bytes)
    }

    fn write(&mut self, fd: u32, offset: u64, data: Bytes) -> Result<u32, FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        if f.flags & open_flags::WRITE == 0 {
            return Err(FsError::BadFd);
        }
        let (m, node, append, home) = (f.mount, f.node, f.flags & open_flags::APPEND != 0, f.home);
        self.fs.write_node(m, node, offset, append, home, &data.0)?;
        Ok(data.0.len() as u32)
    }

    fn stat(&mut self, path: String) -> Result<Stat, FsError> {
        let (m, comps) = self.resolve(&path)?;
        if m == Mount::Dev {
            return Ok(Dev::stat(Dev::find(&comps)?));
        }
        let t = self.fs.tree(m)?;
        Ok(t.stat(t.lookup(&comps)?))
    }

    fn read_dir(&mut self, path: String) -> Result<Vec<DirEntry>, FsError> {
        let (m, comps) = self.resolve(&path)?;
        let mut entries = if m == Mount::Dev {
            match Dev::find(&comps)? {
                None => Dev::list(),
                Some(_) => return Err(FsError::NotDir),
            }
        } else {
            let t = self.fs.tree(m)?;
            t.list(t.lookup(&comps)?)?
        };
        if m == Mount::Ram && comps.is_empty() {
            for name in [SYSTEM, DEV] {
                entries.push(DirEntry { name: name.into(), is_dir: true, size: 0, modified: 0, inode: tree::ROOT });
            }
        }
        // Private directories are invisible to everyone but their owner.
        if m == Mount::Ram && comps == ["home"] && !self.who.service {
            entries.retain(|e| e.name != PRIVATE);
        }
        if m == Mount::Ram && comps == ["home", PRIVATE] {
            entries.retain(|e| e.name == self.who.name);
        }
        // Directories first, then case-insensitive name order.
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(entries)
    }

    fn mkdir(&mut self, path: String) -> Result<(), FsError> {
        let (m, comps) = self.resolve(&path)?;
        if m == Mount::Dev {
            // Its devices and itself exist; nothing can be added.
            return Err(if Dev::find(&comps).is_ok() { FsError::Exists } else { FsError::ReadOnly });
        }
        if in_home(m, &comps) {
            self.fs.reserve(RECORD_SLACK)?;
        }
        self.fs.tree_mut(m)?.create(&comps, true)?;
        self.fs.changed();
        Ok(())
    }

    fn remove(&mut self, path: String) -> Result<(), FsError> {
        let (m, comps) = self.resolve(&path)?;
        if m == Mount::Ram && comps == ["home", PRIVATE] {
            return Err(FsError::Denied);
        }
        let node = self.fs.tree_mut(m)?.unlink(&comps)?;
        self.fs.dispose(m, node);
        self.fs.changed();
        Ok(())
    }

    fn rename(&mut self, from: String, to: String) -> Result<(), FsError> {
        self.move_node(&from, &to, false)
    }

    fn read_file(&mut self, path: String) -> Result<(Vmo, u64), FsError> {
        let (m, comps) = self.resolve(&path)?;
        let t = self.fs.tree(m).map_err(|_| FsError::Invalid)?;
        let Some(tree::Node { kind, .. }) = t.node(t.lookup(&comps)?) else { return Err(FsError::NotFound) };
        let Kind::File(d) = kind else { return Err(FsError::IsDir) };
        let bytes = d.bytes();
        let vmo = Vmo::create(bytes.len().max(1)).map_err(|_| FsError::NoSpace)?;
        vmo.write(0, bytes).map_err(|_| FsError::Io)?;
        Ok((vmo, bytes.len() as u64))
    }

    fn write_file(&mut self, path: String, data: Vmo, len: u64) -> Result<(), FsError> {
        let (m, comps) = self.resolve(&path)?;
        if len > 1 << 30 {
            return Err(FsError::NoSpace);
        }
        if in_home(m, &comps) {
            let t = self.fs.tree(m)?;
            let extra = match t.lookup(&comps).ok().and_then(|n| t.node(n)) {
                Some(tree::Node { kind: Kind::File(d), .. }) => self.fs.growth(d, len as usize),
                _ => len as usize + RECORD_SLACK,
            };
            self.fs.reserve(extra)?;
        }
        if self.fs.tree(m)?.read_only {
            return Err(FsError::ReadOnly);
        }
        // The new contents are all read before the old ones go.
        self.fs.memory_for(len as usize)?;
        let mut buf = alloc::vec::Vec::new();
        buf.try_reserve_exact(len as usize).map_err(|_| FsError::NoSpace)?;
        buf.resize(len as usize, 0);
        data.read(0, &mut buf).map_err(|_| FsError::Invalid)?;
        let tree = self.fs.tree_mut(m)?;
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
        if f.flags & open_flags::WRITE == 0 {
            return Err(FsError::BadFd);
        }
        let (m, node, home) = (f.mount, f.node, f.home);
        self.fs.truncate_node(m, node, home, len.min(MAX_FILE))
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.fs.save()
    }

    fn space(&mut self, path: String) -> Result<Space, FsError> {
        let (m, comps) = self.fs.route(&path)?;
        if m == Mount::Dev {
            return Ok(Space { total: 0, used: 0, persistent: false });
        }
        let t = self.fs.tree(m)?;
        t.lookup(&comps)?;
        let (bytes, _) = t.subtree_size(tree::ROOT);
        Ok(match (m, &self.fs.store) {
            (Mount::Ram, Some(store)) if in_home(m, &comps) => Space {
                total: store.capacity() as u64,
                used: persist::encoded_len(&self.fs.ram, &self.fs.samples) as u64,
                persistent: true,
            },
            (Mount::Ram, _) => {
                // Kept in memory: as much as the free memory allows, but for
                // what is left to the rest of the system.
                let free = vrt::object::system_info().map(|i| i.free_memory).unwrap_or(0);
                let room = free.saturating_sub(MEMORY_RESERVE);
                Space { total: bytes as u64 + room, used: bytes as u64, persistent: false }
            }
            _ => Space { total: bytes as u64, used: bytes as u64, persistent: true },
        })
    }

    fn open_file(&mut self, path: String, flags: u32) -> Result<(Channel, Stat), FsError> {
        let (m, comps) = self.resolve(&path)?;
        if self.file_conns + self.new_files.len() >= MAX_FILE_CONNS {
            return Err(FsError::TooMany);
        }
        let (target, stat, home) = if m == Mount::Dev {
            match Dev::find(&comps)? {
                Some(d) => (Target::Dev(d), Dev::stat(Some(d)), false),
                None => return Err(FsError::IsDir),
            }
        } else {
            let home = in_home(m, &comps);
            let node = self.fs.open_node(m, &comps, flags, home)?;
            self.fs.hold(m, node);
            (Target::Node(m, node), self.fs.tree(m)?.stat(node), home)
        };
        let desc = Rc::new(RefCell::new(Description { target, flags, offset: 0, home }));
        let (ours, theirs) = match Channel::create() {
            Ok(pair) => pair,
            Err(_) => {
                if let Target::Node(m, node) = target {
                    self.fs.release(m, node);
                }
                return Err(FsError::NoSpace);
            }
        };
        self.new_files.push((ours, desc));
        Ok((theirs, stat))
    }

    fn replace(&mut self, from: String, to: String) -> Result<(), FsError> {
        self.move_node(&from, &to, true)
    }

    fn set_modified(&mut self, path: String, modified: u64) -> Result<(), FsError> {
        let (m, comps) = self.resolve(&path)?;
        let tree = self.fs.tree_mut(m)?;
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        let node = tree.lookup(&comps)?;
        if modified == 0 {
            tree.touch(node);
        } else {
            tree.set_modified(node, modified);
        }
        self.fs.changed();
        Ok(())
    }
}

/// One connection to an open file.
struct FileSession<'a> {
    fs: &'a mut Fs,
    desc: &'a Shared,
    new_files: &'a mut NewFiles,
    file_conns: usize,
}

impl FileSession<'_> {
    fn readable(&self) -> Result<(), FsError> {
        if self.desc.borrow().flags & open_flags::READ != 0 { Ok(()) } else { Err(FsError::BadFd) }
    }

    fn writable(&self) -> Result<(), FsError> {
        if self.desc.borrow().flags & open_flags::WRITE != 0 { Ok(()) } else { Err(FsError::BadFd) }
    }

    fn read_at_offset(&mut self, offset: u64, len: u32) -> Result<Vec<u8>, FsError> {
        self.readable()?;
        let target = self.desc.borrow().target;
        match target {
            Target::Node(m, node) => self.fs.read_node(m, node, offset, len),
            Target::Dev(d) => Ok(d.read(len.min(MAX_IO) as usize)),
        }
    }

    /// Writes at `offset` (at the end in append mode); returns where.
    fn write_at_offset(&mut self, offset: u64, data: &[u8]) -> Result<u64, FsError> {
        self.writable()?;
        let d = self.desc.borrow();
        let (target, append, home) = (d.target, d.flags & open_flags::APPEND != 0, d.home);
        drop(d);
        match target {
            Target::Node(m, node) => self.fs.write_node(m, node, offset, append, home, data),
            Target::Dev(dev) => dev.write(data.len()).map(|_| offset),
        }
    }
}

impl file::Server for FileSession<'_> {
    fn read(&mut self, len: u32) -> Result<Bytes, FsError> {
        let offset = self.desc.borrow().offset;
        let data = self.read_at_offset(offset, len)?;
        let target = self.desc.borrow().target;
        if let Target::Node(..) = target {
            self.desc.borrow_mut().offset = offset + data.len() as u64;
        }
        Ok(Bytes(data))
    }

    fn write(&mut self, data: Bytes) -> Result<u32, FsError> {
        let offset = self.desc.borrow().offset;
        let at = self.write_at_offset(offset, &data.0)?;
        let target = self.desc.borrow().target;
        if let Target::Node(..) = target {
            self.desc.borrow_mut().offset = at + data.0.len() as u64;
        }
        Ok(data.0.len() as u32)
    }

    fn read_at(&mut self, offset: u64, len: u32) -> Result<Bytes, FsError> {
        self.read_at_offset(offset, len).map(Bytes)
    }

    fn write_at(&mut self, offset: u64, data: Bytes) -> Result<u32, FsError> {
        self.write_at_offset(offset, &data.0)?;
        Ok(data.0.len() as u32)
    }

    fn seek(&mut self, offset: i64, whence: u32) -> Result<u64, FsError> {
        let target = self.desc.borrow().target;
        let Target::Node(m, node) = target else { return Ok(0) };
        let base = match whence {
            seek::SET => 0,
            seek::CURRENT => self.desc.borrow().offset,
            seek::END => self.fs.tree(m)?.stat(node).size,
            _ => return Err(FsError::Invalid),
        };
        let new = base.checked_add_signed(offset).ok_or(FsError::Invalid)?;
        if new > i64::MAX as u64 {
            return Err(FsError::Invalid);
        }
        self.desc.borrow_mut().offset = new;
        Ok(new)
    }

    fn stat(&mut self) -> Result<Stat, FsError> {
        match self.desc.borrow().target {
            Target::Node(m, node) => Ok(self.fs.tree(m)?.stat(node)),
            Target::Dev(d) => Ok(Dev::stat(Some(d))),
        }
    }

    fn truncate(&mut self, len: u64) -> Result<(), FsError> {
        self.writable()?;
        let d = self.desc.borrow();
        let (target, home) = (d.target, d.home);
        drop(d);
        match target {
            Target::Node(m, node) => self.fs.truncate_node(m, node, home, len),
            Target::Dev(_) => Ok(()),
        }
    }

    fn duplicate(&mut self) -> Result<Channel, FsError> {
        if self.file_conns + self.new_files.len() >= MAX_FILE_CONNS {
            return Err(FsError::TooMany);
        }
        let (ours, theirs) = Channel::create().map_err(|_| FsError::NoSpace)?;
        self.new_files.push((ours, self.desc.clone()));
        Ok(theirs)
    }

    fn flags(&mut self) -> u32 {
        self.desc.borrow().flags
    }

    fn set_append(&mut self, append: bool) -> Result<(), FsError> {
        let mut d = self.desc.borrow_mut();
        if append {
            d.flags |= open_flags::APPEND;
        } else {
            d.flags &= !open_flags::APPEND;
        }
        Ok(())
    }

    fn set_modified(&mut self, modified: u64) -> Result<(), FsError> {
        let Target::Node(m, node) = self.desc.borrow().target else { return Ok(()) };
        let tree = self.fs.tree_mut(m)?;
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        if modified == 0 {
            tree.touch(node);
        } else {
            tree.set_modified(node, modified);
        }
        self.fs.changed();
        Ok(())
    }
}

/// A writable tree with `/tmp` and an empty `/home`.
fn fresh_ram() -> Tree {
    let mut ram = Tree::new(false, device::RAM);
    let _ = ram.mkdir_all(&["tmp"]);
    let _ = ram.mkdir_all(&["home", "user"]);
    ram
}

/// Builds the file systems: the system image from the initrd, and the home
/// directory from the home disk or, failing that, the samples. A `live`
/// system keeps the home directory in memory without looking for a disk.
fn build(archive: &initrd::Archive<'static>, live: bool) -> Fs {
    let mut system = Tree::new(true, device::SYSTEM);
    for f in archive.files() {
        let comps = tree::components(f.path).unwrap_or_default();
        let _ = system.add_file(&comps, Data::Static(f.data));
    }
    let samples = Samples::new(archive);
    let mut store = if live { None } else { Store::connect() };
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
        (None, _) if live => "in memory (live system)",
        (None, _) => "in memory (no home disk)",
        (Some(_), true) => "restored from the home disk",
        (Some(_), false) => "new, on the home disk",
    };
    println!("home directory {}", home);
    Fs { system, ram, samples, store, dirty: None, open: BTreeMap::new(), orphans: BTreeSet::new() }
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
    let mut fs = build(&archive, vrt::env::args().iter().any(|a| a == "live"));

    let Ok(listener) = vproto::register(vfs::NAME) else {
        println!("cannot register the vfs service");
        return 1;
    };
    println!("serving /system ({} files), /dev and /home", archive.len());

    struct Client {
        channel: Channel,
        fds: BTreeMap<u32, OpenFile>,
        who: ClientIdentity,
    }
    struct FileConn {
        channel: Channel,
        desc: Shared,
    }
    const FILE_KEY: u64 = 1 << 62;
    let mut clients: BTreeMap<u64, Client> = BTreeMap::new();
    let mut files: BTreeMap<u64, FileConn> = BTreeMap::new();
    let mut next_id = 1u64;
    let mut new_files = NewFiles::new();
    loop {
        let mut ws = WaitSet::new();
        ws.add(listener.raw(), signals::READABLE | signals::PEER_CLOSED, 0);
        for (&id, c) in &clients {
            ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, id);
        }
        for (&id, f) in &files {
            ws.add(f.channel.raw(), signals::READABLE | signals::PEER_CLOSED, FILE_KEY | id);
        }
        let Ok(ready) = ws.wait(fs.save_deadline()) else { continue };
        for (key, observed) in ready {
            if key == 0 {
                while let Some((ch, who)) = vproto::accept_with_identity(&listener) {
                    clients.insert(next_id, Client { channel: ch, fds: BTreeMap::new(), who });
                    next_id += 1;
                }
                continue;
            }
            let file_conns = files.len();
            if key & FILE_KEY != 0 {
                let id = key & !FILE_KEY;
                let Some(conn) = files.get(&id) else { continue };
                if observed & signals::READABLE != 0 {
                    while let Ok(msg) = conn.channel.read() {
                        let mut session =
                            FileSession { fs: &mut fs, desc: &conn.desc, new_files: &mut new_files, file_conns };
                        match file::dispatch(&mut session, msg) {
                            Ok(reply) => {
                                let _ = reply.send(&conn.channel);
                            }
                            Err(e) => println!("bad file request: {}", e),
                        }
                    }
                } else if observed & signals::PEER_CLOSED != 0 {
                    let conn = files.remove(&id).unwrap();
                    // The last connection closes the open file.
                    if Rc::strong_count(&conn.desc) == 1
                        && let Target::Node(m, node) = conn.desc.borrow().target
                    {
                        fs.release(m, node);
                    }
                }
            } else {
                let Some(client) = clients.get_mut(&key) else { continue };
                if observed & signals::READABLE != 0 {
                    while let Ok(msg) = client.channel.read() {
                        let mut session = Session {
                            fs: &mut fs,
                            fds: &mut client.fds,
                            who: &client.who,
                            new_files: &mut new_files,
                            file_conns,
                        };
                        match vfs::dispatch(&mut session, msg) {
                            Ok(reply) => {
                                let _ = reply.send(&client.channel);
                            }
                            Err(e) => println!("bad request: {}", e),
                        }
                    }
                } else if observed & signals::PEER_CLOSED != 0 {
                    let client = clients.remove(&key).unwrap();
                    for f in client.fds.values() {
                        fs.release(f.mount, f.node);
                    }
                }
            }
            for (channel, desc) in new_files.drain(..) {
                files.insert(next_id, FileConn { channel, desc });
                next_id += 1;
            }
        }
        if vrt::time::now_ns() >= fs.save_deadline() {
            let _ = fs.save();
        }
    }
}
