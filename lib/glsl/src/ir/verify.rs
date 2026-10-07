//! Checks the IR's invariants; used after building and after each pass in
//! tests, and available to debug builds.
//!
//! * Lists alternate blocks and `if`/`loop` nodes, starting and ending
//!   with a block; a block that jumps ends its list.
//! * Every φ has one operand per predecessor and sits at its block's start.
//! * Operands have the types their operation takes.
//! * Every use is dominated by its definition (a φ operand by the end of
//!   the corresponding predecessor).

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::{Block, Func, InstOp, Node, ty_name};
use crate::ops::Ty;

/// Checks `f`; returns the first problem found.
pub fn verify(f: &Func) -> Result<(), String> {
    structure(f, &f.body, true)?;
    let preds = f.preds();
    let order = f.block_order();
    let idom = dominators(f, &order, &preds);
    // Where each value is defined: block and position.
    let mut def_at: Vec<Option<(Block, usize)>> = vec![None; f.values.len()];
    for &b in &order {
        for (pos, &i) in f.blocks[b.0 as usize].insts.iter().enumerate() {
            for &r in &f.insts[i.0 as usize].results {
                if def_at[r.0 as usize].is_some() {
                    return Err(alloc::format!("%{} is defined twice", r.0));
                }
                def_at[r.0 as usize] = Some((b, pos));
            }
        }
    }
    let dominates = |a: Block, b: Block| -> bool {
        let mut x = Some(b);
        while let Some(y) = x {
            if y == a {
                return true;
            }
            x = idom[y.0 as usize];
        }
        false
    };
    for &b in &order {
        let reachable = idom[b.0 as usize].is_some() || Some(b) == super::first_block(&f.body);
        let insts = &f.blocks[b.0 as usize].insts;
        let mut phis_done = false;
        for (pos, &i) in insts.iter().enumerate() {
            let inst = &f.insts[i.0 as usize];
            if inst.op == InstOp::Phi {
                if phis_done {
                    return Err(alloc::format!("b{}: φ after other instructions", b.0));
                }
                let n = preds[b.0 as usize].len();
                if reachable && inst.args.len() != n {
                    return Err(alloc::format!(
                        "b{}: φ %{} has {} operands for {} predecessors",
                        b.0,
                        inst.results[0].0,
                        inst.args.len(),
                        n
                    ));
                }
                if !reachable {
                    continue;
                }
                for (k, &a) in inst.args.iter().enumerate() {
                    let Some((db, _)) = def_at[a.0 as usize] else {
                        return Err(alloc::format!("b{}: φ operand %{} is not defined", b.0, a.0));
                    };
                    let p = preds[b.0 as usize][k];
                    if !dominates(db, p) {
                        return Err(alloc::format!(
                            "b{}: φ operand %{} does not dominate predecessor b{}",
                            b.0,
                            a.0,
                            p.0
                        ));
                    }
                }
                continue;
            }
            phis_done = true;
            if !reachable {
                continue;
            }
            for &a in &inst.args {
                let Some((db, dpos)) = def_at[a.0 as usize] else {
                    return Err(alloc::format!("b{}: %{} is used but not defined", b.0, a.0));
                };
                let ok = if db == b { dpos < pos } else { dominates(db, b) };
                if !ok {
                    return Err(alloc::format!(
                        "b{}: the use of %{} is not dominated by its definition (b{})",
                        b.0,
                        a.0,
                        db.0
                    ));
                }
            }
            types(f, i)?;
        }
    }
    // If conditions are bools defined before the `if`.
    fn conds(f: &Func, list: &[Node], def_at: &[Option<(Block, usize)>]) -> Result<(), String> {
        for n in list {
            match n {
                Node::Block(_) => {}
                Node::If(i) => {
                    if f.ty(i.cond) != Ty::Bool {
                        return Err(alloc::format!("if %{} is not a bool", i.cond.0));
                    }
                    if def_at[i.cond.0 as usize].is_none() {
                        return Err(alloc::format!("if %{} is not defined", i.cond.0));
                    }
                    conds(f, &i.then, def_at)?;
                    conds(f, &i.otherwise, def_at)?;
                }
                Node::Loop(l) => conds(f, &l.body, def_at)?,
            }
        }
        Ok(())
    }
    conds(f, &f.body, &def_at)
}

