//! Threads.
//!
//! In a guest of the driver VM, threads are Linux's (`std`'s): the
//! program's runtime is Linux's there.

#[cfg(not(veda_guest))]
use alloc::boxed::Box;
#[cfg(not(veda_guest))]
use alloc::sync::Arc;

#[cfg(not(veda_guest))]
use vabi::signals;
use vabi::{Error, nr};

#[cfg(not(veda_guest))]
use crate::object::Handle;
use crate::object::Thread;
#[cfg(not(veda_guest))]
use crate::sync::Mutex;
use crate::sys::call;
#[cfg(not(veda_guest))]
use crate::vm::Mapping;

/// Default stack size for spawned threads.
pub const DEFAULT_STACK: usize = 256 * 1024;

#[cfg(not(veda_guest))]
pub struct JoinHandle<T> {
    thread: Thread,
    result: Arc<Mutex<Option<T>>>,
    stack: Option<Mapping>,
}

#[cfg(not(veda_guest))]
impl<T> JoinHandle<T> {
    /// Waits for the thread to finish and returns its result.
    pub fn join(mut self) -> Result<T, Error> {
        self.thread.wait(signals::TERMINATED, vabi::DEADLINE_INFINITE)?;
        // The thread has exited, so its stack can go.
        drop(self.stack.take());
        self.result.lock().take().ok_or(Error::Canceled)
    }

    pub fn thread(&self) -> &Thread {
        &self.thread
    }
}

#[cfg(not(veda_guest))]
impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // A detached thread may still be running on its stack: leak it (it is
        // reclaimed when the process exits).
        if let Some(s) = self.stack.take() {
            core::mem::forget(s);
        }
    }
}

/// A thread of the guest (Linux's).
#[cfg(veda_guest)]
pub struct JoinHandle<T>(std::thread::JoinHandle<T>);

#[cfg(veda_guest)]
impl<T> JoinHandle<T> {
    /// Waits for the thread to finish and returns its result.
    pub fn join(self) -> Result<T, Error> {
        self.0.join().map_err(|_| Error::Canceled)
    }
}

#[cfg(not(veda_guest))]
extern "sysv64" fn thread_entry(arg: usize, _unused: usize) -> ! {
    // SAFETY: `arg` is the box leaked by `Builder::spawn`.
    let f = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) };
    f();
    crate::sys::thread_exit()
}

/// Configures a thread before spawning it.
pub struct Builder {
    name: &'static str,
    stack_size: usize,
    priority: Option<usize>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Builder {
        Builder { name: "thread", stack_size: DEFAULT_STACK, priority: None }
    }

    pub fn name(mut self, name: &'static str) -> Builder {
        self.name = name;
        self
    }

    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = size.max(16 * 1024).next_multiple_of(4096);
        self
    }

    pub fn priority(mut self, p: usize) -> Builder {
        self.priority = Some(p);
        self
    }

    #[cfg(not(veda_guest))]
    pub fn spawn<F, T>(self, f: F) -> Result<JoinHandle<T>, Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let stack = Mapping::anonymous(self.stack_size)?;
        let h = call(nr::THREAD_CREATE, [0, self.name.as_ptr() as usize, self.name.len(), 0, 0, 0])?;
        // SAFETY: the kernel just created this handle for us.
        let thread = Thread(unsafe { Handle::from_raw(h as u32) });
        if let Some(p) = self.priority {
            call(nr::THREAD_SET_PRIORITY, [thread.raw() as usize, p, 0, 0, 0, 0])?;
        }
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let body: Box<dyn FnOnce() + Send> = Box::new(move || {
            let v = f();
            *slot.lock() = Some(v);
        });
        let arg = Box::into_raw(Box::new(body)) as usize;
        let rsp = stack.addr() + stack.len() - 72;
        if let Err(e) =
            call(nr::THREAD_START, [thread.raw() as usize, thread_entry as *const () as usize, rsp, arg, 0, 0])
        {
            // SAFETY: the thread never started, so we still own the box.
            drop(unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) });
            return Err(e);
        }
        Ok(JoinHandle { thread, result, stack: Some(stack) })
    }

    #[cfg(veda_guest)]
    pub fn spawn<F, T>(self, f: F) -> Result<JoinHandle<T>, Error>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Priorities are Linux's business in the guest.
        let _ = self.priority;
        std::thread::Builder::new()
            .name(self.name.into())
            .stack_size(self.stack_size)
            .spawn(f)
            .map(JoinHandle)
            .map_err(|_| Error::NoMemory)
    }
}

/// Spawns a thread with default settings.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    Builder::new().spawn(f).expect("failed to spawn thread")
}

/// Gives up the rest of this time slice.
pub fn yield_now() {
    let _ = call(nr::YIELD, [0; 6]);
}

/// Sets the priority of the calling thread (see `vabi::priority`).
pub fn set_current_priority(thread: &Thread, p: usize) -> Result<(), Error> {
    call(nr::THREAD_SET_PRIORITY, [thread.raw() as usize, p, 0, 0, 0, 0]).map(|_| ())
}
