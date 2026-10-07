//! Operators, constructors and built-in functions as scalar operations.
//!
//! Every value is handled as its scalar components, flattened in GL
//! order (matrix columns, then array elements and structure members). The
//! functions here are written once against [`Emit`], which has two
//! implementations: [`ConstEmit`], which computes immediately and so folds
//! constant expressions, and the SSA IR builder, which emits instructions.
//! The constant folder and the code a shader runs therefore apply the very
//! same formulas (GLSL ES 3.00 chapter 8 gives most of them).

use alloc::vec;
use alloc::vec::Vec;

use crate::builtins::Builtin;
use crate::hir::{BinOp, UnOp};
use crate::ops::{self, Op, Ty};
use crate::types::{Basic, Element, Scalar, Structs, Type};

/// Where scalar operations go.
pub trait Emit {
    type V: Copy;
    /// A constant.
    fn konst(&mut self, v: ops::Value) -> Self::V;
    /// An operation; `ret` is the result type.
    fn op(&mut self, op: Op, args: &[Self::V], ret: Ty) -> Self::V;
    /// `c ? a : b` on scalars of type `ty`.
    fn select(&mut self, c: Self::V, a: Self::V, b: Self::V, ty: Ty) -> Self::V;
}

/// Computes immediately: folds constants.
pub struct ConstEmit;

impl Emit for ConstEmit {
    type V = ops::Value;

    fn konst(&mut self, v: ops::Value) -> ops::Value {
        v
    }

    fn op(&mut self, op: Op, args: &[ops::Value], ret: Ty) -> ops::Value {
        ops::eval(op, args, ret)
    }

    fn select(&mut self, c: ops::Value, a: ops::Value, b: ops::Value, _ty: Ty) -> ops::Value {
        if c.bits() != 0 { a } else { b }
    }
}

/// The scalar type of a GLSL scalar type.
pub fn ty(s: Scalar) -> Ty {
    match s {
        Scalar::Float => Ty::F32,
        Scalar::Int => Ty::I32,
        Scalar::Uint => Ty::U32,
        Scalar::Bool => Ty::Bool,
    }
}

/// The scalar types of a value's flattened components.
pub fn component_types(t: Type, structs: &Structs, out: &mut Vec<Ty>) {
    let n = t.array.unwrap_or(1);
    for _ in 0..n {
        match t.element {
            Element::Basic(b) => {
                if let Some(s) = b.scalar() {
                    for _ in 0..b.components() {
                        out.push(ty(s));
                    }
                } else if b.is_sampler() {
                    // A sampler is a texture unit number.
                    out.push(Ty::I32);
                }
            }
            Element::Struct(id) => {
                for f in &structs.get(id).fields {
                    component_types(f.ty, structs, out);
                }
            }
        }
    }
}

fn f<E: Emit>(e: &mut E, x: f32) -> E::V {
    e.konst(ops::Value::F(x))
}

/// Converts one scalar between GLSL scalar types (constructor rules).
pub fn convert<E: Emit>(e: &mut E, v: E::V, from: Scalar, to: Scalar) -> E::V {
    use Scalar::*;
    match (from, to) {
        (a, b) if a == b => v,
        (Int, Float) => e.op(Op::IToF, &[v], Ty::F32),
        (Uint, Float) => e.op(Op::UToF, &[v], Ty::F32),
        (Bool, Float) => e.op(Op::BToF, &[v], Ty::F32),
        (Float, Int) => e.op(Op::FToI, &[v], Ty::I32),
        (Uint, Int) => e.op(Op::Bitcast, &[v], Ty::I32),
        (Bool, Int) => e.op(Op::BToI, &[v], Ty::I32),
        (Float, Uint) => e.op(Op::FToU, &[v], Ty::U32),
        (Int, Uint) => e.op(Op::Bitcast, &[v], Ty::U32),
        (Bool, Uint) => e.op(Op::BToI, &[v], Ty::U32),
        (Float, Bool) => e.op(Op::FToB, &[v], Ty::Bool),
        (Int | Uint, Bool) => e.op(Op::IToB, &[v], Ty::Bool),
        _ => v,
    }
}

/// Operand `i` of a value broadcast to `n` components (a scalar repeats).
fn at<V: Copy>(v: &[V], i: usize) -> V {
    if v.len() == 1 { v[0] } else { v[i] }
}

/// The arithmetic operation for `op` on scalars of `s`.
fn arith_op(op: BinOp, s: Scalar) -> Op {
    use BinOp::*;
    match (op, s) {
        (Add, Scalar::Float) => Op::FAdd,
        (Sub, Scalar::Float) => Op::FSub,
        (Mul, Scalar::Float) => Op::FMul,
        (Div, Scalar::Float) => Op::FDiv,
        (Add, _) => Op::IAdd,
        (Sub, _) => Op::ISub,
        (Mul, _) => Op::IMul,
        (Div, Scalar::Uint) => Op::UDiv,
        (Div, _) => Op::IDiv,
        (Mod, Scalar::Uint) => Op::URem,
        (Mod, _) => Op::IRem,
        (BitAnd, _) => Op::IAnd,
        (BitOr, _) => Op::IOr,
        (BitXor, _) => Op::IXor,
        (Shl, _) => Op::IShl,
        (Shr, Scalar::Uint) => Op::UShr,
        (Shr, _) => Op::IShr,
        _ => Op::IAdd,
    }
}

