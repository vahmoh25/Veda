//! Scalar operations and their exact semantics.
//!
//! Every computation a shader does is lowered to these operations on
//! scalars ([`Value`]). [`eval`] defines what each one computes, bit for
//! bit, and is the single source of truth: constant expressions are folded
//! with it at compile time, the optimiser folds with it, and the
//! interpreter's SIMD code is tested against it. Where GLSL ES leaves a
//! result undefined, the choice made here is the one SSE hardware makes,
//! so that the interpreter can use the plain instruction:
//!
//! * `min`/`max` return the second operand when either is NaN (`minps`).
//! * Converting a float that is NaN or out of range to `int` gives
//!   `0x80000000` (`cvttps2dq`); to `uint` it saturates (NaN gives 0).
//! * Integer division by zero gives all ones (as Direct3D defines it),
//!   and so does the remainder; `INT_MIN / -1` wraps to `INT_MIN`.
//! * Shift amounts are taken modulo 32.
//! * `round` rounds halfway cases to even, like `roundEven`.
//!
//! Transcendental functions come from `vmath`, so a constant folded on the
//! host or in Veda is the same value the interpreter computes at run time.

use vmath::f32 as m;

/// A scalar type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ty {
    F32,
    I32,
    U32,
    Bool,
}

/// A scalar value. Integers of either signedness share the bit pattern of
/// their 32-bit representation; the operation says how to read it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    F(f32),
    I(i32),
    U(u32),
    B(bool),
}

impl Value {
    pub fn ty(self) -> Ty {
        match self {
            Value::F(_) => Ty::F32,
            Value::I(_) => Ty::I32,
            Value::U(_) => Ty::U32,
            Value::B(_) => Ty::Bool,
        }
    }

    /// The value's 32 bits, as stored in a register (`true` is all ones).
    pub fn bits(self) -> u32 {
        match self {
            Value::F(f) => f.to_bits(),
            Value::I(i) => i as u32,
            Value::U(u) => u,
            Value::B(b) => {
                if b {
                    !0
                } else {
                    0
                }
            }
        }
    }

    /// Reads 32 bits as a value of type `ty`.
    pub fn from_bits(ty: Ty, bits: u32) -> Value {
        match ty {
            Ty::F32 => Value::F(f32::from_bits(bits)),
            Ty::I32 => Value::I(bits as i32),
            Ty::U32 => Value::U(bits),
            Ty::Bool => Value::B(bits != 0),
        }
    }

    /// Bitwise identity: the same type and bits (so NaNs compare equal,
    /// and `0.0` differs from `-0.0`).
    pub fn same(self, other: Value) -> bool {
        self.ty() == other.ty() && self.bits() == other.bits()
    }

    fn f(self) -> f32 {
        match self {
            Value::F(f) => f,
            other => f32::from_bits(other.bits()),
        }
    }

    fn i(self) -> i32 {
        self.bits() as i32
    }

    fn u(self) -> u32 {
        self.bits()
    }

    fn b(self) -> bool {
        self.bits() != 0
    }
}

/// A scalar operation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Op {
    // float -> float
    FNeg,
    FAbs,
    FSign,
    FFloor,
    FCeil,
    FTrunc,
    FRoundEven,
    FFract,
    FSqrt,
    FRsq,
    FExp,
    FLog,
    FExp2,
    FLog2,
    FSin,
    FCos,
    FTan,
    FAsin,
    FAcos,
    FAtan,
    FSinh,
    FCosh,
    FTanh,
    FAsinh,
    FAcosh,
    FAtanh,
    // (float, float) -> float
    FAdd,
    FSub,
    FMul,
    FDiv,
    FMin,
    FMax,
    FPow,
    FAtan2,
    // (float, float) -> bool
    FLt,
    FLe,
    FEq,
    FNe,
    // float -> bool
    FIsNan,
    FIsInf,
    // int (either signedness) -> int
    INeg,
    INot,
    IAbs,
    ISign,
    // (int, int) -> int; the same bits whatever the signedness
    IAdd,
    ISub,
    IMul,
    IAnd,
    IOr,
    IXor,
    IShl,
    // signed and unsigned versions
    IDiv,
    UDiv,
    IRem,
    URem,
    IShr,
    UShr,
    IMin,
    UMin,
    IMax,
    UMax,
    // (int, int) -> bool
    IEq,
    INe,
    ILt,
    ULt,
    ILe,
    ULe,
    // bool
    BNot,
    BAnd,
    BOr,
    BXor,
    BEq,
    // conversions
    FToI,
    FToU,
    IToF,
    UToF,
    BToF,
    BToI,
    FToB,
    IToB,
    /// Reinterpret 32 bits as another type (`floatBitsToInt`...); also
    /// `int`/`uint` conversions, which keep the bits.
    Bitcast,
    /// Float to half precision: the half's bits in the low 16 bits.
    FToHalf,
    /// Half precision bits (the low 16) to float.
    HalfToF,
}

/// What an operation takes and gives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signature {
    pub args: &'static [Ty],
    pub ret: Ty,
}

