//! Running bytecode.

use alloc::vec;
use alloc::vec::Vec;

use super::{ALL, Ins, LANES, MAX_ITERATIONS, Mask, Program};
use crate::ir::TexOp;
use crate::ops::{self, Op, Ty};
use vmath::f32 as m;

/// The most arguments a texture lookup has: four coordinates and two
/// three-component gradients.
const MAX_TEX_ARGS: usize = 10;

/// One register: [`LANES`] 32-bit lanes.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(C, align(64))]
pub struct Lanes(pub [u32; LANES]);

impl Lanes {
    pub const ZERO: Lanes = Lanes([0; LANES]);

    pub fn splat(bits: u32) -> Lanes {
        Lanes([bits; LANES])
    }

    pub fn from_f32(v: [f32; LANES]) -> Lanes {
        Lanes(v.map(f32::to_bits))
    }

    pub fn f32(&self, i: usize) -> f32 {
        f32::from_bits(self.0[i])
    }
}

/// Texture lookups, which the renderer provides.
pub trait Textures {
    /// Looks up a texture for the lanes in `mask`: `index` is each lane's
    /// sampler index; `args` the coordinates then the bias, level or
    /// gradients (as [`TexOp`] says); `out` receives one register per
    /// result. Fragment shaders' lanes are four 2x2 quads (top-left,
    /// top-right, bottom-left, bottom-right), from which implicit levels of
    /// detail come.
    fn sample(&self, op: &TexOp, index: &[u32; LANES], args: &[Lanes], mask: Mask, out: &mut [Lanes]);

    /// A texture's size at a level: width, height, depth or layers.
    fn size(&self, sampler: u32, lod: i32) -> [i32; 3];
}

/// Textures for programs that use none.
pub struct NoTextures;

impl Textures for NoTextures {
    fn sample(&self, _: &TexOp, _: &[u32; LANES], _: &[Lanes], _: Mask, out: &mut [Lanes]) {
        out.fill(Lanes::ZERO);
    }

    fn size(&self, _: u32, _: i32) -> [i32; 3] {
        [0; 3]
    }
}

/// What a program reads besides its inputs.
pub struct Env<'a> {
    /// Default-block uniform storage.
    pub uniforms: &'a [[u32; 4]],
    /// Each uniform block's buffer contents (by program block index).
    pub blocks: &'a [&'a [u8]],
    pub textures: &'a dyn Textures,
}

struct IfFrame {
    saved: Mask,
    else_mask: Mask,
}

struct LoopFrame {
    entry: Mask,
    brk: Mask,
    cont: Mask,
    iterations: u32,
}

/// A register file and the state to run a program on it.
pub struct Exec {
    pub regs: Vec<Lanes>,
    ifs: Vec<IfFrame>,
    loops: Vec<LoopFrame>,
    /// A loop hit [`MAX_ITERATIONS`].
    pub runaway: bool,
}

impl Exec {
    /// A register file for `p`.
    pub fn new(p: &Program) -> Exec {
        Exec { regs: vec![Lanes::ZERO; p.registers], ifs: Vec::new(), loops: Vec::new(), runaway: false }
    }

    /// Runs the prologue (once per draw).
    pub fn prologue(&mut self, p: &Program, env: &Env<'_>) {
        if self.regs.len() < p.registers {
            self.regs.resize(p.registers, Lanes::ZERO);
        }
        self.run_code(&p.prologue, p, ALL, env);
    }

    /// Copies another register file's prologue results (for workers
    /// sharing a draw).
    pub fn copy_prologue(&mut self, p: &Program, from: &Exec) {
        if self.regs.len() < p.registers {
            self.regs.resize(p.registers, Lanes::ZERO);
        }
        self.regs[..p.uniform_registers].copy_from_slice(&from.regs[..p.uniform_registers]);
    }

