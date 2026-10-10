//! Protocols of the Veda system services.
//!
//! Each submodule declares one service protocol with `vipc::protocol!`,
//! together with its wire types. Clients connect to a service by name through
//! the [`registry`], which `init` implements.

#![no_std]

extern crate alloc;

pub mod agent;
pub mod audio;
pub mod block;
pub mod display;
pub mod displaydev;
pub mod fs;
pub mod gpu;
pub mod init;
pub mod input;
pub mod net;
pub mod netring;
pub mod pci;
pub mod shell;
pub mod tty;
pub mod wlan;

pub use fs::vfs;
pub use init::{launcher, registry};

use vrt::object::Channel;

/// Connects to a named service through the registry channel the process
/// received at startup. Returns the client end of a new connection.
pub fn connect(name: &str) -> Result<Channel, ServiceError> {
    let (client, server) = Channel::create().map_err(|_| ServiceError::Unavailable)?;
    match with_registry(|r| r.connect(name.into(), server))? {
        Ok(Ok(())) => Ok(client),
        Ok(Err(_)) => Err(ServiceError::NotFound),
        Err(_) => Err(ServiceError::Unavailable),
    }
}

/// Errors from [`connect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    /// No registry channel or the registry is gone.
    Unavailable,
    /// No such service.
    NotFound,
}

static REGISTRY: vrt::sync::Mutex<Option<registry::Client>> = vrt::sync::Mutex::new(None);

/// Runs `f` with this process's registry client. Calls from several threads
/// are serialised so their replies cannot interleave.
pub fn with_registry<R>(f: impl FnOnce(&registry::Client) -> R) -> Result<R, ServiceError> {
    let mut slot = REGISTRY.lock();
    if slot.is_none() {
        let h = vrt::env::take_handle(vabi::startup::role::REGISTRY).ok_or(ServiceError::Unavailable)?;
        *slot = Some(registry::Client::new(Channel::from_handle(h)));
    }
    Ok(f(slot.as_ref().unwrap()))
}

/// Registers this process as the provider of service `name`. Returns the
/// listener on which new connections arrive.
pub fn register(name: &str) -> Result<Channel, ServiceError> {
    let (listener, theirs) = Channel::create().map_err(|_| ServiceError::Unavailable)?;
    match with_registry(|r| r.register(name.into(), theirs))? {
        Ok(Ok(())) => Ok(listener),
        _ => Err(ServiceError::Unavailable),
    }
}

/// Reads one pending connection from a service listener.
pub fn accept(listener: &Channel) -> Option<Channel> {
    accept_with_identity(listener).map(|(ch, _)| ch)
}

/// Reads one pending connection and who opened it.
pub fn accept_with_identity(listener: &Channel) -> Option<(Channel, init::ClientIdentity)> {
    let msg = listener.read().ok()?;
    match vipc::decode_event::<(Channel, init::ClientIdentity)>(msg) {
        Ok((init::LISTENER_CONNECT, (ch, id))) => Some((ch, id)),
        _ => None,
    }
}
