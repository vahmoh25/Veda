//! The agent protocols: how the voice agent that lives in Veda reaches
//! applications, and how the desktop and Settings reach the agent.
//!
//! * [`agentapp`]: served by an **application** on a channel it hands the
//!   agent ([`agent::Client::register_app`]). The app describes what it
//!   can do ([`AppAgentInfo`]: actions with typed parameters and a
//!   [`Risk`]), reports what it shows ([`agentapp::Client::state`], JSON)
//!   and runs actions with JSON arguments. Applications built on `vui` get
//!   this by implementing three methods of `vui::App`.
//! * [`agent`] (service `"agent"`): the **agent service**. Every program may
//!   ask for its status and register itself; the desktop shell (and only
//!   the shell, as the registry identifies it) attaches the agent's user
//!   interface — status events, live voice levels for the animation and
//!   approval requests it decides on — and Settings (only Settings)
//!   configures the agent: the Deepgram key, models and voice, and the
//!   memory the agent keeps about the user.
//!
//! Approvals are enforced by the agent service, not by the language model:
//! an action with a risk above [`Risk::Routine`] does not run until the
//! user allows it in the shell's interface.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vipc::{enumeration, message, protocol, union};
use vrt::object::{Channel, Vmo};

enumeration! {
    /// How much an action can affect the user, which decides whether the
    /// agent asks first.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Risk {
        /// Reading, or a routine action that is easy to undo (open, play,
        /// scroll, create): done without asking.
        Routine = 1,
        /// Changes or sends something that matters (overwrite a file, join a
        /// network, run a command): asks unless the user said to always
        /// allow it.
        Sensitive = 2,
        /// Deletes data, discards work or affects the whole system (delete
        /// files, end a task, shut down): always asks.
        Destructive = 3,
    }
}

message! {
    /// One parameter of an action (a JSON-schema property for the language
    /// model).
    #[derive(Debug, Clone, PartialEq)]
    pub struct ParamSpec {
        pub name: String,
        /// `"string"`, `"integer"`, `"number"` or `"boolean"`.
        pub kind: String,
        pub description: String,
        pub required: bool,
        /// Allowed values (empty: any).
        pub choices: Vec<String>,
    }
}

message! {
    /// Something an application can do for the agent.
    #[derive(Debug, Clone, PartialEq)]
    pub struct ActionSpec {
        /// A short identifier (`"open_file"`).
        pub name: String,
        /// What it does, for the language model.
        pub description: String,
        pub params: Vec<ParamSpec>,
        pub risk: Risk,
    }
}

message! {
    /// What an application offers the agent.
    #[derive(Debug, Clone, PartialEq)]
    pub struct AppAgentInfo {
        /// What the app is for, in a sentence.
        pub summary: String,
        pub actions: Vec<ActionSpec>,
        /// The app can describe what it shows ([`agentapp::Client::state`]).
        pub has_state: bool,
    }
}

message! {
    /// The outcome of an action.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ActionResult {
        pub ok: bool,
        /// A JSON value when `ok`, otherwise a short explanation.
        pub result: String,
    }
}

protocol! {
    /// Served by applications on the channel they registered with the agent.
    pub mod agentapp = "agentapp" {
        1 => fn describe() -> AppAgentInfo;
        /// What the app shows right now, as a JSON value.
        2 => fn state() -> String;
        /// Runs an action with a JSON object of arguments.
        3 => fn invoke(action: String, args: String) -> ActionResult;
    }
}

enumeration! {
    /// What the agent is doing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AgentState {
        /// Not set up (no Deepgram key) or switched off.
        Off = 0,
        /// Not in a conversation; listening for its name if enabled.
        Asleep = 1,
        /// Connecting to start a conversation.
        Waking = 2,
        /// In a conversation; the user's turn.
        Listening = 3,
        /// Working out an answer or running actions.
        Thinking = 4,
        Speaking = 5,
        /// Something is wrong (network, key); see `detail`.
        Error = 6,
    }
}