    /// Runs the main code for the lanes in `mask` (inputs already in their
    /// registers); returns the lanes not discarded.
    pub fn run(&mut self, p: &Program, mask: Mask, env: &Env<'_>) -> Mask {
        self.run_code(&p.main, p, mask, env)
    }

    fn run_code(&mut self, code: &[Ins], p: &Program, mask: Mask, env: &Env<'_>) -> Mask {
        self.ifs.clear();
        self.loops.clear();
        let mut exec = mask;
        let mut alive = mask;
        let mut pc = 0usize;
        while pc < code.len() {
            match code[pc] {
                Ins::If { c, else_pc } => {
                    let cm = truth(&self.regs[c as usize]) & exec;
                    self.ifs.push(IfFrame { saved: exec, else_mask: exec & !cm });
                    exec = cm;
                    if exec == 0 {
                        pc = else_pc as usize;
                        continue;
                    }
                }
                Ins::Else { end_pc } => {
                    let running = self.running(alive);
                    exec = self.ifs.last().map_or(0, |f| f.else_mask) & running;
                    if exec == 0 {
                        pc = end_pc as usize;
                        continue;
                    }
                }
                Ins::EndIf => {
                    let saved = self.ifs.pop().map_or(0, |f| f.saved);
                    exec = saved & self.running(alive);
                }
                Ins::Loop => {
                    self.loops.push(LoopFrame { entry: exec, brk: 0, cont: 0, iterations: 0 });
                }
                Ins::EndLoop { start_pc } => {
                    let Some(l) = self.loops.last_mut() else { break };
                    l.iterations += 1;
                    let next = l.entry & alive & !l.brk;
                    l.cont = 0;
                    if next != 0 && l.iterations < MAX_ITERATIONS {
                        exec = next;
                        pc = start_pc as usize + 1;
                        continue;
                    }
                    if next != 0 {
                        self.runaway = true;
                    }
                    let entry = l.entry;
                    self.loops.pop();
                    exec = entry & self.running(alive);
                }
                Ins::Break => {
                    if let Some(l) = self.loops.last_mut() {
                        l.brk |= exec;
                    }
                    exec = 0;
                }
                Ins::Continue => {
                    if let Some(l) = self.loops.last_mut() {
                        l.cont |= exec;
                    }
                    exec = 0;
                }
                Ins::Discard => {
                    alive &= !exec;
                    exec = 0;
                }
                ins => self.step(ins, p, exec, env),
            }
            pc += 1;
        }
        alive
    }

    /// The lanes still running in the current loop iteration.
    fn running(&self, alive: Mask) -> Mask {
        match self.loops.last() {
            Some(l) => alive & !(l.brk | l.cont),
            None => alive,
        }
    }

