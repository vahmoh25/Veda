//! The checked shader: a typed tree in which every name is resolved.
//!
//! [`crate::sema`] builds it from the syntax tree. Expressions carry their
//! types; variables, functions and built-ins are referred to by index;
//! constructors, swizzles and member selections are explicit; constant
//! expressions are folded. The IR builder lowers it to SSA.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::Stage;
use crate::Version;
use crate::ast::Interp;
use crate::builtins::{Builtin, BuiltinVar};
use crate::diag::Loc;
use crate::intern::{Interner, Symbol};
use crate::ops;
use crate::types::{Precision, Structs, Type};

/// A variable's index in [`Shader::vars`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VarId(pub u32);

/// A function's index in [`Shader::functions`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FuncId(pub u32);

/// A constant value: its scalar components, flattened in order (array
/// elements, structure members, matrix columns).
pub type ConstValue = Vec<ops::Value>;

/// How a parameter passes its value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

/// What a variable is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VarKind {
    /// A global variable without storage qualifier.
    Global,
    Local,
    Param(ParamMode),
    /// A compile-time constant (its value is in [`Var::value`]).
    Const,
    /// A uniform of the default block.
    Uniform,
    /// A uniform block's instance (its type is the block's structure), or
    /// a member of a block without an instance name.
    BlockInstance(u32),
    BlockMember(u32, u32),
    /// A shader input: an attribute, or a varying of a fragment shader.
    Input,
    /// A shader output: a varying of a vertex shader, or a fragment output.
    Output,
    /// A built-in input or output (`gl_Position`, `gl_FragCoord`...).
    Builtin(BuiltinVar),
}

/// A variable.
#[derive(Clone, Debug)]
pub struct Var {
    pub name: Symbol,
    pub ty: Type,
    pub kind: VarKind,
    pub precision: Option<Precision>,
    pub interp: Interp,
    pub centroid: bool,
    pub invariant: bool,
    /// `layout(location = N)`.
    pub location: Option<u32>,
    /// A constant's value.
    pub value: Option<ConstValue>,
    pub loc: Loc,
    /// Read or written anywhere in the shader (static use).
    pub used: bool,
    /// Cannot be assigned (a `const in` parameter).
    pub read_only: bool,
}

/// Layout of a uniform block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockLayout {
    Std140,
    Shared,
    Packed,
}

/// A uniform block member.
#[derive(Clone, Debug)]
pub struct BlockMember {
    pub name: Symbol,
    pub ty: Type,
    pub precision: Option<Precision>,
    /// Matrices (in this member) are stored row by row.
    pub row_major: bool,
}

/// A uniform block.
#[derive(Clone, Debug)]
pub struct UniformBlock {
    pub name: Symbol,
    pub instance: Option<Symbol>,
    /// An array of blocks: its length.
    pub array: Option<u32>,
    pub members: Vec<BlockMember>,
    pub layout: BlockLayout,
    /// The structure type describing the members (for instance access).
    pub struct_ty: Type,
    pub loc: Loc,
    pub used: bool,
}

/// A unary operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

/// A binary operator; the operand types say what it does (component-wise,
/// linear algebra, comparison of whole values...).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Lt,
    Gt,
    Le,
    Ge,
    /// Whole-value equality (all components, all members).
    Eq,
    Ne,
    /// `^^`: both operands are evaluated.
    Xor,
}

/// `&&` and `||`: the right operand is evaluated only when needed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LogicOp {
    And,
    Or,
}

/// Up to four vector components, by index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Swizzle {
    pub comps: [u8; 4],
    pub len: u8,
}

impl Swizzle {
    pub fn components(&self) -> &[u8] {
        &self.comps[..self.len as usize]
    }
}

/// An expression and its type.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Type,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Const(ConstValue),
    Var(VarId),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Logic(LogicOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Assigns to an l-value; the value is the assigned value.
    Assign(Box<Expr>, Box<Expr>),
    /// `lhs op= rhs`.
    CompoundAssign(BinOp, Box<Expr>, Box<Expr>),
    /// `++`/`--`, before or after.
    IncDec {
        target: Box<Expr>,
        increment: bool,
        prefix: bool,
    },
    /// An array element, vector component or matrix column.
    Index(Box<Expr>, Box<Expr>),
    Swizzle(Box<Expr>, Swizzle),
    /// A structure member, by index.
    Field(Box<Expr>, u32),
    /// A constructor of the expression's type.
    Construct(Vec<Expr>),
    Call(FuncId, Vec<Expr>),
    Builtin(Builtin, Vec<Expr>),
    /// `a, b`.
    Sequence(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// The constant value, if the expression was folded.
    pub fn constant(&self) -> Option<&ConstValue> {
        match &self.kind {
            ExprKind::Const(v) => Some(v),
            _ => None,
        }
    }
}

/// A `switch` clause: its labels (`None` is `default`) and statements;
/// control falls through to the next clause.
#[derive(Clone, Debug)]
pub struct Clause {
    pub labels: Vec<Option<ops::Value>>,
    pub body: Vec<Stmt>,
}

/// A loop of any kind.
#[derive(Clone, Debug)]
pub struct Loop {
    /// Tested before each iteration (after the first, for `do`-`while`).
    pub cond: Option<Expr>,
    /// A `while (T x = e)` condition variable, initialised before each test.
    pub cond_var: Option<(VarId, Expr)>,
    /// Evaluated at the end of each iteration (`for`).
    pub step: Option<Expr>,
    pub body: Vec<Stmt>,
    pub do_while: bool,
}

/// A statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    Expr(Expr),
    /// A local variable, initialised or not.
    Decl(VarId, Option<Expr>),
    Block(Vec<Stmt>),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    Loop(Box<Loop>),
    Switch(Expr, Vec<Clause>),
    Break,
    Continue,
    Return(Option<Expr>),
    Discard,
}

/// A function.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: Symbol,
    pub ret: Type,
    pub ret_precision: Option<Precision>,
    pub params: Vec<VarId>,
    /// `None` for a function only declared.
    pub body: Option<Vec<Stmt>>,
    pub loc: Loc,
    /// Functions this one calls (for detecting recursion).
    pub calls: Vec<FuncId>,
}

/// A checked shader.
#[derive(Debug)]
pub struct Shader {
    pub stage: Stage,
    pub version: Version,
    pub interner: Interner,
    pub structs: Structs,
    pub vars: Vec<Var>,
    pub functions: Vec<Function>,
    pub main: Option<FuncId>,
    /// Global variables' initialisers, run before `main`.
    pub init: Vec<Stmt>,
    pub blocks: Vec<UniformBlock>,
    /// `#pragma STDGL invariant(all)`.
    pub invariant_all: bool,
    /// The shader uses `discard`.
    pub discards: bool,
}

impl Shader {
    pub fn var(&self, id: VarId) -> &Var {
        &self.vars[id.0 as usize]
    }

    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.0 as usize]
    }

    /// A name's text.
    pub fn name(&self, s: Symbol) -> &str {
        self.interner.get(s)
    }
}
