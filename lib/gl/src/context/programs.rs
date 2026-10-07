//! Shaders, programs and uniforms (OpenGL ES 3.0 section 2.12).
//!
//! Shaders and programs share one namespace. Deleting either only flags it
//! while it is in use (a shader attached to a program, a program current),
//! and its name stays valid until it goes (appendix D.1.3).
//!
//! A successful link produces an *executable*: the compiled program, the
//! renderer's version of it, and the default uniform block's storage. The
//! program keeps its executable until the next successful link, and the
//! context keeps the current one even when the program it came from is
//! relinked unsuccessfully, as section 2.12.3 requires.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use vglsl::Stage;
use vglsl::link::{Bindings, Uniform};
use vglsl::program::Program;
use vglsl::types::{Basic, Dim, Sampler, Scalar};

use super::{Context, TEXTURE_UNITS, UNIFORM_BUFFER_BINDINGS, VERTEX_ATTRIBS};
use crate::backend::ProgramId;
use crate::gl;

/// A shader object.
#[derive(Debug)]
pub(crate) struct ShaderObj {
    pub stage: Stage,
    pub sources: Vec<String>,
    pub compiled: Option<vglsl::hir::Shader>,
    pub compile_status: bool,
    pub log: String,
    pub delete_pending: bool,
    /// Programs it is attached to.
    pub attachments: u32,
}

/// A program object.
#[derive(Debug, Default)]
pub(crate) struct ProgramObj {
    pub vertex: Option<u32>,
    pub fragment: Option<u32>,
    pub attrib_bindings: BTreeMap<String, u32>,
    pub feedback_varyings: Vec<String>,
    pub feedback_separate: bool,
    /// The executable of the last successful link.
    pub exe: Option<u32>,
    pub link_status: bool,
    pub validate_status: bool,
    pub log: String,
    pub delete_pending: bool,
    pub binary_retrievable: bool,
}

#[derive(Debug)]
pub(crate) enum Object {
    Shader(ShaderObj),
    Program(ProgramObj),
}

/// A linked program, ready to draw with.
#[derive(Debug)]
pub(crate) struct Executable {
    pub program: Arc<Program>,
    pub backend: ProgramId,
    /// The default uniform block (slots of four words).
    pub uniforms: Vec<[u32; 4]>,
    /// Each uniform block's binding point.
    pub block_bindings: Vec<u32>,
    refs: u32,
}

/// The shader and program namespace, and the executables.
#[derive(Debug)]
pub(crate) struct Programs {
    pub objects: BTreeMap<u32, Object>,
    next_name: u32,
    pub exes: BTreeMap<u32, Executable>,
    next_exe: u32,
    /// The current program's name (0: none) and its executable.
    pub current: u32,
    pub current_exe: Option<u32>,
}

impl Default for Programs {
    fn default() -> Programs {
        Programs {
            objects: BTreeMap::new(),
            next_name: 1,
            exes: BTreeMap::new(),
            next_exe: 1,
            current: 0,
            current_exe: None,
        }
    }
}

impl Programs {
    fn new_name(&mut self) -> u32 {
        loop {
            let n = self.next_name;
            self.next_name = n.checked_add(1).unwrap_or(1);
            if !self.objects.contains_key(&n) {
                return n;
            }
        }
    }

    pub fn exe(&self, key: u32) -> &Executable {
        &self.exes[&key]
    }
}

/// The OpenGL type enumerant of a GLSL type.
pub fn gl_type(b: Basic) -> u32 {
    use Scalar::*;
    match b {
        Basic::Void => gl::NONE,
        Basic::Scalar(Float) => gl::FLOAT,
        Basic::Scalar(Int) => gl::INT,
        Basic::Scalar(Uint) => gl::UNSIGNED_INT,
        Basic::Scalar(Bool) => gl::BOOL,
        Basic::Vector(s, n) => {
            let base = match s {
                Float => [gl::FLOAT_VEC2, gl::FLOAT_VEC3, gl::FLOAT_VEC4],
                Int => [gl::INT_VEC2, gl::INT_VEC3, gl::INT_VEC4],
                Uint => [gl::UNSIGNED_INT_VEC2, gl::UNSIGNED_INT_VEC3, gl::UNSIGNED_INT_VEC4],
                Bool => [gl::BOOL_VEC2, gl::BOOL_VEC3, gl::BOOL_VEC4],
            };
            base[(n.clamp(2, 4) - 2) as usize]
        }
        Basic::Matrix(c, r) => match (c, r) {
            (2, 2) => gl::FLOAT_MAT2,
            (3, 3) => gl::FLOAT_MAT3,
            (4, 4) => gl::FLOAT_MAT4,
            (2, 3) => gl::FLOAT_MAT2x3,
            (2, 4) => gl::FLOAT_MAT2x4,
            (3, 2) => gl::FLOAT_MAT3x2,
            (3, 4) => gl::FLOAT_MAT3x4,
            (4, 2) => gl::FLOAT_MAT4x2,
            _ => gl::FLOAT_MAT4x3,
        },
        Basic::Sampler(s) => sampler_type(s),
    }
}

fn sampler_type(s: Sampler) -> u32 {
    match (s.dim, s.shadow, s.ty) {
        (Dim::D2, true, _) => gl::SAMPLER_2D_SHADOW,
        (Dim::Cube, true, _) => gl::SAMPLER_CUBE_SHADOW,
        (Dim::D2Array, true, _) => gl::SAMPLER_2D_ARRAY_SHADOW,
        (Dim::D3, true, _) => gl::SAMPLER_3D,
        (Dim::D2, _, Scalar::Int) => gl::INT_SAMPLER_2D,
        (Dim::D3, _, Scalar::Int) => gl::INT_SAMPLER_3D,
        (Dim::Cube, _, Scalar::Int) => gl::INT_SAMPLER_CUBE,
        (Dim::D2Array, _, Scalar::Int) => gl::INT_SAMPLER_2D_ARRAY,
        (Dim::D2, _, Scalar::Uint) => gl::UNSIGNED_INT_SAMPLER_2D,
        (Dim::D3, _, Scalar::Uint) => gl::UNSIGNED_INT_SAMPLER_3D,
        (Dim::Cube, _, Scalar::Uint) => gl::UNSIGNED_INT_SAMPLER_CUBE,
        (Dim::D2Array, _, Scalar::Uint) => gl::UNSIGNED_INT_SAMPLER_2D_ARRAY,
        (Dim::D2, ..) => gl::SAMPLER_2D,
        (Dim::D3, ..) => gl::SAMPLER_3D,
        (Dim::Cube, ..) => gl::SAMPLER_CUBE,
        (Dim::D2Array, ..) => gl::SAMPLER_2D_ARRAY,
    }
}

