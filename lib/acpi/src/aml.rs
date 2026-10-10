//! AML: loading definition blocks into a namespace, and evaluating its
//! objects.
//!
//! **Loading** walks a table's terms and records the objects they define:
//! scopes, devices, methods (whose bodies run when they are evaluated),
//! names with their data, operation regions and their fields. An `If`
//! outside any method defines what is inside it only when its condition
//! holds, as the firmware means it (that is how a board leaves out the
//! devices its settings turn off); a condition that cannot be evaluated
//! drops the block, counted in [`Namespace::skipped`]. Other code outside
//! methods is skipped rather than run.
//!
//! **Evaluating** reads a name or runs a method with a small interpreter:
//! integer, string, buffer and package operations, buffer fields, control
//! flow, method calls, and the fields of `SystemMemory` and `PCI_Config`
//! regions, read through [`Memory`]. It changes nothing outside itself:
//! stores to the namespace's names and fields are kept for the rest of that
//! evaluation only, and what would act on the machine (`Notify`, `Sleep`,
//! `Acquire`) does nothing. Regions in other address spaces (I/O ports, the
//! embedded controller) are not read. Every evaluation is bounded in steps
//! and in call depth.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::name::{NameSeg, NameString, Path};
use crate::tables::Table;
use crate::value::{Place, Value};
use crate::{Memory, PciFunction};

/// Steps one evaluation may take (a loop that never ends stops here).
const MAX_STEPS: u32 = 1_000_000;
/// Nested method calls.
const MAX_DEPTH: u32 = 32;
/// Elements a package or bytes a buffer may be created with.
const MAX_ELEMENTS: usize = 1 << 16;
const MAX_BUFFER: usize = 1 << 20;
/// What `Revision` answers.
const REVISION: u64 = 0x2024_0322;

/// Why loading or evaluating failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The code ends early, or a length points past its end (at this
    /// offset in the table's AML).
    Truncated(usize),
    /// A malformed name at this offset.
    BadName(usize),
    /// An opcode that is unknown, or that cannot appear where it does
    /// (`0x5Bxx` for extended opcodes), at this offset.
    Opcode(u16, usize),
    /// Something the interpreter does not do.
    Unsupported(&'static str),
    /// A name with no object.
    NoSuchName(Path),
    /// An operand of the wrong type.
    Type(&'static str),
    /// Firmware memory that could not be read.
    Memory(u64),
    /// A PCI function's configuration space that could not be read (at
    /// this offset).
    PciConfig(PciFunction, u16),
    /// Too many steps, or calls nested too deep.
    Limit,
    DivideByZero,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(at) => write!(f, "the code ends early (at {:#x})", at),
            Error::BadName(at) => write!(f, "a malformed name at {:#x}", at),
            Error::Opcode(op, at) => write!(f, "opcode {:#x} at {:#x}", op, at),
            Error::Unsupported(what) => write!(f, "{} is not supported", what),
            Error::NoSuchName(p) => write!(f, "no object {}", p),
            Error::Type(what) => write!(f, "{} expected", what),
            Error::Memory(a) => write!(f, "firmware memory at {:#x} cannot be read", a),
            Error::PciConfig(function, offset) => {
                write!(f, "the configuration space of PCI function {} cannot be read at {:#x}", function, offset)
            }
            Error::Limit => f.write_str("the evaluation took too long"),
            Error::DivideByZero => f.write_str("division by zero"),
        }
    }
}

/// Where a method's body is: in which loaded table, between which offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    pub table: usize,
    pub start: usize,
    pub end: usize,
}

/// An object in the namespace.
#[derive(Debug, Clone)]
pub enum Object {
    /// A scope with no object of its own (`\_SB`).
    Scope,
    Device,
    Processor,
    PowerResource,
    ThermalZone,
    Method {
        args: u8,
        code: Code,
    },
    Name(Value),
    /// An operation region: its address space (0 memory, 1 I/O ports,
    /// 2 PCI configuration space...) and where it is.
    Region {
        space: u8,
        bounds: Bounds,
    },
    Field(Field),
    /// A field of a buffer (`CreateDWordField`), defined by a method.
    BufferField {
        place: Place,
        bit_offset: u64,
        bit_width: u64,
    },
    Mutex,
    Event,
    Alias(Path),
    /// `\_OSI`, which the interpreter answers itself.
    Osi,
}

/// Where an operation region is.
#[derive(Debug, Clone)]
pub enum Bounds {
    Known {
        offset: u64,
        length: u64,
    },
    /// Evaluated when a field of the region is first read, as its address
    /// may depend on objects a later table defines: the code of its offset
    /// and length, and the scope it is in.
    Deferred {
        scope: Path,
        code: Code,
    },
    Unknown,
}

/// A named field of an operation region.
#[derive(Debug, Clone)]
pub struct Field {
    pub unit: FieldUnit,
    pub bit_offset: u64,
    pub bit_width: u64,
    /// Bits per access (8, 16, 32 or 64).
    pub access_bits: u32,
}

#[derive(Debug, Clone)]
pub enum FieldUnit {
    Region(Path),
    /// An index/data pair (`IndexField`): not read by the interpreter.
    Index,
    /// A banked region (`BankField`): not read by the interpreter.
    Bank,
}

/// The ACPI namespace: every object the loaded tables define.
pub struct Namespace {
    code: Vec<Arc<[u8]>>,
    objects: BTreeMap<Path, Object>,
    /// Methods declared `External` (defined in another table), with their
    /// argument counts: needed to parse calls to them.
    externals: BTreeMap<Path, u8>,
    /// Integers are 32 bits wide under a DSDT of revision 1.
    integer_bits: u32,
    /// Conditional definitions dropped because their condition could not
    /// be evaluated.
    pub skipped: usize,
    /// Why the first of them were (the scope and the error).
    pub skip_reasons: Vec<(Path, Error)>,
    /// Names whose data refers to objects not defined when they were (a
    /// package naming a device further on, or in a later table): they are
    /// evaluated again once a table has loaded.
    unresolved: Vec<Unresolved>,
}

/// A name's data, to be evaluated again.
struct Unresolved {
    path: Path,
    scope: Path,
    table: usize,
    start: usize,
}

impl Default for Namespace {
    fn default() -> Self {
        Self::new()
    }
}

impl Namespace {
    /// An empty namespace with the objects every one has.
    pub fn new() -> Namespace {
        let mut ns = Namespace {
            code: Vec::new(),
            objects: BTreeMap::new(),
            externals: BTreeMap::new(),
            integer_bits: 64,
            skipped: 0,
            skip_reasons: Vec::new(),
            unresolved: Vec::new(),
        };
        ns.objects.insert(Path::root(), Object::Scope);
        for s in ["_GPE", "_PR", "_SB", "_SI", "_TZ"] {
            ns.objects.insert(Path::root().join(s).unwrap(), Object::Scope);
        }
        ns.objects
            .insert(Path::root().join("_OS").unwrap(), Object::Name(Value::String("Microsoft Windows NT".into())));
        ns.objects.insert(Path::root().join("_REV").unwrap(), Object::Name(Value::Integer(2)));
        ns.objects.insert(Path::root().join("_GL").unwrap(), Object::Mutex);
        ns.objects.insert(Path::root().join("_OSI").unwrap(), Object::Osi);
        ns
    }

    /// Loads a DSDT or SSDT. A DSDT of revision 1 makes integers 32 bits
    /// wide.
    pub fn load_table(&mut self, table: &Table, memory: &dyn Memory) -> Result<(), Error> {
        if table.is(b"DSDT") && table.data[8] < 2 {
            self.integer_bits = 32;
        }
        self.load(table.aml(), memory)
    }

    /// Loads a definition block's AML (a table without its header). What
    /// it defined before an error stays defined.
    pub fn load(&mut self, aml: &[u8], memory: &dyn Memory) -> Result<(), Error> {
        let table = self.code.len();
        let code: Arc<[u8]> = Arc::from(aml);
        self.code.push(code.clone());
        let loaded = self.load_list(table, &code, 0, code.len(), &Path::root(), memory);
        self.resolve_again(memory);
        loaded
    }

    /// Evaluates again the data of the names that referred to objects not
    /// defined yet; those that still do wait for the next table.
    fn resolve_again(&mut self, memory: &dyn Memory) {
        for u in core::mem::take(&mut self.unresolved) {
            let code = self.code[u.table].clone();
            let mut c = Cursor { code: &code, pos: u.start };
            let mut f = Frame::new(u.scope.clone(), Vec::new());
            let value = Evaluator::new(self, memory).term(&mut f, &mut c, true);
            match value {
                Ok(v) if !self.dangles(&v) => {
                    self.objects.insert(u.path, Object::Name(v));
                }
                _ => self.unresolved.push(u),
            }
        }
    }

    /// Whether `value` refers to an object that does not exist.
    fn dangles(&self, value: &Value) -> bool {
        match value {
            Value::Reference(p) => !self.objects.contains_key(p),
            Value::Package(items) => items.iter().any(|v| self.dangles(v)),
            _ => false,
        }
    }

