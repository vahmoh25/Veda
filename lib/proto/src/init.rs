//! Protocols implemented by `init`: the service registry and the launcher.

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{enumeration, message, protocol, union};
use vrt::object::Channel;

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RegistryError {
        NotFound = 1,
        AlreadyRegistered = 2,
        Denied = 3,
    }
}

protocol! {
    /// The service registry. Every process receives a registry channel at
    /// startup (role `REGISTRY`).
    pub mod registry = "registry" {
        /// Connects `server_end` to the named service. The connection is
        /// queued until the service registers, so clients may start first.
        1 => fn connect(name: String, server_end: Channel) -> Result<(), RegistryError>;
        /// Registers a service. New connections arrive on `listener` as
        /// [`LISTENER_CONNECT`] events carrying the server end and the
        /// client's [`ClientIdentity`]. Names a system service has
        /// registered stay reserved for system services (`Denied`).
        2 => fn register(name: String, listener: Channel) -> Result<(), RegistryError>;
        /// Names of the registered services.
        3 => fn list() -> Vec<String>;
        /// A new registry channel (to hand to a child process).
        4 => fn clone_registry() -> Result<Channel, RegistryError>;
    }
}

/// Event ordinal on a service's listener channel: the payload is
/// `(Channel, ClientIdentity)`.
pub const LISTENER_CONNECT: u32 = 1;

message! {
    /// Who opened a connection, as `init` knows it: every process's registry
    /// channel belongs to that process, so the identity cannot be forged.
    /// Services that care (the agent trusts only the shell to approve its
    /// actions) read it with [`crate::accept_with_identity`].
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ClientIdentity {
        /// The client's process id (0 if unknown).
        pub koid: u64,
        /// The process name: a system service ("shell"), the id of an
        /// installed application ("settings") or a program's file name.
        pub name: String,
        /// Started by init as a system service.
        pub service: bool,
        /// Started through the launcher (applications and test programs).
        pub app: bool,
    }
}

message! {
    /// An installed application (from `apps/*.app` in the system image).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AppInfo {
        pub id: String,
        pub name: String,
        pub exe: String,
        pub icon: String,
        pub category: String,
        pub description: String,
        /// Shown pinned in the taskbar / start menu.
        pub pinned: bool,
    }
}

message! {
    /// A process as seen by the task manager.
    #[derive(Debug, Clone)]
    pub struct TaskInfo {
        pub koid: u64,
        pub name: String,
        pub state: u32,
        pub threads: u32,
        pub memory: u64,
        pub cpu_ns: u64,
        pub is_app: bool,
    }
}

union! {
    /// News about programs started by init, delivered on the channel
    /// returned by [`launcher::Client::watch`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TaskEvent {
        /// An application stopped unexpectedly (a CPU fault or a panic).
        /// `name` is its process name, which is the id of an installed
        /// application.
        1 => Crashed { koid: u64, name: String },
    }
}

/// Event ordinal of [`TaskEvent`]s on a watch channel.
pub const TASK_EVENT: u32 = 1;

enumeration! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LaunchError {
        NotFound = 1,
        BadImage = 2,
        NoMemory = 3,
        Denied = 4,
        Failed = 5,
    }
}

/// Power actions for [`launcher::Client::power`].
pub mod power {
    pub const SHUTDOWN: u32 = 1;
    pub const REBOOT: u32 = 2;
}

protocol! {
    /// Starts programs and manages running ones.
    pub mod launcher = "launcher" {
        /// Starts the executable at `path` with `args`; returns its koid.
        1 => fn launch(path: String, args: Vec<String>) -> Result<u64, LaunchError>;
        /// Starts an installed application by id.
        2 => fn launch_app(id: String, args: Vec<String>) -> Result<u64, LaunchError>;
        /// Installed applications.
        3 => fn apps() -> Vec<AppInfo>;
        /// Running processes.
        4 => fn tasks() -> Vec<TaskInfo>;
        /// Terminates a process.
        5 => fn kill(koid: u64) -> Result<(), LaunchError>;
        /// Shuts down or reboots the machine.
        6 => fn power(action: u32) -> Result<(), LaunchError>;
        /// A channel on which [`TaskEvent`]s arrive.
        7 => fn watch() -> Result<Channel, LaunchError>;
    }
}
