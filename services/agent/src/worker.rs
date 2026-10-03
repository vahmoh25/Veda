//! The worker thread: runs what the agent decided to do.
//!
//! Function calls from the language model arrive as [`Job::Call`]s. The
//! worker classifies each call (its [`Risk`], from the system function or
//! from the application's description of the action) and either runs it or
//! hands it back as an [`Outcome::Approval`] — nothing that needs the
//! user's consent runs before the user gave it. Applications register here
//! ([`Job::App`]); their abilities are remembered for the agent's prompt.
//!
//! Everything slow lives here, off the conversation's thread: calls into
//! applications (with timeouts, as an application may hang), the file
//! system, the launcher, the shell and the network service.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vagent::policy;
use vagent::tools::{self, names};
use vjson::{Value, object};
use vproto::agent::{AppAgentInfo, Risk, agentapp};
use vproto::display::{WindowKind, display};
use vproto::init::{AppInfo, launcher};
use vproto::shell::shell;
use vproto::wlan::ConnState;
use vrt::object::{Channel, Event};
use vrt::sync::{Condvar, Mutex};
use vrt::time::{DateTime, Duration};

use crate::shared::Shared;

/// The user's home directory (relative paths and `~` start here).
const HOME: &str = "/home/user";
/// Largest text the agent reads from a file at once.
const MAX_READ: usize = 24 * 1024;
/// How long an application may take to answer.
const APP_TIMEOUT_NS: u64 = 3_000_000_000;
/// How long a starting application has to register.
const START_TIMEOUT: Duration = Duration::from_secs(6);

/// Work for the worker.
pub enum Job {
    /// An application registered: it serves `agentapp` on `channel`.
    App { name: String, channel: Channel },
    /// A function call from the language model (`approved` once the user
    /// allowed it).
    Call { id: String, name: String, args: Value, approved: bool },
}

/// An action waiting for the user's consent.
#[derive(Debug, Clone)]
pub struct Pending {
    pub name: String,
    pub args: Value,
    /// Who acts ("Files", an application's name).
    pub app: String,
    pub action: String,
    pub detail: String,
    pub risk: Risk,
    /// Permission key for "always allow".
    pub key: String,
}

/// What became of a call.
pub enum Outcome {
    /// The result for the language model (JSON text).
    Result(String),
    /// The call needs the user's approval first.
    Approval(Pending),
    /// The agent wants to end the conversation.
    End,
}

/// Finished work.
pub enum Done {
    Call { id: String, name: String, outcome: Outcome },
}

struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    done: Mutex<VecDeque<Done>>,
}

/// The main loop's handle on the worker.
pub struct Worker {
    q: Arc<Queue>,
    event: Event,
}

impl Worker {
    pub fn start(shared: Arc<Mutex<Shared>>) -> Option<Worker> {
        let event = Event::create().ok()?;
        let signal = Event::from_handle(event.0.duplicate(None).ok()?);
        let q = Arc::new(Queue {
            jobs: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            done: Mutex::new(VecDeque::new()),
        });
        let wq = q.clone();
        vrt::thread::Builder::new()
            .name("actions")
            .spawn(move || {
                let mut ctx = Ctx::new(shared, wq.clone(), signal);
                ctx.run();
            })
            .ok()?;
        Some(Worker { q, event })
    }

    pub fn submit(&self, job: Job) {
        self.q.jobs.lock().push_back(job);
        self.q.wake.notify_one();
    }

    /// Finished work (clears the event).
    pub fn take(&self) -> Vec<Done> {
        let _ = self.event.clear();
        self.q.done.lock().drain(..).collect()
    }

    pub fn handle(&self) -> RawHandle {
        self.event.raw()
    }

    /// For registering applications from another thread.
    pub fn registrar(&self) -> Registrar {
        Registrar(self.q.clone())
    }
}

/// Hands applications' registrations to the worker (Settings is served on
/// its own thread, and is an application the agent can use too).
#[derive(Clone)]
pub struct Registrar(Arc<Queue>);

impl Registrar {
    pub fn register(&self, name: String, channel: Channel) {
        self.0.jobs.lock().push_back(Job::App { name, channel });
        self.0.wake.notify_one();
    }
}

/// A registered application.
struct AppLink {
    client: agentapp::Client,
    channel: RawHandle,
}

impl AppLink {
    fn alive(&self) -> bool {
        let mut item =
            vabi::WaitItem { handle: self.channel, signals: vabi::signals::PEER_CLOSED, ..Default::default() };
        let _ = vrt::object::wait_many(core::slice::from_mut(&mut item), 0);
        item.observed & vabi::signals::PEER_CLOSED == 0
    }
}

