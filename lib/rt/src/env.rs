//! Program arguments, environment and the handles received at startup.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vabi::RawHandle;
use vabi::startup::StartupView;

use crate::object::{Channel, Handle};
use crate::sync::SpinLock;

#[derive(Default)]
pub(crate) struct Startup {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub handles: Vec<(u32, Handle)>,
}

pub(crate) static STARTUP: SpinLock<Option<Startup>> = SpinLock::new(None);

/// Command-line arguments; `args()[0]` is the program name.
#[cfg(not(veda_guest))]
pub fn args() -> Vec<String> {
    STARTUP.lock().as_ref().map(|s| s.args.clone()).unwrap_or_default()
}

/// Command-line arguments; `args()[0]` is the program name.
#[cfg(veda_guest)]
pub fn args() -> Vec<String> {
    std::env::args().collect()
}

/// The program name (first argument).
pub fn program_name() -> String {
    STARTUP.lock().as_ref().and_then(|s| s.args.first().cloned()).unwrap_or_else(|| "?".to_string())
}

/// The working directory the program was started in (absolute).
pub fn cwd() -> String {
    STARTUP.lock().as_ref().map(|s| s.cwd.clone()).filter(|c| !c.is_empty()).unwrap_or_else(|| "/".to_string())
}

/// Looks up an environment variable.
pub fn var(key: &str) -> Option<String> {
    STARTUP.lock().as_ref()?.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// All environment variables.
pub fn vars() -> Vec<(String, String)> {
    STARTUP.lock().as_ref().map(|s| s.env.clone()).unwrap_or_default()
}

/// Takes the first startup handle with the given role
/// (see `vabi::startup::role`). In a guest of the driver VM, the guest's
/// own (a new one each time).
#[cfg(veda_guest)]
pub fn take_handle(role: u32) -> Option<Handle> {
    // SAFETY: the bridge just gave us this handle.
    crate::guest::bootstrap(role).map(|h| unsafe { Handle::from_raw(h) })
}

/// Takes the first startup handle with the given role
/// (see `vabi::startup::role`).
#[cfg(not(veda_guest))]
pub fn take_handle(role: u32) -> Option<Handle> {
    let mut guard = STARTUP.lock();
    let s = guard.as_mut()?;
    let i = s.handles.iter().position(|(r, _)| *r == role)?;
    Some(s.handles.remove(i).1)
}

/// Roles of the handles that have not been taken yet.
pub fn handle_roles() -> Vec<u32> {
    STARTUP.lock().as_ref().map(|s| s.handles.iter().map(|(r, _)| *r).collect()).unwrap_or_default()
}

/// Reads the startup message from the bootstrap channel the kernel passed
/// the process and keeps what it says for [`args`], [`vars`], [`cwd`] and
/// [`take_handle`]. `vrt`'s entry point does this for Rust programs; C
/// programs' POSIX layer, which has an entry point of its own, calls it.
pub fn adopt(bootstrap: RawHandle) {
    *STARTUP.lock() = Some(read_startup(bootstrap));
}

/// Decodes the startup message on the bootstrap channel.
fn read_startup(bootstrap: RawHandle) -> Startup {
    if bootstrap == vabi::INVALID_HANDLE {
        return Startup::default();
    }
    // SAFETY: the kernel passes us ownership of the bootstrap channel.
    let ch = Channel(unsafe { Handle::from_raw(bootstrap) });
    let Ok(msg) = ch.read() else { return Startup::default() };
    let Ok(view) = StartupView::parse(&msg.bytes) else { return Startup::default() };
    let args = view.args().map(|s| s.to_string()).collect();
    let env = view
        .env()
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect();
    let handles: Vec<(u32, Handle)> =
        msg.handles.into_iter().enumerate().map(|(i, h)| (view.role(i).unwrap_or(0), h)).collect();
    Startup { args, env, cwd: view.cwd().to_string(), handles }
}
