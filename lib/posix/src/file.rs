//! Files and directories of the VFS behind descriptors.

use alloc::string::String;
use alloc::vec::Vec;

use vipc::Bytes;
use vproto::fs::{MAX_IO, Stat, file, open_flags, seek};
use vrt::object::Channel;
use vrt::sync::Mutex;

use crate::error;
use crate::linux::errno::{EINVAL, EIO};
use crate::linux::{self, dt};
use crate::path;
use crate::vfs;

/// An open file: a connection of the VFS's `file` protocol. The VFS keeps
/// the offset, shared with every process the file was passed to.
pub struct VfsFile {
    client: Mutex<file::Client>,
}

impl VfsFile {
    pub fn new(channel: Channel) -> VfsFile {
        VfsFile { client: Mutex::new(file::Client::new(channel)) }
    }

    /// Reads at the offset until `buf` is full or the file ends.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, isize> {
        let c = self.client.lock();
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_IO as usize);
            let Bytes(data) = error::ipc(c.read(want as u32))?;
            buf[done..done + data.len()].copy_from_slice(&data);
            done += data.len();
            if data.len() < want {
                break;
            }
        }
        Ok(done)
    }

    /// Writes all of `data` at the offset (at the end in append mode).
    pub fn write(&self, data: &[u8]) -> Result<usize, isize> {
        let c = self.client.lock();
        for chunk in data.chunks(MAX_IO as usize) {
            error::ipc(c.write(Bytes(chunk.to_vec())))?;
        }
        Ok(data.len())
    }

    pub fn pread(&self, buf: &mut [u8], offset: u64) -> Result<usize, isize> {
        let c = self.client.lock();
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_IO as usize);
            let Bytes(data) = error::ipc(c.read_at(offset + done as u64, want as u32))?;
            buf[done..done + data.len()].copy_from_slice(&data);
            done += data.len();
            if data.len() < want {
                break;
            }
        }
        Ok(done)
    }

    pub fn pwrite(&self, data: &[u8], offset: u64) -> Result<usize, isize> {
        let c = self.client.lock();
        for (i, chunk) in data.chunks(MAX_IO as usize).enumerate() {
            error::ipc(c.write_at(offset + (i * MAX_IO as usize) as u64, Bytes(chunk.to_vec())))?;
        }
        Ok(data.len())
    }

    pub fn seek(&self, offset: i64, whence: u32) -> Result<u64, isize> {
        let whence = match whence {
            linux::seek::SET => seek::SET,
            linux::seek::CUR => seek::CURRENT,
            linux::seek::END => seek::END,
            _ => return Err(EINVAL),
        };
        error::ipc(self.client.lock().seek(offset, whence))
    }

    pub fn stat(&self) -> Result<Stat, isize> {
        error::ipc(self.client.lock().stat())
    }

    pub fn truncate(&self, len: u64) -> Result<(), isize> {
        error::ipc(self.client.lock().truncate(len))
    }

    pub fn set_append(&self, on: bool) -> Result<(), isize> {
        error::ipc(self.client.lock().set_append(on))
    }

    /// Sets the modification time (nanoseconds since the Unix epoch; 0:
    /// now).
    pub fn set_modified(&self, modified: u64) -> Result<(), isize> {
        error::ipc(self.client.lock().set_modified(modified))
    }

    /// The `open_flags` the file was opened with.
    pub fn open_flags(&self) -> Result<u32, isize> {
        self.client.lock().flags().map_err(|_| EIO)
    }

    /// Another connection to this open file (for another process).
    pub fn duplicate(&self) -> Result<Channel, isize> {
        error::ipc(self.client.lock().duplicate())
    }
}

/// The POSIX access mode for VFS open flags.
pub fn access_mode(flags: u32) -> u32 {
    match (flags & open_flags::READ != 0, flags & open_flags::WRITE != 0) {
        (true, true) => linux::o::RDWR,
        (false, true) => linux::o::WRONLY,
        _ => linux::o::RDONLY,
    }
}

