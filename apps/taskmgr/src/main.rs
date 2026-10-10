//! Task Manager: running processes and system load.
//!
//! * **Processes**: a sortable table (name, PID, threads, memory, CPU %)
//!   grouped into apps and system processes, with search and "End task"
//!   (through the launcher, after a confirmation).
//! * **Performance**: live graphs of CPU usage (from the kernel's idle time
//!   across all CPUs) and memory use, with summary tiles.
//!
//! The kernel is sampled once a second; CPU percentages are computed from
//! the differences between consecutive samples. [`agent`] lets the voice
//! agent read the processes and the load, and end applications.

#![no_std]
#![no_main]

extern crate alloc;

mod agent;
mod perf;
mod procs;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vproto::display::modifiers;
use vproto::init::{AppInfo, LaunchError, launcher};
use vproto::input::keys;
use vui::{App, Icon, Rect, Ui, WindowSpec};

vrt::entry!(main);

/// Seconds of history kept for the graphs.
pub const HISTORY: usize = 60;
const SAMPLE_NS: u64 = 1_000_000_000;

/// Core services, used to tell applications from system processes when the
/// launcher (which knows) cannot be asked.
const CORE_SERVICES: &[&str] = &["init", "vfs", "devmgr", "compositor", "audio", "shell"];

/// True for device drivers (shown with a chip icon), the driver VM's
/// monitor among them.
fn is_driver(name: &str) -> bool {
    name.starts_with("virtio-") || name == "drivervm"
}

/// One row of the process table.
#[derive(Debug, Clone)]
pub struct Proc {
    pub koid: u64,
    pub name: String,
    /// Display name (the application's name for apps).
    pub title: String,
    pub icon: Icon,
    pub threads: u32,
    pub memory: u64,
    pub cpu_ns: u64,
    /// CPU usage over the last interval, 0..=100 (of all CPUs).
    pub cpu: f32,
    pub is_app: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Pid,
    Threads,
    Memory,
    Cpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Cpu,
    Memory,
}

/// A dialog on top of the window.
pub enum Dialog {
    ConfirmKill { koid: u64, title: String, system: bool },
    Error(String),
}

pub struct TaskManager {
    pub tab: usize,
    pub procs: Vec<Proc>,
    pub info: vabi::SystemInfo,
    pub cpu_model: String,
    pub version: String,
    /// CPU usage (0..=1) and memory use (0..=1) histories.
    pub cpu_hist: VecDeque<f32>,
    pub mem_hist: VecDeque<f32>,
    pub cpu_now: f32,
    last: Option<(u64, u64, BTreeMap<u64, u64>)>,
    next_sample: u64,
    pub sort: Column,
    pub descending: bool,
    pub selected: Option<u64>,
    pub search: String,
    pub dialog: Option<Dialog>,
    pub resource: Resource,
    launcher: Option<launcher::Client>,
    apps: Vec<AppInfo>,
    /// A text field had the keyboard focus last frame.
    pub typing: bool,
    /// Rows of the table in display order (koid or None for headers).
    pub rows: Vec<procs::Row>,
}

fn cstr(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().to_string()
}

/// `1:02:03` or `12:34`.
pub fn format_uptime(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    let (d, h, m, s) = (s / 86_400, (s / 3600) % 24, (s / 60) % 60, s % 60);
    if d > 0 {
        alloc::format!("{d}d {h}:{m:02}:{s:02}")
    } else if h > 0 {
        alloc::format!("{h}:{m:02}:{s:02}")
    } else {
        alloc::format!("{m}:{s:02}")
    }
}

/// Human-readable memory size.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return alloc::format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return alloc::format!("{kib:.1} KiB");
    }
    let mib = kib / 1024.0;
    if mib < 1024.0 {
        return alloc::format!("{mib:.1} MiB");
    }
    alloc::format!("{:.2} GiB", mib / 1024.0)
}

impl TaskManager {
    fn new() -> TaskManager {
        let mut tm = TaskManager {
            tab: 0,
            procs: Vec::new(),
            info: vabi::SystemInfo::default(),
            cpu_model: String::new(),
            version: String::new(),
            cpu_hist: VecDeque::new(),
            mem_hist: VecDeque::new(),
            cpu_now: 0.0,
            last: None,
            next_sample: 0,
            sort: Column::Cpu,
            descending: true,
            selected: None,
            search: String::new(),
            dialog: None,
            resource: Resource::Cpu,
            launcher: None,
            apps: Vec::new(),
            typing: false,
            rows: Vec::new(),
        };
        if let Some(Ok(apps)) = tm.launcher().map(|l| l.apps()) {
            tm.apps = apps;
        }
        tm.sample();
        tm
    }

