//! `init` â€” the first user-space process.
//!
//! * Starts the system services and the desktop shell from the initrd, giving
//!   each only the capabilities it needs (narrow hardware resources).
//! * Implements the service **registry**: every process gets a registry
//!   channel; services register by name and clients connect by name.
//!   Connections to a service that has not registered yet are queued.
//! * Implements the **launcher** service: starting programs and installed
//!   applications, listing and killing tasks, and power control.

#![no_std]
#![no_main]

extern crate alloc;

mod apps;
mod boot;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vabi::startup::role;
use vabi::{resource_kind, signals};
use vipc::WaitSet;
use vproto::init::{AppInfo, LISTENER_CONNECT, LaunchError, RegistryError, TaskInfo, launcher, registry};
use vrt::object::{Channel, Handle, Process, Resource, Vmo};
use vrt::println;

vrt::entry!(main);

/// What a channel served by init speaks.
enum ConnKind {
    /// A process's registry channel (the koid of the process, if known).
    Registry { owner: u64 },
    /// A launcher connection.
    Launcher,
}

struct Conn {
    channel: Channel,
    kind: ConnKind,
}

/// A process started by init.
struct Child {
    name: String,
    process: Process,
    /// Restart this service if it exits.
    service: bool,
    app: bool,
}

pub struct Init {
    root: Resource,
    initrd: initrd::Archive<'static>,
    initrd_vmo: Vmo,
    framebuffer: Option<Vmo>,
    boot_info: Option<Vmo>,
    services: BTreeMap<String, (Channel, u64)>,
    pending: BTreeMap<String, Vec<Channel>>,
    conns: BTreeMap<u64, Conn>,
    children: BTreeMap<u64, Child>,
    apps: Vec<AppInfo>,
    next_key: u64,
}

const CHILD_KEY: u64 = 1 << 48;

impl Init {
    fn add_conn(&mut self, channel: Channel, kind: ConnKind) {
        self.next_key += 1;
        self.conns.insert(self.next_key, Conn { channel, kind });
    }

    /// A fresh registry channel for a new process (we keep the server end).
    fn new_registry_channel(&mut self) -> Option<Channel> {
        let (ours, theirs) = Channel::create().ok()?;
        self.add_conn(ours, ConnKind::Registry { owner: 0 });
        Some(theirs)
    }

