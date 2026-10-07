//! VIR: the compiler's SSA intermediate representation.
//!
//! A shader becomes one function ([`Func`]) in static single assignment
//! form over *structured* control flow, as Mesa's NIR has it: the body is a
//! list of [`Node`]s that alternates basic blocks with `if` and `loop`
//! nodes, starting and ending with a block. Jumps (`break`, `continue`,
//! `discard`) end blocks; there are no returns and no calls (functions are
//! inlined). Predecessors follow from the structure:
//!
//! * the first block of an `if`'s branches has the block before the `if`;
//! * the block after an `if` has the last blocks of both branches (those
//!   that do not jump);
//! * a loop's first block has the block before the loop, the loop body's
//!   last block (unless it jumps) and every block of the body ending in
//!   `continue`; the block after a loop has every block ending in `break`.
//!
//! Values are scalars ([`ops::Ty`]); vectors, matrices, arrays and
//! structures are their components. φ functions sit at the start of
//! blocks, one operand per predecessor in [`Func::preds`] order.
//!
//! Both back ends want this shape: the SIMD interpreter runs `if` and
//! `loop` under lane masks, and TGSI has structured `IF`/`BGNLOOP`/`BRK`.

pub mod build;
pub mod opt;
pub mod print;
pub mod verify;

use alloc::vec;
use alloc::vec::Vec;

use crate::Stage;
use crate::builtins::TexLod;
use crate::ops::{self, Ty};
use crate::types::Sampler;

/// An SSA value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Value(pub u32);

/// A basic block.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Block(pub u32);

/// An instruction's index in [`Func::insts`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct InstId(pub u32);

/// A built-in input.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BuiltinIn {
    VertexId,
    InstanceId,
    /// `gl_FragCoord`, by component.
    FragCoord(u8),
    FrontFacing,
    /// `gl_PointCoord`, by component.
    PointCoord(u8),
}

/// A built-in output.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BuiltinOut {
    /// `gl_Position`, by component.
    Position(u8),
    PointSize,
    FragDepth,
}

/// A texture lookup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TexOp {
    pub sampler: Sampler,
    /// The sampler index; with `dynamic`, the first argument is added to it
    /// (clamped to `count`).
    pub index: u32,
    pub dynamic: bool,
    pub count: u32,
    /// How the level of detail is chosen (a bias is [`TexLod::Implicit`]
    /// with `bias`).
    pub lod: TexLod,
    pub bias: bool,
    /// Constant texel offset.
    pub offset: [i8; 3],
    /// Coordinate arguments: the coordinates, the array layer, then the
    /// depth reference of a shadow lookup (projection is already divided
    /// out).
    pub coords: u8,
}

impl TexOp {
    /// Results: one for shadow lookups, four otherwise.
    pub fn results(&self) -> usize {
        if self.sampler.shadow { 1 } else { 4 }
    }

    /// The gradient arguments' size (`TexLod::Grad`).
    pub fn grad_size(&self) -> usize {
        use crate::types::Dim;
        match self.sampler.dim {
            Dim::D3 | Dim::Cube => 3,
            Dim::D2 | Dim::D2Array => 2,
        }
    }
}

/// What an instruction does.
#[derive(Clone, PartialEq, Debug)]
pub enum InstOp {
    Const(ops::Value),
    /// A scalar operation (its arguments as [`ops::eval`] takes them).
    Op(ops::Op),
    /// `cond ? a : b` (arguments: cond, a, b).
    Select,
    /// One operand per predecessor of the block, in [`Func::preds`] order.
    Phi,
    /// An input: a vertex attribute's (location, component) or an
    /// interpolated varying's (slot, component).
    LoadInput {
        slot: u32,
        comp: u8,
    },
    LoadBuiltin(BuiltinIn),
    /// Writes the argument to a varying (vertex) or fragment output
    /// (location, component), when the shader finishes.
    StoreOutput {
        slot: u32,
        comp: u8,
    },
    StoreBuiltin(BuiltinOut),
    /// A default-block uniform's 32 bits at (slot, component).
    LoadUniform {
        slot: u32,
        comp: u8,
    },
    /// The same at `base + index * stride`, with the index (the argument)
    /// clamped to `0..count`.
    LoadUniformIndexed {
        base: u32,
        comp: u8,
        stride: u32,
        count: u32,
    },
    /// A uniform block's 32 bits at a byte offset; with an argument, plus
    /// `argument * stride` (the argument clamped to `0..count`).
    LoadBlock {
        block: u32,
        offset: u32,
        stride: u32,
        count: u32,
    },
    Tex(TexOp),
    /// `textureSize`: arguments: [dynamic index,] level; 2 or 3 results.
    TexSize {
        sampler: Sampler,
        index: u32,
        dynamic: bool,
        count: u32,
    },
    /// The derivative of the argument along x (`false`) or y.
    Deriv {
        y: bool,
    },
    /// Nothing: a value no longer used (removed by passes).
    Nop,
}

