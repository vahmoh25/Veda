//! From the checked shader to SSA.
//!
//! The builder walks `main` (and every function it calls, inlined) in
//! program order, keeping the current SSA value of every scalar component
//! of every variable. Control flow merges these maps: at the end of an `if`
//! with φ functions where the branches disagree, at a loop's header with a
//! φ for every variable live there (those the loop does not change are
//! removed later as trivial), after a loop with the states of its `break`s.
//!
//! * Calls are inlined (GLSL ES has no recursion). A function whose returns
//!   are not all a final `return` is wrapped in a loop run once: `return`
//!   breaks out of it, through enclosing loops by setting a flag that is
//!   tested after each of them.
//! * `switch` becomes a loop run once, whose clauses run while a
//!   fall-through flag is set; `continue` inside it breaks out and then
//!   continues the enclosing loop.
//! * `for` loops run their step at each `continue` and at the end of the
//!   body; `do`-`while` loops test their condition there.
//! * Inputs, uniforms and built-in inputs are loaded once, in the entry
//!   block; outputs are stored when `main` finishes.
//! * An index that is not constant selects among an array's elements (a
//!   chain of selects for loads, one select per element for stores), except
//!   for uniforms and uniform blocks, which are loaded at a computed offset
//!   (clamped to the variable, so a stray index reads nothing else).
//! * Samplers are values holding their sampler index; a lookup whose index
//!   is constant (nearly always, after inlining) refers to it directly.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use super::{Block, BuiltinIn, BuiltinOut, Func, IfNode, InstOp, Jump, LoopNode, Node, TexOp, Value};
use crate::Stage;
use crate::builtins::{Builtin, BuiltinVar, TexLod};
use crate::hir::{self, BinOp, Expr, ExprKind as H, FuncId, LogicOp, ParamMode, Shader, Stmt, VarId, VarKind};
use crate::link::{self, Linked, StageLayout};
use crate::lower::{self, Emit};
use crate::ops::{self, Op, Ty};
use crate::types::{Basic, Element, Scalar, Structs, Type};

/// A variable component's key: the variable, then the component.
type Key = (u32, u32);

/// The SSA state: every variable component's current value.
type Defs = BTreeMap<Key, Value>;

/// What a loop context is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopKind {
    Loop,
    Switch,
    Function,
}

/// What runs before a `continue` takes effect.
#[derive(Clone)]
enum Continue {
    None,
    /// A `for` loop's step.
    Step(Expr),
    /// A `do`-`while` loop's condition (break if false).
    Test(Expr),
}

struct LoopCtx {
    kind: LoopKind,
    breaks: Vec<Defs>,
    continues: Vec<Defs>,
    construct: Continue,
    /// For `Function`: the key of the return value and of the return flag.
    ret: Option<(u32, u32)>,
    /// For `Switch`: the key of the "continue the enclosing loop" flag.
    continue_flag: Option<u32>,
    /// A `return` happened inside this loop (so its exit tests the flag).
    returned: bool,
    /// A `continue` was turned into a flag inside this switch.
    continued: bool,
}

/// An access path: the root variable, its type, and the steps (each with
/// the type it applies to).
type Path = (VarId, Type, Vec<(Step, Type)>);

/// One step of an access path.
#[derive(Clone, Copy)]
enum Step {
    Field(u32),
    /// An index: constant, or an SSA value (an int or uint).
    Index(Option<u32>, Value),
    Swizzle(hir::Swizzle),
}

/// Builds the SSA form of a stage of a linked program.
pub fn build(shader: &Shader, layout: &StageLayout, linked: &Linked) -> Func {
    let mut b = Builder::new(shader, layout, linked);
    b.run();
    b.func
}

struct Builder<'a> {
    s: &'a Shader,
    layout: &'a StageLayout,
    linked: &'a Linked,
    func: Func,
    /// Where instructions go.
    cur: Block,
    entry: Block,
    /// Lists being built, innermost last.
    lists: Vec<Vec<Node>>,
    defs: Defs,
    /// The current point cannot be reached (after a jump).
    dead: bool,
    loops: Vec<LoopCtx>,
    /// Loads cached in the entry block.
    cache: BTreeMap<CacheKey, Value>,
    /// Keys for temporaries (beyond the shader's variables).
    next_temp: u32,
    /// Constants already in the entry block, by type and bits.
    constants: BTreeMap<(Ty, u32), Value>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CacheKey {
    Input(u32, u8),
    Uniform(u32, u8),
    Block(u32, u32),
    Builtin(BuiltinIn),
}

impl Emit for Builder<'_> {
    type V = Value;

    fn konst(&mut self, v: ops::Value) -> Value {
        self.konst_value(v)
    }

    fn op(&mut self, op: Op, args: &[Value], ret: Ty) -> Value {
        // Fold constants as the code is built.
        let consts: Option<Vec<ops::Value>> = args.iter().map(|&a| self.func.constant(a)).collect();
        if let Some(c) = consts {
            return self.konst_value(ops::eval(op, &c, ret));
        }
        self.emit(InstOp::Op(op), args.to_vec(), ret)
    }

    fn select(&mut self, c: Value, a: Value, b: Value, ty: Ty) -> Value {
        if let Some(k) = self.func.constant(c) {
            return if k.bits() != 0 { a } else { b };
        }
        if a == b {
            return a;
        }
        self.emit(InstOp::Select, vec![c, a, b], ty)
    }
}

impl<'a> Builder<'a> {
    fn new(s: &'a Shader, layout: &'a StageLayout, linked: &'a Linked) -> Builder<'a> {
        let mut func = Func::new(s.stage);
        let entry = func.new_block();
        Builder {
            s,
            layout,
            linked,
            func,
            cur: entry,
            entry,
            lists: vec![vec![Node::Block(entry)]],
            defs: Defs::new(),
            dead: false,
            loops: Vec::new(),
            cache: BTreeMap::new(),
            next_temp: s.vars.len() as u32,
            constants: BTreeMap::new(),
        }
    }

    fn structs(&self) -> &'a Structs {
        &self.s.structs
    }

    // ---- Instructions ----------------------------------------------------

    fn emit(&mut self, op: InstOp, args: Vec<Value>, ty: Ty) -> Value {
        let id = self.func.create(op, args, &[ty]);
        self.func.push(self.cur, id);
        self.func.insts[id.0 as usize].results[0]
    }

    /// An instruction with no result (a store).
    fn emit_void(&mut self, op: InstOp, args: Vec<Value>) {
        let id = self.func.create(op, args, &[]);
        self.func.push(self.cur, id);
    }

    fn emit_multi(&mut self, op: InstOp, args: Vec<Value>, tys: &[Ty]) -> Vec<Value> {
        let id = self.func.create(op, args, tys);
        self.func.push(self.cur, id);
        self.func.insts[id.0 as usize].results.clone()
    }

    fn konst_value(&mut self, v: ops::Value) -> Value {
        // Constants live in the entry block (shared, and so they dominate
        // every use), once each.
        let key = (v.ty(), v.bits());
        if let Some(&c) = self.constants.get(&key) {
            return c;
        }
        let id = self.func.create(InstOp::Const(v), Vec::new(), &[v.ty()]);
        let entry = self.entry;
        self.func.push(entry, id);
        let c = self.func.insts[id.0 as usize].results[0];
        self.constants.insert(key, c);
        c
    }

    fn zero(&mut self, ty: Ty) -> Value {
        self.konst_value(ops::Value::from_bits(ty, 0))
    }

    fn int(&mut self, v: i32) -> Value {
        self.konst_value(ops::Value::I(v))
    }

    /// A load placed in the entry block, emitted once.
    fn cached(&mut self, key: CacheKey, op: InstOp, ty: Ty) -> Value {
        if let Some(&v) = self.cache.get(&key) {
            return v;
        }
        let id = self.func.create(op, Vec::new(), &[ty]);
        let entry = self.entry;
        self.func.push(entry, id);
        let v = self.func.insts[id.0 as usize].results[0];
        self.cache.insert(key, v);
        v
    }

    fn temp(&mut self) -> u32 {
        self.next_temp += 1;
        self.next_temp - 1
    }

