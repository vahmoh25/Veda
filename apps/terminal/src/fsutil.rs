//! File system helpers on top of the VFS protocol: path manipulation,
//! whole-file reads and writes, recursive copy and removal, and globbing.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use vipc::Bytes;
use vproto::fs::{DirEntry, FsError, MAX_IO, Stat, open_flags, vfs};
use vrt::object::Vmo;

/// The home directory.
pub const HOME: &str = "/home/user";

/// A file system failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The VFS reported an error.
    Fs(FsError),
    /// The VFS connection failed.
    Ipc,
    /// No VFS service.
    Unavailable,
    /// A copy or move into the item itself.
    IntoItself,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fs(e) => write!(f, "{e}"),
            Error::Ipc => f.write_str("the file system service is not responding"),
            Error::Unavailable => f.write_str("the file system service is unavailable"),
            Error::IntoItself => f.write_str("cannot copy or move a folder into itself"),
        }
    }
}

impl From<FsError> for Error {
    fn from(e: FsError) -> Self {
        Error::Fs(e)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

fn flat<T>(r: core::result::Result<core::result::Result<T, FsError>, vipc::IpcError>) -> Result<T> {
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Error::Fs(e)),
        Err(_) => Err(Error::Ipc),
    }
}

/// A connection to the VFS.
pub struct Fs {
    client: Option<vfs::Client>,
}

impl Fs {
    pub fn connect() -> Fs {
        Fs { client: vproto::connect(vfs::NAME).ok().map(vfs::Client::new) }
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

    /// Directory entries (directories first, then by name).
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        flat(self.c()?.read_dir(path.into()))
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        flat(self.c()?.mkdir(path.into()))
    }

    /// Creates `path` and all missing parents.
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

    /// Removes a file or a directory with everything in it.
    pub fn remove_all(&self, path: &str) -> Result<()> {
        let st = self.stat(path)?;
        if st.is_dir {
            for e in self.read_dir(path)? {
                self.remove_all(&join(path, &e.name))?;
            }
        }
        self.remove(path)
    }

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

    /// Replaces (or creates) a file.
    pub fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        let vmo = Vmo::create(data.len().max(1)).map_err(|_| Error::Fs(FsError::NoSpace))?;
        if !data.is_empty() {
            vmo.write(0, data).map_err(|_| Error::Fs(FsError::Io))?;
        }
        flat(self.c()?.write_file(path.into(), vmo, data.len() as u64))
    }

    /// Appends to a file, creating it if needed.
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

    /// Writes all changes to persistent storage now.
    pub fn sync(&self) -> Result<()> {
        flat(self.c()?.sync())
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

    /// Copies a file or (recursively) a directory to `to`, which must not
    /// exist yet.
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

    /// Moves `from` to `to` (renaming within a file system, copying and
    /// deleting across file systems).
    pub fn move_to(&self, from: &str, to: &str) -> Result<()> {
        if is_within(to, from) {
            return Err(Error::IntoItself);
        }
        match self.rename(from, to) {
            Err(Error::Fs(FsError::Invalid)) if mount_of(from) != mount_of(to) => {
                self.copy(from, to)?;
                self.remove_all(from)
            }
            r => r,
        }
    }
}

/// Which file system a path lives on (`/system` is the read-only image).
pub fn mount_of(path: &str) -> u8 {
    if path == "/system" || path.starts_with("/system/") { 1 } else { 0 }
}

/// True if `path` is inside the read-only system image.
pub fn is_read_only(path: &str) -> bool {
    mount_of(path) == 1
}

/// True if `path` equals `dir` or lies below it.
pub fn is_within(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && (dir == "/" || path.as_bytes().get(dir.len()) == Some(&b'/')))
}

/// Normalises an absolute path: collapses `.`, `..` and repeated slashes.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = String::new();
    for p in parts {
        out.push('/');
        out.push_str(p);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Resolves `path` relative to `cwd` (`~` means the home directory).
pub fn resolve(cwd: &str, path: &str) -> String {
    if path == "~" {
        return HOME.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return normalize(&format!("{HOME}/{rest}"));
    }
    if path.starts_with('/') { normalize(path) } else { normalize(&format!("{cwd}/{path}")) }
}

/// `dir/name`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The last component of `path`.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Lower-case extension of a file name (without the dot).
pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Shows `path` with the home directory abbreviated to `~`.
pub fn display_path(path: &str) -> String {
    if path == HOME {
        "~".to_string()
    } else if let Some(rest) = path.strip_prefix(HOME).filter(|r| r.starts_with('/')) {
        format!("~{rest}")
    } else {
        path.to_string()
    }
}

/// Matches `name` against a shell pattern with `*` and `?`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// True if a word contains glob wildcards.
pub fn has_wildcards(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// What kind of file a name looks like (for colours and default apps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Text,
    Image,
    Audio,
    Program,
    Other,
}

/// Classifies a file by its extension.
pub fn file_kind(name: &str) -> FileKind {
    match extension(name).as_str() {
        "txt" | "md" | "rs" | "cfg" | "toml" | "json" | "log" | "ini" | "conf" | "c" | "h" | "cpp" | "hpp" | "py"
        | "sh" | "app" | "csv" | "html" | "css" | "js" | "xml" | "yaml" | "yml" | "vts" | "s" | "asm" => FileKind::Text,
        "png" | "jpg" | "jpeg" | "bmp" | "qoi" => FileKind::Image,
        "wav" | "qoa" | "mp3" | "ogg" | "flac" | "mid" | "midi" | "mod" | "xm" | "s3m" | "it" | "opus" | "aac"
        | "m4a" => FileKind::Audio,
        "exe" => FileKind::Program,
        _ => FileKind::Other,
    }
}

/// The program that opens files of this kind by default.
pub fn default_app(name: &str) -> Option<&'static str> {
    match file_kind(name) {
        FileKind::Text => Some("/system/bin/editor.exe"),
        FileKind::Image => Some("/system/bin/photos.exe"),
        FileKind::Audio => Some("/system/bin/music.exe"),
        _ => None,
    }
}

/// Human-readable size: `512 B`, `12.4 KiB`, `3.0 MiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut unit = 0;
    let mut v = bytes as u128 * 10;
    while v >= 1024 * 10 && unit < UNITS.len() - 1 {
        v /= 1024;
        unit += 1;
    }
    format!("{}.{} {}", v / 10, v % 10, UNITS[unit])
}

/// `YYYY-MM-DD HH:MM` for a modification time in nanoseconds (or `-`).
pub fn format_time(ns: u64) -> String {
    if ns == 0 {
        return "-".to_string();
    }
    let d = vrt::time::DateTime::from_unix(ns / 1_000_000_000);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute)
}
