//! Built-in functions, variables and constants (GLSL ES 1.00 chapter 8
//! and 7, GLSL ES 3.00 chapter 8 and 7).
//!
//! Overloads are matched by rule (GLSL ES has no implicit conversions, so
//! a call matches only a signature with exactly its argument types):
//! `genType` is `float` or a `vec`, `genIType` an `int` or `ivec`, and so
//! on. Availability follows the version, the stage and the enabled
//! extensions. Texture functions are recorded as a [`TexCall`] with their
//! arguments in one order: sampler, coordinate, bias or level of detail,
//! gradients, offset.

use alloc::string::String;
use alloc::vec::Vec;

use crate::pp::{Ext, ExtSet};
use crate::types::{Basic, Dim, Sampler, Scalar, Type};
use crate::{Stage, Version};

/// How a texture function chooses the level of detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TexLod {
    /// From the coordinates' derivatives (fragment shaders), plus an
    /// optional bias; the base level in vertex shaders.
    Implicit,
    /// An explicit level.
    Lod,
    /// From explicit gradients.
    Grad,
    /// `texelFetch`: integer coordinates and level, no filtering.
    Fetch,
}

/// A texture lookup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TexCall {
    pub sampler: Sampler,
    pub lod: TexLod,
    /// The coordinate's last component divides the others.
    pub proj: bool,
    pub offset: bool,
    /// A bias argument follows the coordinate (implicit level only).
    pub bias: bool,
    /// Components of the coordinate argument.
    pub coord_size: u8,
}

/// A built-in function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Builtin {
    Radians,
    Degrees,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    /// One argument (`atan(y_over_x)`) or two (`atan(y, x)`).
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Pow,
    Exp,
    Log,
    Exp2,
    Log2,
    Sqrt,
    InverseSqrt,
    Abs,
    Sign,
    Floor,
    Trunc,
    Round,
    RoundEven,
    Ceil,
    Fract,
    Mod,
    Modf,
    Min,
    Max,
    Clamp,
    Mix,
    Step,
    Smoothstep,
    IsNan,
    IsInf,
    FloatBitsToInt,
    FloatBitsToUint,
    IntBitsToFloat,
    UintBitsToFloat,
    PackSnorm2x16,
    UnpackSnorm2x16,
    PackUnorm2x16,
    UnpackUnorm2x16,
    PackHalf2x16,
    UnpackHalf2x16,
    Length,
    Distance,
    Dot,
    Cross,
    Normalize,
    Faceforward,
    Reflect,
    Refract,
    MatrixCompMult,
    OuterProduct,
    Transpose,
    Determinant,
    Inverse,
    LessThan,
    LessThanEqual,
    GreaterThan,
    GreaterThanEqual,
    Equal,
    NotEqual,
    Any,
    All,
    Not,
    DFdx,
    DFdy,
    Fwidth,
    Texture(TexCall),
    TextureSize,
}

impl Builtin {
    /// Whether a call with constant arguments may be folded into a
    /// constant expression (all but texture lookups and derivatives).
    pub fn constant_foldable(self) -> bool {
        !matches!(self, Builtin::Texture(_) | Builtin::TextureSize | Builtin::DFdx | Builtin::DFdy | Builtin::Fwidth)
    }
}

/// A built-in input or output variable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltinVar {
    Position,
    PointSize,
    VertexId,
    InstanceId,
    FragCoord,
    FrontFacing,
    PointCoord,
    FragColor,
    FragData,
    FragDepth,
}

impl BuiltinVar {
    /// Whether the shader writes it (an output).
    pub fn is_output(self) -> bool {
        matches!(
            self,
            BuiltinVar::Position
                | BuiltinVar::PointSize
                | BuiltinVar::FragColor
                | BuiltinVar::FragData
                | BuiltinVar::FragDepth
        )
    }
}

/// The implementation limits a shader sees as `gl_Max*` constants and that
/// linking enforces.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_vertex_attribs: u32,
    pub max_vertex_uniform_vectors: u32,
    pub max_fragment_uniform_vectors: u32,
    /// GLSL ES 1.00's `gl_MaxVaryingVectors`.
    pub max_varying_vectors: u32,
    pub max_vertex_output_vectors: u32,
    pub max_fragment_input_vectors: u32,
    pub max_vertex_texture_image_units: u32,
    pub max_combined_texture_image_units: u32,
    pub max_texture_image_units: u32,
    pub max_draw_buffers: u32,
    pub min_program_texel_offset: i32,
    pub max_program_texel_offset: i32,
    /// Uniform blocks per stage.
    pub max_uniform_blocks: u32,
    /// Bytes of one uniform block.
    pub max_uniform_block_size: u32,
    pub max_transform_feedback_interleaved_components: u32,
    pub max_transform_feedback_separate_attribs: u32,
    pub max_transform_feedback_separate_components: u32,
}

