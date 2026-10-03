//! Program arguments, environment and the handles received at startup.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::object::Handle;
use crate::sync::SpinLock;

#[derive(Default)]
pub(crate) struct Startup {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub handles: Vec<(u32, Handle)>,
}

pub(crate) static STARTUP: SpinLock<Option<Startup>> = SpinLock::new(None);

/// Command-line arguments; `args()[0]` is the program name.
pub fn args() -> Vec<String> {
    STARTUP.lock().as_ref().map(|s| s.args.clone()).unwrap_or_default()
}

/// The program name (first argument).
pub fn program_name() -> String {
    STARTUP.lock().as_ref().and_then(|s| s.args.first().cloned()).unwrap_or_else(|| "?".to_string())
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
/// (see `vabi::startup::role`).
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
