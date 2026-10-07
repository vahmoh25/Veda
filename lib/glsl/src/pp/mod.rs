//! The preprocessor: GLSL ES 1.00 and 3.00, sections 3.3 to 3.5.
//!
//! It works as C++'s does, without `#` and `##` (neither version has
//! them), character constants or `#include`:
//!
//! * `#version` must come before anything else but white space and
//!   comments; it selects the language version (100 if absent). It is
//!   read before tokenizing, as GLSL ES 3.00 asks (line continuation
//!   exists only in 3.00).
//! * `#define` / `#undef` with object-like and function-like macros, whose
//!   arguments are fully expanded before substitution and whose
//!   replacement is rescanned with the macro disabled (a macro name met
//!   while disabled is never expanded again). Expansion is bounded, so
//!   that a hostile shader cannot grow it exponentially.
//! * `#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif`; in `#if`, an
//!   identifier that is not a macro is an error, not 0 (GLSL ES's rule).
//! * `#error` fails the compilation with its message, `#pragma` is ignored
//!   except for `STDGL invariant(all)`, `#line` renumbers lines and
//!   strings, and `#extension` sets an extension's behaviour.
//! * The predefined macros `__LINE__`, `__FILE__`, `__VERSION__`, `GL_ES`,
//!   `GL_FRAGMENT_PRECISION_HIGH` and one per supported extension.
//!
//! The result is the shader's tokens, with locations as `#line` made them.

mod expr;
pub mod lex;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

pub use lex::{PpKind, PpTok};

use crate::Version;
use crate::diag::{Diagnostics, Loc, error, warning};
use crate::intern::{Interner, Symbol};

/// A punctuator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Punct {
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Dot,
    Comma,
    Semicolon,
    Colon,
    Question,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Inc,
    Dec,
    Shl,
    Shr,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    Ne,
    Amp,
    Caret,
    Pipe,
    Tilde,
    Bang,
    AndAnd,
    OrOr,
    XorXor,
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    ShlAssign,
    ShrAssign,
    AndAssign,
    XorAssign,
    OrAssign,
    Hash,
}

impl Punct {
    /// The punctuator's spelling.
    pub fn as_str(self) -> &'static str {
        use Punct::*;
        match self {
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            Dot => ".",
            Comma => ",",
            Semicolon => ";",
            Colon => ":",
            Question => "?",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            Inc => "++",
            Dec => "--",
            Shl => "<<",
            Shr => ">>",
            Lt => "<",
            Gt => ">",
            Le => "<=",
            Ge => ">=",
            EqEq => "==",
            Ne => "!=",
            Amp => "&",
            Caret => "^",
            Pipe => "|",
            Tilde => "~",
            Bang => "!",
            AndAnd => "&&",
            OrOr => "||",
            XorXor => "^^",
            Assign => "=",
            AddAssign => "+=",
            SubAssign => "-=",
            MulAssign => "*=",
            DivAssign => "/=",
            ModAssign => "%=",
            ShlAssign => "<<=",
            ShrAssign => ">>=",
            AndAssign => "&=",
            XorAssign => "^=",
            OrAssign => "|=",
            Hash => "#",
        }
    }
}

/// The extensions the compiler knows. Which of them an implementation
/// offers is given in [`crate::Options::extensions`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Ext {
    /// `dFdx`, `dFdy` and `fwidth` in GLSL ES 1.00 fragment shaders.
    OesStandardDerivatives,
    /// `texture2DLodEXT` and friends in GLSL ES 1.00 fragment shaders.
    ExtShaderTextureLod,
    /// `gl_FragDepthEXT` in GLSL ES 1.00.
    ExtFragDepth,
    /// More than one `gl_FragData` in GLSL ES 1.00.
    ExtDrawBuffers,
    /// `sampler3D` and `texture3D` in GLSL ES 1.00.
    OesTexture3D,
    /// `sampler2DShadow` and `shadow2DEXT` in GLSL ES 1.00.
    ExtShadowSamplers,
}

impl Ext {
    pub const ALL: [Ext; 6] = [
        Ext::OesStandardDerivatives,
        Ext::ExtShaderTextureLod,
        Ext::ExtFragDepth,
        Ext::ExtDrawBuffers,
        Ext::OesTexture3D,
        Ext::ExtShadowSamplers,
    ];

