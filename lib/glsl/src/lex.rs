//! The language's tokens: phase 11, after preprocessing.
//!
//! Identifiers become keywords, type names or reserved words (an error)
//! according to the language version and enabled extensions; numbers
//! become `int`, `uint` or `float` literals, checked against GLSL ES's
//! syntax: no `u` or `f` suffix in 1.00, and integers whose bit pattern
//! needs more than 32 bits are errors. Characters that are no part of the
//! language are reported here, once they have survived preprocessing.

use alloc::vec::Vec;

use crate::Version;
use crate::diag::{Diagnostics, Loc, error, warning};
use crate::intern::{Interner, Symbol};
use crate::pp::{Ext, ExtSet, PpKind, PpTok, Punct};
use crate::types::{Basic, Dim, Sampler, Scalar};

/// A keyword other than a type name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kw {
    Attribute,
    Const,
    Uniform,
    Varying,
    Layout,
    Centroid,
    Flat,
    Smooth,
    Break,
    Continue,
    Do,
    For,
    While,
    Switch,
    Case,
    Default,
    If,
    Else,
    In,
    Out,
    Inout,
    Invariant,
    Discard,
    Return,
    Lowp,
    Mediump,
    Highp,
    Precision,
    Struct,
}

impl Kw {
    pub fn name(self) -> &'static str {
        use Kw::*;
        match self {
            Attribute => "attribute",
            Const => "const",
            Uniform => "uniform",
            Varying => "varying",
            Layout => "layout",
            Centroid => "centroid",
            Flat => "flat",
            Smooth => "smooth",
            Break => "break",
            Continue => "continue",
            Do => "do",
            For => "for",
            While => "while",
            Switch => "switch",
            Case => "case",
            Default => "default",
            If => "if",
            Else => "else",
            In => "in",
            Out => "out",
            Inout => "inout",
            Invariant => "invariant",
            Discard => "discard",
            Return => "return",
            Lowp => "lowp",
            Mediump => "mediump",
            Highp => "highp",
            Precision => "precision",
            Struct => "struct",
        }
    }
}

/// What a token is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tk {
    Ident(Symbol),
    /// An `int` literal's bit pattern.
    Int(u32),
    Uint(u32),
    Float(f32),
    Bool(bool),
    Kw(Kw),
    /// A type keyword.
    Type(Basic),
    Punct(Punct),
    Eof,
}

/// A token and where it is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Token {
    pub tk: Tk,
    pub loc: Loc,
}

/// How an identifier reads in a given version.
enum Word {
    Kw(Kw),
    Type(Basic),
    Bool(bool),
    Reserved,
    Ident,
}

