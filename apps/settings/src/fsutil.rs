//! The few file system operations Settings needs.

use alloc::string::String;
use alloc::vec::Vec;

use vproto::fs::{DirEntry, FsError, Stat, vfs};

/// A connection to the VFS.
pub struct Fs {
    client: Option<vfs::Client>,
}

fn flat<T>(r: Result<Result<T, FsError>, vipc::IpcError>) -> Option<T> {
    r.ok()?.ok()
}

impl Fs {
    pub fn connect() -> Fs {
        Fs { client: vproto::connect(vfs::NAME).ok().map(vfs::Client::new) }
    }

    pub fn stat(&self, path: &str) -> Option<Stat> {
        flat(self.client.as_ref()?.stat(path.into()))
    }

    pub fn read_dir(&self, path: &str) -> Vec<DirEntry> {
        self.client.as_ref().and_then(|c| flat(c.read_dir(path.into()))).unwrap_or_default()
    }

    /// Reads a whole file.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let (vmo, len) = flat(self.client.as_ref()?.read_file(path.into()))?;
        let mut buf = alloc::vec![0u8; len as usize];
        if len > 0 {
            vmo.read(0, &mut buf).ok()?;
        }
        Some(buf)
    }

    /// Image files (by extension) in a folder, sorted by name.
    pub fn images_in(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .read_dir(dir)
            .into_iter()
            .filter(|e| !e.is_dir && is_image(&e.name))
            .map(|e| alloc::format!("{}/{}", dir.trim_end_matches('/'), e.name))
            .collect();
        v.sort();
        v
    }
}

/// True for the image formats `vimage` decodes.
pub fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".bmp", ".qoi"].iter().any(|e| lower.ends_with(e))
}

/// A readable title from a file path: "/x/misty-forest.jpg" -> "Misty forest".
pub fn title_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    let mut out = String::new();
    for (i, c) in stem.chars().enumerate() {
        let c = if c == '-' || c == '_' { ' ' } else { c };
        if i == 0 { out.extend(c.to_uppercase()) } else { out.push(c) }
    }
    out
}