/// `l op r` for the arithmetic, bitwise and shift operators, with
/// broadcasting and the linear algebra products.
pub fn binary<E: Emit>(
    e: &mut E,
    op: BinOp,
    lt: Type,
    l: &[E::V],
    rt: Type,
    r: &[E::V],
    structs: &Structs,
) -> Vec<E::V> {
    let lb = lt.as_basic();
    let rb = rt.as_basic();
    match op {
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            let s = lb.and_then(Basic::scalar).unwrap_or(Scalar::Float);
            vec![compare(e, op, s, l[0], r[0])]
        }
        BinOp::Eq | BinOp::Ne => vec![equal(e, op == BinOp::Ne, lt, structs, l, r)],
        BinOp::Xor => vec![e.op(Op::BXor, &[l[0], r[0]], Ty::Bool)],
        BinOp::Mul => match (lb, rb) {
            (Some(Basic::Matrix(c, rows)), Some(Basic::Vector(_, _))) => mat_vec(e, c, rows, l, r),
            (Some(Basic::Vector(_, _)), Some(Basic::Matrix(c, rows))) => vec_mat(e, c, rows, l, r),
            (Some(Basic::Matrix(k, rows)), Some(Basic::Matrix(c, _))) => mat_mat(e, k, rows, c, l, r),
            _ => componentwise(e, op, lt, l, rt, r),
        },
        _ => componentwise(e, op, lt, l, rt, r),
    }
}

fn componentwise<E: Emit>(e: &mut E, op: BinOp, lt: Type, l: &[E::V], rt: Type, r: &[E::V]) -> Vec<E::V> {
    // Shifts take their type from the left operand; the right operand may
    // be signed or unsigned (the shift amount is the same bits).
    let _ = rt;
    let ls = lt.as_basic().and_then(Basic::scalar).unwrap_or(Scalar::Float);
    let o = arith_op(op, ls);
    let ret = ty(ls);
    let n = l.len().max(r.len());
    (0..n).map(|i| e.op(o, &[at(l, i), at(r, i)], ret)).collect()
}

fn compare<E: Emit>(e: &mut E, op: BinOp, s: Scalar, a: E::V, b: E::V) -> E::V {
    // a > b is b < a; a >= b is b <= a.
    let (lt, le) = match s {
        Scalar::Float => (Op::FLt, Op::FLe),
        Scalar::Uint => (Op::ULt, Op::ULe),
        _ => (Op::ILt, Op::ILe),
    };
    match op {
        BinOp::Lt => e.op(lt, &[a, b], Ty::Bool),
        BinOp::Le => e.op(le, &[a, b], Ty::Bool),
        BinOp::Gt => e.op(lt, &[b, a], Ty::Bool),
        _ => e.op(le, &[b, a], Ty::Bool),
    }
}

/// Whole-value equality (or inequality) of two values of type `t`: every
/// component of every element and member compares equal.
pub fn equal<E: Emit>(e: &mut E, not: bool, t: Type, structs: &Structs, l: &[E::V], r: &[E::V]) -> E::V {
    let mut types = Vec::new();
    component_types(t, structs, &mut types);
    equal_typed(e, not, &types, l, r)
}

/// Equality of flattened values with the given component types.
pub fn equal_typed<E: Emit>(e: &mut E, not: bool, types: &[Ty], l: &[E::V], r: &[E::V]) -> E::V {
    let mut acc: Option<E::V> = None;
    for i in 0..l.len().min(r.len()) {
        let t = types.get(i).copied().unwrap_or(Ty::F32);
        let c = match (t, not) {
            (Ty::F32, false) => e.op(Op::FEq, &[l[i], r[i]], Ty::Bool),
            (Ty::F32, true) => e.op(Op::FNe, &[l[i], r[i]], Ty::Bool),
            (Ty::Bool, false) => e.op(Op::BEq, &[l[i], r[i]], Ty::Bool),
            (Ty::Bool, true) => e.op(Op::BXor, &[l[i], r[i]], Ty::Bool),
            (_, false) => e.op(Op::IEq, &[l[i], r[i]], Ty::Bool),
            (_, true) => e.op(Op::INe, &[l[i], r[i]], Ty::Bool),
        };
        acc = Some(match acc {
            None => c,
            Some(a) => e.op(if not { Op::BOr } else { Op::BAnd }, &[a, c], Ty::Bool),
        });
    }
    acc.unwrap_or_else(|| e.konst(ops::Value::B(!not)))
}

