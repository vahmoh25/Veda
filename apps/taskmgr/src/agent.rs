//! Task Manager for the voice agent: what it shows (the page, the busiest
//! processes, the system's load) and what it does (listing and sorting the
//! processes, showing the performance graphs, ending applications), through
//! the same table, pages and launcher calls as the window.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use vproto::init::TaskInfo;
use vui::agent::{self, Action, AppAgentInfo, Risk, Value, arg_opt_int, arg_opt_str, object};

use crate::procs::{self, Row};
use crate::{Column, Dialog, Proc, Resource, SAMPLE_NS, TaskManager, format_uptime, human_size};

/// The most processes the agent gets in one list.
const MAX_PROCS: usize = 60;
/// How many processes a list has unless the agent asks for more.
const DEFAULT_PROCS: i64 = 20;
/// How many of the busiest processes the state lists.
const STATE_PROCS: usize = 12;
/// How long an ended process may take to disappear.
const END_WAIT_NS: u64 = 1_000_000_000;

pub fn info() -> AppAgentInfo {
    agent::info(
        "Task Manager shows the running programs with their CPU and memory use and the system's load, and ends \
         applications that misbehave.",
        vec![
            Action::new("list_processes", "Returns the running processes with their CPU and memory use")
                .choice(
                    "sort_by",
                    "The order: cpu (busiest first, the default), memory, name or pid",
                    false,
                    &["cpu", "memory", "name", "pid"],
                )
                .choice("kind", "Which processes: all (the default), apps or system", false, &["all", "apps", "system"])
                .param("name", "string", "Only processes whose name contains this", false)
                .param("limit", "integer", "How many at most (20 by default, up to 60)", false)
                .build(),
            Action::new("show_processes", "Shows the Processes page in the window, sorted and filtered as asked")
                .choice(
                    "sort_by",
                    "The column to sort by (kept as it is if not given)",
                    false,
                    &["cpu", "memory", "name", "pid", "threads"],
                )
                .param(
                    "search",
                    "string",
                    "Show only processes whose name contains this (an empty text shows all)",
                    false,
                )
                .build(),
            Action::new("show_performance", "Shows the Performance page with the CPU or memory graph")
                .choice("graph", "Which graph: cpu (the default) or memory", false, &["cpu", "memory"])
                .build(),
            Action::new(
                "read_performance",
                "Returns the CPU and memory use now and over the last minute, with the busiest processes",
            )
            .build(),
            Action::new("end_process", "Ends a running application at once")
                .param(
                    "name",
                    "string",
                    "The application's process name or title exactly as listed, such as about or Text Editor \
                     (system processes cannot be ended)",
                    false,
                )
                .param("pid", "integer", "Its process id (PID), needed when several have the same name", false)
                .risk(Risk::Destructive)
                .build(),
        ],
    )
}

fn column_name(c: Column) -> &'static str {
    match c {
        Column::Name => "name",
        Column::Pid => "pid",
        Column::Threads => "threads",
        Column::Memory => "memory",
        Column::Cpu => "cpu",
    }
}

fn column(name: &str) -> Result<Column, String> {
    match name {
        "name" => Ok(Column::Name),
        "pid" => Ok(Column::Pid),
        "threads" => Ok(Column::Threads),
        "memory" => Ok(Column::Memory),
        "cpu" => Ok(Column::Cpu),
        other => Err(format!("cannot sort by {other}: by cpu, memory, name, pid or threads")),
    }
}

/// A percentage with one decimal.
fn percent(p: f32) -> f64 {
    ((p * 10.0 + 0.5) as i64) as f64 / 10.0
}

/// One process as the agent sees it.
fn proc_value(p: &Proc) -> Value {
    let mut v = object! { "name" => p.name.as_str() };
    if p.title != p.name {
        v.set("title", p.title.as_str());
    }
    v.set("pid", p.koid);
    v.set("cpu_percent", percent(p.cpu));
    v.set("memory", human_size(p.memory));
    v.set("threads", p.threads);
    v.set("kind", if p.is_app { "app" } else { "system" });
    v
}