    /// The object at `path` (aliases followed).
    pub fn get(&self, path: &Path) -> Option<&Object> {
        let mut path = path;
        for _ in 0..8 {
            match self.objects.get(path)? {
                Object::Alias(target) => path = target,
                other => return Some(other),
            }
        }
        None
    }

    /// Every object, in path order.
    pub fn objects(&self) -> impl Iterator<Item = (&Path, &Object)> {
        self.objects.iter()
    }

    /// The objects directly below `path`.
    pub fn children<'a>(&'a self, path: &'a Path) -> impl Iterator<Item = (&'a Path, &'a Object)> + 'a {
        let depth = path.0.len();
        self.objects
            .range(path.clone()..)
            .skip(1)
            .take_while(move |(p, _)| p.0.starts_with(&path.0))
            .filter(move |(p, _)| p.0.len() == depth + 1)
    }

    /// Every device, in path order.
    pub fn devices(&self) -> impl Iterator<Item = &Path> {
        self.objects.iter().filter(|(_, o)| matches!(o, Object::Device)).map(|(p, _)| p)
    }

    /// Finds the object a name refers to from `scope`, searching the
    /// enclosing scopes for a single-segment name as AML does.
    pub fn resolve(&self, scope: &Path, name: &NameString) -> Option<Path> {
        search(scope, name, |p| self.objects.contains_key(p))
    }

    /// Reads the object at `path`, or calls it with `args` if it is a
    /// method.
    pub fn evaluate(&self, path: &Path, args: &[Value], memory: &dyn Memory) -> Result<Value, Error> {
        self.evaluate_in(&mut Evaluator::new(self, memory), path, args)
    }

    /// Calls method `first` with `first_args`, then evaluates `path` as
    /// [`Namespace::evaluate`] does, in the same evaluation: what the first
    /// stores holds for the second (`\_PIC (1)`, the interrupt model the
    /// firmware's `_PRT` methods answer for). Without a method `first`,
    /// just evaluates `path`.
    pub fn evaluate_after(
        &self,
        first: &Path,
        first_args: &[Value],
        path: &Path,
        args: &[Value],
        memory: &dyn Memory,
    ) -> Result<Value, Error> {
        let mut ev = Evaluator::new(self, memory);
        if let Some(Object::Method { .. }) = self.get(first) {
            ev.call(first, first_args.to_vec())?;
        }
        self.evaluate_in(&mut ev, path, args)
    }

    fn evaluate_in(&self, ev: &mut Evaluator, path: &Path, args: &[Value]) -> Result<Value, Error> {
        match self.get(path) {
            Some(Object::Method { .. }) => ev.call(path, args.to_vec()),
            Some(Object::Osi) => Ok(ev.osi(args.first())),
            Some(_) => {
                let mut f = Frame::new(path.parent().unwrap_or_default(), Vec::new());
                ev.read_name(&mut f, path)
            }
            None => Err(Error::NoSuchName(path.clone())),
        }
    }

    /// Evaluates `name` (`"_HID"`) of `device`: `None` if it has none.
    pub fn evaluate_child(&self, device: &Path, name: &str, memory: &dyn Memory) -> Option<Result<Value, Error>> {
        let path = device.join(name)?;
        self.objects.contains_key(&path).then(|| self.evaluate(&path, &[], memory))
    }

    fn ones(&self) -> u64 {
        if self.integer_bits == 32 { u32::MAX as u64 } else { u64::MAX }
    }

    /// Defines `object` at `path`. The first definition of a name stands
    /// (a scope opened before the object is replaced by it).
    fn define(&mut self, path: Path, object: Object) {
        match self.objects.get(&path) {
            None | Some(Object::Scope) => {
                self.objects.insert(path, object);
            }
            Some(_) => {}
        }
    }

    fn load_list(
        &mut self,
        table: usize,
        code: &Arc<[u8]>,
        start: usize,
        end: usize,
        scope: &Path,
        memory: &dyn Memory,
    ) -> Result<(), Error> {
        let mut c = Cursor { code: &code[..end], pos: start };
        while c.pos < end {
            let at = c.pos;
            match c.byte()? {
                // Scope: the name refers to an existing object.
                0x10 => {
                    let body = c.pkg_end()?;
                    let name = c.name_string()?;
                    let path = self.resolve(scope, &name).unwrap_or_else(|| name.in_scope(scope));
                    self.objects.entry(path.clone()).or_insert(Object::Scope);
                    self.load_list(table, code, c.pos, body, &path, memory)?;
                    c.pos = body;
                }
                // Method
                0x14 => {
                    let body = c.pkg_end()?;
                    let name = c.name_string()?;
                    let flags = c.byte()?;
                    let code = Code { table, start: c.pos, end: body };
                    self.define(name.in_scope(scope), Object::Method { args: flags & 7, code });
                    c.pos = body;
                }
                // Name (a value that cannot be evaluated is left empty)
                0x08 => {
                    let name = c.name_string()?;
                    let mut ev = Evaluator::new(self, memory);
                    let mut f = Frame::new(scope.clone(), Vec::new());
                    let at = c.pos;
                    let value = match ev.term(&mut f, &mut c, true) {
                        Ok(v) => v,
                        Err(_) => {
                            c.pos = at;
                            ev.term(&mut f, &mut c, false)?;
                            Value::Uninitialized
                        }
                    };
                    let path = name.in_scope(scope);
                    if self.dangles(&value) && !self.objects.contains_key(&path) {
                        self.unresolved.push(Unresolved { path: path.clone(), scope: scope.clone(), table, start: at });
                    }
                    self.define(path, Object::Name(value));
                }
                // Alias
                0x06 => {
                    let source = c.name_string()?;
                    let alias = c.name_string()?;
                    let target = self.resolve(scope, &source).unwrap_or_else(|| source.in_scope(scope));
                    self.define(alias.in_scope(scope), Object::Alias(target));
                }
                // External: only methods matter (their argument counts).
                0x15 => {
                    let name = c.name_string()?;
                    let kind = c.byte()?;
                    let args = c.byte()?;
                    if kind == 8 {
                        self.externals.insert(name.in_scope(scope), args & 7);
                    }
                }
                // If, with an optional Else.
                0xA0 => {
                    let body = c.pkg_end()?;
                    let condition = {
                        let mut ev = Evaluator::new(self, memory);
                        let mut f = Frame::new(scope.clone(), Vec::new());
                        ev.term(&mut f, &mut c, true).and_then(|v| int(&v))
                    };
                    let mut after = body;
                    let mut otherwise = None;
                    if code.get(body) == Some(&0xA1) && body < end {
                        let mut e = Cursor { code: &code[..end], pos: body + 1 };
                        let else_end = e.pkg_end()?;
                        otherwise = Some((e.pos, else_end));
                        after = else_end;
                    }
                    match condition {
                        Ok(v) if v != 0 => self.load_list(table, code, c.pos, body, scope, memory)?,
                        Ok(_) => {
                            if let Some((s, e)) = otherwise {
                                self.load_list(table, code, s, e, scope, memory)?;
                            }
                        }
                        Err(e) => {
                            self.skipped += 1;
                            if self.skip_reasons.len() < 16 {
                                self.skip_reasons.push((scope.clone(), e));
                            }
                        }
                    }
                    c.pos = after;
                }
                // An Else without its If.
                0xA1 => c.pos = c.pkg_end()?,
                0x5B => match c.byte()? {
                    // OperationRegion
                    0x80 => {
                        let name = c.name_string()?;
                        let space = c.byte()?;
                        let start = c.pos;
                        let bounds = match Evaluator::new(self, memory).region_bounds(scope, &mut c) {
                            Ok((offset, length)) => Bounds::Known { offset, length },
                            Err(_) => {
                                c.pos = start;
                                let mut f = Frame::new(scope.clone(), Vec::new());
                                let mut ev = Evaluator::new(self, memory);
                                ev.term(&mut f, &mut c, false)?;
                                ev.term(&mut f, &mut c, false)?;
                                Bounds::Deferred { scope: scope.clone(), code: Code { table, start, end: c.pos } }
                            }
                        };
                        self.define(name.in_scope(scope), Object::Region { space, bounds });
                    }
                    // Field, IndexField, BankField
                    op @ (0x81 | 0x86 | 0x87) => {
                        let list_end = c.pkg_end()?;
                        let first = c.name_string()?;
                        let unit = match op {
                            0x81 => {
                                FieldUnit::Region(self.resolve(scope, &first).unwrap_or_else(|| first.in_scope(scope)))
                            }
                            0x86 => {
                                c.name_string()?;
                                FieldUnit::Index
                            }
                            _ => {
                                c.name_string()?;
                                let mut f = Frame::new(scope.clone(), Vec::new());
                                Evaluator::new(self, memory).term(&mut f, &mut c, false)?;
                                FieldUnit::Bank
                            }
                        };
                        let flags = c.byte()?;
                        let mut defined = Vec::new();
                        field_list(&mut c, list_end, flags, &unit, scope, &mut |p, o| defined.push((p, o)))?;
                        for (p, o) in defined {
                            self.define(p, o);
                        }
                        c.pos = list_end;
                    }
                    // Device, Processor, PowerResource, ThermalZone
                    op @ (0x82..=0x85) => {
                        let body = c.pkg_end()?;
                        let name = c.name_string()?;
                        let object = match op {
                            0x82 => Object::Device,
                            0x83 => {
                                c.bytes(6)?; // processor id, PBlk address and length
                                Object::Processor
                            }
                            0x84 => {
                                c.bytes(3)?; // system level, resource order
                                Object::PowerResource
                            }
                            _ => Object::ThermalZone,
                        };
                        let path = name.in_scope(scope);
                        self.define(path.clone(), object);
                        self.load_list(table, code, c.pos, body, &path, memory)?;
                        c.pos = body;
                    }
                    // Mutex
                    0x01 => {
                        let name = c.name_string()?;
                        c.byte()?;
                        self.define(name.in_scope(scope), Object::Mutex);
                    }
                    // Event
                    0x02 => {
                        let name = c.name_string()?;
                        self.define(name.in_scope(scope), Object::Event);
                    }
                    // DataTableRegion: a region over a table, not read.
                    0x88 => {
                        let name = c.name_string()?;
                        let mut f = Frame::new(scope.clone(), Vec::new());
                        let mut ev = Evaluator::new(self, memory);
                        for _ in 0..3 {
                            ev.term(&mut f, &mut c, false)?;
                        }
                        self.define(name.in_scope(scope), Object::Region { space: 0xFE, bounds: Bounds::Unknown });
                    }
                    _ => {
                        c.pos = at;
                        skip_code(self, memory, scope, &mut c)?;
                    }
                },
                _ => {
                    c.pos = at;
                    skip_code(self, memory, scope, &mut c)?;
                }
            }
        }
        Ok(())
    }
}

