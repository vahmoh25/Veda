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
    pub fn new(read_only: bool) -> Tree {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node { parent: ROOT, kind: Kind::Dir(BTreeMap::new()), modified: 0 });
        Tree { nodes, next: ROOT + 1, read_only }
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
        if self.read_only {
            return Err(FsError::ReadOnly);
        }
        let (name, dirs) = comps.split_last().ok_or(FsError::Exists)?;
        let parent = self.lookup(dirs)?;
        let kind = if dir { Kind::Dir(BTreeMap::new()) } else { Kind::File(Data::Owned(Vec::new())) };
        self.insert(parent, name, kind)
    }

    pub fn stat(&self, id: NodeId) -> Stat {
        let n = &self.nodes[&id];
        match &n.kind {
            Kind::File(d) => {
                Stat { size: d.bytes().len() as u64, is_dir: false, read_only: self.read_only, modified: n.modified }
            }
            Kind::Dir(c) => {
                Stat { size: c.len() as u64, is_dir: true, read_only: self.read_only, modified: n.modified }
            }
        }
    }

    pub fn list(&self, id: NodeId) -> Result<Vec<DirEntry>, FsError> {
        let Kind::Dir(children) = &self.nodes[&id].kind else { return Err(FsError::NotDir) };
        Ok(children
            .iter()
            .map(|(name, &child)| {
                let s = self.stat(child);
                DirEntry { name: name.clone(), is_dir: s.is_dir, size: s.size, modified: s.modified }
            })
            .collect())
    }

    pub fn remove(&mut self, comps: &[&str]) -> Result<(), FsError> {
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
        self.nodes.remove(&id);
        Ok(())
    }

    pub fn rename(&mut self, from: &[&str], to: &[&str]) -> Result<(), FsError> {
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
        if let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get(&to_parent) {
            if c.contains_key(*to_name) {
                return Err(FsError::Exists);
            }
        } else {
            return Err(FsError::NotDir);
        }
        if let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get_mut(&from_parent) {
            c.remove(*from_name);
        }
        if let Some(Node { kind: Kind::Dir(c), .. }) = self.nodes.get_mut(&to_parent) {
            c.insert(to_name.to_string(), id);
        }
        if let Some(n) = self.nodes.get_mut(&id) {
            n.parent = to_parent;
        }
        Ok(())
    }

    pub fn touch(&mut self, id: NodeId) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.modified = Self::now();
        }
    }
}
