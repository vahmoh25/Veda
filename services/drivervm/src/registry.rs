//! The registry the guest sees: Veda's, narrowed to the services that
//! drivers attach to.
//!
//! Linux in the driver VM runs a great deal of code Veda did not write,
//! so it gets no more than its drivers need: it may connect to the
//! services drivers attach to (and provide the ones it serves); it cannot
//! reach the files, the agent, or anything else. Connections it opens
//! carry this process's identity, which `devmgr` started as a driver.

use alloc::string::String;
use alloc::vec::Vec;

use vabi::{DEADLINE_INFINITE, Error, signals};
use vipc::WaitSet;
use vproto::init::RegistryError;
use vproto::registry;
use vrt::object::{Channel, Event};
use vrt::sync::Mutex;

/// The services the guest may connect to.
const CONNECT: &[&str] = &["audiodev", "displaydev", "netdev", "wlanphy", "input"];
/// The services it may provide.
const REGISTER: &[&str] = &["gpu"];

pub struct Registry {
    /// Our ends of the guest's registry channels.
    channels: Mutex<Vec<Channel>>,
    /// Signaled when a channel is added.
    added: Event,
}

impl Registry {
    pub fn new() -> Result<Registry, Error> {
        Ok(Registry { channels: Mutex::new(Vec::new()), added: Event::create()? })
    }

    /// A new registry channel for the guest; the end it is to hold.
    pub fn connect(&self) -> Result<Channel, Error> {
        let (ours, theirs) = Channel::create()?;
        self.channels.lock().push(ours);
        self.added.signal()?;
        Ok(theirs)
    }

    /// Serves the guest's registry channels (on a thread of its own).
    pub fn run(&self) -> ! {
        const ADDED: u64 = u64::MAX;
        loop {
            let mut ws = WaitSet::new();
            ws.add(self.added.raw(), signals::SIGNALED, ADDED);
            for (i, ch) in self.channels.lock().iter().enumerate() {
                ws.add(ch.raw(), signals::READABLE | signals::PEER_CLOSED, i as u64);
            }
            let Ok(ready) = ws.wait(DEADLINE_INFINITE) else { continue };
            let mut closed = Vec::new();
            for (key, observed) in ready {
                if key == ADDED {
                    let _ = self.added.clear();
                    continue;
                }
                if observed & signals::READABLE != 0 {
                    loop {
                        let msg = match self.channels.lock().get(key as usize).map(Channel::read) {
                            Some(Ok(msg)) => msg,
                            _ => break,
                        };
                        let reply = registry::dispatch(&mut Narrowed(self), msg);
                        if let (Ok(reply), Some(ch)) = (reply, self.channels.lock().get(key as usize)) {
                            let _ = reply.send(ch);
                        }
                    }
                } else if observed & signals::PEER_CLOSED != 0 {
                    closed.push(key as usize);
                }
            }
            let mut channels = self.channels.lock();
            for i in closed.into_iter().rev() {
                channels.remove(i);
            }
        }
    }
}

/// The registry's calls, as far as the guest may make them.
struct Narrowed<'a>(&'a Registry);

impl registry::Server for Narrowed<'_> {
    fn connect(&mut self, name: String, server_end: Channel) -> Result<(), RegistryError> {
        if !CONNECT.contains(&name.as_str()) {
            return Err(RegistryError::Denied);
        }
        match vproto::with_registry(|r| r.connect(name, server_end)) {
            Ok(Ok(result)) => result,
            _ => Err(RegistryError::Denied),
        }
    }

    fn register(&mut self, name: String, listener: Channel) -> Result<(), RegistryError> {
        if !REGISTER.contains(&name.as_str()) {
            return Err(RegistryError::Denied);
        }
        match vproto::with_registry(|r| r.register(name, listener)) {
            Ok(Ok(result)) => result,
            _ => Err(RegistryError::Denied),
        }
    }

    fn list(&mut self) -> Vec<String> {
        let all = match vproto::with_registry(|r| r.list()) {
            Ok(Ok(names)) => names,
            _ => Vec::new(),
        };
        all.into_iter().filter(|n| CONNECT.contains(&n.as_str()) || REGISTER.contains(&n.as_str())).collect()
    }

    fn clone_registry(&mut self) -> Result<Channel, RegistryError> {
        self.0.connect().map_err(|_| RegistryError::Denied)
    }
}
