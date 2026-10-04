//! `vagent` — the logic of the voice agent that lives in Vindows.
//!
//! The agent service (`services/agent`) does the I/O — audio, the network,
//! talking to applications — and uses this crate for everything that can be
//! decided without it, so that it can be tested on the host:
//!
//! * [`deepgram`]: the Deepgram Voice Agent protocol (the `Settings`
//!   message, server messages, client messages), streaming speech-to-text
//!   for hearing the agent's name, and the REST catalogs.
//! * [`tools`]: the functions the language model may call, their JSON
//!   schemas and how much each can affect the user ([`tools::Risk`]).
//! * [`prompt`]: the agent's character and instructions.
//! * [`memory`]: what the agent remembers about the user.
//! * [`config`]: the agent's settings.
//! * [`policy`]: which actions need the user's approval, and the actions
//!   the user always allows.
//! * [`wake`]: recognising the agent's name in what it hears.
//! * [`gate`]: keeping the agent from hearing its own voice.

#![no_std]

extern crate alloc;

pub mod config;
pub mod deepgram;
pub mod gate;
pub mod memory;
pub mod policy;
pub mod prompt;
pub mod tools;
pub mod wake;

#[cfg(test)]
extern crate std;