impl Op {
    /// The operation's operand and result types. `I32` stands for either
    /// integer type where the operation does not care (callers check).
    pub fn signature(self) -> Signature {
        use Op::*;
        use Ty::*;
        const F1: &[Ty] = &[F32];
        const F2: &[Ty] = &[F32, F32];
        const I1: &[Ty] = &[I32];
        const I2: &[Ty] = &[I32, I32];
        const B1: &[Ty] = &[Bool];
        const B2: &[Ty] = &[Bool, Bool];
        const U1: &[Ty] = &[U32];
        let (args, ret) = match self {
            FNeg | FAbs | FSign | FFloor | FCeil | FTrunc | FRoundEven | FFract | FSqrt | FRsq | FExp | FLog
            | FExp2 | FLog2 | FSin | FCos | FTan | FAsin | FAcos | FAtan | FSinh | FCosh | FTanh | FAsinh | FAcosh
            | FAtanh => (F1, F32),
            FAdd | FSub | FMul | FDiv | FMin | FMax | FPow | FAtan2 => (F2, F32),
            FLt | FLe | FEq | FNe => (F2, Bool),
            FIsNan | FIsInf => (F1, Bool),
            INeg | INot | IAbs | ISign => (I1, I32),
            IAdd | ISub | IMul | IAnd | IOr | IXor | IShl | IDiv | UDiv | IRem | URem | IShr | UShr | IMin | UMin
            | IMax | UMax => (I2, I32),
            IEq | INe | ILt | ULt | ILe | ULe => (I2, Bool),
            BNot => (B1, Bool),
            BAnd | BOr | BXor | BEq => (B2, Bool),
            FToI => (F1, I32),
            FToU => (F1, U32),
            IToF => (I1, F32),
            UToF => (U1, F32),
            BToF => (B1, F32),
            BToI => (B1, I32),
            FToB => (F1, Bool),
            IToB => (I1, Bool),
            Bitcast => (I1, I32),
            FToHalf => (F1, U32),
            HalfToF => (U1, F32),
        };
        Signature { args, ret }
    }

    /// Number of operands.
    pub fn arity(self) -> usize {
        self.signature().args.len()
    }

    /// Whether `op(a, b) == op(b, a)`.
    pub fn commutative(self) -> bool {
        use Op::*;
        matches!(
            self,
            FAdd | FMul
                | FMin
                | FMax
                | FEq
                | FNe
                | IAdd
                | IMul
                | IAnd
                | IOr
                | IXor
                | IMin
                | UMin
                | IMax
                | UMax
                | IEq
                | INe
                | BAnd
                | BOr
                | BXor
                | BEq
        )
    }
}

