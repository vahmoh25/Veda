//! The language's types.
//!
//! [`Basic`] is a type a keyword names: `void`, scalars, vectors, matrices
//! (of `float` only) and samplers. [`Type`] adds structures and arrays
//! (one dimension: neither GLSL ES 1.00 nor 3.00 has arrays of arrays).
//! Precision qualifiers are not part of a type's identity; they travel
//! beside it.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::intern::Symbol;

/// A scalar type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Scalar {
    Float,
    Int,
    Uint,
    Bool,
}

impl Scalar {
    pub fn name(self) -> &'static str {
        match self {
            Scalar::Float => "float",
            Scalar::Int => "int",
            Scalar::Uint => "uint",
            Scalar::Bool => "bool",
        }
    }

    /// The prefix of this scalar's vector types (`vec`, `ivec`...).
    fn vec_prefix(self) -> &'static str {
        match self {
            Scalar::Float => "vec",
            Scalar::Int => "ivec",
            Scalar::Uint => "uvec",
            Scalar::Bool => "bvec",
        }
    }

    /// Whether arithmetic applies (not `bool`).
    pub fn is_numeric(self) -> bool {
        self != Scalar::Bool
    }

    /// `int` or `uint`.
    pub fn is_integer(self) -> bool {
        matches!(self, Scalar::Int | Scalar::Uint)
    }
}

/// What a sampler's coordinates address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Dim {
    D2,
    D3,
    Cube,
    D2Array,
}

/// A sampler type: its dimensionality, whether it compares depth (a
/// shadow sampler) and the type of the values it returns.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Sampler {
    pub dim: Dim,
    pub shadow: bool,
    /// `Float`, `Int` (`isampler`) or `Uint` (`usampler`).
    pub ty: Scalar,
}

impl Sampler {
    pub const fn new(dim: Dim, shadow: bool, ty: Scalar) -> Sampler {
        Sampler { dim, shadow, ty }
    }

    /// Components of the texture coordinate without projection or
    /// comparison reference: 2 for 2D, 3 for 3D, cube and 2D arrays.
    pub fn coords(self) -> u8 {
        match self.dim {
            Dim::D2 => 2,
            Dim::D3 | Dim::Cube | Dim::D2Array => 3,
        }
    }

    pub fn name(self) -> &'static str {
        use Dim::*;
        match (self.ty, self.dim, self.shadow) {
            (Scalar::Float, D2, false) => "sampler2D",
            (Scalar::Float, D3, false) => "sampler3D",
            (Scalar::Float, Cube, false) => "samplerCube",
            (Scalar::Float, D2Array, false) => "sampler2DArray",
            (Scalar::Float, D2, true) => "sampler2DShadow",
            (Scalar::Float, Cube, true) => "samplerCubeShadow",
            (Scalar::Float, D2Array, true) => "sampler2DArrayShadow",
            (Scalar::Int, D2, _) => "isampler2D",
            (Scalar::Int, D3, _) => "isampler3D",
            (Scalar::Int, Cube, _) => "isamplerCube",
            (Scalar::Int, D2Array, _) => "isampler2DArray",
            (Scalar::Uint, D2, _) => "usampler2D",
            (Scalar::Uint, D3, _) => "usampler3D",
            (Scalar::Uint, Cube, _) => "usamplerCube",
            (Scalar::Uint, D2Array, _) => "usampler2DArray",
            _ => "sampler",
        }
    }
}

/// A type named by a keyword.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Basic {
    Void,
    Scalar(Scalar),
    /// A vector of 2 to 4 components.
    Vector(Scalar, u8),
    /// A `float` matrix: columns, then rows (2 to 4 each).
    Matrix(u8, u8),
    Sampler(Sampler),
}

impl Basic {
    pub const FLOAT: Basic = Basic::Scalar(Scalar::Float);
    pub const INT: Basic = Basic::Scalar(Scalar::Int);
    pub const UINT: Basic = Basic::Scalar(Scalar::Uint);
    pub const BOOL: Basic = Basic::Scalar(Scalar::Bool);

    /// A scalar (`n == 1`) or vector of `n` components.
    pub fn vector(s: Scalar, n: u8) -> Basic {
        if n == 1 { Basic::Scalar(s) } else { Basic::Vector(s, n) }
    }

    /// The scalar type of a scalar, vector or matrix's components.
    pub fn scalar(self) -> Option<Scalar> {
        match self {
            Basic::Scalar(s) | Basic::Vector(s, _) => Some(s),
            Basic::Matrix(..) => Some(Scalar::Float),
            _ => None,
        }
    }

    /// Number of scalar components (0 for void and samplers).
    pub fn components(self) -> u32 {
        match self {
            Basic::Scalar(_) => 1,
            Basic::Vector(_, n) => n as u32,
            Basic::Matrix(c, r) => c as u32 * r as u32,
            _ => 0,
        }
    }

    pub fn is_scalar(self) -> bool {
        matches!(self, Basic::Scalar(_))
    }

    pub fn is_vector(self) -> bool {
        matches!(self, Basic::Vector(..))
    }

    pub fn is_matrix(self) -> bool {
        matches!(self, Basic::Matrix(..))
    }

    pub fn is_sampler(self) -> bool {
        matches!(self, Basic::Sampler(_))
    }

    /// Scalars and vectors: what an `if`, swizzle or component-wise
    /// built-in function works on.
    pub fn is_scalar_or_vector(self) -> bool {
        matches!(self, Basic::Scalar(_) | Basic::Vector(..))
    }