    // ---- Structure -------------------------------------------------------

    /// Ends the current block with a jump; what follows is unreachable.
    fn jump(&mut self, j: Jump) {
        if self.dead {
            return;
        }
        self.func.blocks[self.cur.0 as usize].jump = Some(j);
        self.dead = true;
    }

    /// Starts a new list (a branch or loop body) with a fresh block.
    fn open_list(&mut self) -> Block {
        let b = self.func.new_block();
        self.lists.push(vec![Node::Block(b)]);
        self.cur = b;
        b
    }

    fn close_list(&mut self) -> Vec<Node> {
        self.lists.pop().unwrap_or_default()
    }

    /// Appends a node to the current list, then a new block after it.
    fn append_node(&mut self, n: Node) -> Block {
        let b = self.func.new_block();
        if let Some(list) = self.lists.last_mut() {
            list.push(n);
            list.push(Node::Block(b));
        }
        self.cur = b;
        b
    }

    /// Merges states reaching one block in order (φ at `block`).
    fn merge(&mut self, block: Block, states: &[Defs]) -> Defs {
        match states.len() {
            0 => Defs::new(),
            1 => states[0].clone(),
            _ => {
                let mut keys: BTreeSet<Key> = BTreeSet::new();
                for s in states {
                    keys.extend(s.keys().copied());
                }
                let mut out = Defs::new();
                for k in keys {
                    let vals: Vec<Option<Value>> = states.iter().map(|s| s.get(&k).copied()).collect();
                    let first = vals.iter().flatten().next().copied();
                    let Some(first) = first else { continue };
                    if vals.iter().all(|v| *v == Some(first)) {
                        out.insert(k, first);
                        continue;
                    }
                    let ty = self.func.ty(first);
                    let args: Vec<Value> = vals.iter().map(|v| v.unwrap_or(first)).collect();
                    let args =
                        args.into_iter().zip(&vals).map(|(a, v)| if v.is_some() { a } else { self.zero(ty) }).collect();
                    let id = self.func.create(InstOp::Phi, args, &[ty]);
                    // φs go first in their block.
                    self.func.blocks[block.0 as usize].insts.insert(0, id);
                    out.insert(k, self.func.insts[id.0 as usize].results[0]);
                }
                out
            }
        }
    }

    // ---- The program -----------------------------------------------------

    fn run(&mut self) {
        // Every global and output starts as zero (GLSL leaves them
        // undefined); their keys must exist for the loops' φs.
        for (i, v) in self.s.vars.iter().enumerate() {
            let starts = match v.kind {
                VarKind::Global | VarKind::Output => true,
                VarKind::Builtin(b) => b.is_output(),
                _ => false,
            };
            if starts {
                self.define_zero(VarId(i as u32), v.ty);
            }
        }
        let init = self.s.init.clone();
        for st in &init {
            self.stmt(st);
        }
        if let Some(main) = self.s.main {
            self.inline_body(main, &[]);
        }
        if !self.dead {
            self.store_outputs();
        }
        let list = self.lists.pop().unwrap_or_default();
        self.func.body = list;
    }

    fn define_zero(&mut self, id: VarId, ty: Type) {
        let mut tys = Vec::new();
        lower::component_types(ty, self.structs(), &mut tys);
        for (c, t) in tys.into_iter().enumerate() {
            let z = self.zero(t);
            self.defs.insert((id.0, c as u32), z);
        }
    }

    fn store_outputs(&mut self) {
        for (i, v) in self.s.vars.iter().enumerate() {
            let id = VarId(i as u32);
            let mut tys = Vec::new();
            lower::component_types(v.ty, self.structs(), &mut tys);
            match v.kind {
                VarKind::Output => {
                    let Some(&slot) = self.layout.outputs.get(&id) else { continue };
                    let per_slot = self.components_per_slot(v.ty);
                    for c in 0..tys.len() as u32 {
                        let Some(&val) = self.defs.get(&(id.0, c)) else { continue };
                        let (s, comp) = (slot + c / per_slot, (c % per_slot) as u8);
                        self.emit_void(InstOp::StoreOutput { slot: s, comp }, vec![val]);
                    }
                }
                VarKind::Builtin(b) if b.is_output() && v.used => {
                    for c in 0..tys.len() as u32 {
                        let Some(&val) = self.defs.get(&(id.0, c)) else { continue };
                        let op = match b {
                            BuiltinVar::Position => InstOp::StoreBuiltin(BuiltinOut::Position(c as u8)),
                            BuiltinVar::PointSize => InstOp::StoreBuiltin(BuiltinOut::PointSize),
                            BuiltinVar::FragDepth => InstOp::StoreBuiltin(BuiltinOut::FragDepth),
                            BuiltinVar::FragColor => InstOp::StoreOutput { slot: 0, comp: c as u8 },
                            BuiltinVar::FragData => InstOp::StoreOutput { slot: c / 4, comp: (c % 4) as u8 },
                            _ => continue,
                        };
                        self.emit_void(op, vec![val]);
                    }
                }
                _ => {}
            }
        }
    }

    /// Components each varying slot or output location holds for values of
    /// type `t` (one vector, matrix column or array element per slot).
    fn components_per_slot(&self, t: Type) -> u32 {
        match t.element_type().as_basic() {
            Some(Basic::Matrix(_, r)) => r as u32,
            Some(b) => b.components().max(1),
            None => 4,
        }
    }

    // ---- Functions -------------------------------------------------------

    /// Whether a function's only `return` is its last statement.
    fn simple_returns(body: &[Stmt]) -> bool {
        fn has_return(stmts: &[Stmt]) -> bool {
            stmts.iter().any(|s| match s {
                Stmt::Return(_) => true,
                Stmt::Block(b) => has_return(b),
                Stmt::If(_, t, e) => has_return(t) || has_return(e),
                Stmt::Loop(l) => has_return(&l.body),
                Stmt::Switch(_, c) => c.iter().any(|c| has_return(&c.body)),
                _ => false,
            })
        }
        match body.split_last() {
            Some((Stmt::Return(_), rest)) => !has_return(rest),
            Some(_) => !has_return(body),
            None => true,
        }
    }

    /// Runs a function's body (its parameters already defined); returns its
    /// return value's components.
    fn inline_body(&mut self, f: FuncId, _args: &[Value]) -> Vec<Value> {
        let func = self.s.function(f);
        let Some(body) = func.body.clone() else { return Vec::new() };
        let ret_ty = func.ret;
        let mut tys = Vec::new();
        lower::component_types(ret_ty, self.structs(), &mut tys);
        if Self::simple_returns(&body) {
            let (last, rest) = match body.split_last() {
                Some((Stmt::Return(v), rest)) => (Some(v.clone()), rest),
                _ => (None, &body[..]),
            };
            for st in rest {
                self.stmt(st);
            }
            return match last {
                Some(Some(e)) if !self.dead => self.expr(&e),
                _ => tys.iter().map(|&t| self.zero(t)).collect(),
            };
        }
        // Wrapped in a loop run once.
        let ret_key = self.temp();
        let flag_key = self.temp();
        for (c, &t) in tys.iter().enumerate() {
            let z = self.zero(t);
            self.defs.insert((ret_key, c as u32), z);
        }
        let f0 = self.konst_value(ops::Value::B(false));
        self.defs.insert((flag_key, 0), f0);
        self.loop_with(LoopKind::Function, Continue::None, Some((ret_key, flag_key)), |b| {
            for st in &body {
                b.stmt(st);
            }
            b.break_stmt();
        });
        (0..tys.len() as u32)
            .map(|c| match self.defs.get(&(ret_key, c)) {
                Some(&v) => v,
                None => self.zero(tys[c as usize]),
            })
            .collect()
    }