/// An entry of a directory listing.
struct Entry {
    inode: u64,
    kind: u8,
    name: String,
}

/// An open directory, read with `getdents64`.
pub struct Directory {
    pub path: String,
    /// The listing (taken when first read, again after a rewind) and the
    /// position in it.
    state: Mutex<(Option<Vec<Entry>>, usize)>,
}

impl Directory {
    pub fn new(path: String) -> Directory {
        Directory { path, state: Mutex::new((None, 0)) }
    }

    fn list(&self) -> Result<Vec<Entry>, isize> {
        let entries = error::ipc(vfs::with(|c| c.read_dir(self.path.clone()))?)?;
        let here = vfs::stat(&self.path)?.inode;
        let up = vfs::stat(path::parent(&self.path)).map(|s| s.inode).unwrap_or(here);
        let mut out = Vec::with_capacity(entries.len() + 2);
        out.push(Entry { inode: here, kind: dt::DIR, name: ".".into() });
        out.push(Entry { inode: up, kind: dt::DIR, name: "..".into() });
        for e in entries {
            let kind = if e.is_dir {
                dt::DIR
            } else if self.path == "/dev" {
                dt::CHR
            } else {
                dt::REG
            };
            out.push(Entry { inode: e.inode.max(1), kind, name: e.name });
        }
        Ok(out)
    }

    /// Fills `buf` with `struct linux_dirent64` records; 0 at the end.
    pub fn getdents64(&self, buf: &mut [u8]) -> Result<usize, isize> {
        let mut st = self.state.lock();
        if st.0.is_none() {
            st.0 = Some(self.list()?);
        }
        let (entries, pos) = &mut *st;
        let entries = entries.as_ref().unwrap();
        let mut used = 0;
        while let Some(e) = entries.get(*pos) {
            let len = record_len(e.name.len());
            if used + len > buf.len() {
                if used == 0 {
                    return Err(EINVAL);
                }
                break;
            }
            let r = &mut buf[used..used + len];
            r[..8].copy_from_slice(&e.inode.to_le_bytes());
            r[8..16].copy_from_slice(&((*pos + 1) as i64).to_le_bytes());
            r[16..18].copy_from_slice(&(len as u16).to_le_bytes());
            r[18] = e.kind;
            r[19..19 + e.name.len()].copy_from_slice(e.name.as_bytes());
            r[19 + e.name.len()..].fill(0);
            used += len;
            *pos += 1;
        }
        Ok(used)
    }

    /// `lseek` on a directory: positions are the `d_off` cookies; going
    /// back to the start lists the directory afresh.
    pub fn seek(&self, offset: i64, whence: u32) -> Result<u64, isize> {
        let mut st = self.state.lock();
        let new = match whence {
            linux::seek::SET => offset,
            linux::seek::CUR => st.1 as i64 + offset,
            _ => return Err(EINVAL),
        };
        let new = usize::try_from(new).map_err(|_| EINVAL)?;
        if new == 0 {
            st.0 = None;
        }
        st.1 = new;
        Ok(new as u64)
    }
}

/// The size of a `linux_dirent64` record with a name of `len` bytes.
fn record_len(len: usize) -> usize {
    (19 + len + 1).next_multiple_of(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sizes() {
        assert_eq!(record_len(1), 24);
        assert_eq!(record_len(4), 24);
        assert_eq!(record_len(5), 32);
        assert_eq!(record_len(255), 280);
    }

    #[test]
    fn access_modes() {
        assert_eq!(access_mode(open_flags::READ), linux::o::RDONLY);
        assert_eq!(access_mode(open_flags::WRITE | open_flags::CREATE), linux::o::WRONLY);
        assert_eq!(access_mode(open_flags::WRITE | open_flags::APPEND), linux::o::WRONLY);
        // Appending is not writing: O_RDONLY | O_APPEND reads only.
        assert_eq!(access_mode(open_flags::READ | open_flags::APPEND), linux::o::RDONLY);
    }
}
