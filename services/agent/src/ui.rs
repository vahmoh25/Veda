//! The link to the agent's user interface in the shell: one-way events
//! (status, approval requests) on a channel, and a shared page of live
//! voice levels that the shell's animation reads every frame.

use vproto::agent::{AGENT_EVENT, AgentError, AgentEvent, Live, UiLink};
use vrt::object::{Channel, Vmo};
use vrt::vm::Mapping;

pub struct Ui {
    events: Channel,
    map: Mapping,
}

impl Ui {
    /// Creates the link; the [`UiLink`] goes to the shell.
    pub fn attach() -> Result<(Ui, UiLink), AgentError> {
        let (ours, theirs) = Channel::create().map_err(|_| AgentError::Busy)?;
        let vmo = Vmo::create(4096).map_err(|_| AgentError::Busy)?;
        let peer = Vmo::from_handle(vmo.0.duplicate(None).map_err(|_| AgentError::Busy)?);
        let map =
            Mapping::new(vmo, 4096, vabi::map_flags::READ | vabi::map_flags::WRITE).map_err(|_| AgentError::Busy)?;
        Ok((Ui { events: ours, map }, UiLink { events: theirs, live: peer }))
    }

    /// Sends an event; `false` once the shell has gone away.
    pub fn send(&self, ev: AgentEvent) -> bool {
        vipc::send_event(&self.events, AGENT_EVENT, ev).is_ok()
    }

    /// The live values.
    pub fn live(&self) -> &Live {
        // SAFETY: the mapping is a page of zero-initialised memory, aligned,
        // and `Live` is a few atomics, valid for any bit pattern.
        unsafe { &*(self.map.as_ptr() as *const Live) }
    }
}
