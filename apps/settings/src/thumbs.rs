//! Image thumbnails, decoded on a background thread.
//!
//! The UI asks for a thumbnail with [`Thumbnailer::get`]; unknown paths are
//! queued for a worker thread that reads the file through its own VFS
//! connection, decodes it with `vimage` and scales it down. Finished
//! thumbnails are handed back through a mutex and an [`Event`] that the UI
//! waits on (see `App::wait_handles`), so decoding never blocks drawing.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use vabi::RawHandle;
use vgfx::Bitmap;
use vrt::object::Event;
use vrt::sync::{Condvar, Mutex};

use crate::fsutil::Fs;

/// Files larger than this are not decoded.
const MAX_FILE: u64 = 48 << 20;

struct Shared {
    queue: Mutex<VecDeque<String>>,
    wake: Condvar,
    done: Mutex<Vec<(String, Option<Bitmap>)>>,
}

/// A cache of thumbnails filled by a worker thread.
pub struct Thumbnailer {
    shared: Arc<Shared>,
    event: Event,
    cache: BTreeMap<String, Option<Bitmap>>,
    pending: BTreeSet<String>,
}

impl Thumbnailer {
    /// Starts the worker; thumbnails fit in `max_w` x `max_h` pixels.
    pub fn new(max_w: u32, max_h: u32) -> Option<Thumbnailer> {
        let event = Event::create().ok()?;
        let theirs = Event::from_handle(event.0.duplicate(None).ok()?);
        let shared =
            Arc::new(Shared { queue: Mutex::new(VecDeque::new()), wake: Condvar::new(), done: Mutex::new(Vec::new()) });
        let worker_shared = shared.clone();
        vrt::thread::Builder::new()
            .name("thumbnails")
            .stack_size(512 * 1024)
            .spawn(move || worker(worker_shared, theirs, max_w, max_h))
            .ok()?;
        Some(Thumbnailer { shared, event, cache: BTreeMap::new(), pending: BTreeSet::new() })
    }

    /// The thumbnail of `path` if it is ready; otherwise queues it.
    pub fn get(&mut self, path: &str) -> Option<&Bitmap> {
        if self.cache.contains_key(path) {
            return self.cache.get(path).and_then(|b| b.as_ref());
        }
        if self.pending.insert(String::from(path)) {
            self.shared.queue.lock().push_back(String::from(path));
            self.shared.wake.notify_one();
        }
        None
    }

    /// The event signalled when thumbnails are ready.
    pub fn event_handle(&self) -> RawHandle {
        self.event.raw()
    }

    /// Moves finished thumbnails into the cache.
    pub fn collect(&mut self) {
        let _ = self.event.clear();
        let done = core::mem::take(&mut *self.shared.done.lock());
        for (path, bmp) in done {
            self.pending.remove(&path);
            self.cache.insert(path, bmp);
        }
    }
}

fn worker(shared: Arc<Shared>, event: Event, max_w: u32, max_h: u32) {
    let fs = Fs::connect();
    loop {
        let path = {
            let mut q = shared.queue.lock();
            loop {
                if let Some(p) = q.pop_front() {
                    break p;
                }
                q = shared.wake.wait(q);
            }
        };
        let bmp = load(&fs, &path, max_w, max_h);
        shared.done.lock().push((path, bmp));
        let _ = event.signal();
    }
}

fn load(fs: &Fs, path: &str, max_w: u32, max_h: u32) -> Option<Bitmap> {
    let st = fs.stat(path)?;
    if st.is_dir || st.size > MAX_FILE {
        return None;
    }
    let data = fs.read(path)?;
    let img = vimage::decode(&data).ok()?;
    let t = vimage::thumbnail(&img, max_w, max_h, vimage::Filter::Bilinear);
    if t.width == 0 || t.height == 0 {
        return None;
    }
    Some(Bitmap::from_straight(t.width as i32, t.height as i32, t.pixels))
}
