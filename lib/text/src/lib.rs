//! `vtext` — the text editing model behind the Text Editor.
//!
//! * [`Buffer`]: text as lines, addressed by [`Pos`] (line, byte column);
//!   insertion, deletion, word boundaries and search.
//! * [`Document`]: a buffer with a [`Selection`], editing commands
//!   (typing, deletion, indentation, comments, line moves, replace) and
//!   grouped undo/redo, plus modification tracking.
//! * [`Layout`]: soft-wrap layout of a buffer into visual rows on a
//!   monospace grid, with hit testing and vertical cursor movement.
//! * [`highlight`]: incremental syntax highlighting for a few languages.
//!
//! The crate is pure logic without I/O, so all of it is unit-tested on the
//! host (`cargo test -p vtext`).

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod buffer;
mod document;
pub mod highlight;
mod layout;

pub use buffer::{Buffer, LineEnding, Pos};
pub use document::{Document, Motion, Selection};
pub use layout::{Layout, Row, char_width, display_col};
