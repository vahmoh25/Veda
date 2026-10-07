//! Linking: a vertex and a fragment shader into a program.
//!
//! The linker matches the stages' interfaces (varyings by name, type and
//! interpolation; uniforms and uniform blocks by name, which must agree in
//! both stages), assigns locations (attributes from `layout`, then
//! `glBindAttribLocation`, then the first free slots), lays out storage, and
//! builds the lists OpenGL ES reports: active attributes and uniforms (a
//! structure broken down to its members, an array listed once as
//! `name[0]`), uniform blocks with std140 offsets, fragment outputs and
//! transform feedback varyings. Implementation limits are enforced here.
//!
//! Storage. Uniforms of the default block live in *slots* of four 32-bit
//! words: every vector, matrix column, scalar and array element takes one
//! slot, in the order of a depth-first walk of the type ([`slot_count`]).
//! Samplers take a slot too (holding the texture unit) and also get a
//! *sampler index*, which the code refers to. Uniform blocks use the std140
//! layout whatever they declare (shared and packed leave it to the
//! implementation). Varyings get vec4 *varying slots*, one per vector,
//! matrix column or array element.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::builtins::{BuiltinVar, Limits};
use crate::diag::{Diagnostics, Loc, error};
use crate::hir::{BlockLayout, Shader, UniformBlock, VarId, VarKind};
use crate::types::{Basic, Element, Sampler, Scalar, Structs, Type};
use crate::{Stage, Version};

/// Program-wide state that linking reads: attribute bindings and the
/// transform feedback varyings the application asked for.
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    /// `glBindAttribLocation(name, index)`.
    pub attributes: Vec<(String, u32)>,
    /// `glTransformFeedbackVaryings` names.
    pub feedback: Vec<String>,
    /// `GL_SEPARATE_ATTRIBS` (one buffer per varying) or interleaved.
    pub feedback_separate: bool,
}

/// An active vertex attribute.
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: String,
    pub ty: Type,
    /// The first location (matrices take one per column).
    pub location: u32,
    pub var: VarId,
}

/// An active uniform as `glGetActiveUniform` reports it: a basic type,
/// possibly an array (`size` elements, the name ending in `[0]`).
#[derive(Clone, Debug)]
pub struct Uniform {
    pub name: String,
    pub ty: Basic,
    /// Array length (1 for a single value).
    pub size: u32,
    pub is_array: bool,
    /// Default block: the first slot of element 0. Blocks: unused.
    pub slot: u32,
    /// For samplers: the sampler index of element 0.
    pub sampler: Option<u32>,
    /// The uniform block it belongs to, with its std140 placement.
    pub block: Option<BlockPlacement>,
    /// A built-in uniform (`gl_DepthRange.*`), filled in by the GL.
    pub builtin: bool,
}

/// Where a block member is in the block's buffer.
#[derive(Clone, Copy, Debug)]
pub struct BlockPlacement {
    pub block: u32,
    pub offset: u32,
    /// Bytes between array elements (0 if not an array).
    pub array_stride: u32,
    /// Bytes between matrix columns (rows if row-major), 0 if not a matrix.
    pub matrix_stride: u32,
    pub row_major: bool,
}

/// A uniform location: which active uniform, and which array element.
#[derive(Clone, Copy, Debug)]
pub struct Location {
    pub uniform: u32,
    pub element: u32,
}

/// An active uniform block.
#[derive(Clone, Debug)]
pub struct BlockInfo {
    pub name: String,
    /// The array element, for arrays of blocks (`name[2]`).
    pub element: Option<u32>,
    /// Bytes of buffer the block needs.
    pub size: u32,
    /// The active uniforms (indices into [`Program::uniforms`]) it holds.
    pub uniforms: Vec<u32>,
    pub vertex: bool,
    pub fragment: bool,
}

/// A sampler: its type and the slot holding its texture unit.
#[derive(Clone, Copy, Debug)]
pub struct SamplerInfo {
    pub sampler: Sampler,
    pub slot: u32,
    pub vertex: bool,
    pub fragment: bool,
}

/// How a varying is interpolated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interpolation {
    Smooth,
    Flat,
    Centroid,
}

/// A varying passed from the vertex to the fragment shader.
#[derive(Clone, Debug)]
pub struct Varying {
    pub name: String,
    pub ty: Type,
    pub interpolation: Interpolation,
    /// The first varying slot, and how many it takes.
    pub slot: u32,
    pub slots: u32,
}

/// A fragment output.
#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    pub location: u32,
    /// Array outputs take consecutive locations.
    pub count: u32,
    /// `float`, `int` or `uint`, and components.
    pub scalar: Scalar,
    pub components: u8,
    /// GLSL ES 1.00's `gl_FragColor`: written to every draw buffer.
    pub broadcast: bool,
}

