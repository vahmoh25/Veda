//! The semantic checks: from the syntax tree to the checked shader.
//!
//! Names are resolved through nested scopes (GLSL ES's rules: a function's
//! parameters and the top level of its body share one scope, and so do a
//! loop's header and body); types are checked with no implicit conversions;
//! qualifiers are checked against the version, the stage and where they
//! appear, in the order the grammar of each version requires; precision
//! qualifiers are resolved from the defaults in scope; and constant
//! expressions are folded as they are built (see [`crate::lower`]).
//!
//! Each error is reported once, at the construct it concerns, and checking
//! goes on; an expression in error produces no value, so that one mistake
//! does not cascade into many.

mod expr;
mod stmt;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{self, Interp, LayoutId, QualKind, Storage};
use crate::builtins::{self, BuiltinVar, Limits};
use crate::diag::{Diagnostics, Loc, error, warning};
use crate::hir::{
    self, BlockLayout, BlockMember, ConstValue, FuncId, Function, Shader, UniformBlock, Var, VarId, VarKind,
};
use crate::intern::{Interner, Symbol};
use crate::ops;
use crate::pp::{Ext, ExtSet};
use crate::types::{Basic, Dim, Element, Field, Precision, Sampler, Scalar, StructDef, StructId, Structs, Type};
use crate::{Stage, Version};

/// What a name stands for in a scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    Var(VarId),
    Struct(StructId),
    /// User functions with this name (global scope only).
    Funcs,
}

/// A precision default's key: `float`, `int` or a sampler type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PrecKey {
    Float,
    Int,
    Sampler(Sampler),
}

#[derive(Default)]
struct Scope {
    names: BTreeMap<Symbol, Entry>,
    precisions: Vec<(PrecKey, Precision)>,
}

/// A parameter as declared: name, type, direction, `const`, precision
/// and location.
type ParamInfo = (Option<Symbol>, Type, hir::ParamMode, bool, Option<Precision>, Loc);

/// The function being checked.
struct FnCtx {
    ret: Type,
    /// Enclosing loops.
    loops: u32,
    /// Enclosing switch statements.
    switches: u32,
    /// Calls made (for recursion detection).
    calls: Vec<FuncId>,
}

/// Where qualifiers appear.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QualCtx {
    GlobalVar,
    LocalVar,
    Param,
    StructMember,
    BlockMember,
    Block,
    Defaults,
    FunctionReturn,
}

/// Checked qualifiers.
#[derive(Clone, Copy, Debug, Default)]
struct Quals {
    storage: Option<Storage>,
    constant: bool,
    interp: Option<Interp>,
    centroid: bool,
    invariant: bool,
    precision: Option<Precision>,
    location: Option<u32>,
    layout: Option<BlockLayout>,
    row_major: Option<bool>,
}

/// The semantic checker's state.
pub(crate) struct Sema<'d> {
    stage: Stage,
    version: Version,
    enabled: ExtSet,
    limits: Limits,
    interner: Interner,
    diags: &'d mut Diagnostics,
    structs: Structs,
    vars: Vec<Var>,
    functions: Vec<Function>,
    func_names: BTreeMap<Symbol, Vec<FuncId>>,
    blocks: Vec<UniformBlock>,
    scopes: Vec<Scope>,
    init: Vec<hir::Stmt>,
    func: Option<FnCtx>,
    discards: bool,
    invariant_all: bool,
    default_layout: BlockLayout,
    default_row_major: bool,
    /// Functions called by global initialisers.
    init_calls: Vec<FuncId>,
}

/// Checks a parsed shader.
pub(crate) fn check(
    unit: &ast::TranslationUnit,
    stage: Stage,
    version: Version,
    enabled: ExtSet,
    invariant_all: bool,
    limits: &Limits,
    interner: Interner,
    diags: &mut Diagnostics,
) -> Shader {
    let mut s = Sema {
        stage,
        version,
        enabled,
        limits: *limits,
        interner,
        diags,
        structs: Structs::default(),
        vars: Vec::new(),
        functions: Vec::new(),
        func_names: BTreeMap::new(),
        blocks: Vec::new(),
        scopes: vec![Scope::default()],
        init: Vec::new(),
        func: None,
        discards: false,
        invariant_all,
        default_layout: BlockLayout::Shared,
        default_row_major: false,
        init_calls: Vec::new(),
    };
    s.predeclare();
    for item in &unit.items {
        if s.diags.saturated() {
            break;
        }
        match item {
            ast::External::Declaration(d) => s.global_declaration(d),
            ast::External::Function(f) => s.function_definition(f),
        }
    }
    s.finish();
    let main = s.interner.lookup("main").and_then(|m| s.func_names.get(&m)).and_then(|f| f.first().copied());
    Shader {
        stage,
        version,
        interner: s.interner,
        structs: s.structs,
        vars: s.vars,
        functions: s.functions,
        main,
        init: s.init,
        blocks: s.blocks,
        invariant_all: s.invariant_all,
        discards: s.discards,
    }
}

