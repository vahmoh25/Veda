//! File system helpers on top of the VFS protocol: path manipulation,
//! whole-file reads and writes, recursive copy and removal, and globbing.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use vproto::fs::{DirEntry, FsError, Stat, vfs};
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

/// The directory containing `path` (`/` for top-level entries).
pub fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// The last component of `path`.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A name in `dir` that does not exist yet: `base`, then `base (2)`, ...
/// (the number goes before the extension).
pub fn unique_name(fs: &Fs, dir: &str, base: &str) -> String {
    if !fs.exists(&join(dir, base)) {
        return base.to_string();
    }
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => (&base[..i], &base[i..]),
        _ => (base, ""),
    };
    for n in 2.. {
        let candidate = format!("{stem} ({n}){ext}");
        if !fs.exists(&join(dir, &candidate)) {
            return candidate;
        }
    }
    unreachable!()
}

/// A short description of a file's type ("PNG image", "Folder", ...).
pub fn kind_name(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_string();
    }
    let ext = extension(name);
    let known = match ext.as_str() {
        "txt" => "Text document",
        "md" => "Markdown document",
        "rs" => "Rust source",
        "c" | "h" | "cpp" | "hpp" => "C/C++ source",
        "py" => "Python script",
        "sh" => "Shell script",
        "cfg" | "toml" | "ini" | "conf" | "yaml" | "yml" => "Settings file",
        "json" => "JSON document",
        "log" => "Log file",
        "csv" => "CSV table",
        "html" | "css" | "js" | "xml" => "Web document",
        "app" => "App manifest",
        "vts" => "Automation script",
        "png" => "PNG image",
        "jpg" | "jpeg" => "JPEG image",
        "bmp" => "BMP image",
        "qoi" => "QOI image",
        "wav" => "WAV audio",
        "qoa" => "QOA audio",
        "mp3" => "MP3 audio",
        "ogg" | "opus" => "Ogg audio",
        "flac" => "FLAC audio",
        "mid" | "midi" => "MIDI music",
        "mod" | "xm" | "s3m" | "it" => "Tracker music",
        "aac" | "m4a" => "AAC audio",
        "exe" => "Program",
        "ttf" | "otf" => "Font",
        "img" | "iso" => "Disk image",
        _ => "",
    };
    if !known.is_empty() {
        known.to_string()
    } else if ext.is_empty() {
        "File".to_string()
    } else {
        format!("{} file", ext.to_ascii_uppercase())
    }
}

/// A friendly modification time: "Today, 14:05", "Yesterday, 09:30" or
/// "3 Oct 2026, 14:05".
pub fn friendly_time(ns: u64) -> String {
    if ns == 0 {
        return "—".to_string();
    }
    let secs = ns / 1_000_000_000;
    let d = vrt::time::DateTime::from_unix(secs);
    let now_secs = vrt::time::unix_time_ns() / 1_000_000_000;
    let (day, today) = (secs / 86_400, now_secs / 86_400);
    if day == today {
        format!("Today, {:02}:{:02}", d.hour, d.minute)
    } else if day + 1 == today {
        format!("Yesterday, {:02}:{:02}", d.hour, d.minute)
    } else {
        format!("{} {} {}, {:02}:{:02}", d.day, &d.month_name()[..3], d.year, d.hour, d.minute)
    }
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
