//! Expressions: types, l-values, calls, constructors and constant folding.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use super::{Entry, Sema};
use crate::ast::{self, BinaryOp, Callee, ExprKind as A, UnaryOp};
use crate::builtins::{self, Builtin, CallError};
use crate::diag::{Loc, error};
use crate::hir::{self, BinOp, ConstValue, Expr, ExprKind as H, FuncId, LogicOp, Swizzle, UnOp, VarKind};
use crate::intern::Symbol;
use crate::lower::{self, ConstEmit};
use crate::ops;
use crate::types::{Basic, Element, Scalar, Type};
use crate::{Stage, Version};

/// A value or an error already reported.
type R = Option<Expr>;

impl Sema<'_> {
    pub(super) fn konst(&self, value: ConstValue, ty: Type, loc: Loc) -> Expr {
        Expr { kind: H::Const(value), ty, loc }
    }

    /// Checks an expression.
    pub(super) fn expr(&mut self, e: &ast::Expr) -> R {
        let loc = e.loc;
        match &e.kind {
            A::Ident(name) => self.ident(*name, loc),
            A::Int(v) => Some(self.konst(alloc::vec![ops::Value::I(*v as i32)], Type::INT, loc)),
            A::Uint(v) => Some(self.konst(alloc::vec![ops::Value::U(*v)], Type::UINT, loc)),
            A::Float(v) => Some(self.konst(alloc::vec![ops::Value::F(*v)], Type::FLOAT, loc)),
            A::Bool(b) => Some(self.konst(alloc::vec![ops::Value::B(*b)], Type::BOOL, loc)),
            A::Unary(op, a) => self.unary(*op, a, loc),
            A::Binary(op, a, b) => self.binary_expr(*op, a, b, loc),
            A::Assign(op, a, b) => self.assign(*op, a, b, loc),
            A::Ternary(c, a, b) => self.ternary(c, a, b, loc),
            A::Comma(a, b) => {
                let a = self.expr(a);
                let b = self.expr(b)?;
                let a = a?;
                if self.version == Version::V100 && (a.ty.is_array() || b.ty.is_array()) {
                    error!(self.diags, loc, "the sequence operator does not apply to arrays in GLSL ES 1.00");
                    return None;
                }
                let ty = b.ty;
                Some(Expr { kind: H::Sequence(Box::new(a), Box::new(b)), ty, loc })
            }
            A::Index(a, i) => self.index(a, i, loc),
            A::Field(a, f) => self.field(a, *f, loc),
            A::Length(a) => {
                let a = self.expr(a)?;
                if self.version == Version::V100 {
                    error!(self.diags, loc, "length() needs GLSL ES 3.00");
                    return None;
                }
                match a.ty.array {
                    Some(n) => Some(self.konst(alloc::vec![ops::Value::I(n as i32)], Type::INT, loc)),
                    None => {
                        error!(self.diags, loc, "length() only applies to arrays");
                        None
                    }
                }
            }
            A::Call(callee, args) => self.call(callee, args, loc),
        }
    }

    fn ident(&mut self, name: Symbol, loc: Loc) -> R {
        match self.lookup(name) {
            Some(Entry::Var(id)) => {
                let v = &mut self.vars[id.0 as usize];
                v.used = true;
                let ty = v.ty;
                match v.kind {
                    VarKind::BlockInstance(b) | VarKind::BlockMember(b, _) => self.blocks[b as usize].used = true,
                    _ => {}
                }
                if let Some(value) = self.const_value(id) {
                    return Some(self.konst(value, ty, loc));
                }
                Some(Expr { kind: H::Var(id), ty, loc })
            }
            Some(Entry::Struct(_)) => {
                let t = self.name(name);
                error!(self.diags, loc, "'{t}' is a type, not a value");
                None
            }
            Some(Entry::Funcs) => {
                let t = self.name(name);
                error!(self.diags, loc, "'{t}' is a function, not a value");
                None
            }
            None => {
                let t = self.name(name);
                error!(self.diags, loc, "'{t}' is not declared");
                None
            }
        }
    }

    // ---- Operators -------------------------------------------------------

    fn unary(&mut self, op: UnaryOp, a: &ast::Expr, loc: Loc) -> R {
        let a = self.expr(a)?;
        let b = a.ty.as_basic();
        let numeric = b.and_then(Basic::scalar).is_some_and(Scalar::is_numeric);
        match op {
            UnaryOp::Plus => {
                if !numeric {
                    error!(self.diags, loc, "unary '+' needs a number");
                    return None;
                }
                Some(a)
            }
            UnaryOp::Minus => {
                if !numeric {
                    error!(self.diags, loc, "unary '-' needs a number");
                    return None;
                }
                let ty = a.ty;
                Some(self.fold(Expr { kind: H::Unary(UnOp::Neg, Box::new(a)), ty, loc }))
            }
            UnaryOp::Not => {
                if a.ty != Type::BOOL {
                    error!(self.diags, loc, "'!' needs a bool (use not() for vectors)");
                    return None;
                }
                Some(self.fold(Expr { kind: H::Unary(UnOp::Not, Box::new(a)), ty: Type::BOOL, loc }))
            }
            UnaryOp::BitNot => {
                if self.version == Version::V100 {
                    error!(self.diags, loc, "'~' needs GLSL ES 3.00");
                    return None;
                }
                if !b.and_then(Basic::scalar).is_some_and(Scalar::is_integer) || b.is_some_and(Basic::is_matrix) {
                    error!(self.diags, loc, "'~' needs an integer");
                    return None;
                }
                let ty = a.ty;
                Some(self.fold(Expr { kind: H::Unary(UnOp::BitNot, Box::new(a)), ty, loc }))
            }
            UnaryOp::PreInc | UnaryOp::PreDec | UnaryOp::PostInc | UnaryOp::PostDec => {
                if !numeric {
                    error!(self.diags, loc, "'++' and '--' need a number");
                    return None;
                }
                if !self.check_lvalue(&a, "'++' or '--'") {
                    return None;
                }
                let ty = a.ty;
                let increment = matches!(op, UnaryOp::PreInc | UnaryOp::PostInc);
                let prefix = matches!(op, UnaryOp::PreInc | UnaryOp::PreDec);
                Some(Expr { kind: H::IncDec { target: Box::new(a), increment, prefix }, ty, loc })
            }
        }
    }

    fn binary_expr(&mut self, op: BinaryOp, a: &ast::Expr, b: &ast::Expr, loc: Loc) -> R {
        let l = self.expr(a);
        let r = self.expr(b);
        let (l, r) = (l?, r?);
        match op {
            BinaryOp::And | BinaryOp::Or => {
                if l.ty != Type::BOOL || r.ty != Type::BOOL {
                    error!(self.diags, loc, "'{}' needs bool operands", op.spelling());
                    return None;
                }
                let lop = if op == BinaryOp::And { LogicOp::And } else { LogicOp::Or };
                Some(self.fold(Expr { kind: H::Logic(lop, Box::new(l), Box::new(r)), ty: Type::BOOL, loc }))
            }
            _ => {
                let hop = hir_binop(op);
                let ty = self.binary_type(hop, l.ty, r.ty, loc, op.spelling())?;
                Some(self.fold(Expr { kind: H::Binary(hop, Box::new(l), Box::new(r)), ty, loc }))
            }
        }
    }

    /// The type of `l op r`, or an error.
    fn binary_type(&mut self, op: BinOp, lt: Type, rt: Type, loc: Loc, spelling: &str) -> Option<Type> {
        let v3 = self.version == Version::V300;
        let fail = |s: &mut Self, why: &str| {
            let (a, b) = (s.type_name(lt), s.type_name(rt));
            error!(s.diags, loc, "'{spelling}' cannot apply to '{a}' and '{b}'{why}");
            None
        };
        match op {
            BinOp::Eq | BinOp::Ne => {
                if lt != rt {
                    return fail(self, "");
                }
                if lt.contains_sampler(&self.structs) || lt.is_void() {
                    return fail(self, ": samplers and void cannot be compared");
                }
                if !v3 && lt.contains_array(&self.structs) {
                    return fail(self, ": arrays cannot be compared in GLSL ES 1.00");
                }
                return Some(Type::BOOL);
            }
            BinOp::Xor => {
                if lt != Type::BOOL || rt != Type::BOOL {
                    return fail(self, "");
                }
                return Some(Type::BOOL);
            }
            _ => {}
        }
        let (Some(lb), Some(rb)) = (lt.as_basic(), rt.as_basic()) else { return fail(self, "") };
        let (Some(ls), Some(rs)) = (lb.scalar(), rb.scalar()) else { return fail(self, "") };
        if ls == Scalar::Bool || rs == Scalar::Bool {
            return fail(self, "");
        }
        match op {
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                if lb.is_scalar() && lt == rt {
                    Some(Type::BOOL)
                } else {
                    fail(self, ": relational operators compare scalars (use lessThan() for vectors)")
                }
            }
            BinOp::Shl | BinOp::Shr => {
                if !v3 {
                    return fail(self, ": shifts need GLSL ES 3.00");
                }
                if !ls.is_integer() || !rs.is_integer() {
                    return fail(self, ": shifts need integers");
                }
                match (lb, rb) {
                    (Basic::Scalar(_), Basic::Scalar(_)) => Some(lt),
                    (Basic::Vector(_, _), Basic::Scalar(_)) => Some(lt),
                    (Basic::Vector(_, n), Basic::Vector(_, m)) if n == m => Some(lt),
                    _ => fail(self, ""),
                }
            }
            BinOp::Mod | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                if !v3 {
                    let what = if op == BinOp::Mod { "'%'" } else { "bitwise operators" };
                    return fail(self, &alloc::format!(": {what} need GLSL ES 3.00"));
                }
                if !ls.is_integer() || ls != rs {
                    return fail(self, ": these operators need integers of the same signedness");
                }
                broadcast(lb, rb, lt, rt).map_or_else(|| fail(self, ""), Some)
            }
            BinOp::Add | BinOp::Sub | BinOp::Div | BinOp::Mul => {
                if ls != rs {
                    return fail(self, ": there are no implicit conversions");
                }
                match (op, lb, rb) {
                    (BinOp::Mul, Basic::Matrix(c, r), Basic::Vector(_, n)) => {
                        if n == c {
                            Some(Type::vector(Scalar::Float, r))
                        } else {
                            fail(self, "")
                        }
                    }
                    (BinOp::Mul, Basic::Vector(_, n), Basic::Matrix(c, r)) => {
                        if n == r {
                            Some(Type::vector(Scalar::Float, c))
                        } else {
                            fail(self, "")
                        }
                    }
                    (BinOp::Mul, Basic::Matrix(k, r), Basic::Matrix(c, k2)) => {
                        if k == k2 {
                            Some(Type::basic(Basic::Matrix(c, r)))
                        } else {
                            fail(self, "")
                        }
                    }
                    (_, Basic::Matrix(..), Basic::Vector(..)) | (_, Basic::Vector(..), Basic::Matrix(..)) => {
                        fail(self, "")
                    }
                    (_, Basic::Matrix(..), Basic::Matrix(..)) => {
                        if lt == rt {
                            Some(lt)
                        } else {
                            fail(self, "")
                        }
                    }
                    _ => broadcast(lb, rb, lt, rt).map_or_else(|| fail(self, ""), Some),
                }
            }
            _ => fail(self, ""),
        }
    }

    fn assign(&mut self, op: Option<BinaryOp>, a: &ast::Expr, b: &ast::Expr, loc: Loc) -> R {
        let r = self.expr(b);
        let l = self.expr(a)?;
        let r = r?;
        if !self.check_lvalue(&l, "assignment") {
            return None;
        }
        let ty = l.ty;
        match op {
            None => {
                if l.ty != r.ty {
                    let (x, y) = (self.type_name(l.ty), self.type_name(r.ty));
                    error!(self.diags, loc, "cannot assign a '{y}' to a '{x}'");
                    return None;
                }
                if l.ty.contains_sampler(&self.structs) {
                    error!(self.diags, loc, "samplers cannot be assigned");
                    return None;
                }
                if self.version == Version::V100 && l.ty.contains_array(&self.structs) {
                    error!(self.diags, loc, "arrays cannot be assigned in GLSL ES 1.00");
                    return None;
                }
                Some(Expr { kind: H::Assign(Box::new(l), Box::new(r)), ty, loc })
            }
            Some(o) => {
                let hop = hir_binop(o);
                let spelling = alloc::format!("{}=", o.spelling());
                let result = self.binary_type(hop, l.ty, r.ty, loc, &spelling)?;
                if result != l.ty {
                    let (x, y) = (self.type_name(l.ty), self.type_name(r.ty));
                    error!(self.diags, loc, "'{spelling}' of a '{x}' by a '{y}' does not give a '{x}'");
                    return None;
                }
                Some(Expr { kind: H::CompoundAssign(hop, Box::new(l), Box::new(r)), ty, loc })
            }
        }
    }

    fn ternary(&mut self, c: &ast::Expr, a: &ast::Expr, b: &ast::Expr, loc: Loc) -> R {
        let c = self.expr(c);
        let a = self.expr(a);
        let b = self.expr(b);
        let (c, a, b) = (c?, a?, b?);
        if c.ty != Type::BOOL {
            error!(self.diags, c.loc, "the condition of '?:' must be a bool");
            return None;
        }
        if a.ty != b.ty {
            let (x, y) = (self.type_name(a.ty), self.type_name(b.ty));
            error!(self.diags, loc, "the branches of '?:' have different types ('{x}' and '{y}')");
            return None;
        }
        if self.version == Version::V100 && a.ty.is_array() {
            error!(self.diags, loc, "'?:' cannot select arrays in GLSL ES 1.00");
            return None;
        }
        if a.ty.contains_sampler(&self.structs) {
            error!(self.diags, loc, "'?:' cannot select samplers");
            return None;
        }
        let ty = a.ty;
        Some(self.fold(Expr { kind: H::Ternary(Box::new(c), Box::new(a), Box::new(b)), ty, loc }))
    }

    fn index(&mut self, a: &ast::Expr, i: &ast::Expr, loc: Loc) -> R {
        let base = self.expr(a);
        let idx = self.expr(i);
        let (base, idx) = (base?, idx?);
        let int_ok = match idx.ty.as_basic() {
            Some(Basic::Scalar(Scalar::Int)) => true,
            Some(Basic::Scalar(Scalar::Uint)) => self.version == Version::V300,
            _ => false,
        };
        if !int_ok {
            error!(self.diags, idx.loc, "an index must be an integer scalar");
            return None;
        }
        let (ty, len) = match (base.ty.array, base.ty.element) {
            (Some(n), _) => (base.ty.element_type(), n),
            (None, Element::Basic(Basic::Vector(s, n))) => (Type::basic(Basic::Scalar(s)), n as u32),
            (None, Element::Basic(Basic::Matrix(c, r))) => (Type::vector(Scalar::Float, r), c as u32),
            _ => {
                let t = self.type_name(base.ty);
                error!(self.diags, loc, "a '{t}' cannot be indexed");
                return None;
            }
        };
        if let Some(v) = idx.constant() {
            let value = match idx.ty.as_basic() {
                Some(Basic::Scalar(Scalar::Uint)) => i64::from(v[0].bits()),
                _ => i64::from(v[0].bits() as i32),
            };
            if value < 0 || value >= i64::from(len) {
                error!(self.diags, idx.loc, "index {value} is out of range (0 to {})", len - 1);
                return None;
            }
        } else if base.ty.is_array() && ty.contains_sampler(&self.structs) && self.version == Version::V300 {
            error!(self.diags, idx.loc, "arrays of samplers can only be indexed with constant expressions");
            return None;
        }
        Some(self.fold(Expr { kind: H::Index(Box::new(base), Box::new(idx)), ty, loc }))
    }

    fn field(&mut self, a: &ast::Expr, f: Symbol, loc: Loc) -> R {
        let base = self.expr(a)?;
        if base.ty.is_array() {
            error!(self.diags, loc, "an array has no members (only length())");
            return None;
        }
        match base.ty.element {
            Element::Struct(id) => {
                let def = self.structs.get(id);
                match def.fields.iter().position(|x| x.name == f) {
                    Some(i) => {
                        let ty = def.fields[i].ty;
                        Some(self.fold(Expr { kind: H::Field(Box::new(base), i as u32), ty, loc }))
                    }
                    None => {
                        let n = self.name(f);
                        let t = self.type_name(base.ty);
                        error!(self.diags, loc, "'{t}' has no member '{n}'");
                        None
                    }
                }
            }
            Element::Basic(Basic::Vector(s, n)) => {
                let text = self.name(f);
                let Some(swizzle) = parse_swizzle(&text, n) else {
                    error!(self.diags, loc, "invalid swizzle '.{text}' of a {}-component vector", n);
                    return None;
                };
                let ty = Type::vector(s, swizzle.len);
                Some(self.fold(Expr { kind: H::Swizzle(Box::new(base), swizzle), ty, loc }))
            }
            _ => {
                let n = self.name(f);
                let t = self.type_name(base.ty);
                error!(self.diags, loc, "'{t}' has no member '{n}'");
                None
            }
        }
    }

    /// Whether `e` can be assigned to; reports why not.
    pub(super) fn check_lvalue(&mut self, e: &Expr, what: &str) -> bool {
        match &e.kind {
            H::Var(id) => {
                let v = &self.vars[id.0 as usize];
                let ok = match v.kind {
                    VarKind::Local | VarKind::Global | VarKind::Output => true,
                    VarKind::Param(_) => !v.read_only,
                    VarKind::Builtin(b) => b.is_output(),
                    _ => false,
                };
                if !ok {
                    let n = self.name(v.name);
                    let why = match v.kind {
                        VarKind::Const => "a constant",
                        VarKind::Uniform | VarKind::BlockInstance(_) | VarKind::BlockMember(..) => "a uniform",
                        VarKind::Input | VarKind::Builtin(_) => "a shader input",
                        _ => "read-only",
                    };
                    error!(self.diags, e.loc, "{what}: '{n}' is {why}");
                }
                ok
            }
            H::Index(b, _) | H::Field(b, _) => self.check_lvalue(b, what),
            H::Swizzle(b, s) => {
                let c = s.components();
                if (1..c.len()).any(|i| c[..i].contains(&c[i])) {
                    error!(self.diags, e.loc, "{what}: a swizzle with repeated components cannot be assigned");
                    return false;
                }
                self.check_lvalue(b, what)
            }
            H::Const(_) => {
                error!(self.diags, e.loc, "{what}: a constant cannot be assigned");
                false
            }
            _ => {
                error!(self.diags, e.loc, "{what}: the expression cannot be assigned");
                false
            }
        }
    }

    // ---- Calls and constructors ------------------------------------------

    fn call(&mut self, callee: &Callee, args: &[ast::Expr], loc: Loc) -> R {
        // Check the arguments first (an error in one is reported, and the
        // call is dropped).
        let mut checked = Vec::with_capacity(args.len());
        let mut failed = false;
        for a in args {
            match self.expr(a) {
                Some(h) => checked.push(h),
                None => failed = true,
            }
        }
        match callee {
            Callee::Type(spec) => {
                let ty = self.resolve_type(spec, false)?;
                if failed {
                    return None;
                }
                self.construct(ty, checked, loc)
            }
            Callee::NamedArray(name, size) => {
                let Some(Entry::Struct(id)) = self.lookup(*name) else {
                    let t = self.name(*name);
                    error!(self.diags, loc, "'{t}' is not a type");
                    return None;
                };
                if self.version == Version::V100 {
                    error!(self.diags, loc, "array constructors need GLSL ES 3.00");
                    return None;
                }
                let n = match size {
                    Some(e) => self.array_size(e)?,
                    None => 0,
                };
                if failed {
                    return None;
                }
                self.construct(Type::structure(id).array_of(n), checked, loc)
            }
            Callee::Name(name) => {
                match self.lookup(*name) {
                    Some(Entry::Struct(id)) => {
                        if failed {
                            return None;
                        }
                        return self.construct(Type::structure(id), checked, loc);
                    }
                    Some(Entry::Var(_)) => {
                        let t = self.name(*name);
                        error!(self.diags, loc, "'{t}' is not a function");
                        return None;
                    }
                    _ => {}
                }
                if failed {
                    return None;
                }
                self.function_call(*name, checked, loc)
            }
        }
    }

    fn function_call(&mut self, name: Symbol, args: Vec<Expr>, loc: Loc) -> R {
        let types: Vec<Type> = args.iter().map(|a| a.ty).collect();
        // A user function with exactly these parameter types.
        let user = self.func_names.get(&name).and_then(|ids| {
            ids.iter().copied().find(|id| {
                let f = &self.functions[id.0 as usize];
                f.params.len() == types.len()
                    && f.params.iter().zip(&types).all(|(v, t)| self.vars[v.0 as usize].ty == *t)
            })
        });
        if let Some(id) = user {
            return self.user_call(id, args, loc);
        }
        let text = self.name(name);
        let ctx = builtins::CallContext { stage: self.stage, version: self.version, enabled: self.enabled };
        match builtins::resolve(&text, &types, &ctx) {
            Ok(r) => self.builtin_call(r, args, loc),
            Err(CallError::Unavailable(why)) => {
                error!(self.diags, loc, "{why}");
                None
            }
            Err(e) => {
                let list: Vec<String> = types.iter().map(|t| self.type_name(*t)).collect();
                let sig = alloc::format!("{text}({})", list.join(", "));
                if matches!(e, CallError::NoSuchFunction) && !self.func_names.contains_key(&name) {
                    error!(self.diags, loc, "no function named '{text}'");
                } else {
                    error!(self.diags, loc, "no matching overload for '{sig}'");
                }
                None
            }
        }
    }

    fn user_call(&mut self, id: FuncId, args: Vec<Expr>, loc: Loc) -> R {
        let f = &self.functions[id.0 as usize];
        let ret = f.ret;
        let modes: Vec<VarKind> = f.params.iter().map(|p| self.vars[p.0 as usize].kind).collect();
        for (a, m) in args.iter().zip(&modes) {
            if matches!(m, VarKind::Param(hir::ParamMode::Out | hir::ParamMode::InOut))
                && !self.check_lvalue(a, "an out or inout argument")
            {
                return None;
            }
        }
        self.record_call(id);
        Some(Expr { kind: H::Call(id, args), ty: ret, loc })
    }

    fn builtin_call(&mut self, r: builtins::Resolved, args: Vec<Expr>, loc: Loc) -> R {
        for &o in &r.outputs {
            if !self.check_lvalue(&args[o], "an out argument") {
                return None;
            }
        }
        if let Builtin::Texture(t) = r.builtin
            && t.offset
        {
            // The offset is last in the call (the order puts it last too).
            let off = r.order.last().copied().unwrap_or(0);
            let a = &args[off];
            match a.constant() {
                Some(v) => {
                    let (lo, hi) = (self.limits.min_program_texel_offset, self.limits.max_program_texel_offset);
                    if v.iter().any(|c| !(lo..=hi).contains(&(c.bits() as i32))) {
                        error!(self.diags, a.loc, "texel offsets must be between {lo} and {hi}");
                        return None;
                    }
                }
                None => {
                    error!(self.diags, a.loc, "a texel offset must be a constant expression");
                    return None;
                }
            }
        }
        if matches!(r.builtin, Builtin::DFdx | Builtin::DFdy | Builtin::Fwidth) && self.stage != Stage::Fragment {
            error!(self.diags, loc, "derivatives are only available in fragment shaders");
            return None;
        }
        let mut ordered: Vec<Option<Expr>> = args.into_iter().map(Some).collect();
        let args: Vec<Expr> = r.order.iter().filter_map(|&i| ordered.get_mut(i).and_then(Option::take)).collect();
        let e = Expr { kind: H::Builtin(r.builtin, args), ty: r.ret, loc };
        Some(self.fold(e))
    }

    /// A constructor call.
    pub(super) fn construct(&mut self, ty: Type, args: Vec<Expr>, loc: Loc) -> R {
        let v3 = self.version == Version::V300;
        if args.is_empty() {
            error!(self.diags, loc, "a constructor needs arguments");
            return None;
        }
        if ty.is_void() {
            error!(self.diags, loc, "void cannot be constructed");
            return None;
        }
        if ty.contains_sampler(&self.structs) {
            error!(self.diags, loc, "samplers cannot be constructed");
            return None;
        }
        if let Some(n) = ty.array {
            if !v3 {
                error!(self.diags, loc, "array constructors need GLSL ES 3.00");
                return None;
            }
            let elem = ty.element_type();
            for a in &args {
                if a.ty != elem {
                    let (x, y) = (self.type_name(elem), self.type_name(a.ty));
                    error!(self.diags, a.loc, "an array of '{x}' cannot have an element of type '{y}'");
                    return None;
                }
            }
            let len = args.len() as u32;
            if n != 0 && n != len {
                error!(self.diags, loc, "an array of {n} elements is constructed from {len} values");
                return None;
            }
            let ty = elem.array_of(len);
            return Some(self.fold(Expr { kind: H::Construct(args), ty, loc }));
        }
        if let Some(id) = ty.as_struct() {
            let fields: Vec<Type> = self.structs.get(id).fields.iter().map(|f| f.ty).collect();
            if args.len() != fields.len() {
                let t = self.type_name(ty);
                error!(self.diags, loc, "'{t}' has {} members, {} values given", fields.len(), args.len());
                return None;
            }
            for (a, f) in args.iter().zip(&fields) {
                if a.ty != *f {
                    let (x, y) = (self.type_name(*f), self.type_name(a.ty));
                    error!(self.diags, a.loc, "a member of type '{x}' cannot take a '{y}'");
                    return None;
                }
                if !v3 && a.ty.contains_array(&self.structs) {
                    error!(self.diags, a.loc, "structures containing arrays cannot be constructed in GLSL ES 1.00");
                    return None;
                }
            }
            return Some(self.fold(Expr { kind: H::Construct(args), ty, loc }));
        }
        let target = ty.as_basic()?;
        let needed = target.components();
        for a in &args {
            let ok = a.ty.as_basic().is_some_and(|b| b.scalar().is_some());
            if !ok {
                let t = self.type_name(a.ty);
                error!(self.diags, a.loc, "a '{t}' cannot be used to construct a '{}'", target.name());
                return None;
            }
        }
        let single = args.len() == 1;
        if target.is_matrix() && !single && args.iter().any(|a| a.ty.as_basic().is_some_and(Basic::is_matrix)) {
            error!(self.diags, loc, "a matrix argument must be the only argument of a matrix constructor");
            return None;
        }
        if !(single
            && (args[0].ty.as_basic().is_some_and(Basic::is_scalar)
                || target.is_matrix() && args[0].ty.as_basic().is_some_and(Basic::is_matrix)))
        {
            let mut used = 0u32;
            for a in &args {
                if used >= needed {
                    error!(self.diags, a.loc, "too many arguments to the '{}' constructor", target.name());
                    return None;
                }
                used += a.ty.as_basic().map_or(0, Basic::components);
            }
            if used < needed && !target.is_scalar() {
                error!(self.diags, loc, "not enough values to construct a '{}'", target.name());
                return None;
            }
        }
        Some(self.fold(Expr { kind: H::Construct(args), ty, loc }))
    }

    // ---- Constant folding ------------------------------------------------

    /// Replaces `e` by its value if all its operands are constant and the
    /// operation can be folded.
    pub(super) fn fold(&mut self, e: Expr) -> Expr {
        match self.fold_value(&e) {
            Some(v) => Expr { kind: H::Const(v), ty: e.ty, loc: e.loc },
            None => e,
        }
    }

    fn fold_value(&mut self, e: &Expr) -> Option<ConstValue> {
        let c = |x: &Expr| x.constant().cloned();
        let mut emit = ConstEmit;
        match &e.kind {
            H::Unary(op, a) => Some(lower::unary(&mut emit, *op, a.ty, &c(a)?)),
            H::Binary(op, a, b) => {
                let (x, y) = (c(a)?, c(b)?);
                // Integer division by zero in a constant expression is
                // undefined: leave it to run time (and report nothing).
                Some(lower::binary(&mut emit, *op, a.ty, &x, b.ty, &y, &self.structs))
            }
            H::Logic(op, a, b) => {
                let (x, y) = (c(a)?, c(b)?);
                let (p, q) = (x[0].bits() != 0, y[0].bits() != 0);
                Some(alloc::vec![ops::Value::B(match op {
                    LogicOp::And => p && q,
                    LogicOp::Or => p || q,
                })])
            }
            H::Ternary(cond, a, b) => {
                let k = c(cond)?;
                let (x, y) = (c(a)?, c(b)?);
                Some(if k[0].bits() != 0 { x } else { y })
            }
            H::Index(base, idx) => {
                let (v, i) = (c(base)?, c(idx)?);
                let i = i[0].bits() as usize;
                let size = e.ty.flat_components(&self.structs) as usize;
                v.get(i * size..(i + 1) * size).map(<[ops::Value]>::to_vec)
            }
            H::Swizzle(base, s) => {
                let v = c(base)?;
                s.components().iter().map(|&i| v.get(i as usize).copied()).collect()
            }
            H::Field(base, i) => {
                let v = c(base)?;
                let id = base.ty.as_struct()?;
                let fields = &self.structs.get(id).fields;
                let start: u32 = fields[..*i as usize].iter().map(|f| f.ty.flat_components(&self.structs)).sum();
                let len = fields[*i as usize].ty.flat_components(&self.structs);
                v.get(start as usize..(start + len) as usize).map(<[ops::Value]>::to_vec)
            }
            H::Construct(args) => {
                let values: Option<Vec<(Type, ConstValue)>> = args.iter().map(|a| Some((a.ty, c(a)?))).collect();
                Some(lower::construct(&mut emit, e.ty, &values?, &self.structs))
            }
            H::Builtin(b, args) if b.constant_foldable() => {
                let values: Option<Vec<(Type, ConstValue)>> = args.iter().map(|a| Some((a.ty, c(a)?))).collect();
                let (v, out) = lower::builtin(&mut emit, *b, &values?, e.ty);
                // modf writes its second argument: not a constant expression.
                if out.is_some() { None } else { Some(v) }
            }
            _ => None,
        }
    }
}

