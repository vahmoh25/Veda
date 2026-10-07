//! Asynchronous queries and sync objects (OpenGL ES 3.0 sections 2.14 and
//! 5.2).

use super::{Context, Query, Sync};
use crate::backend::QueryKind;
use crate::gl;

/// The slot of a query target among the context's active queries. The two
/// occlusion targets share one: only one occlusion query can be active.
fn query_slot(target: u32) -> Option<usize> {
    match target {
        gl::ANY_SAMPLES_PASSED | gl::ANY_SAMPLES_PASSED_CONSERVATIVE => Some(0),
        gl::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN => Some(2),
        _ => None,
    }
}

impl Context {
    /// `glGenQueries`.
    pub fn gen_queries(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.queries.generate();
        }
    }

    /// One query name.
    pub fn gen_query(&mut self) -> u32 {
        self.queries.generate()
    }

    /// `glIsQuery`: whether `name` names a query object (one that has been
    /// begun).
    pub fn is_query(&self, name: u32) -> bool {
        self.queries.key(name).is_some()
    }

    /// `glDeleteQueries`. An active query stays active until it ends.
    pub fn delete_queries(&mut self, names: &[u32]) {
        for &name in names {
            if let Some((_, Some(q))) = self.queries.delete(name) {
                self.drop_query(q);
            }
        }
    }

    fn drop_query(&mut self, q: Query) {
        if let Some(b) = q.backend {
            self.backend.destroy_query(b);
        }
    }

    /// `glBeginQuery`.
    pub fn begin_query(&mut self, target: u32, id: u32) {
        let Some(slot) = query_slot(target) else { return self.err(gl::INVALID_ENUM) };
        if self.active_queries[slot].is_some() || id == 0 || !self.queries.is_reserved(id) {
            return self.err(gl::INVALID_OPERATION);
        }
        let key =
            self.queries.get_or_create(id, || Query { target, backend: None, result: None, active: false, start: 0 });
        let q = *self.queries.get(key);
        if q.target != target || q.active {
            return self.err(gl::INVALID_OPERATION);
        }
        if let Some(b) = q.backend {
            self.backend.destroy_query(b);
        }
        let backend = match target {
            gl::ANY_SAMPLES_PASSED => Some(self.backend.begin_query(QueryKind::AnySamplesPassed)),
            gl::ANY_SAMPLES_PASSED_CONSERVATIVE => {
                Some(self.backend.begin_query(QueryKind::AnySamplesPassedConservative))
            }
            _ => None,
        };
        let start = self.primitives_written;
        *self.queries.get_mut(key) = Query { target, backend, result: None, active: true, start };
        // The active query holds a reference, so that deleting it lets it
        // finish.
        self.queries.retain(key);
        self.active_queries[slot] = Some(key);
    }

    /// `glEndQuery`.
    pub fn end_query(&mut self, target: u32) {
        let Some(slot) = query_slot(target) else { return self.err(gl::INVALID_ENUM) };
        let Some(key) = self.active_queries[slot] else { return self.err(gl::INVALID_OPERATION) };
        if self.queries.get(key).target != target {
            return self.err(gl::INVALID_OPERATION);
        }
        self.active_queries[slot] = None;
        let written = self.primitives_written;
        let q = self.queries.get_mut(key);
        q.active = false;
        match q.backend {
            Some(b) => self.backend.end_query(b),
            None => q.result = Some(written - q.start),
        }
        if let Some(dead) = self.queries.release(key) {
            self.drop_query(dead);
        }
    }

    /// `glGetQueryiv` (`CURRENT_QUERY`).
    pub fn get_queryiv(&mut self, target: u32, pname: u32) -> i32 {
        let Some(slot) = query_slot(target) else {
            self.err(gl::INVALID_ENUM);
            return 0;
        };
        if pname != gl::CURRENT_QUERY {
            self.err(gl::INVALID_ENUM);
            return 0;
        }
        match self.active_queries[slot] {
            Some(k) if self.queries.get(k).target == target => self.queries.name(k) as i32,
            _ => 0,
        }
    }

    /// `glGetQueryObjectuiv`: `QUERY_RESULT` (waiting for it) or
    /// `QUERY_RESULT_AVAILABLE`.
    pub fn get_query_objectuiv(&mut self, id: u32, pname: u32) -> u32 {
        let Some(key) = self.queries.key(id) else {
            self.err(gl::INVALID_OPERATION);
            return 0;
        };
        if !matches!(pname, gl::QUERY_RESULT | gl::QUERY_RESULT_AVAILABLE) {
            self.err(gl::INVALID_ENUM);
            return 0;
        }
        let q = *self.queries.get(key);
        if q.active {
            self.err(gl::INVALID_OPERATION);
            return 0;
        }
        let result = match (q.result, q.backend) {
            (Some(r), _) => Some(r),
            (None, Some(b)) => {
                let r = self.backend.query_result(b, pname == gl::QUERY_RESULT);
                if let Some(v) = r {
                    self.queries.get_mut(key).result = Some(v);
                }
                r
            }
            (None, None) => Some(0),
        };
        let boolean = q.target != gl::TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN;
        match (pname, result) {
            (gl::QUERY_RESULT_AVAILABLE, r) => u32::from(r.is_some()),
            (_, Some(v)) if boolean => u32::from(v != 0),
            (_, Some(v)) => v.min(u64::from(u32::MAX)) as u32,
            (_, None) => 0,
        }
    }

    // ---- Sync objects -------------------------------------------------------------

    /// `glFenceSync`: a sync object (0 after an error).
    pub fn fence_sync(&mut self, condition: u32, flags: u32) -> u32 {
        if condition != gl::SYNC_GPU_COMMANDS_COMPLETE {
            self.err(gl::INVALID_ENUM);
            return 0;
        }
        if flags != 0 {
            self.err(gl::INVALID_VALUE);
            return 0;
        }
        let fence = self.backend.fence();
        let handle = loop {
            let h = self.next_sync;
            self.next_sync = self.next_sync.checked_add(1).unwrap_or(1);
            if !self.syncs.contains_key(&h) {
                break h;
            }
        };
        self.syncs.insert(handle, Sync { fence, signaled: false });
        handle
    }

    /// `glIsSync`.
    pub fn is_sync(&self, sync: u32) -> bool {
        self.syncs.contains_key(&sync)
    }

    /// `glDeleteSync`.
    pub fn delete_sync(&mut self, sync: u32) {
        if sync != 0 && self.syncs.remove(&sync).is_none() {
            self.err(gl::INVALID_VALUE);
        }
    }

    fn poll_sync(&mut self, sync: u32, timeout_ns: u64) -> Option<bool> {
        let s = *self.syncs.get(&sync)?;
        if s.signaled {
            return Some(true);
        }
        let done = self.backend.wait_fence(s.fence, timeout_ns);
        if done {
            self.syncs.get_mut(&sync).unwrap().signaled = true;
        }
        Some(done)
    }

    /// `glClientWaitSync`.
    pub fn client_wait_sync(&mut self, sync: u32, flags: u32, timeout_ns: u64) -> u32 {
        if !self.syncs.contains_key(&sync) || flags & !gl::SYNC_FLUSH_COMMANDS_BIT != 0 {
            self.err(gl::INVALID_VALUE);
            return gl::WAIT_FAILED;
        }
        if self.syncs[&sync].signaled {
            return gl::ALREADY_SIGNALED;
        }
        if flags & gl::SYNC_FLUSH_COMMANDS_BIT != 0 {
            self.backend.flush();
        }
        match self.poll_sync(sync, timeout_ns) {
            Some(true) => gl::CONDITION_SATISFIED,
            _ => gl::TIMEOUT_EXPIRED,
        }
    }

    /// `glWaitSync`: the renderer runs commands in order, so the server
    /// never has to wait.
    pub fn wait_sync(&mut self, sync: u32, flags: u32, timeout: u64) {
        if !self.syncs.contains_key(&sync) || flags != 0 || timeout != gl::TIMEOUT_IGNORED {
            self.err(gl::INVALID_VALUE);
        }
    }

    /// `glGetSynciv` (one value).
    pub fn get_synciv(&mut self, sync: u32, pname: u32) -> i32 {
        if !self.syncs.contains_key(&sync) {
            self.err(gl::INVALID_VALUE);
            return 0;
        }
        match pname {
            gl::OBJECT_TYPE => gl::SYNC_FENCE as i32,
            gl::SYNC_STATUS => {
                let done = self.poll_sync(sync, 0).unwrap_or(false);
                (if done { gl::SIGNALED } else { gl::UNSIGNALED }) as i32
            }
            gl::SYNC_CONDITION => gl::SYNC_GPU_COMMANDS_COMPLETE as i32,
            gl::SYNC_FLAGS => 0,
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }
}