struct Ctx {
    shared: Arc<Mutex<Shared>>,
    q: Arc<Queue>,
    signal: Event,
    /// Registered applications by id (the newest instance wins).
    apps: BTreeMap<String, AppLink>,
    fs: vfiles::Fs,
}

fn connect<T>(name: &str, make: fn(Channel) -> T) -> Option<T> {
    vproto::connect(name).ok().map(make)
}

/// A path the agent may touch, resolved against the home directory.
fn resolve(path: &str) -> Result<String, String> {
    let p = vfiles::path::resolve(HOME, path.trim());
    if p == "/home/.private" || p.starts_with("/home/.private/") {
        return Err("that location is private".into());
    }
    Ok(p)
}

/// How a path reads aloud ("~/Documents/notes.txt").
fn show(path: &str) -> String {
    vfiles::path::display_path(path)
}

/// Local date and time as people say it.
pub fn now_text() -> String {
    let t = DateTime::now();
    format!("{} {} {} {}, {:02}:{:02}", t.weekday_name(), t.day, t.month_name(), t.year, t.hour, t.minute)
}

impl Ctx {
    fn new(shared: Arc<Mutex<Shared>>, q: Arc<Queue>, signal: Event) -> Ctx {
        Ctx { shared, q, signal, apps: BTreeMap::new(), fs: vfiles::Fs::connect() }
    }

    fn run(&mut self) {
        loop {
            let job = {
                let mut jobs = self.q.jobs.lock();
                loop {
                    if let Some(j) = jobs.pop_front() {
                        break j;
                    }
                    jobs = self.q.wake.wait(jobs);
                }
            };
            match job {
                Job::App { name, channel } => self.register(name, channel),
                Job::Call { id, name, args, approved } => {
                    let outcome = self.call(&name, &args, approved);
                    self.q.done.lock().push_back(Done::Call { id, name, outcome });
                    let _ = self.signal.signal();
                }
            }
        }
    }

    /// Takes application registrations that queued up (while a call waits
    /// for an application to start).
    fn take_registrations(&mut self) {
        let regs: Vec<(String, Channel)> = {
            let mut jobs = self.q.jobs.lock();
            let mut regs = Vec::new();
            let mut rest = VecDeque::new();
            while let Some(j) = jobs.pop_front() {
                match j {
                    Job::App { name, channel } => regs.push((name, channel)),
                    other => rest.push_back(other),
                }
            }
            *jobs = rest;
            regs
        };
        for (name, channel) in regs {
            self.register(name, channel);
        }
    }

    fn register(&mut self, name: String, channel: Channel) {
        let raw = channel.raw();
        let client = agentapp::Client::new(channel);
        client.set_timeout(APP_TIMEOUT_NS);
        match client.describe() {
            Ok(info) => {
                vrt::println!("{} offers {} action(s)", name, info.actions.len());
                self.shared.lock().set_app(&name, info);
                self.apps.insert(name, AppLink { client, channel: raw });
            }
            Err(e) => vrt::println!("{} did not describe itself: {:?}", name, e),
        }
    }

    /// The installed applications.
    fn installed(&self) -> Vec<AppInfo> {
        connect(launcher::NAME, launcher::Client::new).and_then(|l| l.apps().ok()).unwrap_or_default()
    }

    /// Finds an installed application by id or name.
    fn find_app(&self, query: &str) -> Option<AppInfo> {
        let q = query.trim().to_lowercase();
        let apps = self.installed();
        apps.iter()
            .find(|a| a.id.to_lowercase() == q || a.name.to_lowercase() == q)
            .or_else(|| apps.iter().find(|a| a.name.to_lowercase().contains(&q) || q.contains(&a.name.to_lowercase())))
            .cloned()
    }

    /// The live link to an application, starting it if needed.
    fn ensure_running(&mut self, app: &AppInfo) -> Result<(), String> {
        self.take_registrations();
        if self.apps.get(&app.id).is_some_and(AppLink::alive) {
            return Ok(());
        }
        self.apps.remove(&app.id);
        // An open application that has not registered yet (the agent
        // restarted) registers again on its own: wait for it rather than
        // starting a second copy.
        if self.find_window(&app.id).is_none() {
            let launcher = connect(launcher::NAME, launcher::Client::new).ok_or("the launcher is not available")?;
            match launcher.launch_app(app.id.clone(), Vec::new()) {
                Ok(Ok(_)) => {}
                _ => return Err(format!("{} could not be started", app.name)),
            }
            self.note_use(&app.id);
        }
        let end = vrt::time::deadline_after(START_TIMEOUT);
        while vrt::time::now_ns() < end {
            vrt::time::sleep(Duration::from_millis(50));
            self.take_registrations();
            if self.apps.get(&app.id).is_some_and(AppLink::alive) {
                return Ok(());
            }
        }
        Err(format!("{} started but does not take requests from the agent", app.name))
    }