/// Computes `op` on `args`. `ret` gives the result type where the
/// operation does not fix it (the integer operations and `Bitcast`).
pub fn eval(op: Op, args: &[Value], ret: Ty) -> Value {
    use Op::*;
    let a = args.first().copied().unwrap_or(Value::U(0));
    let b = args.get(1).copied().unwrap_or(Value::U(0));
    let int = |v: u32| Value::from_bits(ret, v);
    match op {
        FNeg => Value::F(-a.f()),
        FAbs => Value::F(f32::from_bits(a.u() & 0x7FFF_FFFF)),
        FSign => {
            let x = a.f();
            Value::F(if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                // 0, -0 and NaN keep themselves (NaN: undefined).
                x
            })
        }
        FFloor => Value::F(m::floor(a.f())),
        FCeil => Value::F(m::ceil(a.f())),
        FTrunc => Value::F(m::trunc(a.f())),
        FRoundEven => Value::F(m::round_ties_even(a.f())),
        FFract => {
            let x = a.f();
            Value::F(x - m::floor(x))
        }
        FSqrt => Value::F(m::sqrt(a.f())),
        FRsq => Value::F(1.0 / m::sqrt(a.f())),
        FExp => Value::F(m::exp(a.f())),
        FLog => Value::F(m::ln(a.f())),
        FExp2 => Value::F(m::exp2(a.f())),
        FLog2 => Value::F(m::log2(a.f())),
        FSin => Value::F(m::sin(a.f())),
        FCos => Value::F(m::cos(a.f())),
        FTan => Value::F(m::tan(a.f())),
        FAsin => Value::F(m::asin(a.f())),
        FAcos => Value::F(m::acos(a.f())),
        FAtan => Value::F(m::atan(a.f())),
        FSinh => Value::F(m::sinh(a.f())),
        FCosh => Value::F(m::cosh(a.f())),
        FTanh => Value::F(m::tanh(a.f())),
        FAsinh => Value::F(m::asinh(a.f())),
        FAcosh => Value::F(m::acosh(a.f())),
        FAtanh => Value::F(m::atanh(a.f())),
        FAdd => Value::F(a.f() + b.f()),
        FSub => Value::F(a.f() - b.f()),
        FMul => Value::F(a.f() * b.f()),
        FDiv => Value::F(a.f() / b.f()),
        // minps/maxps: the second operand unless the comparison holds.
        FMin => Value::F(if a.f() < b.f() { a.f() } else { b.f() }),
        FMax => Value::F(if a.f() > b.f() { a.f() } else { b.f() }),
        FPow => Value::F(m::powf(a.f(), b.f())),
        FAtan2 => Value::F(m::atan2(a.f(), b.f())),
        FLt => Value::B(a.f() < b.f()),
        FLe => Value::B(a.f() <= b.f()),
        FEq => Value::B(a.f() == b.f()),
        FNe => Value::B(a.f() != b.f()),
        FIsNan => Value::B(a.f().is_nan()),
        FIsInf => Value::B(a.f().is_infinite()),
        INeg => int(a.i().wrapping_neg() as u32),
        INot => int(!a.u()),
        IAbs => int(a.i().wrapping_abs() as u32),
        ISign => int(a.i().signum() as u32),
        IAdd => int(a.u().wrapping_add(b.u())),
        ISub => int(a.u().wrapping_sub(b.u())),
        IMul => int(a.u().wrapping_mul(b.u())),
        IAnd => int(a.u() & b.u()),
        IOr => int(a.u() | b.u()),
        IXor => int(a.u() ^ b.u()),
        IShl => int(a.u().wrapping_shl(b.u() & 31)),
        IDiv => int(if b.i() == 0 { !0 } else { a.i().wrapping_div(b.i()) as u32 }),
        UDiv => int(if b.u() == 0 { !0 } else { a.u() / b.u() }),
        IRem => int(if b.i() == 0 { !0 } else { a.i().wrapping_rem(b.i()) as u32 }),
        URem => int(if b.u() == 0 { !0 } else { a.u() % b.u() }),
        IShr => int((a.i() >> (b.u() & 31)) as u32),
        UShr => int(a.u() >> (b.u() & 31)),
        IMin => int(a.i().min(b.i()) as u32),
        UMin => int(a.u().min(b.u())),
        IMax => int(a.i().max(b.i()) as u32),
        UMax => int(a.u().max(b.u())),
        IEq => Value::B(a.u() == b.u()),
        INe => Value::B(a.u() != b.u()),
        ILt => Value::B(a.i() < b.i()),
        ULt => Value::B(a.u() < b.u()),
        ILe => Value::B(a.i() <= b.i()),
        ULe => Value::B(a.u() <= b.u()),
        BNot => Value::B(!a.b()),
        BAnd => Value::B(a.b() && b.b()),
        BOr => Value::B(a.b() || b.b()),
        BXor => Value::B(a.b() != b.b()),
        BEq => Value::B(a.b() == b.b()),
        FToI => Value::I(f_to_i(a.f())),
        FToU => Value::U(f_to_u(a.f())),
        IToF => Value::F(a.i() as f32),
        UToF => Value::F(a.u() as f32),
        BToF => Value::F(if a.b() { 1.0 } else { 0.0 }),
        BToI => int(u32::from(a.b())),
        FToB => Value::B(a.f() != 0.0),
        IToB => Value::B(a.u() != 0),
        Bitcast => Value::from_bits(ret, a.bits()),
        FToHalf => Value::U(u32::from(f32_to_f16(a.f()))),
        HalfToF => Value::F(f16_to_f32(a.u() as u16)),
    }
}

/// `cvttps2dq`: truncation, or `0x80000000` for NaN and out-of-range values.
pub fn f_to_i(x: f32) -> i32 {
    if x.is_nan() || !(-2147483648.0..2147483648.0).contains(&x) { i32::MIN } else { x as i32 }
}

/// Truncation to `uint`, saturating (NaN gives 0).
pub fn f_to_u(x: f32) -> u32 {
    x as u32
}

/// Converts to half precision, rounding to nearest even; overflow gives
/// infinity, NaN stays NaN.
pub fn f32_to_f16(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xFF) as i32;
    let mant = bits & 0x7F_FFFF;
    if exp == 0xFF {
        // Infinity or NaN (keep a quiet NaN).
        return sign | 0x7C00 | if mant != 0 { 0x200 | (mant >> 13) as u16 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1F {
        return sign | 0x7C00;
    }
    if e <= 0 {
        // A subnormal half, or zero.
        if e < -10 {
            return sign;
        }
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        let rounded = if rem > halfway || (rem == halfway && half & 1 == 1) { half + 1 } else { half };
        return sign | rounded as u16;
    }
    let half = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1FFF;
    let rounded = if rem > 0x1000 || (rem == 0x1000 && half & 1 == 1) { half + 1 } else { half };
    // A carry into the exponent is correct, up to infinity.
    sign | rounded as u16
}

/// Converts half precision bits to a float, exactly.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1F);
    let mant = u32::from(h & 0x3FF);
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Normalise the subnormal.
            let mut e = 113u32;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x3FF) << 13)
        }
    } else if exp == 0x1F {
        sign | 0x7F80_0000 | (mant << 13)
    } else {
        sign | ((exp + 112) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}
