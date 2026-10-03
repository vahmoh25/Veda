//! One player at a time: the first Music process registers the `music`
//! service; a later one that was asked to open a file (for example from the
//! desktop) hands the file to the running player and exits.

use alloc::string::String;
use alloc::vec::Vec;

use vabi::{RawHandle, signals};
use vrt::object::Channel;
use vrt::time::{Duration, deadline_after};

vipc::protocol! {
    /// Remote control of the running Music player.
    #[allow(dead_code)]
    pub mod remote = "music" {
        /// Adds `path` to the library and plays it; false if it is not an
        /// audio file the player supports.
        1 => fn open(path: String) -> bool;
    }
}

/// Most simultaneous remote connections.
const MAX_CLIENTS: usize = 8;

/// The running player's end of the service.
pub struct Server {
    listener: Channel,
    clients: Vec<Channel>,
}

struct Handler<'a> {
    files: &'a mut Vec<String>,
}

impl remote::Server for Handler<'_> {
    fn open(&mut self, path: String) -> bool {
        let ok = path.starts_with('/') && crate::library::is_audio_file(&path);
        if ok {
            self.files.push(path);
        }
        ok
    }
}

impl Server {
    /// Registers the service; `None` if another player already has it.
    pub fn register() -> Option<Server> {
        vproto::register(remote::NAME).ok().map(|listener| Server { listener, clients: Vec::new() })
    }

    /// Kernel objects to wait on (the listener, then every client).
    pub fn wait_handles(&self, out: &mut Vec<(RawHandle, u32)>) {
        out.push((self.listener.raw(), signals::READABLE));
        for c in &self.clients {
            out.push((c.raw(), signals::READABLE | signals::PEER_CLOSED));
        }
    }

    /// Accepts connections and serves requests; returns the files to open.
    pub fn poll(&mut self) -> Vec<String> {
        while let Some(ch) = vproto::accept(&self.listener) {
            if self.clients.len() < MAX_CLIENTS {
                self.clients.push(ch);
            }
        }
        let mut files = Vec::new();
        self.clients.retain(|c| {
            loop {
                match c.read() {
                    Ok(msg) => {
                        if let Ok(reply) = remote::dispatch(&mut Handler { files: &mut files }, msg) {
                            let _ = reply.send(c);
                        }
                    }
                    Err(vabi::Error::ShouldWait) => break true,
                    Err(_) => break false,
                }
            }
        });
        files
    }
}

/// Hands `path` to the running player (waiting at most a few seconds for
/// its answer). Returns true if it took the file.
pub fn forward(path: &str) -> bool {
    let Ok(ch) = vproto::connect(remote::NAME) else { return false };
    let mut e = vipc::Encoder::with_header(1, 1, vipc::FLAG_REQUEST);
    vipc::Encode::encode(path, &mut e);
    if e.send(&ch).is_err() {
        return false;
    }
    let Ok(mut msg) = ch.read_blocking(deadline_after(Duration::from_secs(3))) else { return false };
    match vipc::open(&mut msg) {
        Ok((h, mut d)) if h.flags & vipc::FLAG_RESPONSE != 0 => <bool as vipc::Decode>::decode(&mut d).unwrap_or(false),
        _ => false,
    }
}
