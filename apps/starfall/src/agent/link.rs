//! A game's registration with the agent service. Games run their own loop
//! rather than `vui::run`, which registers ordinary applications.
//!
//! Registering waits for the service's answer, and a frame must not wait:
//! a helper thread registers, retrying every few seconds while the service
//! does not answer, and registers again when the service went away (init
//! restarts it). The game picks the link up between frames and answers the
//! requests waiting on it without ever waiting itself.

use alloc::sync::Arc;

use vrt::sync::{Condvar, Mutex};
use vrt::time::Duration;
use vui::agent::{AgentLink, AgentServer};

/// The pause before trying to register again.
const RETRY: Duration = Duration::from_secs(5);

/// What the game and the helper thread share.
struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
}

struct Slot {
    /// A registration the helper made, for the game to take.
    link: Option<AgentLink>,
    /// The game needs a registration.
    wanted: bool,
}

/// The game's end of its registration.
pub struct Link {
    link: Option<AgentLink>,
    shared: Arc<Shared>,
}

impl Link {
    /// Starts registering in the background.
    pub fn new() -> Link {
        let shared = Arc::new(Shared { slot: Mutex::new(Slot { link: None, wanted: true }), wake: Condvar::new() });
        let helper = shared.clone();
        let spawned =
            vrt::thread::Builder::new().name("agent-link").stack_size(64 * 1024).spawn(move || register(&helper));
        if spawned.is_err() {
            vrt::println!("cannot start the thread that registers with the agent");
        }
        Link { link: None, shared }
    }

    /// Answers the requests waiting on the link, if any (never waits).
    pub fn serve(&mut self, server: &mut impl AgentServer) {
        if self.link.is_none()
            && let Some(mut slot) = self.shared.slot.try_lock()
        {
            self.link = slot.link.take();
        }
        if let Some(link) = &self.link
            && !link.serve(server)
        {
            // The agent service went away: register with the next one.
            self.link = None;
            self.shared.slot.lock().wanted = true;
            self.shared.wake.notify_one();
        }
    }
}

/// The helper thread: registers whenever the game needs it.
fn register(shared: &Shared) {
    loop {
        let mut slot = shared.slot.lock();
        while !slot.wanted {
            slot = shared.wake.wait(slot);
        }
        drop(slot);
        match AgentLink::connect() {
            Some(link) => {
                let mut slot = shared.slot.lock();
                slot.link = Some(link);
                slot.wanted = false;
            }
            None => vrt::time::sleep(RETRY),
        }
    }
}
