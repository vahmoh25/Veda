//! Optimisation passes.
//!
//! * [`simplify`]: replaces trivial φs (all operands the same value, or the
//!   φ itself), folds operations whose operands became constant, applies
//!   exact algebraic identities (`x * 1.0`, `x - 0.0`, `i + 0`, `b && true`,
//!   `select(c, x, x)`...; nothing that changes a NaN, an infinity or the
//!   sign of a zero), and forwards copies.
//! * [`cse`]: value numbering over the dominator tree of the structure:
//!   an instruction is replaced by an identical one that dominates it.
//!   Texture lookups and derivatives are only shared within a block (their
//!   results depend on which neighbouring invocations run them).
//! * [`dce`]: removes instructions whose results nothing uses.
//! * [`if_convert`]: an `if` whose branches are short, jump nowhere and
//!   only compute becomes straight-line code and selects, which a SIMD
//!   interpreter runs without managing masks.
//!
//! [`optimize`] runs them to a fixed point (bounded).

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use super::{Block, Func, InstOp, Node, Value};
use crate::ops::{self, Op};

/// Runs the passes until nothing changes (at most a few rounds).
pub fn optimize(f: &mut Func) {
    for _ in 0..8 {
        let mut changed = simplify(f);
        changed |= cse(f);
        changed |= if_convert(f);
        changed |= dce(f);
        if !changed {
            break;
        }
    }
}

/// Replaces every use of a value by another (following chains).
fn apply(f: &mut Func, repl: &[Value]) {
    let resolve = |v: Value| resolve(repl, v);
    for inst in &mut f.insts {
        for a in &mut inst.args {
            *a = resolve(*a);
        }
    }
    fn conds(list: &mut [Node], resolve: &dyn Fn(Value) -> Value) {
        for n in list {
            match n {
                Node::Block(_) => {}
                Node::If(i) => {
                    i.cond = resolve(i.cond);
                    conds(&mut i.then, resolve);
                    conds(&mut i.otherwise, resolve);
                }
                Node::Loop(l) => conds(&mut l.body, resolve),
            }
        }
    }
    conds(&mut f.body, &resolve);
}

fn identity(f: &Func) -> Vec<Value> {
    (0..f.values.len() as u32).map(Value).collect()
}

/// A constant of the function, created in the entry block if needed.
fn constant(f: &mut Func, v: ops::Value) -> Value {
    let entry = super::first_block(&f.body).unwrap_or(Block(0));
    // Reuse an existing one.
    for &i in &f.blocks[entry.0 as usize].insts {
        let inst = &f.insts[i.0 as usize];
        if let InstOp::Const(c) = inst.op
            && c.same(v)
        {
            return inst.results[0];
        }
    }
    let id = f.create(InstOp::Const(v), Vec::new(), &[v.ty()]);
    f.blocks[entry.0 as usize].insts.insert(0, id);
    f.insts[id.0 as usize].results[0]
}

/// Trivial φs, folding, identities, copies. Returns whether anything
/// changed.
pub fn simplify(f: &mut Func) -> bool {
    let mut changed = false;
    for _ in 0..64 {
        let mut repl = identity(f);
        let mut any = false;
        let order = f.block_order();
        for &b in &order {
            let ids = f.blocks[b.0 as usize].insts.clone();
            for id in ids {
                let inst = f.insts[id.0 as usize].clone();
                let Some(&result) = inst.results.first() else { continue };
                if repl[result.0 as usize] != result {
                    continue;
                }
                let args: Vec<Value> = inst.args.iter().map(|&a| resolve(&repl, a)).collect();
                let new: Option<Value> = match &inst.op {
                    InstOp::Phi => {
                        // All operands the φ itself or one other value.
                        let mut other = None;
                        let mut trivial = true;
                        for &a in &args {
                            if a == result || Some(a) == other {
                                continue;
                            }
                            if other.is_some() {
                                trivial = false;
                                break;
                            }
                            other = Some(a);
                        }
                        if trivial { other } else { None }
                    }
                    InstOp::Select => {
                        if let Some(c) = f.constant(args[0]) {
                            Some(if c.bits() != 0 { args[1] } else { args[2] })
                        } else if args[1] == args[2] {
                            Some(args[1])
                        } else {
                            None
                        }
                    }
                    InstOp::Op(op) => {
                        let consts: Option<Vec<ops::Value>> = args.iter().map(|&a| f.constant(a)).collect();
                        let ty = f.ty(result);
                        match consts {
                            Some(c) => {
                                let k = constant(f, ops::eval(*op, &c, ty));
                                // The table covers values created since.
                                while repl.len() < f.values.len() {
                                    let n = repl.len() as u32;
                                    repl.push(Value(n));
                                }
                                Some(k)
                            }
                            None => identity_of(f, *op, &args),
                        }
                    }
                    _ => None,
                };
                if let Some(v) = new
                    && v != result
                {
                    repl[result.0 as usize] = v;
                    any = true;
                }
            }
        }
        if !any {
            break;
        }
        changed = true;
        apply(f, &repl);
        remove_replaced(f, &repl);
    }
    changed
}

