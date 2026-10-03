//! Talking to the desktop shell's service from a background thread.
//!
//! Calls into the shell can be slow (setting a wallpaper decodes and scales
//! a large image) and a connection to a service that never registers would
//! wait forever, so every request runs on a worker thread. The worker only
//! connects once the registry lists the `shell` service; until then every
//! request is answered with [`Reply::Unavailable`].

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vproto::shell::{ShellError, shell};
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};

/// A request for the shell.
pub enum Request {
    /// List the wallpapers and the current one.
    Refresh,
    /// Make this image the wallpaper.
    Apply(String),
}

/// What the shell answered.
pub enum Reply {
    /// The shell service is not running (or stopped answering).
    Unavailable,
    List {
        wallpapers: Vec<String>,
        current: String,
    },
    Applied {
        path: String,
        result: Result<(), String>,
    },
}

struct Shared {
    requests: Mutex<VecDeque<Request>>,
    wake: Condvar,
    replies: Mutex<Vec<Reply>>,
}

/// The UI side of the worker.
pub struct ShellLink {
    shared: Arc<Shared>,
    event: Event,
}

impl ShellLink {
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

    pub fn send(&self, r: Request) {
        self.shared.requests.lock().push_back(r);
        self.shared.wake.notify_one();
    }

    /// Replies received so far.
    pub fn take(&self) -> Vec<Reply> {
        let _ = self.event.clear();
        core::mem::take(&mut *self.shared.replies.lock())
    }

    /// Signalled when replies are waiting.
    pub fn event_handle(&self) -> RawHandle {
        self.event.raw()
    }
}

/// True if the shell has registered its service.
fn shell_registered() -> bool {
    match vproto::with_registry(|r| r.list()) {
        Ok(Ok(names)) => names.iter().any(|n| n == shell::NAME),
        _ => false,
    }
}

fn error_text(e: ShellError) -> String {
    match e {
        ShellError::NotFound => "The image file could not be found.",
        ShellError::BadImage => "The image could not be read. It may be damaged or in an unsupported format.",
        ShellError::Unavailable => "The desktop can't change its wallpaper right now.",
    }
    .to_string()
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
            client = vproto::connect(shell::NAME).ok().map(shell::Client::new);
        }
        let reply = match (&client, req) {
            (None, Request::Apply(path)) => {
                Reply::Applied { path, result: Err("The desktop shell is not running.".into()) }
            }
            (None, Request::Refresh) => Reply::Unavailable,
            (Some(c), Request::Refresh) => match (c.wallpapers(), c.wallpaper()) {
                (Ok(wallpapers), Ok(current)) => Reply::List { wallpapers, current },
                _ => {
                    client = None;
                    Reply::Unavailable
                }
            },
            (Some(c), Request::Apply(path)) => match c.set_wallpaper(path.clone()) {
                Ok(Ok(())) => Reply::Applied { path, result: Ok(()) },
                Ok(Err(e)) => Reply::Applied { path, result: Err(error_text(e)) },
                Err(_) => {
                    client = None;
                    Reply::Applied { path, result: Err("The desktop shell is not responding.".into()) }
                }
            },
        };
        shared.replies.lock().push(reply);
        let _ = event.signal();
    }
}