/// Classifies `w` for `version`, with `enabled` extensions.
fn classify(w: &str, version: Version, enabled: ExtSet) -> Word {
    use Kw::*;
    let v3 = version == Version::V300;
    let kw = |k| Word::Kw(k);
    let ty = |b| Word::Type(b);
    let only3 = |word: Word| if v3 { word } else { Word::Ident };
    let f = Scalar::Float;
    match w {
        "const" => kw(Const),
        "uniform" => kw(Uniform),
        "break" => kw(Break),
        "continue" => kw(Continue),
        "do" => kw(Do),
        "for" => kw(For),
        "while" => kw(While),
        "if" => kw(If),
        "else" => kw(Else),
        "in" => kw(In),
        "out" => kw(Out),
        "inout" => kw(Inout),
        "invariant" => kw(Invariant),
        "discard" => kw(Discard),
        "return" => kw(Return),
        "lowp" => kw(Lowp),
        "mediump" => kw(Mediump),
        "highp" => kw(Highp),
        "precision" => kw(Precision),
        "struct" => kw(Struct),
        "true" => Word::Bool(true),
        "false" => Word::Bool(false),
        "void" => ty(Basic::Void),
        "float" => ty(Basic::FLOAT),
        "int" => ty(Basic::INT),
        "bool" => ty(Basic::BOOL),
        "vec2" => ty(Basic::Vector(f, 2)),
        "vec3" => ty(Basic::Vector(f, 3)),
        "vec4" => ty(Basic::Vector(f, 4)),
        "ivec2" => ty(Basic::Vector(Scalar::Int, 2)),
        "ivec3" => ty(Basic::Vector(Scalar::Int, 3)),
        "ivec4" => ty(Basic::Vector(Scalar::Int, 4)),
        "bvec2" => ty(Basic::Vector(Scalar::Bool, 2)),
        "bvec3" => ty(Basic::Vector(Scalar::Bool, 3)),
        "bvec4" => ty(Basic::Vector(Scalar::Bool, 4)),
        "mat2" => ty(Basic::Matrix(2, 2)),
        "mat3" => ty(Basic::Matrix(3, 3)),
        "mat4" => ty(Basic::Matrix(4, 4)),
        "sampler2D" => ty(Basic::Sampler(Sampler::new(Dim::D2, false, f))),
        "samplerCube" => ty(Basic::Sampler(Sampler::new(Dim::Cube, false, f))),
        "attribute" | "varying" => {
            if v3 {
                Word::Reserved
            } else if w == "attribute" {
                kw(Attribute)
            } else {
                kw(Varying)
            }
        }
        "sampler3D" => {
            if v3 || enabled.contains(Ext::OesTexture3D) {
                ty(Basic::Sampler(Sampler::new(Dim::D3, false, f)))
            } else {
                Word::Reserved
            }
        }
        "sampler2DShadow" => {
            if v3 || enabled.contains(Ext::ExtShadowSamplers) {
                ty(Basic::Sampler(Sampler::new(Dim::D2, true, f)))
            } else {
                Word::Reserved
            }
        }
        "switch" | "default" => {
            if v3 {
                kw(if w == "switch" { Switch } else { Default })
            } else {
                Word::Reserved
            }
        }
        "flat" => {
            if v3 {
                kw(Flat)
            } else {
                Word::Reserved
            }
        }
        "layout" => only3(kw(Layout)),
        "centroid" => only3(kw(Centroid)),
        "smooth" => only3(kw(Smooth)),
        "case" => only3(kw(Case)),
        "uint" => only3(ty(Basic::UINT)),
        "uvec2" => only3(ty(Basic::Vector(Scalar::Uint, 2))),
        "uvec3" => only3(ty(Basic::Vector(Scalar::Uint, 3))),
        "uvec4" => only3(ty(Basic::Vector(Scalar::Uint, 4))),
        "mat2x2" => only3(ty(Basic::Matrix(2, 2))),
        "mat2x3" => only3(ty(Basic::Matrix(2, 3))),
        "mat2x4" => only3(ty(Basic::Matrix(2, 4))),
        "mat3x2" => only3(ty(Basic::Matrix(3, 2))),
        "mat3x3" => only3(ty(Basic::Matrix(3, 3))),
        "mat3x4" => only3(ty(Basic::Matrix(3, 4))),
        "mat4x2" => only3(ty(Basic::Matrix(4, 2))),
        "mat4x3" => only3(ty(Basic::Matrix(4, 3))),
        "mat4x4" => only3(ty(Basic::Matrix(4, 4))),
        "samplerCubeShadow" => only3(ty(Basic::Sampler(Sampler::new(Dim::Cube, true, f)))),
        "sampler2DArray" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2Array, false, f)))),
        "sampler2DArrayShadow" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2Array, true, f)))),
        "isampler2D" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2, false, Scalar::Int)))),
        "isampler3D" => only3(ty(Basic::Sampler(Sampler::new(Dim::D3, false, Scalar::Int)))),
        "isamplerCube" => only3(ty(Basic::Sampler(Sampler::new(Dim::Cube, false, Scalar::Int)))),
        "isampler2DArray" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2Array, false, Scalar::Int)))),
        "usampler2D" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2, false, Scalar::Uint)))),
        "usampler3D" => only3(ty(Basic::Sampler(Sampler::new(Dim::D3, false, Scalar::Uint)))),
        "usamplerCube" => only3(ty(Basic::Sampler(Sampler::new(Dim::Cube, false, Scalar::Uint)))),
        "usampler2DArray" => only3(ty(Basic::Sampler(Sampler::new(Dim::D2Array, false, Scalar::Uint)))),
        _ if is_reserved(w, version) => Word::Reserved,
        _ => Word::Ident,
    }
}

/// Words reserved for future use (an error to use).
fn is_reserved(w: &str, version: Version) -> bool {
    const BOTH: &[&str] = &[
        "asm",
        "class",
        "union",
        "enum",
        "typedef",
        "template",
        "this",
        "goto",
        "inline",
        "noinline",
        "volatile",
        "public",
        "static",
        "extern",
        "external",
        "interface",
        "long",
        "short",
        "double",
        "half",
        "fixed",
        "unsigned",
        "superp",
        "input",
        "output",
        "hvec2",
        "hvec3",
        "hvec4",
        "dvec2",
        "dvec3",
        "dvec4",
        "fvec2",
        "fvec3",
        "fvec4",
        "sampler1D",
        "sampler1DShadow",
        "sampler2DRect",
        "sampler3DRect",
        "sampler2DRectShadow",
        "sizeof",
        "cast",
        "namespace",
        "using",
    ];
    const V100: &[&str] = &["packed"];
    const V300: &[&str] = &[
        "coherent",
        "restrict",
        "readonly",
        "writeonly",
        "resource",
        "atomic_uint",
        "noperspective",
        "patch",
        "sample",
        "subroutine",
        "common",
        "partition",
        "active",
        "filter",
        "image1D",
        "image2D",
        "image3D",
        "imageCube",
        "iimage1D",
        "iimage2D",
        "iimage3D",
        "iimageCube",
        "uimage1D",
        "uimage2D",
        "uimage3D",
        "uimageCube",
        "image1DArray",
        "image2DArray",
        "iimage1DArray",
        "iimage2DArray",
        "uimage1DArray",
        "uimage2DArray",
        "imageBuffer",
        "iimageBuffer",
        "uimageBuffer",
        "sampler1DArray",
        "sampler1DArrayShadow",
        "isampler1D",
        "isampler1DArray",
        "usampler1D",
        "usampler1DArray",
        "isampler2DRect",
        "usampler2DRect",
        "samplerBuffer",
        "isamplerBuffer",
        "usamplerBuffer",
        "sampler2DMS",
        "isampler2DMS",
        "usampler2DMS",
        "sampler2DMSArray",
        "isampler2DMSArray",
        "usampler2DMSArray",
    ];
    BOTH.contains(&w)
        || match version {
            Version::V100 => V100.contains(&w),
            Version::V300 => V300.contains(&w),
        }
}