    fn note_use(&self, app: &str) {
        let t = DateTime::now();
        let mut s = self.shared.lock();
        s.memory.note_app_use(app, vrt::time::unix_time_ns() / 1_000_000_000, t.hour as u32);
        s.save_memory();
    }

    /// Runs (or classifies) one call.
    fn call(&mut self, name: &str, args: &Value, approved: bool) -> Outcome {
        if name == names::USE_APP {
            return self.use_app(args, approved);
        }
        if !tools::is_system(name) {
            return Outcome::Result(tools::error(&format!("there is no function called {name}")));
        }
        let fs = &self.fs;
        let exists = |p: &str| resolve(p).map(|p| fs.exists(&p)).unwrap_or(false);
        let risk = match tools::risk(name, args, &exists) {
            tools::Risk::Routine => Risk::Routine,
            tools::Risk::Sensitive => Risk::Sensitive,
            tools::Risk::Destructive => Risk::Destructive,
        };
        let key = policy::key("system", &format!("{name}.{}", args.str("operation").unwrap_or("")));
        if !approved && self.needs_approval(&key, risk) {
            let (action, detail) = tools::describe(name, args);
            let app = match name {
                names::FILES => "Files",
                names::WIFI => "Wi-Fi",
                names::MEMORY => "Memory",
                _ => "System",
            };
            return Outcome::Approval(Pending {
                name: name.into(),
                args: args.clone(),
                app: app.into(),
                action,
                detail,
                risk,
                key,
            });
        }
        let result = match name {
            names::GET_STATUS => Ok(self.status()),
            names::LIST_APPS => Ok(self.list_apps()),
            names::OPEN_APP => self.open_app(args),
            names::APP_ACTIONS => self.app_actions(args),
            names::READ_APP => self.read_app(args),
            names::WINDOW => self.window(args),
            names::FILES => self.files(args),
            names::VOLUME => self.volume(args),
            names::WIFI => self.wifi(args),
            names::WALLPAPER => self.wallpaper(args),
            names::NOTIFY => self.notify(args),
            names::TIMER => self.timer(args),
            names::MEMORY => self.memory(args),
            names::SYSTEM => self.system(args),
            names::TASKS => self.tasks(args),
            names::END_CONVERSATION => return Outcome::End,
            _ => Err("not implemented".into()),
        };
        Outcome::Result(match result {
            Ok(v) => tools::ok(v),
            Err(e) => tools::error(&e),
        })
    }

    fn needs_approval(&self, key: &str, risk: Risk) -> bool {
        let r = match risk {
            Risk::Routine => tools::Risk::Routine,
            Risk::Sensitive => tools::Risk::Sensitive,
            Risk::Destructive => tools::Risk::Destructive,
        };
        self.shared.lock().permissions.needs_approval(key, r)
    }

    fn status(&self) -> Value {
        let mut v = object! { "time" => now_text() };
        if let Some(d) = connect(display::NAME, display::Client::new)
            && let Ok(list) = d.list_windows()
        {
            let windows: Vec<Value> = list
                .iter()
                .filter(|w| matches!(w.kind, WindowKind::Normal | WindowKind::Borderless))
                .map(|w| object! { "app" => w.app_id.as_str(), "title" => w.title.as_str(), "in_front" => w.focused, "state" => format!("{:?}", w.state).to_lowercase() })
                .collect();
            v.set("windows", windows);
        }
        if let Some(a) = connect(vproto::audio::audio::NAME, vproto::audio::audio::Client::new)
            && let Ok(s) = a.status()
        {
            v.set("volume", (s.master_volume * 100.0 + 0.5) as u32);
            v.set("muted", s.muted);
        }
        if let Ok(s) = vnet::status() {
            v.set("internet", format!("{:?}", s.connectivity).to_lowercase());
        }
        if let Ok(w) = vnet::wifi::status() {
            v.set("wifi", object! { "state" => format!("{:?}", w.state).to_lowercase(), "network" => w.name.as_str() });
        }
        v
    }

