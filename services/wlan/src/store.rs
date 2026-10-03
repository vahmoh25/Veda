//! Saved networks on disk, in the user's configuration directory (the part
//! of the file system that is kept on the home disk).
//!
//! The file holds passphrases, which the service needs to join networks
//! again. The `wlan` API never returns them, but the file itself is
//! readable by any program, as the file system has no access control yet.

use alloc::vec::Vec;

use vfiles::fs::Fs;
use vwlan::profile::{self, Profile};

const DIR: &str = "/home/user/.config/wlan";
const PATH: &str = "/home/user/.config/wlan/networks";
const TEMP: &str = "/home/user/.config/wlan/networks.new";

/// Reads the saved networks (none if the file is missing or unreadable).
pub fn load(fs: &Fs) -> Vec<Profile> {
    match fs.read(PATH) {
        Ok(data) => profile::parse(&data),
        Err(_) => Vec::new(),
    }
}

/// Writes the saved networks: to a new file first, then renamed over the
/// old one, so a failure part-way leaves the previous list intact.
pub fn save(fs: &Fs, profiles: &[Profile]) -> bool {
    let text = profile::serialize(profiles);
    if fs.mkdir_all(DIR).is_err() || fs.write(TEMP, text.as_bytes()).is_err() {
        return false;
    }
    if fs.rename(TEMP, PATH).is_ok() {
        return true;
    }
    // Renaming over an existing file is not supported everywhere: replace
    // it instead.
    let _ = fs.remove(PATH);
    fs.rename(TEMP, PATH).is_ok()
}