/// A varying captured by transform feedback.
#[derive(Clone, Debug)]
pub struct Feedback {
    pub name: String,
    pub ty: Basic,
    pub size: u32,
    /// The varying slot and component it starts at, and its components.
    pub slot: u32,
    pub components: u32,
    /// `gl_Position`.
    pub position: bool,
    /// `gl_PointSize`.
    pub point_size: bool,
}

/// Where one stage's variables are, for code generation.
#[derive(Clone, Debug, Default)]
pub struct StageLayout {
    /// Shader inputs: attribute locations (vertex) or varying slots
    /// (fragment).
    pub inputs: BTreeMap<VarId, u32>,
    /// Shader outputs: varying slots (vertex) or output locations
    /// (fragment).
    pub outputs: BTreeMap<VarId, u32>,
    /// Default-block uniforms: their first slot.
    pub uniforms: BTreeMap<VarId, u32>,
    /// Uniforms holding samplers: their first sampler index.
    pub samplers: BTreeMap<VarId, u32>,
    /// Uniform blocks: the shader's block index to the program's block
    /// index (of element 0 for arrays of blocks).
    pub blocks: BTreeMap<u32, u32>,
}

/// A linked program, before code generation.
#[derive(Debug)]
pub struct Linked {
    pub version: Version,
    pub attributes: Vec<Attribute>,
    pub uniforms: Vec<Uniform>,
    pub locations: Vec<Location>,
    /// Slots of default-block storage.
    pub slots: u32,
    pub blocks: Vec<BlockInfo>,
    pub samplers: Vec<SamplerInfo>,
    pub varyings: Vec<Varying>,
    /// Varying slots in use.
    pub varying_slots: u32,
    pub outputs: Vec<Output>,
    pub feedback: Vec<Feedback>,
    pub feedback_separate: bool,
    pub vertex: StageLayout,
    pub fragment: StageLayout,
    /// The vertex shader writes `gl_PointSize`.
    pub writes_point_size: bool,
}

/// Slots a value of type `t` takes in default-block storage.
pub fn slot_count(t: Type, structs: &Structs) -> u32 {
    let one = match t.element {
        Element::Basic(Basic::Matrix(c, _)) => c as u32,
        Element::Basic(_) => 1,
        Element::Struct(id) => structs.get(id).fields.iter().map(|f| slot_count(f.ty, structs)).sum(),
    };
    one * t.array.unwrap_or(1)
}

/// Samplers a value of type `t` holds.
pub fn sampler_count(t: Type, structs: &Structs) -> u32 {
    let one = match t.element {
        Element::Basic(b) => u32::from(b.is_sampler()),
        Element::Struct(id) => structs.get(id).fields.iter().map(|f| sampler_count(f.ty, structs)).sum(),
    };
    one * t.array.unwrap_or(1)
}

/// Varying slots a value of type `t` takes.
pub fn varying_slot_count(t: Type) -> u32 {
    let one = match t.element {
        Element::Basic(Basic::Matrix(c, _)) => c as u32,
        _ => 1,
    };
    one * t.array.unwrap_or(1)
}

// ---- std140 ----------------------------------------------------------------

/// The std140 base alignment and size of `t` (row-major matrices if
/// `row_major`).
pub fn std140(t: Type, row_major: bool, structs: &Structs) -> (u32, u32) {
    let (align, size) = match t.element {
        Element::Basic(b) => match b {
            Basic::Scalar(_) => (4, 4),
            Basic::Vector(_, 2) => (8, 8),
            Basic::Vector(_, 3) => (16, 12),
            Basic::Vector(_, _) => (16, 16),
            Basic::Matrix(c, r) => {
                // An array of columns (rows if row-major), each a vec4-aligned
                // vector.
                let n = if row_major { r } else { c };
                (16, 16 * n as u32)
            }
            _ => (4, 4),
        },
        Element::Struct(id) => {
            let mut offset = 0u32;
            let mut align = 16u32;
            for f in &structs.get(id).fields {
                let (a, s) = std140(f.ty, row_major, structs);
                align = align.max(a);
                offset = offset.next_multiple_of(a) + s;
            }
            (align, offset.next_multiple_of(16))
        }
    };
    match t.array {
        Some(n) => {
            let stride = size.next_multiple_of(16).max(align.next_multiple_of(16));
            (16.max(align), stride * n)
        }
        None => (align, size),
    }
}

/// The std140 array stride of an array of `t`'s element type.
pub fn std140_stride(t: Type, row_major: bool, structs: &Structs) -> u32 {
    let (a, s) = std140(t.element_type(), row_major, structs);
    s.next_multiple_of(16).max(a.next_multiple_of(16))
}

