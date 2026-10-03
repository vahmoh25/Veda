//! Commands run on a thread of their own, so that the window stays
//! responsive while they work and Ctrl+C can stop them.
//!
//! A [`Job`] holds the shell while its command runs and hands it back when
//! the command is done. The command writes to an [`Output`] queue, which the
//! window drains into its screen; the queue's event wakes the window when
//! output arrives or the command has finished.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use vabi::{RawHandle, signals};
use vrt::object::Event;
use vrt::sync::Mutex;
use vrt::thread::JoinHandle;

use crate::screen::{Screen, Style};
use crate::shell::Shell;

/// The stack of a command's thread (folder walks recurse).
const STACK: usize = 1 << 20;

/// One change to the screen.
enum Op {
    Styled(String, Style),
    /// Text with ANSI colour sequences.
    Raw(String),
    FinishLine,
    Clear,
}

/// Output on its way from a command to the screen.
pub struct Output {
    ops: Mutex<Vec<Op>>,
    /// Signalled when output arrives or a command finishes.
    event: Event,
}

impl Output {
    pub fn new() -> Option<Arc<Output>> {
        Some(Arc::new(Output { ops: Mutex::new(Vec::new()), event: Event::create().ok()? }))
    }

    fn push(&self, op: Op) {
        let mut ops = self.ops.lock();
        // The window is woken when the queue stops being empty (it drains
        // all of it at once).
        let wake = ops.is_empty();
        match op {
            // Text in the style of the piece before continues it.
            Op::Styled(more, style) => match ops.last_mut() {
                Some(Op::Styled(text, s)) if *s == style => text.push_str(&more),
                _ => ops.push(Op::Styled(more, style)),
            },
            Op::Raw(more) => match ops.last_mut() {
                Some(Op::Raw(text)) => text.push_str(&more),
                _ => ops.push(Op::Raw(more)),
            },
            // What is still queued would be cleared away at once.
            Op::Clear => {
                ops.clear();
                ops.push(Op::Clear);
            }
            op => ops.push(op),
        }
        drop(ops);
        if wake {
            self.wake();
        }
    }

    /// Writes text in `style`.
    pub fn styled(&self, s: &str, style: Style) {
        if !s.is_empty() {
            self.push(Op::Styled(s.into(), style));
        }
    }

    /// Writes text that may contain ANSI colour sequences.
    pub fn raw(&self, s: &str) {
        if !s.is_empty() {
            self.push(Op::Raw(s.into()));
        }
    }

    /// Ends the current line if anything was written to it.
    pub fn finish_line(&self) {
        self.push(Op::FinishLine);
    }

    /// Clears the screen (the `clear` command).
    pub fn clear(&self) {
        self.push(Op::Clear);
    }

    fn wake(&self) {
        let _ = self.event.signal();
    }

    /// Moves the output that arrived to `screen`.
    pub fn drain_into(&self, screen: &mut Screen) {
        // The event first: output that arrives from now on signals it again.
        let _ = self.event.clear();
        let ops = core::mem::take(&mut *self.ops.lock());
        for op in ops {
            match op {
                Op::Styled(text, style) => screen.write_styled(&text, style),
                Op::Raw(text) => screen.write(&text),
                Op::FinishLine => screen.finish_line(),
                Op::Clear => screen.clear(),
            }
        }
    }

    /// Signalled when there is output to drain or a command has finished.
    pub fn handle(&self) -> RawHandle {
        self.event.raw()
    }

    /// Waits until output arrives, a command finishes or `deadline` passes.
    pub fn wait(&self, deadline: u64) {
        let _ = self.event.wait(signals::SIGNALED, deadline);
    }
}

/// A command running on a thread of its own.
pub struct Job {
    /// The command line as it may be shown (Wi-Fi passwords hidden).
    pub shown: String,
    /// When it started (monotonic nanoseconds).
    pub started: u64,
    thread: JoinHandle<()>,
    /// The shell, while the command does not have it.
    shell: Arc<Mutex<Option<Shell>>>,
    done: Arc<AtomicBool>,
}

impl Job {
    /// Runs `line` with the shell from `shell`, writing to `out`. Without a
    /// thread for it (no memory), the shell stays in `shell` and nothing
    /// runs.
    pub fn start(shell: &mut Option<Shell>, line: String, shown: String, out: &Arc<Output>) -> Option<Job> {
        let slot = Arc::new(Mutex::new(shell.take()));
        let done = Arc::new(AtomicBool::new(false));
        let (slot2, done2, out2) = (slot.clone(), done.clone(), out.clone());
        let thread = vrt::thread::Builder::new().name("command").stack_size(STACK).spawn(move || {
            let taken = slot2.lock().take();
            if let Some(mut sh) = taken {
                sh.execute(&line, &out2);
                *slot2.lock() = Some(sh);
            }
            done2.store(true, Ordering::Release);
            out2.wake();
        });
        match thread {
            Ok(thread) => Some(Job { shown, started: vrt::time::now_ns(), thread, shell: slot, done }),
            Err(_) => {
                *shell = slot.lock().take();
                None
            }
        }
    }

    /// The command has finished.
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// The shell back from a finished command (after its thread has ended).
    pub fn finish(self) -> Option<Shell> {
        let _ = self.thread.join();
        self.shell.lock().take()
    }
}