message! {
    /// The agent's state for the user interface.
    #[derive(Debug, Clone, PartialEq)]
    pub struct AgentStatus {
        pub state: AgentState,
        /// What the agent is called.
        pub name: String,
        /// The agent does not hear the microphone.
        pub muted: bool,
        /// A Deepgram key is set.
        pub configured: bool,
        /// Why the agent is off or failing, in plain words.
        pub detail: String,
        /// Waiting approvals.
        pub pending: u32,
    }
}

message! {
    /// The agent wants to do something that needs the user's consent.
    #[derive(Debug, Clone, PartialEq)]
    pub struct ApprovalRequest {
        pub id: u64,
        /// Who acts ("Files", "System", the app's name).
        pub app: String,
        /// What will happen, in plain words ("Delete “report.txt”").
        pub action: String,
        /// More detail ("It cannot be restored.").
        pub detail: String,
        pub risk: Risk,
        /// The user may allow this kind of action for good.
        pub allow_always: bool,
    }
}

union! {
    /// News for the agent's user interface (one-way, on [`UiLink::events`]).
    #[derive(Debug, Clone, PartialEq)]
    pub enum AgentEvent {
        1 => Status { status: AgentStatus },
        2 => Approval { request: ApprovalRequest },
        /// The request was decided (here or elsewhere), cancelled or
        /// expired: stop showing it.
        3 => ApprovalDone { id: u64 },
    }
}

/// Event ordinal of [`AgentEvent`]s.
pub const AGENT_EVENT: u32 = 1;

message! {
    /// What the shell gets when it attaches the agent's interface.
    #[derive(Debug)]
    pub struct UiLink {
        /// [`AgentEvent`]s.
        pub events: Channel,
        /// A page of [`Live`] values, updated many times a second.
        pub live: Vmo,
    }
}

/// Live values in a shared page: voice levels for the animation. The agent
/// writes, the shell reads (`f32` levels stored as bits).
#[repr(C)]
pub struct Live {
    /// Incremented on every update.
    pub seq: AtomicU32,
    /// Loudness of the agent's voice playing right now (0..=1).
    pub output_level: AtomicU32,
    /// Loudness of the microphone (0..=1).
    pub input_level: AtomicU32,
    /// [`AgentState`] as a number.
    pub state: AtomicU32,
}

impl Live {
    pub fn levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.output_level.load(Ordering::Relaxed)),
            f32::from_bits(self.input_level.load(Ordering::Relaxed)),
        )
    }

    pub fn set_levels(&self, output: f32, input: f32) {
        self.output_level.store(output.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.input_level.store(input.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }
}

message! {
    /// The agent's settings (an OS setting, changed in Settings).
    #[derive(Debug, Clone, PartialEq)]
    pub struct AgentConfig {
        pub enabled: bool,
        /// What the agent is called (and answers to).
        pub name: String,
        /// Deepgram speech-to-text model (`"flux-general-en"`, `"nova-3"`).
        pub listen_model: String,
        /// Language model provider and model, as Deepgram names them
        /// (`"open_ai"`, `"gpt-4o-mini"`).
        pub think_provider: String,
        pub think_model: String,
        /// Deepgram voice (`"aura-2-thalia-en"`).
        pub voice: String,
        /// Speaking rate (0.7 ..= 1.5).
        pub speed: f32,
        /// Listen for its name while asleep.
        pub listen_for_name: bool,
        /// End a conversation after this many seconds of silence.
        pub idle_timeout_s: u32,
        /// A Deepgram key is stored.
        pub has_key: bool,
        /// The last characters of the key ("…a652"), never the key itself.
        pub key_hint: String,
    }
}

message! {
    /// Something the agent remembers about the user.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MemoryItem {
        pub id: u64,
        pub text: String,
        /// `"fact"`, `"preference"` or `"habit"`.
        pub kind: String,
        /// When it was learned (Unix seconds).
        pub created: u64,
    }
}

