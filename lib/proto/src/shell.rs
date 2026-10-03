//! The desktop shell's service: lets applications (Settings, Photos, ...)
//! change the wallpaper and post notifications.
//!
//! [`ShellLink`] calls the service from a background thread, for
//! applications that must not wait for it (setting a wallpaper decodes and
//! scales a large picture first).

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use vabi::RawHandle;
use vipc::{enumeration, protocol};
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};

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

/// Why the wallpaper could not be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperError {
    /// The shell is not running.
    NotRunning,
    /// The shell stopped answering.
    NotResponding,
    /// The shell refused.
    Shell(ShellError),
}

impl fmt::Display for WallpaperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WallpaperError::NotRunning => "The desktop shell is not running.",
            WallpaperError::NotResponding => "The desktop shell is not responding.",
            WallpaperError::Shell(ShellError::NotFound) => "The desktop could not find the picture.",
            WallpaperError::Shell(ShellError::BadImage) => {
                "The desktop could not read the picture. It may be damaged or in an unsupported format."
            }
            WallpaperError::Shell(ShellError::Unavailable) => "The desktop cannot change its wallpaper right now.",
        })
    }
}

/// An answer from [`ShellLink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellReply {
    /// The shell is not running (or stopped answering), so the wallpapers
    /// could not be listed.
    Unavailable,
    /// The wallpapers shipped with the system, and the current wallpaper.
    Wallpapers { list: Vec<String>, current: String },
    /// The outcome of [`ShellLink::set_wallpaper`].
    WallpaperSet { path: String, result: Result<(), WallpaperError> },
}

enum Request {
    Wallpapers,
    SetWallpaper(String),
}

struct Shared {
    requests: Mutex<VecDeque<Request>>,
    wake: Condvar,
    replies: Mutex<Vec<ShellReply>>,
}

/// Calls the shell from a background thread. Requests are answered in order
/// through [`ShellLink::take`]; [`ShellLink::event_handle`] is signalled
/// when answers are waiting (return it from `App::wait_handles`).
///
/// The worker connects only once the registry lists the `shell` service
/// (connecting earlier would wait for it forever) and reconnects after the
/// shell restarts.
pub struct ShellLink {
    shared: Arc<Shared>,
    event: Event,
}

impl ShellLink {
    /// Starts the worker thread.
    pub fn start() -> Option<ShellLink> {
        let event = Event::create().ok()?;
        let theirs = Event::from_handle(event.0.duplicate(None).ok()?);
        let shared = Arc::new(Shared {
            requests: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            replies: Mutex::new(Vec::new()),
        });
        let s = shared.clone();
        vrt::thread::Builder::new().name("shell-link").spawn(move || worker(s, theirs)).ok()?;
        Some(ShellLink { shared, event })
    }

    fn send(&self, r: Request) {
        self.shared.requests.lock().push_back(r);
        self.shared.wake.notify_one();
    }

    /// Asks for the system's wallpapers and the current one (answered with
    /// [`ShellReply::Wallpapers`], or [`ShellReply::Unavailable`]).
    pub fn request_wallpapers(&self) {
        self.send(Request::Wallpapers);
    }

    /// Asks the shell to use the picture at `path` as the wallpaper
    /// (answered with [`ShellReply::WallpaperSet`]).
    pub fn set_wallpaper(&self, path: &str) {
        self.send(Request::SetWallpaper(path.to_string()));
    }

    /// The answers received so far.
    pub fn take(&self) -> Vec<ShellReply> {
        let _ = self.event.clear();
        core::mem::take(&mut *self.shared.replies.lock())
    }

    /// Signalled when answers are waiting.
    pub fn event_handle(&self) -> RawHandle {
        self.event.raw()
    }
}

/// True if the shell has registered its service.
fn shell_registered() -> bool {
    match crate::with_registry(|r| r.list()) {
        Ok(Ok(names)) => names.iter().any(|n| n == shell::NAME),
        _ => false,
    }
}

fn worker(shared: Arc<Shared>, event: Event) {
    let mut client: Option<shell::Client> = None;
    loop {
        let req = {
            let mut q = shared.requests.lock();
            loop {
                if let Some(r) = q.pop_front() {
                    break r;
                }
                q = shared.wake.wait(q);
            }
        };
        if client.is_none() && shell_registered() {
            client = crate::connect(shell::NAME).ok().map(shell::Client::new);
        }
        let reply = match (&client, req) {
            (None, Request::Wallpapers) => ShellReply::Unavailable,
            (None, Request::SetWallpaper(path)) => {
                ShellReply::WallpaperSet { path, result: Err(WallpaperError::NotRunning) }
            }
            (Some(c), Request::Wallpapers) => match (c.wallpapers(), c.wallpaper()) {
                (Ok(list), Ok(current)) => ShellReply::Wallpapers { list, current },
                _ => {
                    client = None;
                    ShellReply::Unavailable
                }
            },
            (Some(c), Request::SetWallpaper(path)) => match c.set_wallpaper(path.clone()) {
                Ok(Ok(())) => ShellReply::WallpaperSet { path, result: Ok(()) },
                Ok(Err(e)) => ShellReply::WallpaperSet { path, result: Err(WallpaperError::Shell(e)) },
                Err(_) => {
                    client = None;
                    ShellReply::WallpaperSet { path, result: Err(WallpaperError::NotResponding) }
                }
            },
        };
        shared.replies.lock().push(reply);
        let _ = event.signal();
    }
}