    fn list_apps(&mut self) -> Value {
        self.take_registrations();
        let infos = self.shared.lock().apps.clone();
        let apps: Vec<Value> = self
            .installed()
            .iter()
            .map(|a| {
                let mut v = object! {
                    "id" => a.id.as_str(),
                    "name" => a.name.as_str(),
                    "about" => a.description.as_str(),
                    "running" => self.apps.get(&a.id).is_some_and(AppLink::alive),
                };
                if let Some(info) = infos.get(&a.id) {
                    v.set("actions", info.actions.iter().map(|x| Value::from(x.name.as_str())).collect::<Vec<_>>());
                }
                v
            })
            .collect();
        Value::Array(apps)
    }

    fn app_arg(&self, args: &Value) -> Result<AppInfo, String> {
        let q = args.str("app").ok_or("which application?")?;
        self.find_app(q).ok_or_else(|| format!("there is no application called {q}"))
    }

    fn open_app(&mut self, args: &Value) -> Result<Value, String> {
        let app = self.app_arg(args)?;
        let file = args.str("file").filter(|f| !f.trim().is_empty()).map(resolve).transpose()?;
        // An open application without a file to open comes to the front.
        if file.is_none()
            && let Some(id) = self.find_window(&app.id)
        {
            self.window_action(id, "focus")?;
            return Ok(object! { "brought_to_front" => app.name.as_str() });
        }
        let launcher = connect(launcher::NAME, launcher::Client::new).ok_or("the launcher is not available")?;
        let argv: Vec<String> = file.iter().cloned().collect();
        match launcher.launch_app(app.id.clone(), argv) {
            Ok(Ok(_)) => {
                self.note_use(&app.id);
                Ok(object! { "opened" => app.name.as_str(), "file" => file.as_deref().map(show) })
            }
            _ => Err(format!("{} could not be started", app.name)),
        }
    }

    fn app_actions(&mut self, args: &Value) -> Result<Value, String> {
        let app = self.app_arg(args)?;
        self.ensure_running(&app)?;
        let link = self.apps.get(&app.id).ok_or("the application went away")?;
        let info = link.client.describe().map_err(|_| format!("{} is not answering", app.name))?;
        self.shared.lock().set_app(&app.id, info.clone());
        Ok(describe_info(&info))
    }

    fn use_app(&mut self, args: &Value, approved: bool) -> Outcome {
        let r = (|| -> Result<Outcome, String> {
            let app = self.app_arg(args)?;
            let action = args.str("action").ok_or("which action?")?.to_string();
            let call_args = match args.get("arguments") {
                Some(v @ Value::Object(_)) => v.clone(),
                Some(Value::String(s)) => {
                    vjson::parse(s).ok().filter(|v| v.as_object().is_some()).unwrap_or_else(Value::object)
                }
                _ => Value::object(),
            };
            self.ensure_running(&app)?;
            let info = self.shared.lock().apps.get(&app.id).cloned();
            let spec = info.as_ref().and_then(|i| i.actions.iter().find(|a| a.name == action)).cloned();
            let Some(spec) = spec else {
                let known: Vec<&str> = info.iter().flat_map(|i| i.actions.iter().map(|a| a.name.as_str())).collect();
                return Err(format!("{} has no action called {action}; it offers: {}", app.name, known.join(", ")));
            };
            let key = policy::key(&app.id, &action);
            if !approved && self.needs_approval(&key, spec.risk) {
                return Ok(Outcome::Approval(Pending {
                    name: names::USE_APP.into(),
                    args: args.clone(),
                    app: app.name.clone(),
                    action: spec.description.clone(),
                    detail: describe_args(&call_args),
                    risk: spec.risk,
                    key,
                }));
            }
            let link = self.apps.get(&app.id).ok_or("the application went away")?;
            let result = link
                .client
                .invoke(action.clone(), call_args.to_string())
                .map_err(|_| format!("{} did not answer", app.name))?;
            Ok(Outcome::Result(if result.ok {
                tools::ok(vjson::parse(&result.result).unwrap_or(Value::String(result.result)))
            } else {
                tools::error(&result.result)
            }))
        })();
        r.unwrap_or_else(|e| Outcome::Result(tools::error(&e)))
    }

    fn read_app(&mut self, args: &Value) -> Result<Value, String> {
        let app = self.app_arg(args)?;
        self.take_registrations();
        let link = self.apps.get(&app.id).filter(|l| l.alive()).ok_or_else(|| format!("{} is not open", app.name))?;
        let state = link.client.state().map_err(|_| format!("{} did not answer", app.name))?;
        Ok(vjson::parse(&state).unwrap_or(Value::String(state)))
    }