    fn step(&mut self, ins: Ins, p: &Program, exec: Mask, env: &Env<'_>) {
        match ins {
            Ins::Op { op, ty, d, a, b } => {
                let (x, y) = (self.regs[a as usize].0, self.regs[b as usize].0);
                self.regs[d as usize].0 = apply(op, ty, &x, &y);
            }
            Ins::Select { d, c, a, b } => {
                let (cv, x, y) = (self.regs[c as usize].0, self.regs[a as usize].0, self.regs[b as usize].0);
                let mut out = [0u32; LANES];
                for i in 0..LANES {
                    // Bools are all ones or zero.
                    let m = 0u32.wrapping_sub(u32::from(cv[i] != 0));
                    out[i] = (x[i] & m) | (y[i] & !m);
                }
                self.regs[d as usize].0 = out;
            }
            Ins::Const { d, bits } => self.regs[d as usize] = Lanes::splat(bits),
            Ins::Copy { d, s } => self.regs[d as usize] = self.regs[s as usize],
            Ins::CopyMasked { d, s } => {
                let (src, dst) = (self.regs[s as usize].0, self.regs[d as usize].0);
                let mut out = [0u32; LANES];
                for i in 0..LANES {
                    let m = 0u32.wrapping_sub((exec >> i) & 1);
                    out[i] = (src[i] & m) | (dst[i] & !m);
                }
                self.regs[d as usize].0 = out;
            }
            Ins::LoadUniform { d, slot, comp } => {
                let v = env.uniforms.get(slot as usize).map_or(0, |s| s[comp as usize & 3]);
                self.regs[d as usize] = Lanes::splat(v);
            }
            Ins::LoadUniformIndexed { d, base, comp, count, index } => {
                let idx = self.regs[index as usize].0;
                let mut out = [0u32; LANES];
                for i in 0..LANES {
                    // Clamped to the variable, as a signed index.
                    let k = (idx[i] as i32).clamp(0, count.max(1) as i32 - 1) as u32;
                    out[i] = env.uniforms.get((base + k) as usize).map_or(0, |s| s[comp as usize & 3]);
                }
                self.regs[d as usize].0 = out;
            }
            Ins::LoadBlock { d, block, offset, size, index } => {
                let data = env.blocks.get(block as usize).copied().unwrap_or(&[]);
                let read = |off: u32| -> u32 {
                    let o = off as usize;
                    match data.get(o..o + 4) {
                        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                        None => 0,
                    }
                };
                match index {
                    None => self.regs[d as usize] = Lanes::splat(read(offset)),
                    Some(r) => {
                        let idx = self.regs[r as usize].0;
                        let mut out = [0u32; LANES];
                        let limit = size.saturating_sub(4).saturating_sub(offset) as i64;
                        for i in 0..LANES {
                            // The byte offset, clamped inside the block.
                            let extra = (idx[i] as i32 as i64).clamp(0, limit.max(0));
                            out[i] = read(offset + extra as u32);
                        }
                        self.regs[d as usize].0 = out;
                    }
                }
            }
            Ins::Tex { op } => {
                let t = &p.tex[op as usize];
                let index = match t.index {
                    Some(r) => {
                        let v = self.regs[r as usize].0;
                        v.map(|x| (x as i32).clamp(0, t.op.count.max(1) as i32 - 1) as u32)
                    }
                    None => [t.op.index; LANES],
                };
                // Coordinates (up to four), then a bias, a level or two
                // gradients of up to three components.
                let mut args = [Lanes::ZERO; MAX_TEX_ARGS];
                let na = t.args.len().min(MAX_TEX_ARGS);
                for (a, &r) in args.iter_mut().zip(&t.args[..na]) {
                    *a = self.regs[r as usize];
                }
                let mut out = [Lanes::ZERO; 4];
                let n = t.results.len().min(4);
                env.textures.sample(&t.op, &index, &args[..na], exec, &mut out[..n]);
                for (k, &r) in t.results.iter().enumerate().take(4) {
                    self.regs[r as usize] = out[k];
                }
            }
            Ins::TexSize { op } => {
                let t = &p.tex_size[op as usize];
                let lod = self.regs[t.lod as usize].0;
                let index = t.index.map(|r| self.regs[r as usize].0);
                let mut out = [[0u32; LANES]; 3];
                for i in 0..LANES {
                    if exec & (1 << i) == 0 {
                        continue;
                    }
                    let s = index.map_or(t.sampler_index, |v| v[i]);
                    let size = env.textures.size(s, lod[i] as i32);
                    for c in 0..3 {
                        out[c][i] = size[c] as u32;
                    }
                }
                for (k, &r) in t.results.iter().enumerate().take(3) {
                    self.regs[r as usize].0 = out[k];
                }
            }
            Ins::Deriv { d, a, y } => {
                let v = self.regs[a as usize].0;
                let mut out = [0u32; LANES];
                for q in 0..LANES / 4 {
                    let b = q * 4;
                    let p = |i: usize| f32::from_bits(v[b + i]);
                    let (d0, d1) = if y {
                        // Columns: bottom minus top.
                        (p(2) - p(0), p(3) - p(1))
                    } else {
                        // Rows: right minus left.
                        (p(1) - p(0), p(3) - p(2))
                    };
                    if y {
                        out[b] = d0.to_bits();
                        out[b + 2] = d0.to_bits();
                        out[b + 1] = d1.to_bits();
                        out[b + 3] = d1.to_bits();
                    } else {
                        out[b] = d0.to_bits();
                        out[b + 1] = d0.to_bits();
                        out[b + 2] = d1.to_bits();
                        out[b + 3] = d1.to_bits();
                    }
                }
                self.regs[d as usize].0 = out;
            }
            _ => {}
        }
    }
}