message! {
    /// A model or voice to choose from.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ModelChoice {
        pub id: String,
        pub name: String,
        /// Language model provider (`"open_ai"`), empty otherwise.
        pub provider: String,
        /// A short description (price tier, accent, ...).
        pub detail: String,
    }
}

message! {
    /// The models and voices Deepgram offers.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ModelCatalog {
        pub think: Vec<ModelChoice>,
        pub voices: Vec<ModelChoice>,
        pub listen: Vec<ModelChoice>,
    }
}

message! {
    /// An action the user allowed for good ("always allow").
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Permission {
        /// `app.action`.
        pub key: String,
        /// How it reads ("Files: rename files").
        pub label: String,
    }
}

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AgentError {
        /// This program may not do that (the interface calls are for the
        /// shell, the configuration calls for Settings).
        Denied = 1,
        NotFound = 2,
        /// An invalid setting.
        BadConfig = 3,
        /// Deepgram could not be reached.
        Network = 4,
        /// No Deepgram key is set.
        NoKey = 5,
        /// Deepgram refused the key.
        BadKey = 6,
        Busy = 7,
    }
}

impl core::fmt::Display for AgentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            AgentError::Denied => "not allowed",
            AgentError::NotFound => "not found",
            AgentError::BadConfig => "invalid setting",
            AgentError::Network => "Deepgram could not be reached",
            AgentError::NoKey => "no Deepgram API key is set",
            AgentError::BadKey => "Deepgram did not accept the API key",
            AgentError::Busy => "the agent is busy",
        })
    }
}

protocol! {
    /// The agent service.
    pub mod agent = "agent" {
        /// The agent's state.
        1 => fn status() -> AgentStatus;
        /// An application offers its abilities: `app` is the agent's end of
        /// a channel on which the application serves [`agentapp`].
        2 => fn register_app(app: Channel) -> Result<(), AgentError>;

        /// The shell attaches the agent's interface.
        10 => fn attach_ui() -> Result<UiLink, AgentError>;
        /// The user's answer to an approval request.
        11 => fn decide(id: u64, allow: bool, always: bool) -> Result<(), AgentError>;
        /// Starts a conversation (tray click, shortcut).
        12 => fn wake() -> Result<(), AgentError>;
        /// Ends the conversation.
        13 => fn sleep() -> Result<(), AgentError>;
        /// Whether the agent's window is open (where approvals appear).
        14 => fn set_window_open(open: bool) -> Result<(), AgentError>;
        /// Stops (or resumes) listening to the microphone.
        15 => fn set_muted(muted: bool) -> Result<(), AgentError>;

        /// Settings: the configuration.
        20 => fn config() -> Result<AgentConfig, AgentError>;
        21 => fn set_config(config: AgentConfig) -> Result<(), AgentError>;
        /// Stores the Deepgram API key (an empty key removes it).
        22 => fn set_api_key(key: String) -> Result<(), AgentError>;
        /// Checks the key with Deepgram; returns a short summary.
        23 => fn check_key() -> Result<String, AgentError>;
        /// The models and voices to choose from (asks Deepgram).
        24 => fn catalog() -> Result<ModelCatalog, AgentError>;
        /// Says a sample sentence in a voice.
        25 => fn preview_voice(voice: String) -> Result<(), AgentError>;
        /// What the agent remembers about the user.
        26 => fn memories() -> Result<Vec<MemoryItem>, AgentError>;
        27 => fn forget(id: u64) -> Result<(), AgentError>;
        28 => fn forget_all() -> Result<(), AgentError>;
        /// Actions the user always allows.
        29 => fn permissions() -> Result<Vec<Permission>, AgentError>;
        30 => fn revoke(key: String) -> Result<(), AgentError>;
    }
}