/// `name[i]` split into `name` and `i`.
fn split_index(name: &str) -> (&str, Option<u32>) {
    if let Some(open) = name.rfind('[')
        && name.ends_with(']')
    {
        let digits = &name[open + 1..name.len() - 1];
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && (digits == "0" || !digits.starts_with('0'))
            && let Ok(i) = digits.parse()
        {
            return (&name[..open], Some(i));
        }
    }
    (name, None)
}

/// The number of bytes a log or name takes with its terminator (0 if
/// empty), as the `*_LENGTH` queries count.
fn length_with_nul(s: &str) -> i32 {
    if s.is_empty() { 0 } else { (s.len() + 1).min(i32::MAX as usize) as i32 }
}

/// Values given to `glUniform*`.
#[derive(Clone, Copy)]
enum Values<'a> {
    F(&'a [f32]),
    I(&'a [i32]),
    U(&'a [u32]),
}

impl Values<'_> {
    fn len(&self) -> usize {
        match self {
            Values::F(v) => v.len(),
            Values::I(v) => v.len(),
            Values::U(v) => v.len(),
        }
    }

    /// Value `i` as stored for a uniform of scalar type `s`.
    fn bits(&self, i: usize, s: Scalar) -> u32 {
        match (self, s) {
            (Values::F(v), Scalar::Bool) => u32::from(v[i] != 0.0),
            (Values::I(v), Scalar::Bool) => u32::from(v[i] != 0),
            (Values::U(v), Scalar::Bool) => u32::from(v[i] != 0),
            (Values::F(v), _) => v[i].to_bits(),
            (Values::I(v), _) => v[i] as u32,
            (Values::U(v), _) => v[i],
        }
    }
}

impl Context {
    fn object(&mut self, name: u32) -> Option<&mut Object> {
        self.programs.objects.get_mut(&name)
    }

    /// The shader named `name`, with the errors for other names.
    fn shader_obj(&mut self, name: u32) -> Option<&mut ShaderObj> {
        match self.programs.objects.get(&name) {
            None => self.err(gl::INVALID_VALUE),
            Some(Object::Program(_)) => self.err(gl::INVALID_OPERATION),
            Some(Object::Shader(_)) => {}
        }
        match self.object(name) {
            Some(Object::Shader(s)) => Some(s),
            _ => None,
        }
    }

    /// The program named `name`, with the errors for other names.
    fn program_obj(&mut self, name: u32) -> Option<&mut ProgramObj> {
        match self.programs.objects.get(&name) {
            None => self.err(gl::INVALID_VALUE),
            Some(Object::Shader(_)) => self.err(gl::INVALID_OPERATION),
            Some(Object::Program(_)) => {}
        }
        match self.object(name) {
            Some(Object::Program(p)) => Some(p),
            _ => None,
        }
    }

    /// The executable of a linked program (`INVALID_OPERATION` if the
    /// program is not linked).
    fn linked_exe(&mut self, program: u32) -> Option<u32> {
        let p = self.program_obj(program)?;
        match (p.link_status, p.exe) {
            (true, Some(e)) => Some(e),
            _ => {
                self.err(gl::INVALID_OPERATION);
                None
            }
        }
    }

    // ---- Shaders -------------------------------------------------------------

    /// `glCreateShader`.
    pub fn create_shader(&mut self, ty: u32) -> u32 {
        let stage = match ty {
            gl::VERTEX_SHADER => Stage::Vertex,
            gl::FRAGMENT_SHADER => Stage::Fragment,
            _ => {
                self.err(gl::INVALID_ENUM);
                return 0;
            }
        };
        let name = self.programs.new_name();
        let s = ShaderObj {
            stage,
            sources: Vec::new(),
            compiled: None,
            compile_status: false,
            log: String::new(),
            delete_pending: false,
            attachments: 0,
        };
        self.programs.objects.insert(name, Object::Shader(s));
        name
    }

    /// `glShaderSource`.
    pub fn shader_source(&mut self, shader: u32, sources: &[&str]) {
        if let Some(s) = self.shader_obj(shader) {
            s.sources = sources.iter().map(|&x| String::from(x)).collect();
        }
    }

    /// `glCompileShader`.
    pub fn compile_shader(&mut self, shader: u32) {
        let options = self.shading;
        if let Some(s) = self.shader_obj(shader) {
            let sources: Vec<&str> = s.sources.iter().map(String::as_str).collect();
            let r = vglsl::compile(s.stage, &sources, &options);
            s.compile_status = r.shader.is_some();
            s.compiled = r.shader;
            s.log = r.log;
        }
    }

    /// `glDeleteShader`.
    pub fn delete_shader(&mut self, shader: u32) {
        if shader == 0 {
            return;
        }
        let Some(s) = self.shader_obj(shader) else { return };
        s.delete_pending = true;
        if s.attachments == 0 {
            self.programs.objects.remove(&shader);
        }
    }

    /// `glIsShader`.
    pub fn is_shader(&self, name: u32) -> bool {
        matches!(self.programs.objects.get(&name), Some(Object::Shader(_)))
    }

    /// `glGetShaderiv`.
    pub fn get_shaderiv(&mut self, shader: u32, pname: u32) -> i32 {
        let Some(s) = self.shader_obj(shader) else { return 0 };

        match pname {
            gl::SHADER_TYPE => match s.stage {
                Stage::Vertex => gl::VERTEX_SHADER as i32,
                Stage::Fragment => gl::FRAGMENT_SHADER as i32,
            },
            gl::DELETE_STATUS => i32::from(s.delete_pending),
            gl::COMPILE_STATUS => i32::from(s.compile_status),
            gl::INFO_LOG_LENGTH => length_with_nul(&s.log),
            gl::SHADER_SOURCE_LENGTH => {
                let n: usize = s.sources.iter().map(String::len).sum();
                if s.sources.is_empty() { 0 } else { (n + 1).min(i32::MAX as usize) as i32 }
            }
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }

    /// `glGetShaderInfoLog`.
    pub fn get_shader_info_log(&mut self, shader: u32) -> String {
        self.shader_obj(shader).map(|s| s.log.clone()).unwrap_or_default()
    }

    /// `glGetShaderSource`: the strings concatenated.
    pub fn get_shader_source(&mut self, shader: u32) -> String {
        self.shader_obj(shader).map(|s| s.sources.concat()).unwrap_or_default()
    }

    /// `glGetShaderPrecisionFormat`: the range (log2 of the magnitudes) and
    /// precision. Every precision is computed as `highp`.
    pub fn get_shader_precision_format(&mut self, shader_type: u32, precision_type: u32) -> ([i32; 2], i32) {
        if !matches!(shader_type, gl::VERTEX_SHADER | gl::FRAGMENT_SHADER) {
            self.err(gl::INVALID_ENUM);
            return ([0; 2], 0);
        }
        match precision_type {
            gl::LOW_FLOAT | gl::MEDIUM_FLOAT | gl::HIGH_FLOAT => ([127, 127], 23),
            gl::LOW_INT | gl::MEDIUM_INT | gl::HIGH_INT => ([31, 30], 0),
            _ => {
                self.err(gl::INVALID_ENUM);
                ([0; 2], 0)
            }
        }
    }

    /// `glReleaseShaderCompiler` (nothing to release).
    pub fn release_shader_compiler(&mut self) {}

    /// `glShaderBinary`: no binary formats are supported.
    pub fn shader_binary(&mut self, _shaders: &[u32], _format: u32, _binary: &[u8]) {
        self.err(gl::INVALID_ENUM);
    }

    // ---- Programs ------------------------------------------------------------

    /// `glCreateProgram`.
    pub fn create_program(&mut self) -> u32 {
        let name = self.programs.new_name();
        self.programs.objects.insert(name, Object::Program(ProgramObj::default()));
        name
    }

    /// `glAttachShader`.
    pub fn attach_shader(&mut self, program: u32, shader: u32) {
        let Some(stage) = self.shader_obj(shader).map(|s| s.stage) else { return };
        let Some(p) = self.program_obj(program) else { return };
        let slot = match stage {
            Stage::Vertex => &mut p.vertex,
            Stage::Fragment => &mut p.fragment,
        };
        // Attached already, or another shader of its kind is.
        if slot.is_some() {
            return self.err(gl::INVALID_OPERATION);
        }
        *slot = Some(shader);
        if let Some(Object::Shader(s)) = self.object(shader) {
            s.attachments += 1;
        }
    }

    /// `glDetachShader`.
    pub fn detach_shader(&mut self, program: u32, shader: u32) {
        if self.shader_obj(shader).is_none() {
            return;
        }
        let Some(p) = self.program_obj(program) else { return };
        if p.vertex == Some(shader) {
            p.vertex = None;
        } else if p.fragment == Some(shader) {
            p.fragment = None;
        } else {
            return self.err(gl::INVALID_OPERATION);
        }
        self.shader_detached(shader);
    }

    fn shader_detached(&mut self, shader: u32) {
        if let Some(Object::Shader(s)) = self.object(shader) {
            s.attachments -= 1;
            if s.attachments == 0 && s.delete_pending {
                self.programs.objects.remove(&shader);
            }
        }
    }

    /// `glGetAttachedShaders`.
    pub fn get_attached_shaders(&mut self, program: u32) -> Vec<u32> {
        self.program_obj(program).map(|p| p.vertex.into_iter().chain(p.fragment).collect()).unwrap_or_default()
    }

    /// `glBindAttribLocation` (used at the next link).
    pub fn bind_attrib_location(&mut self, program: u32, index: u32, name: &str) {
        if index as usize >= VERTEX_ATTRIBS {
            return self.err(gl::INVALID_VALUE);
        }
        if name.starts_with("gl_") {
            return self.err(gl::INVALID_OPERATION);
        }
        if let Some(p) = self.program_obj(program) {
            p.attrib_bindings.insert(String::from(name), index);
        }
    }

    /// `glTransformFeedbackVaryings` (used at the next link).
    pub fn transform_feedback_varyings(&mut self, program: u32, varyings: &[&str], buffer_mode: u32) {
        let separate = match buffer_mode {
            gl::INTERLEAVED_ATTRIBS => false,
            gl::SEPARATE_ATTRIBS => true,
            _ => return self.err(gl::INVALID_ENUM),
        };
        if separate && varyings.len() > super::FEEDBACK_BINDINGS {
            return self.err(gl::INVALID_VALUE);
        }
        if let Some(p) = self.program_obj(program) {
            p.feedback_varyings = varyings.iter().map(|&v| String::from(v)).collect();
            p.feedback_separate = separate;
        }
    }

    /// Whether a transform feedback object that is active uses `program`.
    fn feedback_uses(&self, program: u32) -> bool {
        let mut objects = self.feedbacks.iter().map(|(_, t)| t).chain([&self.default_feedback]);
        objects.any(|t| t.active && t.program == Some(program))
    }

    /// `glLinkProgram`.
    pub fn link_program(&mut self, program: u32) {
        if self.program_obj(program).is_none() {
            return;
        }
        if self.feedback_uses(program) {
            return self.err(gl::INVALID_OPERATION);
        }
        let Some(Object::Program(p)) = self.programs.objects.get(&program) else { return };
        let shader = |name: Option<u32>| match name.and_then(|n| self.programs.objects.get(&n)) {
            Some(Object::Shader(s)) => s.compiled.as_ref(),
            _ => None,
        };
        let (vs, fs) = (shader(p.vertex), shader(p.fragment));
        let result = match (vs, fs) {
            (Some(vs), Some(fs)) => {
                let bindings = Bindings {
                    attributes: p.attrib_bindings.iter().map(|(n, &i)| (n.clone(), i)).collect(),
                    feedback: p.feedback_varyings.clone(),
                    feedback_separate: p.feedback_separate,
                };
                vglsl::program::link(vs, fs, &bindings, &self.shading.limits)
            }
            _ => {
                let missing = match (p.vertex, p.fragment) {
                    (None, _) => "ERROR: no vertex shader is attached\n",
                    (_, None) => "ERROR: no fragment shader is attached\n",
                    _ => "ERROR: an attached shader has not compiled successfully\n",
                };
                vglsl::program::LinkResult { program: None, log: String::from(missing) }
            }
        };
        let exe = result.program.map(|prog| {
            let prog = Arc::new(prog);
            let backend = self.backend.create_program(prog.clone());
            let slots = prog.linked.slots as usize;
            let blocks = prog.linked.blocks.len();
            let key = self.programs.next_exe;
            self.programs.next_exe = self.programs.next_exe.wrapping_add(1).max(1);
            self.programs.exes.insert(
                key,
                Executable {
                    program: prog,
                    backend,
                    uniforms: vec![[0; 4]; slots],
                    block_bindings: vec![0; blocks],
                    refs: 1,
                },
            );
            key
        });
        let Some(Object::Program(p)) = self.programs.objects.get_mut(&program) else { return };
        p.link_status = exe.is_some();
        p.validate_status = false;
        p.log = result.log;
        let old = core::mem::replace(&mut p.exe, exe);
        // A successful relink of the current program installs its new
        // executable; a failed one leaves the current executable in use.
        if let (true, Some(new)) = (self.programs.current == program, exe) {
            self.retain_exe(new);
            let prev = self.programs.current_exe.replace(new);
            self.release_exe(prev);
        }
        self.release_exe(old);
    }

    fn retain_exe(&mut self, key: u32) {
        if let Some(e) = self.programs.exes.get_mut(&key) {
            e.refs += 1;
        }
    }

    fn release_exe(&mut self, key: Option<u32>) {
        let Some(key) = key else { return };
        let Some(e) = self.programs.exes.get_mut(&key) else { return };
        e.refs -= 1;
        if e.refs == 0 {
            let e = self.programs.exes.remove(&key).unwrap();
            self.backend.destroy_program(e.backend);
        }
    }

    /// `glUseProgram`.
    pub fn use_program(&mut self, program: u32) {
        if self.feedback_running() {
            return self.err(gl::INVALID_OPERATION);
        }
        let exe = if program == 0 {
            None
        } else {
            let Some(p) = self.program_obj(program) else { return };
            match (p.link_status, p.exe) {
                (true, Some(e)) => Some(e),
                _ => return self.err(gl::INVALID_OPERATION),
            }
        };
        if let Some(e) = exe {
            self.retain_exe(e);
        }
        let prev_exe = core::mem::replace(&mut self.programs.current_exe, exe);
        self.release_exe(prev_exe);
        let prev = core::mem::replace(&mut self.programs.current, program);
        if prev != program {
            self.maybe_destroy_program(prev);
        }
    }

    /// Destroys a program flagged for deletion once it is not current.
    fn maybe_destroy_program(&mut self, program: u32) {
        let pending = matches!(self.programs.objects.get(&program), Some(Object::Program(p)) if p.delete_pending);
        if !pending || self.programs.current == program {
            return;
        }
        let Some(Object::Program(p)) = self.programs.objects.remove(&program) else { return };
        for s in p.vertex.into_iter().chain(p.fragment) {
            self.shader_detached(s);
        }
        self.release_exe(p.exe);
    }

    /// `glDeleteProgram`.
    pub fn delete_program(&mut self, program: u32) {
        if program == 0 {
            return;
        }
        let Some(p) = self.program_obj(program) else { return };
        p.delete_pending = true;
        self.maybe_destroy_program(program);
    }

    /// `glIsProgram`.
    pub fn is_program(&self, name: u32) -> bool {
        matches!(self.programs.objects.get(&name), Some(Object::Program(_)))
    }

    /// `glValidateProgram`.
    pub fn validate_program(&mut self, program: u32) {
        if self.program_obj(program).is_none() {
            return;
        }
        let Some(Object::Program(p)) = self.programs.objects.get(&program) else { return };
        let (ok, log) = match p.exe {
            Some(e) if p.link_status => match self.sampler_conflict(e) {
                Some(unit) => (false, alloc::format!("ERROR: samplers of different types use texture unit {unit}\n")),
                None => (true, String::new()),
            },
            _ => (false, String::from("ERROR: the program is not linked\n")),
        };
        if let Some(Object::Program(p)) = self.programs.objects.get_mut(&program) {
            p.validate_status = ok;
            p.log = log;
        }
    }

    /// A texture unit that samplers of different types use, if any
    /// (section 2.12.9, "Validation").
    pub(crate) fn sampler_conflict(&self, exe: u32) -> Option<u32> {
        let e = self.programs.exe(exe);
        let samplers = &e.program.linked.samplers;
        let unit = |i: usize| e.uniforms.get(samplers[i].slot as usize).map_or(0, |s| s[0]);
        for i in 0..samplers.len() {
            for j in 0..i {
                if unit(i) == unit(j) && samplers[i].sampler != samplers[j].sampler {
                    return Some(unit(i));
                }
            }
        }
        None
    }

    /// `glGetProgramiv`.
    pub fn get_programiv(&mut self, program: u32, pname: u32) -> i32 {
        let Some(p) = self.program_obj(program) else { return 0 };
        let (status, validate, deleting, log_len) =
            (p.link_status, p.validate_status, p.delete_pending, length_with_nul(&p.log));
        let attached = i32::from(p.vertex.is_some()) + i32::from(p.fragment.is_some());
        let (binary_hint, separate) = (p.binary_retrievable, p.feedback_separate);
        let exe = if p.link_status { p.exe } else { None };
        let linked = exe.map(|e| &self.programs.exe(e).program.linked);
        let max_len = |names: &mut dyn Iterator<Item = &str>| names.map(length_with_nul).max().unwrap_or(0);
        let count = |n: usize| n.min(i32::MAX as usize) as i32;
        match pname {
            gl::DELETE_STATUS => i32::from(deleting),
            gl::LINK_STATUS => i32::from(status),
            gl::VALIDATE_STATUS => i32::from(validate),
            gl::INFO_LOG_LENGTH => log_len,
            gl::ATTACHED_SHADERS => attached,
            gl::ACTIVE_ATTRIBUTES => linked.map_or(0, |l| count(l.attributes.len())),
            gl::ACTIVE_ATTRIBUTE_MAX_LENGTH => {
                linked.map_or(0, |l| max_len(&mut l.attributes.iter().map(|a| a.name.as_str())))
            }
            gl::ACTIVE_UNIFORMS => linked.map_or(0, |l| count(l.uniforms.iter().filter(|u| !u.builtin).count())),
            gl::ACTIVE_UNIFORM_MAX_LENGTH => {
                linked.map_or(0, |l| max_len(&mut l.uniforms.iter().filter(|u| !u.builtin).map(|u| u.name.as_str())))
            }
            gl::ACTIVE_UNIFORM_BLOCKS => linked.map_or(0, |l| count(l.blocks.len())),
            gl::ACTIVE_UNIFORM_BLOCK_MAX_NAME_LENGTH => {
                linked.map_or(0, |l| max_len(&mut l.blocks.iter().map(|b| b.name.as_str())))
            }
            gl::TRANSFORM_FEEDBACK_BUFFER_MODE => {
                (if separate { gl::SEPARATE_ATTRIBS } else { gl::INTERLEAVED_ATTRIBS }) as i32
            }
            gl::TRANSFORM_FEEDBACK_VARYINGS => linked.map_or(0, |l| count(l.feedback.len())),
            gl::TRANSFORM_FEEDBACK_VARYING_MAX_LENGTH => {
                linked.map_or(0, |l| max_len(&mut l.feedback.iter().map(|f| f.name.as_str())))
            }
            gl::PROGRAM_BINARY_RETRIEVABLE_HINT => i32::from(binary_hint),
            gl::PROGRAM_BINARY_LENGTH => 0,
            _ => {
                self.err(gl::INVALID_ENUM);
                0
            }
        }
    }

    /// `glGetProgramInfoLog`.
    pub fn get_program_info_log(&mut self, program: u32) -> String {
        self.program_obj(program).map(|p| p.log.clone()).unwrap_or_default()
    }

    /// `glProgramParameteri`.
    pub fn program_parameteri(&mut self, program: u32, pname: u32, value: i32) {
        if pname != gl::PROGRAM_BINARY_RETRIEVABLE_HINT {
            return self.err(gl::INVALID_ENUM);
        }
        if !matches!(value, 0 | 1) {
            return self.err(gl::INVALID_VALUE);
        }
        if let Some(p) = self.program_obj(program) {
            p.binary_retrievable = value == 1;
        }
    }

    /// `glGetProgramBinary`: no binary formats are supported, so there is
    /// never a binary to return.
    pub fn get_program_binary(&mut self, program: u32) -> Option<(u32, Vec<u8>)> {
        if self.program_obj(program).is_some() {
            self.err(gl::INVALID_OPERATION);
        }
        None
    }

    /// `glProgramBinary`: no binary format is supported.
    pub fn program_binary(&mut self, program: u32, _format: u32, _binary: &[u8]) {
        if self.program_obj(program).is_some() {
            self.err(gl::INVALID_ENUM);
        }
    }

    // ---- Attributes and outputs --------------------------------------------

    /// `glGetAttribLocation`.
    pub fn get_attrib_location(&mut self, program: u32, name: &str) -> i32 {
        let Some(e) = self.linked_exe(program) else { return -1 };
        let l = &self.programs.exe(e).program.linked;
        l.attributes.iter().find(|a| a.name == name).map_or(-1, |a| a.location as i32)
    }

    /// `glGetActiveAttrib`: size, type and name.
    pub fn get_active_attrib(&mut self, program: u32, index: u32) -> Option<(i32, u32, String)> {
        let p = self.program_obj(program)?;
        let exe = if p.link_status { p.exe } else { None };
        let a = exe.and_then(|e| self.programs.exe(e).program.linked.attributes.get(index as usize));
        match a {
            Some(a) => Some((1, a.ty.as_basic().map_or(gl::FLOAT, gl_type), a.name.clone())),
            None => {
                self.err(gl::INVALID_VALUE);
                None
            }
        }
    }

    /// `glGetFragDataLocation`.
    pub fn get_frag_data_location(&mut self, program: u32, name: &str) -> i32 {
        let Some(e) = self.linked_exe(program) else { return -1 };
        let l = &self.programs.exe(e).program.linked;
        let (base, index) = split_index(name);
        for o in l.outputs.iter().filter(|o| !o.broadcast) {
            if o.name == name && (o.count == 1 || index.is_none()) {
                return o.location as i32;
            }
            if let Some(i) = index
                && o.name == base
                && o.count > 1
                && i < o.count
            {
                return (o.location + i) as i32;
            }
        }
        -1
    }

    // ---- Uniforms -----------------------------------------------------------

    /// The active uniforms a program reports (built-ins are filled in by
    /// the GL and not listed).
    fn reported_uniforms(linked: &vglsl::link::Linked) -> impl Iterator<Item = (usize, &Uniform)> {
        linked.uniforms.iter().enumerate().filter(|(_, u)| !u.builtin)
    }

    /// `glGetActiveUniform`: size, type and name.
    pub fn get_active_uniform(&mut self, program: u32, index: u32) -> Option<(i32, u32, String)> {
        let p = self.program_obj(program)?;
        let exe = if p.link_status { p.exe } else { None };
        let u = exe.and_then(|e| Self::reported_uniforms(&self.programs.exe(e).program.linked).nth(index as usize));
        match u {
            Some((_, u)) => Some((u.size as i32, gl_type(u.ty), u.name.clone())),
            None => {
                self.err(gl::INVALID_VALUE);
                None
            }
        }
    }

    /// The `(uniform, element)` a uniform name refers to.
    fn find_uniform(linked: &vglsl::link::Linked, name: &str) -> Option<(usize, u32)> {
        if name.starts_with("gl_") {
            return None;
        }
        let (base, index) = split_index(name);
        for (i, u) in Self::reported_uniforms(linked) {
            if u.is_array {
                let stem = u.name.strip_suffix("[0]").unwrap_or(&u.name);
                if stem == name {
                    return Some((i, 0));
                }
                if let Some(e) = index
                    && stem == base
                    && e < u.size
                {
                    return Some((i, e));
                }
            } else if u.name == name {
                return Some((i, 0));
            }
        }
        None
    }

    /// `glGetUniformLocation`.
    pub fn get_uniform_location(&mut self, program: u32, name: &str) -> i32 {
        let Some(e) = self.linked_exe(program) else { return -1 };
        let l = &self.programs.exe(e).program.linked;
        let Some((u, element)) = Self::find_uniform(l, name) else { return -1 };
        if l.uniforms[u].block.is_some() {
            return -1;
        }
        l.locations.iter().position(|loc| loc.uniform as usize == u && loc.element == element).map_or(-1, |i| i as i32)
    }

    /// `glGetUniformIndices`.
    pub fn get_uniform_indices(&mut self, program: u32, names: &[&str]) -> Vec<u32> {
        let Some(p) = self.program_obj(program) else { return Vec::new() };
        let exe = if p.link_status { p.exe } else { None };
        let Some(e) = exe else { return vec![gl::INVALID_INDEX; names.len()] };
        let l = &self.programs.exe(e).program.linked;
        let reported: Vec<usize> = Self::reported_uniforms(l).map(|(i, _)| i).collect();
        names
            .iter()
            .map(|n| match Self::find_uniform(l, n) {
                Some((u, 0)) => reported.iter().position(|&r| r == u).map_or(gl::INVALID_INDEX, |i| i as u32),
                _ => gl::INVALID_INDEX,
            })
            .collect()
    }

    /// `glGetActiveUniformsiv`.
    pub fn get_active_uniformsiv(&mut self, program: u32, indices: &[u32], pname: u32, out: &mut [i32]) {
        let Some(p) = self.program_obj(program) else { return };
        let exe = if p.link_status { p.exe } else { None };
        let reported: Vec<&Uniform> = match exe {
            Some(e) => Self::reported_uniforms(&self.programs.exe(e).program.linked).map(|(_, u)| u).collect(),
            None => Vec::new(),
        };
        if indices.iter().any(|&i| i as usize >= reported.len()) {
            return self.err(gl::INVALID_VALUE);
        }
        let mut values = Vec::with_capacity(indices.len());
        for &i in indices {
            let u = reported[i as usize];
            let b = u.block;
            values.push(match pname {
                gl::UNIFORM_TYPE => gl_type(u.ty) as i32,
                gl::UNIFORM_SIZE => u.size as i32,
                gl::UNIFORM_NAME_LENGTH => length_with_nul(&u.name),
                gl::UNIFORM_BLOCK_INDEX => b.map_or(-1, |b| b.block as i32),
                gl::UNIFORM_OFFSET => b.map_or(-1, |b| b.offset as i32),
                gl::UNIFORM_ARRAY_STRIDE => b.map_or(-1, |b| b.array_stride as i32),
                gl::UNIFORM_MATRIX_STRIDE => b.map_or(-1, |b| b.matrix_stride as i32),
                gl::UNIFORM_IS_ROW_MAJOR => b.map_or(0, |b| i32::from(b.row_major)),
                _ => return self.err(gl::INVALID_ENUM),
            });
        }
        super::vertex::fill(out, &values);
    }

    /// `glGetUniformBlockIndex`.
    pub fn get_uniform_block_index(&mut self, program: u32, name: &str) -> u32 {
        let Some(p) = self.program_obj(program) else { return gl::INVALID_INDEX };
        let exe = if p.link_status { p.exe } else { None };
        let Some(e) = exe else { return gl::INVALID_INDEX };
        let l = &self.programs.exe(e).program.linked;
        l.blocks.iter().position(|b| b.name == name).map_or(gl::INVALID_INDEX, |i| i as u32)
    }

    /// `glGetActiveUniformBlockiv`.
    pub fn get_active_uniform_blockiv(&mut self, program: u32, index: u32, pname: u32, out: &mut [i32]) {
        let Some(p) = self.program_obj(program) else { return };
        let exe = if p.link_status { p.exe } else { None };
        let Some(e) = exe else { return self.err(gl::INVALID_VALUE) };
        let x = self.programs.exe(e);
        let Some(b) = x.program.linked.blocks.get(index as usize) else { return self.err(gl::INVALID_VALUE) };
        let v: Vec<i32> = match pname {
            gl::UNIFORM_BLOCK_BINDING => vec![x.block_bindings[index as usize] as i32],
            gl::UNIFORM_BLOCK_DATA_SIZE => vec![b.size as i32],
            gl::UNIFORM_BLOCK_NAME_LENGTH => vec![length_with_nul(&b.name)],
            gl::UNIFORM_BLOCK_ACTIVE_UNIFORMS => vec![b.uniforms.len() as i32],
            gl::UNIFORM_BLOCK_ACTIVE_UNIFORM_INDICES => {
                // Indices among the reported uniforms.
                let reported: Vec<usize> = Self::reported_uniforms(&x.program.linked).map(|(i, _)| i).collect();
                b.uniforms
                    .iter()
                    .filter_map(|&u| reported.iter().position(|&r| r == u as usize))
                    .map(|i| i as i32)
                    .collect()
            }
            gl::UNIFORM_BLOCK_REFERENCED_BY_VERTEX_SHADER => vec![i32::from(b.vertex)],
            gl::UNIFORM_BLOCK_REFERENCED_BY_FRAGMENT_SHADER => vec![i32::from(b.fragment)],
            _ => return self.err(gl::INVALID_ENUM),
        };
        super::vertex::fill(out, &v);
    }

    /// `glGetActiveUniformBlockName`.
    pub fn get_active_uniform_block_name(&mut self, program: u32, index: u32) -> String {
        let Some(p) = self.program_obj(program) else { return String::new() };
        let exe = if p.link_status { p.exe } else { None };
        let name =
            exe.and_then(|e| self.programs.exe(e).program.linked.blocks.get(index as usize)).map(|b| b.name.clone());
        name.unwrap_or_else(|| {
            self.err(gl::INVALID_VALUE);
            String::new()
        })
    }

    /// `glUniformBlockBinding`.
    pub fn uniform_block_binding(&mut self, program: u32, index: u32, binding: u32) {
        let Some(p) = self.program_obj(program) else { return };
        let exe = if p.link_status { p.exe } else { None };
        let Some(e) = exe else { return self.err(gl::INVALID_VALUE) };
        let x = self.programs.exes.get_mut(&e).unwrap();
        if index as usize >= x.block_bindings.len() || binding as usize >= UNIFORM_BUFFER_BINDINGS {
            return self.err(gl::INVALID_VALUE);
        }
        x.block_bindings[index as usize] = binding;
    }

    /// `glGetTransformFeedbackVarying`: size, type and name.
    pub fn get_transform_feedback_varying(&mut self, program: u32, index: u32) -> Option<(i32, u32, String)> {
        let p = self.program_obj(program)?;
        let exe = if p.link_status { p.exe } else { None };
        let f = exe.and_then(|e| self.programs.exe(e).program.linked.feedback.get(index as usize));
        match f {
            Some(f) => Some((f.size as i32, gl_type(f.ty), f.name.clone())),
            None => {
                self.err(gl::INVALID_VALUE);
                None
            }
        }
    }

    /// Sets a uniform of the current program: `comps` values per element
    /// of scalar kind `kind` (or a `cols` x `rows` matrix).
    fn set_uniform(&mut self, location: i32, values: Values<'_>, comps: usize, matrix: Option<(u8, u8, bool)>) {
        let Some(exe) = self.programs.current_exe else { return self.err(gl::INVALID_OPERATION) };
        if location == -1 {
            return;
        }
        if !values.len().is_multiple_of(comps) {
            return self.err(gl::INVALID_VALUE);
        }
        let count = values.len() / comps;
        let e = self.programs.exes.get(&exe).unwrap();
        let l = &e.program.linked;
        let Some(loc) = usize::try_from(location).ok().and_then(|i| l.locations.get(i)) else {
            return self.err(gl::INVALID_OPERATION);
        };
        let u = &l.uniforms[loc.uniform as usize];
        let (scalar, n, cols) = match u.ty {
            Basic::Scalar(s) => (s, 1, 1),
            Basic::Vector(s, n) => (s, n as usize, 1),
            Basic::Matrix(c, r) => (Scalar::Float, (c * r) as usize, c as usize),
            Basic::Sampler(_) => (Scalar::Int, 1, 1),
            Basic::Void => return self.err(gl::INVALID_OPERATION),
        };
        let sampler = matches!(u.ty, Basic::Sampler(_));
        // The command must match the uniform's type and size.
        let kind_ok = match (values, scalar) {
            (_, Scalar::Bool) => matrix.is_none(),
            (Values::F(_), Scalar::Float) => matrix.is_some() == matches!(u.ty, Basic::Matrix(..)),
            (Values::I(_), Scalar::Int) => true,
            (Values::U(_), Scalar::Uint) => true,
            _ => false,
        };
        let shape_ok = match (matrix, u.ty) {
            (Some((c, r, _)), Basic::Matrix(uc, ur)) => (c, r) == (uc, ur),
            (Some(_), _) => false,
            (None, _) => comps == n,
        };
        if !kind_ok || !shape_ok || (count > 1 && !u.is_array) || (sampler && comps != 1) {
            return self.err(gl::INVALID_OPERATION);
        }
        if sampler
            && let Values::I(v) = values
            && v.iter().any(|&x| x < 0 || x as usize >= TEXTURE_UNITS)
        {
            return self.err(gl::INVALID_VALUE);
        }
        let (base, size, element) = (u.slot as usize, u.size, loc.element);
        let elements = count.min((size - element) as usize);
        let per = cols;
        let rows = if matrix.is_some() { n / cols } else { n };
        let transpose = matrix.is_some_and(|m| m.2);
        let e = self.programs.exes.get_mut(&exe).unwrap();
        for k in 0..elements {
            let first = base + (element as usize + k) * per;
            for c in 0..cols {
                let Some(slot) = e.uniforms.get_mut(first + c) else { break };
                for (r, word) in slot.iter_mut().enumerate().take(rows) {
                    let i = k * comps + if transpose { r * cols + c } else { c * rows + r };
                    *word = values.bits(i, scalar);
                }
            }
        }
    }

    /// `glUniform{1,2,3,4}fv`: `size` components per element.
    pub fn uniformfv(&mut self, location: i32, size: usize, v: &[f32]) {
        self.set_uniform(location, Values::F(v), size.clamp(1, 4), None);
    }

    /// `glUniform{1,2,3,4}iv`.
    pub fn uniformiv(&mut self, location: i32, size: usize, v: &[i32]) {
        self.set_uniform(location, Values::I(v), size.clamp(1, 4), None);
    }

    /// `glUniform{1,2,3,4}uiv`.
    pub fn uniformuiv(&mut self, location: i32, size: usize, v: &[u32]) {
        self.set_uniform(location, Values::U(v), size.clamp(1, 4), None);
    }

    /// `glUniform1f`.
    pub fn uniform1f(&mut self, location: i32, x: f32) {
        self.uniformfv(location, 1, &[x]);
    }

    /// `glUniform2f`.
    pub fn uniform2f(&mut self, location: i32, x: f32, y: f32) {
        self.uniformfv(location, 2, &[x, y]);
    }

    /// `glUniform3f`.
    pub fn uniform3f(&mut self, location: i32, x: f32, y: f32, z: f32) {
        self.uniformfv(location, 3, &[x, y, z]);
    }

    /// `glUniform4f`.
    pub fn uniform4f(&mut self, location: i32, x: f32, y: f32, z: f32, w: f32) {
        self.uniformfv(location, 4, &[x, y, z, w]);
    }

    /// `glUniform1i`.
    pub fn uniform1i(&mut self, location: i32, x: i32) {
        self.uniformiv(location, 1, &[x]);
    }

    /// `glUniform2i`.
    pub fn uniform2i(&mut self, location: i32, x: i32, y: i32) {
        self.uniformiv(location, 2, &[x, y]);
    }

    /// `glUniform3i`.
    pub fn uniform3i(&mut self, location: i32, x: i32, y: i32, z: i32) {
        self.uniformiv(location, 3, &[x, y, z]);
    }

    /// `glUniform4i`.
    pub fn uniform4i(&mut self, location: i32, x: i32, y: i32, z: i32, w: i32) {
        self.uniformiv(location, 4, &[x, y, z, w]);
    }

    /// `glUniform1ui`.
    pub fn uniform1ui(&mut self, location: i32, x: u32) {
        self.uniformuiv(location, 1, &[x]);
    }

    /// `glUniform2ui`.
    pub fn uniform2ui(&mut self, location: i32, x: u32, y: u32) {
        self.uniformuiv(location, 2, &[x, y]);
    }

    /// `glUniform3ui`.
    pub fn uniform3ui(&mut self, location: i32, x: u32, y: u32, z: u32) {
        self.uniformuiv(location, 3, &[x, y, z]);
    }

    /// `glUniform4ui`.
    pub fn uniform4ui(&mut self, location: i32, x: u32, y: u32, z: u32, w: u32) {
        self.uniformuiv(location, 4, &[x, y, z, w]);
    }

    /// `glUniformMatrix{C}x{R}fv` (`glUniformMatrix4fv` is 4 x 4): `C`
    /// columns of `R` components, column by column unless `transpose`.
    pub fn uniform_matrixfv(&mut self, location: i32, cols: u8, rows: u8, transpose: bool, v: &[f32]) {
        if !(2..=4).contains(&cols) || !(2..=4).contains(&rows) {
            return self.err(gl::INVALID_VALUE);
        }
        self.set_uniform(location, Values::F(v), (cols * rows) as usize, Some((cols, rows, transpose)));
    }

    /// `glUniformMatrix2fv`.
    pub fn uniform_matrix2fv(&mut self, location: i32, transpose: bool, v: &[f32]) {
        self.uniform_matrixfv(location, 2, 2, transpose, v);
    }

    /// `glUniformMatrix3fv`.
    pub fn uniform_matrix3fv(&mut self, location: i32, transpose: bool, v: &[f32]) {
        self.uniform_matrixfv(location, 3, 3, transpose, v);
    }

    /// `glUniformMatrix4fv`.
    pub fn uniform_matrix4fv(&mut self, location: i32, transpose: bool, v: &[f32]) {
        self.uniform_matrixfv(location, 4, 4, transpose, v);
    }

    /// The stored words of a uniform element (`glGetUniform*`), with its
    /// scalar kind.
    fn uniform_words(&mut self, program: u32, location: i32) -> Option<(Vec<u32>, Scalar)> {
        let e = self.linked_exe(program)?;
        let x = self.programs.exe(e);
        let l = &x.program.linked;
        let Some(loc) = usize::try_from(location).ok().and_then(|i| l.locations.get(i)) else {
            self.err(gl::INVALID_OPERATION);
            return None;
        };
        let u = &l.uniforms[loc.uniform as usize];
        let (scalar, cols, rows) = match u.ty {
            Basic::Scalar(s) => (s, 1, 1),
            Basic::Vector(s, n) => (s, 1, n as usize),
            Basic::Matrix(c, r) => (Scalar::Float, c as usize, r as usize),
            _ => (Scalar::Int, 1, 1),
        };
        let first = u.slot as usize + loc.element as usize * cols;
        let mut words = Vec::with_capacity(cols * rows);
        for c in 0..cols {
            let slot = x.uniforms.get(first + c).copied().unwrap_or([0; 4]);
            words.extend_from_slice(&slot[..rows]);
        }
        Some((words, scalar))
    }

    /// `glGetUniformfv`.
    pub fn get_uniformfv(&mut self, program: u32, location: i32, out: &mut [f32]) {
        if let Some((w, s)) = self.uniform_words(program, location) {
            let v: Vec<f32> = w
                .iter()
                .map(|&b| match s {
                    Scalar::Float => f32::from_bits(b),
                    Scalar::Int => b as i32 as f32,
                    Scalar::Uint => b as f32,
                    Scalar::Bool => f32::from(u8::from(b != 0)),
                })
                .collect();
            super::vertex::fill(out, &v);
        }
    }

    /// `glGetUniformiv`.
    pub fn get_uniformiv(&mut self, program: u32, location: i32, out: &mut [i32]) {
        if let Some((w, s)) = self.uniform_words(program, location) {
            let v: Vec<i32> = w
                .iter()
                .map(|&b| match s {
                    Scalar::Float => vmath::f32::round(f32::from_bits(b)) as i32,
                    Scalar::Bool => i32::from(b != 0),
                    _ => b as i32,
                })
                .collect();
            super::vertex::fill(out, &v);
        }
    }

    /// `glGetUniformuiv`.
    pub fn get_uniformuiv(&mut self, program: u32, location: i32, out: &mut [u32]) {
        if let Some((w, s)) = self.uniform_words(program, location) {
            let v: Vec<u32> = w
                .iter()
                .map(|&b| match s {
                    Scalar::Float => vmath::f32::round(f32::from_bits(b)) as u32,
                    Scalar::Bool => u32::from(b != 0),
                    _ => b,
                })
                .collect();
            super::vertex::fill(out, &v);
        }
    }

    /// The current program's name (`CURRENT_PROGRAM`).
    pub(crate) fn current_program(&self) -> u32 {
        self.programs.current
    }
}
