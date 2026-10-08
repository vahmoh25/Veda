//! `vfiles` — working with files in Veda applications.
//!
//! * [`path`]: absolute, `/`-separated paths as the VFS uses them —
//!   normalising, resolving against a working directory and `~`, joining and
//!   splitting, the read-only system image, shell wildcards, natural name
//!   order and free names for new files.
//! * [`kind`]: what a file is, judged by its extension, and the application
//!   that opens it by default. This is the one association table of the
//!   system: Files, the Terminal's `open`, the desktop's icons and the
//!   pickers of Photos, Music and Settings all ask it.
//! * [`format`]: sizes and modification times for people.
//! * [`fs`]: a convenient client of the VFS service — whole-file reads and
//!   writes, appends, recursive copies, moves and removals.
//! * [`trash`]: the Trash, where deleted items wait to be restored or
//!   deleted for good (freedesktop.org's layout, in `~/.local/share/Trash`).
//! * [`thumbs`] (feature `thumbnails`): image thumbnails made on a background
//!   thread.
//!
//! `path`, `kind` and `format`, and the info records of `trash`, are pure and
//! unit-tested on the host.

#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod format;
pub mod fs;
pub mod kind;
pub mod path;
#[cfg(feature = "thumbnails")]
pub mod thumbs;
pub mod trash;

pub use fs::{Error, Fs};
pub use kind::{App, FileKind, default_app, file_kind};
pub use path::HOME;