impl Default for Limits {
    /// OpenGL ES 3.0's minimums, raised where Veda's back ends do better.
    fn default() -> Limits {
        Limits {
            max_vertex_attribs: 16,
            max_vertex_uniform_vectors: 256,
            max_fragment_uniform_vectors: 224,
            max_varying_vectors: 15,
            max_vertex_output_vectors: 16,
            max_fragment_input_vectors: 15,
            max_vertex_texture_image_units: 16,
            max_combined_texture_image_units: 32,
            max_texture_image_units: 16,
            max_draw_buffers: 4,
            min_program_texel_offset: -8,
            max_program_texel_offset: 7,
            max_uniform_blocks: 12,
            max_uniform_block_size: 16384,
            max_transform_feedback_interleaved_components: 64,
            max_transform_feedback_separate_attribs: 4,
            max_transform_feedback_separate_components: 4,
        }
    }
}

/// The built-in constants of a version: name and value.
pub fn constants(version: Version, limits: &Limits, enabled: ExtSet) -> Vec<(&'static str, i32)> {
    let mut v = alloc::vec![
        ("gl_MaxVertexAttribs", limits.max_vertex_attribs as i32),
        ("gl_MaxVertexUniformVectors", limits.max_vertex_uniform_vectors as i32),
        ("gl_MaxVertexTextureImageUnits", limits.max_vertex_texture_image_units as i32),
        ("gl_MaxCombinedTextureImageUnits", limits.max_combined_texture_image_units as i32),
        ("gl_MaxTextureImageUnits", limits.max_texture_image_units as i32),
        ("gl_MaxFragmentUniformVectors", limits.max_fragment_uniform_vectors as i32),
    ];
    match version {
        Version::V100 => {
            v.push(("gl_MaxVaryingVectors", limits.max_varying_vectors as i32));
            let draw = if enabled.contains(Ext::ExtDrawBuffers) { limits.max_draw_buffers } else { 1 };
            v.push(("gl_MaxDrawBuffers", draw as i32));
            if enabled.contains(Ext::ExtDrawBuffers) {
                v.push(("gl_MaxDrawBuffersEXT", draw as i32));
            }
        }
        Version::V300 => {
            v.push(("gl_MaxVertexOutputVectors", limits.max_vertex_output_vectors as i32));
            v.push(("gl_MaxFragmentInputVectors", limits.max_fragment_input_vectors as i32));
            v.push(("gl_MaxDrawBuffers", limits.max_draw_buffers as i32));
            v.push(("gl_MinProgramTexelOffset", limits.min_program_texel_offset));
            v.push(("gl_MaxProgramTexelOffset", limits.max_program_texel_offset));
        }
    }
    v
}

/// A built-in variable available in a shader: name, variable, type.
pub struct VarDecl {
    pub name: &'static str,
    pub var: BuiltinVar,
    pub ty: Type,
}

/// The built-in input and output variables of a stage and version.
pub fn variables(stage: Stage, version: Version, limits: &Limits, enabled: ExtSet) -> Vec<VarDecl> {
    let f = Scalar::Float;
    let mut v = Vec::new();
    let mut add = |name, var, ty| v.push(VarDecl { name, var, ty });
    match stage {
        Stage::Vertex => {
            add("gl_Position", BuiltinVar::Position, Type::vector(f, 4));
            add("gl_PointSize", BuiltinVar::PointSize, Type::FLOAT);
            if version == Version::V300 {
                add("gl_VertexID", BuiltinVar::VertexId, Type::INT);
                add("gl_InstanceID", BuiltinVar::InstanceId, Type::INT);
            }
        }
        Stage::Fragment => {
            add("gl_FragCoord", BuiltinVar::FragCoord, Type::vector(f, 4));
            add("gl_FrontFacing", BuiltinVar::FrontFacing, Type::BOOL);
            add("gl_PointCoord", BuiltinVar::PointCoord, Type::vector(f, 2));
            match version {
                Version::V100 => {
                    add("gl_FragColor", BuiltinVar::FragColor, Type::vector(f, 4));
                    let n = if enabled.contains(Ext::ExtDrawBuffers) { limits.max_draw_buffers } else { 1 };
                    add("gl_FragData", BuiltinVar::FragData, Type::vector(f, 4).array_of(n));
                    if enabled.contains(Ext::ExtFragDepth) {
                        add("gl_FragDepthEXT", BuiltinVar::FragDepth, Type::FLOAT);
                    }
                }
                Version::V300 => add("gl_FragDepth", BuiltinVar::FragDepth, Type::FLOAT),
            }
        }
    }
    v
}

