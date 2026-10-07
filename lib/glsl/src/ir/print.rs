//! A textual form of the IR, for tests and debugging.
//!
//! ```text
//! b0:
//!   %0:f32 = const 1
//!   %1:u32 = uniform 0.x
//!   %2:f32 = FMul %0, %3
//!   if %4 {
//!     b1:
//!       break
//!   } else {
//!     b2:
//!   }
//!   b3:
//!     %5:f32 = phi %2, %6
//! ```

use alloc::string::String;
use core::fmt::Write;

use super::{BuiltinIn, BuiltinOut, Func, InstOp, Jump, Node, ty_name};
use crate::ops;

/// The function as text.
pub fn print(f: &Func) -> String {
    let mut out = String::new();
    list(f, &f.body, 0, &mut out);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn list(f: &Func, nodes: &[Node], depth: usize, out: &mut String) {
    for n in nodes {
        match n {
            Node::Block(b) => {
                indent(out, depth);
                let _ = writeln!(out, "b{}:", b.0);
                let data = &f.blocks[b.0 as usize];
                for &i in &data.insts {
                    indent(out, depth + 1);
                    inst(f, i, out);
                    out.push('\n');
                }
                if let Some(j) = data.jump {
                    indent(out, depth + 1);
                    out.push_str(match j {
                        Jump::Break => "break\n",
                        Jump::Continue => "continue\n",
                        Jump::Discard => "discard\n",
                    });
                }
            }
            Node::If(i) => {
                indent(out, depth);
                let _ = writeln!(out, "if %{} {{", i.cond.0);
                list(f, &i.then, depth + 1, out);
                indent(out, depth);
                out.push_str("} else {\n");
                list(f, &i.otherwise, depth + 1, out);
                indent(out, depth);
                out.push_str("}\n");
            }
            Node::Loop(l) => {
                indent(out, depth);
                out.push_str("loop {\n");
                list(f, &l.body, depth + 1, out);
                indent(out, depth);
                out.push_str("}\n");
            }
        }
    }
}

fn constant(v: ops::Value) -> String {
    match v {
        ops::Value::F(x) => alloc::format!("{x:?}"),
        ops::Value::I(x) => alloc::format!("{x}"),
        ops::Value::U(x) => alloc::format!("{x}u"),
        ops::Value::B(x) => alloc::format!("{x}"),
    }
}

fn inst(f: &Func, id: super::InstId, out: &mut String) {
    let i = f.inst(id);
    if !i.results.is_empty() {
        for (k, r) in i.results.iter().enumerate() {
            if k > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "%{}:{}", r.0, ty_name(f.ty(*r)));
        }
        out.push_str(" = ");
    }
    let comp = |c: u8| ['x', 'y', 'z', 'w'].get(c as usize).copied().unwrap_or('?');
    match &i.op {
        InstOp::Const(v) => out.push_str(&alloc::format!("const {}", constant(*v))),
        InstOp::Op(op) => {
            let _ = write!(out, "{op:?}");
        }
        InstOp::Select => out.push_str("select"),
        InstOp::Phi => out.push_str("phi"),
        InstOp::LoadInput { slot, comp: c } => {
            let _ = write!(out, "input {slot}.{}", comp(*c));
        }
        InstOp::LoadBuiltin(b) => {
            let _ = match b {
                BuiltinIn::VertexId => write!(out, "gl_VertexID"),
                BuiltinIn::InstanceId => write!(out, "gl_InstanceID"),
                BuiltinIn::FragCoord(c) => write!(out, "gl_FragCoord.{}", comp(*c)),
                BuiltinIn::FrontFacing => write!(out, "gl_FrontFacing"),
                BuiltinIn::PointCoord(c) => write!(out, "gl_PointCoord.{}", comp(*c)),
            };
        }
        InstOp::StoreOutput { slot, comp: c } => {
            let _ = write!(out, "output {slot}.{} =", comp(*c));
        }
        InstOp::StoreBuiltin(b) => {
            let _ = match b {
                BuiltinOut::Position(c) => write!(out, "gl_Position.{} =", comp(*c)),
                BuiltinOut::PointSize => write!(out, "gl_PointSize ="),
                BuiltinOut::FragDepth => write!(out, "gl_FragDepth ="),
            };
        }
        InstOp::LoadUniform { slot, comp: c } => {
            let _ = write!(out, "uniform {slot}.{}", comp(*c));
        }
        InstOp::LoadUniformIndexed { base, comp: c, stride, count } => {
            let _ = write!(out, "uniform {base}.{} [*{stride} <{count}]", comp(*c));
        }
        InstOp::LoadBlock { block, offset, stride, count } => {
            let _ = write!(out, "block {block}+{offset}");
            if *count > 0 {
                let _ = write!(out, " [*{stride} <{count}]");
            }
        }
        InstOp::Tex(t) => {
            let _ = write!(out, "tex {} s{}", t.sampler.name(), t.index);
            if t.dynamic {
                out.push_str("[dyn]");
            }
            let _ = write!(out, " {:?}", t.lod);
            if t.bias {
                out.push_str(" bias");
            }
            if t.offset != [0; 3] {
                let _ = write!(out, " offset {:?}", t.offset);
            }
        }
        InstOp::TexSize { index, .. } => {
            let _ = write!(out, "texsize s{index}");
        }
        InstOp::Deriv { y } => out.push_str(if *y { "ddy" } else { "ddx" }),
        InstOp::Nop => out.push_str("nop"),
    }
    for (k, a) in i.args.iter().enumerate() {
        out.push_str(if k == 0 { " " } else { ", " });
        let _ = write!(out, "%{}", a.0);
    }
}