/// Skips one term of code outside a method, without running it.
fn skip_code(ns: &Namespace, memory: &dyn Memory, scope: &Path, c: &mut Cursor) -> Result<(), Error> {
    let mut f = Frame::new(scope.clone(), Vec::new());
    let mut ev = Evaluator::new(ns, memory);
    match c.peek()? {
        // While, Return, Break, Continue, Noop, Breakpoint
        0xA2 => {
            c.pos += 1;
            c.pos = c.pkg_end()?;
        }
        0xA4 => {
            c.pos += 1;
            ev.term(&mut f, c, false)?;
        }
        0xA5 | 0x9F | 0xA3 | 0xCC => c.pos += 1,
        _ => {
            ev.term(&mut f, c, false)?;
        }
    }
    Ok(())
}

/// Searches for a name from `scope` as AML does: a single segment without
/// a prefix in the scope and then in each enclosing one; anything else
/// exactly where it points.
fn search(scope: &Path, name: &NameString, exists: impl Fn(&Path) -> bool) -> Option<Path> {
    if !name.searches() {
        let p = name.in_scope(scope);
        return exists(&p).then_some(p);
    }
    let seg = name.segs[0];
    let mut s = scope.clone();
    loop {
        let p = s.child(seg);
        if exists(&p) {
            return Some(p);
        }
        s.0.pop()?;
    }
}

/// Parses a field list, giving each named field to `define`.
fn field_list(
    c: &mut Cursor,
    end: usize,
    flags: u8,
    unit: &FieldUnit,
    scope: &Path,
    define: &mut dyn FnMut(Path, Object),
) -> Result<(), Error> {
    let access = |kind: u8| match kind & 0xF {
        2 => 16,
        3 => 32,
        4 => 64,
        _ => 8,
    };
    let mut access_bits = access(flags);
    let mut bit = 0u64;
    while c.pos < end {
        match c.peek()? {
            // Reserved bits
            0x00 => {
                c.pos += 1;
                bit += c.pkg_length()? as u64;
            }
            // AccessField, ExtendedAccessField
            0x01 => {
                c.pos += 1;
                access_bits = access(c.byte()?);
                c.byte()?;
            }
            0x03 => {
                c.pos += 1;
                access_bits = access(c.byte()?);
                c.bytes(2)?;
            }
            // ConnectField: a name or a buffer (a GPIO or serial bus connection).
            0x02 => {
                c.pos += 1;
                if c.peek()? == 0x11 {
                    c.pos += 1;
                    c.pos = c.pkg_end()?;
                } else {
                    c.name_string()?;
                }
            }
            _ => {
                let seg = c.name_seg()?;
                let width = c.pkg_length()? as u64;
                let field = Field { unit: unit.clone(), bit_offset: bit, bit_width: width, access_bits };
                define(scope.child(seg), Object::Field(field));
                bit += width;
            }
        }
    }
    Ok(())
}

/// A position in a table's AML.
struct Cursor<'a> {
    code: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Result<u8, Error> {
        self.code.get(self.pos).copied().ok_or(Error::Truncated(self.pos))
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.code.get(self.pos + ahead).copied()
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.code.len()).ok_or(Error::Truncated(self.pos))?;
        let s = &self.code[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn word(&mut self) -> Result<u16, Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn dword(&mut self) -> Result<u32, Error> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn qword(&mut self) -> Result<u64, Error> {
        Ok(self.dword()? as u64 | (self.dword()? as u64) << 32)
    }

    /// A PkgLength's value.
    fn pkg_length(&mut self) -> Result<usize, Error> {
        let lead = self.byte()?;
        let follow = (lead >> 6) as usize;
        if follow == 0 {
            return Ok((lead & 0x3F) as usize);
        }
        let mut len = (lead & 0x0F) as usize;
        for i in 0..follow {
            len |= (self.byte()? as usize) << (4 + 8 * i);
        }
        Ok(len)
    }

    /// A PkgLength, as the offset where the package ends.
    fn pkg_end(&mut self) -> Result<usize, Error> {
        let start = self.pos;
        let end = start + self.pkg_length()?;
        if end > self.code.len() || end < self.pos {
            return Err(Error::Truncated(start));
        }
        Ok(end)
    }

    fn name_seg(&mut self) -> Result<NameSeg, Error> {
        let at = self.pos;
        let b = self.bytes(4)?;
        let lead = matches!(b[0], b'A'..=b'Z' | b'_');
        if !lead || !b[1..].iter().all(|&c| matches!(c, b'A'..=b'Z' | b'_' | b'0'..=b'9')) {
            return Err(Error::BadName(at));
        }
        Ok([b[0], b[1], b[2], b[3]])
    }

    fn name_string(&mut self) -> Result<NameString, Error> {
        let mut n = NameString::default();
        if self.peek()? == b'\\' {
            n.absolute = true;
            self.pos += 1;
        } else {
            while self.peek()? == b'^' {
                n.parents += 1;
                self.pos += 1;
            }
        }
        match self.peek()? {
            0x00 => self.pos += 1,
            0x2E => {
                self.pos += 1;
                n.segs.push(self.name_seg()?);
                n.segs.push(self.name_seg()?);
            }
            0x2F => {
                self.pos += 1;
                for _ in 0..self.byte()? {
                    n.segs.push(self.name_seg()?);
                }
            }
            _ => n.segs.push(self.name_seg()?),
        }
        Ok(n)
    }

    /// A string's characters, up to and including its terminating NUL.
    fn string(&mut self) -> Result<String, Error> {
        let at = self.pos;
        let len = self.code[at..].iter().position(|&b| b == 0).ok_or(Error::Truncated(at))?;
        self.pos = at + len + 1;
        Ok(String::from_utf8_lossy(&self.code[at..at + len]).into_owned())
    }
}

fn is_name_start(b: u8) -> bool {
    matches!(b, b'\\' | b'^' | 0x2E | 0x2F | b'A'..=b'Z' | b'_')
}

fn int(v: &Value) -> Result<u64, Error> {
    v.as_integer().ok_or(Error::Type("an integer"))
}

/// One method invocation: its scope, arguments, locals and the objects it
/// has defined.
struct Frame {
    scope: Path,
    args: Vec<Value>,
    locals: [Value; 8],
    objects: BTreeMap<Path, Object>,
}

impl Frame {
    fn new(scope: Path, args: Vec<Value>) -> Frame {
        Frame { scope, args, locals: Default::default(), objects: BTreeMap::new() }
    }
}

/// How running a list of terms ended.
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// What a store goes to.
#[derive(Debug, Clone)]
enum Target {
    None,
    Local(u8),
    Arg(u8),
    Named(Path),
    Element(Place, usize),
}

/// One evaluation.
struct Evaluator<'n> {
    ns: &'n Namespace,
    memory: &'n dyn Memory,
    steps: u32,
    depth: u32,
    /// The namespace's names and fields as this evaluation has stored to
    /// them (the firmware's own are never written).
    written: BTreeMap<Path, Value>,
    /// The PCI functions of the `PCI_Config` regions read so far.
    pci: BTreeMap<Path, PciFunction>,
}