/// `procs` in the order of `sort`: the largest numbers first, names and
/// process ids from the smallest.
fn sorted<'a>(procs: impl Iterator<Item = &'a Proc>, sort: Column) -> Vec<&'a Proc> {
    let mut v: Vec<&Proc> = procs.collect();
    v.sort_by(|a, b| {
        match sort {
            Column::Cpu => b.cpu.partial_cmp(&a.cpu).unwrap_or(Ordering::Equal).then(b.cpu_ns.cmp(&a.cpu_ns)),
            Column::Memory => b.memory.cmp(&a.memory),
            Column::Threads => b.threads.cmp(&a.threads),
            Column::Name => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
            Column::Pid => Ordering::Equal,
        }
        .then(a.koid.cmp(&b.koid))
    });
    v
}

/// Memory in use, the total and the share in use (percent).
fn memory(tm: &TaskManager) -> (u64, u64, f64) {
    let total = tm.info.total_memory;
    let used = total.saturating_sub(tm.info.free_memory);
    let share = if total > 0 { percent(used as f32 * 100.0 / total as f32) } else { 0.0 };
    (used, total, share)
}

pub fn state(tm: &TaskManager) -> Value {
    let mut v = object! { "page" => if tm.tab == 0 { "processes" } else { "performance" } };
    if tm.tab == 0 {
        v.set("sorted_by", column_name(tm.sort));
        v.set("descending", tm.descending);
        if !tm.search.is_empty() {
            v.set("search", tm.search.as_str());
        }
        v.set("selected", tm.selected_proc().map(|p| object! { "name" => p.name.as_str(), "pid" => p.koid }));
    } else {
        v.set("graph", if tm.resource == Resource::Cpu { "cpu" } else { "memory" });
    }
    if let Some(d) = &tm.dialog {
        let shown = match d {
            Dialog::ConfirmKill { title, .. } => format!("asking the user whether to end {title}"),
            Dialog::Error(msg) => msg.clone(),
        };
        v.set("dialog", shown);
    }
    let (used, total, share) = memory(tm);
    v.set("cpu_percent", percent(tm.cpu_now * 100.0));
    v.set("memory_used", human_size(used));
    v.set("memory_total", human_size(total));
    v.set("memory_percent", share);
    v.set("processes", tm.procs.len());
    v.set("applications", tm.procs.iter().filter(|p| p.is_app).count());
    let busiest: Vec<Value> =
        sorted(tm.procs.iter(), Column::Cpu).into_iter().take(STATE_PROCS).map(proc_value).collect();
    v.set("busiest", busiest);
    v
}

impl TaskManager {
    /// A new sample when the last one is a second old, as the window takes
    /// them (CPU use is measured between two samples).
    fn refresh(&mut self) {
        let now = vrt::time::now_ns();
        if now >= self.next_sample {
            self.sample();
            self.next_sample = now + SAMPLE_NS;
        }
    }

    /// The system's load now and over the last minute.
    fn performance_value(&self) -> Value {
        let info = &self.info;
        let n = self.cpu_hist.len().max(1) as f32;
        let average = self.cpu_hist.iter().sum::<f32>() / n;
        let peak = self.cpu_hist.iter().copied().fold(0.0f32, f32::max);
        let (used, total, share) = memory(self);
        let busiest: Vec<Value> = sorted(self.procs.iter(), Column::Cpu)
            .into_iter()
            .take(3)
            .map(|p| object! { "name" => p.title.as_str(), "pid" => p.koid, "cpu_percent" => percent(p.cpu) })
            .collect();
        let largest: Vec<Value> = sorted(self.procs.iter(), Column::Memory)
            .into_iter()
            .take(3)
            .map(|p| object! { "name" => p.title.as_str(), "pid" => p.koid, "memory" => human_size(p.memory) })
            .collect();
        object! {
            "cpu_percent" => percent(self.cpu_now * 100.0),
            "cpu_average_percent" => percent(average * 100.0),
            "cpu_peak_percent" => percent(peak * 100.0),
            "measured_over_seconds" => self.cpu_hist.len(),
            "cpu" => self.cpu_model.as_str(),
            "cores" => info.cpu_count,
            "memory_used" => human_size(used),
            "memory_total" => human_size(total),
            "memory_percent" => share,
            "processes" => info.process_count,
            "threads" => info.thread_count,
            "up_time" => format_uptime(info.uptime_ns),
            "busiest" => busiest,
            "largest" => largest,
        }
    }