/// Drops instructions whose results were replaced.
fn remove_replaced(f: &mut Func, repl: &[Value]) {
    for b in 0..f.blocks.len() {
        let insts = core::mem::take(&mut f.blocks[b].insts);
        f.blocks[b].insts = insts
            .into_iter()
            .filter(|&i| f.insts[i.0 as usize].results.iter().all(|r| repl.get(r.0 as usize).is_none_or(|x| x == r)))
            .collect();
    }
}

/// `op(args)` when an exact identity makes it one of its operands.
fn identity_of(f: &Func, op: Op, args: &[Value]) -> Option<Value> {
    let c = |i: usize| f.constant(args[i]);
    let is = |v: Option<ops::Value>, bits: u32| v.is_some_and(|x| x.bits() == bits);
    let one_f = 1.0f32.to_bits();
    match op {
        // x * 1 and 1 * x (NaN stays NaN; signs are kept).
        Op::FMul if is(c(1), one_f) => Some(args[0]),
        Op::FMul if is(c(0), one_f) => Some(args[1]),
        // x - 0.0 is exact (x + 0.0 is not, for x = -0.0).
        Op::FSub if is(c(1), 0) => Some(args[0]),
        Op::FDiv if is(c(1), one_f) => Some(args[0]),
        Op::IAdd | Op::IOr | Op::IXor if is(c(1), 0) => Some(args[0]),
        Op::IAdd | Op::IOr | Op::IXor if is(c(0), 0) => Some(args[1]),
        Op::ISub | Op::IShl | Op::IShr | Op::UShr if is(c(1), 0) => Some(args[0]),
        Op::IMul if is(c(1), 1) => Some(args[0]),
        Op::IMul if is(c(0), 1) => Some(args[1]),
        Op::IAnd if is(c(1), !0) => Some(args[0]),
        Op::IAnd if is(c(0), !0) => Some(args[1]),
        Op::BAnd if is(c(1), !0) => Some(args[0]),
        Op::BAnd if is(c(0), !0) => Some(args[1]),
        Op::BOr if is(c(1), 0) => Some(args[0]),
        Op::BOr if is(c(0), 0) => Some(args[1]),
        Op::BAnd | Op::BOr if args[0] == args[1] => Some(args[0]),
        Op::IMin | Op::IMax | Op::UMin | Op::UMax if args[0] == args[1] => Some(args[0]),
        _ => None,
    }
}

/// Whether an instruction can be shared with an identical one elsewhere in
/// the dominator tree (rather than only within its block).
fn shareable_across_blocks(op: &InstOp) -> bool {
    !matches!(op, InstOp::Tex(_) | InstOp::Deriv { .. } | InstOp::TexSize { .. } | InstOp::Phi)
}

/// Whether an instruction is a pure computation (no effect, result
/// determined by its operands).
fn pure(op: &InstOp) -> bool {
    !matches!(op, InstOp::StoreOutput { .. } | InstOp::StoreBuiltin(_) | InstOp::Phi | InstOp::Nop)
}

/// A key identifying an instruction's computation.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Key {
    op: alloc::string::String,
    args: Vec<u32>,
    tys: Vec<u8>,
}

fn key(f: &Func, id: super::InstId) -> Key {
    let inst = &f.insts[id.0 as usize];
    let mut args: Vec<u32> = inst.args.iter().map(|v| v.0).collect();
    if let InstOp::Op(op) = inst.op
        && op.commutative()
        && args.len() == 2
    {
        args.sort_unstable();
    }
    let op = match &inst.op {
        // Constants by type and bits.
        InstOp::Const(c) => alloc::format!("const {:?} {}", c.ty(), c.bits()),
        other => alloc::format!("{other:?}"),
    };
    Key { op, args, tys: inst.results.iter().map(|&r| f.ty(r) as u8).collect() }
}

/// Global value numbering over the structure's dominance.
pub fn cse(f: &mut Func) -> bool {
    let mut repl = identity(f);
    let mut any = false;
    let mut scopes: Vec<BTreeMap<Key, Value>> = vec![BTreeMap::new()];
    let body = core::mem::take(&mut f.body);
    cse_list(f, &body, &mut scopes, &mut repl, &mut any);
    f.body = body;
    if any {
        apply(f, &repl);
        remove_replaced(f, &repl);
    }
    any
}

