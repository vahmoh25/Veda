//! Making an application available to the voice agent.
//!
//! Agent compatibility is part of the application platform: an [`App`]
//! describes what it can do ([`App::agent_info`]), what it shows
//! ([`App::agent_state`]) and runs actions ([`App::agent_invoke`]); [`run`]
//! registers it with the agent service and answers the agent's requests on
//! the main thread, between frames, then redraws. Applications with their
//! own event loop use [`AgentLink`] and [`AgentServer`] directly.
//!
//! ```ignore
//! impl vui::App for Editor {
//!     fn agent_info(&self) -> Option<AppAgentInfo> {
//!         Some(agent::info("A text editor with tabs.", alloc::vec![
//!             Action::new("open_file", "Opens a text file in a new tab")
//!                 .param("path", "string", "Path of the file", true)
//!                 .build(),
//!         ]))
//!     }
//!     fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
//!         match action {
//!             "open_file" => { /* ... */ Ok(Value::Null) }
//!             _ => Err("unknown action".into()),
//!         }
//!     }
//! }
//! ```
//!
//! [`App`]: crate::App
//! [`App::agent_info`]: crate::App::agent_info
//! [`App::agent_state`]: crate::App::agent_state
//! [`App::agent_invoke`]: crate::App::agent_invoke
//! [`run`]: crate::run

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vabi::RawHandle;
use vproto::agent::{ActionResult, agent, agentapp};
pub use vproto::agent::{ActionSpec, AppAgentInfo, ParamSpec, Risk};
use vrt::object::Channel;

pub use vjson::{Map, Value, object};

/// How long registering with the agent service may take before the
/// application gives up and runs without it.
const REGISTER_TIMEOUT_NS: u64 = 500_000_000;

/// Builds an [`ActionSpec`].
pub struct Action(ActionSpec);

impl Action {
    /// A routine action (see [`Action::risk`]).
    pub fn new(name: &str, description: &str) -> Action {
        Action(ActionSpec {
            name: name.into(),
            description: description.into(),
            params: Vec::new(),
            risk: Risk::Routine,
        })
    }

    /// Adds a parameter: `kind` is `"string"`, `"integer"`, `"number"` or
    /// `"boolean"`.
    pub fn param(mut self, name: &str, kind: &str, description: &str, required: bool) -> Action {
        self.0.params.push(ParamSpec {
            name: name.into(),
            kind: kind.into(),
            description: description.into(),
            required,
            choices: Vec::new(),
        });
        self
    }

    /// Adds a string parameter that takes one of `choices`.
    pub fn choice(mut self, name: &str, description: &str, required: bool, choices: &[&str]) -> Action {
        self.0.params.push(ParamSpec {
            name: name.into(),
            kind: "string".into(),
            description: description.into(),
            required,
            choices: choices.iter().map(|c| c.to_string()).collect(),
        });
        self
    }

    /// How much the action can affect the user (the agent asks the user
    /// before [`Risk::Sensitive`] and [`Risk::Destructive`] actions).
    pub fn risk(mut self, risk: Risk) -> Action {
        self.0.risk = risk;
        self
    }

    pub fn build(self) -> ActionSpec {
        self.0
    }
}

/// An [`AppAgentInfo`] with state.
pub fn info(summary: &str, actions: Vec<ActionSpec>) -> AppAgentInfo {
    AppAgentInfo { summary: summary.into(), actions, has_state: true }
}

/// Reads a string argument.
pub fn arg_str<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name).and_then(Value::as_str).ok_or_else(|| alloc::format!("missing text argument '{name}'"))
}

/// Reads an optional string argument.
pub fn arg_opt_str<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Reads a number argument (integers and numbers in strings accepted, as
/// language models sometimes quote them).
pub fn arg_f64(args: &Value, name: &str) -> Result<f64, String> {
    match args.get(name) {
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(Value::String(s)) => s.trim().parse().map_err(|_| alloc::format!("'{name}' must be a number")),
        _ => Err(alloc::format!("missing number argument '{name}'")),
    }
}

/// Reads a boolean argument (`"true"`/`"false"` accepted).
pub fn arg_bool(args: &Value, name: &str) -> Option<bool> {
    match args.get(name) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => match s.as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// What a program implements to serve the agent.
pub trait AgentServer {
    fn agent_info(&self) -> AppAgentInfo;
    fn agent_state(&self) -> Value;
    fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String>;
}

/// Adapts an [`AgentServer`] to the generated protocol server.
struct Dispatch<'a, S: AgentServer + ?Sized>(&'a mut S);

impl<S: AgentServer + ?Sized> agentapp::Server for Dispatch<'_, S> {
    fn describe(&mut self) -> AppAgentInfo {
        self.0.agent_info()
    }

    fn state(&mut self) -> String {
        self.0.agent_state().to_string()
    }

    fn invoke(&mut self, action: String, args: String) -> ActionResult {
        let args = match vjson::parse(&args) {
            Ok(v @ Value::Object(_)) => v,
            Ok(Value::Null) => Value::object(),
            Ok(_) | Err(_) => return ActionResult { ok: false, result: "the arguments must be a JSON object".into() },
        };
        match self.0.agent_invoke(&action, &args) {
            Ok(v) => ActionResult { ok: true, result: v.to_string() },
            Err(e) => ActionResult { ok: false, result: e },
        }
    }
}

/// An application's link to the agent service.
pub struct AgentLink {
    channel: Channel,
}

impl AgentLink {
    /// Registers with the agent service, if it is running.
    pub fn connect() -> Option<AgentLink> {
        let running = vproto::with_registry(|r| r.list().map(|l| l.iter().any(|n| n == agent::NAME)))
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(false);
        if !running {
            return None;
        }
        let client = agent::Client::new(vproto::connect(agent::NAME).ok()?);
        client.set_timeout(REGISTER_TIMEOUT_NS);
        let (ours, theirs) = Channel::create().ok()?;
        match client.register_app(theirs) {
            Ok(Ok(())) => Some(AgentLink { channel: ours }),
            _ => None,
        }
    }

    /// Readable when the agent asks something (for wait sets).
    pub fn handle(&self) -> RawHandle {
        self.channel.raw()
    }

    /// Answers every waiting request. Returns `false` once the agent
    /// service has gone away.
    pub fn serve<S: AgentServer + ?Sized>(&self, server: &mut S) -> bool {
        loop {
            match self.channel.read() {
                Ok(msg) => {
                    if let Ok(reply) = agentapp::dispatch(&mut Dispatch(server), msg) {
                        let _ = reply.send(&self.channel);
                    }
                }
                Err(vabi::Error::ShouldWait) => return true,
                Err(_) => return false,
            }
        }
    }
}