    /// The extension's name, as in `#extension` and its macro.
    pub fn name(self) -> &'static str {
        match self {
            Ext::OesStandardDerivatives => "GL_OES_standard_derivatives",
            Ext::ExtShaderTextureLod => "GL_EXT_shader_texture_lod",
            Ext::ExtFragDepth => "GL_EXT_frag_depth",
            Ext::ExtDrawBuffers => "GL_EXT_draw_buffers",
            Ext::OesTexture3D => "GL_OES_texture_3D",
            Ext::ExtShadowSamplers => "GL_EXT_shadow_samplers",
        }
    }

    /// Whether the extension exists for shaders of `version` (all of these
    /// are core features of GLSL ES 3.00).
    pub fn applies_to(self, version: Version) -> bool {
        version == Version::V100
    }

    fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// A set of extensions.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ExtSet(u32);

impl ExtSet {
    pub const NONE: ExtSet = ExtSet(0);

    /// Every extension the compiler knows.
    pub fn all() -> ExtSet {
        let mut s = ExtSet::NONE;
        for e in Ext::ALL {
            s.insert(e);
        }
        s
    }

    pub fn contains(self, e: Ext) -> bool {
        self.0 & e.bit() != 0
    }

    pub fn insert(&mut self, e: Ext) {
        self.0 |= e.bit();
    }

    pub fn remove(&mut self, e: Ext) {
        self.0 &= !e.bit();
    }
}

/// The result of preprocessing.
#[derive(Debug)]
pub struct Preprocessed {
    pub version: Version,
    /// The shader's tokens: no newlines, locations as `#line` made them.
    pub tokens: Vec<PpTok>,
    /// Extensions the shader enabled (`enable`, `require` or `warn`).
    pub enabled: ExtSet,
    /// Extensions whose use should be warned about.
    pub warn: ExtSet,
    /// `#pragma STDGL invariant(all)` was given.
    pub invariant_all: bool,
}

/// The most tokens macro expansion may produce in one shader.
const MAX_EXPANDED_TOKENS: usize = 1 << 20;
/// The deepest nesting of macro arguments being expanded.
const MAX_ARG_DEPTH: usize = 64;

/// A macro definition.
#[derive(Clone, Debug)]
struct Macro {
    /// `None` for an object-like macro.
    params: Option<Vec<Symbol>>,
    body: Vec<PpTok>,
    /// Predefined by the implementation: may not be redefined or undefined.
    predefined: bool,
    /// `__LINE__` or `__FILE__`, whose value depends on where they are used.
    dynamic: Option<Dynamic>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dynamic {
    Line,
    File,
}

/// One `#if` group being processed.
#[derive(Clone, Copy, Debug)]
struct Cond {
    /// The enclosing group is being processed.
    parent_active: bool,
    /// This branch is being processed.
    active: bool,
    /// A branch of this `#if` has already been taken.
    taken: bool,
    /// `#else` was seen.
    in_else: bool,
    loc: Loc,
}

/// A macro expansion being rescanned.
struct Context {
    name: Symbol,
    tokens: Vec<PpTok>,
    pos: usize,
}

/// Where unexpanded tokens come from: the file (running text, which may
/// continue over newlines inside a macro invocation) or a fixed list (a
/// directive's expression, or a macro argument).
struct Input<'t> {
    tokens: &'t [PpTok],
    pos: usize,
    /// Running text: newlines inside macro invocations are skipped.
    multiline: bool,
}

/// Symbols the preprocessor compares against.
struct Names {
    defined: Symbol,
    es: Symbol,
    stdgl: Symbol,
    invariant: Symbol,
    all: Symbol,
}

/// Preprocesses a shader's source strings, with the extensions in
/// `supported` available.
pub fn preprocess(
    sources: &[&str],
    supported: ExtSet,
    interner: &mut Interner,
    diags: &mut Diagnostics,
) -> Preprocessed {
    let sniffed = sniff_version(sources);
    let version = match sniffed {
        Some(300) => Version::V300,
        _ => Version::V100,
    };
    let file = lex::tokenize(sources, version == Version::V300, interner, diags);
    let names = Names {
        defined: interner.intern("defined"),
        es: interner.intern("es"),
        stdgl: interner.intern("STDGL"),
        invariant: interner.intern("invariant"),
        all: interner.intern("all"),
    };
    let mut pp = Preprocessor {
        interner,
        diags,
        names,
        version,
        supported,
        macros: BTreeMap::new(),
        conds: Vec::new(),
        out: Vec::new(),
        expanded: 0,
        line_delta: 0,
        line_delta_string: 0,
        string_delta: 0,
        seen_code: false,
        seen_directive: false,
        enabled: ExtSet::NONE,
        warn: ExtSet::NONE,
        invariant_all: false,
        sniffed,
        overflow: false,
    };
    pp.predefine();
    pp.run(&file);
    Preprocessed { version, tokens: pp.out, enabled: pp.enabled, warn: pp.warn, invariant_all: pp.invariant_all }
}