impl InstOp {
    /// Whether the instruction must stay even if its results are unused.
    pub fn has_effect(&self) -> bool {
        matches!(self, InstOp::StoreOutput { .. } | InstOp::StoreBuiltin(_))
    }
}

/// An instruction.
#[derive(Clone, Debug)]
pub struct Inst {
    pub op: InstOp,
    pub args: Vec<Value>,
    pub results: Vec<Value>,
}

/// A value's type and where it comes from.
#[derive(Clone, Copy, Debug)]
pub struct ValueData {
    pub ty: Ty,
    pub inst: InstId,
}

/// How a block ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Jump {
    Break,
    Continue,
    /// The fragment is discarded; the invocation ends.
    Discard,
}

/// A basic block.
#[derive(Clone, Debug, Default)]
pub struct BlockData {
    pub insts: Vec<InstId>,
    pub jump: Option<Jump>,
}

/// A control flow node.
#[derive(Clone, Debug)]
pub enum Node {
    Block(Block),
    If(alloc::boxed::Box<IfNode>),
    Loop(alloc::boxed::Box<LoopNode>),
}

#[derive(Clone, Debug)]
pub struct IfNode {
    pub cond: Value,
    pub then: Vec<Node>,
    pub otherwise: Vec<Node>,
}

#[derive(Clone, Debug)]
pub struct LoopNode {
    pub body: Vec<Node>,
}

/// A shader in SSA form.
#[derive(Clone, Debug)]
pub struct Func {
    pub stage: Stage,
    pub values: Vec<ValueData>,
    pub insts: Vec<Inst>,
    pub blocks: Vec<BlockData>,
    pub body: Vec<Node>,
}

impl Func {
    pub fn new(stage: Stage) -> Func {
        Func { stage, values: Vec::new(), insts: Vec::new(), blocks: Vec::new(), body: Vec::new() }
    }

    pub fn new_block(&mut self) -> Block {
        self.blocks.push(BlockData::default());
        Block(self.blocks.len() as u32 - 1)
    }

    pub fn ty(&self, v: Value) -> Ty {
        self.values[v.0 as usize].ty
    }

    pub fn inst(&self, id: InstId) -> &Inst {
        &self.insts[id.0 as usize]
    }

    /// The instruction defining `v`.
    pub fn def(&self, v: Value) -> &Inst {
        &self.insts[self.values[v.0 as usize].inst.0 as usize]
    }

    /// The constant `v` is, if it is one.
    pub fn constant(&self, v: Value) -> Option<ops::Value> {
        match self.def(v).op {
            InstOp::Const(c) => Some(c),
            _ => None,
        }
    }

    /// Creates an instruction with results of the given types (not yet in a
    /// block).
    pub fn create(&mut self, op: InstOp, args: Vec<Value>, result_types: &[Ty]) -> InstId {
        let id = InstId(self.insts.len() as u32);
        let mut results = Vec::with_capacity(result_types.len());
        for &ty in result_types {
            results.push(Value(self.values.len() as u32));
            self.values.push(ValueData { ty, inst: id });
        }
        self.insts.push(Inst { op, args, results });
        id
    }

    /// Appends an instruction to a block.
    pub fn push(&mut self, block: Block, id: InstId) {
        self.blocks[block.0 as usize].insts.push(id);
    }

    /// The predecessors of every block, from the structure.
    pub fn preds(&self) -> Vec<Vec<Block>> {
        let mut preds = vec![Vec::new(); self.blocks.len()];
        let mut breaks = Vec::new();
        let mut continues = Vec::new();
        link_list(&self.body, None, self, &mut preds, &mut breaks, &mut continues);
        preds
    }

    /// Every block in program order (the order of the structure).
    pub fn block_order(&self) -> Vec<Block> {
        let mut out = Vec::with_capacity(self.blocks.len());
        fn walk(list: &[Node], out: &mut Vec<Block>) {
            for n in list {
                match n {
                    Node::Block(b) => out.push(*b),
                    Node::If(i) => {
                        walk(&i.then, out);
                        walk(&i.otherwise, out);
                    }
                    Node::Loop(l) => walk(&l.body, out),
                }
            }
        }
        walk(&self.body, &mut out);
        out
    }