/// The type of a component-wise operation with broadcasting, if the
/// shapes agree: same types, or a scalar with a vector or matrix.
fn broadcast(lb: Basic, rb: Basic, lt: Type, rt: Type) -> Option<Type> {
    match (lb, rb) {
        _ if lt == rt => Some(lt),
        (Basic::Scalar(_), Basic::Vector(..) | Basic::Matrix(..)) => Some(rt),
        (Basic::Vector(..) | Basic::Matrix(..), Basic::Scalar(_)) => Some(lt),
        _ => None,
    }
}

fn hir_binop(op: BinaryOp) -> BinOp {
    match op {
        BinaryOp::Add => BinOp::Add,
        BinaryOp::Sub => BinOp::Sub,
        BinaryOp::Mul => BinOp::Mul,
        BinaryOp::Div => BinOp::Div,
        BinaryOp::Mod => BinOp::Mod,
        BinaryOp::Shl => BinOp::Shl,
        BinaryOp::Shr => BinOp::Shr,
        BinaryOp::Lt => BinOp::Lt,
        BinaryOp::Gt => BinOp::Gt,
        BinaryOp::Le => BinOp::Le,
        BinaryOp::Ge => BinOp::Ge,
        BinaryOp::Eq => BinOp::Eq,
        BinaryOp::Ne => BinOp::Ne,
        BinaryOp::BitAnd => BinOp::BitAnd,
        BinaryOp::BitXor => BinOp::BitXor,
        BinaryOp::BitOr => BinOp::BitOr,
        BinaryOp::Xor => BinOp::Xor,
        // && and || are handled before.
        BinaryOp::And | BinaryOp::Or => BinOp::Xor,
    }
}

/// Parses a swizzle (`xyz`, `rgba`, `stp`...) of a vector of `n`
/// components: one to four components of one set, each inside the vector.
pub(crate) fn parse_swizzle(text: &str, n: u8) -> Option<Swizzle> {
    const SETS: [&[u8; 4]; 3] = [b"xyzw", b"rgba", b"stpq"];
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let set = SETS.iter().find(|s| s.contains(&bytes[0]))?;
    let mut comps = [0u8; 4];
    for (i, b) in bytes.iter().enumerate() {
        let c = set.iter().position(|x| x == b)? as u8;
        if c >= n {
            return None;
        }
        comps[i] = c;
    }
    Some(Swizzle { comps, len: bytes.len() as u8 })
}
