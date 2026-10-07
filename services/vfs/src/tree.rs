//! An in-memory hierarchical file system.
//!
//! The same structure backs both the read-only system image (built from the
//! initrd) and the writable RAM file system. File contents are either owned
//! bytes or a borrowed slice of the initrd (copied on first write), which
//! makes populating the home directory with sample files free.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vproto::fs::{DirEntry, FsError, Stat};

pub type NodeId = u64;
pub const ROOT: NodeId = 1;

pub enum Data {
    Owned(Vec<u8>),
    Static(&'static [u8]),
}

impl Data {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Data::Owned(v) => v,
            Data::Static(s) => s,
        }
    }

    /// Converts borrowed contents into owned ones before a write.
    pub fn make_mut(&mut self) -> &mut Vec<u8> {
        if let Data::Static(s) = self {
            *self = Data::Owned(s.to_vec());
        }
        match self {
            Data::Owned(v) => v,
            Data::Static(_) => unreachable!(),
        }
    }
}

pub enum Kind {
    File(Data),
    Dir(BTreeMap<String, NodeId>),
}

pub struct Node {
    pub parent: NodeId,
    pub kind: Kind,
    pub modified: u64,
}

pub struct Tree {
    nodes: BTreeMap<NodeId, Node>,
    next: NodeId,
    pub read_only: bool,
    /// The `Stat::device` of its files.
    device: u64,
}

/// Splits a path into normalised components (rejecting `..` escapes).
pub fn components(path: &str) -> Result<Vec<&str>, FsError> {
    let mut out: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c if c.len() > 255 => return Err(FsError::Invalid),
            c => out.push(c),
        }
    }
    Ok(out)
}