fn cse_list(f: &Func, list: &[Node], scopes: &mut Vec<BTreeMap<Key, Value>>, repl: &mut [Value], any: &mut bool) {
    for n in list {
        match n {
            Node::Block(b) => {
                let mut local: BTreeMap<Key, Value> = BTreeMap::new();
                for &i in &f.blocks[b.0 as usize].insts {
                    let inst = &f.insts[i.0 as usize];
                    if !pure(&inst.op) || inst.results.len() != 1 {
                        continue;
                    }
                    let mut k = key(f, i);
                    // Operands as already replaced.
                    k.args = k.args.iter().map(|&a| resolve(repl, Value(a)).0).collect();
                    if let InstOp::Op(op) = inst.op
                        && op.commutative()
                    {
                        k.args.sort_unstable();
                    }
                    let r = inst.results[0];
                    let found = local.get(&k).copied().or_else(|| {
                        if shareable_across_blocks(&inst.op) {
                            scopes.iter().rev().find_map(|s| s.get(&k).copied())
                        } else {
                            None
                        }
                    });
                    match found {
                        Some(v) => {
                            repl[r.0 as usize] = v;
                            *any = true;
                        }
                        None => {
                            local.insert(k.clone(), r);
                            if shareable_across_blocks(&inst.op)
                                && let Some(s) = scopes.last_mut()
                            {
                                s.insert(k, r);
                            }
                        }
                    }
                }
            }
            Node::If(i) => {
                scopes.push(BTreeMap::new());
                cse_list(f, &i.then, scopes, repl, any);
                scopes.pop();
                scopes.push(BTreeMap::new());
                cse_list(f, &i.otherwise, scopes, repl, any);
                scopes.pop();
            }
            Node::Loop(l) => {
                scopes.push(BTreeMap::new());
                cse_list(f, &l.body, scopes, repl, any);
                scopes.pop();
            }
        }
    }
}

fn resolve(repl: &[Value], mut v: Value) -> Value {
    let mut n = 0;
    while let Some(&next) = repl.get(v.0 as usize) {
        if next == v || n == 64 {
            break;
        }
        v = next;
        n += 1;
    }
    v
}

/// Removes instructions whose results are unused. Returns whether anything
/// was removed.
pub fn dce(f: &mut Func) -> bool {
    let mut live = vec![false; f.values.len()];
    let mut work: Vec<Value> = Vec::new();
    // Roots: operands of effects, and if conditions.
    for b in f.block_order() {
        for &i in &f.blocks[b.0 as usize].insts {
            let inst = &f.insts[i.0 as usize];
            if inst.op.has_effect() {
                work.extend(inst.args.iter().copied());
            }
        }
    }
    fn conds(list: &[Node], work: &mut Vec<Value>) {
        for n in list {
            match n {
                Node::Block(_) => {}
                Node::If(i) => {
                    work.push(i.cond);
                    conds(&i.then, work);
                    conds(&i.otherwise, work);
                }
                Node::Loop(l) => conds(&l.body, work),
            }
        }
    }
    conds(&f.body, &mut work);
    while let Some(v) = work.pop() {
        if live[v.0 as usize] {
            continue;
        }
        live[v.0 as usize] = true;
        let def = f.values[v.0 as usize].inst;
        work.extend(f.insts[def.0 as usize].args.iter().copied());
    }
    let mut removed = false;
    for b in 0..f.blocks.len() {
        let before = f.blocks[b].insts.len();
        let insts = core::mem::take(&mut f.blocks[b].insts);
        f.blocks[b].insts = insts
            .into_iter()
            .filter(|&i| {
                let inst = &f.insts[i.0 as usize];
                inst.op.has_effect() || inst.results.iter().any(|r| live[r.0 as usize])
            })
            .collect();
        removed |= f.blocks[b].insts.len() != before;
    }
    if removed {
        prune(f);
    }
    removed
}

/// Removes `if` nodes with empty branches and no φs depending on them,
/// merging the blocks around them.
fn prune(f: &mut Func) {
    let mut body = core::mem::take(&mut f.body);
    prune_list(f, &mut body);
    f.body = body;
}