impl<'n> Evaluator<'n> {
    fn new(ns: &'n Namespace, memory: &'n dyn Memory) -> Evaluator<'n> {
        Evaluator { ns, memory, steps: 0, depth: 0, written: BTreeMap::new(), pci: BTreeMap::new() }
    }

    fn tick(&mut self) -> Result<(), Error> {
        self.steps += 1;
        if self.steps > MAX_STEPS { Err(Error::Limit) } else { Ok(()) }
    }

    fn mask(&self, v: u64) -> u64 {
        v & self.ns.ones()
    }

    fn truth(&self, b: bool) -> Value {
        Value::Integer(if b { self.ns.ones() } else { 0 })
    }

    fn object<'a>(&'a self, f: &'a Frame, path: &Path) -> Option<&'a Object> {
        match f.objects.get(path) {
            Some(o) => Some(o),
            None => self.ns.get(path),
        }
    }

    fn lookup(&self, f: &Frame, name: &NameString) -> Option<Path> {
        search(&f.scope, name, |p| f.objects.contains_key(p) || self.ns.objects.contains_key(p))
    }

    /// Where a name points and, if it is a method, how many arguments a
    /// call to it takes.
    fn arity(&self, f: &Frame, name: &NameString) -> (Option<Path>, Option<u8>) {
        if let Some(p) = self.lookup(f, name) {
            let n = match self.object(f, &p) {
                Some(Object::Method { args, .. }) => Some(*args),
                Some(Object::Osi) => Some(1),
                _ => None,
            };
            return (Some(p), n);
        }
        match search(&f.scope, name, |p| self.ns.externals.contains_key(p)) {
            Some(p) => {
                let n = self.ns.externals.get(&p).copied();
                (Some(p), n)
            }
            None => (None, None),
        }
    }

    /// Evaluates (`run`) or only parses one term.
    fn term(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<Value, Error> {
        self.tick()?;
        let at = c.pos;
        let op = c.byte()?;
        Ok(match op {
            0x00 => Value::Integer(0),
            0x01 => Value::Integer(1),
            0xFF => Value::Integer(self.ns.ones()),
            0x0A => Value::Integer(c.byte()? as u64),
            0x0B => Value::Integer(c.word()? as u64),
            0x0C => Value::Integer(c.dword()? as u64),
            0x0E => Value::Integer(c.qword()?),
            0x0D => Value::String(c.string()?),
            0x11 => Value::Buffer(self.buffer(f, c, run)?),
            0x12 => Value::Package(self.package(f, c, run, false)?),
            0x13 => Value::Package(self.package(f, c, run, true)?),
            0x60..=0x67 if run => f.locals[(op - 0x60) as usize].clone(),
            0x68..=0x6E if run => f.args.get((op - 0x68) as usize).cloned().unwrap_or_default(),
            0x60..=0x6E => Value::Uninitialized,
            b if is_name_start(b) => {
                c.pos = at;
                return self.name_term(f, c, run);
            }
            0x5B => return self.extended(f, c, at, run),
            _ => return self.operator(f, c, op, at, run),
        })
    }

    /// A name in a term: a call if it is a method, else its value.
    fn name_term(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<Value, Error> {
        let name = c.name_string()?;
        let (path, arity) = self.arity(f, &name);
        if let Some(n) = arity {
            let mut args = Vec::with_capacity(n as usize);
            for _ in 0..n {
                args.push(self.term(f, c, run)?);
            }
            if !run {
                return Ok(Value::Uninitialized);
            }
            let path = path.unwrap_or_else(|| name.in_scope(&f.scope));
            return match self.object(f, &path) {
                Some(Object::Osi) => Ok(self.osi(args.first())),
                Some(Object::Method { .. }) => self.call(&path, args),
                _ => Err(Error::NoSuchName(path)),
            };
        }
        if !run {
            return Ok(Value::Uninitialized);
        }
        let path = path.ok_or_else(|| Error::NoSuchName(name.in_scope(&f.scope)))?;
        self.read_name(f, &path)
    }

    /// The value of a named object.
    fn read_name(&mut self, f: &mut Frame, path: &Path) -> Result<Value, Error> {
        if let Some(o) = f.objects.get(path) {
            return match o.clone() {
                Object::Name(v) => Ok(v),
                Object::BufferField { place, bit_offset, bit_width } => {
                    let source = self.read_place(f, &place);
                    let bytes = source.to_bytes().ok_or(Error::Type("a buffer"))?;
                    bits_of(&bytes, bit_offset, bit_width)
                }
                Object::Field(field) => match self.written.get(path) {
                    Some(v) => Ok(v.clone()),
                    None => self.read_field(f, &field),
                },
                _ => Ok(Value::Reference(path.clone())),
            };
        }
        if let Some(v) = self.written.get(path) {
            return Ok(v.clone());
        }
        let ns = self.ns;
        match ns.get(path) {
            Some(Object::Name(v)) => Ok(v.clone()),
            Some(Object::Field(field)) => self.read_field(f, field),
            Some(Object::Method { args: 0, .. }) => self.call(path, Vec::new()),
            Some(_) => Ok(Value::Reference(path.clone())),
            None => Err(Error::NoSuchName(path.clone())),
        }
    }

    /// Reads a field of a `SystemMemory` or a `PCI_Config` region.
    fn read_field(&mut self, f: &Frame, field: &Field) -> Result<Value, Error> {
        let FieldUnit::Region(region) = &field.unit else {
            return Err(Error::Unsupported("reading an index or bank field"));
        };
        let Some(Object::Region { space, bounds }) = self.object(f, region).cloned() else {
            return Err(Error::NoSuchName(region.clone()));
        };
        match space {
            0 | 2 => {}
            1 => return Err(Error::Unsupported("reading I/O ports")),
            3 => return Err(Error::Unsupported("reading the embedded controller")),
            _ => return Err(Error::Unsupported("reading this address space")),
        }
        let (base, length) = match bounds {
            Bounds::Known { offset, length } => (offset, length),
            Bounds::Deferred { scope, code } => {
                let bytes = self.ns.code.get(code.table).ok_or(Error::Truncated(code.start))?.clone();
                let mut c = Cursor { code: &bytes[..code.end], pos: code.start };
                self.region_bounds(&scope, &mut c)?
            }
            Bounds::Unknown => return Err(Error::Unsupported("a region whose address cannot be evaluated")),
        };
        // Whole accesses of the field's width, as the hardware may need.
        let unit = (field.access_bits / 8).max(1) as u64;
        let first = field.bit_offset / 8 / unit * unit;
        let last = (field.bit_offset + field.bit_width).div_ceil(8).div_ceil(unit) * unit;
        if last > length || last - first > MAX_BUFFER as u64 {
            return Err(Error::Type("a field inside its region"));
        }
        let mut bytes = vec![0u8; (last - first) as usize];
        let address = base.wrapping_add(first);
        if space == 2 {
            // An offset in the function's configuration space (4 KiB).
            let function = self.pci_function(f, region)?;
            let offset = u16::try_from(address)
                .ok()
                .filter(|&o| o as usize + bytes.len() <= 0x1000)
                .ok_or(Error::Type("a field inside configuration space"))?;
            if !self.memory.read_pci(function, offset, &mut bytes) {
                return Err(Error::PciConfig(function, offset));
            }
        } else if !self.memory.read(address, &mut bytes) {
            return Err(Error::Memory(address));
        }
        bits_of(&bytes, field.bit_offset - first * 8, field.bit_width)
    }

    /// The PCI function whose configuration space the `PCI_Config` region
    /// at `region` is, as ACPI has it (and ACPICA finds it): the one at the
    /// `_ADR` of the device the region is in, on the bus of the PCI root
    /// bridge above it (its `_SEG` and `_BBN`, 0 without them), or of the
    /// last PCI-to-PCI bridge between the two (the secondary bus its
    /// configuration space holds).
    fn pci_function(&mut self, f: &Frame, region: &Path) -> Result<PciFunction, Error> {
        if let Some(&function) = self.pci.get(region) {
            return Ok(function);
        }
        // Finding it evaluates the devices' objects, which could read such
        // a region in turn: bounded as calls are.
        if self.depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        self.depth += 1;
        let found = self.find_pci_function(f, region);
        self.depth -= 1;
        let function = found?;
        self.pci.insert(region.clone(), function);
        Ok(function)
    }

    fn find_pci_function(&mut self, f: &Frame, region: &Path) -> Result<PciFunction, Error> {
        // The devices the region is in up to the root bridge, nearest first.
        let mut devices = Vec::new();
        let mut root = None;
        let mut at = region.parent();
        while let Some(p) = at {
            if matches!(self.object(f, &p), Some(Object::Device)) {
                if ["_HID", "_CID"].iter().any(|id| self.child_value(&p, id).is_some_and(|v| is_pci_root_id(&v))) {
                    root = Some(p);
                    break;
                }
                devices.push(p.clone());
            }
            at = p.parent();
        }
        let mut function = PciFunction::default();
        if let Some(root) = &root {
            function.segment = self.child_integer(root, "_SEG").unwrap_or(0) as u16;
            function.bus = self.child_integer(root, "_BBN").unwrap_or(0) as u8;
            if devices.is_empty() {
                // A region of the root bridge's own.
                set_pci_address(&mut function, self.child_integer(root, "_ADR").unwrap_or(0));
            }
        }
        // From the root bridge down to the region's device: a bridge between
        // them (its header type 1, or 2 for CardBus) puts the devices below
        // it on its secondary bus.
        let mut bus = function.bus;
        for (i, d) in devices.iter().enumerate().rev() {
            let Some(adr) = self.child_integer(d, "_ADR") else { continue };
            set_pci_address(&mut function, adr);
            function.bus = bus;
            if i > 0 && matches!(self.config_byte(function, 0x0E)? & 0x7F, 1 | 2) {
                bus = self.config_byte(function, 0x19)?;
            }
        }
        Ok(function)
    }

    /// The value of `device`'s object `name` (`_ADR`) in this evaluation,
    /// if it has one that evaluates.
    fn child_value(&mut self, device: &Path, name: &str) -> Option<Value> {
        let path = device.join(name)?;
        self.ns.get(&path)?;
        self.read_name(&mut Frame::new(device.clone(), Vec::new()), &path).ok()
    }

    fn child_integer(&mut self, device: &Path, name: &str) -> Option<u64> {
        self.child_value(device, name).and_then(|v| v.as_integer())
    }

    fn config_byte(&self, function: PciFunction, offset: u16) -> Result<u8, Error> {
        let mut b = [0u8];
        if self.memory.read_pci(function, offset, &mut b) { Ok(b[0]) } else { Err(Error::PciConfig(function, offset)) }
    }

    fn read_place(&self, f: &Frame, place: &Place) -> Value {
        match place {
            Place::Local(n) => f.locals[*n as usize].clone(),
            Place::Arg(n) => f.args.get(*n as usize).cloned().unwrap_or_default(),
            Place::Named(p) => match f.objects.get(p) {
                Some(Object::Name(v)) => v.clone(),
                _ => match (self.written.get(p), self.ns.get(p)) {
                    (Some(v), _) => v.clone(),
                    (None, Some(Object::Name(v))) => v.clone(),
                    _ => Value::Uninitialized,
                },
            },
            Place::Temporary(v) => v.clone(),
        }
    }

    /// Changes the value held in a place.
    fn update_place(
        &mut self,
        f: &mut Frame,
        place: &Place,
        change: impl FnOnce(&mut Value) -> Result<(), Error>,
    ) -> Result<(), Error> {
        match place {
            Place::Local(n) => change(&mut f.locals[*n as usize]),
            Place::Arg(n) => match f.args.get_mut(*n as usize) {
                Some(v) => change(v),
                None => Err(Error::Type("an argument")),
            },
            Place::Named(p) => {
                if let Some(Object::Name(v)) = f.objects.get_mut(p) {
                    return change(v);
                }
                let mut v = match (self.written.get(p), self.ns.get(p)) {
                    (Some(v), _) => v.clone(),
                    (None, Some(Object::Name(v))) => v.clone(),
                    _ => return Err(Error::Type("a named buffer or package")),
                };
                change(&mut v)?;
                self.written.insert(p.clone(), v);
                Ok(())
            }
            Place::Temporary(v) => change(&mut v.clone()),
        }
    }

    fn store(&mut self, f: &mut Frame, target: &Target, value: Value) -> Result<(), Error> {
        match target {
            Target::None => Ok(()),
            Target::Local(n) => {
                f.locals[*n as usize] = value;
                Ok(())
            }
            Target::Arg(n) => {
                let n = *n as usize;
                if f.args.len() <= n {
                    f.args.resize(n + 1, Value::Uninitialized);
                }
                f.args[n] = value;
                Ok(())
            }
            Target::Element(place, i) => {
                let i = *i;
                self.update_place(f, place, |v| set_element(v, i, value))
            }
            Target::Named(path) => self.store_named(f, path, value),
        }
    }

    fn store_named(&mut self, f: &mut Frame, path: &Path, value: Value) -> Result<(), Error> {
        if let Some(o) = f.objects.get_mut(path) {
            match o {
                Object::Name(v) => {
                    *v = converted(v, value);
                    return Ok(());
                }
                Object::BufferField { place, bit_offset, bit_width } => {
                    let (place, offset, width) = (place.clone(), *bit_offset, *bit_width);
                    return self.update_place(f, &place, |b| set_bits(b, offset, width, &value));
                }
                Object::Field(_) => {
                    self.written.insert(path.clone(), value);
                    return Ok(());
                }
                _ => return Err(Error::Unsupported("storing to this kind of object")),
            }
        }
        match self.ns.get(path) {
            Some(Object::Name(v)) => {
                let new = converted(self.written.get(path).unwrap_or(v), value);
                self.written.insert(path.clone(), new);
                Ok(())
            }
            // Kept for this evaluation; the firmware's field is not written.
            Some(Object::Field(_)) => {
                self.written.insert(path.clone(), value);
                Ok(())
            }
            Some(_) => Err(Error::Unsupported("storing to this kind of object")),
            None => Err(Error::NoSuchName(path.clone())),
        }
    }

    fn read_target(&mut self, f: &mut Frame, target: &Target) -> Result<Value, Error> {
        match target {
            Target::None => Ok(Value::Uninitialized),
            Target::Local(n) => Ok(f.locals[*n as usize].clone()),
            Target::Arg(n) => Ok(f.args.get(*n as usize).cloned().unwrap_or_default()),
            Target::Named(p) => self.read_name(f, p),
            Target::Element(place, i) => element(&self.read_place(f, place), *i),
        }
    }

    /// A SuperName or Target: where a result goes.
    fn target(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<Target, Error> {
        let at = c.pos;
        match c.peek()? {
            0x00 => {
                c.pos += 1;
                Ok(Target::None)
            }
            op @ 0x60..=0x67 => {
                c.pos += 1;
                Ok(Target::Local(op - 0x60))
            }
            op @ 0x68..=0x6E => {
                c.pos += 1;
                Ok(Target::Arg(op - 0x68))
            }
            // Debug
            0x5B if c.peek_at(1) == Some(0x31) => {
                c.pos += 2;
                Ok(Target::None)
            }
            b if is_name_start(b) => {
                let name = c.name_string()?;
                match self.arity(f, &name) {
                    // A method that returns a reference.
                    (_, Some(n)) if n > 0 => {
                        c.pos = at;
                        let v = self.term(f, c, run)?;
                        reference_target(v, run)
                    }
                    (Some(p), _) => Ok(Target::Named(p)),
                    (None, _) => Ok(Target::Named(name.in_scope(&f.scope))),
                }
            }
            // Index, DerefOf, RefOf
            _ => {
                let v = self.term(f, c, run)?;
                reference_target(v, run)
            }
        }
    }

    /// An operand whose place matters (`Index`, `CreateDWordField`).
    fn operand_place(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<Place, Error> {
        let at = c.pos;
        match c.peek()? {
            op @ 0x60..=0x67 => {
                c.pos += 1;
                Ok(Place::Local(op - 0x60))
            }
            op @ 0x68..=0x6E => {
                c.pos += 1;
                Ok(Place::Arg(op - 0x68))
            }
            b if is_name_start(b) => {
                let name = c.name_string()?;
                if let (Some(p), None) = self.arity(f, &name)
                    && matches!(self.object(f, &p), Some(Object::Name(_)))
                {
                    return Ok(Place::Named(p));
                }
                c.pos = at;
                Ok(Place::Temporary(self.term(f, c, run)?))
            }
            _ => Ok(Place::Temporary(self.term(f, c, run)?)),
        }
    }

    fn buffer(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<Vec<u8>, Error> {
        let end = c.pkg_end()?;
        let size = self.term(f, c, run)?;
        if c.pos > end {
            return Err(Error::Truncated(c.pos));
        }
        let code = c.code;
        let init = &code[c.pos..end];
        c.pos = end;
        if !run {
            return Ok(Vec::new());
        }
        let size = int(&size)? as usize;
        if size > MAX_BUFFER {
            return Err(Error::Limit);
        }
        let mut b = vec![0u8; size.max(init.len())];
        b[..init.len()].copy_from_slice(init);
        Ok(b)
    }

    fn package(&mut self, f: &mut Frame, c: &mut Cursor, run: bool, var: bool) -> Result<Vec<Value>, Error> {
        let end = c.pkg_end()?;
        let count = if var { int(&self.term(f, c, run)?)? as usize } else { c.byte()? as usize };
        let mut items = Vec::new();
        while c.pos < end {
            // A name in a package is a reference to the object, not its value.
            let item = if is_name_start(c.peek()?) {
                let n = c.name_string()?;
                Value::Reference(self.lookup(f, &n).unwrap_or_else(|| n.in_scope(&f.scope)))
            } else {
                self.term(f, c, run)?
            };
            items.push(item);
        }
        if c.pos != end {
            return Err(Error::Truncated(end));
        }
        if run && items.len() < count {
            items.resize(count.min(MAX_ELEMENTS), Value::Uninitialized);
        }
        Ok(items)
    }

    /// Evaluates an operation region's offset and length.
    fn region_bounds(&mut self, scope: &Path, c: &mut Cursor) -> Result<(u64, u64), Error> {
        let mut f = Frame::new(scope.clone(), Vec::new());
        let offset = int(&self.term(&mut f, c, true)?)?;
        let length = int(&self.term(&mut f, c, true)?)?;
        Ok((offset, length))
    }

    fn osi(&self, arg: Option<&Value>) -> Value {
        let s = arg.and_then(Value::as_str).unwrap_or("");
        // As Windows answers (the firmware is written for it), and the
        // features every interpreter has.
        let yes = s.starts_with("Windows ")
            || matches!(
                s,
                "Module Device"
                    | "Processor Device"
                    | "3.0 Thermal Model"
                    | "3.0 _SCP Extensions"
                    | "Processor Aggregator Device"
                    | "Extended Address Space Descriptor"
            );
        self.truth(yes)
    }

    fn call(&mut self, path: &Path, args: Vec<Value>) -> Result<Value, Error> {
        let Some(Object::Method { code, .. }) = self.ns.get(path) else {
            return Err(Error::NoSuchName(path.clone()));
        };
        if self.depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        let code = *code;
        let bytes = self.ns.code.get(code.table).ok_or(Error::Truncated(code.start))?.clone();
        let mut c = Cursor { code: &bytes[..code.end], pos: code.start };
        let mut f = Frame::new(path.clone(), args);
        self.depth += 1;
        let flow = self.run_list(&mut f, &mut c, code.end);
        self.depth -= 1;
        Ok(match flow? {
            // A reference into the method's own objects outlives them as a value.
            Flow::Return(Value::Element(place, i)) => {
                Value::Element(Box::new(Place::Temporary(self.read_place(&f, &place))), i)
            }
            Flow::Return(v) => v,
            _ => Value::Uninitialized,
        })
    }

    fn run_list(&mut self, f: &mut Frame, c: &mut Cursor, end: usize) -> Result<Flow, Error> {
        while c.pos < end {
            match self.statement(f, c)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn statement(&mut self, f: &mut Frame, c: &mut Cursor) -> Result<Flow, Error> {
        self.tick()?;
        match c.peek()? {
            // If, with an optional Else.
            0xA0 => {
                c.pos += 1;
                let end = c.pkg_end()?;
                let taken = int(&self.term(f, c, true)?)? != 0;
                let mut flow = Flow::Normal;
                if taken {
                    flow = self.run_list(f, c, end)?;
                }
                c.pos = end;
                if c.peek().ok() == Some(0xA1) {
                    c.pos += 1;
                    let else_end = c.pkg_end()?;
                    if !taken {
                        flow = self.run_list(f, c, else_end)?;
                    }
                    c.pos = else_end;
                }
                Ok(flow)
            }
            // While
            0xA2 => {
                c.pos += 1;
                let end = c.pkg_end()?;
                let condition = c.pos;
                loop {
                    c.pos = condition;
                    if int(&self.term(f, c, true)?)? == 0 {
                        break;
                    }
                    match self.run_list(f, c, end)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Normal | Flow::Continue => {}
                    }
                }
                c.pos = end;
                Ok(Flow::Normal)
            }
            0xA4 => {
                c.pos += 1;
                Ok(Flow::Return(self.term(f, c, true)?))
            }
            0xA5 => {
                c.pos += 1;
                Ok(Flow::Break)
            }
            0x9F => {
                c.pos += 1;
                Ok(Flow::Continue)
            }
            0xA3 | 0xCC => {
                c.pos += 1;
                Ok(Flow::Normal)
            }
            // Name: an object of this invocation.
            0x08 => {
                c.pos += 1;
                let name = c.name_string()?;
                let value = self.term(f, c, true)?;
                f.objects.insert(name.in_scope(&f.scope), Object::Name(value));
                Ok(Flow::Normal)
            }
            0x14 | 0x10 => Err(Error::Unsupported("a method or scope defined inside a method")),
            0x5B => match c.peek_at(1) {
                // OperationRegion
                Some(0x80) => {
                    c.pos += 2;
                    let name = c.name_string()?;
                    let space = c.byte()?;
                    let offset = self.term(f, c, true)?.as_integer();
                    let length = self.term(f, c, true)?.as_integer();
                    let bounds = match (offset, length) {
                        (Some(offset), Some(length)) => Bounds::Known { offset, length },
                        _ => Bounds::Unknown,
                    };
                    f.objects.insert(name.in_scope(&f.scope), Object::Region { space, bounds });
                    Ok(Flow::Normal)
                }
                // Field (the others are not read anyway)
                Some(op @ (0x81 | 0x86 | 0x87)) => {
                    c.pos += 2;
                    let end = c.pkg_end()?;
                    let first = c.name_string()?;
                    let unit = match op {
                        0x81 => FieldUnit::Region(self.lookup(f, &first).unwrap_or_else(|| first.in_scope(&f.scope))),
                        0x86 => {
                            c.name_string()?;
                            FieldUnit::Index
                        }
                        _ => {
                            c.name_string()?;
                            self.term(f, c, false)?;
                            FieldUnit::Bank
                        }
                    };
                    let flags = c.byte()?;
                    let scope = f.scope.clone();
                    let mut defined = Vec::new();
                    field_list(c, end, flags, &unit, &scope, &mut |p, o| defined.push((p, o)))?;
                    f.objects.extend(defined);
                    c.pos = end;
                    Ok(Flow::Normal)
                }
                Some(0x01) => {
                    c.pos += 2;
                    let name = c.name_string()?;
                    c.byte()?;
                    f.objects.insert(name.in_scope(&f.scope), Object::Mutex);
                    Ok(Flow::Normal)
                }
                Some(0x02) => {
                    c.pos += 2;
                    let name = c.name_string()?;
                    f.objects.insert(name.in_scope(&f.scope), Object::Event);
                    Ok(Flow::Normal)
                }
                Some(0x82..=0x85) => Err(Error::Unsupported("a device defined inside a method")),
                _ => {
                    self.term(f, c, true)?;
                    Ok(Flow::Normal)
                }
            },
            _ => {
                self.term(f, c, true)?;
                Ok(Flow::Normal)
            }
        }
    }

    /// Two operands and a target, for the binary operators.
    fn binary(&mut self, f: &mut Frame, c: &mut Cursor, run: bool) -> Result<(Value, Value, Target), Error> {
        let a = self.term(f, c, run)?;
        let b = self.term(f, c, run)?;
        let t = self.target(f, c, run)?;
        Ok((a, b, t))
    }

    fn operator(&mut self, f: &mut Frame, c: &mut Cursor, op: u8, at: usize, run: bool) -> Result<Value, Error> {
        match op {
            // Store
            0x70 => {
                let v = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if run {
                    self.store(f, &t, v.clone())?;
                }
                Ok(v)
            }
            // RefOf
            0x71 => {
                let t = self.target(f, c, run)?;
                Ok(match t {
                    Target::Named(p) => Value::Reference(p),
                    Target::Element(place, i) => Value::Element(Box::new(place), i),
                    _ => Value::Uninitialized,
                })
            }
            // Add, Subtract, Multiply, ShiftLeft, ShiftRight, And, Nand, Or, Nor, Xor, Mod
            0x72 | 0x74 | 0x77 | 0x79 | 0x7A | 0x7B | 0x7C | 0x7D | 0x7E | 0x7F | 0x85 => {
                let (a, b, t) = self.binary(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let (a, b) = (int(&a)?, int(&b)?);
                let r = match op {
                    0x72 => a.wrapping_add(b),
                    0x74 => a.wrapping_sub(b),
                    0x77 => a.wrapping_mul(b),
                    0x79 => a.checked_shl(b.min(64) as u32).unwrap_or(0),
                    0x7A => a.checked_shr(b.min(64) as u32).unwrap_or(0),
                    0x7B => a & b,
                    0x7C => !(a & b),
                    0x7D => a | b,
                    0x7E => !(a | b),
                    0x7F => a ^ b,
                    _ => a.checked_rem(b).ok_or(Error::DivideByZero)?,
                };
                let r = Value::Integer(self.mask(r));
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // Concatenate, ConcatenateResTemplate
            0x73 | 0x84 => {
                let (a, b, t) = self.binary(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let r = if op == 0x73 { concatenate(&a, &b)? } else { concatenate_templates(&a, &b)? };
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // Increment, Decrement
            0x75 | 0x76 => {
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let v = int(&self.read_target(f, &t)?)?;
                let r = Value::Integer(self.mask(if op == 0x75 { v.wrapping_add(1) } else { v.wrapping_sub(1) }));
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // Divide: remainder, then quotient.
            0x78 => {
                let a = self.term(f, c, run)?;
                let b = self.term(f, c, run)?;
                let remainder = self.target(f, c, run)?;
                let quotient = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let (a, b) = (int(&a)?, int(&b)?);
                if b == 0 {
                    return Err(Error::DivideByZero);
                }
                self.store(f, &remainder, Value::Integer(a % b))?;
                self.store(f, &quotient, Value::Integer(a / b))?;
                Ok(Value::Integer(a / b))
            }
            // Not, FindSetLeftBit, FindSetRightBit, FromBCD-likes
            0x80..=0x82 => {
                let a = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let a = int(&a)?;
                let r = match op {
                    0x80 => self.mask(!a),
                    0x81 => 64 - a.leading_zeros() as u64,
                    _ => {
                        if a == 0 {
                            0
                        } else {
                            a.trailing_zeros() as u64 + 1
                        }
                    }
                };
                let r = Value::Integer(r);
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // DerefOf
            0x83 => {
                let v = self.term(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                match v {
                    Value::Reference(p) => self.read_name(f, &p),
                    Value::Element(place, i) => element(&self.read_place(f, &place), i),
                    Value::String(s) => {
                        let name = NameString::parse(&s).ok_or(Error::Type("a name"))?;
                        let p = self.lookup(f, &name).ok_or_else(|| Error::NoSuchName(name.in_scope(&f.scope)))?;
                        self.read_name(f, &p)
                    }
                    _ => Err(Error::Type("a reference")),
                }
            }
            // Notify
            0x86 => {
                self.target(f, c, run)?;
                self.term(f, c, run)?;
                Ok(Value::Uninitialized)
            }
            // SizeOf
            0x87 => {
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let v = match self.read_target(f, &t)? {
                    Value::Reference(p) => self.read_name(f, &p)?,
                    v => v,
                };
                Ok(Value::Integer(match v {
                    Value::String(s) => s.len() as u64,
                    Value::Buffer(b) => b.len() as u64,
                    Value::Package(p) => p.len() as u64,
                    _ => return Err(Error::Type("a string, buffer or package")),
                }))
            }
            // Index
            0x88 => {
                let place = self.operand_place(f, c, run)?;
                let i = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let r = Value::Element(Box::new(place), int(&i)? as usize);
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // Match
            0x89 => {
                let package = self.term(f, c, run)?;
                let op1 = c.byte()?;
                let v1 = self.term(f, c, run)?;
                let op2 = c.byte()?;
                let v2 = self.term(f, c, run)?;
                let start = self.term(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let Value::Package(items) = package else { return Err(Error::Type("a package")) };
                let (v1, v2) = (int(&v1)?, int(&v2)?);
                let test = |op: u8, x: u64, v: u64| match op {
                    0 => true,
                    1 => x == v,
                    2 => x <= v,
                    3 => x < v,
                    4 => x >= v,
                    _ => x > v,
                };
                for (i, item) in items.iter().enumerate().skip(int(&start)? as usize) {
                    if let Some(x) = item.as_integer()
                        && test(op1, x, v1)
                        && test(op2, x, v2)
                    {
                        return Ok(Value::Integer(i as u64));
                    }
                }
                Ok(Value::Integer(self.ns.ones()))
            }
            // CreateDWordField, CreateWordField, CreateByteField, CreateBitField, CreateQWordField
            0x8A | 0x8B | 0x8C | 0x8D | 0x8F => {
                let place = self.operand_place(f, c, run)?;
                let index = self.term(f, c, run)?;
                let name = c.name_string()?;
                if run {
                    let index = int(&index)?;
                    let (bit_offset, bit_width) = match op {
                        0x8A => (index * 8, 32),
                        0x8B => (index * 8, 16),
                        0x8C => (index * 8, 8),
                        0x8D => (index, 1),
                        _ => (index * 8, 64),
                    };
                    f.objects.insert(name.in_scope(&f.scope), Object::BufferField { place, bit_offset, bit_width });
                }
                Ok(Value::Uninitialized)
            }
            // ObjectType
            0x8E => {
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                Ok(Value::Integer(match &t {
                    Target::Named(p) => match self.object(f, p) {
                        Some(Object::Name(v)) => v.type_code(),
                        Some(Object::Field(_)) => 5,
                        Some(Object::Device) => 6,
                        Some(Object::Event) => 7,
                        Some(Object::Method { .. }) | Some(Object::Osi) => 8,
                        Some(Object::Mutex) => 9,
                        Some(Object::Region { .. }) => 10,
                        Some(Object::PowerResource) => 11,
                        Some(Object::Processor) => 12,
                        Some(Object::ThermalZone) => 13,
                        Some(Object::BufferField { .. }) => 14,
                        _ => 0,
                    },
                    other => self.read_target(f, other)?.type_code(),
                }))
            }
            // LAnd, LOr
            0x90 | 0x91 => {
                let a = self.term(f, c, run)?;
                let b = self.term(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let (a, b) = (int(&a)? != 0, int(&b)? != 0);
                Ok(self.truth(if op == 0x90 { a && b } else { a || b }))
            }
            // LNot
            0x92 => {
                let a = self.term(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                Ok(self.truth(int(&a)? == 0))
            }
            // LEqual, LGreater, LLess
            0x93..=0x95 => {
                let a = self.term(f, c, run)?;
                let b = self.term(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let order = compare(&a, &b)?;
                Ok(self.truth(match op {
                    0x93 => order.is_eq(),
                    0x94 => order.is_gt(),
                    _ => order.is_lt(),
                }))
            }
            // ToBuffer, ToDecimalString, ToHexString, ToInteger
            0x96..=0x99 => {
                let a = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let r = match op {
                    0x96 => Value::Buffer(a.to_bytes().ok_or(Error::Type("data"))?),
                    0x97 => Value::String(to_decimal_string(&a)?),
                    0x98 => Value::String(to_hex_string(&a)?),
                    _ => Value::Integer(to_integer(&a)?),
                };
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // ToString
            0x9C => {
                let a = self.term(f, c, run)?;
                let len = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let bytes = a.to_bytes().ok_or(Error::Type("a buffer"))?;
                let len = int(&len)? as usize;
                let n = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len()).min(len);
                let r = Value::String(String::from_utf8_lossy(&bytes[..n]).into_owned());
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // CopyObject
            0x9D => {
                let v = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if run {
                    match &t {
                        Target::Named(p) if f.objects.contains_key(p) => {
                            f.objects.insert(p.clone(), Object::Name(v.clone()));
                        }
                        Target::Named(p) => {
                            self.written.insert(p.clone(), v.clone());
                        }
                        other => self.store(f, other, v.clone())?,
                    }
                }
                Ok(v)
            }
            // Mid
            0x9E => {
                let a = self.term(f, c, run)?;
                let index = self.term(f, c, run)?;
                let len = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let (index, len) = (int(&index)? as usize, int(&len)? as usize);
                let r = match &a {
                    Value::String(s) => {
                        let b = s.as_bytes();
                        let start = index.min(b.len());
                        let end = start.saturating_add(len).min(b.len());
                        Value::String(String::from_utf8_lossy(&b[start..end]).into_owned())
                    }
                    Value::Buffer(b) => {
                        let start = index.min(b.len());
                        let end = start.saturating_add(len).min(b.len());
                        Value::Buffer(b[start..end].to_vec())
                    }
                    _ => return Err(Error::Type("a string or buffer")),
                };
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            // Statements met while only parsing (code outside methods).
            0xA0..=0xA2 if !run => {
                c.pos = c.pkg_end()?;
                Ok(Value::Uninitialized)
            }
            _ => Err(Error::Opcode(op as u16, at)),
        }
    }

    fn extended(&mut self, f: &mut Frame, c: &mut Cursor, at: usize, run: bool) -> Result<Value, Error> {
        let op = c.byte()?;
        match op {
            // CondRefOf: the name may not exist.
            0x12 => {
                let found = if is_name_start(c.peek()?) {
                    let name = c.name_string()?;
                    self.lookup(f, &name).map(Value::Reference)
                } else {
                    match self.target(f, c, run)? {
                        Target::Named(p) => Some(Value::Reference(p)),
                        _ => None,
                    }
                };
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                match found {
                    Some(r) => {
                        self.store(f, &t, r)?;
                        Ok(self.truth(true))
                    }
                    None => Ok(Value::Integer(0)),
                }
            }
            // CreateField
            0x13 => {
                let place = self.operand_place(f, c, run)?;
                let bit = self.term(f, c, run)?;
                let width = self.term(f, c, run)?;
                let name = c.name_string()?;
                if run {
                    let (bit_offset, bit_width) = (int(&bit)?, int(&width)?);
                    f.objects.insert(name.in_scope(&f.scope), Object::BufferField { place, bit_offset, bit_width });
                }
                Ok(Value::Uninitialized)
            }
            // LoadTable, Load, Unload
            0x1F => {
                for _ in 0..6 {
                    self.term(f, c, run)?;
                }
                if run { Err(Error::Unsupported("loading tables")) } else { Ok(Value::Uninitialized) }
            }
            0x20 => {
                c.name_string()?;
                self.target(f, c, run)?;
                if run { Err(Error::Unsupported("loading tables")) } else { Ok(Value::Uninitialized) }
            }
            0x2A => {
                self.target(f, c, run)?;
                if run { Err(Error::Unsupported("unloading tables")) } else { Ok(Value::Uninitialized) }
            }
            // Stall, Sleep: nothing to wait for.
            0x21 | 0x22 => {
                self.term(f, c, run)?;
                Ok(Value::Uninitialized)
            }
            // Acquire (always acquired), Wait (always signalled)
            0x23 => {
                self.target(f, c, run)?;
                c.word()?;
                Ok(Value::Integer(0))
            }
            0x25 => {
                self.target(f, c, run)?;
                self.term(f, c, run)?;
                Ok(Value::Integer(0))
            }
            // Signal, Reset, Release
            0x24 | 0x26 | 0x27 => {
                self.target(f, c, run)?;
                Ok(Value::Uninitialized)
            }
            // FromBCD, ToBCD
            0x28 | 0x29 => {
                let a = self.term(f, c, run)?;
                let t = self.target(f, c, run)?;
                if !run {
                    return Ok(Value::Uninitialized);
                }
                let a = int(&a)?;
                let r = Value::Integer(if op == 0x28 { from_bcd(a) } else { to_bcd(a) });
                self.store(f, &t, r.clone())?;
                Ok(r)
            }
            0x30 => Ok(Value::Integer(REVISION)),
            0x31 => Ok(Value::Uninitialized),
            // Fatal
            0x32 => {
                c.bytes(5)?;
                self.term(f, c, run)?;
                if run { Err(Error::Unsupported("Fatal")) } else { Ok(Value::Uninitialized) }
            }
            0x33 => Ok(Value::Integer(0)),
            _ => Err(Error::Opcode(0x5B00 | op as u16, at)),
        }
    }
}

fn reference_target(v: Value, run: bool) -> Result<Target, Error> {
    match v {
        Value::Element(place, i) => Ok(Target::Element(*place, i)),
        Value::Reference(p) => Ok(Target::Named(p)),
        _ if !run => Ok(Target::None),
        _ => Err(Error::Type("a reference")),
    }
}

/// `PNP0A03` or `PNP0A08`, a PCI root bridge's id, as a compressed EISA
/// id or a string, or among a package of ids (`_CID`).
fn is_pci_root_id(v: &Value) -> bool {
    match v {
        Value::Integer(i) => matches!(*i, 0x030A_D041 | 0x080A_D041),
        Value::String(s) => s == "PNP0A03" || s == "PNP0A08",
        Value::Package(ids) => ids.iter().any(is_pci_root_id),
        _ => false,
    }
}

/// A PCI `_ADR`: the device in the high word, the function in the low one
/// (`0xFFFF` for every function, which no configuration space is).
fn set_pci_address(function: &mut PciFunction, adr: u64) {
    function.device = (adr >> 16) as u8;
    function.function = (adr & 0xFFFF).min(0xFF) as u8;
}

/// Bits `[bit, bit + width)` of `bytes`: an integer up to 64 bits, else a
/// buffer.
fn bits_of(bytes: &[u8], bit: u64, width: u64) -> Result<Value, Error> {
    if bit + width > bytes.len() as u64 * 8 {
        return Err(Error::Type("a field inside its buffer"));
    }
    let get = |i: u64| bytes[((bit + i) / 8) as usize] >> ((bit + i) % 8) & 1;
    if width <= 64 {
        Ok(Value::Integer((0..width).fold(0u64, |v, i| v | (get(i) as u64) << i)))
    } else {
        let mut out = vec![0u8; width.div_ceil(8) as usize];
        for i in 0..width {
            out[(i / 8) as usize] |= get(i) << (i % 8);
        }
        Ok(Value::Buffer(out))
    }
}

/// Stores `value` into bits `[bit, bit + width)` of the buffer `into`.
fn set_bits(into: &mut Value, bit: u64, width: u64, value: &Value) -> Result<(), Error> {
    let Value::Buffer(b) = into else { return Err(Error::Type("a buffer")) };
    if bit + width > b.len() as u64 * 8 {
        return Err(Error::Type("a field inside its buffer"));
    }
    let source = match value {
        Value::Integer(v) => v.to_le_bytes().to_vec(),
        other => other.to_bytes().ok_or(Error::Type("data"))?,
    };
    for i in 0..width {
        let on = source.get((i / 8) as usize).is_some_and(|&s| s >> (i % 8) & 1 != 0);
        let (at, mask) = (((bit + i) / 8) as usize, 1u8 << ((bit + i) % 8));
        if on {
            b[at] |= mask;
        } else {
            b[at] &= !mask;
        }
    }
    Ok(())
}

fn element(container: &Value, i: usize) -> Result<Value, Error> {
    match container {
        Value::Package(p) => p.get(i).cloned().ok_or(Error::Type("an index inside the package")),
        Value::Buffer(b) => {
            b.get(i).map(|&x| Value::Integer(x as u64)).ok_or(Error::Type("an index inside the buffer"))
        }
        Value::String(s) => {
            s.as_bytes().get(i).map(|&x| Value::Integer(x as u64)).ok_or(Error::Type("an index inside the string"))
        }
        _ => Err(Error::Type("a package, buffer or string")),
    }
}

fn set_element(container: &mut Value, i: usize, value: Value) -> Result<(), Error> {
    match container {
        Value::Package(p) => {
            *p.get_mut(i).ok_or(Error::Type("an index inside the package"))? = value;
            Ok(())
        }
        Value::Buffer(b) => {
            *b.get_mut(i).ok_or(Error::Type("an index inside the buffer"))? = int(&value)? as u8;
            Ok(())
        }
        _ => Err(Error::Type("a package or buffer")),
    }
}

/// A value stored to a named object takes the object's type.
fn converted(current: &Value, value: Value) -> Value {
    match (current, &value) {
        (Value::Integer(_), Value::Buffer(_) | Value::String(_)) => value.as_integer().map_or(value, Value::Integer),
        (Value::Buffer(old), Value::Integer(_) | Value::String(_) | Value::Buffer(_)) => {
            let bytes = value.to_bytes().unwrap_or_default();
            let mut b = vec![0u8; old.len().max(1)];
            let n = bytes.len().min(b.len());
            b[..n].copy_from_slice(&bytes[..n]);
            Value::Buffer(b)
        }
        (Value::String(_), Value::Integer(v)) => Value::String(format!("{:X}", v)),
        _ => value,
    }
}

fn compare(a: &Value, b: &Value) -> Result<core::cmp::Ordering, Error> {
    match a {
        Value::Integer(_) | Value::Uninitialized => Ok(int(a)?.cmp(&int(b)?)),
        Value::String(_) | Value::Buffer(_) => {
            let x = a.to_bytes().ok_or(Error::Type("data"))?;
            let y = match (a, b) {
                (Value::String(_), Value::Integer(v)) => format!("{:X}", v).into_bytes(),
                _ => b.to_bytes().ok_or(Error::Type("data"))?,
            };
            Ok(x.cmp(&y))
        }
        _ => Err(Error::Type("an integer, string or buffer")),
    }
}

fn concatenate(a: &Value, b: &Value) -> Result<Value, Error> {
    Ok(match a {
        Value::Integer(x) => {
            let mut v = x.to_le_bytes().to_vec();
            v.extend_from_slice(&match b {
                Value::Integer(y) => y.to_le_bytes().to_vec(),
                other => other.to_bytes().ok_or(Error::Type("data"))?,
            });
            Value::Buffer(v)
        }
        Value::String(s) => {
            let tail = match b {
                Value::String(t) => t.clone(),
                Value::Integer(v) => format!("{:X}", v),
                other => to_hex_string(other)?,
            };
            Value::String(format!("{}{}", s, tail))
        }
        Value::Buffer(x) => {
            let mut v = x.clone();
            v.extend_from_slice(&b.to_bytes().ok_or(Error::Type("data"))?);
            Value::Buffer(v)
        }
        _ => return Err(Error::Type("an integer, string or buffer")),
    })
}

/// Joins two resource templates: the first without its end tag.
fn concatenate_templates(a: &Value, b: &Value) -> Result<Value, Error> {
    let (Some(x), Some(y)) = (a.as_buffer(), b.as_buffer()) else {
        return Err(Error::Type("resource templates"));
    };
    let mut v = x.to_vec();
    if v.len() >= 2 && v[v.len() - 2] == 0x79 {
        v.truncate(v.len() - 2);
    }
    v.extend_from_slice(y);
    if y.is_empty() {
        v.extend_from_slice(&[0x79, 0]);
    }
    Ok(Value::Buffer(v))
}

fn to_decimal_string(v: &Value) -> Result<String, Error> {
    Ok(match v {
        Value::Integer(x) => format!("{}", x),
        Value::String(s) => s.clone(),
        Value::Buffer(b) => b.iter().map(|x| format!("{}", x)).collect::<Vec<_>>().join(","),
        _ => return Err(Error::Type("data")),
    })
}

fn to_hex_string(v: &Value) -> Result<String, Error> {
    Ok(match v {
        Value::Integer(x) => format!("{:X}", x),
        Value::String(s) => s.clone(),
        Value::Buffer(b) => b.iter().map(|x| format!("0x{:02X}", x)).collect::<Vec<_>>().join(","),
        _ => return Err(Error::Type("data")),
    })
}

fn to_integer(v: &Value) -> Result<u64, Error> {
    match v {
        Value::String(s) => {
            let s = s.trim();
            match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).map_err(|_| Error::Type("a number")),
                None => s.parse::<u64>().map_err(|_| Error::Type("a number")),
            }
        }
        other => int(other),
    }
}

fn from_bcd(v: u64) -> u64 {
    (0..16).rev().fold(0, |acc, i| acc * 10 + (v >> (i * 4) & 0xF))
}

fn to_bcd(mut v: u64) -> u64 {
    let mut r = 0;
    for i in 0..16 {
        r |= (v % 10) << (i * 4);
        v /= 10;
    }
    r
}