impl Tree {
    pub fn new(read_only: bool, device: u64) -> Tree {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node { parent: ROOT, kind: Kind::Dir(BTreeMap::new()), modified: 0 });
        Tree { nodes, next: ROOT + 1, read_only, device }
    }

    fn now() -> u64 {
        vrt::time::unix_time_ns()
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id)
    }

    pub fn lookup(&self, comps: &[&str]) -> Result<NodeId, FsError> {
        let mut cur = ROOT;
        for c in comps {
            match &self.nodes[&cur].kind {
                Kind::Dir(children) => cur = *children.get(*c).ok_or(FsError::NotFound)?,
                Kind::File(_) => return Err(FsError::NotDir),
            }
        }
        Ok(cur)
    }

    fn insert(&mut self, parent: NodeId, name: &str, kind: Kind) -> Result<NodeId, FsError> {
        let id = self.next;
        let Some(Node { kind: Kind::Dir(children), .. }) = self.nodes.get_mut(&parent) else {
            return Err(FsError::NotDir);
        };
        if children.contains_key(name) {
            return Err(FsError::Exists);
        }
        children.insert(name.to_string(), id);
        self.next += 1;
        self.nodes.insert(id, Node { parent, kind, modified: Self::now() });
        Ok(id)
    }

    /// Creates every missing directory along `comps`; returns the last one.
    pub fn mkdir_all(&mut self, comps: &[&str]) -> Result<NodeId, FsError> {
        let mut cur = ROOT;
        for c in comps {
            let next = match &self.nodes[&cur].kind {
                Kind::Dir(children) => children.get(*c).copied(),
                Kind::File(_) => return Err(FsError::NotDir),
            };
            cur = match next {
                Some(id) => id,
                None => self.insert(cur, c, Kind::Dir(BTreeMap::new()))?,
            };
        }
        Ok(cur)
    }

    /// Adds a file, creating parent directories (used when populating).
    pub fn add_file(&mut self, comps: &[&str], data: Data) -> Result<NodeId, FsError> {
        let (name, dirs) = comps.split_last().ok_or(FsError::Invalid)?;
        let parent = self.mkdir_all(dirs)?;
        self.insert(parent, name, Kind::File(data))
    }

    pub fn create(&mut self, comps: &[&str], dir: bool) -> Result<NodeId, FsError> {
        // What is there already is reported as such, even where nothing
        // can be made (as POSIX file systems do: `mkdir -p` relies on it).
        if self.lookup(comps).is_ok() {
            return Err(FsError::Exists);
        }
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let (name, dirs) = comps.split_last().ok_or(FsError::Exists)?;
        let parent = self.lookup(dirs)?;
        let kind = if dir { Kind::Dir(BTreeMap::new()) } else { Kind::File(Data::Owned(Vec::new())) };
        self.insert(parent, name, kind)
    }

    /// Whether file contents are a program: an ELF or PE executable, or a
    /// script that names its interpreter.
    pub fn is_program(bytes: &[u8]) -> bool {
        bytes.starts_with(b"\x7fELF") || bytes.starts_with(b"MZ") || bytes.starts_with(b"#!")
    }

    pub fn stat(&self, id: NodeId) -> Stat {
        let n = &self.nodes[&id];
        let (size, is_dir, executable) = match &n.kind {
            Kind::File(d) => (d.bytes().len() as u64, false, Self::is_program(d.bytes())),
            Kind::Dir(c) => (c.len() as u64, true, false),
        };
        Stat {
            size,
            is_dir,
            read_only: self.read_only,
            modified: n.modified,
            inode: id,
            device: self.device,
            executable,
            char_device: false,
        }
    }

    /// The bytes of all files under `id` (itself included) and the number
    /// of nodes there.
    pub fn subtree_size(&self, id: NodeId) -> (usize, usize) {
        match self.nodes.get(&id).map(|n| &n.kind) {
            Some(Kind::File(d)) => (d.bytes().len(), 1),
            Some(Kind::Dir(children)) => children.values().fold((0, 1), |(bytes, nodes), &child| {
                let (b, n) = self.subtree_size(child);
                (bytes + b, nodes + n)
            }),
            None => (0, 0),
        }
    }

    pub fn list(&self, id: NodeId) -> Result<Vec<DirEntry>, FsError> {
        let Kind::Dir(children) = &self.nodes[&id].kind else { return Err(FsError::NotDir) };
        Ok(children
            .iter()
            .map(|(name, &child)| {
                let s = self.stat(child);
                DirEntry { name: name.clone(), is_dir: s.is_dir, size: s.size, modified: s.modified, inode: child }
            })
            .collect())
    }

    /// Takes `comps` out of its directory (a directory must be empty) and
    /// returns its node. The node stays readable until it is
    /// [forgotten](Self::forget): POSIX keeps an unlinked file alive while
    /// it is open.
    pub fn unlink(&mut self, comps: &[&str]) -> Result<NodeId, FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let (name, dirs) = comps.split_last().ok_or(FsError::Invalid)?;
        let parent = self.lookup(dirs)?;
        let id = self.lookup(comps)?;
        if let Kind::Dir(c) = &self.nodes[&id].kind
            && !c.is_empty()
        {
            return Err(FsError::NotEmpty);
        }
        if let Some(Node { kind: Kind::Dir(children), .. }) = self.nodes.get_mut(&parent) {
            children.remove(*name);
        }
        Ok(id)
    }

    /// Deletes a node [unlinked](Self::unlink) before.
    pub fn forget(&mut self, id: NodeId) {
        if id != ROOT {
            self.nodes.remove(&id);
        }
    }

    /// Moves `from` to `to`. If `to` exists, the move fails with `Exists`
    /// unless `replace` is set: then `to` (a file, when `from` is a file; an
    /// empty directory, when `from` is a directory) is unlinked first and
    /// returned, to be [forgotten](Self::forget) once no longer open.
    /// Either everything happens or nothing does.
    pub fn rename(&mut self, from: &[&str], to: &[&str], replace: bool) -> Result<Option<NodeId>, FsError> {
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let id = self.lookup(from)?;
        let (from_name, from_dirs) = from.split_last().ok_or(FsError::Invalid)?;
        let (to_name, to_dirs) = to.split_last().ok_or(FsError::Invalid)?;
        let from_parent = self.lookup(from_dirs)?;
        let to_parent = self.lookup(to_dirs)?;
        // Refuse to move a directory into itself.
        let mut p = to_parent;
        loop {
            if p == id {
                return Err(FsError::Invalid);
            }
            if p == ROOT {
                break;
            }
            p = self.nodes[&p].parent;
        }
        let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get(&to_parent) else { return Err(FsError::NotDir) };
        let replaced = match c.get(*to_name).copied() {
            None => None,
            // A name for itself: nothing to do.
            Some(old) if old == id => return Ok(None),
            Some(_) if !replace => return Err(FsError::Exists),
            Some(old) => {
                match (&self.nodes[&id].kind, &self.nodes[&old].kind) {
                    (Kind::File(_), Kind::Dir(_)) => return Err(FsError::IsDir),
                    (Kind::Dir(_), Kind::File(_)) => return Err(FsError::NotDir),
                    (Kind::Dir(_), Kind::Dir(c)) if !c.is_empty() => return Err(FsError::NotEmpty),
                    _ => {}
                }
                Some(old)
            }
        };
        if let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get_mut(&from_parent) {
            c.remove(*from_name);
        }
        if let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get_mut(&to_parent) {
            c.insert(to_name.to_string(), id);
        }
        if let Some(n) = self.nodes.get_mut(&id) {
            n.parent = to_parent;
        }
        Ok(replaced)
    }

    pub fn touch(&mut self, id: NodeId) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.modified = Self::now();
        }
    }

    /// Sets a node's modification time (nanoseconds since the Unix epoch).
    pub fn set_modified(&mut self, id: NodeId, modified: u64) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.modified = modified;
        }
    }
}
