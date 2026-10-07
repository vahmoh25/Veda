//! The syntax tree the parser builds: GLSL ES 1.00 and 3.00 alike.
//!
//! The parser accepts the grammar of chapter 9 of either version and
//! records what it saw, qualifier order included; the semantic checks
//! (which qualifiers a version allows, in which order, types, scopes)
//! belong to [`crate::sema`].

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::diag::Loc;
use crate::intern::Symbol;
use crate::types::{Basic, Precision};

/// A whole shader.
#[derive(Debug, Default)]
pub struct TranslationUnit {
    pub items: Vec<External>,
}

/// A declaration at file scope.
#[derive(Debug)]
pub enum External {
    Function(FunctionDef),
    Declaration(Declaration),
}

/// A storage qualifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Storage {
    Const,
    Attribute,
    Varying,
    Uniform,
    In,
    Out,
    /// Parameters only.
    InOut,
}

/// An interpolation qualifier (GLSL ES 3.00).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Interp {
    Smooth,
    Flat,
}

/// One `layout(...)` entry: `name` or `name = value`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LayoutId {
    pub name: Symbol,
    pub value: Option<i64>,
    pub loc: Loc,
}

/// Which qualifier was written, for checking their order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QualKind {
    Layout,
    Invariant,
    Interp,
    Centroid,
    Storage,
    Precision,
}

/// The qualifiers in front of a type.
#[derive(Clone, Debug, Default)]
pub struct Qualifiers {
    /// The storage qualifier; for a parameter, its direction (`const in`
    /// has `In` here and `constant` set).
    pub storage: Option<Storage>,
    /// `const` was written.
    pub constant: bool,
    pub interp: Option<Interp>,
    pub centroid: bool,
    pub invariant: bool,
    pub precision: Option<Precision>,
    pub layout: Vec<LayoutId>,
    /// The qualifiers in the order written, with locations.
    pub order: Vec<(QualKind, Loc)>,
}