/// Where a call to a built-in function is checked.
pub struct CallContext {
    pub stage: Stage,
    pub version: Version,
    pub enabled: ExtSet,
}

/// A resolved built-in call.
pub struct Resolved {
    pub builtin: Builtin,
    pub ret: Type,
    /// The arguments' order for [`Builtin::Texture`] (indices into the
    /// call's arguments), the identity otherwise.
    pub order: Vec<usize>,
    /// Indices of `out` parameters.
    pub outputs: Vec<usize>,
}

/// Why a built-in call failed to resolve.
pub enum CallError {
    /// No built-in function has this name (in this version and stage).
    NoSuchFunction,
    /// The function exists, but not with these argument types.
    NoOverload,
    /// The function exists but is not available here (with the reason).
    Unavailable(String),
}

/// Whether `name` names a built-in function in this version (for
/// redeclaration checks), whatever the stage.
pub fn is_builtin_name(name: &str, version: Version) -> bool {
    let all = ExtSet::all();
    let any_args: &[Type] = &[];
    [Stage::Vertex, Stage::Fragment].into_iter().any(|stage| {
        let ctx = CallContext { stage, version, enabled: all };
        !matches!(resolve(name, any_args, &ctx), Err(CallError::NoSuchFunction))
    })
}

fn float_n(t: Type) -> Option<u8> {
    match t.as_basic()? {
        Basic::Scalar(Scalar::Float) => Some(1),
        Basic::Vector(Scalar::Float, n) => Some(n),
        _ => None,
    }
}

fn scalar_n(t: Type, s: Scalar) -> Option<u8> {
    match t.as_basic()? {
        Basic::Scalar(x) if x == s => Some(1),
        Basic::Vector(x, n) if x == s => Some(n),
        _ => None,
    }
}

/// A vector (not a scalar) of `s` with its size.
fn vec_n(t: Type, s: Scalar) -> Option<u8> {
    match t.as_basic()? {
        Basic::Vector(x, n) if x == s => Some(n),
        _ => None,
    }
}

fn matrix(t: Type) -> Option<(u8, u8)> {
    match t.as_basic()? {
        Basic::Matrix(c, r) => Some((c, r)),
        _ => None,
    }
}

fn sampler(t: Type) -> Option<Sampler> {
    match t.as_basic()? {
        Basic::Sampler(s) => Some(s),
        _ => None,
    }
}