fn mat_vec<E: Emit>(e: &mut E, cols: u8, rows: u8, m: &[E::V], v: &[E::V]) -> Vec<E::V> {
    let (cols, rows) = (cols as usize, rows as usize);
    (0..rows)
        .map(|r| {
            let mut acc = e.op(Op::FMul, &[m[r], v[0]], Ty::F32);
            for c in 1..cols {
                let p = e.op(Op::FMul, &[m[c * rows + r], v[c]], Ty::F32);
                acc = e.op(Op::FAdd, &[acc, p], Ty::F32);
            }
            acc
        })
        .collect()
}

fn vec_mat<E: Emit>(e: &mut E, cols: u8, rows: u8, v: &[E::V], m: &[E::V]) -> Vec<E::V> {
    let (cols, rows) = (cols as usize, rows as usize);
    (0..cols)
        .map(|c| {
            let mut acc = e.op(Op::FMul, &[v[0], m[c * rows]], Ty::F32);
            for r in 1..rows {
                let p = e.op(Op::FMul, &[v[r], m[c * rows + r]], Ty::F32);
                acc = e.op(Op::FAdd, &[acc, p], Ty::F32);
            }
            acc
        })
        .collect()
}

/// `a` (k columns of `rows`) times `b` (`cols` columns of k rows).
fn mat_mat<E: Emit>(e: &mut E, k: u8, rows: u8, cols: u8, a: &[E::V], b: &[E::V]) -> Vec<E::V> {
    let (k, rows, cols) = (k as usize, rows as usize, cols as usize);
    let mut out = Vec::with_capacity(cols * rows);
    for c in 0..cols {
        for r in 0..rows {
            let mut acc = e.op(Op::FMul, &[a[r], b[c * k]], Ty::F32);
            for i in 1..k {
                let p = e.op(Op::FMul, &[a[i * rows + r], b[c * k + i]], Ty::F32);
                acc = e.op(Op::FAdd, &[acc, p], Ty::F32);
            }
            out.push(acc);
        }
    }
    out
}

/// A unary operator on each component.
pub fn unary<E: Emit>(e: &mut E, op: UnOp, t: Type, a: &[E::V]) -> Vec<E::V> {
    let s = t.as_basic().and_then(Basic::scalar).unwrap_or(Scalar::Float);
    let (o, r) = match (op, s) {
        (UnOp::Neg, Scalar::Float) => (Op::FNeg, Ty::F32),
        (UnOp::Neg, _) => (Op::INeg, ty(s)),
        (UnOp::Not, _) => (Op::BNot, Ty::Bool),
        (UnOp::BitNot, _) => (Op::INot, ty(s)),
    };
    a.iter().map(|&v| e.op(o, &[v], r)).collect()
}

