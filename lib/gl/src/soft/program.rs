//! Programs as the software renderer runs them: both stages compiled for
//! the SIMD interpreter, with the registers of their interfaces found
//! once.

use alloc::sync::Arc;
use alloc::vec::Vec;

use vglsl::interp::{self, Input, Output, Reg};
use vglsl::ir::{BuiltinIn, BuiltinOut};
use vglsl::link::Interpolation;
use vglsl::program::Program;
use vglsl::types::{Basic, Scalar};

use crate::context::DRAW_BUFFERS;

/// One component of a varying the fragment shader reads.
#[derive(Clone, Copy, Debug)]
pub struct Varying {
    pub interpolation: Interpolation,
    /// The vertex shader's output register (none if it never writes it:
    /// the value is undefined, 0 here).
    pub vs: Option<Reg>,
    /// The fragment shader's input register.
    pub fs: Reg,
}

/// A fragment shader output: the registers of its components and the kind
/// of values it writes.
#[derive(Clone, Copy, Debug)]
pub struct FragOutput {
    pub regs: [Option<Reg>; 4],
    pub scalar: Scalar,
}

/// One captured component of transform feedback.
#[derive(Clone, Copy, Debug)]
pub struct Captured {
    /// The buffer it goes to.
    pub buffer: u8,
    /// The vertex shader register holding it (none: never written, 0).
    pub reg: Option<Reg>,
}

/// A program compiled for the software renderer.
#[derive(Debug)]
pub struct SoftProgram {
    pub program: Arc<Program>,
    pub vs: interp::Program,
    pub fs: interp::Program,
    /// Vertex shader inputs: each attribute location read and the
    /// registers of its components.
    pub attribs: Vec<(u32, [Option<Reg>; 4])>,
    /// Attribute locations read (bit mask).
    pub attrib_mask: u32,
    pub vertex_id: Option<Reg>,
    pub instance_id: Option<Reg>,
    pub position: [Option<Reg>; 4],
    pub point_size: Option<Reg>,
    /// What the fragment shader reads of the vertex shader's outputs, in
    /// the order post-transform vertices store them.
    pub varyings: Vec<Varying>,
    pub frag_coord: [Option<Reg>; 4],
    pub front_facing: Option<Reg>,
    pub point_coord: [Option<Reg>; 2],
    /// The output feeding each draw buffer (by fragment output location).
    pub outputs: [Option<FragOutput>; DRAW_BUFFERS],
    pub frag_depth: Option<Reg>,
    /// Captured components, in the order they are written.
    pub feedback: Vec<Captured>,
    /// Bytes per vertex of each feedback buffer.
    pub feedback_strides: Vec<usize>,
    /// The fragment shader may discard.
    pub discards: bool,
    /// Uses centroid sampling (relevant with multisampling).
    pub centroid: bool,
    /// Registers needed to run either stage.
    pub registers: usize,
}

fn input(p: &interp::Program, i: Input) -> Option<Reg> {
    p.input(i)
}

fn output(p: &interp::Program, o: Output) -> Option<Reg> {
    p.output(o)
}

/// The columns and rows of a basic type (vectors are one column).
fn shape(b: Basic) -> (u32, u32) {
    match b {
        Basic::Matrix(c, r) => (u32::from(c), u32::from(r)),
        Basic::Vector(_, n) => (1, u32::from(n)),
        _ => (1, 1),
    }
}

impl SoftProgram {
    pub fn new(program: Arc<Program>) -> SoftProgram {
        let vs = interp::compile(&program.vertex);
        let fs = interp::compile(&program.fragment);
        let l = &program.linked;
        let mut attribs: Vec<(u32, [Option<Reg>; 4])> = Vec::new();
        let mut attrib_mask = 0u32;
        for &(i, r) in &vs.inputs {
            if let Input::Value { slot, comp } = i {
                match attribs.iter_mut().find(|(l, _)| *l == slot) {
                    Some((_, regs)) => regs[comp as usize & 3] = Some(r),
                    None => {
                        let mut regs = [None; 4];
                        regs[comp as usize & 3] = Some(r);
                        attribs.push((slot, regs));
                    }
                }
                if slot < 32 {
                    attrib_mask |= 1 << slot;
                }
            }
        }
        let position = [0, 1, 2, 3].map(|c| output(&vs, Output::Builtin(BuiltinOut::Position(c))));
        let point_size = output(&vs, Output::Builtin(BuiltinOut::PointSize));
        // The varyings the fragment shader reads, component by component.
        let mut varyings = Vec::new();
        let mut centroid = false;
        for v in &l.varyings {
            for s in 0..v.slots {
                for comp in 0..4u8 {
                    let slot = v.slot + s;
                    let Some(fs_reg) = input(&fs, Input::Value { slot, comp }) else { continue };
                    centroid |= v.interpolation == Interpolation::Centroid;
                    varyings.push(Varying {
                        interpolation: v.interpolation,
                        vs: output(&vs, Output::Value { slot, comp }),
                        fs: fs_reg,
                    });
                }
            }
        }
        let frag_coord = [0, 1, 2, 3].map(|c| input(&fs, Input::Builtin(BuiltinIn::FragCoord(c))));
        let front_facing = input(&fs, Input::Builtin(BuiltinIn::FrontFacing));
        let point_coord = [0, 1].map(|c| input(&fs, Input::Builtin(BuiltinIn::PointCoord(c))));
        let mut outputs = [None; DRAW_BUFFERS];
        for o in &l.outputs {
            for k in 0..o.count {
                let location = o.location + k;
                let regs = [0, 1, 2, 3].map(|c| output(&fs, Output::Value { slot: location, comp: c }));
                let out = FragOutput { regs, scalar: o.scalar };
                if o.broadcast {
                    outputs = [Some(out); DRAW_BUFFERS];
                } else if let Some(slot) = outputs.get_mut(location as usize) {
                    *slot = Some(out);
                }
            }
        }
        let frag_depth = output(&fs, Output::Builtin(BuiltinOut::FragDepth));
        // Transform feedback: each captured variable's components in order.
        let mut feedback = Vec::new();
        let mut strides = Vec::new();
        for (i, f) in l.feedback.iter().enumerate() {
            let buffer = if l.feedback_separate { i as u8 } else { 0 };
            if f.position {
                for reg in position {
                    feedback.push(Captured { buffer, reg });
                }
            } else if f.point_size {
                feedback.push(Captured { buffer, reg: point_size });
            } else {
                let (cols, rows) = shape(f.ty);
                for e in 0..f.size {
                    for c in 0..cols {
                        for r in 0..rows {
                            let slot = f.slot + e * cols + c;
                            feedback.push(Captured { buffer, reg: output(&vs, Output::Value { slot, comp: r as u8 }) });
                        }
                    }
                }
            }
            // Separate: a buffer each; interleaved: one buffer for all.
            let bytes = f.components as usize * 4;
            match strides.first_mut() {
                Some(s) if !l.feedback_separate => *s += bytes,
                _ => strides.push(bytes),
            }
        }
        let registers = vs.registers.max(fs.registers);
        let discards = fs.discards;
        let vertex_id = input(&vs, Input::Builtin(BuiltinIn::VertexId));
        let instance_id = input(&vs, Input::Builtin(BuiltinIn::InstanceId));
        SoftProgram {
            program,
            vs,
            fs,
            attribs,
            attrib_mask,
            vertex_id,
            instance_id,
            position,
            point_size,
            varyings,
            frag_coord,
            front_facing,
            point_coord,
            outputs,
            frag_depth,
            feedback,
            feedback_strides: strides,
            discards,
            centroid,
            registers,
        }
    }
}
