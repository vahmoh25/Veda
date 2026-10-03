//! `vfs` — the Vindows file system service.
//!
//! Namespace:
//! * `/system` — the read-only system image (the initrd), and
//! * everything else — a writable in-memory file system that starts with the
//!   user's home directory, populated from `samples/` in the system image.
//!
//! Each client connection has its own file descriptor table.

#![no_std]
#![no_main]

extern crate alloc;

mod tree;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use vabi::signals;
use vipc::{Bytes, WaitSet};
use vproto::fs::{DirEntry, FsError, MAX_IO, Stat, open_flags, vfs};
use vrt::object::{Channel, Vmo};
use vrt::println;

use tree::{Data, Kind, NodeId, Tree};

vrt::entry!(main);

const SYSTEM: &str = "system";

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
}

struct Fs {
    system: Tree,
    ram: Tree,
}

impl Fs {
    /// Resolves a path to a tree and the components inside it.
    fn route<'p>(&self, path: &'p str) -> Result<(Mount, Vec<&'p str>), FsError> {
        let comps = tree::components(path)?;
        if comps.first() == Some(&SYSTEM) {
            Ok((Mount::System, comps[1..].to_vec()))
        } else {
            Ok((Mount::Ram, comps))
        }
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
        let tree = self.fs.tree_mut(m);
        let writing = flags & (open_flags::WRITE | open_flags::CREATE | open_flags::TRUNCATE | open_flags::APPEND) != 0;
        if writing && tree.read_only {
            return Err(FsError::ReadOnly);
        }
        let node = match tree.lookup(&comps) {
            Ok(n) => n,
            Err(FsError::NotFound) if flags & open_flags::CREATE != 0 => tree.create(&comps, false)?,
            Err(e) => return Err(e),
        };
        match &mut tree.node_mut(node).unwrap().kind {
            Kind::Dir(_) => return Err(FsError::IsDir),
            Kind::File(d) if flags & open_flags::TRUNCATE != 0 => {
                *d = Data::Owned(Vec::new());
                tree.touch(node);
            }
            Kind::File(_) => {}
        }
        if self.fds.len() >= MAX_FDS {
            return Err(FsError::TooMany);
        }
        let fd = (1..).find(|fd| !self.fds.contains_key(fd)).unwrap();
        self.fds.insert(fd, OpenFile { mount: m, node, flags });
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
        let (m, node, append) = (f.mount, f.node, f.flags & open_flags::APPEND != 0);
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
        self.fs.tree_mut(m).create(&comps, true).map(|_| ())
    }

    fn remove(&mut self, path: String) -> Result<(), FsError> {
        let (m, comps) = self.fs.route(&path)?;
        self.fs.tree_mut(m).remove(&comps)
    }

    fn rename(&mut self, from: String, to: String) -> Result<(), FsError> {
        let (m1, c1) = self.fs.route(&from)?;
        let (m2, c2) = self.fs.route(&to)?;
        if m1 != m2 {
            return Err(FsError::Invalid);
        }
        self.fs.tree_mut(m1).rename(&c1, &c2)
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
        let mut buf = alloc::vec![0u8; len as usize];
        data.read(0, &mut buf).map_err(|_| FsError::Invalid)?;
        let tree = self.fs.tree_mut(m);
        let node = match tree.lookup(&comps) {
            Ok(n) => n,
            Err(FsError::NotFound) => tree.create(&comps, false)?,
            Err(e) => return Err(e),
        };
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        match &mut tree.node_mut(node).unwrap().kind {
            Kind::File(d) => *d = Data::Owned(buf),
            Kind::Dir(_) => return Err(FsError::IsDir),
        }
        tree.touch(node);
        Ok(())
    }

    fn truncate(&mut self, fd: u32, len: u64) -> Result<(), FsError> {
        let f = self.fds.get(&fd).ok_or(FsError::BadFd)?;
        let (m, node) = (f.mount, f.node);
        let tree = self.fs.tree_mut(m);
        if tree.read_only {
            return Err(FsError::ReadOnly);
        }
        let Some(tree::Node { kind: Kind::File(d), .. }) = tree.node_mut(node) else { return Err(FsError::BadFd) };
        d.make_mut().resize(len.min(1 << 31) as usize, 0);
        Ok(())
    }
}

/// Builds the file system trees from the initrd.
fn build(archive: &initrd::Archive<'static>) -> Fs {
    let mut system = Tree::new(true);
    let mut ram = Tree::new(false);
    for f in archive.files() {
        let comps = tree::components(f.path).unwrap_or_default();
        let _ = system.add_file(&comps, Data::Static(f.data));
        // Sample documents, pictures and music seed the user's home.
        if let Some(rest) = f.path.strip_prefix("samples/") {
            let mut home = alloc::vec!["home", "user"];
            home.extend(tree::components(rest).unwrap_or_default());
            let _ = ram.add_file(&home, Data::Static(f.data));
        }
    }
    for dir in ["home/user/Documents", "home/user/Pictures", "home/user/Music", "home/user/Desktop", "tmp"] {
        let comps = tree::components(dir).unwrap_or_default();
        let _ = ram.mkdir_all(&comps);
    }
    Fs { system, ram }
}

fn main() -> i32 {
    let Some(initrd_vmo) = vrt::env::take_handle(vabi::startup::role::INITRD).map(Vmo::from_handle) else {
        println!("vfs: no initrd handle");
        return 1;
    };
    let size = initrd_vmo.size().unwrap_or(0);
    let Ok(addr) = initrd_vmo.map(0, size, vabi::map_flags::READ) else {
        println!("vfs: cannot map the initrd");
        return 1;
    };
    // SAFETY: the mapping is read-only and lives as long as the process.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    let archive = initrd::Archive::open(bytes).expect("vfs: corrupt initrd");
    let mut fs = build(&archive);

    let listener = vproto::register(vfs::NAME).expect("vfs: cannot register");
    println!("vfs: serving /system ({} files) and /home", archive.len());

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
        let Ok(ready) = ws.wait(vabi::DEADLINE_INFINITE) else { continue };
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
                        Err(e) => println!("vfs: bad request: {}", e),
                    }
                }
            } else if observed & signals::PEER_CLOSED != 0 {
                clients.remove(&key);
            }
        }
    }
}
