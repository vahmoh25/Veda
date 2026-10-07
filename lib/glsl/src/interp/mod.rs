//! The SIMD shader interpreter: compiled bytecode for the software
//! renderer.
//!
//! A stage's IR is compiled ([`compile`]) to straight bytecode over a file
//! of registers, each holding [`LANES`] 32-bit lanes: sixteen vertices, or
//! four 2x2 quads of fragments (so that derivatives and texture levels of
//! detail can be computed). Every instruction processes all lanes; control
//! flow runs under lane masks:
//!
//! * `if` narrows the active mask to the lanes whose condition holds, runs
//!   the other branch with the rest, and skips a branch no lane takes;
//! * a loop runs until no lane is left in it; `break` and `continue` take
//!   lanes out of the rest of the loop or of the iteration, `discard` out
//!   of the shader;
//! * φ functions become copies at the end of their predecessors, made only
//!   in the lanes that take that path, and so does every value used after
//!   the loop that computes it (each lane keeps its last iteration's
//!   value).
//!
//! Work that depends only on uniforms and constants is done once per draw:
//! it forms the *prologue*, whose results sit in registers the main code
//! only reads. Inputs (attributes, interpolated varyings, built-ins) are
//! registers the caller fills; outputs are registers it reads afterwards.
//!
//! Loops are cut short after [`MAX_ITERATIONS`] iterations, so that a shader
//! that never ends cannot hang the program that runs it.

mod compile;
mod exec;

pub use compile::compile;
pub use exec::{Env, Exec, Lanes, NoTextures, Textures};

use alloc::vec::Vec;

use crate::ir::{BuiltinIn, BuiltinOut, TexOp};
use crate::ops::{Op, Ty};
use crate::types::Sampler;

/// Lanes per register.
pub const LANES: usize = 16;

/// A lane mask (one bit per lane).
pub type Mask = u32;

/// All lanes.
pub const ALL: Mask = (1 << LANES) - 1;

/// The most iterations a loop runs (per batch of lanes).
pub const MAX_ITERATIONS: u32 = 1 << 22;

/// A register index.
pub type Reg = u16;

/// A bytecode instruction.
#[derive(Clone, Copy, Debug)]
pub enum Ins {
    /// `d = op(a, b)` (unary operations ignore `b`); `ty` is the result
    /// type where the operation needs it.
    Op {
        op: Op,
        ty: Ty,
        d: Reg,
        a: Reg,
        b: Reg,
    },
    Select {
        d: Reg,
        c: Reg,
        a: Reg,
        b: Reg,
    },
    /// Every lane of `d` is the constant.
    Const {
        d: Reg,
        bits: u32,
    },
    Copy {
        d: Reg,
        s: Reg,
    },
    /// Copies only the active lanes.
    CopyMasked {
        d: Reg,
        s: Reg,
    },
    /// Every lane of `d` is the uniform's 32 bits.
    LoadUniform {
        d: Reg,
        slot: u32,
        comp: u8,
    },
    /// Per lane: the uniform at `base + clamp(index, 0, count - 1)`.
    LoadUniformIndexed {
        d: Reg,
        base: u32,
        comp: u8,
        count: u32,
        index: Reg,
    },
    /// A uniform block's 32 bits at `offset` (plus the per-lane byte offset
    /// in `index`, clamped to the block).
    LoadBlock {
        d: Reg,
        block: u32,
        offset: u32,
        size: u32,
        index: Option<Reg>,
    },
    /// A texture lookup: [`Program::tex`] holds its description.
    Tex {
        op: u16,
    },
    TexSize {
        op: u16,
    },
    /// The derivative of `a` along x (or y).
    Deriv {
        d: Reg,
        a: Reg,
        y: bool,
    },
    /// Narrows the mask to the lanes where `c` holds; jumps to `else_pc` if
    /// none does.
    If {
        c: Reg,
        else_pc: u32,
    },
    /// The other lanes; jumps to `end_pc` if none.
    Else {
        end_pc: u32,
    },
    EndIf,
    Loop,
    /// Back to `start_pc` while lanes remain in the loop.
    EndLoop {
        start_pc: u32,
    },
    Break,
    Continue,
    Discard,
}

/// A texture lookup's registers.
#[derive(Clone, Debug)]
pub struct TexIns {
    pub op: TexOp,
    /// The sampler index's register, for a dynamic index.
    pub index: Option<Reg>,
    /// Coordinates, then bias, level or gradients.
    pub args: Vec<Reg>,
    /// One or four results.
    pub results: Vec<Reg>,
}

/// A `textureSize` call's registers.
#[derive(Clone, Debug)]
pub struct TexSizeIns {
    pub sampler: Sampler,
    pub sampler_index: u32,
    pub index: Option<Reg>,
    pub lod: Reg,
    pub results: Vec<Reg>,
}

/// An input the caller provides in a register before running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// An attribute's (location, component) or a varying's (slot,
    /// component).
    Value {
        slot: u32,
        comp: u8,
    },
    Builtin(BuiltinIn),
}

/// An output the caller reads from a register afterwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    Value { slot: u32, comp: u8 },
    Builtin(BuiltinOut),
}

/// A compiled stage.
#[derive(Clone, Debug)]
pub struct Program {
    /// Run once per draw: constants, uniforms and what depends only on
    /// them.
    pub prologue: Vec<Ins>,
    /// Run for every batch of lanes.
    pub main: Vec<Ins>,
    pub tex: Vec<TexIns>,
    pub tex_size: Vec<TexSizeIns>,
    /// Registers in all.
    pub registers: usize,
    /// Registers `0..uniform_registers` hold the prologue's results.
    pub uniform_registers: usize,
    /// Where the caller puts each input.
    pub inputs: Vec<(Input, Reg)>,
    /// Where the caller finds each output.
    pub outputs: Vec<(Output, Reg)>,
    /// The program may discard fragments.
    pub discards: bool,
    /// The program computes derivatives (directly or for texture levels).
    pub derivatives: bool,
}

impl Program {
    /// The register an input goes in, if the program reads it.
    pub fn input(&self, i: Input) -> Option<Reg> {
        self.inputs.iter().find(|(x, _)| *x == i).map(|(_, r)| *r)
    }

    /// The register an output is in, if the program writes it.
    pub fn output(&self, o: Output) -> Option<Reg> {
        self.outputs.iter().find(|(x, _)| *x == o).map(|(_, r)| *r)
    }
}

/// The interpreter's implementation of one operation on all lanes (for the
/// tests that compare it with [`crate::ops::eval`]).
#[cfg(test)]
pub(crate) fn exec_apply_for_tests(op: Op, ty: Ty, a: &[u32; LANES], c: &[u32; LANES]) -> [u32; LANES] {
    exec::apply(op, ty, a, c)
}
