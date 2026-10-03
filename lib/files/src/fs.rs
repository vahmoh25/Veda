//! A convenient client of the VFS service: whole-file reads and writes,
//! appends, recursive copies, moves and removals, and how full a file
//! system is.
//!
//! [`Fs`] wraps `vproto::vfs::Client` and flattens its two error layers
//! (IPC and file system) into one [`Error`].

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use vipc::Bytes;
use vproto::fs::{MAX_IO, open_flags, vfs};
use vrt::object::Vmo;

pub use vproto::fs::{DirEntry, FsError, Space, Stat};

use crate::path::{is_within, join, same_volume, unique_name};

/// A file system operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The VFS refused the operation.
    Fs(FsError),
    /// The VFS stopped answering.
    Ipc,
    /// There is no VFS service.
    Unavailable,
    /// A folder cannot be copied or moved into itself.
    IntoItself,
}

impl Error {
    /// True if the operation failed because the file system is full.
    pub fn is_no_space(&self) -> bool {
        *self == Error::Fs(FsError::NoSpace)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fs(FsError::NoSpace) => f.write_str("there is not enough free space"),
            Error::Fs(e) => write!(f, "{e}"),
            Error::Ipc => f.write_str("the file system service is not responding"),
            Error::Unavailable => f.write_str("the file system service is unavailable"),
            Error::IntoItself => f.write_str("a folder cannot be copied or moved into itself"),
        }
    }
}

impl From<FsError> for Error {
    fn from(e: FsError) -> Self {
        Error::Fs(e)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// Flattens an IPC result carrying a file system result.
fn flat<T>(r: core::result::Result<core::result::Result<T, FsError>, vipc::IpcError>) -> Result<T> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Error::Fs(e)),
        Err(_) => Err(Error::Ipc),
    }
}

/// A connection to the VFS. Every call fails with [`Error::Unavailable`] if
/// the service could not be reached.
pub struct Fs {
    client: Option<vfs::Client>,
}

impl Fs {
    /// Connects to the VFS service.
    pub fn connect() -> Fs {
        Fs { client: vproto::connect(vfs::NAME).ok().map(vfs::Client::new) }
    }

    /// The underlying protocol client, if connected.
    pub fn client(&self) -> Option<&vfs::Client> {
        self.client.as_ref()
    }

    fn c(&self) -> Result<&vfs::Client> {
        self.client.as_ref().ok_or(Error::Unavailable)
    }

    pub fn stat(&self, path: &str) -> Result<Stat> {
        flat(self.c()?.stat(path.into()))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.stat(path).is_ok()
    }

    pub fn is_dir(&self, path: &str) -> bool {
        self.stat(path).is_ok_and(|s| s.is_dir)
    }

    /// The entries of a directory (folders first, then by name).
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        flat(self.c()?.read_dir(path.into()))
    }

    /// How full the file system holding `path` is.
    pub fn space(&self, path: &str) -> Result<Space> {
        flat(self.c()?.space(path.into()))
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        flat(self.c()?.mkdir(path.into()))
    }

    /// Creates `path` and every missing parent.
    pub fn mkdir_all(&self, path: &str) -> Result<()> {
        let mut cur = String::new();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur.push('/');
            cur.push_str(comp);
            match self.stat(&cur) {
                Ok(s) if s.is_dir => {}
                Ok(_) => return Err(Error::Fs(FsError::NotDir)),
                Err(Error::Fs(FsError::NotFound)) => self.mkdir(&cur)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Removes a file or an empty directory.
    pub fn remove(&self, path: &str) -> Result<()> {
        flat(self.c()?.remove(path.into()))
    }

    /// Removes a file, or a directory with everything in it.
    pub fn remove_all(&self, path: &str) -> Result<()> {
        if self.stat(path)?.is_dir {
            for e in self.read_dir(path)? {
                self.remove_all(&join(path, &e.name))?;
            }
        }
        self.remove(path)
    }

    /// Renames or moves within one file system.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        flat(self.c()?.rename(from.into(), to.into()))
    }

    /// Reads a whole file.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let (vmo, len) = flat(self.c()?.read_file(path.into()))?;
        let mut buf = alloc::vec![0u8; len as usize];
        if len > 0 {
            vmo.read(0, &mut buf).map_err(|_| Error::Fs(FsError::Io))?;
        }
        Ok(buf)
    }

    /// Replaces (or creates) a file with `data`.
    pub fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let vmo = Vmo::create(data.len().max(1)).map_err(|_| Error::Fs(FsError::NoSpace))?;
        if !data.is_empty() {
            vmo.write(0, data).map_err(|_| Error::Fs(FsError::Io))?;
        }
        flat(self.c()?.write_file(path.into(), vmo, data.len() as u64))
    }

    /// Appends `data` to a file, creating it if needed.
    pub fn append(&self, path: &str, data: &[u8]) -> Result<()> {
        let c = self.c()?;
        let fd = flat(c.open(path.into(), open_flags::WRITE | open_flags::CREATE | open_flags::APPEND))?;
        let mut result = Ok(());
        for chunk in data.chunks(MAX_IO as usize) {
            if let Err(e) = flat(c.write(fd, 0, Bytes(chunk.to_vec()))) {
                result = Err(e);
                break;
            }
        }
        let _ = c.close(fd);
        result
    }

    /// Creates an empty file, or updates the modification time of an
    /// existing one.
    pub fn touch(&self, path: &str) -> Result<()> {
        let c = self.c()?;
        let fd = flat(c.open(path.into(), open_flags::WRITE | open_flags::CREATE))?;
        let r = flat(c.write(fd, 0, Bytes(Vec::new()))).map(|_| ());
        let _ = c.close(fd);
        r
    }

    /// Writes all changes to persistent storage now.
    pub fn sync(&self) -> Result<()> {
        flat(self.c()?.sync())
    }

    /// Copies a file, or a directory with everything in it, to `to`, which
    /// must not exist yet.
    pub fn copy(&self, from: &str, to: &str) -> Result<()> {
        if is_within(to, from) {
            return Err(Error::IntoItself);
        }
        let st = self.stat(from)?;
        if self.exists(to) {
            return Err(Error::Fs(FsError::Exists));
        }
        if st.is_dir {
            self.mkdir(to)?;
            for e in self.read_dir(from)? {
                self.copy(&join(from, &e.name), &join(to, &e.name))?;
            }
            Ok(())
        } else {
            let data = self.read(from)?;
            self.write(to, &data)
        }
    }

    /// Moves `from` to `to`: a rename within one file system, a copy and a
    /// removal across file systems.
    pub fn move_to(&self, from: &str, to: &str) -> Result<()> {
        if is_within(to, from) {
            return Err(Error::IntoItself);
        }
        match self.rename(from, to) {
            Err(Error::Fs(FsError::Invalid)) if !same_volume(from, to) => {
                self.copy(from, to)?;
                self.remove_all(from)
            }
            r => r,
        }
    }

    /// A name for a new item in `dir` based on `base` that is not in use:
    /// `base`, else `stem (2).ext`, `stem (3).ext`, ...
    pub fn unique_name(&self, dir: &str, base: &str) -> String {
        unique_name(base, |name| self.exists(&join(dir, name)))
    }
}