    /// The running programs according to the launcher (which knows which
    /// are applications).
    fn tasks(&mut self) -> Result<Vec<TaskInfo>, String> {
        match self.launcher().map(|l| l.tasks()) {
            Some(Ok(tasks)) => Ok(tasks),
            Some(Err(_)) => Err("the application launcher did not answer".into()),
            None => Err("the application launcher is not available".into()),
        }
    }

    /// The name an application is shown with (its process name otherwise).
    fn title_of(&self, t: &TaskInfo) -> String {
        self.apps.iter().find(|a| a.id == t.name).map_or_else(|| t.name.clone(), |a| a.name.clone())
    }

    /// Ends the application named in `args` (by name, PID or both). It is
    /// looked up again now, as the user may have allowed this a while ago,
    /// and only an exact match is ended.
    fn agent_end(&mut self, args: &Value) -> Result<Value, String> {
        let name = arg_opt_str(args, "name").map(|n| n.trim().trim_end_matches(".exe").to_lowercase());
        let pid = arg_opt_int(args, "pid")?;
        if name.is_none() && pid.is_none() {
            return Err("which application? Give its name or PID".into());
        }
        let tasks = self.tasks()?;
        let named = |t: &TaskInfo| {
            name.as_ref().is_none_or(|n| t.name.to_lowercase() == *n || self.title_of(t).to_lowercase() == *n)
        };
        let found: Vec<&TaskInfo> =
            tasks.iter().filter(|t| pid.is_none_or(|p| t.koid as i64 == p) && named(t)).collect();
        let t = match found.as_slice() {
            [t] => *t,
            [] => {
                if let (Some(p), Some(n)) = (pid, &name)
                    && let Some(other) = tasks.iter().find(|t| t.koid as i64 == p)
                {
                    return Err(format!("PID {p} is {}, not {n}", self.title_of(other)));
                }
                if let Some(p) = pid {
                    return Err(format!("no running program has PID {p} (it may have exited)"));
                }
                let apps: Vec<String> = tasks.iter().filter(|t| t.is_app).map(|t| self.title_of(t)).collect();
                return Err(format!(
                    "no running program is called {} (it may have exited); the applications running are: {}",
                    name.as_deref().unwrap_or(""),
                    if apps.is_empty() { String::from("none") } else { apps.join(", ") }
                ));
            }
            many => {
                let pids: Vec<String> = many.iter().map(|t| t.koid.to_string()).collect();
                return Err(format!(
                    "{} programs are called {}, with PIDs {}: say which PID to end",
                    many.len(),
                    self.title_of(many[0]),
                    pids.join(", ")
                ));
            }
        };
        let (koid, title) = (t.koid, self.title_of(t));
        if !t.is_app {
            return Err(format!(
                "{title} is a system process: ending it can stop Veda from working, so Task Manager ends only \
                 applications for the agent"
            ));
        }
        if t.name == "taskmgr" {
            return Err("that is Task Manager itself: close its window instead".into());
        }
        // The window's question about this process is answered.
        if matches!(self.dialog, Some(Dialog::ConfirmKill { koid: k, .. }) if k == koid) {
            self.dialog = None;
        }
        self.end(koid);
        if let Some(Dialog::Error(msg)) = &self.dialog {
            let msg = msg.clone();
            self.dialog = None;
            return Err(msg);
        }
        // Wait (briefly) until it is gone, so that the table shows it.
        let end = vrt::time::now_ns() + END_WAIT_NS;
        let mut gone = false;
        while !gone && vrt::time::now_ns() < end {
            gone = !self.tasks()?.iter().any(|t| t.koid == koid);
            if !gone {
                vrt::time::sleep(vrt::time::Duration::from_millis(50));
            }
        }
        if self.procs.iter().any(|p| p.koid == koid) {
            self.sample();
            self.next_sample = vrt::time::now_ns() + SAMPLE_NS;
        }
        let mut v = object! { "ended" => title, "pid" => koid };
        if !gone {
            v.set("note", "it is still shutting down");
        }
        Ok(v)
    }