    /// Builds a loop of `kind`: φs at the header for every variable, the
    /// body from `body`, the exit's state from the `break`s.
    fn loop_with(
        &mut self,
        kind: LoopKind,
        construct: Continue,
        ret: Option<(u32, u32)>,
        body: impl FnOnce(&mut Self),
    ) {
        if self.dead {
            return;
        }
        let pre = self.defs.clone();
        let header = self.open_list();
        // A φ per variable component, with the preheader's value first.
        let mut phis: Vec<(Key, super::InstId)> = Vec::with_capacity(pre.len());
        let mut header_defs = Defs::new();
        for (&k, &v) in &pre {
            let ty = self.func.ty(v);
            let id = self.func.create(InstOp::Phi, vec![v], &[ty]);
            self.func.blocks[header.0 as usize].insts.push(id);
            header_defs.insert(k, self.func.insts[id.0 as usize].results[0]);
            phis.push((k, id));
        }
        self.defs = header_defs;
        let continue_flag = if kind == LoopKind::Switch { Some(self.temp()) } else { None };
        self.loops.push(LoopCtx {
            kind,
            breaks: Vec::new(),
            continues: Vec::new(),
            construct,
            ret,
            continue_flag,
            returned: false,
            continued: false,
        });
        body(self);
        // Falling off the end of the body is a `continue`.
        let end = if self.dead {
            None
        } else {
            self.run_continue_construct();
            if self.dead { None } else { Some(self.defs.clone()) }
        };
        let ctx = self.loops.pop().expect("loop context");
        let list = self.close_list();
        // Back edges: the end of the body first, then each `continue`
        // (the order of `Func::preds`).
        let mut back: Vec<Defs> = Vec::new();
        back.extend(end);
        back.extend(ctx.continues);
        for (k, id) in &phis {
            for state in &back {
                let v = state.get(k).copied().unwrap_or(self.func.insts[id.0 as usize].results[0]);
                self.func.insts[id.0 as usize].args.push(v);
            }
        }
        let exit = self.append_node(Node::Loop(alloc::boxed::Box::new(LoopNode { body: list })));
        self.dead = ctx.breaks.is_empty();
        self.defs = self.merge(exit, &ctx.breaks);
        if self.dead {
            return;
        }
        // Propagate a `return` or a switch's `continue` outward.
        if ctx.returned
            && let Some(outer) = self.loops.last()
        {
            let (_, flag) = self.function_ret().unwrap_or((0, 0));
            let flag_v = self.defs.get(&(flag, 0)).copied();
            if let Some(f) = flag_v {
                let outer_kind = outer.kind;
                if outer_kind == LoopKind::Function {
                    self.if_then(f, |b| b.break_stmt());
                } else {
                    if let Some(o) = self.loops.last_mut() {
                        o.returned = true;
                    }
                    self.if_then(f, |b| b.break_stmt());
                }
            }
        }
        if ctx.continued
            && let Some(flag) = ctx.continue_flag
            && let Some(&f) = self.defs.get(&(flag, 0))
        {
            self.if_then(f, |b| b.continue_stmt());
        }
    }

    /// The innermost function's (return value key, return flag key).
    fn function_ret(&self) -> Option<(u32, u32)> {
        self.loops.iter().rev().find(|l| l.kind == LoopKind::Function).and_then(|l| l.ret)
    }

    /// `if (cond) { then }` with no else.
    fn if_then(&mut self, cond: Value, then: impl FnOnce(&mut Self)) {
        self.if_else(cond, then, |_| {});
    }

    fn if_else(&mut self, cond: Value, then: impl FnOnce(&mut Self), otherwise: impl FnOnce(&mut Self)) {
        if self.dead {
            return;
        }
        if let Some(k) = self.func.constant(cond) {
            if k.bits() != 0 {
                then(self);
            } else {
                otherwise(self);
            }
            return;
        }
        let before = self.defs.clone();
        self.open_list();
        then(self);
        let then_state = (!self.dead).then(|| self.defs.clone());
        let then_list = self.close_list();
        self.defs = before;
        self.dead = false;
        self.open_list();
        otherwise(self);
        let else_state = (!self.dead).then(|| self.defs.clone());
        let else_list = self.close_list();
        let merge =
            self.append_node(Node::If(alloc::boxed::Box::new(IfNode { cond, then: then_list, otherwise: else_list })));
        let states: Vec<Defs> = [then_state, else_state].into_iter().flatten().collect();
        self.dead = states.is_empty();
        self.defs = self.merge(merge, &states);
    }

    /// Runs the innermost real loop's continue construct (a `for` step, a
    /// `do`-`while` test).
    fn run_continue_construct(&mut self) {
        let construct = self.loops.last().map(|l| l.construct.clone()).unwrap_or(Continue::None);
        match construct {
            Continue::None => {}
            Continue::Step(e) => {
                self.expr(&e);
            }
            Continue::Test(e) => {
                let c = self.expr(&e);
                if let Some(&c) = c.first() {
                    let nc = self.op(Op::BNot, &[c], Ty::Bool);
                    self.if_then(nc, |b| b.break_stmt());
                }
            }
        }
    }

    fn break_stmt(&mut self) {
        if self.dead {
            return;
        }
        let state = self.defs.clone();
        if let Some(l) = self.loops.last_mut() {
            l.breaks.push(state);
        }
        self.jump(Jump::Break);
    }

    fn continue_stmt(&mut self) {
        if self.dead {
            return;
        }
        match self.loops.last().map(|l| l.kind) {
            Some(LoopKind::Switch) => {
                // Leave the switch, then continue the enclosing loop.
                let flag = self.loops.last().and_then(|l| l.continue_flag).unwrap_or(0);
                let t = self.konst_value(ops::Value::B(true));
                self.defs.insert((flag, 0), t);
                if let Some(l) = self.loops.last_mut() {
                    l.continued = true;
                }
                self.break_stmt();
            }
            _ => {
                self.run_continue_construct();
                if self.dead {
                    return;
                }
                let state = self.defs.clone();
                if let Some(l) = self.loops.last_mut() {
                    l.continues.push(state);
                }
                self.jump(Jump::Continue);
            }
        }
    }

    fn return_stmt(&mut self, value: Option<&Expr>) {
        if self.dead {
            return;
        }
        let Some((ret_key, flag_key)) = self.function_ret() else {
            // A final return of a simple function is handled by its caller.
            return;
        };
        if let Some(e) = value {
            let vals = self.expr(e);
            for (c, v) in vals.into_iter().enumerate() {
                self.defs.insert((ret_key, c as u32), v);
            }
        }
        let t = self.konst_value(ops::Value::B(true));
        self.defs.insert((flag_key, 0), t);
        if let Some(l) = self.loops.last_mut()
            && l.kind != LoopKind::Function
        {
            l.returned = true;
        }
        self.break_stmt();
    }

    // ---- Statements --------------------------------------------------------