fn prune_list(f: &mut Func, list: &mut Vec<Node>) {
    let mut i = 0;
    while i < list.len() {
        match &mut list[i] {
            Node::If(x) => {
                prune_list(f, &mut x.then);
                prune_list(f, &mut x.otherwise);
            }
            Node::Loop(l) => prune_list(f, &mut l.body),
            Node::Block(_) => {}
        }
        if let Node::If(x) = &list[i] {
            let empty = |l: &[Node]| match l {
                [Node::Block(b)] => {
                    let d = &f.blocks[b.0 as usize];
                    d.insts.is_empty() && d.jump.is_none()
                }
                _ => false,
            };
            let merge = match list.get(i + 1) {
                Some(Node::Block(m)) => Some(*m),
                _ => None,
            };
            let phis = merge.is_some_and(|m| {
                f.blocks[m.0 as usize].insts.iter().any(|&id| f.insts[id.0 as usize].op == InstOp::Phi)
            });
            if empty(&x.then)
                && empty(&x.otherwise)
                && !phis
                && let (Some(Node::Block(before)), Some(m)) = (list.get(i.wrapping_sub(1)), merge)
            {
                // Merge `m` into the block before the if.
                let before = *before;
                let moved = core::mem::take(&mut f.blocks[m.0 as usize].insts);
                let jump = f.blocks[m.0 as usize].jump.take();
                f.blocks[before.0 as usize].insts.extend(moved);
                f.blocks[before.0 as usize].jump = jump;
                list.drain(i..=i + 1);
                continue;
            }
        }
        i += 1;
    }
}

/// The most instructions a branch may have to be if-converted.
const IF_CONVERT_LIMIT: usize = 24;

/// Turns short, side-effect-free `if`s into selects.
pub fn if_convert(f: &mut Func) -> bool {
    let mut body = core::mem::take(&mut f.body);
    let changed = if_convert_list(f, &mut body);
    f.body = body;
    changed
}

fn if_convert_list(f: &mut Func, list: &mut Vec<Node>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < list.len() {
        match &mut list[i] {
            Node::If(x) => {
                changed |= if_convert_list(f, &mut x.then);
                changed |= if_convert_list(f, &mut x.otherwise);
            }
            Node::Loop(l) => changed |= if_convert_list(f, &mut l.body),
            Node::Block(_) => {}
        }
        let convertible = |f: &Func, l: &[Node]| -> Option<Block> {
            let [Node::Block(b)] = l else { return None };
            let d = &f.blocks[b.0 as usize];
            if d.jump.is_some() || d.insts.len() > IF_CONVERT_LIMIT {
                return None;
            }
            let ok = d.insts.iter().all(|&id| {
                let op = &f.insts[id.0 as usize].op;
                matches!(
                    op,
                    InstOp::Op(_)
                        | InstOp::Select
                        | InstOp::Const(_)
                        | InstOp::LoadUniformIndexed { .. }
                        | InstOp::LoadBlock { .. }
                )
            });
            ok.then_some(*b)
        };
        if let Node::If(x) = &list[i]
            && let (Some(t), Some(e)) = (convertible(f, &x.then), convertible(f, &x.otherwise))
            && let (Some(Node::Block(before)), Some(Node::Block(merge))) =
                (list.get(i.wrapping_sub(1)), list.get(i + 1))
        {
            let (before, merge, cond) = (*before, *merge, x.cond);
            // Hoist both branches, then the merge's φs become selects.
            let mut moved = core::mem::take(&mut f.blocks[t.0 as usize].insts);
            moved.extend(core::mem::take(&mut f.blocks[e.0 as usize].insts));
            f.blocks[before.0 as usize].insts.extend(moved);
            let merge_insts = core::mem::take(&mut f.blocks[merge.0 as usize].insts);
            let mut rest = Vec::new();
            for id in merge_insts {
                let inst = f.insts[id.0 as usize].clone();
                if inst.op == InstOp::Phi && inst.args.len() == 2 {
                    // Operands in predecessor order: then, else.
                    let r = inst.results[0];
                    let ty = f.ty(r);
                    let _ = ty;
                    f.insts[id.0 as usize].op = InstOp::Select;
                    f.insts[id.0 as usize].args = vec![cond, inst.args[0], inst.args[1]];
                    f.blocks[before.0 as usize].insts.push(id);
                } else if inst.op == InstOp::Phi {
                    // One branch did not reach the merge: impossible here
                    // (neither jumps), keep it.
                    rest.push(id);
                } else {
                    rest.push(id);
                }
            }
            let jump = f.blocks[merge.0 as usize].jump.take();
            f.blocks[before.0 as usize].insts.extend(rest);
            f.blocks[before.0 as usize].jump = jump;
            list.drain(i..=i + 1);
            changed = true;
            continue;
        }
        i += 1;
    }
    changed
}