fn structure(f: &Func, list: &[Node], top: bool) -> Result<(), String> {
    if list.is_empty() {
        return Err("an empty list".into());
    }
    let _ = top;
    for (i, n) in list.iter().enumerate() {
        let is_block = matches!(n, Node::Block(_));
        if (i % 2 == 0) != is_block {
            return Err("lists must alternate blocks and if/loop nodes".into());
        }
        match n {
            Node::Block(b) => {
                if f.blocks[b.0 as usize].jump.is_some() && i + 1 != list.len() {
                    return Err(alloc::format!("b{} jumps but is not the last of its list", b.0));
                }
            }
            Node::If(x) => {
                structure(f, &x.then, false)?;
                structure(f, &x.otherwise, false)?;
            }
            Node::Loop(l) => structure(f, &l.body, false)?,
        }
    }
    if !matches!(list.last(), Some(Node::Block(_))) {
        return Err("a list must end with a block".into());
    }
    Ok(())
}

/// Immediate dominators (Cooper, Harvey and Kennedy's iterative method),
/// with program order as the reverse postorder (in structured control flow
/// only loop back edges go backwards).
pub fn dominators(f: &Func, order: &[Block], preds: &[Vec<Block>]) -> Vec<Option<Block>> {
    let mut index = vec![usize::MAX; f.blocks.len()];
    for (i, b) in order.iter().enumerate() {
        index[b.0 as usize] = i;
    }
    let mut idom: Vec<Option<Block>> = vec![None; f.blocks.len()];
    let Some(&entry) = order.first() else { return idom };
    idom[entry.0 as usize] = Some(entry);
    let intersect = |idom: &[Option<Block>], mut a: Block, mut b: Block| -> Block {
        while a != b {
            while index[a.0 as usize] > index[b.0 as usize] {
                a = idom[a.0 as usize].unwrap_or(entry);
            }
            while index[b.0 as usize] > index[a.0 as usize] {
                b = idom[b.0 as usize].unwrap_or(entry);
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order[1..] {
            let mut new: Option<Block> = None;
            for &p in &preds[b.0 as usize] {
                if idom[p.0 as usize].is_none() {
                    continue;
                }
                new = Some(match new {
                    None => p,
                    Some(n) => intersect(&idom, p, n),
                });
            }
            if new.is_some() && new != idom[b.0 as usize] {
                idom[b.0 as usize] = new;
                changed = true;
            }
        }
    }
    // The entry has no dominator.
    idom[entry.0 as usize] = None;
    idom
}

fn types(f: &Func, id: super::InstId) -> Result<(), String> {
    let inst = &f.insts[id.0 as usize];
    let bad = |what: &str| Err(alloc::format!("instruction %{:?}: {what}", inst.results.first().map(|r| r.0)));
    match &inst.op {
        InstOp::Op(op) => {
            let sig = op.signature();
            if inst.args.len() != sig.args.len() {
                return bad("wrong number of operands");
            }
            for (&a, &t) in inst.args.iter().zip(sig.args) {
                let at = f.ty(a);
                let ok = match t {
                    Ty::I32 => matches!(at, Ty::I32 | Ty::U32) || *op == crate::ops::Op::Bitcast,
                    other => at == other,
                };
                if !ok {
                    return bad(&alloc::format!("{op:?} takes {} but has {}", ty_name(t), ty_name(at)));
                }
            }
        }
        InstOp::Select
            if inst.args.len() != 3 || f.ty(inst.args[0]) != Ty::Bool || f.ty(inst.args[1]) != f.ty(inst.args[2]) =>
        {
            return bad("malformed select");
        }
        _ => {}
    }
    Ok(())
}
