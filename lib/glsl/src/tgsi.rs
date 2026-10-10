//! TGSI, Gallium's shader language, in the text form virglrenderer reads.
//!
//! In virtio-gpu's 3D mode the guest hands shaders to the host as TGSI
//! text; virglrenderer turns them into GLSL for the host's OpenGL (or
//! OpenGL ES, under ANGLE on Windows). This module writes that text for
//! one stage of a linked program, from its optimised IR.
//!
//! The IR's scalar SSA form is kept. Every value gets a component of a
//! temporary register of its own and every operation becomes one
//! instruction writing one component; virglrenderer declares each
//! temporary register as a GLSL variable of its own, so the host's
//! compiler sees straight-line SSA code and allocates registers and
//! vectorises as it likes. φ functions become copies at the ends of their
//! predecessors. Values that need no computation (constants, uniforms,
//! inputs) are not copied: instructions read them where they are.
//!
//! What TGSI, or virglrenderer's reading of it, lacks is lowered here:
//!
//! * `continue`: virglrenderer has no `CONT`. A flag set by the
//!   `continue` guards the rest of the loop body.
//! * Inverse trigonometric and hyperbolic functions: formulas over the
//!   functions TGSI has (`atan` by a minimax polynomial).
//! * `isnan` and `isinf`: integer tests of the bits, which the host's
//!   compiler cannot fold away the way it may fold `x != x`.
//! * Samplers indexed by a variable: a test per array element, as hosts
//!   running GLSL ES 3.10 accept only constant indices.
//! * Half-float conversion (`packHalf2x16`): integer arithmetic.
//!
//! Every constant is a 32-bit integer immediate, so floats reach the host
//! exactly (virglrenderer prints float immediates with 8 digits). As
//! virglrenderer keeps every temporary in a float variable, a constant that
//! is not a normal float (a small integer, all ones) is never moved into
//! one directly: the host's compiler may fold and flush it. It is built at
//! run time instead, by adding it to a uniform that is always zero (the
//! slot after the program's own, which the renderer fills).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::Stage;
use crate::builtins::TexLod;
use crate::ir::{self, Block, BuiltinIn, BuiltinOut, Func, InstId, InstOp, Jump, Node, TexOp, Value};
use crate::link::{Interpolation, Linked};
use crate::ops::Op;
use crate::types::{Dim, Sampler, Scalar};

/// What the host supports beyond the basics.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// `EXT_texture_shadow_lod`: a bias on shadow lookups in cube maps.
    /// Without it, such a bias is ignored (it only changes the mipmap
    /// level a depth cube map is read from).
    pub shadow_lod: bool,
}

/// One stage in TGSI.
#[derive(Clone, Debug)]
pub struct Shader {
    pub text: String,
    /// An upper bound on the tokens the text assembles to (the host
    /// allocates this many).
    pub tokens: u32,
    /// Vertex shaders: the output registers of `gl_Position` and
    /// `gl_PointSize`.
    pub position: Option<u32>,
    pub point_size: Option<u32>,
    /// Vertex shaders: the output register of each varying slot.
    pub varyings: Vec<u32>,
    /// Vertex shaders: the attribute locations they read, in the order of
    /// their input registers (the first location's is IN[0]). The inputs
    /// are packed, as Gallium's drivers take them (one compiles only the
    /// inputs a shader reads, the first into its first slot): a vertex
    /// element for each, in this order.
    pub attributes: Vec<u32>,
    /// The samplers (program sampler indices) the shader reads.
    pub samplers: Vec<u32>,
    /// The uniform blocks (program block indices) it reads; block `b` is
    /// constant buffer `b + 1`.
    pub blocks: Vec<u32>,
}

/// Translates a stage of a linked program.
pub fn translate(f: &Func, linked: &Linked, options: &Options) -> Shader {
    let mut t = Translator::new(f, linked, options);
    t.scan();
    t.place();
    let body = f.body.clone();
    t.list(&body, &Ctx { end: End::Top, lp: None });
    t.finish()
}

// ---- Operands ---------------------------------------------------------------

/// Where a scalar is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Src {
    /// A temporary register's component (register * 4 + component).
    Temp(u32),
    /// An immediate's component (immediate * 4 + component).
    Imm(u32),
    /// An input register's component.
    Input(u32, u8),
    /// A system value (`gl_VertexID`, `gl_InstanceID`).
    System(u32),
    /// A default-block uniform: (slot, component).
    Uniform(u32, u8),
    /// A uniform block's component: (block, vec4, component).
    Ubo(u32, u32, u8),
}

const SWIZZLE: [&str; 4] = ["xxxx", "yyyy", "zzzz", "wwww"];
const MASK: [&str; 4] = ["x", "y", "z", "w"];

impl Src {
    fn text(self) -> String {
        match self {
            Src::Temp(c) => format!("TEMP[{}].{}", c / 4, SWIZZLE[(c % 4) as usize]),
            Src::Imm(c) => format!("IMM[{}].{}", c / 4, SWIZZLE[(c % 4) as usize]),
            Src::Input(r, c) => format!("IN[{r}].{}", SWIZZLE[c as usize]),
            Src::System(r) => format!("SV[{r}].xxxx"),
            Src::Uniform(s, c) => format!("CONST[{s}].{}", SWIZZLE[c as usize]),
            Src::Ubo(b, i, c) => format!("CONST[{}][{i}].{}", b + 1, SWIZZLE[c as usize]),
        }
    }

    fn neg(self) -> String {
        format!("-{}", self.text())
    }

    fn abs(self) -> String {
        format!("|{}|", self.text())
    }
}

/// A temporary component as a destination.
fn dst(c: u32) -> String {
    format!("TEMP[{}].{}", c / 4, MASK[(c % 4) as usize])
}

// ---- Constants --------------------------------------------------------------

const F_ONE: u32 = 0x3F80_0000;
const F_HALF: u32 = 0x3F00_0000;
const LOG2_E: f32 = core::f32::consts::LOG2_E;
const LN_2: f32 = core::f32::consts::LN_2;
const PI: f32 = core::f32::consts::PI;
const PI_2: f32 = core::f32::consts::FRAC_PI_2;
/// The minimax polynomial for `atan` on [0, 1] (odd powers 1 to 11).
const ATAN: [f32; 6] = [0.999_979_3, -0.332_675_64, 0.193_892_5, -0.117_350_32, 0.053_681_38, -0.012_132_321];

// ---- Control flow context ---------------------------------------------------

/// Where control goes at the end of a list.
#[derive(Clone, Copy)]
enum End {
    /// The function's end.
    Top,
    /// The block after an `if`.
    Merge(Block),
    /// The header of the loop whose body the list is.
    Header(Block),
}

/// The innermost loop.
#[derive(Clone, Copy)]
struct LoopCtx {
    header: Block,
    exit: Block,
    /// The flag a `continue` sets, if the body has one.
    flag: Option<u32>,
}

#[derive(Clone, Copy)]
struct Ctx {
    end: End,
    lp: Option<LoopCtx>,
}

// ---- The translator ---------------------------------------------------------

struct Translator<'a> {
    f: &'a Func,
    linked: &'a Linked,
    options: &'a Options,
    preds: Vec<Vec<Block>>,
    /// Where each value is, once placed.
    loc: Vec<Option<Src>>,
    /// Temporary components allocated.
    temps: u32,
    /// Immediate components.
    imms: Vec<u32>,
    imm_pool: BTreeMap<u32, u32>,
    imm_vectors: BTreeMap<[u32; 4], u32>,
    code: String,
    depth: usize,
    /// Declarations found by `scan`.
    attributes: Vec<u32>,
    varyings_in: Vec<u32>,
    input_regs: BTreeMap<u32, u32>,
    frag_coord: Option<u32>,
    front_face: Option<u32>,
    point_coord: Option<u32>,
    vertex_id: Option<u32>,
    instance_id: Option<u32>,
    uniform_slots: u32,
    blocks: Vec<u32>,
    samplers: BTreeMap<u32, Sampler>,
    outputs: Vec<(u32, String)>,
    output_regs: BTreeMap<OutKey, u32>,
    /// Vertex outputs the code stores, by (register, component).
    stored: Vec<(u32, u8)>,
    uses_address: bool,
    /// Whether code reads the always-zero uniform.
    uses_zero: bool,
    broadcast: bool,
}

