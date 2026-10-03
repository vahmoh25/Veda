//! Protocols implemented by `init`: the service registry and the launcher.

use alloc::string::String;
use alloc::vec::Vec;
use vipc::{enumeration, message, protocol};
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
        /// [`LISTENER_CONNECT`] events carrying the server end.
        2 => fn register(name: String, listener: Channel) -> Result<(), RegistryError>;
        /// Names of the registered services.
        3 => fn list() -> Vec<String>;
        /// A new registry channel (to hand to a child process).
        4 => fn clone_registry() -> Result<Channel, RegistryError>;
    }
}

/// Event ordinal on a service's listener channel: payload is a `Channel`.
pub const LISTENER_CONNECT: u32 = 1;

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
    }
}