    fn launcher(&mut self) -> Option<&launcher::Client> {
        if self.launcher.is_none() {
            self.launcher = vproto::connect(launcher::NAME).ok().map(launcher::Client::new);
        }
        self.launcher.as_ref()
    }

    /// Takes a sample of the system and the processes.
    pub fn sample(&mut self) {
        let Ok(info) = vrt::object::system_info() else { return };
        self.cpu_model = cstr(&info.cpu_brand);
        self.version = cstr(&info.version);
        let mut list = alloc::vec![vabi::ProcessInfo::default(); 512];
        let n = vrt::object::process_list(&mut list).unwrap_or(0).min(list.len());
        list.truncate(n);
        list.retain(|p| p.state == vabi::process_state::RUNNING);
        // Which processes are applications (started on request) according
        // to the launcher; everything else (services, drivers) is a system
        // process.
        let app_koids: Option<Vec<u64>> = match self.launcher().map(|l| l.tasks()) {
            Some(Ok(tasks)) => Some(tasks.iter().filter(|t| t.is_app).map(|t| t.koid).collect()),
            _ => None,
        };
        let cpus = info.cpu_count.max(1) as f64;
        let (cpu_now, prev_cpu) = match &self.last {
            Some((t0, idle0, per)) if info.uptime_ns > *t0 => {
                let dt = (info.uptime_ns - t0) as f64 * cpus;
                let idle = info.idle_time_ns.saturating_sub(*idle0) as f64;
                (Some((1.0 - idle / dt).clamp(0.0, 1.0) as f32), Some((dt, per.clone())))
            }
            _ => (None, None),
        };
        let mut procs = Vec::with_capacity(list.len());
        let mut per = BTreeMap::new();
        for p in &list {
            per.insert(p.koid, p.cpu_time_ns);
            let name = p.name().to_string();
            let app = self.apps.iter().find(|a| a.id == name);
            let is_app = match &app_koids {
                Some(koids) => koids.contains(&p.koid),
                None => app.is_some() && !CORE_SERVICES.contains(&name.as_str()),
            };
            let cpu = match &prev_cpu {
                Some((dt, prev)) => {
                    let before = prev.get(&p.koid).copied().unwrap_or(p.cpu_time_ns);
                    ((p.cpu_time_ns.saturating_sub(before) as f64 / dt) * 100.0).clamp(0.0, 100.0) as f32
                }
                None => 0.0,
            };
            let icon = match app {
                Some(a) if is_app => Icon::by_name(&a.icon).unwrap_or(Icon::Grid),
                _ if is_driver(&name) => Icon::Cpu,
                _ if !is_app => Icon::Settings,
                _ => Icon::Cube,
            };
            procs.push(Proc {
                koid: p.koid,
                title: app.map(|a| a.name.clone()).unwrap_or_else(|| name.clone()),
                name,
                icon,
                threads: p.threads,
                memory: p.memory_bytes,
                cpu_ns: p.cpu_time_ns,
                cpu,
                is_app,
            });
        }
        if let Some(c) = cpu_now {
            self.cpu_now = c;
            push(&mut self.cpu_hist, c);
        }
        let mem = if info.total_memory > 0 {
            info.total_memory.saturating_sub(info.free_memory) as f32 / info.total_memory as f32
        } else {
            0.0
        };
        push(&mut self.mem_hist, mem);
        self.last = Some((info.uptime_ns, info.idle_time_ns, per));
        self.info = info;
        self.procs = procs;
        if self.selected.is_some_and(|k| !self.procs.iter().any(|p| p.koid == k)) {
            self.selected = None;
        }
        procs::rebuild_rows(self);
    }

    pub fn selected_proc(&self) -> Option<&Proc> {
        self.selected.and_then(|k| self.procs.iter().find(|p| p.koid == k))
    }

    /// Asks before ending the selected process.
    pub fn ask_end(&mut self) {
        let Some(p) = self.selected_proc() else { return };
        self.dialog = Some(Dialog::ConfirmKill { koid: p.koid, title: p.title.clone(), system: !p.is_app });
    }

