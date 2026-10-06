//! Names in the ACPI namespace.

use alloc::vec::Vec;
use core::fmt;

/// One name segment: four characters, padded with `_` (`_SB_`, `PC00`).
pub type NameSeg = [u8; 4];

/// Makes a segment from text (`"_HID"`, `"GPI0"`, `"_SB"`), padding it
/// with `_`. `None` if it is longer than four characters or empty.
pub fn seg(text: &str) -> Option<NameSeg> {
    let b = text.as_bytes();
    if b.is_empty() || b.len() > 4 {
        return None;
    }
    let mut s = [b'_'; 4];
    s[..b.len()].copy_from_slice(b);
    Some(s)
}

/// An absolute path in the namespace (`\_SB.PC00.SPI1`). The root has no
/// segments.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Path(pub Vec<NameSeg>);

impl Path {
    pub fn root() -> Path {
        Path(Vec::new())
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn child(&self, seg: NameSeg) -> Path {
        let mut p = self.clone();
        p.0.push(seg);
        p
    }

    /// The path of `name` (`"_HID"`) below this one.
    pub fn join(&self, name: &str) -> Option<Path> {
        seg(name).map(|s| self.child(s))
    }

    pub fn parent(&self) -> Option<Path> {
        let mut p = self.clone();
        p.0.pop().map(|_| p)
    }

    pub fn last(&self) -> Option<NameSeg> {
        self.0.last().copied()
    }

    /// Parses an absolute path written as in ASL: `\_SB.PC00.SPI1`.
    pub fn parse(text: &str) -> Option<Path> {
        let rest = text.strip_prefix('\\')?;
        if rest.is_empty() {
            return Some(Path::root());
        }
        rest.split('.').map(seg).collect::<Option<Vec<_>>>().map(Path)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\\")?;
        for (i, seg) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            // Trailing padding is not shown (`_SB_` is `_SB`).
            let len = seg.iter().rposition(|&c| c != b'_').map_or(1, |p| p + 1);
            for &c in &seg[..len] {
                write!(f, "{}", c as char)?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A name as AML writes it: from the root (`\`), or relative to the
/// current scope after going up `parents` levels (`^`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameString {
    pub absolute: bool,
    pub parents: usize,
    pub segs: Vec<NameSeg>,
}

impl NameString {
    /// The null name (no object), which marks an absent target.
    pub fn is_null(&self) -> bool {
        !self.absolute && self.parents == 0 && self.segs.is_empty()
    }

    /// Whether a reference with this name searches the enclosing scopes:
    /// a single segment without a prefix.
    pub fn searches(&self) -> bool {
        !self.absolute && self.parents == 0 && self.segs.len() == 1
    }

    /// The path this name denotes in `scope`, without searching (where a
    /// definition with this name goes).
    pub fn in_scope(&self, scope: &Path) -> Path {
        let mut p = if self.absolute { Path::root() } else { scope.clone() };
        for _ in 0..self.parents {
            p.0.pop();
        }
        p.0.extend_from_slice(&self.segs);
        p
    }

    /// Parses a name as written in ASL or a resource source: `\_SB.GPI0`,
    /// `^^GPI0`, `GPI0`.
    pub fn parse(text: &str) -> Option<NameString> {
        let mut n = NameString::default();
        let mut rest = text;
        if let Some(r) = rest.strip_prefix('\\') {
            n.absolute = true;
            rest = r;
        }
        while let Some(r) = rest.strip_prefix('^') {
            n.parents += 1;
            rest = r;
        }
        if !rest.is_empty() {
            n.segs = rest.split('.').map(seg).collect::<Option<Vec<_>>>()?;
        }
        Some(n)
    }
}
