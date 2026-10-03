//! Setting the desktop wallpaper through the shell's service, from a background thread.
//!
//! The shell decodes and scales the picture before it answers, which takes a while under
//! emulation, and connecting to a service that has not registered would wait forever; so the
//! calls run on a worker that only connects once the registry lists the `shell` service.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vproto::shell::{ShellError, shell};
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};

struct Shared {
    requests: Mutex<VecDeque<String>>,
    wake: Condvar,
    replies: Mutex<Vec<Result<(), String>>>,
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

    /// Asks the shell to use the picture at `path` as the wallpaper.
    pub fn set_wallpaper(&self, path: &str) {
        self.shared.requests.lock().push_back(path.to_string());
        self.shared.wake.notify_one();
    }

    /// Answers received so far.
    pub fn take(&self) -> Vec<Result<(), String>> {
        let _ = self.event.clear();
        core::mem::take(&mut *self.shared.replies.lock())
    }

    /// Signalled when answers are waiting.
    pub fn event_handle(&self) -> RawHandle {
        self.event.raw()
    }
}

fn worker(shared: Arc<Shared>, event: Event) {
    let mut client: Option<shell::Client> = None;
    loop {
        let path = {
            let mut q = shared.requests.lock();
            loop {
                if let Some(p) = q.pop_front() {
                    break p;
                }
                q = shared.wake.wait(q);
            }
        };
        if client.is_none() && shell_registered() {
            client = vproto::connect(shell::NAME).ok().map(shell::Client::new);
        }
        let reply = match &client {
            None => Err("The desktop is not running.".to_string()),
            Some(c) => match c.set_wallpaper(path) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(error_text(e)),
                Err(_) => {
                    client = None;
                    Err("The desktop is not responding.".to_string())
                }
            },
        };
        shared.replies.lock().push(reply);
        let _ = event.signal();
    }
}

fn error_text(e: ShellError) -> String {
    match e {
        ShellError::NotFound => "The desktop could not find the picture.",
        ShellError::BadImage => "The desktop could not read the picture.",
        ShellError::Unavailable => "The desktop cannot change its wallpaper right now.",
    }
    .to_string()
}

/// True if the shell has registered its service.
fn shell_registered() -> bool {
    match vproto::with_registry(|r| r.list()) {
        Ok(Ok(names)) => names.iter().any(|n| n == shell::NAME),
        _ => false,
    }
}
