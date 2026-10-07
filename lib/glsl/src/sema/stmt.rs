//! Statements.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::Sema;
use crate::ast::{self, StmtKind};
use crate::diag::{Loc, error};
use crate::hir::{self, Clause, Loop, Stmt};
use crate::ops;
use crate::types::{Basic, Scalar, Type};
use crate::{Stage, Version};

impl Sema<'_> {
    /// Checks statements in the current scope.
    pub(super) fn statements(&mut self, stmts: &[ast::Stmt]) -> Vec<Stmt> {
        let mut out = Vec::new();
        for s in stmts {
            if self.diags.saturated() {
                break;
            }
            self.statement(s, &mut out);
        }
        out
    }

    /// Checks a statement in a scope of its own (the branches of `if`, the
    /// body of `do`).
    fn scoped(&mut self, s: &ast::Stmt) -> Vec<Stmt> {
        self.push_scope();
        let mut out = Vec::new();
        self.statement(s, &mut out);
        self.pop_scope();
        out
    }

    /// Checks a loop body, which shares the loop's scope: a compound body's
    /// statements are checked in it directly.
    fn loop_body(&mut self, s: &ast::Stmt) -> Vec<Stmt> {
        match &s.kind {
            StmtKind::Compound(stmts) => self.statements(stmts),
            _ => {
                let mut out = Vec::new();
                self.statement(s, &mut out);
                out
            }
        }
    }

    fn condition(&mut self, e: &ast::Expr) -> Option<hir::Expr> {
        let c = self.expr(e)?;
        if c.ty != Type::BOOL {
            error!(self.diags, c.loc, "a condition must be a bool");
            return None;
        }
        Some(c)
    }

    fn statement(&mut self, s: &ast::Stmt, out: &mut Vec<Stmt>) {
        let loc = s.loc;
        match &s.kind {
            StmtKind::Empty => {}
            StmtKind::Expr(e) => {
                if let Some(h) = self.expr(e) {
                    out.push(Stmt::Expr(h));
                }
            }
            StmtKind::Declaration(d) => self.local_declaration(d, out),
            StmtKind::Compound(stmts) => {
                self.push_scope();
                let body = self.statements(stmts);
                self.pop_scope();
                out.push(Stmt::Block(body));
            }
            StmtKind::If { cond, then, otherwise } => {
                let c = self.condition(cond);
                let t = self.scoped(then);
                let e = otherwise.as_ref().map(|o| self.scoped(o)).unwrap_or_default();
                if let Some(c) = c {
                    out.push(Stmt::If(c, t, e));
                }
            }
            StmtKind::Switch { selector, body } => self.switch(selector, body, loc, out),
            StmtKind::Case(_) | StmtKind::Default => {
                error!(self.diags, loc, "case and default labels can only appear directly in a switch");
            }
            StmtKind::While { cond, body } => {
                self.push_scope();
                let (cond, cond_var) = self.loop_condition(cond);
                self.enter_loop();
                let body = self.loop_body(body);
                self.leave_loop();
                self.pop_scope();
                if let Some(c) = cond {
                    out.push(Stmt::Loop(Box::new(Loop { cond: Some(c), cond_var, step: None, body, do_while: false })));
                }
            }
            StmtKind::DoWhile { body, cond } => {
                self.enter_loop();
                let body = self.scoped(body);
                self.leave_loop();
                if let Some(c) = self.condition(cond) {
                    out.push(Stmt::Loop(Box::new(Loop {
                        cond: Some(c),
                        cond_var: None,
                        step: None,
                        body,
                        do_while: true,
                    })));
                }
            }
            StmtKind::For { init, cond, step, body } => {
                self.push_scope();
                let mut prelude = Vec::new();
                if let Some(i) = init {
                    self.statement(i, &mut prelude);
                }
                let (c, cond_var) = match cond {
                    Some(c) => self.loop_condition(c),
                    None => (Some(self.konst(alloc::vec![ops::Value::B(true)], Type::BOOL, loc)), None),
                };
                let step = step.as_ref().and_then(|e| self.expr(e));
                self.enter_loop();
                let body = self.loop_body(body);
                self.leave_loop();
                self.pop_scope();
                if let Some(c) = c {
                    prelude.push(Stmt::Loop(Box::new(Loop { cond: Some(c), cond_var, step, body, do_while: false })));
                    out.push(Stmt::Block(prelude));
                }
            }
            StmtKind::Continue => {
                if self.func.as_ref().is_none_or(|f| f.loops == 0) {
                    error!(self.diags, loc, "'continue' outside a loop");
                    return;
                }
                out.push(Stmt::Continue);
            }
            StmtKind::Break => {
                if self.func.as_ref().is_none_or(|f| f.loops == 0 && f.switches == 0) {
                    error!(self.diags, loc, "'break' outside a loop or switch");
                    return;
                }
                out.push(Stmt::Break);
            }
            StmtKind::Discard => {
                if self.stage != Stage::Fragment {
                    error!(self.diags, loc, "'discard' is only allowed in fragment shaders");
                    return;
                }
                self.discards = true;
                out.push(Stmt::Discard);
            }
            StmtKind::Return(value) => {
                let ret = self.func.as_ref().map_or(Type::VOID, |f| f.ret);
                match value {
                    None if ret.is_void() => out.push(Stmt::Return(None)),
                    None => {
                        let t = self.type_name(ret);
                        error!(self.diags, loc, "this function must return a '{t}'");
                    }
                    Some(e) => {
                        let Some(v) = self.expr(e) else { return };
                        if ret.is_void() {
                            if v.ty.is_void() {
                                // `return f();` with a void f: allowed.
                                out.push(Stmt::Expr(v));
                                out.push(Stmt::Return(None));
                            } else {
                                error!(self.diags, loc, "a void function cannot return a value");
                            }
                        } else if v.ty != ret {
                            let (a, b) = (self.type_name(ret), self.type_name(v.ty));
                            error!(self.diags, loc, "returning a '{b}' from a function returning '{a}'");
                        } else {
                            out.push(Stmt::Return(Some(v)));
                        }
                    }
                }
            }
        }
    }

    fn enter_loop(&mut self) {
        if let Some(f) = &mut self.func {
            f.loops += 1;
        }
    }

    fn leave_loop(&mut self) {
        if let Some(f) = &mut self.func {
            f.loops -= 1;
        }
    }

    /// A loop condition: an expression, or a declaration `T x = e` whose
    /// value is the condition.
    fn loop_condition(&mut self, c: &ast::Condition) -> (Option<hir::Expr>, Option<(hir::VarId, hir::Expr)>) {
        match c {
            ast::Condition::Expr(e) => (self.condition(e), None),
            ast::Condition::Decl { ty, name, init, loc } => {
                let d = ast::Declarator { name: *name, array: None, init: Some(init.clone()), loc: *loc };
                let mut decl = Vec::new();
                self.variables_into(ty, core::slice::from_ref(&d), *loc, false, &mut decl);
                match decl.pop() {
                    Some(Stmt::Decl(id, Some(value))) => {
                        if value.ty != Type::BOOL {
                            error!(self.diags, *loc, "a condition must be a bool");
                            return (None, None);
                        }
                        let var = hir::Expr { kind: hir::ExprKind::Var(id), ty: Type::BOOL, loc: *loc };
                        (Some(var), Some((id, value)))
                    }
                    _ => (None, None),
                }
            }
        }
    }

    fn local_declaration(&mut self, d: &ast::Declaration, out: &mut Vec<Stmt>) {
        match d {
            ast::Declaration::Variables { ty, declarators, loc } => {
                self.variables_into(ty, declarators, *loc, false, out);
            }
            ast::Declaration::Precision { precision, ty, loc } => self.precision_statement(*precision, ty, *loc),
            ast::Declaration::Prototype(p) => {
                error!(self.diags, p.loc, "functions can only be declared at global scope");
            }
            ast::Declaration::Invariant { loc, .. } => {
                error!(self.diags, *loc, "'invariant' redeclarations must be at global scope");
            }
            ast::Declaration::Block(b) => error!(self.diags, b.loc, "uniform blocks must be at global scope"),
            ast::Declaration::Defaults { loc, .. } => {
                error!(self.diags, *loc, "default layout qualifiers must be at global scope");
            }
        }
    }

    fn switch(&mut self, selector: &ast::Expr, body: &[ast::Stmt], loc: Loc, out: &mut Vec<Stmt>) {
        if self.version == Version::V100 {
            error!(self.diags, loc, "switch needs GLSL ES 3.00");
            return;
        }
        let sel = self.expr(selector);
        let sel_scalar = sel.as_ref().and_then(|s| s.ty.as_basic());
        let int_ty = match sel_scalar {
            Some(Basic::Scalar(s @ (Scalar::Int | Scalar::Uint))) => Some(s),
            Some(_) => {
                error!(self.diags, selector.loc, "a switch's expression must be an integer scalar");
                None
            }
            None => None,
        };
        let mut clauses: Vec<Clause> = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        let mut has_default = false;
        // A clause collects labels until a statement follows them.
        let mut open_labels = false;
        self.push_scope();
        if let Some(f) = &mut self.func {
            f.switches += 1;
        }
        for s in body {
            match &s.kind {
                StmtKind::Case(e) => {
                    let label = self.expr(e);
                    let value = match (&label, int_ty) {
                        (Some(l), Some(t)) => match (l.constant(), l.ty.as_basic()) {
                            (Some(v), Some(Basic::Scalar(lt))) if lt == t => Some(v[0]),
                            (Some(_), _) => {
                                error!(self.diags, s.loc, "a case label must have the type of the switch's expression");
                                None
                            }
                            (None, _) => {
                                error!(self.diags, s.loc, "a case label must be a constant expression");
                                None
                            }
                        },
                        _ => None,
                    };
                    if let Some(v) = value {
                        if seen.contains(&v.bits()) {
                            error!(self.diags, s.loc, "duplicate case label");
                        }
                        seen.push(v.bits());
                    }
                    if !open_labels {
                        clauses.push(Clause { labels: Vec::new(), body: Vec::new() });
                        open_labels = true;
                    }
                    // A label in error is dropped (the compilation fails).
                    if let (Some(c), Some(v)) = (clauses.last_mut(), value) {
                        c.labels.push(Some(v));
                    }
                }
                StmtKind::Default => {
                    if has_default {
                        error!(self.diags, s.loc, "more than one default label");
                    }
                    has_default = true;
                    if !open_labels {
                        clauses.push(Clause { labels: Vec::new(), body: Vec::new() });
                        open_labels = true;
                    }
                    if let Some(c) = clauses.last_mut() {
                        c.labels.push(None);
                    }
                }
                _ => {
                    if clauses.is_empty() {
                        error!(self.diags, s.loc, "statements in a switch must follow a case or default label");
                        continue;
                    }
                    open_labels = false;
                    let mut stmts = Vec::new();
                    self.statement(s, &mut stmts);
                    if let Some(c) = clauses.last_mut() {
                        c.body.extend(stmts);
                    }
                }
            }
        }
        if open_labels {
            error!(self.diags, loc, "a label must be followed by a statement before the end of the switch");
        }
        if let Some(f) = &mut self.func {
            f.switches -= 1;
        }
        self.pop_scope();
        if let (Some(sel), Some(_)) = (sel, int_ty) {
            out.push(Stmt::Switch(sel, clauses));
        }
    }
}
