//! Interned strings.
//!
//! Identifiers and number spellings are stored once per compilation and
//! referred to by [`Symbol`], a small integer: tokens stay `Copy`, and
//! comparing two names is comparing two integers.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// An interned string; resolve it with [`Interner::get`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Symbol(u32);

impl Symbol {
    /// The symbol's index, dense from 0 in the order strings were interned.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The table of interned strings of one compilation.
#[derive(Default, Debug)]
pub struct Interner {
    map: BTreeMap<Box<str>, Symbol>,
    names: Vec<Box<str>>,
}

impl Interner {
    /// An empty table.
    pub fn new() -> Interner {
        Interner::default()
    }

    /// The symbol for `s`, interning it on first use.
    pub fn intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }
        let sym = Symbol(self.names.len() as u32);
        self.names.push(s.into());
        self.map.insert(s.into(), sym);
        sym
    }

    /// The symbol for `s` if it has been interned.
    pub fn lookup(&self, s: &str) -> Option<Symbol> {
        self.map.get(s).copied()
    }

    /// The string `sym` stands for.
    pub fn get(&self, sym: Symbol) -> &str {
        &self.names[sym.index()]
    }
}