/// The std140 offset of member `index` of a structure's fields.
pub fn std140_member_offset(fields: &[(Type, bool)], index: usize, structs: &Structs) -> u32 {
    let mut offset = 0u32;
    for (i, &(t, rm)) in fields.iter().enumerate() {
        let (a, s) = std140(t, rm, structs);
        offset = offset.next_multiple_of(a);
        if i == index {
            return offset;
        }
        offset += s;
    }
    offset
}

// ---- Linking -----------------------------------------------------------------

/// The result of linking: the program, or nothing and the log.
pub struct LinkResult {
    pub linked: Option<Linked>,
    pub log: String,
}

/// Links a vertex and a fragment shader.
pub fn link(vs: &Shader, fs: &Shader, bindings: &Bindings, limits: &Limits) -> LinkResult {
    let mut diags = Diagnostics::new();
    let linked = Linker { vs, fs, bindings, limits, diags: &mut diags }.run();
    let ok = !diags.has_errors();
    LinkResult { linked: if ok { linked } else { None }, log: diags.log() }
}

struct Linker<'a> {
    vs: &'a Shader,
    fs: &'a Shader,
    bindings: &'a Bindings,
    limits: &'a Limits,
    diags: &'a mut Diagnostics,
}

/// One stage's view of a uniform name.
#[derive(Clone, Copy)]
struct StageUniform {
    var: VarId,
    ty: Type,
}

