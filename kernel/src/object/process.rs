//! Processes: an address space, a handle table and a set of threads.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use vabi::{ProcessInfo, process_state};

use super::handle::HandleTable;
use super::{Name, Signals};
use crate::mm::aspace::AddressSpace;
use crate::sched::{self, Thread, WakeReason};
use crate::sync::SpinLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Created, no thread started yet.
    Created,
    Running,
    /// All threads have exited.
    Terminated { code: i64 },
}

pub struct Process {
    pub koid: u64,
    pub parent_koid: u64,
    pub name: Name,
    aspace: SpinLock<Option<Arc<AddressSpace>>>,
    pub handles: SpinLock<HandleTable>,
    threads: SpinLock<Vec<Arc<Thread>>>,
    state: SpinLock<ProcessState>,
    /// Exit code requested by `process_exit`/kill, applied at termination.
    pending_code: SpinLock<Option<i64>>,
    pub signals: Signals,
    exited_threads_cpu_ns: AtomicU64,
}

/// Every process ever created that is still referenced somewhere.
static PROCESSES: SpinLock<Vec<Weak<Process>>> = SpinLock::new(Vec::new());

impl Process {
    pub fn new(name: &str, parent_koid: u64) -> Option<Arc<Process>> {
        let p = Arc::new(Process {
            koid: super::new_koid(),
            parent_koid,
            name: Name::new(name),
            aspace: SpinLock::new(Some(AddressSpace::new()?)),
            handles: SpinLock::new(HandleTable::new()),
            threads: SpinLock::new(Vec::new()),
            state: SpinLock::new(ProcessState::Created),
            pending_code: SpinLock::new(None),
            signals: Signals::new(0),
            exited_threads_cpu_ns: AtomicU64::new(0),
        });
        let mut list = PROCESSES.lock();
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(&p));
        Some(p)
    }

    /// The address space, unless the process has terminated.
    pub fn aspace(&self) -> Option<Arc<AddressSpace>> {
        self.aspace.lock().clone()
    }

    pub fn state(&self) -> ProcessState {
        *self.state.lock()
    }

    pub fn is_terminated(&self) -> bool {
        matches!(self.state(), ProcessState::Terminated { .. })
    }

    /// Registers a new thread. Fails once the process has terminated.
    pub fn add_thread(&self, t: Arc<Thread>) -> bool {
        if self.is_terminated() {
            return false;
        }
        self.threads.lock().push(t);
        true
    }

    /// Marks the process running (first thread started).
    pub fn mark_started(&self) {
        let mut s = self.state.lock();
        if *s == ProcessState::Created {
            *s = ProcessState::Running;
        }
    }

    /// Called by a thread of this process as it exits.
    pub fn thread_exited(self: &Arc<Self>, t: &Thread) {
        self.exited_threads_cpu_ns.fetch_add(t.cpu_time_ns.load(Ordering::Relaxed), Ordering::Relaxed);
        let last = {
            let mut threads = self.threads.lock();
            threads.retain(|x| x.koid != t.koid);
            threads.iter().all(|x| x.is_dead() || x.state() == sched::State::New)
        };
        if last {
            let code = self.pending_code.lock().unwrap_or(0);
            self.terminate(code);
        }
    }

    /// Requests termination of every thread with `code` (kill or crash).
    pub fn kill(self: &Arc<Self>, code: i64) {
        {
            let mut pc = self.pending_code.lock();
            if pc.is_none() {
                *pc = Some(code);
            }
        }
        let threads: Vec<Arc<Thread>> = self.threads.lock().clone();
        if threads.iter().all(|t| t.state() == sched::State::New) {
            // Nothing is running: terminate right away.
            self.terminate(code);
            return;
        }
        for t in threads {
            t.mark_kill();
            match t.state() {
                sched::State::Blocked => sched::wake(&t, WakeReason::Killed),
                sched::State::Running => {
                    let cpu = t.sched.lock().cpu;
                    if cpu != crate::arch::percpu::cpu_id_or_boot() {
                        let apic = crate::arch::percpu::get(cpu as usize).apic_id;
                        crate::arch::apic::send_ipi(apic, crate::arch::idt::RESCHED_VECTOR as u32);
                    }
                }
                _ => {}
            }
        }
    }

    /// Final teardown: close all handles, release the address space and
    /// signal `TERMINATED`.
    fn terminate(&self, code: i64) {
        {
            let mut s = self.state.lock();
            if matches!(*s, ProcessState::Terminated { .. }) {
                return;
            }
            *s = ProcessState::Terminated { code };
        }
        let handles = self.handles.lock().take_all();
        drop(handles);
        let unstarted: Vec<Arc<Thread>> = core::mem::take(&mut *self.threads.lock());
        drop(unstarted);
        let aspace = self.aspace.lock().take();
        drop(aspace);
        let level = if code == vabi::EXIT_CODE_CRASHED { crate::log::Level::Warn } else { crate::log::Level::Debug };
        crate::log::write(level, format_args!("process {} ({}) exited with code {}", self.name.as_str(), self.koid, code));
        self.signals.update(0, vabi::signals::TERMINATED);
    }

    pub fn exit_code(&self) -> Option<i64> {
        match self.state() {
            ProcessState::Terminated { code } => Some(code),
            _ => None,
        }
    }

    pub fn info(&self) -> ProcessInfo {
        let threads = self.threads.lock();
        let running_cpu: u64 = threads.iter().map(|t| t.cpu_time_ns.load(Ordering::Relaxed)).sum();
        let state = match self.state() {
            ProcessState::Created | ProcessState::Running => process_state::RUNNING,
            ProcessState::Terminated { code } if code == vabi::EXIT_CODE_CRASHED => process_state::CRASHED,
            ProcessState::Terminated { code } if code == vabi::EXIT_CODE_KILLED => process_state::KILLED,
            ProcessState::Terminated { .. } => process_state::EXITED,
        };
        ProcessInfo {
            koid: self.koid,
            parent_koid: self.parent_koid,
            name: self.name.raw(),
            state,
            threads: threads.len() as u32,
            exit_code: self.exit_code().unwrap_or(0),
            memory_bytes: self.aspace().map(|a| a.committed_bytes()).unwrap_or(0),
            cpu_time_ns: running_cpu + self.exited_threads_cpu_ns.load(Ordering::Relaxed),
            handles: self.handles.lock().len() as u32,
            _reserved: 0,
        }
    }

    pub fn thread_count(&self) -> usize {
        self.threads.lock().len()
    }
}

/// All live processes.
pub fn all() -> Vec<Arc<Process>> {
    PROCESSES.lock().iter().filter_map(|w| w.upgrade()).collect()
}

pub fn find(koid: u64) -> Option<Arc<Process>> {
    PROCESSES.lock().iter().filter_map(|w| w.upgrade()).find(|p| p.koid == koid)
}