/// The lanes whose bool register is true.
fn truth(r: &Lanes) -> Mask {
    let mut m = 0;
    for i in 0..LANES {
        m |= u32::from(r.0[i] != 0) << i;
    }
    m
}

type L = [u32; LANES];

#[inline(always)]
fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

#[inline(always)]
fn b(x: bool) -> u32 {
    0u32.wrapping_sub(u32::from(x))
}

#[inline(always)]
fn map1(a: &L, g: impl Fn(u32) -> u32) -> L {
    let mut d = [0u32; LANES];
    for i in 0..LANES {
        d[i] = g(a[i]);
    }
    d
}

#[inline(always)]
fn map2(a: &L, c: &L, g: impl Fn(u32, u32) -> u32) -> L {
    let mut d = [0u32; LANES];
    for i in 0..LANES {
        d[i] = g(a[i], c[i]);
    }
    d
}

#[inline(always)]
fn ff(a: &L, g: impl Fn(f32) -> f32) -> L {
    map1(a, |x| g(f(x)).to_bits())
}

#[inline(always)]
fn fff(a: &L, c: &L, g: impl Fn(f32, f32) -> f32) -> L {
    map2(a, c, |x, y| g(f(x), f(y)).to_bits())
}

/// `op` on every lane, exactly as [`ops::eval`] defines it.
pub(crate) fn apply(op: Op, ty: Ty, a: &L, c: &L) -> L {
    use Op::*;
    match op {
        FNeg => map1(a, |x| x ^ 0x8000_0000),
        FAbs => map1(a, |x| x & 0x7FFF_FFFF),
        FSign => ff(a, |x| {
            if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                x
            }
        }),
        FFloor => ff(a, m::floor),
        FCeil => ff(a, m::ceil),
        FTrunc => ff(a, m::trunc),
        FRoundEven => ff(a, m::round_ties_even),
        FFract => ff(a, |x| x - m::floor(x)),
        FSqrt => ff(a, m::sqrt),
        FRsq => ff(a, |x| 1.0 / m::sqrt(x)),
        FExp => ff(a, m::exp),
        FLog => ff(a, m::ln),
        FExp2 => ff(a, m::exp2),
        FLog2 => ff(a, m::log2),
        FSin => ff(a, m::sin),
        FCos => ff(a, m::cos),
        FTan => ff(a, m::tan),
        FAsin => ff(a, m::asin),
        FAcos => ff(a, m::acos),
        FAtan => ff(a, m::atan),
        FSinh => ff(a, m::sinh),
        FCosh => ff(a, m::cosh),
        FTanh => ff(a, m::tanh),
        FAsinh => ff(a, m::asinh),
        FAcosh => ff(a, m::acosh),
        FAtanh => ff(a, m::atanh),
        FAdd => fff(a, c, |x, y| x + y),
        FSub => fff(a, c, |x, y| x - y),
        FMul => fff(a, c, |x, y| x * y),
        FDiv => fff(a, c, |x, y| x / y),
        FMin => fff(a, c, |x, y| if x < y { x } else { y }),
        FMax => fff(a, c, |x, y| if x > y { x } else { y }),
        FPow => fff(a, c, m::powf),
        FAtan2 => fff(a, c, m::atan2),
        FLt => map2(a, c, |x, y| b(f(x) < f(y))),
        FLe => map2(a, c, |x, y| b(f(x) <= f(y))),
        FEq => map2(a, c, |x, y| b(f(x) == f(y))),
        FNe => map2(a, c, |x, y| b(f(x) != f(y))),
        FIsNan => map1(a, |x| b(f(x).is_nan())),
        FIsInf => map1(a, |x| b(f(x).is_infinite())),
        INeg => map1(a, |x| (x as i32).wrapping_neg() as u32),
        INot => map1(a, |x| !x),
        IAbs => map1(a, |x| (x as i32).wrapping_abs() as u32),
        ISign => map1(a, |x| (x as i32).signum() as u32),
        IAdd => map2(a, c, u32::wrapping_add),
        ISub => map2(a, c, u32::wrapping_sub),
        IMul => map2(a, c, u32::wrapping_mul),
        IAnd => map2(a, c, |x, y| x & y),
        IOr => map2(a, c, |x, y| x | y),
        IXor => map2(a, c, |x, y| x ^ y),
        IShl => map2(a, c, |x, y| x.wrapping_shl(y & 31)),
        IDiv => map2(a, c, |x, y| if y == 0 { !0 } else { (x as i32).wrapping_div(y as i32) as u32 }),
        UDiv => map2(a, c, |x, y| x.checked_div(y).unwrap_or(!0)),
        IRem => map2(a, c, |x, y| if y == 0 { !0 } else { (x as i32).wrapping_rem(y as i32) as u32 }),
        URem => map2(a, c, |x, y| x.checked_rem(y).unwrap_or(!0)),
        IShr => map2(a, c, |x, y| ((x as i32) >> (y & 31)) as u32),
        UShr => map2(a, c, |x, y| x >> (y & 31)),
        IMin => map2(a, c, |x, y| (x as i32).min(y as i32) as u32),
        UMin => map2(a, c, u32::min),
        IMax => map2(a, c, |x, y| (x as i32).max(y as i32) as u32),
        UMax => map2(a, c, u32::max),
        IEq => map2(a, c, |x, y| b(x == y)),
        INe => map2(a, c, |x, y| b(x != y)),
        ILt => map2(a, c, |x, y| b((x as i32) < (y as i32))),
        ULt => map2(a, c, |x, y| b(x < y)),
        ILe => map2(a, c, |x, y| b((x as i32) <= (y as i32))),
        ULe => map2(a, c, |x, y| b(x <= y)),
        BNot => map1(a, |x| b(x == 0)),
        BAnd => map2(a, c, |x, y| b(x != 0 && y != 0)),
        BOr => map2(a, c, |x, y| b(x != 0 || y != 0)),
        BXor => map2(a, c, |x, y| b((x != 0) != (y != 0))),
        BEq => map2(a, c, |x, y| b((x != 0) == (y != 0))),
        FToI => map1(a, |x| ops::f_to_i(f(x)) as u32),
        FToU => map1(a, |x| ops::f_to_u(f(x))),
        IToF => map1(a, |x| ((x as i32) as f32).to_bits()),
        UToF => map1(a, |x| (x as f32).to_bits()),
        BToF => map1(a, |x| if x != 0 { 1.0f32.to_bits() } else { 0 }),
        BToI => map1(a, |x| u32::from(x != 0)),
        FToB => map1(a, |x| b(f(x) != 0.0)),
        IToB => map1(a, |x| b(x != 0)),
        Bitcast => {
            // A bool result normalises (anything non-zero is true).
            if ty == Ty::Bool { map1(a, |x| b(x != 0)) } else { *a }
        }
        FToHalf => map1(a, |x| u32::from(ops::f32_to_f16(f(x)))),
        HalfToF => map1(a, |x| ops::f16_to_f32(x as u16).to_bits()),
    }
}