    /// The id of a window of `app` (or with `app` in its title).
    fn find_window(&self, app: &str) -> Option<u32> {
        let d = connect(display::NAME, display::Client::new)?;
        let list = d.list_windows().ok()?;
        let q = app.to_lowercase();
        list.iter()
            .filter(|w| matches!(w.kind, WindowKind::Normal | WindowKind::Borderless))
            .rev()
            .find(|w| w.app_id.to_lowercase() == q || w.title.to_lowercase().contains(&q))
            .map(|w| w.id)
    }

    fn window_action(&self, id: u32, op: &str) -> Result<(), String> {
        let s = connect(shell::NAME, shell::Client::new).ok_or("the desktop is not available")?;
        match s.window_action(id, op.into()) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(format!("the desktop refused ({e:?})")),
            Err(_) => Err("the desktop did not answer".into()),
        }
    }

    fn window(&mut self, args: &Value) -> Result<Value, String> {
        let op = args.str("operation").unwrap_or("list");
        match op {
            "list" => Ok(self.status()["windows"].clone()),
            "show_desktop" => {
                self.window_action(0, "show_desktop")?;
                Ok(Value::Null)
            }
            _ => {
                let target = args.str("window").ok_or("which window?")?;
                let id = self
                    .find_app(target)
                    .and_then(|a| self.find_window(&a.id))
                    .or_else(|| self.find_window(target))
                    .ok_or_else(|| format!("no open window matches {target}"))?;
                self.window_action(id, op)?;
                Ok(Value::Null)
            }
        }
    }

    fn files(&mut self, args: &Value) -> Result<Value, String> {
        let op = args.str("operation").unwrap_or("");
        let path = resolve(args.str("path").unwrap_or("~"))?;
        let err = |e: vfiles::Error| e.to_string();
        match op {
            "list" => {
                let entries = self.fs.read_dir(&path).map_err(err)?;
                let list: Vec<Value> = entries
                    .iter()
                    .filter(|e| !e.name.starts_with('.'))
                    .take(150)
                    .map(|e| object! { "name" => e.name.as_str(), "folder" => e.is_dir, "size" => vfiles::format::human_size(e.size) })
                    .collect();
                Ok(object! { "folder" => show(&path), "entries" => list, "total" => entries.len() })
            }
            "find" => {
                let what = args.str("text").or(args.str("to")).ok_or("what should I look for?")?.to_lowercase();
                let mut found = Vec::new();
                self.find(&path, &what, 0, &mut found);
                Ok(object! { "matches" => found.into_iter().map(Value::from).collect::<Vec<_>>() })
            }
            "info" => {
                let st = self.fs.stat(&path).map_err(err)?;
                Ok(object! {
                    "path" => show(&path),
                    "folder" => st.is_dir,
                    "size" => vfiles::format::human_size(st.size),
                    "modified" => vfiles::format::format_time(st.modified),
                    "kind" => vfiles::kind::kind_name(vfiles::path::file_name(&path), st.is_dir),
                })
            }
            "read" => {
                let bytes = self.fs.read(&path).map_err(err)?;
                match core::str::from_utf8(&bytes) {
                    Ok(text) => {
                        let mut end = text.len().min(MAX_READ);
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        Ok(object! { "path" => show(&path), "text" => &text[..end], "truncated" => end < text.len() })
                    }
                    Err(_) => Ok(object! {
                        "path" => show(&path),
                        "kind" => vfiles::kind::kind_name(vfiles::path::file_name(&path), false),
                        "size" => vfiles::format::human_size(bytes.len() as u64),
                        "note" => "not a text file; open it in its application instead",
                    }),
                }
            }
            "write" | "append" => {
                let text = args.str("text").unwrap_or("");
                if !path.starts_with("/home/") && !path.starts_with("/tmp/") {
                    return Err("files can only be written in the home folder".into());
                }
                let _ = self.fs.mkdir_all(vfiles::path::parent(&path));
                if op == "write" {
                    self.fs.write(&path, text.as_bytes()).map_err(err)?;
                } else {
                    self.fs.append(&path, text.as_bytes()).map_err(err)?;
                }
                Ok(object! { "saved" => show(&path) })
            }
            "create_folder" => {
                self.fs.mkdir_all(&path).map_err(err)?;
                Ok(object! { "created" => show(&path) })
            }
            "copy" | "move" | "rename" => {
                let to = args.str("to").ok_or("where to?")?;
                let mut dest = if op == "rename" && !to.contains('/') && !to.starts_with('~') {
                    vfiles::path::join(vfiles::path::parent(&path), to)
                } else {
                    resolve(to)?
                };
                if self.fs.is_dir(&dest) {
                    dest = vfiles::path::join(&dest, vfiles::path::file_name(&path));
                }
                if op == "copy" {
                    self.fs.copy(&path, &dest).map_err(err)?;
                } else {
                    self.fs.move_to(&path, &dest).map_err(err)?;
                }
                Ok(object! { "done" => op, "from" => show(&path), "to" => show(&dest) })
            }
            "delete" => {
                if path == HOME || !path.starts_with("/home/user/") {
                    return Err("only files and folders inside the home folder can be deleted".into());
                }
                self.fs.remove_all(&path).map_err(err)?;
                Ok(object! { "deleted" => show(&path) })
            }
            other => Err(format!("unknown file operation {other}")),
        }
    }

    /// Paths under `dir` whose names contain `what` (bounded search).
    fn find(&self, dir: &str, what: &str, depth: u32, out: &mut Vec<String>) {
        if depth > 6 || out.len() >= 40 {
            return;
        }
        let Ok(entries) = self.fs.read_dir(dir) else { return };
        for e in entries {
            if e.name.starts_with('.') {
                continue;
            }
            let p = vfiles::path::join(dir, &e.name);
            if e.name.to_lowercase().contains(what) {
                out.push(show(&p));
            }
            if e.is_dir {
                self.find(&p, what, depth + 1, out);
            }
        }
    }

    fn volume(&self, args: &Value) -> Result<Value, String> {
        let a =
            connect(vproto::audio::audio::NAME, vproto::audio::audio::Client::new).ok_or("sound is not available")?;
        let s = a.status().map_err(|_| "the sound service did not answer")?;
        let mut level = (s.master_volume * 100.0 + 0.5) as i64;
        let mut muted = s.muted;
        if let Some(l) = args["level"].as_f64() {
            level = l as i64;
            muted = false;
        }
        if let Some(c) = args["change"].as_f64() {
            level += c as i64;
            muted = false;
        }
        if let Some(m) = args["mute"].as_bool() {
            muted = m;
        }
        let level = level.clamp(0, 100);
        a.set_master(level as f32 / 100.0, muted).map_err(|_| "the sound service did not answer")?;
        Ok(object! { "volume" => level, "muted" => muted })
    }

    fn wifi(&self, args: &Value) -> Result<Value, String> {
        use vnet::wifi;
        let err = |e: wifi::WifiError| e.to_string();
        match args.str("operation").unwrap_or("status") {
            "status" => {
                let s = wifi::status().map_err(err)?;
                Ok(object! {
                    "state" => format!("{:?}", s.state).to_lowercase(),
                    "network" => s.name.as_str(),
                    "radio_on" => s.state != ConnState::RadioOff && s.state != ConnState::NoAdapter,
                })
            }
            "scan" => {
                let _ = wifi::scan();
                vrt::time::sleep(Duration::from_secs(3));
                let nets = wifi::networks().map_err(err)?;
                let list: Vec<Value> = nets
                    .iter()
                    .take(20)
                    .map(|n| object! { "name" => n.name.as_str(), "security" => format!("{:?}", n.security), "bars" => n.bars, "saved" => n.saved, "connected" => n.connected })
                    .collect();
                Ok(Value::Array(list))
            }
            "connect" => {
                let net = args.str("network").ok_or("which network?")?;
                wifi::connect(net, args.str("password").filter(|p| !p.is_empty()), true).map_err(err)?;
                let s = wifi::wait_for(Duration::from_secs(20), |s| s.state == ConnState::Connected).map_err(err)?;
                Ok(object! { "connected" => s.state == ConnState::Connected, "network" => net })
            }
            "disconnect" => wifi::disconnect().map(|_| Value::Null).map_err(err),
            "on" => wifi::set_radio(true).map(|_| Value::Null).map_err(err),
            "off" => wifi::set_radio(false).map(|_| Value::Null).map_err(err),
            other => Err(format!("unknown Wi-Fi operation {other}")),
        }
    }

    fn wallpaper(&self, args: &Value) -> Result<Value, String> {
        let s = connect(shell::NAME, shell::Client::new).ok_or("the desktop is not available")?;
        let list = s.wallpapers().map_err(|_| "the desktop did not answer")?;
        let current = s.wallpaper().unwrap_or_default();
        let name_of = |p: &str| vfiles::path::file_stem(p).to_string();
        match args.str("operation").unwrap_or("list") {
            "list" => Ok(object! {
                "pictures" => list.iter().map(|p| Value::from(name_of(p))).collect::<Vec<_>>(),
                "current" => name_of(&current),
            }),
            op @ ("set" | "next") => {
                let path = if op == "next" {
                    let i = list.iter().position(|p| *p == current).map(|i| (i + 1) % list.len().max(1)).unwrap_or(0);
                    list.get(i).cloned().ok_or("there are no wallpapers")?
                } else {
                    let want = args.str("picture").ok_or("which picture?")?;
                    let w = want.to_lowercase();
                    match list.iter().find(|p| name_of(p).to_lowercase().contains(&w)) {
                        Some(p) => p.clone(),
                        None => resolve(want)?,
                    }
                };
                match s.set_wallpaper(path.clone()) {
                    Ok(Ok(())) => Ok(object! { "wallpaper" => name_of(&path) }),
                    _ => Err(format!("the desktop could not use {}", show(&path))),
                }
            }
            other => Err(format!("unknown wallpaper operation {other}")),
        }
    }

    fn notify(&self, args: &Value) -> Result<Value, String> {
        let s = connect(shell::NAME, shell::Client::new).ok_or("the desktop is not available")?;
        let title = args.str("title").unwrap_or("Reminder");
        let text = args.str("text").unwrap_or("");
        s.notify(title.into(), text.into(), "agent".into()).map_err(|_| "the desktop did not answer")?;
        Ok(Value::Null)
    }

    fn timer(&self, args: &Value) -> Result<Value, String> {
        let now = vrt::time::unix_time_ns() / 1_000_000_000;
        let mut sh = self.shared.lock();
        match args.str("operation").unwrap_or("list") {
            "set" => {
                let label = args.str("label").unwrap_or("Timer");
                let due = if let Some(s) = args["seconds"].as_f64() {
                    now + (s.max(1.0) as u64)
                } else if let Some(at) = args.str("at") {
                    let (h, m) = at.split_once(':').ok_or("the time must look like 17:30")?;
                    let (h, m): (u64, u64) = (h.trim().parse().map_err(|_| "bad hour")?, m.trim().parse().map_err(|_| "bad minute")?);
                    if h > 23 || m > 59 {
                        return Err("the time must be between 00:00 and 23:59".into());
                    }
                    let today = now - now % 86_400 + h * 3600 + m * 60;
                    if today > now { today } else { today + 86_400 }
                } else {
                    return Err("when? give seconds or a time".into());
                };
                let id = sh.add_timer(label, due);
                Ok(object! { "id" => id, "label" => label, "due_in_seconds" => due - now })
            }
            "list" => Ok(Value::Array(
                sh.timers.iter().map(|t| object! { "id" => t.id, "label" => t.label.as_str(), "due_in_seconds" => t.due.saturating_sub(now) }).collect(),
            )),
            "cancel" => {
                let id = args["id"].as_u64().or_else(|| {
                    let l = args.str("label")?.to_lowercase();
                    sh.timers.iter().find(|t| t.label.to_lowercase().contains(&l)).map(|t| t.id)
                });
                match id.and_then(|id| sh.remove_timer(id)) {
                    Some(t) => Ok(object! { "cancelled" => t.label.as_str() }),
                    None => Err("no such timer".into()),
                }
            }
            other => Err(format!("unknown timer operation {other}")),
        }
    }

    fn memory(&self, args: &Value) -> Result<Value, String> {
        let now = vrt::time::unix_time_ns() / 1_000_000_000;
        let mut sh = self.shared.lock();
        let r = match args.str("operation").unwrap_or("search") {
            "remember" => {
                let text = args.str("text").filter(|t| !t.trim().is_empty()).ok_or("what should I remember?")?;
                let id = sh.memory.remember(text, args.str("kind").unwrap_or("fact"), now);
                Ok(object! { "remembered" => id })
            }
            "search" => Ok(Value::Array(
                sh.memory
                    .search(args.str("text").unwrap_or(""), 15)
                    .iter()
                    .map(|f| object! { "id" => f.id, "text" => f.text.as_str() })
                    .collect(),
            )),
            "forget" => match args["id"].as_u64() {
                Some(id) if sh.memory.forget(id) => Ok(object! { "forgot" => id }),
                Some(_) => Err("nothing with that id".into()),
                None => {
                    let gone = sh.memory.forget_matching(args.str("text").unwrap_or(""));
                    if gone.is_empty() {
                        Err("nothing matched".into())
                    } else {
                        Ok(
                            object! { "forgot" => gone.iter().map(|f| Value::from(f.text.as_str())).collect::<Vec<_>>() },
                        )
                    }
                }
            },
            "forget_all" => {
                sh.memory.forget_all();
                Ok(object! { "forgot" => "everything" })
            }
            other => Err(format!("unknown memory operation {other}")),
        };
        sh.save_memory();
        r
    }

    fn system(&self, args: &Value) -> Result<Value, String> {
        let launcher = connect(launcher::NAME, launcher::Client::new).ok_or("the launcher is not available")?;
        match args.str("operation").unwrap_or("info") {
            "info" => {
                let info = vrt::object::system_info().map_err(|_| "no system information")?;
                Ok(object! {
                    "os" => "Vindows 0.1",
                    "cpus" => info.cpu_count,
                    "memory_total" => vfiles::format::human_size(info.total_memory),
                    "memory_free" => vfiles::format::human_size(info.free_memory),
                    "uptime_minutes" => vrt::time::now_ns() / 60_000_000_000,
                })
            }
            "restart" => {
                let _ = launcher.power(vproto::init::power::REBOOT);
                Ok(Value::Null)
            }
            "shutdown" => {
                let _ = launcher.power(vproto::init::power::SHUTDOWN);
                Ok(Value::Null)
            }
            other => Err(format!("unknown system operation {other}")),
        }
    }

    fn tasks(&self, args: &Value) -> Result<Value, String> {
        let launcher = connect(launcher::NAME, launcher::Client::new).ok_or("the launcher is not available")?;
        let tasks = launcher.tasks().map_err(|_| "the launcher did not answer")?;
        match args.str("operation").unwrap_or("list") {
            "list" => Ok(Value::Array(
                tasks
                    .iter()
                    .map(|t| object! { "name" => t.name.as_str(), "memory" => vfiles::format::human_size(t.memory), "app" => t.is_app })
                    .collect(),
            )),
            "end" => {
                let name = args.str("name").ok_or("which program?")?.to_lowercase();
                // Only applications: system services are not the agent's to stop.
                let t = tasks.iter().find(|t| t.is_app && t.name.to_lowercase() == name).ok_or_else(|| format!("no running application called {name}"))?;
                match launcher.kill(t.koid) {
                    Ok(Ok(())) => Ok(object! { "ended" => t.name.as_str() }),
                    _ => Err(format!("{} could not be ended", t.name)),
                }
            }
            other => Err(format!("unknown task operation {other}")),
        }
    }
}

