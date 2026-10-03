//! The desktop shell's service: lets applications (Settings, Photos, ...)
//! change the wallpaper and post notifications.

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{enumeration, protocol};

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ShellError {
        NotFound = 1,
        BadImage = 2,
        Unavailable = 3,
    }
}

protocol! {
    /// The desktop shell.
    pub mod shell = "shell" {
        /// Sets the desktop wallpaper to the image at `path` (PNG, JPEG,
        /// BMP or QOI). The choice is remembered across reboots when the
        /// home directory is persistent.
        1 => fn set_wallpaper(path: String) -> Result<(), ShellError>;
        /// Path of the current wallpaper.
        2 => fn wallpaper() -> String;
        /// Shows a notification bubble. `icon` is a `vui::Icon` name.
        3 => fn notify(title: String, body: String, icon: String) -> ();
        /// Wallpapers shipped with the system.
        4 => fn wallpapers() -> Vec<String>;
    }
}