impl Qualifiers {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// An array size as written: `[N]` or `[]`.
#[derive(Debug, Clone)]
pub enum ArraySize {
    Sized(Box<Expr>),
    Unsized,
}

/// What a type specifier names.
#[derive(Debug, Clone)]
pub enum TypeName {
    Basic(Basic),
    /// A structure name (resolved by the semantic checks).
    Named(Symbol),
    /// A structure defined right here.
    Struct(Box<StructSpec>),
}

/// A type specifier: a type, maybe with an array size (`float[3]`, GLSL
/// ES 3.00). A precision written with it is in the [`Qualifiers`].
#[derive(Debug, Clone)]
pub struct TypeSpec {
    pub name: TypeName,
    pub array: Option<ArraySize>,
    pub loc: Loc,
}

/// `struct Name { ... }`.
#[derive(Debug, Clone)]
pub struct StructSpec {
    pub name: Option<(Symbol, Loc)>,
    pub members: Vec<MemberDecl>,
    pub loc: Loc,
}

/// A line of structure or block members: `qualifiers type a, b[2];`.
#[derive(Debug, Clone)]
pub struct MemberDecl {
    pub qualifiers: Qualifiers,
    pub ty: TypeSpec,
    pub names: Vec<MemberName>,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub struct MemberName {
    pub name: Symbol,
    pub array: Option<ArraySize>,
    pub loc: Loc,
}

/// A qualified type: `const highp vec3`.
#[derive(Debug, Clone)]
pub struct FullType {
    pub qualifiers: Qualifiers,
    pub ty: TypeSpec,
}

/// One variable in a declaration.
#[derive(Debug, Clone)]
pub struct Declarator {
    pub name: Symbol,
    pub array: Option<ArraySize>,
    pub init: Option<Expr>,
    pub loc: Loc,
}

/// A declaration.
#[derive(Debug, Clone)]
pub enum Declaration {
    /// Variables, or just a type (`struct S { ... };`, `float;`).
    Variables { ty: FullType, declarators: Vec<Declarator>, loc: Loc },
    /// `precision mediump float;`
    Precision { precision: Precision, ty: TypeSpec, loc: Loc },
    /// `invariant gl_Position, v;`
    Invariant { names: Vec<(Symbol, Loc)>, loc: Loc },
    /// A function prototype.
    Prototype(Prototype),
    /// `uniform Name { ... } instance[N];`
    Block(Block),
    /// `layout(std140) uniform;`: defaults for later blocks.
    Defaults { qualifiers: Qualifiers, loc: Loc },
}

/// An interface block.
#[derive(Debug, Clone)]
pub struct Block {
    pub qualifiers: Qualifiers,
    pub name: Symbol,
    pub members: Vec<MemberDecl>,
    pub instance: Option<(Symbol, Option<ArraySize>, Loc)>,
    pub loc: Loc,
}

/// A function parameter.
#[derive(Debug, Clone)]
pub struct Param {
    pub qualifiers: Qualifiers,
    pub ty: TypeSpec,
    pub name: Option<Symbol>,
    pub array: Option<ArraySize>,
    pub loc: Loc,
}

/// A function's prototype.
#[derive(Debug, Clone)]
pub struct Prototype {
    pub ret: FullType,
    pub name: Symbol,
    pub params: Vec<Param>,
    pub loc: Loc,
}

/// A function definition.
#[derive(Debug)]
pub struct FunctionDef {
    pub proto: Prototype,
    pub body: Vec<Stmt>,
    /// Where the closing brace is.
    pub end: Loc,
}

/// A statement.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub loc: Loc,
}

/// `while` and `for` conditions may declare a variable.
#[derive(Debug, Clone)]
pub enum Condition {
    Expr(Expr),
    Decl { ty: FullType, name: Symbol, init: Expr, loc: Loc },
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Declaration(Declaration),
    Expr(Expr),
    Empty,
    /// `{ ... }`: a new scope.
    Compound(Vec<Stmt>),
    If {
        cond: Expr,
        then: Box<Stmt>,
        otherwise: Option<Box<Stmt>>,
    },
    Switch {
        selector: Expr,
        body: Vec<Stmt>,
    },
    Case(Expr),
    Default,
    While {
        cond: Condition,
        body: Box<Stmt>,
    },
    DoWhile {
        body: Box<Stmt>,
        cond: Expr,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Condition>,
        step: Option<Expr>,
        body: Box<Stmt>,
    },
    Continue,
    Break,
    Return(Option<Expr>),
    Discard,
}

/// A unary operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    BitNot,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
}

/// A binary operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    And,
    Xor,
    Or,
}

impl BinaryOp {
    pub fn spelling(self) -> &'static str {
        use BinaryOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Mod => "%",
            Shl => "<<",
            Shr => ">>",
            Lt => "<",
            Gt => ">",
            Le => "<=",
            Ge => ">=",
            Eq => "==",
            Ne => "!=",
            BitAnd => "&",
            BitXor => "^",
            BitOr => "|",
            And => "&&",
            Xor => "^^",
            Or => "||",
        }
    }
}

/// What is called: a function (or a structure's constructor: the name is
/// resolved later) or a type's constructor.
#[derive(Debug, Clone)]
pub enum Callee {
    Name(Symbol),
    /// `Name[N](...)`: an array constructor of a structure type.
    NamedArray(Symbol, Option<Box<Expr>>),
    Type(TypeSpec),
}

/// An expression.
#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Ident(Symbol),
    Int(u32),
    Uint(u32),
    Float(f32),
    Bool(bool),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `a = b`, or `a op= b`.
    Assign(Option<BinaryOp>, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Comma(Box<Expr>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// `.name`: a structure member or a swizzle.
    Field(Box<Expr>, Symbol),
    Call(Callee, Vec<Expr>),
    /// `a.length()`.
    Length(Box<Expr>),
}