    fn stmts(&mut self, list: &[Stmt]) {
        for s in list {
            if self.dead {
                return;
            }
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        if self.dead {
            return;
        }
        match s {
            Stmt::Expr(e) => {
                self.expr(e);
            }
            Stmt::Decl(id, init) => {
                let v = self.s.var(*id);
                match init {
                    Some(e) => {
                        let vals = self.expr(e);
                        self.write_whole(*id, &vals);
                    }
                    None => self.define_zero(*id, v.ty),
                }
            }
            Stmt::Block(list) => self.stmts(list),
            Stmt::If(c, t, e) => {
                let cv = self.expr(c);
                let Some(&cv) = cv.first() else { return };
                self.if_else(cv, |b| b.stmts(t), |b| b.stmts(e));
            }
            Stmt::Loop(l) => {
                let construct = match (&l.step, l.do_while) {
                    (Some(step), _) => Continue::Step(step.clone()),
                    (None, true) => l.cond.clone().map_or(Continue::None, Continue::Test),
                    _ => Continue::None,
                };
                let l = l.clone();
                self.loop_with(LoopKind::Loop, construct, None, |b| {
                    if !l.do_while {
                        if let Some((var, init)) = &l.cond_var {
                            let v = b.expr(init);
                            b.write_whole(*var, &v);
                        }
                        if let Some(c) = &l.cond {
                            let cv = b.expr(c);
                            if let Some(&cv) = cv.first() {
                                let nc = b.op(Op::BNot, &[cv], Ty::Bool);
                                b.if_then(nc, |b| b.break_stmt());
                            }
                        }
                    }
                    b.stmts(&l.body);
                });
            }
            Stmt::Switch(sel, clauses) => self.switch(sel, clauses),
            Stmt::Break => self.break_stmt(),
            Stmt::Continue => self.continue_stmt(),
            Stmt::Return(v) => self.return_stmt(v.as_ref()),
            Stmt::Discard => self.jump(Jump::Discard),
        }
    }

    fn switch(&mut self, sel: &Expr, clauses: &[hir::Clause]) {
        let sv = self.expr(sel);
        let Some(&sv) = sv.first() else { return };
        // Whether the selector matches no label (for `default`).
        let all_labels: Vec<ops::Value> = clauses.iter().flat_map(|c| c.labels.iter().flatten().copied()).collect();
        let mut matched_any = self.konst_value(ops::Value::B(false));
        for l in &all_labels {
            let k = self.konst_value(*l);
            let eq = self.op(Op::IEq, &[sv, k], Ty::Bool);
            matched_any = self.op(Op::BOr, &[matched_any, eq], Ty::Bool);
        }
        let is_default = self.op(Op::BNot, &[matched_any], Ty::Bool);
        let ft_key = self.temp();
        let f0 = self.konst_value(ops::Value::B(false));
        self.defs.insert((ft_key, 0), f0);
        let clauses = clauses.to_vec();
        self.loop_with(LoopKind::Switch, Continue::None, None, |b| {
            if let Some(flag) = b.loops.last().and_then(|l| l.continue_flag) {
                let f = b.konst_value(ops::Value::B(false));
                b.defs.insert((flag, 0), f);
            }
            for c in &clauses {
                let mut cond = b.defs.get(&(ft_key, 0)).copied().unwrap_or(f0);
                for l in &c.labels {
                    let m = match l {
                        Some(v) => {
                            let k = b.konst_value(*v);
                            b.op(Op::IEq, &[sv, k], Ty::Bool)
                        }
                        None => is_default,
                    };
                    cond = b.op(Op::BOr, &[cond, m], Ty::Bool);
                }
                b.if_then(cond, |b| {
                    let t = b.konst_value(ops::Value::B(true));
                    b.defs.insert((ft_key, 0), t);
                    b.stmts(&c.body);
                });
                if b.dead {
                    return;
                }
            }
            b.break_stmt();
        });
    }

    // ---- Variables -------------------------------------------------------

    /// The components of a whole variable.
    fn read_var(&mut self, id: VarId) -> Vec<Value> {
        let v = self.s.var(id);
        let n = v.ty.flat_components(self.structs()) as usize;
        self.read_var_range(id, 0, n)
    }

    /// Components `start..start+n` of a variable.
    fn read_var_range(&mut self, id: VarId, start: usize, n: usize) -> Vec<Value> {
        let v = self.s.var(id);
        let mut tys = Vec::new();
        lower::component_types(v.ty, self.structs(), &mut tys);
        let kind = v.kind;
        (start..start + n)
            .map(|c| {
                let ty = tys.get(c).copied().unwrap_or(Ty::F32);
                self.read_component(id, kind, c as u32, ty)
            })
            .collect()
    }

    fn read_component(&mut self, id: VarId, kind: VarKind, c: u32, ty: Ty) -> Value {
        match kind {
            VarKind::Input => self.input_component(id, c, ty),
            VarKind::Uniform => self.uniform_component(id, c, ty),
            VarKind::BlockInstance(_) | VarKind::BlockMember(..) => self.block_component(id, c, ty),
            VarKind::Builtin(b) if !b.is_output() => self.builtin_input(b, c, ty),
            VarKind::Const => {
                let v = self.s.var(id).value.as_ref().and_then(|v| v.get(c as usize).copied());
                match v {
                    Some(x) => self.konst_value(x),
                    None => self.zero(ty),
                }
            }
            _ => match self.defs.get(&(id.0, c)) {
                Some(&v) => v,
                None => self.zero(ty),
            },
        }
    }

    fn input_component(&mut self, id: VarId, c: u32, ty: Ty) -> Value {
        let v = self.s.var(id);
        let Some(&base) = self.layout.inputs.get(&id) else { return self.zero(ty) };
        let per = self.components_per_slot(v.ty);
        let (slot, comp) = (base + c / per, (c % per) as u8);
        let raw = self.cached(CacheKey::Input(slot, comp), InstOp::LoadInput { slot, comp }, Ty::F32);
        self.retype(raw, ty)
    }

    /// Reinterprets a loaded 32-bit value (stored as `F32` bits) as `ty`.
    fn retype(&mut self, raw: Value, ty: Ty) -> Value {
        if ty == self.func.ty(raw) {
            return raw;
        }
        match ty {
            Ty::Bool => {
                let as_int = self.op(Op::Bitcast, &[raw], Ty::I32);
                self.op(Op::IToB, &[as_int], Ty::Bool)
            }
            _ => self.op(Op::Bitcast, &[raw], ty),
        }
    }

    fn builtin_input(&mut self, b: BuiltinVar, c: u32, ty: Ty) -> Value {
        let which = match b {
            BuiltinVar::VertexId => BuiltinIn::VertexId,
            BuiltinVar::InstanceId => BuiltinIn::InstanceId,
            BuiltinVar::FragCoord => BuiltinIn::FragCoord(c as u8),
            BuiltinVar::FrontFacing => BuiltinIn::FrontFacing,
            BuiltinVar::PointCoord => BuiltinIn::PointCoord(c as u8),
            _ => return self.zero(ty),
        };
        self.cached(CacheKey::Builtin(which), InstOp::LoadBuiltin(which), ty)
    }

    /// Slot and component of flat component `c` of a uniform of type `t`.
    fn uniform_place(&self, t: Type, mut c: u32) -> (u32, u8) {
        let structs = self.structs();
        let mut slot = 0u32;
        let mut ty = t;
        loop {
            if ty.is_array() {
                let elem = ty.element_type();
                let per = elem.flat_components(structs);
                let i = c / per;
                c %= per;
                slot += i * link::slot_count(elem, structs);
                ty = elem;
                continue;
            }
            match ty.element {
                Element::Struct(id) => {
                    for f in &structs.get(id).fields {
                        let n = f.ty.flat_components(structs);
                        if c < n {
                            ty = f.ty;
                            break;
                        }
                        c -= n;
                        slot += link::slot_count(f.ty, structs);
                    }
                }
                Element::Basic(Basic::Matrix(_, r)) => {
                    return (slot + c / r as u32, (c % r as u32) as u8);
                }
                Element::Basic(_) => return (slot, c as u8),
            }
        }
    }

    fn uniform_component(&mut self, id: VarId, c: u32, ty: Ty) -> Value {
        let v = self.s.var(id);
        let Some(&base) = self.layout.uniforms.get(&id) else { return self.zero(ty) };
        // A sampler component is its sampler index.
        if let Some(&first) = self.layout.samplers.get(&id) {
            let (slot_off, _) = self.uniform_place(v.ty, c);
            if self.sampler_at(v.ty, c) {
                let index = first + self.sampler_ordinal(v.ty, c);
                let _ = slot_off;
                return self.int(index as i32);
            }
        }
        let (off, comp) = self.uniform_place(v.ty, c);
        let slot = base + off;
        let raw = self.cached(CacheKey::Uniform(slot, comp), InstOp::LoadUniform { slot, comp }, Ty::U32);
        self.uniform_retype(raw, ty)
    }

    /// Uniform storage holds 32 bits: floats as floats, ints, and bools as
    /// 0 or 1.
    fn uniform_retype(&mut self, raw: Value, ty: Ty) -> Value {
        match ty {
            Ty::U32 => raw,
            Ty::Bool => self.op(Op::IToB, &[raw], Ty::Bool),
            _ => self.op(Op::Bitcast, &[raw], ty),
        }
    }

    /// Whether flat component `c` of type `t` is a sampler.
    fn sampler_at(&self, t: Type, c: u32) -> bool {
        let mut tys = Vec::new();
        component_basics(t, self.structs(), &mut tys);
        tys.get(c as usize).is_some_and(|b| b.is_sampler())
    }

    /// How many samplers come before flat component `c` of type `t`.
    fn sampler_ordinal(&self, t: Type, c: u32) -> u32 {
        let mut tys = Vec::new();
        component_basics(t, self.structs(), &mut tys);
        tys[..c as usize].iter().filter(|b| b.is_sampler()).count() as u32
    }

    /// The uniform block, array element base and std140 byte offset of flat
    /// component `c` of a block variable.
    fn block_place(&self, id: VarId, c: u32) -> Option<(u32, u32, Ty)> {
        let v = self.s.var(id);
        let structs = self.structs();
        let (block_index, member) = match v.kind {
            VarKind::BlockInstance(b) => (b, None),
            VarKind::BlockMember(b, m) => (b, Some(m)),
            _ => return None,
        };
        let block = &self.s.blocks[block_index as usize];
        let program_block = *self.layout.blocks.get(&block_index)?;
        let fields: Vec<(Type, bool)> = block.members.iter().map(|m| (m.ty, m.row_major)).collect();
        let mut c = c;
        let mut element = 0u32;
        // An instance array: pick the element (block).
        let (mut ty, mut offset, mut row_major) = match member {
            Some(m) => {
                let o = link::std140_member_offset(&fields, m as usize, structs);
                (block.members[m as usize].ty, o, block.members[m as usize].row_major)
            }
            None => {
                let per = block.struct_ty.flat_components(structs);
                if block.array.is_some() {
                    element = c / per;
                    c %= per;
                }
                // Find the member.
                let mut found = None;
                let mut acc = 0;
                for (i, m) in block.members.iter().enumerate() {
                    let n = m.ty.flat_components(structs);
                    if c < acc + n {
                        found = Some(i);
                        break;
                    }
                    acc += n;
                }
                let i = found?;
                c -= acc;
                let o = link::std140_member_offset(&fields, i, structs);
                (block.members[i].ty, o, block.members[i].row_major)
            }
        };
        loop {
            if ty.is_array() {
                let elem = ty.element_type();
                let per = elem.flat_components(structs);
                let stride = link::std140_stride(ty, row_major, structs);
                offset += (c / per) * stride;
                c %= per;
                ty = elem;
                continue;
            }
            match ty.element {
                Element::Struct(sid) => {
                    let sf: Vec<(Type, bool)> = structs.get(sid).fields.iter().map(|f| (f.ty, row_major)).collect();
                    let mut acc = 0;
                    let mut next = None;
                    for (i, f) in structs.get(sid).fields.iter().enumerate() {
                        let n = f.ty.flat_components(structs);
                        if c < acc + n {
                            next = Some((i, f.ty));
                            break;
                        }
                        acc += n;
                    }
                    let (i, fty) = next?;
                    c -= acc;
                    offset += link::std140_member_offset(&sf, i, structs);
                    ty = fty;
                }
                Element::Basic(Basic::Matrix(_, r)) => {
                    let (col, row) = (c / r as u32, c % r as u32);
                    offset += if row_major { row * 16 + col * 4 } else { col * 16 + row * 4 };
                    row_major = false;
                    let _ = row_major;
                    return Some((program_block + element, offset, Ty::F32));
                }
                Element::Basic(b) => {
                    let s = b.scalar().map_or(Ty::F32, lower::ty);
                    return Some((program_block + element, offset + c * 4, s));
                }
            }
        }
    }

    fn block_component(&mut self, id: VarId, c: u32, ty: Ty) -> Value {
        let Some((block, offset, _)) = self.block_place(id, c) else { return self.zero(ty) };
        let raw = self.cached(
            CacheKey::Block(block, offset),
            InstOp::LoadBlock { block, offset, stride: 0, count: 0 },
            Ty::U32,
        );
        self.uniform_retype(raw, ty)
    }

    /// Writes a whole variable.
    fn write_whole(&mut self, id: VarId, vals: &[Value]) {
        for (c, &v) in vals.iter().enumerate() {
            self.defs.insert((id.0, c as u32), v);
        }
    }

    // ---- Access paths ------------------------------------------------------

    /// Splits an l-value or variable expression into its root variable and
    /// steps; `None` for other expressions.
    fn path(&mut self, e: &Expr) -> Option<Path> {
        match &e.kind {
            H::Var(id) => Some((*id, e.ty, Vec::new())),
            H::Field(b, i) => {
                let (root, rt, mut steps) = self.path(b)?;
                steps.push((Step::Field(*i), b.ty));
                Some((root, rt, steps))
            }
            H::Swizzle(b, s) => {
                let (root, rt, mut steps) = self.path(b)?;
                steps.push((Step::Swizzle(*s), b.ty));
                Some((root, rt, steps))
            }
            H::Index(b, i) => {
                let (root, rt, mut steps) = self.path(b)?;
                let iv = self.expr(i);
                let iv = *iv.first()?;
                let k = self.func.constant(iv).map(|c| c.bits());
                steps.push((Step::Index(k, iv), b.ty));
                Some((root, rt, steps))
            }
            _ => None,
        }
    }

    /// The flat component offsets a path selects, as (static base,
    /// dynamic parts), plus the selected type. Each dynamic part is (index
    /// value, element stride in components, element count).
    fn path_offsets(&self, steps: &[(Step, Type)], leaf: Type) -> (Vec<u32>, Vec<(Value, u32, u32)>) {
        let structs = self.structs();
        let mut base = 0u32;
        let mut dynamic = Vec::new();
        let mut swizzle: Option<hir::Swizzle> = None;
        for (step, ty) in steps {
            match step {
                Step::Field(i) => {
                    if let Some(id) = ty.as_struct() {
                        let fields = &structs.get(id).fields;
                        base += fields[..*i as usize].iter().map(|f| f.ty.flat_components(structs)).sum::<u32>();
                    }
                }
                Step::Index(k, v) => {
                    let (stride, count) = match (ty.array, ty.element) {
                        (Some(n), _) => (ty.element_type().flat_components(structs), n),
                        (None, Element::Basic(Basic::Vector(_, n))) => (1, n as u32),
                        (None, Element::Basic(Basic::Matrix(c, r))) => (r as u32, c as u32),
                        _ => (1, 1),
                    };
                    match k {
                        Some(i) => base += i * stride,
                        None => dynamic.push((*v, stride, count)),
                    }
                }
                Step::Swizzle(s) => swizzle = Some(*s),
            }
        }
        let n = leaf.flat_components(structs);
        let comps: Vec<u32> = match swizzle {
            Some(s) => s.components().iter().map(|&c| base + c as u32).collect(),
            None => (base..base + n).collect(),
        };
        (comps, dynamic)
    }

    /// Reads through a path (any variable kind).
    fn read_path(&mut self, root: VarId, steps: &[(Step, Type)], leaf: Type) -> Vec<Value> {
        let (comps, dynamic) = self.path_offsets(steps, leaf);
        let var = self.s.var(root);
        let mut tys = Vec::new();
        lower::component_types(var.ty, self.structs(), &mut tys);
        if dynamic.is_empty() {
            let kind = var.kind;
            return comps
                .iter()
                .map(|&c| {
                    let ty = tys.get(c as usize).copied().unwrap_or(Ty::F32);
                    self.read_component(root, kind, c, ty)
                })
                .collect();
        }
        // A computed index: uniforms and blocks load at a computed offset;
        // everything else selects among the possible elements.
        match var.kind {
            VarKind::Uniform if !self.layout.samplers.contains_key(&root) => {
                self.read_uniform_dynamic(root, &comps, &dynamic, &tys)
            }
            VarKind::BlockInstance(_) | VarKind::BlockMember(..) => {
                self.read_block_dynamic(root, &comps, &dynamic, &tys)
            }
            _ => {
                let kind = var.kind;
                let total: u32 = var.ty.flat_components(self.structs());
                comps
                    .iter()
                    .map(|&c| {
                        let ty = tys.get(c as usize).copied().unwrap_or(Ty::F32);
                        self.select_component(root, kind, c, &dynamic, total, ty)
                    })
                    .collect()
            }
        }
    }

    /// Component `c` (static part) plus the dynamic offsets, as a chain of
    /// selects over every place it can be.
    fn select_component(
        &mut self,
        root: VarId,
        kind: VarKind,
        c: u32,
        dynamic: &[(Value, u32, u32)],
        total: u32,
        ty: Ty,
    ) -> Value {
        // Every combination of dynamic indices.
        let mut places: Vec<(u32, Vec<(Value, u32)>)> = vec![(c, Vec::new())];
        for &(v, stride, count) in dynamic {
            let mut next = Vec::with_capacity(places.len() * count as usize);
            for (base, conds) in &places {
                for i in 0..count {
                    let mut cs = conds.clone();
                    cs.push((v, i));
                    next.push((base + i * stride, cs));
                }
            }
            places = next;
        }
        let mut result: Option<Value> = None;
        for (place, conds) in places.into_iter().rev() {
            if place >= total {
                continue;
            }
            let value = self.read_component(root, kind, place, ty);
            result = Some(match result {
                // The last candidate is the default (an out-of-range index
                // reads it: undefined in GLSL, harmless here).
                None => value,
                Some(rest) => {
                    let cond = self.index_matches(&conds);
                    self.select(cond, value, rest, ty)
                }
            });
        }
        result.unwrap_or_else(|| self.zero(ty))
    }

    /// The condition that each (index value, element) pair matches.
    fn index_matches(&mut self, conds: &[(Value, u32)]) -> Value {
        let mut acc: Option<Value> = None;
        for &(v, i) in conds {
            let k = self.konst_value(ops::Value::from_bits(self.func.ty(v), i));
            let eq = self.op(Op::IEq, &[v, k], Ty::Bool);
            acc = Some(match acc {
                None => eq,
                Some(a) => self.op(Op::BAnd, &[a, eq], Ty::Bool),
            });
        }
        acc.unwrap_or_else(|| self.konst_value(ops::Value::B(true)))
    }

    fn read_uniform_dynamic(
        &mut self,
        root: VarId,
        comps: &[u32],
        dynamic: &[(Value, u32, u32)],
        tys: &[Ty],
    ) -> Vec<Value> {
        let var = self.s.var(root);
        let Some(&base) = self.layout.uniforms.get(&root) else {
            return comps.iter().map(|&c| self.zero(tys[c as usize])).collect();
        };
        let total_slots = link::slot_count(var.ty, self.structs());
        // The slot offset of one step of each dynamic index: the distance
        // between the first selected component and the same component of
        // the next element (layouts are regular within an array).
        let c0 = comps.first().copied().unwrap_or(0);
        let total = var.ty.flat_components(self.structs());
        let slot_strides: Vec<u32> = dynamic
            .iter()
            .map(|&(_, stride, count)| {
                if count < 2 || c0 + stride >= total {
                    return 0;
                }
                let a = self.uniform_place(var.ty, c0).0;
                let b = self.uniform_place(var.ty, c0 + stride).0;
                b.saturating_sub(a)
            })
            .collect();
        let mut off: Option<Value> = None;
        for (i, &(v, _, _)) in dynamic.iter().enumerate() {
            let iv = if self.func.ty(v) == Ty::U32 { self.op(Op::Bitcast, &[v], Ty::I32) } else { v };
            let s = self.int(slot_strides[i] as i32);
            let term = self.op(Op::IMul, &[iv, s], Ty::I32);
            off = Some(match off {
                None => term,
                Some(a) => self.op(Op::IAdd, &[a, term], Ty::I32),
            });
        }
        let off = off.unwrap_or_else(|| self.int(0));
        comps
            .iter()
            .map(|&c| {
                let (slot, comp) = self.uniform_place(var.ty, c);
                let ty = tys[c as usize];
                // Clamp so that slot + offset stays inside the variable.
                let count = total_slots.saturating_sub(slot).max(1);
                let raw = self.emit(
                    InstOp::LoadUniformIndexed { base: base + slot, comp, stride: 1, count },
                    vec![off],
                    Ty::U32,
                );
                self.uniform_retype(raw, ty)
            })
            .collect()
    }

    fn read_block_dynamic(
        &mut self,
        root: VarId,
        comps: &[u32],
        dynamic: &[(Value, u32, u32)],
        tys: &[Ty],
    ) -> Vec<Value> {
        let var = self.s.var(root);
        let (block_index, member) = match var.kind {
            VarKind::BlockInstance(b) => (b, None),
            VarKind::BlockMember(b, m) => (b, Some(m)),
            _ => return comps.iter().map(|&c| self.zero(tys[c as usize])).collect(),
        };
        let block = &self.s.blocks[block_index as usize];
        // Instance arrays select the block itself.
        if member.is_none() && block.array.is_some() {
            let kind = var.kind;
            let total = var.ty.flat_components(self.structs());
            return comps
                .iter()
                .map(|&c| self.select_component(root, kind, c, dynamic, total, tys[c as usize]))
                .collect();
        }
        // Byte strides of the dynamic indices: the distance between the
        // first selected component and the same one of the next element.
        let c0 = comps.first().copied().unwrap_or(0);
        let total = var.ty.flat_components(self.structs());
        let strides: Vec<u32> = dynamic
            .iter()
            .map(|&(_, stride, count)| {
                if count < 2 || c0 + stride >= total {
                    return 0;
                }
                let a = self.block_place(root, c0).map_or(0, |p| p.1);
                let b = self.block_place(root, c0 + stride).map_or(a, |p| p.1);
                b.saturating_sub(a)
            })
            .collect();
        let mut off: Option<Value> = None;
        for (i, &(v, _, _)) in dynamic.iter().enumerate() {
            let iv = if self.func.ty(v) == Ty::U32 { self.op(Op::Bitcast, &[v], Ty::I32) } else { v };
            let s = self.int(strides[i] as i32);
            let term = self.op(Op::IMul, &[iv, s], Ty::I32);
            off = Some(match off {
                None => term,
                Some(a) => self.op(Op::IAdd, &[a, term], Ty::I32),
            });
        }
        let off = off.unwrap_or_else(|| self.int(0));
        let size = self
            .linked
            .blocks
            .get(self.layout.blocks.get(&block_index).copied().unwrap_or(0) as usize)
            .map_or(16, |b| b.size);
        comps
            .iter()
            .map(|&c| {
                let ty = tys[c as usize];
                let Some((blk, offset, _)) = self.block_place(root, c) else { return self.zero(ty) };
                let raw =
                    self.emit(InstOp::LoadBlock { block: blk, offset, stride: 1, count: size }, vec![off], Ty::U32);
                self.uniform_retype(raw, ty)
            })
            .collect()
    }

    /// Writes `vals` through a path.
    fn write_path(&mut self, root: VarId, steps: &[(Step, Type)], leaf: Type, vals: &[Value]) {
        let (comps, dynamic) = self.path_offsets(steps, leaf);
        let var = self.s.var(root);
        if dynamic.is_empty() {
            for (&c, &v) in comps.iter().zip(vals) {
                self.defs.insert((root.0, c), v);
            }
            return;
        }
        // Every place the value may go keeps its value unless the index
        // matches it.
        let total = var.ty.flat_components(self.structs());
        let mut tys = Vec::new();
        lower::component_types(var.ty, self.structs(), &mut tys);
        for (&c, &v) in comps.iter().zip(vals) {
            let mut places: Vec<(u32, Vec<(Value, u32)>)> = vec![(c, Vec::new())];
            for &(iv, stride, count) in &dynamic {
                let mut next = Vec::new();
                for (base, conds) in &places {
                    for i in 0..count {
                        let mut cs = conds.clone();
                        cs.push((iv, i));
                        next.push((base + i * stride, cs));
                    }
                }
                places = next;
            }
            for (place, conds) in places {
                if place >= total {
                    continue;
                }
                let ty = tys.get(place as usize).copied().unwrap_or(Ty::F32);
                let old = self.defs.get(&(root.0, place)).copied().unwrap_or_else(|| self.zero(ty));
                let cond = self.index_matches(&conds);
                let new = self.select(cond, v, old, ty);
                self.defs.insert((root.0, place), new);
            }
        }
    }

    // ---- Expressions -------------------------------------------------------

    /// Whether evaluating `e` can change state (so it must not be evaluated
    /// speculatively).
    fn has_effects(e: &Expr) -> bool {
        match &e.kind {
            H::Const(_) | H::Var(_) => false,
            H::Assign(..) | H::CompoundAssign(..) | H::IncDec { .. } | H::Call(..) => true,
            H::Builtin(Builtin::Modf, _) => true,
            H::Unary(_, a) | H::Swizzle(a, _) | H::Field(a, _) => Self::has_effects(a),
            H::Binary(_, a, b) | H::Logic(_, a, b) | H::Index(a, b) | H::Sequence(a, b) => {
                Self::has_effects(a) || Self::has_effects(b)
            }
            H::Ternary(c, a, b) => Self::has_effects(c) || Self::has_effects(a) || Self::has_effects(b),
            H::Construct(args) | H::Builtin(_, args) => args.iter().any(Self::has_effects),
        }
    }

    /// Evaluates an expression to its components.
    fn expr(&mut self, e: &Expr) -> Vec<Value> {
        if self.dead {
            let mut tys = Vec::new();
            lower::component_types(e.ty, self.structs(), &mut tys);
            return tys.iter().map(|&t| self.zero(t)).collect();
        }
        match &e.kind {
            H::Const(c) => c.iter().map(|&v| self.konst_value(v)).collect(),
            H::Var(id) => self.read_var(*id),
            H::Field(..) | H::Swizzle(..) | H::Index(..) => {
                if let Some((root, _, steps)) = self.path(e) {
                    return self.read_path(root, &steps, e.ty);
                }
                // Indexing an rvalue: evaluate it, then pick.
                self.rvalue_access(e)
            }
            H::Unary(op, a) => {
                let v = self.expr(a);
                lower::unary(self, *op, a.ty, &v)
            }
            H::Binary(op, a, b) => {
                let l = self.expr(a);
                let r = self.expr(b);
                let structs = self.structs();
                lower::binary(self, *op, a.ty, &l, b.ty, &r, structs)
            }
            H::Logic(op, a, b) => {
                let l = self.expr(a)[0];
                if !Self::has_effects(b) {
                    let r = self.expr(b)[0];
                    let o = if *op == LogicOp::And { Op::BAnd } else { Op::BOr };
                    return vec![self.op(o, &[l, r], Ty::Bool)];
                }
                // Short-circuit for real.
                let key = self.temp();
                self.defs.insert((key, 0), l);
                let cond = if *op == LogicOp::And { l } else { self.op(Op::BNot, &[l], Ty::Bool) };
                let b = (**b).clone();
                self.if_then(cond, |s| {
                    let r = s.expr(&b)[0];
                    s.defs.insert((key, 0), r);
                });
                vec![self.defs.get(&(key, 0)).copied().unwrap_or(l)]
            }
            H::Ternary(c, a, b) => {
                let cv = self.expr(c)[0];
                if !Self::has_effects(a) && !Self::has_effects(b) {
                    let x = self.expr(a);
                    let y = self.expr(b);
                    let mut tys = Vec::new();
                    lower::component_types(e.ty, self.structs(), &mut tys);
                    return x.iter().zip(&y).zip(&tys).map(|((&p, &q), &t)| self.select(cv, p, q, t)).collect();
                }
                let key = self.temp();
                let (a, b) = ((**a).clone(), (**b).clone());
                let mut tys = Vec::new();
                lower::component_types(e.ty, self.structs(), &mut tys);
                for (i, &t) in tys.iter().enumerate() {
                    let z = self.zero(t);
                    self.defs.insert((key, i as u32), z);
                }
                self.if_else(
                    cv,
                    |s| {
                        let v = s.expr(&a);
                        for (i, x) in v.into_iter().enumerate() {
                            s.defs.insert((key, i as u32), x);
                        }
                    },
                    |s| {
                        let v = s.expr(&b);
                        for (i, x) in v.into_iter().enumerate() {
                            s.defs.insert((key, i as u32), x);
                        }
                    },
                );
                (0..tys.len() as u32)
                    .map(|i| self.defs.get(&(key, i)).copied().unwrap_or_else(|| self.zero(tys[i as usize])))
                    .collect()
            }
            H::Assign(target, value) => {
                // The target's indices are evaluated once, before the value.
                let place = self.path(target);
                let v = self.expr(value);
                if let Some((root, _, steps)) = place {
                    self.write_path(root, &steps, target.ty, &v);
                }
                v
            }
            H::CompoundAssign(op, target, value) => {
                let Some((root, _, steps)) = self.path(target) else { return self.expr(value) };
                let r = self.expr(value);
                let l = self.read_path(root, &steps, target.ty);
                let structs = self.structs();
                let v = lower::binary(self, *op, target.ty, &l, value.ty, &r, structs);
                self.write_path(root, &steps, target.ty, &v);
                v
            }
            H::IncDec { target, increment, prefix } => {
                let Some((root, _, steps)) = self.path(target) else { return Vec::new() };
                let old = self.read_path(root, &steps, target.ty);
                let s = target.ty.as_basic().and_then(Basic::scalar).unwrap_or(Scalar::Float);
                let one = match s {
                    Scalar::Float => self.konst_value(ops::Value::F(1.0)),
                    Scalar::Uint => self.konst_value(ops::Value::U(1)),
                    _ => self.int(1),
                };
                let one_ty = Type::basic(Basic::Scalar(s));
                let op = if *increment { BinOp::Add } else { BinOp::Sub };
                let structs = self.structs();
                let new = lower::binary(self, op, target.ty, &old, one_ty, &[one], structs);
                self.write_path(root, &steps, target.ty, &new);
                if *prefix { new } else { old }
            }
            H::Construct(args) => {
                let vals: Vec<(Type, Vec<Value>)> = args.iter().map(|a| (a.ty, self.expr(a))).collect();
                let structs = self.structs();
                lower::construct(self, e.ty, &vals, structs)
            }
            H::Call(f, args) => self.call(*f, args),
            H::Builtin(b, args) => self.builtin(*b, args, e.ty),
            H::Sequence(a, b) => {
                self.expr(a);
                self.expr(b)
            }
        }
    }

    /// Field, swizzle or index of a value that is not a variable.
    fn rvalue_access(&mut self, e: &Expr) -> Vec<Value> {
        match &e.kind {
            H::Field(b, i) => {
                let v = self.expr(b);
                let Some(id) = b.ty.as_struct() else { return v };
                let structs = self.structs();
                let fields = &structs.get(id).fields;
                let start: u32 = fields[..*i as usize].iter().map(|f| f.ty.flat_components(structs)).sum();
                let n = fields[*i as usize].ty.flat_components(structs);
                v[start as usize..(start + n) as usize].to_vec()
            }
            H::Swizzle(b, s) => {
                let v = self.expr(b);
                s.components().iter().map(|&c| v[c as usize]).collect()
            }
            H::Index(b, i) => {
                let v = self.expr(b);
                let iv = self.expr(i)[0];
                let size = e.ty.flat_components(self.structs()) as usize;
                let count = v.len() / size.max(1);
                if let Some(k) = self.func.constant(iv) {
                    let k = (k.bits() as usize).min(count.saturating_sub(1));
                    return v[k * size..(k + 1) * size].to_vec();
                }
                let mut tys = Vec::new();
                lower::component_types(e.ty, self.structs(), &mut tys);
                (0..size)
                    .map(|c| {
                        let mut result = v[(count - 1) * size + c];
                        for k in (0..count - 1).rev() {
                            let cond = self.index_matches(&[(iv, k as u32)]);
                            result = self.select(cond, v[k * size + c], result, tys[c]);
                        }
                        result
                    })
                    .collect()
            }
            _ => self.expr(e),
        }
    }

    /// Inlines a call.
    fn call(&mut self, f: FuncId, args: &[Expr]) -> Vec<Value> {
        let func = self.s.function(f);
        let params = func.params.clone();
        // Evaluate the arguments left to right; out and inout arguments
        // remember their place (its indices evaluated now, once).
        type Place = (VarId, Vec<(Step, Type)>, Type);
        let mut outs: Vec<(VarId, Place)> = Vec::new();
        let mut values: Vec<(VarId, Vec<Value>)> = Vec::new();
        for (p, a) in params.iter().zip(args) {
            let mode = match self.s.var(*p).kind {
                VarKind::Param(m) => m,
                _ => ParamMode::In,
            };
            match mode {
                ParamMode::In => {
                    let v = self.expr(a);
                    values.push((*p, v));
                }
                ParamMode::Out | ParamMode::InOut => {
                    let Some((root, _, steps)) = self.path(a) else { continue };
                    let v = if mode == ParamMode::InOut {
                        self.read_path(root, &steps, a.ty)
                    } else {
                        let ty = self.s.var(*p).ty;
                        let mut tys = Vec::new();
                        lower::component_types(ty, self.structs(), &mut tys);
                        tys.iter().map(|&t| self.zero(t)).collect()
                    };
                    outs.push((*p, (root, steps, a.ty)));
                    values.push((*p, v));
                }
            }
        }
        for (p, v) in values {
            self.write_whole(p, &v);
        }
        let ret = self.inline_body(f, &[]);
        for (p, (root, steps, ty)) in outs {
            let v = self.read_var(p);
            self.write_path(root, &steps, ty, &v);
        }
        ret
    }

    fn builtin(&mut self, b: Builtin, args: &[Expr], ret: Type) -> Vec<Value> {
        match b {
            Builtin::Texture(t) => return self.texture(t, args),
            Builtin::TextureSize => return self.texture_size(args),
            Builtin::DFdx | Builtin::DFdy | Builtin::Fwidth => {
                let v = self.expr(&args[0]);
                return v
                    .iter()
                    .map(|&x| {
                        if self.func.constant(x).is_some() {
                            return self.konst_value(ops::Value::F(0.0));
                        }
                        match b {
                            Builtin::DFdx => self.emit(InstOp::Deriv { y: false }, vec![x], Ty::F32),
                            Builtin::DFdy => self.emit(InstOp::Deriv { y: true }, vec![x], Ty::F32),
                            _ => {
                                let dx = self.emit(InstOp::Deriv { y: false }, vec![x], Ty::F32);
                                let dy = self.emit(InstOp::Deriv { y: true }, vec![x], Ty::F32);
                                let ax = self.op(Op::FAbs, &[dx], Ty::F32);
                                let ay = self.op(Op::FAbs, &[dy], Ty::F32);
                                self.op(Op::FAdd, &[ax, ay], Ty::F32)
                            }
                        }
                    })
                    .collect();
            }
            _ => {}
        }
        if b == Builtin::Modf {
            // modf(x, out i): the place of i is evaluated once.
            let x = self.expr(&args[0]);
            let place = self.path(&args[1]);
            let zeros: Vec<Value> = x.iter().map(|_| self.konst_value(ops::Value::F(0.0))).collect();
            let vals = [(args[0].ty, x), (args[1].ty, zeros)];
            let (result, out) = lower::builtin(self, b, &vals, ret);
            if let (Some(o), Some((root, _, steps))) = (out, place) {
                self.write_path(root, &steps, args[1].ty, &o);
            }
            return result;
        }
        let vals: Vec<(Type, Vec<Value>)> = args.iter().map(|a| (a.ty, self.expr(a))).collect();
        let (result, _) = lower::builtin(self, b, &vals, ret);
        result
    }

    /// The sampler index value of a sampler expression: (index, constant?).
    fn sampler_index(&mut self, e: &Expr) -> (Value, u32) {
        let v = self.expr(e);
        let count = self.linked.samplers.len().max(1) as u32;
        (v.first().copied().unwrap_or_else(|| self.int(0)), count)
    }

    fn texture(&mut self, t: crate::builtins::TexCall, args: &[Expr]) -> Vec<Value> {
        let (index_v, count) = self.sampler_index(&args[0]);
        let coord = self.expr(&args[1]);
        let mut rest: Vec<Vec<Value>> = args[2..].iter().map(|a| self.expr(a)).collect();
        let s = t.sampler;
        // Projection: divide the coordinates (and the reference) by q.
        let mut coords: Vec<Value> = if t.proj {
            let q = *coord.last().unwrap_or(&coord[0]);
            let inv = {
                let one = self.konst_value(ops::Value::F(1.0));
                self.op(Op::FDiv, &[one, q], Ty::F32)
            };
            // Projection exists for 2D and 3D lookups: s, t (r) and the
            // shadow reference are divided.
            let n = s.coords() as usize + usize::from(s.shadow);
            // For a 2D lookup with a vec4, z is ignored: take x, y (and the
            // reference, which is z for shadow lookups).
            let take: Vec<usize> =
                if s.dim == crate::types::Dim::D2 && !s.shadow { vec![0, 1] } else { (0..n).collect() };
            take.iter().map(|&i| self.op(Op::FMul, &[coord[i], inv], Ty::F32)).collect()
        } else {
            coord.clone()
        };
        if t.lod == TexLod::Fetch {
            coords = coord;
        }
        let mut extra: Vec<Value> = Vec::new();
        let mut offset = [0i8; 3];
        // Normalised order: [bias | lod | dPdx dPdy], offset.
        if t.offset
            && let Some(o) = rest.pop()
        {
            for (i, &v) in o.iter().enumerate().take(3) {
                offset[i] = self.func.constant(v).map_or(0, |c| c.bits() as i32 as i8);
            }
        }
        let lod = match t.lod {
            TexLod::Implicit if t.bias => {
                extra.extend(rest.first().cloned().unwrap_or_default());
                TexLod::Implicit
            }
            TexLod::Lod | TexLod::Fetch => {
                extra.extend(rest.first().cloned().unwrap_or_default());
                t.lod
            }
            TexLod::Grad => {
                for g in rest.iter().take(2) {
                    extra.extend(g.iter().copied());
                }
                TexLod::Grad
            }
            l => l,
        };
        // A vertex shader's implicit lookups use the base level.
        let (lod, bias) = if self.s.stage == Stage::Vertex && lod == TexLod::Implicit {
            extra.clear();
            extra.push(self.konst_value(ops::Value::F(0.0)));
            (TexLod::Lod, false)
        } else {
            (lod, t.bias)
        };
        let constant = self.func.constant(index_v);
        let mut targs = Vec::new();
        if constant.is_none() {
            targs.push(index_v);
        }
        let ncoords = coords.len() as u8;
        targs.extend(coords);
        targs.extend(extra);
        let op = TexOp {
            sampler: s,
            index: constant.map_or(0, |c| c.bits()),
            dynamic: constant.is_none(),
            count,
            lod,
            bias,
            offset,
            coords: ncoords,
        };
        let ty = lower::ty(s.ty);
        let results = op.results();
        let tys: Vec<Ty> = if s.shadow { vec![Ty::F32] } else { vec![ty; results] };
        self.emit_multi(InstOp::Tex(op), targs, &tys)
    }

    fn texture_size(&mut self, args: &[Expr]) -> Vec<Value> {
        let (index_v, count) = self.sampler_index(&args[0]);
        let lod = self.expr(&args[1])[0];
        let Some(Basic::Sampler(s)) = args[0].ty.as_basic() else { return Vec::new() };
        let n = match s.dim {
            crate::types::Dim::D2 | crate::types::Dim::Cube => 2,
            _ => 3,
        };
        let constant = self.func.constant(index_v);
        let mut targs = Vec::new();
        if constant.is_none() {
            targs.push(index_v);
        }
        targs.push(lod);
        let op =
            InstOp::TexSize { sampler: s, index: constant.map_or(0, |c| c.bits()), dynamic: constant.is_none(), count };
        self.emit_multi(op, targs, &vec![Ty::I32; n])
    }
}

/// The basic type of each flattened component of `t` (samplers count as
/// one component).
fn component_basics(t: Type, structs: &Structs, out: &mut Vec<Basic>) {
    for _ in 0..t.array.unwrap_or(1) {
        match t.element {
            Element::Basic(b @ Basic::Sampler(_)) => out.push(b),
            Element::Basic(b) => {
                for _ in 0..b.components() {
                    out.push(b);
                }
            }
            Element::Struct(id) => {
                for f in &structs.get(id).fields {
                    component_basics(f.ty, structs, out);
                }
            }
        }
    }
}