impl Linker<'_> {
    fn err(&mut self, msg: String) {
        error!(self.diags, Loc::NONE, "{msg}");
    }

    fn run(&mut self) -> Option<Linked> {
        let (vs, fs) = (self.vs, self.fs);
        if vs.stage != Stage::Vertex || fs.stage != Stage::Fragment {
            self.err("a program needs a vertex shader and a fragment shader".into());
            return None;
        }
        if vs.version != fs.version {
            self.err("the vertex and fragment shaders are written in different GLSL ES versions".into());
            return None;
        }
        let mut out = Linked {
            version: vs.version,
            attributes: Vec::new(),
            uniforms: Vec::new(),
            locations: Vec::new(),
            slots: 0,
            blocks: Vec::new(),
            samplers: Vec::new(),
            varyings: Vec::new(),
            varying_slots: 0,
            outputs: Vec::new(),
            feedback: Vec::new(),
            feedback_separate: self.bindings.feedback_separate,
            vertex: StageLayout::default(),
            fragment: StageLayout::default(),
            writes_point_size: false,
        };
        self.attributes(&mut out);
        self.varyings(&mut out);
        self.uniforms(&mut out);
        self.blocks(&mut out);
        self.outputs(&mut out);
        self.feedback(&mut out);
        out.writes_point_size = vs.vars.iter().any(|v| v.kind == VarKind::Builtin(BuiltinVar::PointSize) && v.used);
        Some(out)
    }

    fn type_name(&self, s: &Shader, t: Type) -> String {
        let names = |n| -> String { s.name(n).into() };
        alloc::format!("{}", t.display(&s.structs, &names))
    }

    // ---- Attributes ----------------------------------------------------

    fn attributes(&mut self, out: &mut Linked) {
        let vs = self.vs;
        let max = self.limits.max_vertex_attribs;
        let mut taken = alloc::vec![false; max as usize];
        let mut pending = Vec::new();
        for (i, v) in vs.vars.iter().enumerate() {
            if v.kind != VarKind::Input || !v.used {
                continue;
            }
            let name = String::from(vs.name(v.name));
            let columns = varying_slot_count(v.ty);
            let bound = self.bindings.attributes.iter().rev().find(|(n, _)| *n == name).map(|(_, l)| *l);
            match v.location.or(bound) {
                Some(l) => {
                    if l.checked_add(columns).is_none_or(|end| end > max) {
                        self.err(alloc::format!("attribute '{name}' at location {l} is beyond the {max} locations"));
                        continue;
                    }
                    for c in l..l + columns {
                        if taken[c as usize] && vs.version == Version::V300 {
                            self.err(alloc::format!("attribute '{name}' shares location {c} with another"));
                        }
                        taken[c as usize] = true;
                    }
                    out.attributes.push(Attribute { name, ty: v.ty, location: l, var: VarId(i as u32) });
                }
                None => pending.push((i, name, columns)),
            }
        }
        for (i, name, columns) in pending {
            let v = &vs.vars[i];
            let free = (0..max).find(|&l| l + columns <= max && (l..l + columns).all(|c| !taken[c as usize]));
            match free {
                Some(l) => {
                    for c in l..l + columns {
                        taken[c as usize] = true;
                    }
                    out.attributes.push(Attribute { name, ty: v.ty, location: l, var: VarId(i as u32) });
                }
                None => self.err(alloc::format!("too many vertex attributes (the limit is {max})")),
            }
        }
        out.attributes.sort_by_key(|a| a.location);
        for a in &out.attributes {
            out.vertex.inputs.insert(a.var, a.location);
        }
    }

    // ---- Varyings ------------------------------------------------------

    fn varyings(&mut self, out: &mut Linked) {
        let (vs, fs) = (self.vs, self.fs);
        let mut slot = 0u32;
        // Every fragment input the fragment shader uses needs a vertex
        // output of the same name and type.
        for (fi, f) in fs.vars.iter().enumerate() {
            if f.kind != VarKind::Input {
                continue;
            }
            let name = String::from(fs.name(f.name));
            let vout = vs.vars.iter().enumerate().find(|(_, v)| v.kind == VarKind::Output && vs.name(v.name) == name);
            let Some((vi, v)) = vout else {
                if f.used {
                    self.err(alloc::format!("fragment shader input '{name}' is not declared by the vertex shader"));
                }
                continue;
            };
            if v.ty != f.ty {
                let (a, b) = (self.type_name(vs, v.ty), self.type_name(fs, f.ty));
                self.err(alloc::format!(
                    "varying '{name}' is a '{a}' in the vertex shader and a '{b}' in the fragment shader"
                ));
                continue;
            }
            if vs.version == Version::V300 && v.interp != f.interp {
                self.err(alloc::format!("varying '{name}' has different interpolation qualifiers in the two shaders"));
                continue;
            }
            if f.invariant && !v.invariant {
                self.err(alloc::format!("varying '{name}' is invariant in the fragment shader only"));
                continue;
            }
            let slots = varying_slot_count(v.ty);
            let interpolation = match (f.interp, f.centroid || v.centroid) {
                (crate::ast::Interp::Flat, _) => Interpolation::Flat,
                (_, true) => Interpolation::Centroid,
                _ => Interpolation::Smooth,
            };
            out.varyings.push(Varying { name, ty: v.ty, interpolation, slot, slots });
            out.vertex.outputs.insert(VarId(vi as u32), slot);
            out.fragment.inputs.insert(VarId(fi as u32), slot);
            slot += slots;
        }
        // Vertex outputs nothing reads still get slots (transform feedback
        // may capture them).
        for (vi, v) in vs.vars.iter().enumerate() {
            if v.kind == VarKind::Output && !out.vertex.outputs.contains_key(&VarId(vi as u32)) {
                let slots = varying_slot_count(v.ty);
                out.vertex.outputs.insert(VarId(vi as u32), slot);
                out.varyings.push(Varying {
                    name: String::from(vs.name(v.name)),
                    ty: v.ty,
                    interpolation: Interpolation::Smooth,
                    slot,
                    slots,
                });
                slot += slots;
            }
        }
        // Built-in invariance rules (GLSL ES 1.00 4.6.4).
        let inv = |s: &Shader, b: BuiltinVar| s.vars.iter().any(|v| v.kind == VarKind::Builtin(b) && v.invariant);
        if inv(fs, BuiltinVar::FragCoord) && !inv(vs, BuiltinVar::Position) {
            self.err("gl_FragCoord is invariant but gl_Position is not".into());
        }
        if inv(fs, BuiltinVar::PointCoord) && !inv(vs, BuiltinVar::PointSize) {
            self.err("gl_PointCoord is invariant but gl_PointSize is not".into());
        }
        // The limit, counted as GLSL ES packs varyings into rows of four.
        let used: Vec<Type> =
            out.varyings.iter().filter(|v| out.fragment.inputs.values().any(|&s| s == v.slot)).map(|v| v.ty).collect();
        let max = match vs.version {
            Version::V100 => self.limits.max_varying_vectors,
            Version::V300 => self.limits.max_vertex_output_vectors.min(self.limits.max_fragment_input_vectors),
        };
        if !packs_into(&used, max) {
            self.err(alloc::format!("too many varyings (they do not fit in {max} vectors)"));
        }
        out.varying_slots = slot;
    }

    // ---- Uniforms ------------------------------------------------------

    fn uniforms(&mut self, out: &mut Linked) {
        let (vs, fs) = (self.vs, self.fs);
        // Default-block uniforms by name, in declaration order (vertex
        // shader first).
        let mut names: Vec<(String, Option<StageUniform>, Option<StageUniform>)> = Vec::new();
        for (stage, s) in [(0, vs), (1, fs)] {
            for (i, v) in s.vars.iter().enumerate() {
                if v.kind != VarKind::Uniform {
                    continue;
                }
                let name = String::from(s.name(v.name));
                let su = StageUniform { var: VarId(i as u32), ty: v.ty };
                match names.iter_mut().find(|(n, _, _)| *n == name) {
                    Some(e) => {
                        if stage == 0 {
                            e.1 = Some(su);
                        } else {
                            e.2 = Some(su);
                        }
                    }
                    None => names.push((name, (stage == 0).then_some(su), (stage == 1).then_some(su))),
                }
            }
        }
        let mut slot = 0u32;
        let mut sampler = 0u32;
        for (name, v, f) in names {
            let used_v = v.filter(|u| vs.vars[u.var.0 as usize].used);
            let used_f = f.filter(|u| fs.vars[u.var.0 as usize].used);
            if let (Some(a), Some(b)) = (v, f)
                && !same_type(vs, a.ty, fs, b.ty)
            {
                self.err(alloc::format!("uniform '{name}' has different types in the two shaders"));
                continue;
            }
            if let (Some(a), Some(b)) = (v, f) {
                let (pa, pb) = (vs.vars[a.var.0 as usize].precision, fs.vars[b.var.0 as usize].precision);
                if pa != pb && (used_v.is_some() && used_f.is_some()) {
                    self.err(alloc::format!("uniform '{name}' has different precisions in the two shaders"));
                    continue;
                }
            }
            if used_v.is_none() && used_f.is_none() {
                continue;
            }
            let (shader, u) = match (v, f) {
                (Some(a), _) => (vs, a),
                (None, Some(b)) => (fs, b),
                _ => continue,
            };
            let builtin = name.starts_with("gl_");
            let first_slot = slot;
            let first_sampler = sampler;
            self.flatten(
                &name,
                u.ty,
                shader,
                out,
                &mut slot,
                &mut sampler,
                builtin,
                used_v.is_some(),
                used_f.is_some(),
            );
            if let Some(a) = v {
                out.vertex.uniforms.insert(a.var, first_slot);
                if sampler > first_sampler {
                    out.vertex.samplers.insert(a.var, first_sampler);
                }
            }
            if let Some(b) = f {
                out.fragment.uniforms.insert(b.var, first_slot);
                if sampler > first_sampler {
                    out.fragment.samplers.insert(b.var, first_sampler);
                }
            }
        }
        out.slots = slot;
        // Limits (in vectors, as GLSL ES counts them: one per vector or
        // matrix column, scalars packed four to a vector).
        for (s, limit, what) in [
            (vs, self.limits.max_vertex_uniform_vectors, "vertex"),
            (fs, self.limits.max_fragment_uniform_vectors, "fragment"),
        ] {
            let used: Vec<Type> = s
                .vars
                .iter()
                .filter(|v| v.kind == VarKind::Uniform && v.used && !s.name(v.name).starts_with("gl_"))
                .flat_map(|v| leaf_types(v.ty, &s.structs))
                .filter(|t| {
                    !t.as_basic().is_some_and(Basic::is_sampler)
                        && !t.element_type().as_basic().is_some_and(Basic::is_sampler)
                })
                .collect();
            if !packs_into(&used, limit) {
                self.err(alloc::format!("too many uniforms in the {what} shader (they do not fit in {limit} vectors)"));
            }
            let samplers: u32 = s
                .vars
                .iter()
                .filter(|v| v.kind == VarKind::Uniform && v.used)
                .map(|v| sampler_count(v.ty, &s.structs))
                .sum();
            let max = if what == "vertex" {
                self.limits.max_vertex_texture_image_units
            } else {
                self.limits.max_texture_image_units
            };
            if samplers > max {
                self.err(alloc::format!("too many samplers in the {what} shader (the limit is {max})"));
            }
        }
    }

    /// Lists the leaves of a uniform of type `t` named `name`, giving each
    /// its slots, samplers and locations.
    #[allow(clippy::too_many_arguments)]
    fn flatten(
        &mut self,
        name: &str,
        t: Type,
        s: &Shader,
        out: &mut Linked,
        slot: &mut u32,
        sampler: &mut u32,
        builtin: bool,
        vertex: bool,
        fragment: bool,
    ) {
        match t.element {
            Element::Struct(id) => {
                let n = t.array.unwrap_or(1);
                for e in 0..n {
                    let base = if t.array.is_some() { alloc::format!("{name}[{e}]") } else { String::from(name) };
                    for f in &s.structs.get(id).fields {
                        let member = alloc::format!("{base}.{}", s.name(f.name));
                        self.flatten(&member, f.ty, s, out, slot, sampler, builtin, vertex, fragment);
                    }
                }
            }
            Element::Basic(b) => {
                let size = t.array.unwrap_or(1);
                let is_array = t.array.is_some();
                let index = out.uniforms.len() as u32;
                let sampler_index = if let Basic::Sampler(smp) = b {
                    let first = *sampler;
                    for e in 0..size {
                        out.samplers.push(SamplerInfo { sampler: smp, slot: *slot + e, vertex, fragment });
                    }
                    *sampler += size;
                    Some(first)
                } else {
                    None
                };
                out.uniforms.push(Uniform {
                    name: if is_array { alloc::format!("{name}[0]") } else { String::from(name) },
                    ty: b,
                    size,
                    is_array,
                    slot: *slot,
                    sampler: sampler_index,
                    block: None,
                    builtin,
                });
                for e in 0..size {
                    out.locations.push(Location { uniform: index, element: e });
                }
                *slot += slot_count(t, &s.structs);
            }
        }
    }

    // ---- Uniform blocks ------------------------------------------------

    fn blocks(&mut self, out: &mut Linked) {
        let (vs, fs) = (self.vs, self.fs);
        let mut names: Vec<(String, Option<u32>, Option<u32>)> = Vec::new();
        for (stage, s) in [(0, vs), (1, fs)] {
            for (i, b) in s.blocks.iter().enumerate() {
                let name = String::from(s.name(b.name));
                match names.iter_mut().find(|(n, _, _)| *n == name) {
                    Some(e) => {
                        if stage == 0 {
                            e.1 = Some(i as u32);
                        } else {
                            e.2 = Some(i as u32);
                        }
                    }
                    None => names.push((name, (stage == 0).then_some(i as u32), (stage == 1).then_some(i as u32))),
                }
            }
        }
        let mut per_stage = [0u32; 2];
        for (name, v, f) in names {
            let vb = v.map(|i| &vs.blocks[i as usize]);
            let fb = f.map(|i| &fs.blocks[i as usize]);
            if let (Some(a), Some(b)) = (vb, fb)
                && !same_block(vs, a, fs, b)
            {
                self.err(alloc::format!("uniform block '{name}' differs between the two shaders"));
                continue;
            }
            let (s, b) = match (vb, fb) {
                (Some(a), _) => (vs, a),
                (None, Some(b)) => (fs, b),
                _ => continue,
            };
            // std140 and shared blocks are active even if unused.
            let used = vb.is_some_and(|x| x.used) || fb.is_some_and(|x| x.used);
            if !used && b.layout == BlockLayout::Packed {
                continue;
            }
            let first = out.blocks.len() as u32;
            let elements = b.array.unwrap_or(1);
            for e in 0..elements {
                let index = out.blocks.len() as u32;
                let block_name = match b.array {
                    Some(_) => alloc::format!("{name}[{e}]"),
                    None => name.clone(),
                };
                let fields: Vec<(Type, bool)> = b.members.iter().map(|m| (m.ty, m.row_major)).collect();
                let mut uniforms = Vec::new();
                for (mi, m) in b.members.iter().enumerate() {
                    let offset = std140_member_offset(&fields, mi, &s.structs);
                    let prefix = match b.instance {
                        Some(_) => alloc::format!("{name}.{}", s.name(m.name)),
                        None => String::from(s.name(m.name)),
                    };
                    self.block_leaves(&prefix, m.ty, m.row_major, offset, index, s, out, &mut uniforms);
                }
                // The block's size: the end of its last member, rounded up
                // to a vec4 as a structure's is.
                let end = (0..fields.len())
                    .map(|i| {
                        std140_member_offset(&fields, i, &s.structs) + std140(fields[i].0, fields[i].1, &s.structs).1
                    })
                    .max()
                    .unwrap_or(0);
                let size = end.next_multiple_of(16).max(16);
                out.blocks.push(BlockInfo {
                    name: block_name,
                    element: b.array.map(|_| e),
                    size,
                    uniforms,
                    vertex: vb.is_some(),
                    fragment: fb.is_some(),
                });
            }
            if let Some(i) = v {
                out.vertex.blocks.insert(i, first);
                per_stage[0] += elements;
            }
            if let Some(i) = f {
                out.fragment.blocks.insert(i, first);
                per_stage[1] += elements;
            }
        }
        let max = self.limits.max_uniform_blocks;
        for (n, what) in [(per_stage[0], "vertex"), (per_stage[1], "fragment")] {
            if n > max {
                self.err(alloc::format!("too many uniform blocks in the {what} shader (the limit is {max})"));
            }
        }
        if out.blocks.iter().any(|b| b.size > self.limits.max_uniform_block_size) {
            let m = self.limits.max_uniform_block_size;
            self.err(alloc::format!("a uniform block is larger than {m} bytes"));
        }
    }

    /// Lists a block member's leaves as active uniforms.
    #[allow(clippy::too_many_arguments)]
    fn block_leaves(
        &mut self,
        name: &str,
        t: Type,
        row_major: bool,
        offset: u32,
        block: u32,
        s: &Shader,
        out: &mut Linked,
        list: &mut Vec<u32>,
    ) {
        match t.element {
            Element::Struct(id) => {
                let stride = std140_stride(t, row_major, &s.structs);
                let fields: Vec<(Type, bool)> = s.structs.get(id).fields.iter().map(|f| (f.ty, row_major)).collect();
                for e in 0..t.array.unwrap_or(1) {
                    let base = match t.array {
                        Some(_) => alloc::format!("{name}[{e}]"),
                        None => String::from(name),
                    };
                    let element_offset = offset + e * stride;
                    for (i, f) in s.structs.get(id).fields.iter().enumerate() {
                        let member = alloc::format!("{base}.{}", s.name(f.name));
                        let o = element_offset + std140_member_offset(&fields, i, &s.structs);
                        self.block_leaves(&member, f.ty, row_major, o, block, s, out, list);
                    }
                }
            }
            Element::Basic(b) => {
                let array_stride = if t.array.is_some() { std140_stride(t, row_major, &s.structs) } else { 0 };
                let matrix_stride = if b.is_matrix() { 16 } else { 0 };
                list.push(out.uniforms.len() as u32);
                out.uniforms.push(Uniform {
                    name: if t.array.is_some() { alloc::format!("{name}[0]") } else { String::from(name) },
                    ty: b,
                    size: t.array.unwrap_or(1),
                    is_array: t.array.is_some(),
                    slot: 0,
                    sampler: None,
                    block: Some(BlockPlacement {
                        block,
                        offset,
                        array_stride,
                        matrix_stride,
                        row_major: row_major && b.is_matrix(),
                    }),
                    builtin: false,
                });
            }
        }
    }

    // ---- Fragment outputs ----------------------------------------------

    fn outputs(&mut self, out: &mut Linked) {
        let fs = self.fs;
        let max = self.limits.max_draw_buffers;
        let mut taken = alloc::vec![false; max as usize];
        for (i, v) in fs.vars.iter().enumerate() {
            let (location, count, broadcast) = match v.kind {
                VarKind::Output => (v.location.unwrap_or(0), v.ty.array.unwrap_or(1), false),
                VarKind::Builtin(BuiltinVar::FragColor) if v.used => (0, 1, true),
                VarKind::Builtin(BuiltinVar::FragData) if v.used => (0, v.ty.array.unwrap_or(1), false),
                _ => continue,
            };
            let name = String::from(fs.name(v.name));
            if location.checked_add(count).is_none_or(|end| end > max) {
                self.err(alloc::format!("output '{name}' is beyond the {max} draw buffers"));
                continue;
            }
            for l in location..location + count {
                if taken[l as usize] {
                    self.err(alloc::format!("output '{name}' shares location {l} with another output"));
                }
                taken[l as usize] = true;
            }
            let b = v.ty.element_type().as_basic().unwrap_or(Basic::Vector(Scalar::Float, 4));
            out.outputs.push(Output {
                name,
                location,
                count,
                scalar: b.scalar().unwrap_or(Scalar::Float),
                components: b.components() as u8,
                broadcast,
            });
            out.fragment.outputs.insert(VarId(i as u32), location);
        }
    }

    // ---- Transform feedback --------------------------------------------

    fn feedback(&mut self, out: &mut Linked) {
        let vs = self.vs;
        let names = self.bindings.feedback.clone();
        let mut total = 0u32;
        for n in &names {
            // `name` or `name[i]`.
            let (base, element) = match n.find('[') {
                Some(i) if n.ends_with(']') => (&n[..i], n[i + 1..n.len() - 1].parse::<u32>().ok()),
                _ => (n.as_str(), None),
            };
            if n.contains('[') && element.is_none() {
                self.err(alloc::format!("transform feedback varying '{n}' is not a valid name"));
                continue;
            }
            if base == "gl_Position" || base == "gl_PointSize" {
                let position = base == "gl_Position";
                let (ty, comps) = if position { (Basic::Vector(Scalar::Float, 4), 4) } else { (Basic::FLOAT, 1) };
                out.feedback.push(Feedback {
                    name: n.clone(),
                    ty,
                    size: 1,
                    slot: 0,
                    components: comps,
                    position,
                    point_size: !position,
                });
                total += comps;
                continue;
            }
            let Some((vi, v)) =
                vs.vars.iter().enumerate().find(|(_, v)| v.kind == VarKind::Output && vs.name(v.name) == base)
            else {
                self.err(alloc::format!("transform feedback varying '{n}' is not an output of the vertex shader"));
                continue;
            };
            let Some(&slot) = out.vertex.outputs.get(&VarId(vi as u32)) else { continue };
            let elem = v.ty.element_type();
            let b = elem.as_basic().unwrap_or(Basic::FLOAT);
            let per = varying_slot_count(elem);
            let (first, count) = match (v.ty.array, element) {
                (Some(len), Some(e)) if e < len => (e, 1),
                (Some(_), Some(e)) => {
                    self.err(alloc::format!("transform feedback varying '{n}': index {e} is out of range"));
                    continue;
                }
                (None, Some(_)) => {
                    self.err(alloc::format!("transform feedback varying '{n}' is not an array"));
                    continue;
                }
                (len, None) => (0, len.unwrap_or(1)),
            };
            let components = b.components() * count;
            out.feedback.push(Feedback {
                name: n.clone(),
                ty: b,
                size: count,
                slot: slot + first * per,
                components,
                position: false,
                point_size: false,
            });
            total += components;
            if self.bindings.feedback_separate && components > self.limits.max_transform_feedback_separate_components {
                self.err(alloc::format!("transform feedback varying '{n}' has too many components"));
            }
        }
        if names.iter().enumerate().any(|(i, a)| names[..i].contains(a)) {
            self.err("a transform feedback varying is listed twice".into());
        }
        if self.bindings.feedback_separate {
            if out.feedback.len() as u32 > self.limits.max_transform_feedback_separate_attribs {
                self.err("too many separate transform feedback varyings".into());
            }
        } else if total > self.limits.max_transform_feedback_interleaved_components {
            self.err("too many interleaved transform feedback components".into());
        }
    }
}