    fn image(&self, path: &str) -> Option<&'static [u8]> {
        let p = path.trim_start_matches('/').trim_start_matches("system/");
        self.initrd.find(p).map(|f| f.data)
    }

    /// Starts `path` from the system image with a registry channel plus
    /// `extra` handles.
    pub fn spawn(&mut self, name: &str, path: &str, args: &[String], extra: Vec<(u32, Handle)>, service: bool) -> Result<u64, LaunchError> {
        let image = self.image(path).ok_or(LaunchError::NotFound)?;
        let registry = self.new_registry_channel().ok_or(LaunchError::NoMemory)?;
        let mut spawn = vrt::process::Spawn::new(name).handle(role::REGISTRY, registry.into_handle());
        for a in args {
            spawn = spawn.arg(a);
        }
        spawn = spawn.env("HOME=/home/user");
        for (r, h) in extra {
            spawn = spawn.handle(r, h);
        }
        let process = spawn.start(image).map_err(|e| {
            println!("init: failed to start {}: {}", path, e);
            match e {
                vrt::process::SpawnError::BadImage(_) => LaunchError::BadImage,
                vrt::process::SpawnError::Kernel(vabi::Error::NoMemory) => LaunchError::NoMemory,
                _ => LaunchError::Failed,
            }
        })?;
        let koid = process.0.koid();
        // Attribute the registry channel we just created to this process.
        if let Some(c) = self.conns.get_mut(&self.next_key) {
            c.kind = ConnKind::Registry { owner: koid };
        }
        self.children.insert(koid, Child { name: name.to_string(), process, service, app: !service });
        Ok(koid)
    }

    /// Hands a queued or new connection to a registered service.
    fn deliver(&mut self, name: &str, server_end: Channel) {
        if let Some((listener, _)) = self.services.get(name) {
            match vipc::send_event(listener, LISTENER_CONNECT, server_end) {
                Ok(()) => return,
                Err(_) => {
                    // The service went away; forget it. The connection is lost
                    // (the client will see PEER_CLOSED).
                    println!("init: service '{}' is gone", name);
                    self.services.remove(name);
                    return;
                }
            }
        }
        self.pending.entry(name.to_string()).or_default().push(server_end);
    }

    fn handle_registry(&mut self, key: u64, msg: vrt::Message) {
        struct Reg<'a> {
            init: &'a mut Init,
            key: u64,
        }
        impl registry::Server for Reg<'_> {
            fn connect(&mut self, name: String, server_end: Channel) -> Result<(), RegistryError> {
                if name == launcher::NAME {
                    self.init.add_conn(server_end, ConnKind::Launcher);
                } else {
                    self.init.deliver(&name, server_end);
                }
                Ok(())
            }

            fn register(&mut self, name: String, listener: Channel) -> Result<(), RegistryError> {
                if self.init.services.contains_key(&name) || name == launcher::NAME || name == registry::NAME {
                    return Err(RegistryError::AlreadyRegistered);
                }
                let owner = match self.init.conns.get(&self.key) {
                    Some(Conn { kind: ConnKind::Registry { owner }, .. }) => *owner,
                    _ => 0,
                };
                println!("init: service '{}' registered", name);
                self.init.services.insert(name.clone(), (listener, owner));
                for ch in self.init.pending.remove(&name).unwrap_or_default() {
                    self.init.deliver(&name, ch);
                }
                Ok(())
            }

            fn list(&mut self) -> Vec<String> {
                self.init.services.keys().cloned().collect()
            }

            fn clone_registry(&mut self) -> Result<Channel, RegistryError> {
                self.init.new_registry_channel().ok_or(RegistryError::Denied)
            }
        }
        let mut reg = Reg { init: self, key };
        let reply = registry::dispatch(&mut reg, msg);
        if let (Ok(reply), Some(conn)) = (reply, self.conns.get(&key)) {
            let _ = reply.send(&conn.channel);
        }
    }

    fn handle_launcher(&mut self, key: u64, msg: vrt::Message) {
        struct Launch<'a>(&'a mut Init);
        impl launcher::Server for Launch<'_> {
            fn launch(&mut self, path: String, args: Vec<String>) -> Result<u64, LaunchError> {
                let name = path.rsplit('/').next().unwrap_or(&path).trim_end_matches(".exe").to_string();
                self.0.spawn(&name, &path, &args, Vec::new(), false)
            }

            fn launch_app(&mut self, id: String, args: Vec<String>) -> Result<u64, LaunchError> {
                let app = self.0.apps.iter().find(|a| a.id == id).cloned().ok_or(LaunchError::NotFound)?;
                self.0.spawn(&app.id, &app.exe, &args, Vec::new(), false)
            }

            fn apps(&mut self) -> Vec<AppInfo> {
                self.0.apps.clone()
            }

            fn tasks(&mut self) -> Vec<TaskInfo> {
                let mut infos = alloc::vec![vabi::ProcessInfo::default(); 256];
                let n = vrt::object::process_list(&mut infos).unwrap_or(0).min(infos.len());
                infos[..n]
                    .iter()
                    .filter(|p| p.state == vabi::process_state::RUNNING)
                    .map(|p| TaskInfo {
                        koid: p.koid,
                        name: p.name().to_string(),
                        state: p.state,
                        threads: p.threads,
                        memory: p.memory_bytes,
                        cpu_ns: p.cpu_time_ns,
                        is_app: self.0.children.get(&p.koid).is_some_and(|c| c.app),
                    })
                    .collect()
            }

            fn kill(&mut self, koid: u64) -> Result<(), LaunchError> {
                if let Some(child) = self.0.children.get(&koid) {
                    return child.process.kill().map_err(|_| LaunchError::Failed);
                }
                let res = self.0.root.create(resource_kind::PROCESS, 0, 0).map_err(|_| LaunchError::Denied)?;
                let p = vrt::object::process_open(&res, koid).map_err(|_| LaunchError::NotFound)?;
                p.kill().map_err(|_| LaunchError::Failed)
            }

            fn power(&mut self, action: u32) -> Result<(), LaunchError> {
                println!("init: power action {}", action);
                let res = self.0.root.create(resource_kind::POWER, 0, 0).map_err(|_| LaunchError::Denied)?;
                vrt::object::power(&res, action as usize).map_err(|_| LaunchError::Failed)
            }
        }
        let reply = launcher::dispatch(&mut Launch(self), msg);
        if let (Ok(reply), Some(conn)) = (reply, self.conns.get(&key)) {
            let _ = reply.send(&conn.channel);
        }
    }

    fn on_child_exit(&mut self, koid: u64) {
        let Some(child) = self.children.remove(&koid) else { return };
        let code = child.process.info().map(|i| i.exit_code).unwrap_or(0);
        if code == vabi::EXIT_CODE_CRASHED {
            println!("init: {} crashed", child.name);
        } else {
            println!("init: {} exited with code {}", child.name, code);
        }
        // Drop the services it provided.
        self.services.retain(|name, (_, owner)| {
            let keep = *owner != koid;
            if !keep {
                println!("init: service '{}' unregistered", name);
            }
            keep
        });
        if child.service {
            // Services are expected to run forever; a restart policy could go here.
            println!("init: warning: system service {} is no longer running", child.name);
        }
    }

    fn run(&mut self) -> ! {
        loop {
            let mut ws = WaitSet::new();
            for (&k, c) in &self.conns {
                ws.add(c.channel.raw(), signals::READABLE | signals::PEER_CLOSED, k);
            }
            for (&koid, c) in &self.children {
                ws.add(c.process.raw(), signals::TERMINATED, CHILD_KEY | koid);
            }
            let ready = match ws.wait(vabi::DEADLINE_INFINITE) {
                Ok(r) => r,
                Err(e) => {
                    println!("init: wait failed: {}", e);
                    continue;
                }
            };
            for (key, observed) in ready {
                if key & CHILD_KEY != 0 {
                    self.on_child_exit(key & !CHILD_KEY);
                    continue;
                }
                if observed & signals::READABLE != 0 {
                    // Drain everything queued on this connection.
                    while let Some(conn) = self.conns.get(&key) {
                        let Ok(msg) = conn.channel.read() else { break };
                        match conn.kind {
                            ConnKind::Registry { .. } => self.handle_registry(key, msg),
                            ConnKind::Launcher => self.handle_launcher(key, msg),
                        }
                    }
                } else if observed & signals::PEER_CLOSED != 0 {
                    self.conns.remove(&key);
                }
            }
        }
    }
}