    /// The type's name as written in a shader.
    pub fn name(self) -> String {
        match self {
            Basic::Void => "void".into(),
            Basic::Scalar(s) => s.name().into(),
            Basic::Vector(s, n) => alloc::format!("{}{}", s.vec_prefix(), n),
            Basic::Matrix(c, r) if c == r => alloc::format!("mat{c}"),
            Basic::Matrix(c, r) => alloc::format!("mat{c}x{r}"),
            Basic::Sampler(s) => s.name().into(),
        }
    }
}

/// A precision qualifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Precision {
    Low,
    Medium,
    High,
}

impl Precision {
    pub fn name(self) -> &'static str {
        match self {
            Precision::Low => "lowp",
            Precision::Medium => "mediump",
            Precision::High => "highp",
        }
    }
}

/// Index of a structure type in [`Structs`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StructId(pub u32);

/// A structure's member.
#[derive(Clone, PartialEq, Debug)]
pub struct Field {
    pub name: Symbol,
    pub ty: Type,
    pub precision: Option<Precision>,
}

/// A structure type. Two structures are the same type only if they are
/// the same definition (name equivalence), except across shader stages,
/// where linking compares names and members.
#[derive(Clone, PartialEq, Debug)]
pub struct StructDef {
    /// `None` for an anonymous structure.
    pub name: Option<Symbol>,
    pub fields: Vec<Field>,
}

/// The structures a shader defines.
#[derive(Clone, Default, Debug)]
pub struct Structs {
    pub defs: Vec<StructDef>,
}

impl Structs {
    pub fn add(&mut self, def: StructDef) -> StructId {
        self.defs.push(def);
        StructId(self.defs.len() as u32 - 1)
    }

    pub fn get(&self, id: StructId) -> &StructDef {
        &self.defs[id.0 as usize]
    }
}

/// What a value's type is, apart from arrays.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Element {
    Basic(Basic),
    Struct(StructId),
}

/// A complete type: a basic type or structure, possibly an array of them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Type {
    pub element: Element,
    /// The array's length, if the type is an array.
    pub array: Option<u32>,
}

impl Type {
    pub const VOID: Type = Type::basic(Basic::Void);
    pub const FLOAT: Type = Type::basic(Basic::FLOAT);
    pub const INT: Type = Type::basic(Basic::INT);
    pub const UINT: Type = Type::basic(Basic::UINT);
    pub const BOOL: Type = Type::basic(Basic::BOOL);

    pub const fn basic(b: Basic) -> Type {
        Type { element: Element::Basic(b), array: None }
    }

    pub const fn structure(id: StructId) -> Type {
        Type { element: Element::Struct(id), array: None }
    }

    pub fn vector(s: Scalar, n: u8) -> Type {
        Type::basic(Basic::vector(s, n))
    }

    pub fn array_of(self, len: u32) -> Type {
        Type { element: self.element, array: Some(len) }
    }

    /// The type of one element of an array (the type itself otherwise).
    pub fn element_type(self) -> Type {
        Type { element: self.element, array: None }
    }

    pub fn is_array(self) -> bool {
        self.array.is_some()
    }

    /// The basic type, if not an array or structure.
    pub fn as_basic(self) -> Option<Basic> {
        match (self.element, self.array) {
            (Element::Basic(b), None) => Some(b),
            _ => None,
        }
    }

    pub fn as_struct(self) -> Option<StructId> {
        match (self.element, self.array) {
            (Element::Struct(s), None) => Some(s),
            _ => None,
        }
    }

    pub fn is_void(self) -> bool {
        self == Type::VOID
    }

    /// Whether the type is or contains a sampler.
    pub fn contains_sampler(self, structs: &Structs) -> bool {
        match self.element {
            Element::Basic(b) => b.is_sampler(),
            Element::Struct(id) => structs.get(id).fields.iter().any(|f| f.ty.contains_sampler(structs)),
        }
    }

    /// Whether the type is or contains an array.
    pub fn contains_array(self, structs: &Structs) -> bool {
        self.array.is_some()
            || match self.element {
                Element::Basic(_) => false,
                Element::Struct(id) => structs.get(id).fields.iter().any(|f| f.ty.contains_array(structs)),
            }
    }

    /// Whether the type is or contains a structure.
    pub fn contains_struct(self) -> bool {
        matches!(self.element, Element::Struct(_))
    }

    /// Scalar components in the whole value (arrays and structures
    /// flattened; samplers count as one).
    pub fn flat_components(self, structs: &Structs) -> u32 {
        let one = match self.element {
            Element::Basic(Basic::Sampler(_)) => 1,
            Element::Basic(b) => b.components(),
            Element::Struct(id) => structs.get(id).fields.iter().map(|f| f.ty.flat_components(structs)).sum(),
        };
        one * self.array.unwrap_or(1)
    }

    /// The type's name as written in a shader.
    pub fn display<'a>(&'a self, structs: &'a Structs, names: &'a dyn Fn(Symbol) -> String) -> TypeDisplay<'a> {
        TypeDisplay { ty: self, structs, names }
    }
}

/// Formats a [`Type`] for messages.
pub struct TypeDisplay<'a> {
    ty: &'a Type,
    structs: &'a Structs,
    names: &'a dyn Fn(Symbol) -> String,
}

impl fmt::Display for TypeDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty.element {
            Element::Basic(b) => f.write_str(&b.name())?,
            Element::Struct(id) => match self.structs.get(id).name {
                Some(n) => f.write_str(&(self.names)(n))?,
                None => f.write_str("<anonymous struct>")?,
            },
        }
        if let Some(n) = self.ty.array {
            write!(f, "[{n}]")?;
        }
        Ok(())
    }
}