/// Whether a uniform's type is the same in both shaders: structures are
/// compared by name and members (they are different types in each shader).
fn same_type(a: &Shader, ta: Type, b: &Shader, tb: Type) -> bool {
    if ta.array != tb.array {
        return false;
    }
    match (ta.element, tb.element) {
        (Element::Basic(x), Element::Basic(y)) => x == y,
        (Element::Struct(x), Element::Struct(y)) => {
            let (sx, sy) = (a.structs.get(x), b.structs.get(y));
            sx.name.map(|n| a.name(n)) == sy.name.map(|n| b.name(n))
                && sx.fields.len() == sy.fields.len()
                && sx.fields.iter().zip(&sy.fields).all(|(f, g)| {
                    a.name(f.name) == b.name(g.name) && f.precision == g.precision && same_type(a, f.ty, b, g.ty)
                })
        }
        _ => false,
    }
}

fn same_block(a: &Shader, x: &UniformBlock, b: &Shader, y: &UniformBlock) -> bool {
    x.array == y.array
        && x.layout == y.layout
        && x.instance.map(|n| a.name(n)) == y.instance.map(|n| b.name(n))
        && x.members.len() == y.members.len()
        && x.members
            .iter()
            .zip(&y.members)
            .all(|(m, n)| a.name(m.name) == b.name(n.name) && m.row_major == n.row_major && same_type(a, m.ty, b, n.ty))
}