/// An output, as the IR names it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum OutKey {
    Varying(u32),
    Color(u32),
    Position,
    PointSize,
    Depth,
}

impl<'a> Translator<'a> {
    fn new(f: &'a Func, linked: &'a Linked, options: &'a Options) -> Translator<'a> {
        Translator {
            f,
            linked,
            options,
            preds: f.preds(),
            loc: vec![None; f.values.len()],
            temps: 0,
            imms: Vec::new(),
            imm_pool: BTreeMap::new(),
            imm_vectors: BTreeMap::new(),
            code: String::new(),
            depth: 1,
            attributes: Vec::new(),
            varyings_in: Vec::new(),
            input_regs: BTreeMap::new(),
            frag_coord: None,
            front_face: None,
            point_coord: None,
            vertex_id: None,
            instance_id: None,
            uniform_slots: 0,
            blocks: Vec::new(),
            samplers: BTreeMap::new(),
            outputs: Vec::new(),
            output_regs: BTreeMap::new(),
            stored: Vec::new(),
            uses_address: false,
            uses_zero: false,
            broadcast: false,
        }
    }

    fn vertex(&self) -> bool {
        self.f.stage == Stage::Vertex
    }

    // ---- Declarations ---------------------------------------------------

    /// Finds the inputs, outputs, uniforms and samplers the code uses.
    fn scan(&mut self) {
        let mut attributes = Vec::new();
        let mut slots = Vec::new();
        let mut builtins = Vec::new();
        for b in self.f.block_order() {
            for &i in &self.f.blocks[b.0 as usize].insts {
                let inst = self.f.inst(i);
                match &inst.op {
                    InstOp::LoadInput { slot, .. } => {
                        if self.vertex() {
                            attributes.push(*slot);
                        } else {
                            slots.push(*slot);
                        }
                    }
                    InstOp::LoadBuiltin(bi) => builtins.push(*bi),
                    InstOp::LoadUniform { slot, .. } => self.uniform_slots = self.uniform_slots.max(slot + 1),
                    InstOp::LoadUniformIndexed { base, stride, count, .. } => {
                        let last = base + stride * count.saturating_sub(1);
                        self.uniform_slots = self.uniform_slots.max(last + 1);
                    }
                    InstOp::LoadBlock { block, .. } => self.blocks.push(*block),
                    InstOp::Tex(op) => {
                        for k in 0..if op.dynamic { op.count } else { 1 } {
                            self.samplers.insert(op.index + k, op.sampler);
                        }
                    }
                    InstOp::TexSize { sampler, index, dynamic, count } => {
                        for k in 0..if *dynamic { *count } else { 1 } {
                            self.samplers.insert(index + k, *sampler);
                        }
                    }
                    _ => {}
                }
            }
        }
        attributes.sort_unstable();
        attributes.dedup();
        slots.sort_unstable();
        slots.dedup();
        self.blocks.sort_unstable();
        self.blocks.dedup();
        self.attributes = attributes;
        // Fragment inputs: the varyings read, then the built-ins.
        let mut reg = 0;
        for &s in &slots {
            self.input_regs.insert(s, reg);
            reg += 1;
        }
        self.varyings_in = slots;
        for bi in builtins {
            match bi {
                BuiltinIn::FragCoord(_) if self.frag_coord.is_none() => {
                    self.frag_coord = Some(reg);
                    reg += 1;
                }
                BuiltinIn::FrontFacing if self.front_face.is_none() => {
                    self.front_face = Some(reg);
                    reg += 1;
                }
                BuiltinIn::PointCoord(_) if self.point_coord.is_none() => {
                    self.point_coord = Some(reg);
                    reg += 1;
                }
                BuiltinIn::VertexId if self.vertex_id.is_none() => {
                    self.vertex_id = Some(u32::from(self.instance_id.is_some()));
                }
                BuiltinIn::InstanceId if self.instance_id.is_none() => {
                    self.instance_id = Some(u32::from(self.vertex_id.is_some()));
                }
                _ => {}
            }
        }
        // Outputs.
        let mut out = 0;
        if self.vertex() {
            self.declare_output(OutKey::Position, &mut out, "POSITION".into());
            for s in 0..self.linked.varying_slots {
                self.declare_output(OutKey::Varying(s), &mut out, format!("GENERIC[{s}]"));
            }
            if self.linked.writes_point_size {
                self.declare_output(OutKey::PointSize, &mut out, "PSIZE".into());
            }
        } else {
            for o in &self.linked.outputs {
                self.broadcast |= o.broadcast;
                for e in 0..o.count {
                    let l = o.location + e;
                    self.declare_output(OutKey::Color(l), &mut out, format!("COLOR[{l}]"));
                }
            }
            let writes_depth = self.f.insts.iter().any(|i| i.op == InstOp::StoreBuiltin(BuiltinOut::FragDepth));
            if writes_depth {
                self.declare_output(OutKey::Depth, &mut out, "POSITION".into());
            }
        }
    }

    fn declare_output(&mut self, key: OutKey, next: &mut u32, semantic: String) {
        self.output_regs.insert(key, *next);
        self.outputs.push((*next, semantic));
        *next += 1;
    }

    // ---- Registers and immediates ---------------------------------------

    fn fresh(&mut self) -> u32 {
        self.temps += 1;
        self.temps - 1
    }

    /// Four components of one register.
    fn fresh_vec(&mut self) -> u32 {
        self.temps = self.temps.next_multiple_of(4);
        self.temps += 4;
        self.temps - 4
    }

    fn imm(&mut self, bits: u32) -> Src {
        if let Some(&c) = self.imm_pool.get(&bits) {
            return Src::Imm(c);
        }
        let c = self.imms.len() as u32;
        self.imms.push(bits);
        self.imm_pool.insert(bits, c);
        Src::Imm(c)
    }

    fn imm_f(&mut self, x: f32) -> Src {
        self.imm(x.to_bits())
    }

    /// A whole immediate register (texel offsets need three components
    /// of one register).
    fn imm_vec(&mut self, v: [u32; 4]) -> u32 {
        if let Some(&r) = self.imm_vectors.get(&v) {
            return r;
        }
        while !self.imms.len().is_multiple_of(4) {
            self.imms.push(0);
        }
        let r = self.imms.len() as u32 / 4;
        self.imms.extend_from_slice(&v);
        self.imm_vectors.insert(v, r);
        r
    }

    /// Decides where every value lives.
    fn place(&mut self) {
        for b in self.f.block_order() {
            for &i in &self.f.blocks[b.0 as usize].insts {
                let inst = self.f.inst(i);
                let results = inst.results.clone();
                match &inst.op {
                    InstOp::Const(c) => {
                        let s = self.imm(c.bits());
                        self.loc[results[0].0 as usize] = Some(s);
                    }
                    InstOp::LoadUniform { slot, comp } => {
                        self.loc[results[0].0 as usize] = Some(Src::Uniform(*slot, *comp));
                    }
                    InstOp::LoadBlock { block, offset, .. } if inst.args.is_empty() => {
                        self.loc[results[0].0 as usize] = Some(Src::Ubo(*block, offset / 16, ((offset / 4) % 4) as u8));
                    }
                    InstOp::LoadInput { slot, comp } => {
                        let r = if self.vertex() { self.attribute_reg(*slot) } else { self.input_regs[slot] };
                        self.loc[results[0].0 as usize] = Some(Src::Input(r, *comp));
                    }
                    InstOp::LoadBuiltin(bi) => {
                        let s = match bi {
                            BuiltinIn::FragCoord(c) => Src::Input(self.frag_coord.unwrap_or(0), *c),
                            BuiltinIn::PointCoord(c) => Src::Input(self.point_coord.unwrap_or(0), *c),
                            BuiltinIn::VertexId => Src::System(self.vertex_id.unwrap_or(0)),
                            BuiltinIn::InstanceId => Src::System(self.instance_id.unwrap_or(0)),
                            BuiltinIn::FrontFacing => Src::Temp(self.fresh()),
                        };
                        self.loc[results[0].0 as usize] = Some(s);
                    }
                    InstOp::Tex(_) | InstOp::TexSize { .. } => {
                        let base = self.fresh_vec();
                        for (k, r) in results.iter().enumerate() {
                            self.loc[r.0 as usize] = Some(Src::Temp(base + k as u32));
                        }
                    }
                    _ => {
                        for r in results {
                            let t = self.fresh();
                            self.loc[r.0 as usize] = Some(Src::Temp(t));
                        }
                    }
                }
            }
        }
    }

    fn at(&self, v: Value) -> Src {
        self.loc[v.0 as usize].expect("value without a place")
    }

    /// The temporary component a value was given.
    fn temp_of(&self, v: Value) -> u32 {
        match self.at(v) {
            Src::Temp(t) => t,
            other => panic!("value {v:?} is not in a temporary but {other:?}"),
        }
    }

    // ---- Emission ---------------------------------------------------------

    fn line(&mut self, s: &str) {
        for _ in 0..self.depth {
            self.code.push_str("  ");
        }
        self.code.push_str(s);
        self.code.push('\n');
    }

    fn open(&mut self, s: &str) {
        self.line(s);
        self.depth += 1;
    }

    fn close(&mut self, s: &str) {
        self.depth -= 1;
        self.line(s);
    }

    fn reopen(&mut self, s: &str) {
        self.depth -= 1;
        self.line(s);
        self.depth += 1;
    }

    fn op1(&mut self, name: &str, d: u32, a: &str) {
        let l = format!("{name} {}, {a}", dst(d));
        self.line(&l);
    }

    fn op2(&mut self, name: &str, d: u32, a: &str, b: &str) {
        let l = format!("{name} {}, {a}, {b}", dst(d));
        self.line(&l);
    }

    fn op3(&mut self, name: &str, d: u32, a: &str, b: &str, c: &str) {
        let l = format!("{name} {}, {a}, {b}, {c}", dst(d));
        self.line(&l);
    }

    fn mov(&mut self, d: u32, s: Src) {
        if s != Src::Temp(d) {
            self.copy_to(&dst(d), s);
        }
    }

    /// Copies 32 bits to a destination. A constant the host's compiler
    /// might not keep as it is (see [`float_safe`]) is built at run time,
    /// from a uniform that is always zero.
    fn copy_to(&mut self, d: &str, s: Src) {
        match s {
            Src::Imm(c) if !float_safe(self.imms[c as usize]) => {
                let z = self.zero();
                self.line(&format!("UADD {d}, {}, {}", z.text(), s.text()));
            }
            _ => self.line(&format!("MOV {d}, {}", s.text())),
        }
    }

    /// A source an instruction may read as a float without changing its
    /// bits: unsafe constants are first built at run time.
    fn opaque(&mut self, s: Src) -> Src {
        match s {
            Src::Imm(c) if !float_safe(self.imms[c as usize]) => {
                let d = self.fresh();
                self.copy_to(&dst(d), s);
                Src::Temp(d)
            }
            _ => s,
        }
    }

    /// The always-zero uniform (in the slot after the program's).
    fn zero(&mut self) -> Src {
        self.uses_zero = true;
        Src::Uniform(self.linked.slots, 0)
    }

    /// A new temporary computed by `name` from `args`.
    fn calc(&mut self, name: &str, args: &[&str]) -> Src {
        let d = self.fresh();
        match args {
            [a] => self.op1(name, d, a),
            [a, b] => self.op2(name, d, a, b),
            [a, b, c] => self.op3(name, d, a, b, c),
            _ => unreachable!(),
        }
        Src::Temp(d)
    }

    // ---- Control flow -----------------------------------------------------

    /// Emits a list; returns whether a `continue` in it may have run (so
    /// that what follows in the loop body must be skipped).
    fn list(&mut self, list: &[Node], ctx: &Ctx) -> bool {
        let mut continued = false;
        let mut guards = 0;
        for (k, n) in list.iter().enumerate() {
            match n {
                Node::Block(b) => {
                    for &i in &self.f.blocks[b.0 as usize].insts.clone() {
                        self.inst(i);
                    }
                    match self.f.blocks[b.0 as usize].jump {
                        Some(Jump::Break) => {
                            if let Some(lp) = ctx.lp {
                                self.phi_copies(*b, lp.exit);
                            }
                            self.line("BRK");
                        }
                        Some(Jump::Continue) => {
                            if let Some(lp) = ctx.lp {
                                self.phi_copies(*b, lp.header);
                                if let Some(flag) = lp.flag {
                                    let on = self.imm(!0);
                                    self.mov(flag, on);
                                }
                            }
                            continued = true;
                        }
                        Some(Jump::Discard) => self.line("KILL"),
                        None => match list.get(k + 1) {
                            Some(Node::Loop(l)) => {
                                if let Some(h) = ir::first_block(&l.body) {
                                    self.phi_copies(*b, h);
                                }
                            }
                            Some(_) => {}
                            None => match ctx.end {
                                End::Merge(m) | End::Header(m) => self.phi_copies(*b, m),
                                End::Top => {}
                            },
                        },
                    }
                    if self.f.blocks[b.0 as usize].jump.is_some() {
                        // Nothing after a jump runs.
                        break;
                    }
                }
                Node::If(i) => {
                    let merge = match list.get(k + 1) {
                        Some(Node::Block(m)) => *m,
                        _ => Block(u32::MAX),
                    };
                    let inner = Ctx { end: End::Merge(merge), lp: ctx.lp };
                    let c = self.at(i.cond).text();
                    self.open(&format!("UIF {c}"));
                    let a = self.list(&i.then, &inner);
                    self.reopen("ELSE");
                    let b = self.list(&i.otherwise, &inner);
                    self.close("ENDIF");
                    if a || b {
                        continued = true;
                        if k + 1 < list.len()
                            && let Some(flag) = ctx.lp.and_then(|lp| lp.flag)
                        {
                            // Skip the rest of this list once continued.
                            self.open(&format!("UIF {}", Src::Temp(flag).text()));
                            self.reopen("ELSE");
                            guards += 1;
                        }
                    }
                }
                Node::Loop(l) => {
                    let header = ir::first_block(&l.body).unwrap_or(Block(u32::MAX));
                    let exit = match list.get(k + 1) {
                        Some(Node::Block(m)) => *m,
                        _ => Block(u32::MAX),
                    };
                    let flag = has_continue(self.f, &l.body).then(|| self.fresh());
                    self.open("BGNLOOP");
                    if let Some(fl) = flag {
                        let off = self.imm(0);
                        self.mov(fl, off);
                    }
                    let inner = Ctx { end: End::Header(header), lp: Some(LoopCtx { header, exit, flag }) };
                    self.list(&l.body, &inner);
                    self.close("ENDLOOP");
                }
            }
        }
        for _ in 0..guards {
            self.close("ENDIF");
        }
        continued
    }

    /// Copies into the φs of `succ` for the edge from `from`, ordered so
    /// that no source is overwritten before it is read.
    fn phi_copies(&mut self, from: Block, succ: Block) {
        let Some(k) = self.preds.get(succ.0 as usize).and_then(|p| p.iter().position(|&x| x == from)) else {
            return;
        };
        let mut pending: Vec<(u32, Src)> = Vec::new();
        for &i in &self.f.blocks[succ.0 as usize].insts {
            let inst = self.f.inst(i);
            if inst.op != InstOp::Phi {
                break;
            }
            if let Some(&a) = inst.args.get(k) {
                let d = self.temp_of(inst.results[0]);
                let s = self.at(a);
                if s != Src::Temp(d) {
                    pending.push((d, s));
                }
            }
        }
        while !pending.is_empty() {
            match pending.iter().position(|&(d, _)| !pending.iter().any(|&(_, s)| s == Src::Temp(d))) {
                Some(i) => {
                    let (d, s) = pending.remove(i);
                    self.mov(d, s);
                }
                None => {
                    // A cycle: save one destination's value first.
                    let (d, _) = pending[0];
                    let t = self.fresh();
                    self.mov(t, Src::Temp(d));
                    for c in &mut pending {
                        if c.1 == Src::Temp(d) {
                            c.1 = Src::Temp(t);
                        }
                    }
                }
            }
        }
    }

    // ---- Instructions -----------------------------------------------------

    fn inst(&mut self, id: InstId) {
        let inst = self.f.inst(id).clone();
        match &inst.op {
            InstOp::Const(_) | InstOp::Phi | InstOp::Nop | InstOp::LoadUniform { .. } | InstOp::LoadInput { .. } => {}
            InstOp::LoadBlock { .. } if inst.args.is_empty() => {}
            InstOp::LoadBuiltin(BuiltinIn::FrontFacing) => {
                // TGSI's face is positive for front faces.
                let d = self.temp_of(inst.results[0]);
                let face = Src::Input(self.front_face.unwrap_or(0), 0).text();
                let zero = self.imm(0).text();
                self.op2("FSLT", d, &zero, &face);
            }
            InstOp::LoadBuiltin(_) => {}
            InstOp::Op(op) => {
                let d = self.temp_of(inst.results[0]);
                let a = self.at(inst.args[0]);
                let b = inst.args.get(1).map(|&v| self.at(v));
                self.op(*op, d, a, b);
            }
            InstOp::Select => {
                let d = self.temp_of(inst.results[0]);
                let (c, a, b) = (self.at(inst.args[0]), self.at(inst.args[1]), self.at(inst.args[2]));
                // virglrenderer selects with a float `mix`.
                let (c, a, b) = (self.opaque(c), self.opaque(a), self.opaque(b));
                self.op3("UCMP", d, &c.text(), &a.text(), &b.text());
            }
            InstOp::StoreOutput { slot, comp } => {
                let key = if self.vertex() { OutKey::Varying(*slot) } else { OutKey::Color(*slot) };
                self.store(key, *comp, inst.args[0]);
            }
            InstOp::StoreBuiltin(b) => {
                let (key, comp) = match b {
                    BuiltinOut::Position(c) => (OutKey::Position, *c),
                    BuiltinOut::PointSize => (OutKey::PointSize, 0),
                    BuiltinOut::FragDepth => (OutKey::Depth, 2),
                };
                self.store(key, comp, inst.args[0]);
            }
            InstOp::LoadUniformIndexed { base, comp, stride, count } => {
                let d = self.temp_of(inst.results[0]);
                let i = self.clamped_index(inst.args[0], *count);
                let i = if *stride == 1 {
                    i
                } else {
                    let s = self.imm(*stride).text();
                    self.calc("UMUL", &[&i.text(), &s])
                };
                self.address(i);
                self.op1("MOV", d, &format!("CONST[ADDR[0].x+{base}].{}", SWIZZLE[*comp as usize]));
            }
            InstOp::LoadBlock { block, offset, stride, count } => {
                let d = self.temp_of(inst.results[0]);
                let i = self.clamped_index(inst.args[0], *count);
                self.load_block(d, *block, *offset, *stride, i);
            }
            InstOp::Tex(op) => self.tex(op, &inst.args, &inst.results),
            InstOp::TexSize { sampler, index, dynamic, count } => {
                let (idx, lod) = if *dynamic { (Some(inst.args[0]), inst.args[1]) } else { (None, inst.args[0]) };
                self.tex_size(*sampler, *index, idx, *count, lod, &inst.results);
            }
            InstOp::Deriv { y } => {
                let d = self.temp_of(inst.results[0]);
                let a = self.at(inst.args[0]).text();
                self.op1(if *y { "DDY" } else { "DDX" }, d, &a);
            }
        }
    }

    fn store(&mut self, key: OutKey, comp: u8, v: Value) {
        let Some(&r) = self.output_regs.get(&key) else { return };
        let s = self.at(v);
        self.copy_to(&format!("OUT[{r}].{}", MASK[comp as usize]), s);
        self.stored.push((r, comp));
    }

    /// The index argument clamped to `0..count`.
    fn clamped_index(&mut self, v: Value, count: u32) -> Src {
        let i = self.at(v);
        if let Src::Imm(c) = i {
            let k = (self.imms[c as usize] as i32).clamp(0, count.max(1) as i32 - 1);
            return self.imm(k as u32);
        }
        let zero = self.imm(0).text();
        let last = self.imm(count.max(1) - 1).text();
        let lo = self.calc("IMAX", &[&i.text(), &zero]);
        self.calc("IMIN", &[&lo.text(), &last])
    }

    fn address(&mut self, i: Src) {
        self.uses_address = true;
        let l = format!("UARL ADDR[0].x, {}", i.text());
        self.line(&l);
    }

    /// `d` = the 32 bits of block `block` at `offset + i * stride` bytes.
    fn load_block(&mut self, d: u32, block: u32, offset: u32, stride: u32, i: Src) {
        let cb = block + 1;
        if stride.is_multiple_of(16) {
            let i = if stride == 16 {
                i
            } else {
                let s = self.imm(stride / 16).text();
                self.calc("UMUL", &[&i.text(), &s])
            };
            self.address(i);
            let l = format!(
                "MOV {}, CONST[{cb}][ADDR[0].x+{}].{}",
                dst(d),
                offset / 16,
                SWIZZLE[((offset / 4) % 4) as usize]
            );
            self.line(&l);
            return;
        }
        // A stride that is not a whole number of vec4s (std140 never has
        // one, but stay correct): find the word, then its component.
        let s = self.imm(stride / 4).text();
        let o = self.imm(offset / 4).text();
        let w = self.calc("UMAD", &[&i.text(), &s, &o]);
        let two = self.imm(2).text();
        let three = self.imm(3).text();
        let v = self.calc("USHR", &[&w.text(), &two]);
        let c = self.calc("AND", &[&w.text(), &three]);
        self.address(v);
        let t = self.fresh_vec();
        self.line(&format!("MOV TEMP[{}], CONST[{cb}][ADDR[0].x+0]", t / 4));
        // d = c == 0 ? x : c == 1 ? y : c == 2 ? z : w
        self.mov(d, Src::Temp(t + 3));
        for k in (0..3u32).rev() {
            let kk = self.imm(k).text();
            let eq = self.calc("USEQ", &[&c.text(), &kk]);
            self.op3("UCMP", d, &eq.text(), &Src::Temp(t + k).text(), &Src::Temp(d).text());
        }
    }

    // ---- Scalar operations --------------------------------------------

    fn op(&mut self, op: Op, d: u32, a: Src, b: Option<Src>) {
        use Op::*;
        let at = a.text();
        let b = b.unwrap_or(a);
        let bt = b.text();
        match op {
            FNeg => self.op1("MOV", d, &a.neg()),
            FAbs => self.op1("MOV", d, &a.abs()),
            FSign => self.op1("SSG", d, &at),
            FFloor => self.op1("FLR", d, &at),
            FCeil => self.op1("CEIL", d, &at),
            FTrunc => self.op1("TRUNC", d, &at),
            FRoundEven => self.op1("ROUND", d, &at),
            FFract => self.op1("FRC", d, &at),
            FSqrt => self.op1("SQRT", d, &at),
            FRsq => self.op1("RSQ", d, &at),
            FExp2 => self.op1("EX2", d, &at),
            FLog2 => self.op1("LG2", d, &at),
            FSin => self.op1("SIN", d, &at),
            FCos => self.op1("COS", d, &at),
            FExp => {
                let k = self.imm_f(LOG2_E).text();
                let t = self.calc("MUL", &[&at, &k]);
                self.op1("EX2", d, &t.text());
            }
            FLog => {
                let t = self.calc("LG2", &[&at]);
                let k = self.imm_f(LN_2).text();
                self.op2("MUL", d, &t.text(), &k);
            }
            FTan => {
                let s = self.calc("SIN", &[&at]);
                let c = self.calc("COS", &[&at]);
                self.op2("DIV", d, &s.text(), &c.text());
            }
            FAtan => {
                let one = self.imm(F_ONE);
                self.atan2(d, a, one);
            }
            FAtan2 => self.atan2(d, a, b),
            FAsin => {
                // atan2(x, sqrt((1 - x)(1 + x)))
                let s = self.sqrt_one_minus_sq(a);
                self.atan2(d, a, s);
            }
            FAcos => {
                let s = self.sqrt_one_minus_sq(a);
                self.atan2(d, s, a);
            }
            FSinh | FCosh => {
                // (e^x -+ e^-x) / 2
                let k = self.imm_f(LOG2_E).text();
                let t = self.calc("MUL", &[&at, &k]);
                let p = self.calc("EX2", &[&t.text()]);
                let n = self.calc("EX2", &[&t.neg()]);
                let s = if op == FSinh {
                    self.calc("ADD", &[&p.text(), &n.neg()])
                } else {
                    self.calc("ADD", &[&p.text(), &n.text()])
                };
                let h = self.imm(F_HALF).text();
                self.op2("MUL", d, &s.text(), &h);
            }
            FTanh => {
                // (e^2x - 1) / (e^2x + 1), with x clamped to +-10, where
                // tanh is +-1 to float precision (and e^2x stays finite).
                let lo = self.imm_f(-10.0).text();
                let hi = self.imm_f(10.0).text();
                let x = self.calc("MAX", &[&at, &lo]);
                let x = self.calc("MIN", &[&x.text(), &hi]);
                let k = self.imm_f(2.0 * LOG2_E).text();
                let t = self.calc("MUL", &[&x.text(), &k]);
                let e = self.calc("EX2", &[&t.text()]);
                let one = self.imm(F_ONE);
                let num = self.calc("ADD", &[&e.text(), &one.neg()]);
                let den = self.calc("ADD", &[&e.text(), &one.text()]);
                self.op2("DIV", d, &num.text(), &den.text());
            }
            FAsinh => {
                // sign(x) log(|x| + sqrt(x^2 + 1))
                let one = self.imm(F_ONE).text();
                let sq = self.calc("MAD", &[&at, &at, &one]);
                let r = self.calc("SQRT", &[&sq.text()]);
                let s = self.calc("ADD", &[&a.abs(), &r.text()]);
                let l = self.calc("LG2", &[&s.text()]);
                let k = self.imm_f(LN_2).text();
                let m = self.calc("MUL", &[&l.text(), &k]);
                let sg = self.calc("SSG", &[&at]);
                self.op2("MUL", d, &m.text(), &sg.text());
            }
            FAcosh => {
                // log(x + sqrt(x^2 - 1))
                let one = self.imm(F_ONE);
                let sq = self.calc("MAD", &[&at, &at, &one.neg()]);
                let r = self.calc("SQRT", &[&sq.text()]);
                let s = self.calc("ADD", &[&at, &r.text()]);
                let l = self.calc("LG2", &[&s.text()]);
                let k = self.imm_f(LN_2).text();
                self.op2("MUL", d, &l.text(), &k);
            }
            FAtanh => {
                // log((1 + x) / (1 - x)) / 2
                let one = self.imm(F_ONE).text();
                let p = self.calc("ADD", &[&one, &at]);
                let m = self.calc("ADD", &[&one, &a.neg()]);
                let q = self.calc("DIV", &[&p.text(), &m.text()]);
                let l = self.calc("LG2", &[&q.text()]);
                let k = self.imm_f(LN_2 * 0.5).text();
                self.op2("MUL", d, &l.text(), &k);
            }
            FAdd => self.op2("ADD", d, &at, &bt),
            FSub => self.op2("ADD", d, &at, &b.neg()),
            FMul => self.op2("MUL", d, &at, &bt),
            FDiv => self.op2("DIV", d, &at, &bt),
            FMin => self.op2("MIN", d, &at, &bt),
            FMax => self.op2("MAX", d, &at, &bt),
            FPow => self.op2("POW", d, &at, &bt),
            FLt => self.op2("FSLT", d, &at, &bt),
            FLe => self.op2("FSGE", d, &bt, &at),
            FEq => self.op2("FSEQ", d, &at, &bt),
            FNe => self.op2("FSNE", d, &at, &bt),
            FIsNan | FIsInf => {
                let m = self.imm(0x7FFF_FFFF).text();
                let inf = self.imm(0x7F80_0000).text();
                let t = self.calc("AND", &[&at, &m]);
                if op == FIsNan {
                    self.op2("USLT", d, &inf, &t.text());
                } else {
                    self.op2("USEQ", d, &t.text(), &inf);
                }
            }
            INeg => self.op1("INEG", d, &at),
            INot | BNot => self.op1("NOT", d, &at),
            IAbs => self.op1("IABS", d, &at),
            ISign => self.op1("ISSG", d, &at),
            IAdd => self.op2("UADD", d, &at, &bt),
            ISub => {
                let n = self.calc("INEG", &[&bt]);
                self.op2("UADD", d, &at, &n.text());
            }
            IMul => self.op2("UMUL", d, &at, &bt),
            IAnd | BAnd => self.op2("AND", d, &at, &bt),
            IOr | BOr => self.op2("OR", d, &at, &bt),
            IXor | BXor => self.op2("XOR", d, &at, &bt),
            IShl => self.op2("SHL", d, &at, &bt),
            IShr => self.op2("ISHR", d, &at, &bt),
            UShr => self.op2("USHR", d, &at, &bt),
            IDiv => self.op2("IDIV", d, &at, &bt),
            UDiv => self.op2("UDIV", d, &at, &bt),
            IRem => self.op2("MOD", d, &at, &bt),
            URem => self.op2("UMOD", d, &at, &bt),
            IMin => self.op2("IMIN", d, &at, &bt),
            UMin => self.op2("UMIN", d, &at, &bt),
            IMax => self.op2("IMAX", d, &at, &bt),
            UMax => self.op2("UMAX", d, &at, &bt),
            IEq | BEq => self.op2("USEQ", d, &at, &bt),
            INe => self.op2("USNE", d, &at, &bt),
            ILt => self.op2("ISLT", d, &at, &bt),
            ULt => self.op2("USLT", d, &at, &bt),
            ILe => self.op2("ISGE", d, &bt, &at),
            ULe => self.op2("USGE", d, &bt, &at),
            FToI => self.op1("F2I", d, &at),
            FToU => self.op1("F2U", d, &at),
            IToF => self.op1("I2F", d, &at),
            UToF => self.op1("U2F", d, &at),
            BToF => {
                let one = self.imm(F_ONE).text();
                self.op2("AND", d, &at, &one);
            }
            BToI => {
                let one = self.imm(1).text();
                self.op2("AND", d, &at, &one);
            }
            FToB => {
                let zero = self.imm(0).text();
                self.op2("FSNE", d, &at, &zero);
            }
            IToB => {
                let zero = self.imm(0).text();
                self.op2("USNE", d, &at, &zero);
            }
            Bitcast => self.op1("MOV", d, &at),
            FToHalf => self.f_to_half(d, a),
            HalfToF => self.half_to_f(d, a),
        }
    }

    /// `sqrt((1 - x)(1 + x))`.
    fn sqrt_one_minus_sq(&mut self, x: Src) -> Src {
        let one = self.imm(F_ONE).text();
        let m = self.calc("ADD", &[&one, &x.neg()]);
        let p = self.calc("ADD", &[&one, &x.text()]);
        let q = self.calc("MUL", &[&m.text(), &p.text()]);
        let zero = self.imm(0).text();
        let q = self.calc("MAX", &[&q.text(), &zero]);
        self.calc("SQRT", &[&q.text()])
    }

    /// `d = atan2(y, x)`: the angle of the smaller over the larger
    /// magnitude by polynomial, then reflected into the right octant.
    fn atan2(&mut self, d: u32, y: Src, x: Src) {
        let ax = x.abs();
        let ay = y.abs();
        let lo = self.calc("MIN", &[&ax, &ay]);
        let hi = self.calc("MAX", &[&ax, &ay]);
        // Avoid 0/0 (atan2(0, 0) is undefined; give 0).
        let tiny = self.imm_f(1.0e-30).text();
        let hi = self.calc("MAX", &[&hi.text(), &tiny]);
        let r = self.calc("DIV", &[&lo.text(), &hi.text()]);
        let r2 = self.calc("MUL", &[&r.text(), &r.text()]);
        // p = ((((c5 r2 + c4) r2 + c3) r2 + c2) r2 + c1) r2 + c0
        let c5 = self.imm_f(ATAN[5]).text();
        let c4 = self.imm_f(ATAN[4]).text();
        let mut p = self.calc("MAD", &[&c5, &r2.text(), &c4]);
        for &c in ATAN[..4].iter().rev() {
            let ct = self.imm_f(c).text();
            p = self.calc("MAD", &[&p.text(), &r2.text(), &ct]);
        }
        let t = self.calc("MUL", &[&p.text(), &r.text()]);
        // |y| > |x|: pi/2 - t.
        let half_pi = self.imm_f(PI_2).text();
        let swapped = self.calc("FSLT", &[&ax, &ay]);
        let alt = self.calc("ADD", &[&half_pi, &t.neg()]);
        let t = self.calc("UCMP", &[&swapped.text(), &alt.text(), &t.text()]);
        // x < 0: pi - t.
        let pi = self.imm_f(PI).text();
        let zero = self.imm(0).text();
        let neg = self.calc("FSLT", &[&x.text(), &zero]);
        let alt = self.calc("ADD", &[&pi, &t.neg()]);
        let t = self.calc("UCMP", &[&neg.text(), &alt.text(), &t.text()]);
        // The sign of y.
        let sign = self.imm(0x8000_0000).text();
        let s = self.calc("AND", &[&y.text(), &sign]);
        self.op2("OR", d, &t.text(), &s.text());
    }

    /// Half-precision bits (the low 16) to a float, exactly.
    fn half_to_f(&mut self, d: u32, h: Src) {
        let ht = h.text();
        let k = |t: &mut Self, v: u32| t.imm(v).text();
        let c10 = k(self, 10);
        let c13 = k(self, 13);
        let c16 = k(self, 16);
        let c23 = k(self, 23);
        let x1f = k(self, 0x1F);
        let x3ff = k(self, 0x3FF);
        let c112 = k(self, 112);
        let x8000 = k(self, 0x8000);
        let inf = k(self, 0x7F80_0000);
        let e = self.calc("USHR", &[&ht, &c10]);
        let e = self.calc("AND", &[&e.text(), &x1f]);
        let m = self.calc("AND", &[&ht, &x3ff]);
        let mshift = self.calc("SHL", &[&m.text(), &c13]);
        // Normal: ((e + 112) << 23) | (m << 13).
        let eb = self.calc("UADD", &[&e.text(), &c112]);
        let eb = self.calc("SHL", &[&eb.text(), &c23]);
        let normal = self.calc("OR", &[&eb.text(), &mshift.text()]);
        // Subnormal (or zero): m * 2^-24, exact.
        let mf = self.calc("U2F", &[&m.text()]);
        let scale = self.imm_f(1.0 / 16_777_216.0).text();
        let sub = self.calc("MUL", &[&mf.text(), &scale]);
        // Infinity or NaN.
        let special = self.calc("OR", &[&inf, &mshift.text()]);
        let zero = k(self, 0);
        let is_sub = self.calc("USEQ", &[&e.text(), &zero]);
        let is_special = self.calc("USEQ", &[&e.text(), &x1f]);
        let v = self.calc("UCMP", &[&is_sub.text(), &sub.text(), &normal.text()]);
        let v = self.calc("UCMP", &[&is_special.text(), &special.text(), &v.text()]);
        let s = self.calc("AND", &[&ht, &x8000]);
        let s = self.calc("SHL", &[&s.text(), &c16]);
        self.op2("OR", d, &v.text(), &s.text());
    }

    /// A float to half-precision bits, rounding to nearest even (as
    /// [`crate::ops::f32_to_f16`]).
    fn f_to_half(&mut self, d: u32, f: Src) {
        let ft = f.text();
        let k = |t: &mut Self, v: u32| t.imm(v).text();
        let c1 = k(self, 1);
        let c13 = k(self, 13);
        let c16 = k(self, 16);
        let c23 = k(self, 23);
        let xff = k(self, 0xFF);
        let mant_mask = k(self, 0x7F_FFFF);
        let x8000 = k(self, 0x8000);
        let sign = self.calc("USHR", &[&ft, &c16]);
        let sign = self.calc("AND", &[&sign.text(), &x8000]);
        let exp = self.calc("USHR", &[&ft, &c23]);
        let exp = self.calc("AND", &[&exp.text(), &xff]);
        let mant = self.calc("AND", &[&ft, &mant_mask]);
        // e = exp - 112 (the half's exponent field, signed).
        let m112 = k(self, (-112i32) as u32);
        let e = self.calc("UADD", &[&exp.text(), &m112]);
        // Normal halves: half = (e << 10) | (mant >> 13), rounded on the
        // 13 dropped bits.
        let c10 = k(self, 10);
        let eh = self.calc("SHL", &[&e.text(), &c10]);
        let mh = self.calc("USHR", &[&mant.text(), &c13]);
        let half_n = self.calc("OR", &[&eh.text(), &mh.text()]);
        let x1fff = k(self, 0x1FFF);
        let rem_n = self.calc("AND", &[&mant.text(), &x1fff]);
        let x1000 = k(self, 0x1000);
        let norm = self.round_even(half_n, rem_n, x1000.clone());
        // Subnormal halves: m = mant | 0x800000, shift = 14 - e (14 to 24).
        let x800000 = k(self, 0x80_0000);
        let m = self.calc("OR", &[&mant.text(), &x800000]);
        let c14 = k(self, 14);
        let ne = self.calc("INEG", &[&e.text()]);
        let shift = self.calc("UADD", &[&c14, &ne.text()]);
        let c24 = k(self, 24);
        let shift = self.calc("UMIN", &[&shift.text(), &c24]);
        let half_s = self.calc("USHR", &[&m.text(), &shift.text()]);
        let bit = self.calc("SHL", &[&c1, &shift.text()]);
        let minus1 = k(self, !0);
        let lowmask = self.calc("UADD", &[&bit.text(), &minus1]);
        let rem_s = self.calc("AND", &[&m.text(), &lowmask.text()]);
        let halfway = self.calc("USHR", &[&bit.text(), &c1]);
        let sub = self.round_even(half_s, rem_s, halfway.text());
        // Pick: NaN/infinity, overflow, normal, subnormal, underflow.
        let x7c00 = k(self, 0x7C00);
        let x200 = k(self, 0x200);
        let nan_m = self.calc("USHR", &[&mant.text(), &c13]);
        let nan_m = self.calc("OR", &[&nan_m.text(), &x200]);
        let zero = k(self, 0);
        let has_m = self.calc("USNE", &[&mant.text(), &zero]);
        let nan_m = self.calc("UCMP", &[&has_m.text(), &nan_m.text(), &zero]);
        let special = self.calc("OR", &[&x7c00, &nan_m.text()]);
        let is_special = self.calc("USEQ", &[&exp.text(), &xff]);
        let c31 = k(self, 31);
        let overflow = self.calc("ISGE", &[&e.text(), &c31]);
        let is_normal = self.calc("ISLT", &[&zero, &e.text()]);
        let m10 = k(self, (-10i32) as u32);
        let underflow = self.calc("ISLT", &[&e.text(), &m10]);
        let v = self.calc("UCMP", &[&underflow.text(), &zero, &sub.text()]);
        let v = self.calc("UCMP", &[&is_normal.text(), &norm.text(), &v.text()]);
        let v = self.calc("UCMP", &[&overflow.text(), &x7c00, &v.text()]);
        let v = self.calc("UCMP", &[&is_special.text(), &special.text(), &v.text()]);
        self.op2("OR", d, &v.text(), &sign.text());
    }

    /// `half + 1` if `rem` is above `halfway`, or at it with `half` odd.
    fn round_even(&mut self, half: Src, rem: Src, halfway: String) -> Src {
        let one = self.imm(1).text();
        let above = self.calc("USLT", &[&halfway, &rem.text()]);
        let at = self.calc("USEQ", &[&rem.text(), &halfway]);
        let zero = self.imm(0).text();
        let odd = self.calc("AND", &[&half.text(), &one]);
        let odd = self.calc("USNE", &[&odd.text(), &zero]);
        let tie = self.calc("AND", &[&at.text(), &odd.text()]);
        let up = self.calc("OR", &[&above.text(), &tie.text()]);
        let inc = self.calc("AND", &[&up.text(), &one]);
        self.calc("UADD", &[&half.text(), &inc.text()])
    }

    // ---- Textures -------------------------------------------------------

    fn tex(&mut self, op: &TexOp, args: &[Value], results: &[Value]) {
        let (index, rest) = if op.dynamic { (Some(args[0]), &args[1..]) } else { (None, args) };
        let nc = op.coords as usize;
        let coords = &rest[..nc.min(rest.len())];
        let extra = &rest[nc.min(rest.len())..];
        let s = op.sampler;
        let target = target_name(s);
        // Where the results go: they were given four aligned components.
        let base = self.temp_of(results[0]) & !3;
        // The coordinate register: coordinates, layer and reference in
        // order; the bias or level in w when it is free.
        let c = self.fresh_vec();
        for (k, &v) in coords.iter().enumerate().take(4) {
            let src = self.at(v);
            self.mov(c + k as u32, src);
        }
        let full = nc >= 4;
        let (opcode, second): (&str, Option<String>) = match op.lod {
            TexLod::Implicit if op.bias => {
                let bias = self.at(extra[0]);
                if full {
                    if self.options.shadow_lod { ("TXB2", Some(bias.text())) } else { ("TEX", None) }
                } else {
                    self.mov(c + 3, bias);
                    ("TXB", None)
                }
            }
            TexLod::Implicit => ("TEX", None),
            TexLod::Lod => {
                let lod = self.at(extra[0]);
                if full {
                    ("TXL2", Some(lod.text()))
                } else {
                    self.mov(c + 3, lod);
                    ("TXL", None)
                }
            }
            TexLod::Fetch => {
                let lod = self.at(extra[0]);
                self.mov(c + 3, lod);
                ("TXF", None)
            }
            TexLod::Grad => ("TXD", None),
        };
        let grads = if op.lod == TexLod::Grad {
            let gs = op.grad_size();
            let dx = self.fresh_vec();
            let dy = self.fresh_vec();
            for i in 0..gs {
                let a = self.at(extra[i]);
                self.mov(dx + i as u32, a);
                let b = self.at(extra[gs + i]);
                self.mov(dy + i as u32, b);
            }
            Some((dx, dy))
        } else {
            None
        };
        let offset = if op.offset != [0; 3] {
            let r =
                self.imm_vec([op.offset[0] as i32 as u32, op.offset[1] as i32 as u32, op.offset[2] as i32 as u32, 0]);
            format!(", IMM[{r}].xyz")
        } else {
            String::new()
        };
        let mask = if s.shadow { ".x" } else { "" };
        let emit = |t: &mut Self, sampler: u32| {
            let mut l = format!("{opcode} TEMP[{}]{mask}, TEMP[{}]", base / 4, c / 4);
            if let Some((dx, dy)) = grads {
                let _ = write!(l, ", TEMP[{}], TEMP[{}]", dx / 4, dy / 4);
            }
            if let Some(sec) = &second {
                let _ = write!(l, ", {sec}");
            }
            let _ = write!(l, ", SAMP[{sampler}], {target}{offset}");
            t.line(&l);
        };
        self.for_each_sampler(op.index, index, op.count, emit);
    }

    fn tex_size(&mut self, s: Sampler, first: u32, index: Option<Value>, count: u32, lod: Value, results: &[Value]) {
        let base = self.temp_of(results[0]) & !3;
        let mask = if results.len() == 3 { "xyz" } else { "xy" };
        let lod = self.at(lod).text();
        let target = target_name(s);
        let emit = |t: &mut Self, sampler: u32| {
            t.line(&format!("TXQ TEMP[{}].{mask}, {lod}, SAMP[{sampler}], {target}", base / 4));
        };
        self.for_each_sampler(first, index, count, emit);
    }

    /// Emits a lookup for a constant sampler, or for each sampler an index
    /// may select (clamped to the array), chosen by tests.
    fn for_each_sampler(&mut self, first: u32, index: Option<Value>, count: u32, emit: impl Fn(&mut Self, u32)) {
        let Some(index) = index else {
            emit(self, first);
            return;
        };
        let i = self.clamped_index(index, count);
        if let Src::Imm(c) = i {
            let k = self.imms[c as usize];
            emit(self, first + k);
            return;
        }
        let n = count.max(1);
        for k in 0..n - 1 {
            let kk = self.imm(k).text();
            let eq = self.calc("USEQ", &[&i.text(), &kk]);
            self.open(&format!("UIF {}", eq.text()));
            emit(self, first + k);
            self.reopen("ELSE");
        }
        emit(self, first + n - 1);
        for _ in 0..n - 1 {
            self.close("ENDIF");
        }
    }

    // ---- The text ---------------------------------------------------------

    fn finish(mut self) -> Shader {
        // Vertex outputs the code never writes still get a value, so that
        // the host's linker sees every varying written.
        if self.vertex() {
            let mut fill = Vec::new();
            for &(r, _) in &self.outputs {
                for c in 0..4u8 {
                    if !self.stored.contains(&(r, c)) {
                        fill.push((r, c));
                    }
                }
            }
            let zero = self.imm(0).text();
            let one = self.imm(F_ONE).text();
            let mut init = String::new();
            for (r, c) in fill {
                // Unwritten positions and sizes are (0, 0, 0, 1) and 1.
                let is_pos = self.output_regs.get(&OutKey::Position) == Some(&r);
                let is_size = self.output_regs.get(&OutKey::PointSize) == Some(&r);
                let v = if (is_pos && c == 3) || (is_size && c == 0) { &one } else { &zero };
                let _ = writeln!(init, "  MOV OUT[{r}].{}, {v}", MASK[c as usize]);
            }
            self.code.insert_str(0, &init);
        }
        let mut t = String::new();
        t.push_str(if self.vertex() { "VERT\n" } else { "FRAG\n" });
        if self.broadcast {
            t.push_str("PROPERTY FS_COLOR0_WRITES_ALL_CBUFS 1\n");
        }
        // Inputs.
        if self.vertex() {
            for r in 0..self.attributes.len() {
                let _ = writeln!(t, "DCL IN[{r}]");
            }
        } else {
            for &s in &self.varyings_in {
                let r = self.input_regs[&s];
                let interp = self
                    .linked
                    .varyings
                    .iter()
                    .find(|v| s >= v.slot && s < v.slot + v.slots)
                    .map_or(Interpolation::Smooth, |v| v.interpolation);
                let mode = match interp {
                    Interpolation::Smooth => "PERSPECTIVE",
                    Interpolation::Flat => "CONSTANT",
                    Interpolation::Centroid => "PERSPECTIVE, CENTROID",
                };
                let _ = writeln!(t, "DCL IN[{r}], GENERIC[{s}], {mode}");
            }
            if let Some(r) = self.frag_coord {
                let _ = writeln!(t, "DCL IN[{r}], POSITION, LINEAR");
            }
            if let Some(r) = self.front_face {
                let _ = writeln!(t, "DCL IN[{r}], FACE, CONSTANT");
            }
            if let Some(r) = self.point_coord {
                let _ = writeln!(t, "DCL IN[{r}], PCOORD, LINEAR");
            }
        }
        for &(r, ref sem) in &self.outputs {
            let _ = writeln!(t, "DCL OUT[{r}], {sem}");
        }
        for (&i, &s) in &self.samplers {
            let ret = match s.ty {
                Scalar::Int => "SINT",
                Scalar::Uint => "UINT",
                _ => "FLOAT",
            };
            let _ = writeln!(t, "DCL SAMP[{i}]");
            let _ = writeln!(t, "DCL SVIEW[{i}], {}, {ret}", target_name(s));
        }
        if self.uses_zero {
            self.uniform_slots = self.uniform_slots.max(self.linked.slots + 1);
        }
        if self.uniform_slots > 0 {
            let _ = writeln!(t, "DCL CONST[0..{}]", self.uniform_slots - 1);
        }
        for &b in &self.blocks {
            let size = self.linked.blocks.get(b as usize).map_or(16, |info| info.size).max(16);
            let _ = writeln!(t, "DCL CONST[{}][0..{}]", b + 1, size.div_ceil(16) - 1);
        }
        if let Some(r) = self.vertex_id {
            let _ = writeln!(t, "DCL SV[{r}], VERTEXID");
        }
        if let Some(r) = self.instance_id {
            let _ = writeln!(t, "DCL SV[{r}], INSTANCEID");
        }
        if self.temps > 0 {
            let _ = writeln!(t, "DCL TEMP[0..{}]", self.temps.div_ceil(4) - 1);
        }
        if self.uses_address {
            t.push_str("DCL ADDR[0]\n");
        }
        while !self.imms.len().is_multiple_of(4) {
            self.imms.push(0);
        }
        for (i, v) in self.imms.chunks(4).enumerate() {
            let _ = writeln!(t, "IMM[{i}] UINT32 {{{}, {}, {}, {}}}", v[0], v[1], v[2], v[3]);
        }
        t.push_str(&label_branches(&self.code));
        t.push_str("END\n");
        // A token is never shorter than a character of text.
        let tokens = t.len() as u32 + 16;
        let position = self.output_regs.get(&OutKey::Position).copied().filter(|_| self.vertex());
        let point_size = self.output_regs.get(&OutKey::PointSize).copied();
        let varyings = (0..self.linked.varying_slots)
            .map(|s| self.output_regs.get(&OutKey::Varying(s)).copied().unwrap_or(0))
            .collect();
        Shader {
            text: t,
            tokens,
            position,
            point_size,
            varyings: if self.vertex() { varyings } else { Vec::new() },
            attributes: if self.vertex() { self.attributes } else { Vec::new() },
            samplers: self.samplers.keys().copied().collect(),
            blocks: self.blocks,
        }
    }

    /// The input register of attribute location `slot` (one the shader
    /// reads): its place among them.
    fn attribute_reg(&self, slot: u32) -> u32 {
        self.attributes.binary_search(&slot).map_or(0, |r| r as u32)
    }
}

/// Whether 32 bits survive the host's compiler as a float constant: a
/// normal float, or +0. virglrenderer keeps every temporary as a float,
/// and a compiler folding constants may flush the others (small integers
/// read as floats are denormals) or canonicalise them (NaN payloads, such
/// as `true`, all ones).
/// Gives each `IF`, `UIF` and `ELSE` (one instruction a line) the
/// instruction where execution goes on when no invocation takes it: the
/// matching `ELSE` or `ENDIF`, as `:n`. TGSI's interpreter (softpipe's)
/// jumps there; translators to other languages follow the nesting instead,
/// and without a label it would jump to the first instruction.
fn label_branches(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut labels = vec![None; lines.len()];
    let mut open = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        match l.trim_start().split(' ').next().unwrap_or("") {
            "IF" | "UIF" => open.push(i),
            "ELSE" => {
                if let Some(at) = open.pop() {
                    labels[at] = Some(i);
                }
                open.push(i);
            }
            "ENDIF" => {
                if let Some(at) = open.pop() {
                    labels[at] = Some(i);
                }
            }
            _ => {}
        }
    }
    let mut out = String::with_capacity(code.len() + 8 * lines.len());
    for (l, label) in lines.iter().zip(labels) {
        out.push_str(l);
        if let Some(n) = label {
            let _ = write!(out, " :{n}");
        }
        out.push('\n');
    }
    out
}

fn float_safe(bits: u32) -> bool {
    let e = bits & 0x7F80_0000;
    bits == 0 || (e != 0 && e != 0x7F80_0000)
}

/// The TGSI texture target of a sampler type.
fn target_name(s: Sampler) -> &'static str {
    match (s.dim, s.shadow) {
        (Dim::D2, false) => "2D",
        (Dim::D2, true) => "SHADOW2D",
        (Dim::D3, _) => "3D",
        (Dim::Cube, false) => "CUBE",
        (Dim::Cube, true) => "SHADOWCUBE",
        (Dim::D2Array, false) => "2D_ARRAY",
        (Dim::D2Array, true) => "SHADOW2D_ARRAY",
    }
}

/// Whether a loop body has a `continue` of its own (not of an inner loop).
fn has_continue(f: &Func, list: &[Node]) -> bool {
    list.iter().any(|n| match n {
        Node::Block(b) => f.blocks[b.0 as usize].jump == Some(Jump::Continue),
        Node::If(i) => has_continue(f, &i.then) || has_continue(f, &i.otherwise),
        Node::Loop(_) => false,
    })
}
