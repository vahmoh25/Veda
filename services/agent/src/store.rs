//! The agent's private directory, `/home/.private/agent`, on the home disk.
//! Only the agent service can open it (the file system checks who is
//! asking), so the Deepgram key and what the agent remembers about the user
//! stay out of reach of other programs.

use alloc::string::String;

use vfiles::Fs;

pub const DIR: &str = "/home/.private/agent";

pub const CONFIG: &str = "config.json";
pub const KEY: &str = "deepgram-key";
pub const MEMORY: &str = "memory.json";
pub const PERMISSIONS: &str = "permissions.json";
pub const TIMERS: &str = "timers.json";

pub struct Store {
    fs: Fs,
}

impl Store {
    /// Opens (and creates) the private directory.
    pub fn open() -> Store {
        let fs = Fs::connect();
        if let Err(e) = fs.mkdir_all(DIR) {
            vrt::println!("cannot create {}: {}", DIR, e);
        }
        Store { fs }
    }

    fn path(name: &str) -> String {
        alloc::format!("{DIR}/{name}")
    }

    /// A stored text file, if it exists.
    pub fn read(&self, name: &str) -> Option<String> {
        let bytes = self.fs.read(&Self::path(name)).ok()?;
        String::from_utf8(bytes).ok()
    }

    /// Replaces a stored file (written to a temporary name first, so an
    /// interrupted write never leaves half a file).
    pub fn write(&self, name: &str, text: &str) -> bool {
        let tmp = Self::path(&alloc::format!("{name}.new"));
        let path = Self::path(name);
        if let Err(e) = self.fs.write(&tmp, text.as_bytes()) {
            vrt::println!("cannot save {}: {}", name, e);
            return false;
        }
        let _ = self.fs.remove(&path);
        match self.fs.rename(&tmp, &path) {
            Ok(()) => true,
            Err(e) => {
                vrt::println!("cannot save {}: {}", name, e);
                false
            }
        }
    }

    pub fn remove(&self, name: &str) {
        let _ = self.fs.remove(&Self::path(name));
    }
}