/// The basic types (with array sizes) a value of type `t` holds.
fn leaf_types(t: Type, structs: &Structs) -> Vec<Type> {
    match t.element {
        Element::Basic(_) => alloc::vec![t],
        Element::Struct(id) => {
            let mut out = Vec::new();
            for _ in 0..t.array.unwrap_or(1) {
                for f in &structs.get(id).fields {
                    out.extend(leaf_types(f.ty, structs));
                }
            }
            out
        }
    }
}

/// Whether values of these types fit in `rows` vectors of four, packed as
/// GLSL ES 1.00's appendix A does: matrices and arrays of vec4 first, then
/// vec3 (padded), vec2 two to a row, scalars four to a row; each value in
/// contiguous rows of one column group.
pub fn packs_into(types: &[Type], rows: u32) -> bool {
    // Rows needed by each width class: (width in components, rows).
    let mut need = [0u32; 5];
    for t in types {
        let n = t.array.unwrap_or(1);
        match t.element {
            Element::Basic(Basic::Matrix(c, r)) => {
                let width = if r == 4 || c == 4 { 4 } else { r as usize };
                need[width.min(4)] += n * c as u32;
            }
            Element::Basic(b) => {
                let w = b.components().clamp(1, 4) as usize;
                need[w] += n;
            }
            Element::Struct(_) => need[4] += n,
        }
    }
    // vec4s and vec3s take whole rows (a vec3 leaves a column for scalars);
    // vec2s pair up; scalars fill what is left.
    let full = need[4] + need[3];
    let pairs = need[2].div_ceil(2);
    let spare_from_vec3 = need[3];
    let spare_from_pairs = if need[2] % 2 == 1 { 2 } else { 0 };
    let scalars = need[1].saturating_sub(spare_from_vec3 + spare_from_pairs);
    let total = full + pairs + scalars.div_ceil(4);
    total <= rows
}

/// Describes a program's interface for messages and tests.
pub fn describe(l: &Linked) -> String {
    let mut s = String::new();
    for a in &l.attributes {
        let _ = writeln!(s, "attribute {} @{}", a.name, a.location);
    }
    for u in &l.uniforms {
        let _ = writeln!(s, "uniform {} {} x{} slot {}", u.name, u.ty.name(), u.size, u.slot);
    }
    for v in &l.varyings {
        let _ = writeln!(s, "varying {} slot {}+{}", v.name, v.slot, v.slots);
    }
    s
}