/// Reads the `#version` number if the shader starts with one (after white
/// space and comments), before anything else is done with the source.
fn sniff_version(sources: &[&str]) -> Option<u32> {
    let mut text = String::new();
    for s in sources {
        let s = s.split('\0').next().unwrap_or("");
        text.push_str(s);
        if text.len() > 4096 {
            break;
        }
    }
    let b = text.as_bytes();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2 + text[i + 2..].find("*/")? + 2;
        } else {
            break;
        }
    }
    if i >= b.len() || b[i] != b'#' {
        return None;
    }
    i += 1;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    if !b[i..].starts_with(b"version") {
        return None;
    }
    i += 7;
    let start = {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        i
    };
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    text[start..i].parse().ok()
}

struct Preprocessor<'a> {
    interner: &'a mut Interner,
    diags: &'a mut Diagnostics,
    names: Names,
    version: Version,
    supported: ExtSet,
    macros: BTreeMap<Symbol, Macro>,
    conds: Vec<Cond>,
    out: Vec<PpTok>,
    /// Tokens produced by macro expansion so far.
    expanded: usize,
    /// `#line` adjustments: lines of the physical string
    /// `line_delta_string` are shifted by `line_delta`; every string number
    /// by `string_delta`.
    line_delta: i64,
    line_delta_string: u32,
    string_delta: i64,
    /// A token other than a directive has been seen.
    seen_code: bool,
    /// A directive has been seen.
    seen_directive: bool,
    enabled: ExtSet,
    warn: ExtSet,
    invariant_all: bool,
    sniffed: Option<u32>,
    /// Expansion grew too large; the rest of the shader is dropped.
    overflow: bool,
}