impl Sema<'_> {
    fn sym(&mut self, s: &str) -> Symbol {
        self.interner.intern(s)
    }

    fn name(&self, s: Symbol) -> String {
        self.interner.get(s).into()
    }

    fn type_name(&self, t: Type) -> String {
        let names = |s: Symbol| -> String { self.interner.get(s).into() };
        alloc::format!("{}", t.display(&self.structs, &names))
    }

    // ---- Scopes ----------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn lookup(&self, name: Symbol) -> Option<Entry> {
        self.scopes.iter().rev().find_map(|s| s.names.get(&name).copied())
    }

    /// Declares `name` in the current scope; an error if it is already
    /// declared there.
    fn declare(&mut self, name: Symbol, entry: Entry, loc: Loc) -> bool {
        let text = self.interner.get(name);
        if text.starts_with("gl_") {
            let t = String::from(text);
            error!(self.diags, loc, "'{t}': names beginning with 'gl_' are reserved");
            return false;
        }
        let global = self.scopes.len() == 1;
        let scope = self.scopes.last_mut().expect("a scope");
        if let Some(old) = scope.names.get(&name) {
            // Several prototypes and one definition of a function share a
            // name.
            if !(*old == Entry::Funcs && entry == Entry::Funcs) {
                let t = String::from(self.interner.get(name));
                error!(self.diags, loc, "'{t}' redeclared");
                return false;
            }
        }
        if global && entry != Entry::Funcs && builtins::is_builtin_name(self.interner.get(name), self.version) {
            // A variable or structure may hide a built-in function in a
            // nested scope only.
            let t = String::from(self.interner.get(name));
            error!(self.diags, loc, "'{t}' is the name of a built-in function");
            return false;
        }
        scope.names.insert(name, entry);
        true
    }

    fn new_var(&mut self, var: Var) -> VarId {
        self.vars.push(var);
        VarId(self.vars.len() as u32 - 1)
    }

    fn plain_var(&self, name: Symbol, ty: Type, kind: VarKind, precision: Option<Precision>, loc: Loc) -> Var {
        Var {
            name,
            ty,
            kind,
            precision,
            interp: Interp::Smooth,
            centroid: false,
            invariant: false,
            location: None,
            value: None,
            loc,
            used: false,
            read_only: false,
        }
    }

    // ---- Built-ins -------------------------------------------------------

    fn predeclare(&mut self) {
        // Default precisions (GLSL ES 4.5.3 / 4.5.4).
        let lowp_samplers =
            [Sampler::new(Dim::D2, false, Scalar::Float), Sampler::new(Dim::Cube, false, Scalar::Float)];
        let scope = &mut self.scopes[0];
        match self.stage {
            Stage::Vertex => {
                scope.precisions.push((PrecKey::Float, Precision::High));
                scope.precisions.push((PrecKey::Int, Precision::High));
            }
            Stage::Fragment => scope.precisions.push((PrecKey::Int, Precision::Medium)),
        }
        for s in lowp_samplers {
            scope.precisions.push((PrecKey::Sampler(s), Precision::Low));
        }
        // GL_OES_texture_3D and GL_EXT_shadow_samplers give their samplers
        // lowp defaults in GLSL ES 1.00.
        if self.version == Version::V100 {
            if self.enabled.contains(Ext::OesTexture3D) {
                let s = Sampler::new(Dim::D3, false, Scalar::Float);
                self.scopes[0].precisions.push((PrecKey::Sampler(s), Precision::Low));
            }
            if self.enabled.contains(Ext::ExtShadowSamplers) {
                let s = Sampler::new(Dim::D2, true, Scalar::Float);
                self.scopes[0].precisions.push((PrecKey::Sampler(s), Precision::Low));
            }
        }
        // Constants.
        for (name, value) in builtins::constants(self.version, &self.limits, self.enabled) {
            let sym = self.sym(name);
            let mut v = self.plain_var(sym, Type::INT, VarKind::Const, Some(Precision::Medium), Loc::NONE);
            v.value = Some(vec![ops::Value::I(value)]);
            let id = self.new_var(v);
            self.scopes[0].names.insert(sym, Entry::Var(id));
        }
        // Inputs and outputs.
        for d in builtins::variables(self.stage, self.version, &self.limits, self.enabled) {
            let sym = self.sym(d.name);
            let precision = match d.var {
                BuiltinVar::FrontFacing => None,
                BuiltinVar::PointCoord | BuiltinVar::FragColor | BuiltinVar::FragData => Some(Precision::Medium),
                BuiltinVar::PointSize if self.version == Version::V100 => Some(Precision::Medium),
                _ => Some(Precision::High),
            };
            let v = self.plain_var(sym, d.ty, VarKind::Builtin(d.var), precision, Loc::NONE);
            let id = self.new_var(v);
            self.scopes[0].names.insert(sym, Entry::Var(id));
        }
        // uniform gl_DepthRangeParameters gl_DepthRange;
        let fields = ["near", "far", "diff"]
            .into_iter()
            .map(|n| Field { name: self.interner.intern(n), ty: Type::FLOAT, precision: Some(Precision::High) })
            .collect();
        let struct_name = self.sym("gl_DepthRangeParameters");
        let id = self.structs.add(StructDef { name: Some(struct_name), fields });
        self.scopes[0].names.insert(struct_name, Entry::Struct(id));
        let sym = self.sym("gl_DepthRange");
        let v = self.plain_var(sym, Type::structure(id), VarKind::Uniform, None, Loc::NONE);
        let vid = self.new_var(v);
        self.scopes[0].names.insert(sym, Entry::Var(vid));
    }

    // ---- Precision -------------------------------------------------------

    fn precision_key(t: Type) -> Option<PrecKey> {
        match t.element {
            Element::Basic(b) => match b {
                Basic::Sampler(s) => Some(PrecKey::Sampler(s)),
                _ => match b.scalar()? {
                    Scalar::Float => Some(PrecKey::Float),
                    Scalar::Int | Scalar::Uint => Some(PrecKey::Int),
                    Scalar::Bool => None,
                },
            },
            Element::Struct(_) => None,
        }
    }

    fn default_precision(&self, key: PrecKey) -> Option<Precision> {
        self.scopes.iter().rev().find_map(|s| s.precisions.iter().rev().find(|(k, _)| *k == key).map(|(_, p)| *p))
    }

    /// The precision of a declaration of type `t` with `explicit` precision;
    /// reports a missing default.
    fn resolve_precision(&mut self, t: Type, explicit: Option<Precision>, loc: Loc) -> Option<Precision> {
        let key = Self::precision_key(t)?;
        if explicit.is_some() {
            return explicit;
        }
        let p = self.default_precision(key);
        if p.is_none() {
            let what = match key {
                PrecKey::Float => "float".into(),
                PrecKey::Int => "int".into(),
                PrecKey::Sampler(s) => String::from(s.name()),
            };
            error!(self.diags, loc, "no precision specified for '{what}' (declare one, or a default with 'precision')");
        }
        p
    }

    fn precision_statement(&mut self, precision: Precision, spec: &ast::TypeSpec, loc: Loc) {
        let ok = match (&spec.name, &spec.array) {
            (ast::TypeName::Basic(b), None) => match b {
                Basic::Scalar(Scalar::Float) => Some(PrecKey::Float),
                Basic::Scalar(Scalar::Int) => Some(PrecKey::Int),
                Basic::Sampler(s) => {
                    let allowed = self.version == Version::V300
                        || (s.dim == Dim::D2 || s.dim == Dim::Cube) && !s.shadow
                        || (s.dim == Dim::D3 && self.enabled.contains(Ext::OesTexture3D))
                        || (s.shadow && self.enabled.contains(Ext::ExtShadowSamplers));
                    if allowed { Some(PrecKey::Sampler(*s)) } else { None }
                }
                _ => None,
            },
            _ => None,
        };
        match ok {
            Some(key) => self.scopes.last_mut().expect("a scope").precisions.push((key, precision)),
            None => error!(self.diags, loc, "a default precision can only be given for float, int or a sampler type"),
        }
    }

    // ---- Qualifiers ------------------------------------------------------

    fn check_quals(&mut self, q: &ast::Qualifiers, ctx: QualCtx) -> Quals {
        let mut out = Quals {
            storage: q.storage,
            constant: q.constant,
            interp: q.interp,
            centroid: q.centroid,
            invariant: q.invariant,
            precision: q.precision,
            ..Quals::default()
        };
        let v3 = self.version == Version::V300;
        let fs = self.stage == Stage::Fragment;
        // Order: layout, invariant, interpolation, centroid, storage,
        // precision.
        let rank = |k: QualKind| match k {
            QualKind::Layout => 0,
            QualKind::Invariant => 1,
            QualKind::Interp => 2,
            QualKind::Centroid => 3,
            QualKind::Storage => 4,
            QualKind::Precision => 5,
        };
        let mut last = 0;
        let mut layouts = 0;
        for &(k, loc) in &q.order {
            if rank(k) < last {
                error!(self.diags, loc, "qualifiers are in the wrong order");
                break;
            }
            last = rank(k);
            if k == QualKind::Layout {
                layouts += 1;
                if layouts > 1 {
                    error!(self.diags, loc, "only one layout qualifier is allowed");
                }
            }
        }
        let loc_of = |k: QualKind| q.order.iter().find(|(x, _)| *x == k).map_or(Loc::NONE, |(_, l)| *l);
        // Parameters: const only with in.
        if ctx == QualCtx::Param {
            if q.constant && matches!(q.storage, Some(Storage::Out | Storage::InOut)) {
                error!(self.diags, loc_of(QualKind::Storage), "'const' cannot qualify an out or inout parameter");
            }
            if let Some(s) = q.storage
                && !matches!(s, Storage::In | Storage::Out | Storage::InOut | Storage::Const)
            {
                error!(self.diags, loc_of(QualKind::Storage), "a parameter can only be const, in, out or inout");
            }
        } else if q.storage == Some(Storage::InOut) {
            error!(self.diags, loc_of(QualKind::Storage), "'inout' can only qualify a parameter");
        }
        let storage_ok = match ctx {
            QualCtx::GlobalVar => match q.storage {
                None | Some(Storage::Const | Storage::Uniform) => true,
                Some(Storage::Attribute) => self.stage == Stage::Vertex,
                Some(Storage::Varying) => true,
                Some(Storage::In | Storage::Out) => v3,
                Some(Storage::InOut) => false,
            },
            QualCtx::LocalVar => matches!(q.storage, None | Some(Storage::Const)),
            QualCtx::Param => true,
            QualCtx::StructMember | QualCtx::FunctionReturn | QualCtx::BlockMember => q.storage.is_none(),
            QualCtx::Block | QualCtx::Defaults => q.storage == Some(Storage::Uniform),
        };
        if !storage_ok {
            let what = match q.storage {
                Some(Storage::Attribute) if fs => "'attribute' is only allowed in vertex shaders",
                Some(Storage::In | Storage::Out) if !v3 => "'in' and 'out' only qualify parameters in GLSL ES 1.00",
                _ => match ctx {
                    QualCtx::LocalVar => "local variables can only be qualified 'const'",
                    QualCtx::StructMember => "structure members cannot have storage qualifiers",
                    QualCtx::BlockMember => "block members cannot have storage qualifiers",
                    QualCtx::FunctionReturn => "a function's return type cannot have storage qualifiers",
                    QualCtx::Block | QualCtx::Defaults => "blocks must be 'uniform' blocks",
                    _ => "this storage qualifier is not allowed here",
                },
            };
            error!(self.diags, loc_of(QualKind::Storage), "{what}");
            out.storage = None;
        }
        // Interpolation and centroid: shader outputs of the vertex shader
        // and inputs of the fragment shader.
        let interface = matches!(
            (self.stage, q.storage),
            (Stage::Vertex, Some(Storage::Out)) | (Stage::Fragment, Some(Storage::In))
        );
        if (q.interp.is_some() || q.centroid) && !(ctx == QualCtx::GlobalVar && interface) {
            let k = if q.interp.is_some() { QualKind::Interp } else { QualKind::Centroid };
            error!(
                self.diags,
                loc_of(k),
                "interpolation qualifiers only apply to vertex shader outputs and fragment shader inputs"
            );
            out.interp = None;
            out.centroid = false;
        }
        if q.invariant {
            let output = matches!(
                (self.stage, q.storage),
                (Stage::Vertex, Some(Storage::Out | Storage::Varying)) | (Stage::Fragment, Some(Storage::Varying))
            );
            if !(ctx == QualCtx::GlobalVar && output) {
                error!(self.diags, loc_of(QualKind::Invariant), "'invariant' only applies to vertex shader outputs");
                out.invariant = false;
            }
        }
        if q.precision.is_some() && matches!(ctx, QualCtx::Block | QualCtx::Defaults) {
            error!(self.diags, loc_of(QualKind::Precision), "a block cannot have a precision qualifier");
        }
        self.check_layout(&q.layout, ctx, q.storage, &mut out);
        out
    }

    fn check_layout(&mut self, ids: &[LayoutId], ctx: QualCtx, storage: Option<Storage>, out: &mut Quals) {
        for id in ids {
            let name = String::from(self.interner.get(id.name));
            let block = matches!(ctx, QualCtx::Block | QualCtx::Defaults);
            let member = ctx == QualCtx::BlockMember;
            match (name.as_str(), id.value) {
                ("location", Some(v)) => {
                    let io = ctx == QualCtx::GlobalVar
                        && matches!(
                            (self.stage, storage),
                            (Stage::Vertex, Some(Storage::In)) | (Stage::Fragment, Some(Storage::Out))
                        );
                    if !io {
                        error!(
                            self.diags,
                            id.loc, "'location' only applies to vertex shader inputs and fragment shader outputs"
                        );
                    } else if !(0..=i64::from(u32::MAX / 2)).contains(&v) {
                        error!(self.diags, id.loc, "invalid location {v}");
                    } else {
                        out.location = Some(v as u32);
                    }
                }
                ("location", None) => error!(self.diags, id.loc, "'location' needs a value: location = N"),
                ("std140" | "shared" | "packed", None) if block => {
                    out.layout = Some(match name.as_str() {
                        "std140" => BlockLayout::Std140,
                        "shared" => BlockLayout::Shared,
                        _ => BlockLayout::Packed,
                    });
                }
                ("row_major" | "column_major", None) if block || member => {
                    out.row_major = Some(name == "row_major");
                }
                ("std140" | "shared" | "packed" | "row_major" | "column_major", _) => {
                    error!(self.diags, id.loc, "layout qualifier '{name}' is not allowed here");
                }
                _ => error!(self.diags, id.loc, "unknown layout qualifier '{name}'"),
            }
        }
    }

    // ---- Types -----------------------------------------------------------

    /// Resolves a type specifier (a structure defined in it is declared in
    /// the current scope). `array` is the declarator's array size, if any.
    fn resolve_type(&mut self, spec: &ast::TypeSpec, embedded: bool) -> Option<Type> {
        let element = match &spec.name {
            ast::TypeName::Basic(b) => Type::basic(*b),
            ast::TypeName::Named(n) => match self.lookup(*n) {
                Some(Entry::Struct(id)) => Type::structure(id),
                _ => {
                    let t = self.name(*n);
                    error!(self.diags, spec.loc, "'{t}' is not a type");
                    return None;
                }
            },
            ast::TypeName::Struct(s) => {
                if embedded {
                    error!(self.diags, s.loc, "structure definitions cannot be nested in other structures");
                    return None;
                }
                Type::structure(self.struct_definition(s)?)
            }
        };
        match &spec.array {
            None => Some(element),
            Some(size) => {
                if self.version == Version::V100 {
                    error!(self.diags, spec.loc, "array types need GLSL ES 3.00 (put the size after the name)");
                    return None;
                }
                match size {
                    ast::ArraySize::Sized(e) => {
                        let n = self.array_size(e)?;
                        Some(element.array_of(n))
                    }
                    // `float[] x = ...`: sized by the initializer.
                    ast::ArraySize::Unsized => Some(element.array_of(0)),
                }
            }
        }
    }

    /// Applies a declarator's `[N]` (or `[]`) to `ty`.
    fn apply_array(&mut self, ty: Type, size: Option<&ast::ArraySize>, loc: Loc) -> Option<Type> {
        match size {
            None => Some(ty),
            Some(_) if ty.is_array() => {
                error!(self.diags, loc, "arrays of arrays are not supported");
                None
            }
            Some(ast::ArraySize::Sized(e)) => {
                let n = self.array_size(e)?;
                Some(ty.array_of(n))
            }
            Some(ast::ArraySize::Unsized) => {
                if self.version == Version::V100 {
                    error!(self.diags, loc, "unsized arrays need GLSL ES 3.00");
                    return None;
                }
                Some(ty.array_of(0))
            }
        }
    }

    /// An array size: a constant integral expression greater than zero.
    fn array_size(&mut self, e: &ast::Expr) -> Option<u32> {
        let v = self.const_int(e)?;
        if v <= 0 {
            error!(self.diags, e.loc, "array size must be greater than zero");
            return None;
        }
        if v > 65536 {
            error!(self.diags, e.loc, "array size {v} is too large");
            return None;
        }
        Some(v as u32)
    }

    /// A constant integral expression's value.
    fn const_int(&mut self, e: &ast::Expr) -> Option<i64> {
        let h = self.expr(e)?;
        match (h.constant(), h.ty.as_basic()) {
            (Some(v), Some(Basic::Scalar(Scalar::Int))) => Some(i64::from(v[0].bits() as i32)),
            (Some(v), Some(Basic::Scalar(Scalar::Uint))) => Some(i64::from(v[0].bits())),
            (_, Some(Basic::Scalar(Scalar::Int | Scalar::Uint))) => {
                error!(self.diags, e.loc, "a constant expression is required");
                None
            }
            _ => {
                error!(self.diags, e.loc, "an integral constant expression is required");
                None
            }
        }
    }

    fn struct_definition(&mut self, s: &ast::StructSpec) -> Option<StructId> {
        let mut fields: Vec<Field> = Vec::new();
        for m in &s.members {
            let q = self.check_quals(&m.qualifiers, QualCtx::StructMember);
            let Some(base) = self.resolve_type(&m.ty, true) else { continue };
            if base.is_void() {
                error!(self.diags, m.loc, "a structure member cannot be void");
                continue;
            }
            for n in &m.names {
                let Some(ty) = self.apply_array(base, n.array.as_ref(), n.loc) else { continue };
                if ty.array == Some(0) {
                    error!(self.diags, n.loc, "structure members cannot be unsized arrays");
                    continue;
                }
                if fields.iter().any(|f| f.name == n.name) {
                    let t = self.name(n.name);
                    error!(self.diags, n.loc, "duplicate structure member '{t}'");
                    continue;
                }
                let precision = self.resolve_precision(ty, q.precision, n.loc);
                fields.push(Field { name: n.name, ty, precision });
            }
        }
        if fields.is_empty() {
            error!(self.diags, s.loc, "a structure must have at least one member");
            return None;
        }
        let id = self.structs.add(StructDef { name: s.name.map(|(n, _)| n), fields });
        if let Some((name, loc)) = s.name
            && !self.declare(name, Entry::Struct(id), loc)
        {
            return None;
        }
        Some(id)
    }

    // ---- Declarations ----------------------------------------------------

    fn global_declaration(&mut self, d: &ast::Declaration) {
        match d {
            ast::Declaration::Variables { ty, declarators, loc } => self.variables(ty, declarators, *loc, true),
            ast::Declaration::Precision { precision, ty, loc } => self.precision_statement(*precision, ty, *loc),
            ast::Declaration::Invariant { names, loc } => self.invariant_redeclaration(names, *loc),
            ast::Declaration::Prototype(p) => {
                self.prototype(p, false);
            }
            ast::Declaration::Block(b) => self.uniform_block(b),
            ast::Declaration::Defaults { qualifiers, .. } => {
                if self.version == Version::V100 {
                    error!(self.diags, d_loc(d), "layout qualifiers need GLSL ES 3.00");
                    return;
                }
                let q = self.check_quals(qualifiers, QualCtx::Defaults);
                if let Some(l) = q.layout {
                    self.default_layout = l;
                }
                if let Some(r) = q.row_major {
                    self.default_row_major = r;
                }
            }
        }
    }

    fn invariant_redeclaration(&mut self, names: &[(Symbol, Loc)], loc: Loc) {
        if self.scopes.len() != 1 {
            error!(self.diags, loc, "'invariant' redeclarations must be at global scope");
            return;
        }
        for &(name, nloc) in names {
            match self.lookup(name) {
                Some(Entry::Var(id)) => {
                    let v = &self.vars[id.0 as usize];
                    let output = match v.kind {
                        VarKind::Output => self.stage == Stage::Vertex,
                        VarKind::Input => self.stage == Stage::Fragment && self.version == Version::V100,
                        VarKind::Builtin(b) => matches!(
                            b,
                            BuiltinVar::Position
                                | BuiltinVar::PointSize
                                | BuiltinVar::FragCoord
                                | BuiltinVar::PointCoord
                                | BuiltinVar::FrontFacing
                        ),
                        _ => false,
                    };
                    if output {
                        self.vars[id.0 as usize].invariant = true;
                    } else {
                        let t = self.name(name);
                        error!(self.diags, nloc, "'{t}' cannot be made invariant: it is not a shader output");
                    }
                }
                _ => {
                    let t = self.name(name);
                    error!(self.diags, nloc, "'{t}' is not declared");
                }
            }
        }
    }

    /// A variable declaration (global or local). Initialisers of globals go
    /// to [`Shader::init`]; locals become statements in `out`.
    fn variables(&mut self, full: &ast::FullType, declarators: &[ast::Declarator], loc: Loc, global: bool) {
        let mut out = Vec::new();
        self.variables_into(full, declarators, loc, global, &mut out);
        if global {
            self.init.append(&mut out);
        }
    }

    fn variables_into(
        &mut self,
        full: &ast::FullType,
        declarators: &[ast::Declarator],
        loc: Loc,
        global: bool,
        out: &mut Vec<hir::Stmt>,
    ) {
        let ctx = if global { QualCtx::GlobalVar } else { QualCtx::LocalVar };
        let q = self.check_quals(&full.qualifiers, ctx);
        let Some(base) = self.resolve_type(&full.ty, false) else { return };
        if declarators.is_empty() {
            // `struct S { ... };` or `float;`: allowed, declares nothing
            // (qualifiers make no sense on their own).
            if !full.qualifiers.is_empty() && !matches!(full.ty.name, ast::TypeName::Struct(_)) {
                warning!(self.diags, loc, "declaration declares nothing");
            }
            return;
        }
        if base.is_void() {
            error!(self.diags, loc, "variables cannot be void");
            return;
        }
        for d in declarators {
            let Some(mut ty) = self.apply_array(base, d.array.as_ref(), d.loc) else { continue };
            self.declare_variable(&q, &mut ty, d, global, out);
        }
    }

    fn declare_variable(
        &mut self,
        q: &Quals,
        ty: &mut Type,
        d: &ast::Declarator,
        global: bool,
        out: &mut Vec<hir::Stmt>,
    ) {
        let loc = d.loc;
        let storage = q.storage;
        let v3 = self.version == Version::V300;
        // The initializer first (it cannot see the variable itself).
        let init = match &d.init {
            Some(e) => match self.expr(e) {
                Some(h) => Some(h),
                None => {
                    // Still declare the variable, to avoid follow-on errors.
                    let kind = if global { VarKind::Global } else { VarKind::Local };
                    if ty.array != Some(0) {
                        let v = self.plain_var(d.name, *ty, kind, q.precision, loc);
                        let id = self.new_var(v);
                        self.declare(d.name, Entry::Var(id), loc);
                    }
                    return;
                }
            },
            None => None,
        };
        if ty.array == Some(0) {
            // Sized by the initializer.
            match init.as_ref().map(|i| i.ty) {
                Some(t) if t.is_array() && t.element == ty.element => *ty = t,
                Some(_) => {
                    error!(self.diags, loc, "an unsized array needs an array initialiser of the same type");
                    return;
                }
                None => {
                    error!(self.diags, loc, "an unsized array needs an initialiser");
                    return;
                }
            }
        }
        if ty.is_array() && !v3 && (storage == Some(Storage::Const) || init.is_some()) {
            error!(self.diags, loc, "arrays cannot be initialised in GLSL ES 1.00");
            return;
        }
        if ty.contains_sampler(&self.structs) && storage != Some(Storage::Uniform) {
            error!(self.diags, loc, "samplers can only be uniforms or function parameters");
            return;
        }
        // Storage-specific rules.
        let kind = match storage {
            None => {
                if global {
                    VarKind::Global
                } else {
                    VarKind::Local
                }
            }
            Some(Storage::Const) => VarKind::Const,
            Some(Storage::Uniform) => VarKind::Uniform,
            Some(Storage::Attribute) => VarKind::Input,
            Some(Storage::Varying) => {
                if self.stage == Stage::Vertex {
                    VarKind::Output
                } else {
                    VarKind::Input
                }
            }
            Some(Storage::In) => VarKind::Input,
            Some(Storage::Out) => VarKind::Output,
            Some(Storage::InOut) => return,
        };
        if matches!(kind, VarKind::Uniform | VarKind::Input | VarKind::Output) && init.is_some() {
            let what = match kind {
                VarKind::Uniform => "uniforms",
                VarKind::Input => "inputs",
                _ => "outputs",
            };
            error!(self.diags, loc, "{what} cannot have initialisers");
            return;
        }
        if matches!(kind, VarKind::Input | VarKind::Output) && !self.interface_type_ok(*ty, kind, storage, q, loc) {
            return;
        }
        if kind == VarKind::Const && init.is_none() {
            error!(self.diags, loc, "a const variable needs an initialiser");
            return;
        }
        let mut value = None;
        if let Some(i) = &init {
            if i.ty != *ty {
                let (a, b) = (self.type_name(*ty), self.type_name(i.ty));
                error!(self.diags, loc, "cannot initialise a '{a}' with a '{b}'");
                return;
            }
            if !v3 && ty.contains_array(&self.structs) {
                error!(self.diags, loc, "arrays cannot be initialised in GLSL ES 1.00");
                return;
            }
            if (kind == VarKind::Const || global) && i.constant().is_none() {
                let what = if kind == VarKind::Const { "a const variable" } else { "a global variable" };
                error!(self.diags, loc, "the initialiser of {what} must be a constant expression");
                return;
            }
            if kind == VarKind::Const {
                value = i.constant().cloned();
            }
        }
        let precision = self.resolve_precision(*ty, q.precision, loc);
        let mut var = self.plain_var(d.name, *ty, kind, precision, loc);
        var.interp = q.interp.unwrap_or(Interp::Smooth);
        var.centroid = q.centroid;
        var.invariant = q.invariant || (self.invariant_all && kind == VarKind::Output);
        var.location = q.location;
        var.value = value;
        let id = self.new_var(var);
        if !self.declare(d.name, Entry::Var(id), loc) {
            return;
        }
        if kind != VarKind::Const {
            match init {
                Some(i) => out.push(hir::Stmt::Decl(id, Some(i))),
                None if !global => out.push(hir::Stmt::Decl(id, None)),
                None => {}
            }
        }
    }

    /// Checks the type of a shader input or output.
    fn interface_type_ok(&mut self, ty: Type, kind: VarKind, storage: Option<Storage>, q: &Quals, loc: Loc) -> bool {
        let elem = ty.element_type();
        let b = match elem.as_basic() {
            Some(b) => b,
            None => {
                error!(self.diags, loc, "shader inputs and outputs cannot be structures");
                return false;
            }
        };
        let scalar = b.scalar();
        let v3 = self.version == Version::V300;
        let (ok, what) = match (self.version, storage, self.stage) {
            (Version::V100, Some(Storage::Attribute), _) => {
                (!ty.is_array() && scalar == Some(Scalar::Float), "attributes can only be float, vec2-4 or mat2-4")
            }
            (Version::V100, _, _) => {
                (scalar == Some(Scalar::Float), "varyings can only be float, vectors, matrices or arrays of these")
            }
            (_, Some(Storage::In), Stage::Vertex) => (
                !ty.is_array() && scalar.is_some_and(|s| s != Scalar::Bool),
                "vertex shader inputs cannot be bool, arrays or structures",
            ),
            (_, Some(Storage::Out), Stage::Fragment) => (
                !b.is_matrix() && scalar.is_some_and(|s| s != Scalar::Bool),
                "fragment shader outputs can only be float, int, uint, their vectors, or arrays of these",
            ),
            _ => (scalar.is_some_and(|s| s != Scalar::Bool), "shader inputs and outputs cannot be bool or structures"),
        };
        if !ok {
            error!(self.diags, loc, "{what}");
            return false;
        }
        // Integer varyings must be flat (GLSL ES 3.00 4.3.4 / 4.3.6).
        let varying =
            matches!((self.stage, kind), (Stage::Vertex, VarKind::Output) | (Stage::Fragment, VarKind::Input));
        if v3 && varying && scalar.is_some_and(Scalar::is_integer) && q.interp != Some(Interp::Flat) {
            let side = if self.stage == Stage::Vertex { "vertex shader outputs" } else { "fragment shader inputs" };
            error!(self.diags, loc, "integer {side} must be qualified 'flat'");
            return false;
        }
        true
    }

    fn uniform_block(&mut self, b: &ast::Block) {
        if self.version == Version::V100 {
            error!(self.diags, b.loc, "uniform blocks need GLSL ES 3.00");
            return;
        }
        if self.scopes.len() != 1 {
            error!(self.diags, b.loc, "uniform blocks must be at global scope");
            return;
        }
        let q = self.check_quals(&b.qualifiers, QualCtx::Block);
        if q.storage != Some(Storage::Uniform) {
            return;
        }
        if self.blocks.iter().any(|x| x.name == b.name) {
            let t = self.name(b.name);
            error!(self.diags, b.loc, "uniform block '{t}' is declared twice");
            return;
        }
        let layout = q.layout.unwrap_or(self.default_layout);
        let block_row_major = q.row_major.unwrap_or(self.default_row_major);
        let mut members = Vec::new();
        for m in &b.members {
            let mq = self.check_quals(&m.qualifiers, QualCtx::BlockMember);
            let Some(base) = self.resolve_type(&m.ty, true) else { continue };
            for n in &m.names {
                let Some(ty) = self.apply_array(base, n.array.as_ref(), n.loc) else { continue };
                if ty.array == Some(0) {
                    error!(self.diags, n.loc, "block members cannot be unsized arrays");
                    continue;
                }
                if ty.is_void() || ty.contains_sampler(&self.structs) {
                    error!(self.diags, n.loc, "block members cannot be void or samplers");
                    continue;
                }
                if members.iter().any(|x: &BlockMember| x.name == n.name) {
                    let t = self.name(n.name);
                    error!(self.diags, n.loc, "duplicate block member '{t}'");
                    continue;
                }
                let precision = self.resolve_precision(ty, mq.precision, n.loc);
                members.push(BlockMember {
                    name: n.name,
                    ty,
                    precision,
                    row_major: mq.row_major.unwrap_or(block_row_major),
                });
            }
        }
        if members.is_empty() {
            error!(self.diags, b.loc, "a uniform block must have at least one member");
            return;
        }
        let fields = members.iter().map(|m| Field { name: m.name, ty: m.ty, precision: m.precision }).collect();
        let sid = self.structs.add(StructDef { name: Some(b.name), fields });
        let array = match &b.instance {
            Some((_, Some(size), loc)) => match size {
                ast::ArraySize::Sized(e) => match self.array_size(e) {
                    Some(n) => Some(n),
                    None => return,
                },
                ast::ArraySize::Unsized => {
                    error!(self.diags, *loc, "an array of blocks needs a size");
                    return;
                }
            },
            _ => None,
        };
        let index = self.blocks.len() as u32;
        let mut struct_ty = Type::structure(sid);
        if let Some(n) = array {
            struct_ty = struct_ty.array_of(n);
        }
        self.blocks.push(UniformBlock {
            name: b.name,
            instance: b.instance.as_ref().map(|i| i.0),
            array,
            members: members.clone(),
            layout,
            struct_ty: Type::structure(sid),
            loc: b.loc,
            used: false,
        });
        match &b.instance {
            Some((name, _, loc)) => {
                let v = self.plain_var(*name, struct_ty, VarKind::BlockInstance(index), None, *loc);
                let id = self.new_var(v);
                self.declare(*name, Entry::Var(id), *loc);
            }
            None => {
                for (i, m) in members.iter().enumerate() {
                    let v = self.plain_var(m.name, m.ty, VarKind::BlockMember(index, i as u32), m.precision, b.loc);
                    let id = self.new_var(v);
                    self.declare(m.name, Entry::Var(id), b.loc);
                }
            }
        }
    }

    // ---- Functions -------------------------------------------------------

    /// Declares a function from its prototype; returns its id. A definition
    /// (`defining`) must not have been seen before for the same signature.
    fn prototype(&mut self, p: &ast::Prototype, defining: bool) -> Option<FuncId> {
        if self.scopes.len() != 1 {
            error!(self.diags, p.loc, "functions can only be declared at global scope");
            return None;
        }
        let rq = self.check_quals(&p.ret.qualifiers, QualCtx::FunctionReturn);
        let ret = self.resolve_type(&p.ret.ty, false)?;
        if ret.is_array() && self.version == Version::V100 {
            error!(self.diags, p.loc, "functions cannot return arrays in GLSL ES 1.00");
            return None;
        }
        if ret.array == Some(0) {
            error!(self.diags, p.loc, "a function cannot return an unsized array");
            return None;
        }
        if ret.contains_sampler(&self.structs) {
            error!(self.diags, p.loc, "a function cannot return a sampler");
            return None;
        }
        let ret_precision = self.resolve_precision(ret, rq.precision, p.loc);
        // Parameters.
        let mut params: Vec<ParamInfo> = Vec::new();
        for param in &p.params {
            let q = self.check_quals(&param.qualifiers, QualCtx::Param);
            let base = self.resolve_type(&param.ty, false)?;
            let ty = self.apply_array(base, param.array.as_ref(), param.loc)?;
            if ty.is_void() {
                error!(self.diags, param.loc, "a parameter cannot be void");
                return None;
            }
            if ty.array == Some(0) {
                error!(self.diags, param.loc, "a parameter cannot be an unsized array");
                return None;
            }
            let mode = match q.storage {
                Some(Storage::Out) => hir::ParamMode::Out,
                Some(Storage::InOut) => hir::ParamMode::InOut,
                _ => hir::ParamMode::In,
            };
            if mode != hir::ParamMode::In && ty.contains_sampler(&self.structs) {
                error!(self.diags, param.loc, "samplers can only be 'in' parameters");
                return None;
            }
            let precision = self.resolve_precision(ty, q.precision, param.loc);
            params.push((param.name, ty, mode, q.constant, precision, param.loc));
        }
        let name_text = self.name(p.name);
        if name_text == "main" && (ret != Type::VOID || !params.is_empty()) {
            error!(self.diags, p.loc, "'main' must be declared 'void main()'");
            return None;
        }
        // A built-in function: 3.00 forbids overloading it, 1.00 only
        // redefining it (the same parameter types).
        let types: Vec<Type> = params.iter().map(|x| x.1).collect();
        if builtins::is_builtin_name(&name_text, self.version) {
            let ctx = builtins::CallContext { stage: self.stage, version: self.version, enabled: self.enabled };
            let same = builtins::resolve(&name_text, &types, &ctx).is_ok();
            if self.version == Version::V300 || same {
                error!(self.diags, p.loc, "built-in function '{name_text}' cannot be redeclared or overloaded");
                return None;
            }
        }
        // An earlier declaration with the same parameter types.
        let existing = self.func_names.get(&p.name).and_then(|ids| {
            ids.iter().copied().find(|id| {
                let f = &self.functions[id.0 as usize];
                f.params.len() == types.len()
                    && f.params.iter().zip(&types).all(|(v, t)| self.vars[v.0 as usize].ty == *t)
            })
        });
        if let Some(id) = existing {
            let f = &self.functions[id.0 as usize];
            if f.ret != ret {
                error!(self.diags, p.loc, "'{name_text}' redeclared with a different return type");
                return None;
            }
            let modes_match = f.params.iter().zip(&params).all(|(v, p)| {
                let old = &self.vars[v.0 as usize];
                matches!((old.kind, p.2), (VarKind::Param(a), b) if a == b)
            });
            if !modes_match {
                error!(self.diags, p.loc, "'{name_text}' redeclared with different parameter qualifiers");
                return None;
            }
            if defining {
                if f.body.is_some() {
                    error!(self.diags, p.loc, "'{name_text}' is already defined");
                    return None;
                }
                // The definition's parameter names are the ones that count.
                let ids = f.params.clone();
                for (v, p) in ids.iter().zip(&params) {
                    let var = &mut self.vars[v.0 as usize];
                    var.name = p.0.unwrap_or(var.name);
                    var.loc = p.5;
                    var.precision = p.4;
                    var.read_only = p.3;
                }
            }
            return Some(id);
        }
        if !self.declare(p.name, Entry::Funcs, p.loc) {
            return None;
        }
        let anon = self.sym("");
        let mut param_ids = Vec::with_capacity(params.len());
        for (name, ty, mode, constant, precision, loc) in params {
            let mut v = self.plain_var(name.unwrap_or(anon), ty, VarKind::Param(mode), precision, loc);
            v.read_only = constant;
            param_ids.push(self.new_var(v));
        }
        self.functions.push(Function {
            name: p.name,
            ret,
            ret_precision,
            params: param_ids,
            body: None,
            loc: p.loc,
            calls: Vec::new(),
        });
        let id = FuncId(self.functions.len() as u32 - 1);
        self.func_names.entry(p.name).or_default().push(id);
        Some(id)
    }

    fn function_definition(&mut self, f: &ast::FunctionDef) {
        let Some(id) = self.prototype(&f.proto, true) else { return };
        let ret = self.functions[id.0 as usize].ret;
        self.push_scope();
        let params = self.functions[id.0 as usize].params.clone();
        for &p in &params {
            let v = &self.vars[p.0 as usize];
            let (name, loc) = (v.name, v.loc);
            if !self.interner.get(name).is_empty() {
                self.declare(name, Entry::Var(p), loc);
            }
        }
        self.func = Some(FnCtx { ret, loops: 0, switches: 0, calls: Vec::new() });
        // The body shares the parameters' scope.
        let body = self.statements(&f.body);
        let ctx = self.func.take();
        self.pop_scope();
        let func = &mut self.functions[id.0 as usize];
        func.body = Some(body);
        func.calls = ctx.map(|c| c.calls).unwrap_or_default();
    }

    /// Checks done once the whole shader has been seen.
    fn finish(&mut self) {
        // main must be defined.
        let main = self.interner.lookup("main").and_then(|m| self.func_names.get(&m)).and_then(|f| f.first().copied());
        match main {
            Some(id) if self.functions[id.0 as usize].body.is_some() => {}
            _ => error!(self.diags, Loc::NONE, "'main' is not defined"),
        }
        // Every function called must be defined.
        let mut reported = Vec::new();
        for f in &self.functions {
            for &c in f.calls.iter().chain(&self.init_calls) {
                let callee = &self.functions[c.0 as usize];
                if callee.body.is_none() && !reported.contains(&c) {
                    reported.push(c);
                    let n = String::from(self.interner.get(callee.name));
                    error!(self.diags, callee.loc, "function '{n}' is called but never defined");
                }
            }
        }
        // No recursion (static, through any chain of calls).
        let n = self.functions.len();
        let mut state = vec![0u8; n]; // 0 unvisited, 1 on stack, 2 done
        for start in 0..n {
            if state[start] == 0 && self.has_cycle(start, &mut state) {
                let name = String::from(self.interner.get(self.functions[start].name));
                error!(self.diags, self.functions[start].loc, "recursion is not allowed ('{name}' calls itself)");
                break;
            }
        }
        // GLSL ES 1.00: gl_FragColor and gl_FragData cannot both be written.
        if self.stage == Stage::Fragment && self.version == Version::V100 {
            let used = |b: BuiltinVar| self.vars.iter().any(|v| v.kind == VarKind::Builtin(b) && v.used);
            if used(BuiltinVar::FragColor) && used(BuiltinVar::FragData) {
                error!(self.diags, Loc::NONE, "a shader cannot write both gl_FragColor and gl_FragData");
            }
        }
        // GLSL ES 3.00: several outputs all need a location.
        if self.stage == Stage::Fragment && self.version == Version::V300 {
            let outputs: Vec<&Var> = self.vars.iter().filter(|v| v.kind == VarKind::Output).collect();
            if outputs.len() > 1 && outputs.iter().any(|v| v.location.is_none()) {
                error!(self.diags, outputs[0].loc, "with more than one output, every output needs a layout location");
            }
        }
    }

    fn has_cycle(&self, f: usize, state: &mut [u8]) -> bool {
        // Iterative depth-first search: call chains may be long.
        let mut stack: Vec<(usize, usize)> = vec![(f, 0)];
        state[f] = 1;
        while let Some(&mut (node, ref mut next)) = stack.last_mut() {
            let calls = &self.functions[node].calls;
            if *next < calls.len() {
                let c = calls[*next].0 as usize;
                *next += 1;
                match state[c] {
                    1 => return true,
                    0 => {
                        state[c] = 1;
                        stack.push((c, 0));
                    }
                    _ => {}
                }
            } else {
                state[node] = 2;
                stack.pop();
            }
        }
        false
    }

    /// Records a call from the current function (or a global initialiser).
    fn record_call(&mut self, id: FuncId) {
        match &mut self.func {
            Some(f) => {
                if !f.calls.contains(&id) {
                    f.calls.push(id);
                }
            }
            None => {
                if !self.init_calls.contains(&id) {
                    self.init_calls.push(id);
                }
            }
        }
    }

    /// The value of a constant variable.
    fn const_value(&self, id: VarId) -> Option<ConstValue> {
        let v = &self.vars[id.0 as usize];
        if v.kind == VarKind::Const { v.value.clone() } else { None }
    }
}

fn d_loc(d: &ast::Declaration) -> Loc {
    match d {
        ast::Declaration::Variables { loc, .. }
        | ast::Declaration::Precision { loc, .. }
        | ast::Declaration::Invariant { loc, .. }
        | ast::Declaration::Defaults { loc, .. } => *loc,
        ast::Declaration::Prototype(p) => p.loc,
        ast::Declaration::Block(b) => b.loc,
    }
}
