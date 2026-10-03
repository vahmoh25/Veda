//! Events: a bare signal word for cross-thread and cross-process
//! notification.

use alloc::sync::Arc;

use super::Signals;

pub struct Event {
    pub koid: u64,
    pub signals: Signals,
}

impl Event {
    pub fn new() -> Arc<Event> {
        Arc::new(Event { koid: super::new_koid(), signals: Signals::new(0) })
    }
}