    /// Calls `f` on every use of a value (argument and if-condition).
    pub fn for_each_use(&self, mut f: impl FnMut(Value)) {
        for b in self.block_order() {
            for &i in &self.blocks[b.0 as usize].insts {
                for &a in &self.insts[i.0 as usize].args {
                    f(a);
                }
            }
        }
        fn conds(list: &[Node], f: &mut impl FnMut(Value)) {
            for n in list {
                match n {
                    Node::Block(_) => {}
                    Node::If(i) => {
                        f(i.cond);
                        conds(&i.then, f);
                        conds(&i.otherwise, f);
                    }
                    Node::Loop(l) => conds(&l.body, f),
                }
            }
        }
        conds(&self.body, &mut f);
    }
}

/// The first block of a list (lists always start with one).
pub fn first_block(list: &[Node]) -> Option<Block> {
    match list.first() {
        Some(Node::Block(b)) => Some(*b),
        _ => None,
    }
}

/// The last block of a list (lists always end with one).
pub fn last_block(list: &[Node]) -> Option<Block> {
    match list.last() {
        Some(Node::Block(b)) => Some(*b),
        _ => None,
    }
}

/// Fills in predecessors for a list entered from `entry`; records the
/// blocks ending in `break` and `continue` of the innermost loop.
fn link_list(
    list: &[Node],
    entry: Option<Block>,
    f: &Func,
    preds: &mut [Vec<Block>],
    breaks: &mut Vec<Block>,
    continues: &mut Vec<Block>,
) {
    let mut prev: Option<Block> = entry;
    // `prev` is the block control falls from into the next node, or None
    // if the previous node does not fall through.
    let mut falls = entry.is_some();
    for n in list {
        match n {
            Node::Block(b) => {
                if falls && let Some(p) = prev {
                    preds[b.0 as usize].push(p);
                }
                let jump = f.blocks[b.0 as usize].jump;
                match jump {
                    Some(Jump::Break) => breaks.push(*b),
                    Some(Jump::Continue) => continues.push(*b),
                    _ => {}
                }
                prev = Some(*b);
                falls = jump.is_none();
            }
            Node::If(i) => {
                let before = if falls { prev } else { None };
                link_list(&i.then, before, f, preds, breaks, continues);
                link_list(&i.otherwise, before, f, preds, breaks, continues);
                // The merge block's predecessors: the branches' last blocks
                // that fall through. Encode them as a pending list.
                let ends: Vec<Block> = [last_block(&i.then), last_block(&i.otherwise)]
                    .into_iter()
                    .flatten()
                    .filter(|b| f.blocks[b.0 as usize].jump.is_none() && reachable(*b, preds, f))
                    .collect();
                // The next node is a block (lists alternate): give it the
                // ends now, and mark that nothing else falls into it.
                pending_merge(list, n, &ends, preds);
                prev = None;
                falls = false;
            }
            Node::Loop(l) => {
                let header = first_block(&l.body);
                let mut inner_breaks = Vec::new();
                let mut inner_continues = Vec::new();
                let before = if falls { prev } else { None };
                link_list(&l.body, before, f, preds, &mut inner_breaks, &mut inner_continues);
                if let Some(h) = header {
                    // Back edges: the body's last block (if it falls
                    // through) and every `continue`.
                    if let Some(last) = last_block(&l.body)
                        && f.blocks[last.0 as usize].jump.is_none()
                        && reachable(last, preds, f)
                    {
                        preds[h.0 as usize].push(last);
                    }
                    for c in inner_continues {
                        if reachable(c, preds, f) {
                            preds[h.0 as usize].push(c);
                        }
                    }
                }
                let exits: Vec<Block> = inner_breaks.into_iter().filter(|b| reachable(*b, preds, f)).collect();
                pending_merge(list, n, &exits, preds);
                prev = None;
                falls = false;
            }
        }
    }
}

/// Whether a block can be reached: the entry block, or one with
/// predecessors (filled in program order before it is asked).
fn reachable(b: Block, preds: &[Vec<Block>], f: &Func) -> bool {
    !preds[b.0 as usize].is_empty() || first_block(&f.body) == Some(b)
}

/// Gives the block following node `n` in `list` the predecessors `ends`.
fn pending_merge(list: &[Node], n: &Node, ends: &[Block], preds: &mut [Vec<Block>]) {
    let pos = list.iter().position(|x| core::ptr::eq(x, n));
    if let Some(p) = pos
        && let Some(Node::Block(next)) = list.get(p + 1)
    {
        preds[next.0 as usize].extend_from_slice(ends);
    }
}

/// The scalar type of a value of each [`ops::Ty`], for messages.
pub fn ty_name(t: Ty) -> &'static str {
    match t {
        Ty::F32 => "f32",
        Ty::I32 => "i32",
        Ty::U32 => "u32",
        Ty::Bool => "bool",
    }
}