    pub(crate) fn agent_invoke(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        // An error message left open would hide what the agent shows.
        if matches!(self.dialog, Some(Dialog::Error(_))) {
            self.dialog = None;
        }
        match action {
            "list_processes" => {
                self.refresh();
                let sort = column(arg_opt_str(args, "sort_by").unwrap_or("cpu"))?;
                let kind = arg_opt_str(args, "kind").unwrap_or("all");
                if !matches!(kind, "all" | "apps" | "system") {
                    return Err(format!("'kind' cannot be {kind}: it is all, apps or system"));
                }
                let name = arg_opt_str(args, "name").map(|n| n.trim().to_lowercase());
                let limit = arg_opt_int(args, "limit")?.unwrap_or(DEFAULT_PROCS).clamp(1, MAX_PROCS as i64) as usize;
                let wanted = |p: &&Proc| {
                    let kind_ok = match kind {
                        "apps" => p.is_app,
                        "system" => !p.is_app,
                        _ => true,
                    };
                    kind_ok
                        && name
                            .as_ref()
                            .is_none_or(|n| p.name.to_lowercase().contains(n) || p.title.to_lowercase().contains(n))
                };
                let list = sorted(self.procs.iter().filter(wanted), sort);
                let shown: Vec<Value> = list.iter().take(limit).map(|p| proc_value(p)).collect();
                Ok(object! { "processes" => shown, "total" => list.len(), "sorted_by" => column_name(sort) })
            }
            "show_processes" => {
                self.tab = 0;
                if let Some(s) = arg_opt_str(args, "sort_by") {
                    let col = column(s)?;
                    // As a click on the column's header: numbers from the
                    // largest, names from A.
                    if self.sort != col {
                        self.sort = col;
                        self.descending = col != Column::Name;
                    }
                }
                if let Some(search) = args.get("search").and_then(Value::as_str) {
                    self.search = search.trim().to_string();
                }
                procs::rebuild_rows(self);
                let shown = self.rows.iter().filter(|r| matches!(r, Row::Proc(_))).count();
                Ok(object! {
                    "page" => "processes",
                    "sorted_by" => column_name(self.sort),
                    "descending" => self.descending,
                    "search" => self.search.as_str(),
                    "shown" => shown,
                })
            }
            "show_performance" => {
                self.resource = match arg_opt_str(args, "graph").unwrap_or("cpu") {
                    "cpu" => Resource::Cpu,
                    "memory" => Resource::Memory,
                    other => return Err(format!("there is no {other} graph: cpu or memory")),
                };
                self.tab = 1;
                self.refresh();
                let mut v = self.performance_value();
                v.set("page", "performance");
                v.set("graph", if self.resource == Resource::Cpu { "cpu" } else { "memory" });
                Ok(v)
            }
            "read_performance" => {
                self.refresh();
                Ok(self.performance_value())
            }
            "end_process" => self.agent_end(args),
            other => Err(format!("Task Manager has no action called {other}")),
        }
    }
}