fn main() -> i32 {
    println!("init: starting Vindows {}", env!("CARGO_PKG_VERSION"));
    let root = Resource::from_handle(vrt::env::take_handle(role::ROOT_RESOURCE).expect("init: no root resource"));
    let initrd_vmo = Vmo::from_handle(vrt::env::take_handle(role::INITRD).expect("init: no initrd"));
    let size = initrd_vmo.size().expect("init: initrd size");
    let addr = initrd_vmo.map(0, size, vabi::map_flags::READ).expect("init: cannot map the initrd");
    // SAFETY: the read-only mapping lives for the rest of init's life.
    let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    let archive = initrd::Archive::open(bytes).expect("init: the initrd is corrupt");

    let mut init = Init {
        root,
        initrd: archive,
        initrd_vmo,
        framebuffer: vrt::env::take_handle(role::FRAMEBUFFER).map(Vmo::from_handle),
        boot_info: vrt::env::take_handle(role::BOOT_INFO).map(Vmo::from_handle),
        services: BTreeMap::new(),
        pending: BTreeMap::new(),
        conns: BTreeMap::new(),
        children: BTreeMap::new(),
        apps: Vec::new(),
        next_key: 0,
    };
    init.apps = apps::load(&init.initrd);
    println!("init: {} application(s) installed", init.apps.len());
    boot::start_system(&mut init);
    boot::start_requested(&mut init);
    init.run()
}
