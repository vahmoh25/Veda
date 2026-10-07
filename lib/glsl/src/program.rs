//! A program: linking plus the optimised code of both stages.
//!
//! [`link`] is what `glLinkProgram` does: it links the shaders' interfaces
//! ([`crate::link`]), builds each stage's SSA form ([`crate::ir::build`]),
//! optimises it and checks it. The result keeps everything the back ends
//! and the OpenGL ES API need, and nothing of the shaders themselves, which
//! may change or go away after linking.

use alloc::string::String;

use crate::builtins::Limits;
use crate::hir::Shader;
use crate::ir::{self, Func};
use crate::link::{self, Bindings, Linked};

/// A linked program.
#[derive(Debug)]
pub struct Program {
    pub linked: Linked,
    pub vertex: Func,
    pub fragment: Func,
}

/// The result of linking.
pub struct LinkResult {
    pub program: Option<Program>,
    /// Errors, as `glGetProgramInfoLog` returns them.
    pub log: String,
}

/// Links a vertex and a fragment shader into a program.
pub fn link(vs: &Shader, fs: &Shader, bindings: &Bindings, limits: &Limits) -> LinkResult {
    let r = link::link(vs, fs, bindings, limits);
    let Some(linked) = r.linked else { return LinkResult { program: None, log: r.log } };
    let mut vertex = ir::build::build(vs, &linked.vertex, &linked);
    let mut fragment = ir::build::build(fs, &linked.fragment, &linked);
    ir::opt::optimize(&mut vertex);
    ir::opt::optimize(&mut fragment);
    // A failure here is a compiler bug; refuse the program rather than run
    // code that breaks the IR's rules.
    for (f, what) in [(&vertex, "vertex"), (&fragment, "fragment")] {
        if let Err(e) = ir::verify::verify(f) {
            let log = alloc::format!("{}ERROR: internal compiler error in the {what} shader: {e}\n", r.log);
            return LinkResult { program: None, log };
        }
    }
    LinkResult { program: Some(Program { linked, vertex, fragment }), log: r.log }
}