/// An application's actions for the language model.
fn describe_info(info: &AppAgentInfo) -> Value {
    let actions: Vec<Value> = info
        .actions
        .iter()
        .map(|a| {
            let params: Vec<(String, String, String, bool, Vec<String>)> = a
                .params
                .iter()
                .map(|p| (p.name.clone(), p.kind.clone(), p.description.clone(), p.required, p.choices.clone()))
                .collect();
            object! {
                "name" => a.name.as_str(),
                "description" => a.description.as_str(),
                "parameters" => tools::action_schema(&params),
                "needs_approval" => a.risk != Risk::Routine,
            }
        })
        .collect();
    object! { "about" => info.summary.as_str(), "actions" => actions }
}

/// Arguments as they read in an approval request.
fn describe_args(args: &Value) -> String {
    let Some(m) = args.as_object() else { return String::new() };
    let parts: Vec<String> = m
        .iter()
        .map(|(k, v)| {
            let val: String = match v {
                Value::String(s) => s.chars().take(80).collect(),
                other => other.to_string().chars().take(80).collect(),
            };
            format!("{k}: {val}")
        })
        .collect();
    parts.join(", ")
}

/// The applications and their actions, for the prompt.
pub fn catalog(installed: &[AppInfo], infos: &BTreeMap<String, AppAgentInfo>) -> String {
    let mut s = String::new();
    for a in installed {
        s.push_str(&format!("- {} ({}): ", a.id, a.name));
        match infos.get(&a.id) {
            Some(info) if !info.actions.is_empty() => {
                s.push_str(if info.summary.is_empty() { &a.description } else { &info.summary });
                let sigs: Vec<String> = info
                    .actions
                    .iter()
                    .map(|x| {
                        tools::signature(
                            &x.name,
                            &x.params.iter().map(|p| (p.name.clone(), p.required)).collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                s.push_str(&format!(" Actions: {}.", sigs.join(", ")));
            }
            _ => s.push_str(&a.description),
        }
        s.push('\n');
    }
    s
}

/// The installed applications (for the prompt).
pub fn installed_apps() -> Vec<AppInfo> {
    connect(launcher::NAME, launcher::Client::new).and_then(|l| l.apps().ok()).unwrap_or_default()
}