/// Converts preprocessed tokens to the language's tokens, ending with
/// [`Tk::Eof`].
pub fn tokens(
    pp: &[PpTok],
    version: Version,
    enabled: ExtSet,
    interner: &Interner,
    diags: &mut Diagnostics,
) -> Vec<Token> {
    let mut out = Vec::with_capacity(pp.len() + 1);
    for t in pp {
        let loc = t.loc;
        let tk = match t.kind {
            PpKind::Ident(s) => {
                let w = interner.get(s);
                match classify(w, version, enabled) {
                    Word::Kw(k) => Tk::Kw(k),
                    Word::Type(b) => Tk::Type(b),
                    Word::Bool(b) => Tk::Bool(b),
                    Word::Reserved => {
                        error!(diags, loc, "'{w}' is a reserved word");
                        Tk::Ident(s)
                    }
                    Word::Ident => {
                        if w.len() > 1024 {
                            error!(diags, loc, "identifier longer than 1024 characters");
                        } else if w.contains("__") && !w.starts_with("__") {
                            warning!(diags, loc, "identifiers containing '__' are reserved: '{w}'");
                        }
                        Tk::Ident(s)
                    }
                }
            }
            PpKind::Number(s) => number(interner.get(s), version, loc, diags),
            PpKind::Punct(Punct::Hash) => {
                error!(diags, loc, "unexpected '#'");
                continue;
            }
            PpKind::Punct(p) => Tk::Punct(p),
            PpKind::Invalid(c) => {
                error!(diags, loc, "invalid character '{}'", c.escape_default());
                continue;
            }
            PpKind::Newline => continue,
        };
        out.push(Token { tk, loc });
    }
    let end = pp.last().map_or(Loc::new(0, 1), |t| t.loc);
    out.push(Token { tk: Tk::Eof, loc: end });
    out
}

/// Converts a preprocessing number to a literal.
fn number(text: &str, version: Version, loc: Loc, diags: &mut Diagnostics) -> Tk {
    let hex = text.starts_with("0x") || text.starts_with("0X");
    let is_float = text.contains('.') || (!hex && (text.contains('e') || text.contains('E')));
    if is_float {
        let body = match text.strip_suffix(['f', 'F']) {
            Some(b) if version == Version::V300 => b,
            Some(_) => {
                error!(diags, loc, "floating-point suffixes need GLSL ES 3.00: '{text}'");
                return Tk::Float(0.0);
            }
            None => text,
        };
        if !float_syntax(body) {
            error!(diags, loc, "invalid floating-point constant '{text}'");
            return Tk::Float(0.0);
        }
        return match body.parse::<f32>() {
            Ok(v) if v.is_infinite() => {
                warning!(diags, loc, "floating-point constant '{text}' is too large: infinity");
                Tk::Float(v)
            }
            Ok(v) => Tk::Float(v),
            Err(_) => {
                error!(diags, loc, "invalid floating-point constant '{text}'");
                Tk::Float(0.0)
            }
        };
    }
    let (body, unsigned) = match text.strip_suffix(['u', 'U']) {
        Some(b) => (b, true),
        None => (text, false),
    };
    if unsigned && version == Version::V100 {
        error!(diags, loc, "unsigned integers need GLSL ES 3.00: '{text}'");
        return Tk::Int(0);
    }
    let (digits, radix) = if hex {
        (&body[2..], 16)
    } else if body.len() > 1 && body.starts_with('0') {
        (&body[1..], 8)
    } else {
        (body, 10)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        error!(diags, loc, "invalid integer constant '{text}'");
        return Tk::Int(0);
    }
    match u64::from_str_radix(digits, radix) {
        Ok(v) if v <= u64::from(u32::MAX) => {
            if unsigned {
                Tk::Uint(v as u32)
            } else {
                Tk::Int(v as u32)
            }
        }
        _ => {
            error!(diags, loc, "integer constant '{text}' needs more than 32 bits");
            Tk::Int(0)
        }
    }
}

/// Whether `s` (without suffix) is a GLSL floating-point constant: a
/// fractional constant (`1.`, `.5`, `1.5`) with an optional exponent, or
/// digits with an exponent.
fn float_syntax(s: &str) -> bool {
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((a, b)) => {
            (!a.is_empty() || !b.is_empty())
                && a.chars().all(|c| c.is_ascii_digit())
                && b.chars().all(|c| c.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.chars().all(|c| c.is_ascii_digit()) && exponent.is_some(),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let digits = e.strip_prefix(['+', '-']).unwrap_or(e);
        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}