/// A constructor of `target` from arguments (type, flattened components).
/// The semantic checks have validated the arguments.
pub fn construct<E: Emit>(e: &mut E, target: Type, args: &[(Type, Vec<E::V>)], structs: &Structs) -> Vec<E::V> {
    // Arrays and structures: the arguments are the elements or members.
    if target.is_array() || target.as_struct().is_some() {
        let _ = structs;
        return args.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    }
    let Some(b) = target.as_basic() else { return Vec::new() };
    let Some(to) = b.scalar() else { return Vec::new() };
    let arg_scalar = |t: Type| t.as_basic().and_then(Basic::scalar).unwrap_or(Scalar::Float);
    match b {
        Basic::Scalar(_) => {
            let (t, v) = &args[0];
            vec![convert(e, v[0], arg_scalar(*t), to)]
        }
        Basic::Vector(_, n) => {
            if args.len() == 1 && args[0].1.len() == 1 {
                let (t, v) = &args[0];
                let c = convert(e, v[0], arg_scalar(*t), to);
                return vec![c; n as usize];
            }
            let mut out = Vec::with_capacity(n as usize);
            'args: for (t, v) in args {
                for &c in v {
                    if out.len() == n as usize {
                        break 'args;
                    }
                    out.push(convert(e, c, arg_scalar(*t), to));
                }
            }
            out
        }
        Basic::Matrix(cols, rows) => {
            let (cols, rows) = (cols as usize, rows as usize);
            let zero = f(e, 0.0);
            let one = f(e, 1.0);
            if args.len() == 1 {
                let (t, v) = &args[0];
                if let Some(Basic::Matrix(sc, sr)) = t.as_basic() {
                    // From a matrix: the overlap, then the identity.
                    let (sc, sr) = (sc as usize, sr as usize);
                    let mut out = Vec::with_capacity(cols * rows);
                    for c in 0..cols {
                        for r in 0..rows {
                            out.push(if c < sc && r < sr {
                                v[c * sr + r]
                            } else if c == r {
                                one
                            } else {
                                zero
                            });
                        }
                    }
                    return out;
                }
                if v.len() == 1 {
                    // From a scalar: the diagonal.
                    let d = convert(e, v[0], arg_scalar(*t), Scalar::Float);
                    let mut out = Vec::with_capacity(cols * rows);
                    for c in 0..cols {
                        for r in 0..rows {
                            out.push(if c == r { d } else { zero });
                        }
                    }
                    return out;
                }
            }
            let mut out = Vec::with_capacity(cols * rows);
            'margs: for (t, v) in args {
                for &c in v {
                    if out.len() == cols * rows {
                        break 'margs;
                    }
                    out.push(convert(e, c, arg_scalar(*t), Scalar::Float));
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Applies a one-operand float operation to each component.
fn each<E: Emit>(e: &mut E, op: Op, a: &[E::V]) -> Vec<E::V> {
    a.iter().map(|&v| e.op(op, &[v], Ty::F32)).collect()
}

fn dot<E: Emit>(e: &mut E, a: &[E::V], b: &[E::V]) -> E::V {
    let mut acc = e.op(Op::FMul, &[a[0], b[0]], Ty::F32);
    for i in 1..a.len().min(b.len()) {
        let p = e.op(Op::FMul, &[a[i], b[i]], Ty::F32);
        acc = e.op(Op::FAdd, &[acc, p], Ty::F32);
    }
    acc
}

/// The components of the result of a built-in function (not a texture
/// lookup or derivative), from its arguments; for `modf`, also the value
/// of its `out` argument.
pub fn builtin<E: Emit>(
    e: &mut E,
    b: Builtin,
    args: &[(Type, Vec<E::V>)],
    ret: Type,
) -> (Vec<E::V>, Option<Vec<E::V>>) {
    use Builtin::*;
    let a = |i: usize| -> &[E::V] { &args[i].1 };
    let scalar_of = |i: usize| args[i].0.as_basic().and_then(Basic::scalar).unwrap_or(Scalar::Float);
    let n = ret.as_basic().map_or(0, Basic::components) as usize;
    let r = match b {
        Radians => {
            let k = f(e, core::f32::consts::PI / 180.0);
            a(0).iter().map(|&x| e.op(Op::FMul, &[x, k], Ty::F32)).collect()
        }
        Degrees => {
            let k = f(e, 180.0 / core::f32::consts::PI);
            a(0).iter().map(|&x| e.op(Op::FMul, &[x, k], Ty::F32)).collect()
        }
        Sin => each(e, Op::FSin, a(0)),
        Cos => each(e, Op::FCos, a(0)),
        Tan => each(e, Op::FTan, a(0)),
        Asin => each(e, Op::FAsin, a(0)),
        Acos => each(e, Op::FAcos, a(0)),
        Atan if args.len() == 2 => (0..n).map(|i| e.op(Op::FAtan2, &[a(0)[i], a(1)[i]], Ty::F32)).collect(),
        Atan => each(e, Op::FAtan, a(0)),
        Sinh => each(e, Op::FSinh, a(0)),
        Cosh => each(e, Op::FCosh, a(0)),
        Tanh => each(e, Op::FTanh, a(0)),
        Asinh => each(e, Op::FAsinh, a(0)),
        Acosh => each(e, Op::FAcosh, a(0)),
        Atanh => each(e, Op::FAtanh, a(0)),
        Pow => (0..n).map(|i| e.op(Op::FPow, &[a(0)[i], a(1)[i]], Ty::F32)).collect(),
        Exp => each(e, Op::FExp, a(0)),
        Log => each(e, Op::FLog, a(0)),
        Exp2 => each(e, Op::FExp2, a(0)),
        Log2 => each(e, Op::FLog2, a(0)),
        Sqrt => each(e, Op::FSqrt, a(0)),
        InverseSqrt => each(e, Op::FRsq, a(0)),
        Abs | Sign => {
            let s = scalar_of(0);
            let op = match (b, s) {
                (Abs, Scalar::Float) => Op::FAbs,
                (Abs, _) => Op::IAbs,
                (_, Scalar::Float) => Op::FSign,
                _ => Op::ISign,
            };
            a(0).iter().map(|&x| e.op(op, &[x], ty(s))).collect()
        }
        Floor => each(e, Op::FFloor, a(0)),
        Trunc => each(e, Op::FTrunc, a(0)),
        Round | RoundEven => each(e, Op::FRoundEven, a(0)),
        Ceil => each(e, Op::FCeil, a(0)),
        Fract => each(e, Op::FFract, a(0)),
        Mod => (0..n)
            .map(|i| {
                // x - y * floor(x / y)
                let (x, y) = (a(0)[i], at(a(1), i));
                let q = e.op(Op::FDiv, &[x, y], Ty::F32);
                let fl = e.op(Op::FFloor, &[q], Ty::F32);
                let p = e.op(Op::FMul, &[y, fl], Ty::F32);
                e.op(Op::FSub, &[x, p], Ty::F32)
            })
            .collect(),
        Modf => {
            let whole = each(e, Op::FTrunc, a(0));
            let frac = (0..n).map(|i| e.op(Op::FSub, &[a(0)[i], whole[i]], Ty::F32)).collect();
            return (frac, Some(whole));
        }
        Min | Max => {
            let s = scalar_of(0);
            let op = match (b, s) {
                (Min, Scalar::Float) => Op::FMin,
                (Min, Scalar::Uint) => Op::UMin,
                (Min, _) => Op::IMin,
                (_, Scalar::Float) => Op::FMax,
                (_, Scalar::Uint) => Op::UMax,
                _ => Op::IMax,
            };
            (0..n).map(|i| e.op(op, &[a(0)[i], at(a(1), i)], ty(s))).collect()
        }
        Clamp => {
            let s = scalar_of(0);
            let (min, max) = match s {
                Scalar::Float => (Op::FMin, Op::FMax),
                Scalar::Uint => (Op::UMin, Op::UMax),
                _ => (Op::IMin, Op::IMax),
            };
            (0..n)
                .map(|i| {
                    let lo = e.op(max, &[a(0)[i], at(a(1), i)], ty(s));
                    e.op(min, &[lo, at(a(2), i)], ty(s))
                })
                .collect()
        }
        Mix => {
            if scalar_of(2) == Scalar::Bool {
                (0..n).map(|i| e.select(a(2)[i], a(1)[i], a(0)[i], Ty::F32)).collect()
            } else {
                let one = f(e, 1.0);
                (0..n)
                    .map(|i| {
                        // x * (1 - a) + y * a
                        let t = at(a(2), i);
                        let inv = e.op(Op::FSub, &[one, t], Ty::F32);
                        let p = e.op(Op::FMul, &[a(0)[i], inv], Ty::F32);
                        let q = e.op(Op::FMul, &[a(1)[i], t], Ty::F32);
                        e.op(Op::FAdd, &[p, q], Ty::F32)
                    })
                    .collect()
            }
        }
        Step => {
            let (zero, one) = (f(e, 0.0), f(e, 1.0));
            (0..n)
                .map(|i| {
                    let lt = e.op(Op::FLt, &[a(1)[i], at(a(0), i)], Ty::Bool);
                    e.select(lt, zero, one, Ty::F32)
                })
                .collect()
        }
        Smoothstep => {
            let (zero, one, two, three) = (f(e, 0.0), f(e, 1.0), f(e, 2.0), f(e, 3.0));
            (0..n)
                .map(|i| {
                    // t = clamp((x - e0) / (e1 - e0), 0, 1); t * t * (3 - 2 t)
                    let (e0, e1, x) = (at(a(0), i), at(a(1), i), a(2)[i]);
                    let num = e.op(Op::FSub, &[x, e0], Ty::F32);
                    let den = e.op(Op::FSub, &[e1, e0], Ty::F32);
                    let q = e.op(Op::FDiv, &[num, den], Ty::F32);
                    let lo = e.op(Op::FMax, &[q, zero], Ty::F32);
                    let t = e.op(Op::FMin, &[lo, one], Ty::F32);
                    let tt = e.op(Op::FMul, &[t, t], Ty::F32);
                    let t2 = e.op(Op::FMul, &[two, t], Ty::F32);
                    let k = e.op(Op::FSub, &[three, t2], Ty::F32);
                    e.op(Op::FMul, &[tt, k], Ty::F32)
                })
                .collect()
        }
        IsNan => a(0).iter().map(|&x| e.op(Op::FIsNan, &[x], Ty::Bool)).collect(),
        IsInf => a(0).iter().map(|&x| e.op(Op::FIsInf, &[x], Ty::Bool)).collect(),
        FloatBitsToInt => a(0).iter().map(|&x| e.op(Op::Bitcast, &[x], Ty::I32)).collect(),
        FloatBitsToUint => a(0).iter().map(|&x| e.op(Op::Bitcast, &[x], Ty::U32)).collect(),
        IntBitsToFloat | UintBitsToFloat => a(0).iter().map(|&x| e.op(Op::Bitcast, &[x], Ty::F32)).collect(),
        PackSnorm2x16 | PackUnorm2x16 => {
            let (lo, hi, scale) = if b == PackSnorm2x16 { (-1.0, 1.0, 32767.0) } else { (0.0, 1.0, 65535.0) };
            let (lo, hi, scale) = (f(e, lo), f(e, hi), f(e, scale));
            let mask = e.konst(ops::Value::U(0xFFFF));
            let sixteen = e.konst(ops::Value::U(16));
            let mut halves = [None, None];
            for (i, h) in halves.iter_mut().enumerate() {
                let c = e.op(Op::FMax, &[a(0)[i], lo], Ty::F32);
                let c = e.op(Op::FMin, &[c, hi], Ty::F32);
                let c = e.op(Op::FMul, &[c, scale], Ty::F32);
                let c = e.op(Op::FRoundEven, &[c], Ty::F32);
                let c = e.op(Op::FToI, &[c], Ty::I32);
                let c = e.op(Op::Bitcast, &[c], Ty::U32);
                *h = Some(e.op(Op::IAnd, &[c, mask], Ty::U32));
            }
            let (x, y) = (halves[0].unwrap_or(mask), halves[1].unwrap_or(mask));
            let y = e.op(Op::IShl, &[y, sixteen], Ty::U32);
            vec![e.op(Op::IOr, &[x, y], Ty::U32)]
        }
        UnpackSnorm2x16 => {
            let p = e.op(Op::Bitcast, &[a(0)[0]], Ty::I32);
            let sixteen = e.konst(ops::Value::I(16));
            let lo = e.op(Op::IShl, &[p, sixteen], Ty::I32);
            let lo = e.op(Op::IShr, &[lo, sixteen], Ty::I32);
            let hi = e.op(Op::IShr, &[p, sixteen], Ty::I32);
            let (scale, min, max) = (f(e, 1.0 / 32767.0), f(e, -1.0), f(e, 1.0));
            [lo, hi]
                .into_iter()
                .map(|h| {
                    let x = e.op(Op::IToF, &[h], Ty::F32);
                    let x = e.op(Op::FMul, &[x, scale], Ty::F32);
                    let x = e.op(Op::FMax, &[x, min], Ty::F32);
                    e.op(Op::FMin, &[x, max], Ty::F32)
                })
                .collect()
        }
        UnpackUnorm2x16 => {
            let p = a(0)[0];
            let mask = e.konst(ops::Value::U(0xFFFF));
            let sixteen = e.konst(ops::Value::U(16));
            let lo = e.op(Op::IAnd, &[p, mask], Ty::U32);
            let hi = e.op(Op::UShr, &[p, sixteen], Ty::U32);
            let scale = f(e, 1.0 / 65535.0);
            [lo, hi]
                .into_iter()
                .map(|h| {
                    let x = e.op(Op::UToF, &[h], Ty::F32);
                    e.op(Op::FMul, &[x, scale], Ty::F32)
                })
                .collect()
        }
        PackHalf2x16 => {
            let x = e.op(Op::FToHalf, &[a(0)[0]], Ty::U32);
            let y = e.op(Op::FToHalf, &[a(0)[1]], Ty::U32);
            let sixteen = e.konst(ops::Value::U(16));
            let y = e.op(Op::IShl, &[y, sixteen], Ty::U32);
            vec![e.op(Op::IOr, &[x, y], Ty::U32)]
        }
        UnpackHalf2x16 => {
            let p = a(0)[0];
            let mask = e.konst(ops::Value::U(0xFFFF));
            let sixteen = e.konst(ops::Value::U(16));
            let lo = e.op(Op::IAnd, &[p, mask], Ty::U32);
            let hi = e.op(Op::UShr, &[p, sixteen], Ty::U32);
            vec![e.op(Op::HalfToF, &[lo], Ty::F32), e.op(Op::HalfToF, &[hi], Ty::F32)]
        }
        Length => {
            let d = dot(e, a(0), a(0));
            vec![e.op(Op::FSqrt, &[d], Ty::F32)]
        }
        Distance => {
            let diff: Vec<E::V> = (0..a(0).len()).map(|i| e.op(Op::FSub, &[a(0)[i], a(1)[i]], Ty::F32)).collect();
            let d = dot(e, &diff, &diff);
            vec![e.op(Op::FSqrt, &[d], Ty::F32)]
        }
        Dot => vec![dot(e, a(0), a(1))],
        Cross => {
            let (x, y) = (a(0), a(1));
            let mut c = |i: usize, j: usize| {
                let p = e.op(Op::FMul, &[x[i], y[j]], Ty::F32);
                let q = e.op(Op::FMul, &[y[i], x[j]], Ty::F32);
                e.op(Op::FSub, &[p, q], Ty::F32)
            };
            vec![c(1, 2), c(2, 0), c(0, 1)]
        }
        Normalize => {
            let d = dot(e, a(0), a(0));
            let inv = e.op(Op::FRsq, &[d], Ty::F32);
            a(0).iter().map(|&x| e.op(Op::FMul, &[x, inv], Ty::F32)).collect()
        }
        Faceforward => {
            // dot(Nref, I) < 0 ? N : -N
            let d = dot(e, a(2), a(1));
            let zero = f(e, 0.0);
            let neg = e.op(Op::FLt, &[d, zero], Ty::Bool);
            (0..n)
                .map(|i| {
                    let m = e.op(Op::FNeg, &[a(0)[i]], Ty::F32);
                    e.select(neg, a(0)[i], m, Ty::F32)
                })
                .collect()
        }
        Reflect => {
            // I - 2 dot(N, I) N
            let d = dot(e, a(1), a(0));
            let two = f(e, 2.0);
            let k = e.op(Op::FMul, &[two, d], Ty::F32);
            (0..n)
                .map(|i| {
                    let p = e.op(Op::FMul, &[k, a(1)[i]], Ty::F32);
                    e.op(Op::FSub, &[a(0)[i], p], Ty::F32)
                })
                .collect()
        }
        Refract => {
            // k = 1 - eta^2 (1 - dot(N, I)^2);
            // k < 0 ? 0 : eta I - (eta dot(N, I) + sqrt(k)) N
            let eta = a(2)[0];
            let d = dot(e, a(1), a(0));
            let (zero, one) = (f(e, 0.0), f(e, 1.0));
            let dd = e.op(Op::FMul, &[d, d], Ty::F32);
            let t = e.op(Op::FSub, &[one, dd], Ty::F32);
            let ee = e.op(Op::FMul, &[eta, eta], Ty::F32);
            let et = e.op(Op::FMul, &[ee, t], Ty::F32);
            let k = e.op(Op::FSub, &[one, et], Ty::F32);
            let total = e.op(Op::FLt, &[k, zero], Ty::Bool);
            let sk = e.op(Op::FSqrt, &[k], Ty::F32);
            let ed = e.op(Op::FMul, &[eta, d], Ty::F32);
            let c = e.op(Op::FAdd, &[ed, sk], Ty::F32);
            (0..n)
                .map(|i| {
                    let p = e.op(Op::FMul, &[eta, a(0)[i]], Ty::F32);
                    let q = e.op(Op::FMul, &[c, a(1)[i]], Ty::F32);
                    let v = e.op(Op::FSub, &[p, q], Ty::F32);
                    e.select(total, zero, v, Ty::F32)
                })
                .collect()
        }
        MatrixCompMult => (0..n).map(|i| e.op(Op::FMul, &[a(0)[i], a(1)[i]], Ty::F32)).collect(),
        OuterProduct => {
            let (c, r) = (a(0), a(1));
            let mut out = Vec::with_capacity(c.len() * r.len());
            for &rv in r {
                for &cv in c {
                    out.push(e.op(Op::FMul, &[cv, rv], Ty::F32));
                }
            }
            out
        }
        Transpose => {
            let Some(Basic::Matrix(cols, rows)) = args[0].0.as_basic() else { return (Vec::new(), None) };
            let (cols, rows) = (cols as usize, rows as usize);
            let m = a(0);
            let mut out = Vec::with_capacity(cols * rows);
            // The result has `rows` columns of `cols` rows.
            for r in 0..rows {
                for c in 0..cols {
                    out.push(m[c * rows + r]);
                }
            }
            out
        }
        Determinant => {
            let size = args[0].0.as_basic().map_or(2, |b| match b {
                Basic::Matrix(c, _) => c as usize,
                _ => 2,
            });
            vec![determinant(e, a(0), size)]
        }
        Inverse => {
            let size = args[0].0.as_basic().map_or(2, |b| match b {
                Basic::Matrix(c, _) => c as usize,
                _ => 2,
            });
            inverse(e, a(0), size)
        }
        LessThan | LessThanEqual | GreaterThan | GreaterThanEqual => {
            let s = scalar_of(0);
            let op = match b {
                LessThan => BinOp::Lt,
                LessThanEqual => BinOp::Le,
                GreaterThan => BinOp::Gt,
                _ => BinOp::Ge,
            };
            (0..n).map(|i| compare(e, op, s, a(0)[i], a(1)[i])).collect()
        }
        Equal | NotEqual => {
            let s = ty(scalar_of(0));
            let not = b == NotEqual;
            (0..n).map(|i| equal_typed(e, not, &[s], &a(0)[i..=i], &a(1)[i..=i])).collect()
        }
        Any | All => {
            let op = if b == Any { Op::BOr } else { Op::BAnd };
            let mut acc = a(0)[0];
            for &x in &a(0)[1..] {
                acc = e.op(op, &[acc, x], Ty::Bool);
            }
            vec![acc]
        }
        Not => a(0).iter().map(|&x| e.op(Op::BNot, &[x], Ty::Bool)).collect(),
        DFdx | DFdy | Fwidth | Texture(_) | TextureSize => Vec::new(),
    };
    (r, None)
}

/// Element (c, r) of a column-major square matrix of `size`.
fn el<V: Copy>(m: &[V], size: usize, c: usize, r: usize) -> V {
    m[c * size + r]
}

fn mul<E: Emit>(e: &mut E, a: E::V, b: E::V) -> E::V {
    e.op(Op::FMul, &[a, b], Ty::F32)
}

fn sub<E: Emit>(e: &mut E, a: E::V, b: E::V) -> E::V {
    e.op(Op::FSub, &[a, b], Ty::F32)
}

fn add<E: Emit>(e: &mut E, a: E::V, b: E::V) -> E::V {
    e.op(Op::FAdd, &[a, b], Ty::F32)
}

/// The determinant of the square matrix `m`, by cofactor expansion along
/// the first column.
fn determinant<E: Emit>(e: &mut E, m: &[E::V], size: usize) -> E::V {
    match size {
        2 => {
            let p = mul(e, el(m, 2, 0, 0), el(m, 2, 1, 1));
            let q = mul(e, el(m, 2, 1, 0), el(m, 2, 0, 1));
            sub(e, p, q)
        }
        3 => {
            // det = sum over the first column of m(0, r) * C(0, r), where
            // C(0, r) = (-1)^r * the minor without column 0 and row r.
            let mut acc = None;
            for r in 0..3 {
                let rs: Vec<usize> = (0..3).filter(|&x| x != r).collect();
                let p = mul(e, el(m, 3, 1, rs[0]), el(m, 3, 2, rs[1]));
                let q = mul(e, el(m, 3, 2, rs[0]), el(m, 3, 1, rs[1]));
                let minor = sub(e, p, q);
                let term = mul(e, el(m, 3, 0, r), minor);
                acc = Some(match acc {
                    None => term,
                    Some(a) if r == 1 => sub(e, a, term),
                    Some(a) => add(e, a, term),
                });
            }
            acc.unwrap_or_else(|| f(e, 0.0))
        }
        _ => {
            let cof = cofactors4(e, m);
            let mut acc = mul(e, el(m, 4, 0, 0), cof[0]);
            for (r, &c) in cof.iter().enumerate().take(4).skip(1) {
                let p = mul(e, el(m, 4, 0, r), c);
                acc = add(e, acc, p);
            }
            acc
        }
    }
}

/// The 3x3 cofactors C(c, r), in the order of the matrix's components.
fn cofactors3<E: Emit>(e: &mut E, m: &[E::V]) -> Vec<E::V> {
    let mut out = Vec::with_capacity(9);
    for c in 0..3 {
        for r in 0..3 {
            // The minor without column c and row r.
            let cs: Vec<usize> = (0..3).filter(|&x| x != c).collect();
            let rs: Vec<usize> = (0..3).filter(|&x| x != r).collect();
            let p = mul(e, el(m, 3, cs[0], rs[0]), el(m, 3, cs[1], rs[1]));
            let q = mul(e, el(m, 3, cs[1], rs[0]), el(m, 3, cs[0], rs[1]));
            let minor = sub(e, p, q);
            out.push(if (c + r) % 2 == 0 { minor } else { e.op(Op::FNeg, &[minor], Ty::F32) });
        }
    }
    out
}

/// The 4x4 cofactors C(c, r), in the order of the matrix's components.
fn cofactors4<E: Emit>(e: &mut E, m: &[E::V]) -> Vec<E::V> {
    let mut out = Vec::with_capacity(16);
    for c in 0..4 {
        for r in 0..4 {
            let cs: Vec<usize> = (0..4).filter(|&x| x != c).collect();
            let rs: Vec<usize> = (0..4).filter(|&x| x != r).collect();
            let sub3: Vec<E::V> =
                cs.iter().flat_map(|&cc| rs.iter().map(move |&rr| (cc, rr))).map(|(cc, rr)| el(m, 4, cc, rr)).collect();
            let minor = determinant(e, &sub3, 3);
            out.push(if (c + r) % 2 == 0 { minor } else { e.op(Op::FNeg, &[minor], Ty::F32) });
        }
    }
    out
}

/// The inverse: the transposed cofactor matrix divided by the determinant
/// (undefined, as GLSL says, for a singular matrix).
fn inverse<E: Emit>(e: &mut E, m: &[E::V], size: usize) -> Vec<E::V> {
    let cof = match size {
        2 => {
            // C(0,0) = d, C(0,1) = -c, C(1,0) = -b, C(1,1) = a
            let (a, b, c, d) = (el(m, 2, 0, 0), el(m, 2, 0, 1), el(m, 2, 1, 0), el(m, 2, 1, 1));
            let nb = e.op(Op::FNeg, &[b], Ty::F32);
            let nc = e.op(Op::FNeg, &[c], Ty::F32);
            vec![d, nc, nb, a]
        }
        3 => cofactors3(e, m),
        _ => cofactors4(e, m),
    };
    let det = match size {
        2 => determinant(e, m, 2),
        _ => {
            let mut acc = mul(e, el(m, size, 0, 0), cof[0]);
            for (r, &c) in cof.iter().enumerate().take(size).skip(1) {
                let p = mul(e, el(m, size, 0, r), c);
                acc = add(e, acc, p);
            }
            acc
        }
    };
    let one = f(e, 1.0);
    let inv = e.op(Op::FDiv, &[one, det], Ty::F32);
    let mut out = Vec::with_capacity(size * size);
    // inverse(c, r) = C(r, c) / det
    for c in 0..size {
        for r in 0..size {
            let x = cof[r * size + c];
            out.push(mul(e, x, inv));
        }
    }
    out
}
