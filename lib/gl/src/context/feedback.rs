//! Transform feedback objects (OpenGL ES 3.0 section 2.15).

use super::{Context, TransformFeedback};
use crate::gl;

impl Context {
    /// `glGenTransformFeedbacks`.
    pub fn gen_transform_feedbacks(&mut self, names: &mut [u32]) {
        for n in names {
            *n = self.feedbacks.generate();
        }
    }

    /// One transform feedback name.
    pub fn gen_transform_feedback(&mut self) -> u32 {
        self.feedbacks.generate()
    }

    /// `glIsTransformFeedback`.
    pub fn is_transform_feedback(&self, name: u32) -> bool {
        self.feedbacks.key(name).is_some()
    }

    /// `glBindTransformFeedback`.
    pub fn bind_transform_feedback(&mut self, target: u32, id: u32) {
        if target != gl::TRANSFORM_FEEDBACK {
            return self.err(gl::INVALID_ENUM);
        }
        if self.feedback_running() || (id != 0 && !self.feedbacks.is_reserved(id)) {
            return self.err(gl::INVALID_OPERATION);
        }
        self.feedback = if id == 0 { None } else { Some(self.feedbacks.get_or_create(id, TransformFeedback::default)) };
    }

    /// `glDeleteTransformFeedbacks`.
    pub fn delete_transform_feedbacks(&mut self, names: &[u32]) {
        let active = names.iter().filter_map(|&n| self.feedbacks.key(n)).any(|k| self.feedbacks.get(k).active);
        if active {
            return self.err(gl::INVALID_OPERATION);
        }
        for &name in names {
            if name == 0 {
                continue;
            }
            let key = self.feedbacks.key(name);
            if key.is_some() && self.feedback == key {
                self.feedback = None;
            }
            if let Some((_, Some(t))) = self.feedbacks.delete(name) {
                self.release_buffer(t.generic);
                for b in t.bindings {
                    self.release_buffer(b.buffer);
                }
            }
        }
    }

    /// `glBeginTransformFeedback`.
    pub fn begin_transform_feedback(&mut self, primitive_mode: u32) {
        if !matches!(primitive_mode, gl::POINTS | gl::LINES | gl::TRIANGLES) {
            return self.err(gl::INVALID_ENUM);
        }
        if self.tf().active {
            return self.err(gl::INVALID_OPERATION);
        }
        let Some(exe) = self.programs.current_exe else { return self.err(gl::INVALID_OPERATION) };
        let x = self.programs.exe(exe);
        let varyings = x.program.linked.feedback.len();
        let separate = x.program.linked.feedback_separate;
        if varyings == 0 {
            return self.err(gl::INVALID_OPERATION);
        }
        let needed = if separate { varyings } else { 1 };
        if self.tf().bindings[..needed.min(super::FEEDBACK_BINDINGS)].iter().any(|b| b.buffer.is_none()) {
            return self.err(gl::INVALID_OPERATION);
        }
        let program = self.programs.current;
        let t = self.tf_mut();
        t.active = true;
        t.paused = false;
        t.primitive_mode = primitive_mode;
        t.program = Some(program);
        t.vertices = 0;
    }

    /// `glEndTransformFeedback`.
    pub fn end_transform_feedback(&mut self) {
        let t = self.tf_mut();
        if !t.active {
            return self.err(gl::INVALID_OPERATION);
        }
        t.active = false;
        t.paused = false;
        t.program = None;
    }

    /// `glPauseTransformFeedback`.
    pub fn pause_transform_feedback(&mut self) {
        let t = self.tf_mut();
        if !t.active || t.paused {
            return self.err(gl::INVALID_OPERATION);
        }
        t.paused = true;
    }

    /// `glResumeTransformFeedback`.
    pub fn resume_transform_feedback(&mut self) {
        let program = self.programs.current;
        let t = self.tf_mut();
        if !t.active || !t.paused || t.program != Some(program) {
            return self.err(gl::INVALID_OPERATION);
        }
        t.paused = false;
    }
}