impl Preprocessor<'_> {
    fn predefine(&mut self) {
        let one = self.interner.intern("1");
        let version = self.interner.intern(match self.version {
            Version::V100 => "100",
            Version::V300 => "300",
        });
        let define = |pp: &mut Self, name: &str, value: Option<Symbol>, dynamic: Option<Dynamic>| {
            let sym = pp.interner.intern(name);
            let body = value.map(|v| vec![PpTok::new(PpKind::Number(v), Loc::NONE, false)]).unwrap_or_default();
            pp.macros.insert(sym, Macro { params: None, body, predefined: true, dynamic });
        };
        define(self, "__LINE__", None, Some(Dynamic::Line));
        define(self, "__FILE__", None, Some(Dynamic::File));
        define(self, "__VERSION__", Some(version), None);
        define(self, "GL_ES", Some(one), None);
        // highp is supported in fragment shaders; GLSL ES 3.00 defines the
        // macro unconditionally, 1.00 when highp is available.
        define(self, "GL_FRAGMENT_PRECISION_HIGH", Some(one), None);
        for e in Ext::ALL {
            if self.supported.contains(e) && e.applies_to(self.version) {
                define(self, e.name(), Some(one), None);
            }
        }
    }

    /// Maps a physical location through `#line`.
    fn map(&self, loc: Loc) -> Loc {
        let string = (loc.string as i64 + self.string_delta).clamp(0, u32::MAX as i64) as u32;
        let line = if loc.string == self.line_delta_string {
            (loc.line as i64 + self.line_delta).clamp(0, u32::MAX as i64) as u32
        } else {
            loc.line
        };
        Loc::new(string, line)
    }

    fn active(&self) -> bool {
        self.conds.last().is_none_or(|c| c.active)
    }

    fn run(&mut self, file: &[PpTok]) {
        let mut cursor = 0;
        let mut line_start = true;
        while cursor < file.len() && !self.overflow {
            let tok = file[cursor];
            match tok.kind {
                PpKind::Newline => {
                    cursor += 1;
                    line_start = true;
                }
                PpKind::Punct(Punct::Hash) if line_start => {
                    cursor += 1;
                    let start = cursor;
                    while file[cursor].kind != PpKind::Newline {
                        cursor += 1;
                    }
                    let newline = file[cursor];
                    self.directive(tok, &file[start..cursor], newline);
                    self.seen_directive = true;
                }
                _ if !self.active() => {
                    while file[cursor].kind != PpKind::Newline {
                        cursor += 1;
                    }
                }
                _ => {
                    line_start = false;
                    self.seen_code = true;
                    let mut input = Input { tokens: file, pos: cursor, multiline: true };
                    let mut line = Vec::new();
                    self.expand(&mut input, &mut Vec::new(), &mut line, 0);
                    // Every token carries its physical location (expanded
                    // tokens the invocation's); `#line` applies once, here.
                    for mut t in line {
                        t.loc = self.map(t.loc);
                        self.out.push(t);
                    }
                    cursor = input.pos;
                }
            }
        }
        if let Some(c) = self.conds.last() {
            // `c.loc` was mapped through `#line` when the #if was read.
            error!(self.diags, c.loc, "unterminated #if (missing #endif)");
        }
    }

    /// Spelling of a token, for messages and `#error`.
    fn spell(&self, t: &PpTok) -> String {
        match t.kind {
            PpKind::Ident(s) | PpKind::Number(s) => self.interner.get(s).into(),
            PpKind::Punct(p) => p.as_str().into(),
            PpKind::Invalid(c) => {
                let mut s = String::new();
                s.push(c);
                s
            }
            PpKind::Newline => "end of line".into(),
        }
    }

    fn directive(&mut self, hash: PpTok, toks: &[PpTok], newline: PpTok) {
        let loc = self.map(hash.loc);
        let Some(first) = toks.first() else {
            return; // the null directive
        };
        let name: String = match first.kind {
            PpKind::Ident(s) => self.interner.get(s).into(),
            _ => {
                if self.active() {
                    let t = self.spell(first);
                    error!(self.diags, loc, "invalid preprocessor directive '#{t}'");
                }
                return;
            }
        };
        let args = &toks[1..];
        match name.as_str() {
            "if" | "ifdef" | "ifndef" => {
                let parent_active = self.active();
                let value = if !parent_active {
                    false
                } else if name == "if" {
                    self.condition(args, loc)
                } else {
                    let defined = self.ifdef_name(args, loc, &name).is_some_and(|n| self.macros.contains_key(&n));
                    defined == (name == "ifdef")
                };
                self.conds.push(Cond {
                    parent_active,
                    active: parent_active && value,
                    taken: value,
                    in_else: false,
                    loc,
                });
            }
            "elif" => {
                let Some(c) = self.conds.last().copied() else {
                    error!(self.diags, loc, "#elif without #if");
                    return;
                };
                if c.in_else {
                    error!(self.diags, loc, "#elif after #else");
                }
                // The expression is evaluated only if no earlier branch was
                // taken (C++ and GLSL ES 3.00 agree).
                let value = c.parent_active && !c.taken && self.condition(args, loc);
                let top = self.conds.last_mut().unwrap();
                top.active = value;
                top.taken |= value;
            }
            "else" => {
                let Some(c) = self.conds.last().copied() else {
                    error!(self.diags, loc, "#else without #if");
                    return;
                };
                if c.in_else {
                    error!(self.diags, loc, "#else after #else");
                }
                if !args.is_empty() && c.parent_active {
                    error!(self.diags, loc, "unexpected tokens after #else");
                }
                let top = self.conds.last_mut().unwrap();
                top.active = c.parent_active && !c.taken;
                top.taken = true;
                top.in_else = true;
            }
            "endif" => {
                let Some(c) = self.conds.pop() else {
                    error!(self.diags, loc, "#endif without #if");
                    return;
                };
                if !args.is_empty() && c.parent_active {
                    error!(self.diags, loc, "unexpected tokens after #endif");
                }
            }
            _ if !self.active() => {}
            "define" => self.define(args, loc),
            "undef" => self.undef(args, loc),
            "error" => {
                let mut msg = String::new();
                for (i, t) in args.iter().enumerate() {
                    if i > 0 && t.space {
                        msg.push(' ');
                    }
                    msg.push_str(&self.spell(t));
                }
                error!(self.diags, loc, "#error {msg}");
            }
            "pragma" => self.pragma(args),
            "extension" => self.extension(args, loc),
            "version" => self.version_directive(args, loc),
            "line" => self.line(args, loc, newline),
            _ => error!(self.diags, loc, "invalid preprocessor directive '#{name}'"),
        }
    }

    /// The identifier after `#ifdef`, `#ifndef` or `#undef`.
    fn ifdef_name(&mut self, args: &[PpTok], loc: Loc, directive: &str) -> Option<Symbol> {
        match args {
            [t] => match t.kind {
                PpKind::Ident(s) => Some(s),
                _ => {
                    let d = String::from(directive);
                    error!(self.diags, loc, "#{d} needs a macro name");
                    None
                }
            },
            [] => {
                let d = String::from(directive);
                error!(self.diags, loc, "#{d} needs a macro name");
                None
            }
            [t, ..] => {
                let d = String::from(directive);
                error!(self.diags, loc, "unexpected tokens after #{d}");
                t.ident()
            }
        }
    }

    /// Evaluates an `#if` / `#elif` expression.
    fn condition(&mut self, args: &[PpTok], loc: Loc) -> bool {
        if args.is_empty() {
            error!(self.diags, loc, "#if with no expression");
            return false;
        }
        let resolved = self.resolve_defined(args, loc);
        let mut input = Input { tokens: &resolved, pos: 0, multiline: false };
        let mut tokens = Vec::new();
        self.expand(&mut input, &mut Vec::new(), &mut tokens, 0);
        let tokens = self.resolve_defined(&tokens, loc);
        match expr::evaluate(&tokens, self.interner, self.names.defined) {
            Ok(v) => v != 0,
            Err(e) => {
                let msg = e.message(self.interner);
                error!(self.diags, loc, "{msg}");
                false
            }
        }
    }

    /// Replaces `defined X` and `defined ( X )` by 1 or 0.
    fn resolve_defined(&mut self, args: &[PpTok], loc: Loc) -> Vec<PpTok> {
        let mut out = Vec::with_capacity(args.len());
        let one = self.interner.intern("1");
        let zero = self.interner.intern("0");
        let mut i = 0;
        while i < args.len() {
            let t = args[i];
            if t.kind != PpKind::Ident(self.names.defined) {
                out.push(t);
                i += 1;
                continue;
            }
            let (name, next) = match (args.get(i + 1), args.get(i + 2), args.get(i + 3)) {
                (Some(n), _, _) if n.ident().is_some() => (n.ident(), i + 2),
                (Some(l), Some(n), Some(r)) if l.is_punct(Punct::LParen) && r.is_punct(Punct::RParen) => {
                    (n.ident(), i + 4)
                }
                _ => (None, args.len()),
            };
            match name {
                Some(n) => {
                    let value = if self.macros.contains_key(&n) { one } else { zero };
                    out.push(PpTok::new(PpKind::Number(value), t.loc, t.space));
                }
                None => {
                    error!(self.diags, loc, "'defined' needs a macro name");
                    out.push(PpTok::new(PpKind::Number(zero), t.loc, t.space));
                }
            }
            i = next;
        }
        out
    }

    fn define(&mut self, args: &[PpTok], loc: Loc) {
        let Some(name_tok) = args.first() else {
            error!(self.diags, loc, "#define needs a macro name");
            return;
        };
        let Some(name) = name_tok.ident() else {
            let t = self.spell(name_tok);
            error!(self.diags, loc, "'{t}' is not a valid macro name");
            return;
        };
        let text = self.interner.get(name);
        if text.len() > 1024 {
            error!(self.diags, loc, "macro name longer than 1024 characters");
            return;
        }
        if name == self.names.defined {
            error!(self.diags, loc, "'defined' cannot be defined as a macro");
            return;
        }
        if text.starts_with("GL_") {
            let t = String::from(text);
            error!(self.diags, loc, "macro names beginning with 'GL_' are reserved: '{t}'");
            return;
        }
        if text.contains("__") {
            let t = String::from(text);
            warning!(self.diags, loc, "macro names containing '__' are reserved: '{t}'");
        }
        let mut rest = &args[1..];
        let params = match rest.first() {
            Some(t) if t.is_punct(Punct::LParen) && !t.space => {
                let mut params = Vec::new();
                let mut i = 1;
                loop {
                    match rest.get(i) {
                        Some(t) if t.is_punct(Punct::RParen) && params.is_empty() => {
                            i += 1;
                            break;
                        }
                        Some(t) if t.ident().is_some() => {
                            let p = t.ident().unwrap();
                            if params.contains(&p) {
                                let n = String::from(self.interner.get(p));
                                error!(self.diags, loc, "duplicate macro parameter '{n}'");
                                return;
                            }
                            params.push(p);
                            match rest.get(i + 1) {
                                Some(t) if t.is_punct(Punct::Comma) => i += 2,
                                Some(t) if t.is_punct(Punct::RParen) => {
                                    i += 2;
                                    break;
                                }
                                _ => {
                                    error!(self.diags, loc, "malformed macro parameter list");
                                    return;
                                }
                            }
                        }
                        _ => {
                            error!(self.diags, loc, "malformed macro parameter list");
                            return;
                        }
                    }
                }
                rest = &rest[i..];
                Some(params)
            }
            _ => None,
        };
        let mut body: Vec<PpTok> = rest.to_vec();
        if let Some(first) = body.first_mut() {
            first.space = false;
        }
        let new = Macro { params, body, predefined: false, dynamic: None };
        if let Some(old) = self.macros.get(&name) {
            let n = String::from(self.interner.get(name));
            if old.predefined {
                error!(self.diags, loc, "cannot redefine the predefined macro '{n}'");
                return;
            }
            if !same_definition(old, &new) {
                error!(self.diags, loc, "macro '{n}' redefined differently");
                return;
            }
        }
        self.macros.insert(name, new);
    }

    fn undef(&mut self, args: &[PpTok], loc: Loc) {
        let Some(name) = self.ifdef_name(args, loc, "undef") else { return };
        let text = self.interner.get(name);
        if text.starts_with("GL_") {
            let t = String::from(text);
            error!(self.diags, loc, "macro names beginning with 'GL_' are reserved: '{t}'");
            return;
        }
        if self.macros.get(&name).is_some_and(|m| m.predefined) {
            let t = String::from(text);
            error!(self.diags, loc, "cannot undefine the predefined macro '{t}'");
            return;
        }
        self.macros.remove(&name);
    }

    fn pragma(&mut self, args: &[PpTok]) {
        // STDGL invariant(all): every output is invariant.
        if let [a, b, l, c, r] = args
            && a.ident() == Some(self.names.stdgl)
            && b.ident() == Some(self.names.invariant)
            && l.is_punct(Punct::LParen)
            && c.ident() == Some(self.names.all)
            && r.is_punct(Punct::RParen)
        {
            self.invariant_all = true;
        }
    }

    fn extension(&mut self, args: &[PpTok], loc: Loc) {
        let (name, behavior) = match args {
            [n, c, b] if c.is_punct(Punct::Colon) => match (n.ident(), b.ident()) {
                (Some(n), Some(b)) => (n, b),
                _ => {
                    error!(self.diags, loc, "#extension must be '#extension name : behavior'");
                    return;
                }
            },
            _ => {
                error!(self.diags, loc, "#extension must be '#extension name : behavior'");
                return;
            }
        };
        if self.seen_code {
            // Both versions require it; many GLSL ES 1.00 shaders written
            // for WebGL ignore the rule, so it is only a warning there.
            match self.version {
                Version::V300 => {
                    error!(self.diags, loc, "#extension must come before any non-preprocessor tokens");
                }
                Version::V100 => {
                    warning!(self.diags, loc, "#extension should come before any non-preprocessor tokens");
                }
            }
        }
        #[derive(PartialEq)]
        enum B {
            Require,
            Enable,
            Warn,
            Disable,
        }
        let b = match self.interner.get(behavior) {
            "require" => B::Require,
            "enable" => B::Enable,
            "warn" => B::Warn,
            "disable" => B::Disable,
            other => {
                let o = String::from(other);
                error!(self.diags, loc, "unknown extension behavior '{o}'");
                return;
            }
        };
        let ext_name = String::from(self.interner.get(name));
        if name == self.names.all {
            match b {
                B::Require | B::Enable => {
                    error!(self.diags, loc, "'all' can only be used with 'warn' or 'disable'");
                }
                B::Warn => {
                    for e in Ext::ALL {
                        if self.supported.contains(e) && e.applies_to(self.version) {
                            self.enabled.insert(e);
                            self.warn.insert(e);
                        }
                    }
                }
                B::Disable => {
                    self.enabled = ExtSet::NONE;
                    self.warn = ExtSet::NONE;
                }
            }
            return;
        }
        let ext = Ext::ALL
            .into_iter()
            .find(|e| e.name() == ext_name && self.supported.contains(*e) && e.applies_to(self.version));
        let Some(ext) = ext else {
            if b == B::Require {
                error!(self.diags, loc, "extension '{ext_name}' is not supported");
            } else {
                warning!(self.diags, loc, "extension '{ext_name}' is not supported");
            }
            return;
        };
        match b {
            B::Require | B::Enable => {
                self.enabled.insert(ext);
                self.warn.remove(ext);
            }
            B::Warn => {
                self.enabled.insert(ext);
                self.warn.insert(ext);
            }
            B::Disable => {
                self.enabled.remove(ext);
                self.warn.remove(ext);
            }
        }
    }

    fn version_directive(&mut self, args: &[PpTok], loc: Loc) {
        if self.seen_code || self.seen_directive || self.sniffed.is_none() {
            error!(self.diags, loc, "#version must come before anything else in the shader");
            return;
        }
        let number = args.first().and_then(|t| match t.kind {
            PpKind::Number(s) => self.interner.get(s).parse::<u32>().ok(),
            _ => None,
        });
        let profile = args.get(1).and_then(|t| t.ident());
        match (number, profile, args.len()) {
            (Some(100), None, 1) => {}
            (Some(300), Some(p), 2) if p == self.names.es => {}
            (Some(300), _, _) => error!(self.diags, loc, "#version 300 must be '#version 300 es'"),
            (Some(n), _, _) if n != 100 => error!(self.diags, loc, "GLSL ES version {n} is not supported"),
            _ => error!(self.diags, loc, "malformed #version directive"),
        }
    }

    fn line(&mut self, args: &[PpTok], loc: Loc, newline: PpTok) {
        let mut input = Input { tokens: args, pos: 0, multiline: false };
        let mut tokens = Vec::new();
        self.expand(&mut input, &mut Vec::new(), &mut tokens, 0);
        let parsed = expr::evaluate_line(&tokens, self.interner, self.names.defined);
        match parsed {
            Ok((line, string)) => {
                if !(0..=i64::from(u32::MAX)).contains(&line)
                    || string.is_some_and(|s| !(0..=i64::from(u32::MAX)).contains(&s))
                {
                    error!(self.diags, loc, "#line number out of range");
                    return;
                }
                // The line after the directive gets number `line`.
                let next_physical = newline.loc.line as i64 + 1;
                self.line_delta = line - next_physical;
                self.line_delta_string = newline.loc.string;
                if let Some(s) = string {
                    self.string_delta = s - newline.loc.string as i64;
                }
            }
            Err(e) => {
                let msg = e.message(self.interner);
                error!(self.diags, loc, "#line: {msg}");
            }
        }
    }

    /// Takes the next token: from the innermost expansion with tokens left
    /// (finished expansions are dropped, which re-enables their macros), or
    /// from `input`. Running text stops at a newline unless inside a macro
    /// invocation (`in_args`), where newlines are skipped.
    fn next(&mut self, input: &mut Input<'_>, stack: &mut Vec<Context>, in_args: bool) -> Option<PpTok> {
        while let Some(ctx) = stack.last_mut() {
            if ctx.pos < ctx.tokens.len() {
                ctx.pos += 1;
                return Some(ctx.tokens[ctx.pos - 1]);
            }
            stack.pop();
        }
        loop {
            let t = *input.tokens.get(input.pos)?;
            if t.kind == PpKind::Newline {
                if !(input.multiline && in_args) {
                    return None;
                }
                input.pos += 1;
                // A directive inside a macro invocation's arguments.
                if input.tokens.get(input.pos).is_some_and(|t| t.is_punct(Punct::Hash)) {
                    let loc = self.map(t.loc);
                    error!(self.diags, loc, "preprocessor directive inside macro arguments");
                    return None;
                }
                continue;
            }
            input.pos += 1;
            return Some(t);
        }
    }

    /// Whether the next token (without taking it) is `(`.
    fn next_is_lparen(&self, input: &Input<'_>, stack: &[Context]) -> bool {
        for ctx in stack.iter().rev() {
            if ctx.pos < ctx.tokens.len() {
                return ctx.tokens[ctx.pos].is_punct(Punct::LParen);
            }
        }
        let mut i = input.pos;
        while let Some(t) = input.tokens.get(i) {
            if t.kind != PpKind::Newline {
                return t.is_punct(Punct::LParen);
            }
            if !input.multiline {
                return false;
            }
            i += 1;
            // A directive line ends the search.
            if input.tokens.get(i).is_some_and(|t| t.is_punct(Punct::Hash)) {
                return false;
            }
        }
        false
    }

    /// Expands tokens from `input` (and `stack`) into `out`: running text
    /// up to the end of the line (or across lines inside a macro
    /// invocation), or all of a fixed list. Tokens keep their physical
    /// locations; tokens a macro produced take its invocation's.
    fn expand(&mut self, input: &mut Input<'_>, stack: &mut Vec<Context>, out: &mut Vec<PpTok>, depth: usize) {
        self.expand_with(input, stack, out, depth, &[]);
    }

    /// [`Self::expand`], with `outer` naming the macros being expanded
    /// around this expansion (when expanding a macro argument), which are
    /// disabled as the ones on `stack` are.
    fn expand_with(
        &mut self,
        input: &mut Input<'_>,
        stack: &mut Vec<Context>,
        out: &mut Vec<PpTok>,
        depth: usize,
        outer: &[Symbol],
    ) {
        while let Some(mut tok) = self.next(input, stack, false) {
            if self.overflow {
                return;
            }
            let name = match tok.kind {
                PpKind::Ident(name) if !tok.noexpand => name,
                _ => {
                    out.push(tok);
                    continue;
                }
            };
            let Some(mac) = self.macros.get(&name) else {
                out.push(tok);
                continue;
            };
            if outer.contains(&name) || stack.iter().any(|c| c.name == name) {
                // Met while its own expansion is rescanned: painted.
                tok.noexpand = true;
                out.push(tok);
                continue;
            }
            let loc = tok.loc;
            if let Some(d) = mac.dynamic {
                let value = match d {
                    Dynamic::Line => self.map(loc).line,
                    Dynamic::File => self.map(loc).string,
                };
                let mut s = String::new();
                let _ = write!(s, "{value}");
                let sym = self.interner.intern(&s);
                out.push(PpTok { kind: PpKind::Number(sym), loc, space: tok.space, noexpand: false });
                continue;
            }
            let replacement = match &mac.params {
                None => {
                    let mut body = mac.body.clone();
                    for t in body.iter_mut() {
                        t.loc = loc;
                    }
                    if let Some(first) = body.first_mut() {
                        first.space = tok.space;
                    }
                    body
                }
                Some(params) => {
                    if !self.next_is_lparen(input, stack) {
                        // A function-like macro's name without arguments is
                        // just an identifier.
                        out.push(tok);
                        continue;
                    }
                    let params = params.clone();
                    let body = mac.body.clone();
                    let Some(args) = self.collect_args(input, stack, loc) else { return };
                    if !(args.len() == params.len() || (params.is_empty() && args.len() == 1 && args[0].is_empty())) {
                        let n = String::from(self.interner.get(name));
                        let at = self.map(loc);
                        error!(self.diags, at, "macro '{n}' takes {} argument(s), {} given", params.len(), args.len());
                        continue;
                    }
                    if depth >= MAX_ARG_DEPTH {
                        let at = self.map(loc);
                        error!(self.diags, at, "macro arguments nested too deeply");
                        self.overflow = true;
                        return;
                    }
                    // Each argument is fully expanded on its own first, with
                    // the macros being expanded around it still disabled.
                    let mut disabled: Vec<Symbol> = outer.to_vec();
                    disabled.extend(stack.iter().map(|c| c.name));
                    let mut expanded_args = Vec::with_capacity(args.len());
                    for arg in &args {
                        let mut arg_input = Input { tokens: arg, pos: 0, multiline: false };
                        let mut arg_out = Vec::new();
                        self.expand_with(&mut arg_input, &mut Vec::new(), &mut arg_out, depth + 1, &disabled);
                        if self.overflow {
                            return;
                        }
                        expanded_args.push(arg_out);
                    }
                    let mut result = Vec::with_capacity(body.len());
                    for t in &body {
                        match t.kind {
                            PpKind::Ident(p) if params.contains(&p) => {
                                let i = params.iter().position(|&q| q == p).unwrap_or(0);
                                for (k, a) in expanded_args[i].iter().enumerate() {
                                    let mut a = *a;
                                    a.loc = loc;
                                    if k == 0 {
                                        a.space = t.space;
                                    }
                                    result.push(a);
                                }
                            }
                            _ => {
                                let mut t = *t;
                                t.loc = loc;
                                result.push(t);
                            }
                        }
                    }
                    if let Some(first) = result.first_mut() {
                        first.space = tok.space;
                    }
                    result
                }
            };
            self.expanded += replacement.len();
            if self.expanded > MAX_EXPANDED_TOKENS {
                let at = self.map(loc);
                error!(self.diags, at, "macro expansion produces too many tokens");
                self.overflow = true;
                return;
            }
            stack.push(Context { name, tokens: replacement, pos: 0 });
        }
    }

    /// Collects a function-like macro invocation's arguments, from its `(`
    /// to the matching `)`.
    fn collect_args(&mut self, input: &mut Input<'_>, stack: &mut Vec<Context>, loc: Loc) -> Option<Vec<Vec<PpTok>>> {
        let open = self.next(input, stack, true);
        debug_assert!(open.is_some_and(|t| t.is_punct(Punct::LParen)));
        let mut args: Vec<Vec<PpTok>> = vec![Vec::new()];
        let mut depth = 1usize;
        let mut total = 0usize;
        loop {
            let Some(t) = self.next(input, stack, true) else {
                let at = self.map(loc);
                error!(self.diags, at, "unterminated macro invocation");
                return None;
            };
            match t.kind {
                PpKind::Punct(Punct::LParen) => depth += 1,
                PpKind::Punct(Punct::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(args);
                    }
                }
                PpKind::Punct(Punct::Comma) if depth == 1 => {
                    args.push(Vec::new());
                    continue;
                }
                _ => {}
            }
            if let Some(arg) = args.last_mut() {
                arg.push(t);
            }
            total += 1;
            if total > MAX_EXPANDED_TOKENS {
                let at = self.map(loc);
                error!(self.diags, at, "macro arguments too long");
                self.overflow = true;
                return None;
            }
        }
    }
}

/// Whether two macro definitions are the same (C's rule: same parameters,
/// same replacement tokens with the same white space between them).
fn same_definition(a: &Macro, b: &Macro) -> bool {
    a.params == b.params
        && a.body.len() == b.body.len()
        && a.body.iter().zip(&b.body).enumerate().all(|(i, (x, y))| x.kind == y.kind && (i == 0 || x.space == y.space))
}
