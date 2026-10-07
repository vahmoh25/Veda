//! `vglsl` — the OpenGL ES Shading Language compiler.
//!
//! It compiles shaders written in GLSL ES 1.00 (OpenGL ES 2.0) and 3.00
//! (OpenGL ES 3.0), links them into programs, and produces code for Veda's
//! two OpenGL ES back ends:
//!
//! * [`pp`] — the preprocessor (`#define`, `#if`, `#line`, `#extension`...).
//! * [`lex`] and [`parse`] — tokens and the syntax tree ([`ast`]).
//! * `sema` — the semantic checks, producing the checked shader ([`hir`]):
//!   names resolved, every expression typed, constant expressions folded
//!   with [`lower`] and [`ops`], the single definition of what each
//!   operation computes.
//!
//! [`compile`] runs these stages for one shader, as `glCompileShader` does.
//! Every stage reports problems as [`diag::Diagnostics`], formatted as
//! OpenGL information logs; no input, however malformed, makes it panic.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod ast;
pub mod builtins;
pub mod diag;
pub mod hir;
pub mod intern;
pub mod interp;
pub mod ir;
pub mod lex;
pub mod link;
pub mod lower;
pub mod ops;
pub mod parse;
pub mod pp;
pub mod program;
mod sema;
pub mod tgsi;
pub mod types;

#[cfg(test)]
mod tests;

use alloc::string::String;

pub use builtins::Limits;
pub use pp::{Ext, ExtSet};

/// The language version a shader is written in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Version {
    /// GLSL ES 1.00 (no `#version`, or `#version 100`).
    V100,
    /// GLSL ES 3.00 (`#version 300 es`).
    V300,
}

/// A programmable stage of the pipeline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Vertex,
    Fragment,
}

/// What the compiler knows of the implementation it compiles for.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub limits: Limits,
    /// The extensions the implementation offers (shaders enable them with
    /// `#extension`).
    pub extensions: ExtSet,
}

impl Default for Options {
    fn default() -> Options {
        Options { limits: Limits::default(), extensions: ExtSet::all() }
    }
}

/// The result of compiling one shader.
#[derive(Debug)]
pub struct Compiled {
    /// The checked shader, if it compiled.
    pub shader: Option<hir::Shader>,
    /// Errors and warnings, as `glGetShaderInfoLog` returns them.
    pub log: String,
}

/// Compiles one shader from its source strings (as `glShaderSource` gives
/// them).
pub fn compile(stage: Stage, sources: &[&str], options: &Options) -> Compiled {
    let mut interner = intern::Interner::new();
    let mut diags = diag::Diagnostics::new();
    let pre = pp::preprocess(sources, options.extensions, &mut interner, &mut diags);
    let tokens = lex::tokens(&pre.tokens, pre.version, pre.enabled, &interner, &mut diags);
    let unit = parse::parse(&tokens, &interner, &mut diags);
    // Checking a tree with syntax errors would mostly report their echoes.
    if diags.has_errors() {
        return Compiled { shader: None, log: diags.log() };
    }
    let shader =
        sema::check(&unit, stage, pre.version, pre.enabled, pre.invariant_all, &options.limits, interner, &mut diags);
    let ok = !diags.has_errors();
    Compiled { shader: ok.then_some(shader), log: diags.log() }
}