    fn end(&mut self, koid: u64) {
        let r = match self.launcher() {
            Some(l) => l.kill(koid),
            None => {
                self.dialog = Some(Dialog::Error("The application launcher is not available.".into()));
                return;
            }
        };
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let why = match e {
                    LaunchError::NotFound => "the process has already exited",
                    LaunchError::Denied => "permission was denied",
                    _ => "the system refused",
                };
                self.dialog = Some(Dialog::Error(alloc::format!("The process could not be ended: {why}.")));
            }
            Err(_) => self.dialog = Some(Dialog::Error("The application launcher is not responding.".into())),
        }
        // Give the kernel a moment to tear the process down, then refresh.
        vrt::time::sleep(vrt::time::Duration::from_millis(30));
        self.sample();
        self.next_sample = vrt::time::now_ns() + SAMPLE_NS;
    }

    fn dialogs(&mut self, ui: &mut Ui) {
        let Some(d) = self.dialog.take() else { return };
        let keep = match &d {
            Dialog::ConfirmKill { koid, title, system } => {
                let (heading, message) = if *system {
                    (
                        alloc::format!("End system process “{title}”?"),
                        "This process is part of Veda. Ending it can make the system unstable or stop it from working."
                            .to_string(),
                    )
                } else {
                    (alloc::format!("End “{title}”?"), "Unsaved work in this app will be lost.".to_string())
                };
                match ui.message_box(&heading, &message, &["End task", "Cancel"]) {
                    Some(0) => {
                        let k = *koid;
                        self.end(k);
                        false
                    }
                    Some(_) => false,
                    None => true,
                }
            }
            Dialog::Error(msg) => ui.message_box("Task Manager", msg, &["OK"]).is_none(),
        };
        if keep && self.dialog.is_none() {
            self.dialog = Some(d);
        }
    }

    fn handle_keys(&mut self, ui: &mut Ui) {
        for k in ui.input.keys.clone() {
            let ctrl = k.modifiers & modifiers::CTRL != 0;
            match k.code {
                keys::TAB if ctrl => self.tab = (self.tab + 1) % 2,
                keys::KEY_1 if ctrl => self.tab = 0,
                keys::KEY_2 if ctrl => self.tab = 1,
                keys::F5 => {
                    self.sample();
                    self.next_sample = ui.now() + SAMPLE_NS;
                }
                keys::F if ctrl => {
                    self.tab = 0;
                    procs::focus_search(ui, self.search.len());
                }
                _ if self.tab == 0 => procs::handle_key(self, ui, k.code),
                _ if self.tab == 1 => match k.code {
                    keys::UP | keys::LEFT => self.resource = Resource::Cpu,
                    keys::DOWN | keys::RIGHT => self.resource = Resource::Memory,
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

fn push(h: &mut VecDeque<f32>, v: f32) {
    h.push_back(v);
    while h.len() > HISTORY {
        h.pop_front();
    }
}

impl App for TaskManager {
    fn agent_info(&self) -> Option<vui::agent::AppAgentInfo> {
        Some(agent::info())
    }

    fn agent_state(&self) -> vui::agent::Value {
        agent::state(self)
    }

    fn agent_invoke(&mut self, action: &str, args: &vui::agent::Value) -> Result<vui::agent::Value, String> {
        TaskManager::agent_invoke(self, action, args)
    }

    fn update(&mut self, ui: &mut Ui) {
        let now = ui.now();
        if self.next_sample == 0 {
            // The first CPU figure needs two samples; take the second soon.
            self.next_sample = now + 400_000_000;
        } else if now >= self.next_sample {
            self.sample();
            self.next_sample = now + SAMPLE_NS;
        }
        ui.repaint_at(self.next_sample);
        let dialog_at_start = self.dialog.is_some();
        let before =
            (self.tab, self.selected, self.sort, self.descending, self.resource, self.search.len(), self.rows.len());
        let t = ui.theme().clone();
        let (top, rest) = ui.rect().split_top(58);
        ui.canvas.fill_rect(Rect::new(top.x, top.bottom() - 1, top.w, 1), t.border);
        let mut tab = self.tab;
        ui.tabs(Rect::new(top.x + 14, top.y + 10, 320, 46), &["Processes", "Performance"], &mut tab);
        self.tab = tab;
        if self.tab == 0 {
            procs::draw(self, ui, top, rest);
        } else {
            self.typing = false;
            perf::draw(self, ui, rest);
        }
        // Clicks handled while drawing can leave parts drawn earlier stale.
        if before
            != (self.tab, self.selected, self.sort, self.descending, self.resource, self.search.len(), self.rows.len())
        {
            ui.repaint();
        }
        // Keys after the mouse (handled while drawing), so a click and a key
        // in the same frame act in order. (While a menu is open, vui gives it
        // the keyboard and widgets see no keys.)
        if !dialog_at_start && !self.typing && !ui.input.keys.is_empty() {
            self.handle_keys(ui);
            ui.repaint();
        }
        // A dialog opened this frame is shown from the next one.
        if dialog_at_start {
            self.dialogs(ui);
        } else if self.dialog.is_some() {
            ui.repaint();
        }
    }
}

fn main() -> i32 {
    let tm = TaskManager::new();
    let mut spec = WindowSpec::new("Task Manager", 900, 600);
    spec.app_id = "taskmgr".into();
    spec.min_width = 640;
    spec.min_height = 420;
    vui::run(spec, tm)
}