/// Resolves a call of the built-in function `name` with arguments of
/// `args` types.
pub fn resolve(name: &str, args: &[Type], ctx: &CallContext) -> Result<Resolved, CallError> {
    use Builtin::*;
    let v3 = ctx.version == Version::V300;
    let fs = ctx.stage == Stage::Fragment;
    let n = args.len();
    let ok = |b: Builtin, ret: Type| Ok(Resolved { builtin: b, ret, order: (0..n).collect(), outputs: Vec::new() });
    let same = |i: usize, j: usize| args.get(i) == args.get(j);

    // Functions of one genType returning the same type.
    let unary_float = |b: Builtin, needs3: bool| {
        if needs3 && !v3 {
            return Err(CallError::NoSuchFunction);
        }
        match (n, args.first().and_then(|&t| float_n(t))) {
            (1, Some(_)) => ok(b, args[0]),
            _ => Err(CallError::NoOverload),
        }
    };

    match name {
        "radians" => unary_float(Radians, false),
        "degrees" => unary_float(Degrees, false),
        "sin" => unary_float(Sin, false),
        "cos" => unary_float(Cos, false),
        "tan" => unary_float(Tan, false),
        "asin" => unary_float(Asin, false),
        "acos" => unary_float(Acos, false),
        "atan" => match n {
            1 => unary_float(Atan, false),
            2 if float_n(args[0]).is_some() && same(0, 1) => ok(Atan, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "sinh" => unary_float(Sinh, true),
        "cosh" => unary_float(Cosh, true),
        "tanh" => unary_float(Tanh, true),
        "asinh" => unary_float(Asinh, true),
        "acosh" => unary_float(Acosh, true),
        "atanh" => unary_float(Atanh, true),
        "pow" => binary_same(args, Pow),
        "exp" => unary_float(Exp, false),
        "log" => unary_float(Log, false),
        "exp2" => unary_float(Exp2, false),
        "log2" => unary_float(Log2, false),
        "sqrt" => unary_float(Sqrt, false),
        "inversesqrt" => unary_float(InverseSqrt, false),
        "abs" | "sign" => {
            let b = if name == "abs" { Abs } else { Sign };
            match (n, args.first()) {
                (1, Some(&t)) if float_n(t).is_some() => ok(b, t),
                (1, Some(&t)) if v3 && scalar_n(t, Scalar::Int).is_some() => ok(b, t),
                _ => Err(CallError::NoOverload),
            }
        }
        "floor" => unary_float(Floor, false),
        "trunc" => unary_float(Trunc, true),
        "round" => unary_float(Round, true),
        "roundEven" => unary_float(RoundEven, true),
        "ceil" => unary_float(Ceil, false),
        "fract" => unary_float(Fract, false),
        "mod" => match n {
            2 if float_n(args[0]).is_some() && (same(0, 1) || args[1] == Type::FLOAT) => ok(Mod, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "modf" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match n {
                2 if float_n(args[0]).is_some() && same(0, 1) => {
                    Ok(Resolved { builtin: Modf, ret: args[0], order: alloc::vec![0, 1], outputs: alloc::vec![1] })
                }
                _ => Err(CallError::NoOverload),
            }
        }
        "min" | "max" => {
            let b = if name == "min" { Min } else { Max };
            if n != 2 {
                return Err(CallError::NoOverload);
            }
            let t = args[0];
            let elem = t.as_basic().and_then(Basic::scalar);
            match elem {
                Some(Scalar::Float) if float_n(t).is_some() && (same(0, 1) || args[1] == Type::FLOAT) => ok(b, t),
                Some(s @ (Scalar::Int | Scalar::Uint))
                    if v3
                        && !t.as_basic().is_some_and(Basic::is_matrix)
                        && (same(0, 1) || args[1] == Type::basic(Basic::Scalar(s))) =>
                {
                    ok(b, t)
                }
                _ => Err(CallError::NoOverload),
            }
        }
        "clamp" => {
            if n != 3 {
                return Err(CallError::NoOverload);
            }
            let t = args[0];
            let elem = t.as_basic().and_then(Basic::scalar);
            let scalar_bounds = |s: Scalar| args[1] == Type::basic(Basic::Scalar(s)) && args[2] == args[1];
            match elem {
                Some(Scalar::Float)
                    if float_n(t).is_some() && ((same(0, 1) && same(0, 2)) || scalar_bounds(Scalar::Float)) =>
                {
                    ok(Clamp, t)
                }
                Some(s @ (Scalar::Int | Scalar::Uint))
                    if v3
                        && !t.as_basic().is_some_and(Basic::is_matrix)
                        && ((same(0, 1) && same(0, 2)) || scalar_bounds(s)) =>
                {
                    ok(Clamp, t)
                }
                _ => Err(CallError::NoOverload),
            }
        }
        "mix" => {
            if n != 3 || float_n(args[0]).is_none() || !same(0, 1) {
                return Err(CallError::NoOverload);
            }
            let size = float_n(args[0]).unwrap_or(0);
            // mix(x, y, a): a float or genType weight, or (3.00) a bool
            // selector of the same size.
            if same(0, 2) || args[2] == Type::FLOAT || v3 && scalar_n(args[2], Scalar::Bool) == Some(size) {
                ok(Mix, args[0])
            } else {
                Err(CallError::NoOverload)
            }
        }
        "step" => match n {
            2 if float_n(args[1]).is_some() && (same(0, 1) || args[0] == Type::FLOAT) => ok(Step, args[1]),
            _ => Err(CallError::NoOverload),
        },
        "smoothstep" => match n {
            3 if float_n(args[2]).is_some()
                && ((same(0, 2) && same(1, 2)) || (args[0] == Type::FLOAT && args[1] == Type::FLOAT)) =>
            {
                ok(Smoothstep, args[2])
            }
            _ => Err(CallError::NoOverload),
        },
        "isnan" | "isinf" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match (n, args.first().and_then(|&t| float_n(t))) {
                (1, Some(k)) => ok(if name == "isnan" { IsNan } else { IsInf }, Type::vector(Scalar::Bool, k)),
                _ => Err(CallError::NoOverload),
            }
        }
        "floatBitsToInt" | "floatBitsToUint" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            let (b, s) =
                if name == "floatBitsToInt" { (FloatBitsToInt, Scalar::Int) } else { (FloatBitsToUint, Scalar::Uint) };
            match (n, args.first().and_then(|&t| float_n(t))) {
                (1, Some(k)) => ok(b, Type::vector(s, k)),
                _ => Err(CallError::NoOverload),
            }
        }
        "intBitsToFloat" | "uintBitsToFloat" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            let (b, s) =
                if name == "intBitsToFloat" { (IntBitsToFloat, Scalar::Int) } else { (UintBitsToFloat, Scalar::Uint) };
            match (n, args.first().and_then(|&t| scalar_n(t, s))) {
                (1, Some(k)) => ok(b, Type::vector(Scalar::Float, k)),
                _ => Err(CallError::NoOverload),
            }
        }
        "packSnorm2x16" | "packUnorm2x16" | "packHalf2x16" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            let b = match name {
                "packSnorm2x16" => PackSnorm2x16,
                "packUnorm2x16" => PackUnorm2x16,
                _ => PackHalf2x16,
            };
            if n == 1 && args[0] == Type::vector(Scalar::Float, 2) {
                ok(b, Type::UINT)
            } else {
                Err(CallError::NoOverload)
            }
        }
        "unpackSnorm2x16" | "unpackUnorm2x16" | "unpackHalf2x16" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            let b = match name {
                "unpackSnorm2x16" => UnpackSnorm2x16,
                "unpackUnorm2x16" => UnpackUnorm2x16,
                _ => UnpackHalf2x16,
            };
            if n == 1 && args[0] == Type::UINT {
                ok(b, Type::vector(Scalar::Float, 2))
            } else {
                Err(CallError::NoOverload)
            }
        }
        "length" => match (n, args.first().and_then(|&t| float_n(t))) {
            (1, Some(_)) => ok(Length, Type::FLOAT),
            _ => Err(CallError::NoOverload),
        },
        "distance" | "dot" => match n {
            2 if float_n(args[0]).is_some() && same(0, 1) => {
                ok(if name == "distance" { Distance } else { Dot }, Type::FLOAT)
            }
            _ => Err(CallError::NoOverload),
        },
        "cross" => {
            let v3t = Type::vector(Scalar::Float, 3);
            if n == 2 && args[0] == v3t && args[1] == v3t { ok(Cross, v3t) } else { Err(CallError::NoOverload) }
        }
        "normalize" => unary_float(Normalize, false),
        "faceforward" => match n {
            3 if float_n(args[0]).is_some() && same(0, 1) && same(0, 2) => ok(Faceforward, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "reflect" => binary_same(args, Reflect),
        "refract" => match n {
            3 if float_n(args[0]).is_some() && same(0, 1) && args[2] == Type::FLOAT => ok(Refract, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "matrixCompMult" => match n {
            2 if matrix(args[0]).is_some() && same(0, 1) => ok(MatrixCompMult, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "outerProduct" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match (
                n,
                args.first().and_then(|&t| vec_n(t, Scalar::Float)),
                args.get(1).and_then(|&t| vec_n(t, Scalar::Float)),
            ) {
                (2, Some(rows), Some(cols)) => ok(OuterProduct, Type::basic(Basic::Matrix(cols, rows))),
                _ => Err(CallError::NoOverload),
            }
        }
        "transpose" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match (n, args.first().and_then(|&t| matrix(t))) {
                (1, Some((c, r))) => ok(Transpose, Type::basic(Basic::Matrix(r, c))),
                _ => Err(CallError::NoOverload),
            }
        }
        "determinant" | "inverse" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match (n, args.first().and_then(|&t| matrix(t))) {
                (1, Some((c, r))) if c == r => {
                    if name == "determinant" {
                        ok(Determinant, Type::FLOAT)
                    } else {
                        ok(Inverse, args[0])
                    }
                }
                _ => Err(CallError::NoOverload),
            }
        }
        "lessThan" | "lessThanEqual" | "greaterThan" | "greaterThanEqual" => {
            let b = match name {
                "lessThan" => LessThan,
                "lessThanEqual" => LessThanEqual,
                "greaterThan" => GreaterThan,
                _ => GreaterThanEqual,
            };
            if n != 2 || !same(0, 1) {
                return Err(CallError::NoOverload);
            }
            let t = args[0];
            let size = vec_n(t, Scalar::Float)
                .or_else(|| vec_n(t, Scalar::Int))
                .or_else(|| if v3 { vec_n(t, Scalar::Uint) } else { None });
            match size {
                Some(k) => ok(b, Type::vector(Scalar::Bool, k)),
                None => Err(CallError::NoOverload),
            }
        }
        "equal" | "notEqual" => {
            let b = if name == "equal" { Equal } else { NotEqual };
            if n != 2 || !same(0, 1) {
                return Err(CallError::NoOverload);
            }
            let t = args[0];
            let size = vec_n(t, Scalar::Float)
                .or_else(|| vec_n(t, Scalar::Int))
                .or_else(|| vec_n(t, Scalar::Bool))
                .or_else(|| if v3 { vec_n(t, Scalar::Uint) } else { None });
            match size {
                Some(k) => ok(b, Type::vector(Scalar::Bool, k)),
                None => Err(CallError::NoOverload),
            }
        }
        "any" | "all" => match (n, args.first().and_then(|&t| vec_n(t, Scalar::Bool))) {
            (1, Some(_)) => ok(if name == "any" { Any } else { All }, Type::BOOL),
            _ => Err(CallError::NoOverload),
        },
        "not" => match (n, args.first().and_then(|&t| vec_n(t, Scalar::Bool))) {
            (1, Some(_)) => ok(Not, args[0]),
            _ => Err(CallError::NoOverload),
        },
        "dFdx" | "dFdy" | "fwidth" => {
            let b = match name {
                "dFdx" => DFdx,
                "dFdy" => DFdy,
                _ => Fwidth,
            };
            if !v3 && !ctx.enabled.contains(Ext::OesStandardDerivatives) {
                return Err(CallError::Unavailable(alloc::format!(
                    "'{name}' needs the GL_OES_standard_derivatives extension"
                )));
            }
            if !fs {
                return Err(CallError::Unavailable(alloc::format!("'{name}' is only available in fragment shaders")));
            }
            match (n, args.first().and_then(|&t| float_n(t))) {
                (1, Some(_)) => ok(b, args[0]),
                _ => Err(CallError::NoOverload),
            }
        }
        "textureSize" => {
            if !v3 {
                return Err(CallError::NoSuchFunction);
            }
            match (n, args.first().and_then(|&t| sampler(t))) {
                (2, Some(s)) if args[1] == Type::INT => {
                    let size = match s.dim {
                        Dim::D2 | Dim::Cube => 2,
                        Dim::D3 | Dim::D2Array => 3,
                    };
                    ok(TextureSize, Type::vector(Scalar::Int, size))
                }
                _ => Err(CallError::NoOverload),
            }
        }
        _ => texture(name, args, ctx),
    }
}

/// Two arguments of the same `genType`.
fn binary_same(args: &[Type], b: Builtin) -> Result<Resolved, CallError> {
    if args.len() == 2 && float_n(args[0]).is_some() && args[0] == args[1] {
        Ok(Resolved { builtin: b, ret: args[0], order: alloc::vec![0, 1], outputs: Vec::new() })
    } else {
        Err(CallError::NoOverload)
    }
}

/// The texture lookup functions of both versions and the extensions.
fn texture(name: &str, args: &[Type], ctx: &CallContext) -> Result<Resolved, CallError> {
    let v3 = ctx.version == Version::V300;
    let fs = ctx.stage == Stage::Fragment;
    let vs = ctx.stage == Stage::Vertex;
    // (proj, lod, offset)
    let shape = if v3 {
        match name {
            "texture" => (false, TexLod::Implicit, false),
            "textureProj" => (true, TexLod::Implicit, false),
            "textureLod" => (false, TexLod::Lod, false),
            "textureOffset" => (false, TexLod::Implicit, true),
            "texelFetch" => (false, TexLod::Fetch, false),
            "texelFetchOffset" => (false, TexLod::Fetch, true),
            "textureProjOffset" => (true, TexLod::Implicit, true),
            "textureLodOffset" => (false, TexLod::Lod, true),
            "textureProjLod" => (true, TexLod::Lod, false),
            "textureProjLodOffset" => (true, TexLod::Lod, true),
            "textureGrad" => (false, TexLod::Grad, false),
            "textureGradOffset" => (false, TexLod::Grad, true),
            "textureProjGrad" => (true, TexLod::Grad, false),
            "textureProjGradOffset" => (true, TexLod::Grad, true),
            _ => return Err(CallError::NoSuchFunction),
        }
    } else {
        return texture_100(name, args, ctx);
    };
    let (proj, lod, offset) = shape;
    let n = args.len();
    let Some(s) = args.first().and_then(|&t| sampler(t)) else { return Err(CallError::NoOverload) };
    // The coordinate's size: coordinates, plus the reference for shadow
    // samplers, plus q when projecting.
    let base = s.coords() + u8::from(s.shadow) + u8::from(proj);
    let coord = args.get(1).copied();
    let coord_ok = match lod {
        TexLod::Fetch => {
            // texelFetch: no cube maps, no shadow samplers; integer coordinates.
            if s.dim == Dim::Cube || s.shadow {
                return Err(CallError::NoOverload);
            }
            coord.and_then(|t| scalar_n(t, Scalar::Int)) == Some(s.coords())
        }
        _ => {
            let size = coord.and_then(float_n);
            // textureProj on a 2D sampler takes vec3 or vec4.
            size == Some(base) || (proj && s.dim == Dim::D2 && !s.shadow && size == Some(4))
        }
    };
    if !coord_ok {
        return Err(CallError::NoOverload);
    }
    let coord_size = coord.and_then(|t| t.as_basic()).map_or(0, Basic::components) as u8;
    // Which combinations exist (GLSL ES 3.00 section 8.8).
    let allowed = match (s.dim, s.shadow) {
        (Dim::Cube, false) => !proj && !offset,
        (Dim::Cube, true) => !proj && !offset && matches!(lod, TexLod::Implicit | TexLod::Grad),
        (Dim::D2Array, false) => !proj,
        (Dim::D2Array, true) => !proj && (lod == TexLod::Implicit && !offset || lod == TexLod::Grad),
        (Dim::D3, _) => true,
        (Dim::D2, _) => true,
    };
    if !allowed {
        return Err(CallError::NoOverload);
    }
    // The arguments after the coordinate.
    let offset_ty = Type::vector(Scalar::Int, if s.dim == Dim::D3 { 3 } else { 2 });
    let grad_ty = Type::vector(Scalar::Float, if matches!(s.dim, Dim::D3 | Dim::Cube) { 3 } else { 2 });
    let mut i = 2;
    let mut order = alloc::vec![0, 1];
    let mut bias = false;
    let lod_index;
    match lod {
        TexLod::Lod => {
            if args.get(i) != Some(&Type::FLOAT) {
                return Err(CallError::NoOverload);
            }
            lod_index = Some(i);
            i += 1;
        }
        TexLod::Fetch => {
            if args.get(i) != Some(&Type::INT) {
                return Err(CallError::NoOverload);
            }
            lod_index = Some(i);
            i += 1;
        }
        TexLod::Grad => {
            if args.get(i) != Some(&grad_ty) || args.get(i + 1) != Some(&grad_ty) {
                return Err(CallError::NoOverload);
            }
            lod_index = None;
            order.push(i);
            order.push(i + 1);
            i += 2;
        }
        TexLod::Implicit => lod_index = None,
    }
    let offset_index = if offset {
        if args.get(i) != Some(&offset_ty) {
            return Err(CallError::NoOverload);
        }
        i += 1;
        Some(i - 1)
    } else {
        None
    };
    // An optional bias comes last.
    if lod == TexLod::Implicit && i < n {
        if args[i] != Type::FLOAT || s.dim == Dim::D2Array && s.shadow {
            return Err(CallError::NoOverload);
        }
        if !fs {
            return Err(CallError::Unavailable(alloc::format!(
                "'{name}' with a bias is only available in fragment shaders"
            )));
        }
        bias = true;
        order.push(i);
        i += 1;
    }
    if i != n {
        return Err(CallError::NoOverload);
    }
    if let Some(l) = lod_index {
        order.push(l);
    }
    if let Some(o) = offset_index {
        order.push(o);
    }
    let _ = vs;
    let ret = if s.shadow { Type::FLOAT } else { Type::vector(s.ty, 4) };
    let call = TexCall { sampler: s, lod, proj, offset, bias, coord_size };
    Ok(Resolved { builtin: Builtin::Texture(call), ret, order, outputs: Vec::new() })
}

/// GLSL ES 1.00's texture functions, with its extensions.
fn texture_100(name: &str, args: &[Type], ctx: &CallContext) -> Result<Resolved, CallError> {
    let fs = ctx.stage == Stage::Fragment;
    let f = Scalar::Float;
    let d2 = Sampler::new(Dim::D2, false, f);
    let d3 = Sampler::new(Dim::D3, false, f);
    let cube = Sampler::new(Dim::Cube, false, f);
    let shadow = Sampler::new(Dim::D2, true, f);
    let lod_ext = ctx.enabled.contains(Ext::ExtShaderTextureLod);
    // (sampler, coordinate sizes, proj, lod, extension needed, stage rule)
    #[derive(PartialEq)]
    enum Where {
        Any,
        VertexOnly,
        FragmentOnly,
    }
    let (s, sizes, proj, lod, ext, place): (Sampler, &[u8], bool, TexLod, Option<Ext>, Where) = match name {
        "texture2D" => (d2, &[2], false, TexLod::Implicit, None, Where::Any),
        "texture2DProj" => (d2, &[3, 4], true, TexLod::Implicit, None, Where::Any),
        "texture2DLod" => (d2, &[2], false, TexLod::Lod, None, Where::VertexOnly),
        "texture2DProjLod" => (d2, &[3, 4], true, TexLod::Lod, None, Where::VertexOnly),
        "textureCube" => (cube, &[3], false, TexLod::Implicit, None, Where::Any),
        "textureCubeLod" => (cube, &[3], false, TexLod::Lod, None, Where::VertexOnly),
        "texture2DLodEXT" => (d2, &[2], false, TexLod::Lod, Some(Ext::ExtShaderTextureLod), Where::FragmentOnly),
        "texture2DProjLodEXT" => (d2, &[3, 4], true, TexLod::Lod, Some(Ext::ExtShaderTextureLod), Where::FragmentOnly),
        "textureCubeLodEXT" => (cube, &[3], false, TexLod::Lod, Some(Ext::ExtShaderTextureLod), Where::FragmentOnly),
        "texture2DGradEXT" => (d2, &[2], false, TexLod::Grad, Some(Ext::ExtShaderTextureLod), Where::Any),
        "texture2DProjGradEXT" => (d2, &[3, 4], true, TexLod::Grad, Some(Ext::ExtShaderTextureLod), Where::Any),
        "textureCubeGradEXT" => (cube, &[3], false, TexLod::Grad, Some(Ext::ExtShaderTextureLod), Where::Any),
        "texture3D" => (d3, &[3], false, TexLod::Implicit, Some(Ext::OesTexture3D), Where::Any),
        "texture3DProj" => (d3, &[4], true, TexLod::Implicit, Some(Ext::OesTexture3D), Where::Any),
        "texture3DLod" => (d3, &[3], false, TexLod::Lod, Some(Ext::OesTexture3D), Where::VertexOnly),
        "texture3DProjLod" => (d3, &[4], true, TexLod::Lod, Some(Ext::OesTexture3D), Where::VertexOnly),
        "shadow2DEXT" => (shadow, &[3], false, TexLod::Implicit, Some(Ext::ExtShadowSamplers), Where::Any),
        "shadow2DProjEXT" => (shadow, &[4], true, TexLod::Implicit, Some(Ext::ExtShadowSamplers), Where::Any),
        _ => return Err(CallError::NoSuchFunction),
    };
    if let Some(e) = ext
        && !ctx.enabled.contains(e)
    {
        return Err(CallError::Unavailable(alloc::format!("'{name}' needs the {} extension", e.name())));
    }
    match place {
        Where::VertexOnly if fs => {
            let hint = if lod_ext { " (use the EXT variant)" } else { "" };
            return Err(CallError::Unavailable(alloc::format!("'{name}' is only available in vertex shaders{hint}")));
        }
        Where::FragmentOnly if !fs => {
            return Err(CallError::Unavailable(alloc::format!("'{name}' is only available in fragment shaders")));
        }
        _ => {}
    }
    let n = args.len();
    if args.first().and_then(|&t| sampler(t)) != Some(s) {
        return Err(CallError::NoOverload);
    }
    let size = args.get(1).copied().and_then(float_n);
    if !size.is_some_and(|k| sizes.contains(&k)) {
        return Err(CallError::NoOverload);
    }
    let grad_ty = Type::vector(f, if s.dim == Dim::Cube { 3 } else { 2 });
    let mut order = alloc::vec![0, 1];
    let mut bias = false;
    match lod {
        TexLod::Lod => {
            if n != 3 || args[2] != Type::FLOAT {
                return Err(CallError::NoOverload);
            }
            order.push(2);
        }
        TexLod::Grad => {
            if n != 4 || args[2] != grad_ty || args[3] != grad_ty {
                return Err(CallError::NoOverload);
            }
            order.extend([2, 3]);
        }
        _ => match n {
            2 => {}
            3 if args[2] == Type::FLOAT => {
                if !fs {
                    return Err(CallError::Unavailable(alloc::format!(
                        "'{name}' with a bias is only available in fragment shaders"
                    )));
                }
                bias = true;
                order.push(2);
            }
            _ => return Err(CallError::NoOverload),
        },
    }
    let coord_size = size.unwrap_or(0);
    let ret = if s.shadow { Type::FLOAT } else { Type::vector(f, 4) };
    let call = TexCall { sampler: s, lod, proj, offset: false, bias, coord_size };
    Ok(Resolved { builtin: Builtin::Texture(call), ret, order, outputs: Vec::new() })
}
