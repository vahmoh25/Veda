//! The parser: GLSL ES 1.00 and 3.00 grammar (chapter 9), by recursive
//! descent, with binary operators by precedence climbing.
//!
//! Whether a statement is a declaration is decided syntactically: it starts
//! with a qualifier, a type keyword or `struct`, or with a name followed by
//! a name (`S s;`) or by an array size and a name (`S[2] s;`). Structure
//! names used as constructors parse as calls and are resolved by the
//! semantic checks.
//!
//! On a syntax error the parser reports it, skips to the end of the
//! statement or declaration and goes on, so that one compilation finds
//! several errors. Nesting is bounded: a hostile shader cannot exhaust the
//! stack.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::ast::*;
use crate::diag::{Diagnostics, Loc, error};
use crate::intern::{Interner, Symbol};
use crate::lex::{Kw, Tk, Token};
use crate::pp::Punct;
use crate::types::Precision;

/// The deepest nesting of expressions or statements accepted.
pub const MAX_NESTING: usize = 128;

/// Parses a shader's tokens (ending with [`Tk::Eof`]).
pub fn parse(tokens: &[Token], interner: &Interner, diags: &mut Diagnostics) -> TranslationUnit {
    let length = interner.lookup("length");
    let mut p = Parser { toks: tokens, pos: 0, diags, interner, length, depth: 0, too_deep: false };
    let mut unit = TranslationUnit::default();
    while !p.at_eof() {
        if p.diags.saturated() {
            break;
        }
        let start = p.pos;
        match p.external() {
            Ok(item) => unit.items.push(item),
            Err(Stop) => {
                p.recover_external();
                if p.pos == start {
                    p.pos += 1;
                }
            }
        }
    }
    unit
}

/// A syntax error was reported; the caller recovers.
#[derive(Debug)]
struct Stop;

type PResult<T> = Result<T, Stop>;

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    diags: &'a mut Diagnostics,
    interner: &'a Interner,
    /// The symbol `length`, if the shader uses the word at all.
    length: Option<Symbol>,
    depth: usize,
    /// Nesting went past [`MAX_NESTING`]: the current function is dropped.
    too_deep: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Tk {
        self.toks[self.pos.min(self.toks.len() - 1)].tk
    }

    fn peek_at(&self, ahead: usize) -> Tk {
        self.toks[(self.pos + ahead).min(self.toks.len() - 1)].tk
    }

    fn loc(&self) -> Loc {
        self.toks[self.pos.min(self.toks.len() - 1)].loc
    }

    fn at_eof(&self) -> bool {
        self.peek() == Tk::Eof
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)];
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is_punct(&self, p: Punct) -> bool {
        self.peek() == Tk::Punct(p)
    }

    fn eat_punct(&mut self, p: Punct) -> bool {
        if self.is_punct(p) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn is_kw(&self, k: Kw) -> bool {
        self.peek() == Tk::Kw(k)
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Describes the current token for messages.
    fn describe(&self, tk: Tk) -> alloc::string::String {
        match tk {
            Tk::Ident(s) => alloc::format!("'{}'", self.interner.get(s)),
            Tk::Int(v) => alloc::format!("'{}'", v as i32),
            Tk::Uint(v) => alloc::format!("'{v}u'"),
            Tk::Float(v) => alloc::format!("'{v}'"),
            Tk::Bool(b) => alloc::format!("'{b}'"),
            Tk::Kw(k) => alloc::format!("'{}'", k.name()),
            Tk::Type(b) => alloc::format!("'{}'", b.name()),
            Tk::Punct(p) => alloc::format!("'{}'", p.as_str()),
            Tk::Eof => "end of shader".into(),
        }
    }

    fn unexpected<T>(&mut self, wanted: &str) -> PResult<T> {
        let loc = self.loc();
        let found = self.describe(self.peek());
        error!(self.diags, loc, "syntax error: expected {wanted}, found {found}");
        Err(Stop)
    }

    fn expect_punct(&mut self, p: Punct) -> PResult<Loc> {
        if self.is_punct(p) { Ok(self.bump().loc) } else { self.unexpected(&alloc::format!("'{}'", p.as_str())) }
    }

    fn expect_ident(&mut self) -> PResult<(Symbol, Loc)> {
        match self.peek() {
            Tk::Ident(s) => {
                let t = self.bump();
                Ok((s, t.loc))
            }
            _ => self.unexpected("an identifier"),
        }
    }

    /// Enters one level of nesting; every successful `enter` is paired with
    /// a [`Self::leave`].
    fn enter(&mut self) -> PResult<()> {
        if self.depth >= MAX_NESTING {
            if !self.too_deep {
                let loc = self.loc();
                error!(self.diags, loc, "the shader nests expressions or statements too deeply");
            }
            self.too_deep = true;
            return Err(Stop);
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Skips to the end of a file-scope declaration after an error.
    fn recover_external(&mut self) {
        self.depth = 0;
        self.too_deep = false;
        let mut braces = 0usize;
        while !self.at_eof() {
            match self.bump().tk {
                Tk::Punct(Punct::LBrace) => braces += 1,
                Tk::Punct(Punct::RBrace) => {
                    braces = braces.saturating_sub(1);
                    if braces == 0 {
                        // `struct {...} s;` continues after the brace.
                        if !self.is_punct(Punct::Semicolon) {
                            return;
                        }
                    }
                }
                Tk::Punct(Punct::Semicolon) if braces == 0 => return,
                _ => {}
            }
        }
    }

    /// Skips to the end of a statement after an error: past a `;` at this
    /// level, or up to the `}` that closes the block.
    fn recover_statement(&mut self) {
        let mut braces = 0usize;
        while !self.at_eof() {
            match self.peek() {
                Tk::Punct(Punct::LBrace) => braces += 1,
                Tk::Punct(Punct::RBrace) => {
                    if braces == 0 {
                        return;
                    }
                    braces -= 1;
                }
                Tk::Punct(Punct::Semicolon) if braces == 0 => {
                    self.bump();
                    return;
                }
                _ => {}
            }
            self.bump();
        }
    }

    // ---- Declarations ----------------------------------------------------

    fn external(&mut self) -> PResult<External> {
        if self.is_kw(Kw::Precision) || self.at_invariant_list() {
            return Ok(External::Declaration(self.declaration()?));
        }
        // A function: [qualifiers] type name '(' ...
        let loc = self.loc();
        let qualifiers = self.qualifiers()?;
        if self.starts_type()
            && let Some(after) = self.type_end()
            && matches!(self.toks.get(after).map(|t| t.tk), Some(Tk::Ident(_)))
            && matches!(self.toks.get(after + 1).map(|t| t.tk), Some(Tk::Punct(Punct::LParen)))
        {
            let ty = self.type_spec()?;
            let (name, _) = self.expect_ident()?;
            let proto = self.prototype_rest(FullType { qualifiers, ty }, name, loc)?;
            if self.is_punct(Punct::LBrace) {
                let body = self.compound_body()?;
                let end = self.toks[self.pos.saturating_sub(1)].loc;
                return Ok(External::Function(FunctionDef { proto, body, end }));
            }
            self.expect_punct(Punct::Semicolon)?;
            return Ok(External::Declaration(Declaration::Prototype(proto)));
        }
        Ok(External::Declaration(self.declaration_rest(qualifiers, loc)?))
    }

    /// Whether the tokens are `invariant name ;` or `invariant name ,`.
    fn at_invariant_list(&self) -> bool {
        self.is_kw(Kw::Invariant)
            && matches!(self.peek_at(1), Tk::Ident(_))
            && matches!(self.peek_at(2), Tk::Punct(Punct::Semicolon | Punct::Comma))
    }

    /// Whether the current token can start a declaration (rather than an
    /// expression statement).
    fn starts_declaration(&self) -> bool {
        match self.peek() {
            Tk::Kw(
                Kw::Const
                | Kw::Uniform
                | Kw::Attribute
                | Kw::Varying
                | Kw::In
                | Kw::Out
                | Kw::Inout
                | Kw::Centroid
                | Kw::Flat
                | Kw::Smooth
                | Kw::Invariant
                | Kw::Layout
                | Kw::Lowp
                | Kw::Mediump
                | Kw::Highp
                | Kw::Precision
                | Kw::Struct,
            ) => true,
            // `vec4(1.0).x;` and `float[2](a, b);` are expressions.
            Tk::Type(_) => self
                .type_end()
                .is_some_and(|i| !matches!(self.toks.get(i).map(|t| t.tk), Some(Tk::Punct(Punct::LParen)))),
            Tk::Ident(_) => match self.peek_at(1) {
                Tk::Ident(_) => true,
                // `S[2] s;` or `S[] s = ...;`
                Tk::Punct(Punct::LBracket) => self.array_type_then_name(self.pos + 1),
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether the tokens from `i` (a `[`) are a bracketed size followed by
    /// a name: `[...] name`.
    fn array_type_then_name(&self, i: usize) -> bool {
        let mut depth = 0usize;
        let mut j = i;
        while let Some(t) = self.toks.get(j) {
            match t.tk {
                Tk::Punct(Punct::LBracket) => depth += 1,
                Tk::Punct(Punct::RBracket) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.toks.get(j + 1).map(|t| t.tk), Some(Tk::Ident(_)));
                    }
                }
                Tk::Punct(Punct::Semicolon | Punct::LBrace | Punct::RBrace) | Tk::Eof => return false,
                _ => {}
            }
            j += 1;
        }
        false
    }

    /// Whether the current token starts a type specifier.
    fn starts_type(&self) -> bool {
        matches!(self.peek(), Tk::Type(_) | Tk::Kw(Kw::Struct) | Tk::Ident(_))
    }

    /// Index just past the type specifier starting at the current token
    /// (without parsing it), if it can be found.
    fn type_end(&self) -> Option<usize> {
        let mut i = self.pos;
        match self.toks.get(i)?.tk {
            Tk::Type(_) | Tk::Ident(_) => i += 1,
            Tk::Kw(Kw::Struct) => {
                // struct [name] { ... }
                while self.toks.get(i)?.tk != Tk::Punct(Punct::LBrace) {
                    i += 1;
                }
                let mut depth = 0usize;
                loop {
                    match self.toks.get(i)?.tk {
                        Tk::Punct(Punct::LBrace) => depth += 1,
                        Tk::Punct(Punct::RBrace) => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        Tk::Eof => return None,
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ => return None,
        }
        if self.toks.get(i)?.tk == Tk::Punct(Punct::LBracket) {
            let mut depth = 0usize;
            loop {
                match self.toks.get(i)?.tk {
                    Tk::Punct(Punct::LBracket) => depth += 1,
                    Tk::Punct(Punct::RBracket) => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    Tk::Eof | Tk::Punct(Punct::Semicolon) => return None,
                    _ => {}
                }
                i += 1;
            }
        }
        Some(i)
    }

    /// Qualifiers, in any order (the semantic checks enforce the order).
    fn qualifiers(&mut self) -> PResult<Qualifiers> {
        let mut q = Qualifiers::default();
        loop {
            let loc = self.loc();
            let kind = match self.peek() {
                Tk::Kw(Kw::Layout) => {
                    self.bump();
                    self.expect_punct(Punct::LParen)?;
                    loop {
                        let (name, loc) = self.expect_ident()?;
                        let value = if self.eat_punct(Punct::Assign) {
                            match self.bump().tk {
                                Tk::Int(v) => Some(i64::from(v as i32)),
                                Tk::Uint(v) => Some(i64::from(v)),
                                _ => {
                                    self.pos -= 1;
                                    return self.unexpected("an integer constant");
                                }
                            }
                        } else {
                            None
                        };
                        q.layout.push(LayoutId { name, value, loc });
                        if !self.eat_punct(Punct::Comma) {
                            break;
                        }
                    }
                    self.expect_punct(Punct::RParen)?;
                    QualKind::Layout
                }
                Tk::Kw(Kw::Invariant) => {
                    self.bump();
                    if q.invariant {
                        error!(self.diags, loc, "duplicate 'invariant' qualifier");
                    }
                    q.invariant = true;
                    QualKind::Invariant
                }
                Tk::Kw(k @ (Kw::Smooth | Kw::Flat)) => {
                    self.bump();
                    if q.interp.is_some() {
                        error!(self.diags, loc, "more than one interpolation qualifier");
                    }
                    q.interp = Some(if k == Kw::Smooth { Interp::Smooth } else { Interp::Flat });
                    QualKind::Interp
                }
                Tk::Kw(Kw::Centroid) => {
                    self.bump();
                    if q.centroid {
                        error!(self.diags, loc, "duplicate 'centroid' qualifier");
                    }
                    q.centroid = true;
                    QualKind::Centroid
                }
                Tk::Kw(k @ (Kw::Const | Kw::Attribute | Kw::Varying | Kw::Uniform | Kw::In | Kw::Out | Kw::Inout)) => {
                    self.bump();
                    let s = match k {
                        Kw::Const => Storage::Const,
                        Kw::Attribute => Storage::Attribute,
                        Kw::Varying => Storage::Varying,
                        Kw::Uniform => Storage::Uniform,
                        Kw::In => Storage::In,
                        Kw::Out => Storage::Out,
                        _ => Storage::InOut,
                    };
                    let direction = matches!(s, Storage::In | Storage::Out | Storage::InOut);
                    match (q.storage, s) {
                        (None, _) => q.storage = Some(s),
                        // A parameter's `const in` (the checks reject it
                        // elsewhere): keep the direction.
                        (Some(Storage::Const), _) if direction => q.storage = Some(s),
                        (Some(Storage::In | Storage::Out | Storage::InOut), Storage::Const) => {}
                        _ => error!(self.diags, loc, "more than one storage qualifier"),
                    }
                    if s == Storage::Const {
                        if q.constant {
                            error!(self.diags, loc, "duplicate 'const' qualifier");
                        }
                        q.constant = true;
                    }
                    QualKind::Storage
                }
                Tk::Kw(k @ (Kw::Lowp | Kw::Mediump | Kw::Highp)) => {
                    self.bump();
                    if q.precision.is_some() {
                        error!(self.diags, loc, "more than one precision qualifier");
                    }
                    q.precision = Some(precision_of(k));
                    QualKind::Precision
                }
                _ => return Ok(q),
            };
            q.order.push((kind, loc));
        }
    }

    /// A type specifier: a basic type, a structure name or definition, then
    /// an optional array size.
    fn type_spec(&mut self) -> PResult<TypeSpec> {
        let loc = self.loc();
        let name = match self.peek() {
            Tk::Type(b) => {
                self.bump();
                TypeName::Basic(b)
            }
            Tk::Ident(s) => {
                self.bump();
                TypeName::Named(s)
            }
            Tk::Kw(Kw::Struct) => TypeName::Struct(Box::new(self.struct_spec()?)),
            _ => return self.unexpected("a type"),
        };
        let array = self.array_size_opt()?;
        Ok(TypeSpec { name, array, loc })
    }

    /// `[N]` or `[]`, if present.
    fn array_size_opt(&mut self) -> PResult<Option<ArraySize>> {
        if !self.eat_punct(Punct::LBracket) {
            return Ok(None);
        }
        if self.eat_punct(Punct::RBracket) {
            return Ok(Some(ArraySize::Unsized));
        }
        let size = self.conditional()?;
        self.expect_punct(Punct::RBracket)?;
        if self.is_punct(Punct::LBracket) {
            let loc = self.loc();
            error!(self.diags, loc, "arrays of arrays are not supported in GLSL ES 1.00 and 3.00");
            return Err(Stop);
        }
        Ok(Some(ArraySize::Sized(Box::new(size))))
    }

    fn struct_spec(&mut self) -> PResult<StructSpec> {
        let loc = self.bump().loc; // struct
        let name = match self.peek() {
            Tk::Ident(s) => {
                let t = self.bump();
                Some((s, t.loc))
            }
            _ => None,
        };
        self.expect_punct(Punct::LBrace)?;
        let members = self.member_list()?;
        Ok(StructSpec { name, members, loc })
    }

    /// Member declarations up to and including the closing brace.
    fn member_list(&mut self) -> PResult<Vec<MemberDecl>> {
        self.enter()?;
        let r = self.member_list_inner();
        self.leave();
        r
    }

    fn member_list_inner(&mut self) -> PResult<Vec<MemberDecl>> {
        let mut members = Vec::new();
        while !self.eat_punct(Punct::RBrace) {
            if self.at_eof() {
                return self.unexpected("'}'");
            }
            let loc = self.loc();
            let qualifiers = self.qualifiers()?;
            let ty = self.type_spec()?;
            let mut names = Vec::new();
            loop {
                let (name, loc) = self.expect_ident()?;
                let array = self.array_size_opt()?;
                names.push(MemberName { name, array, loc });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::Semicolon)?;
            members.push(MemberDecl { qualifiers, ty, names, loc });
        }
        Ok(members)
    }

    /// A declaration, ending with its semicolon.
    fn declaration(&mut self) -> PResult<Declaration> {
        let loc = self.loc();
        // precision <precision> <type>;
        if self.eat_kw(Kw::Precision) {
            let precision = match self.peek() {
                Tk::Kw(k @ (Kw::Lowp | Kw::Mediump | Kw::Highp)) => {
                    self.bump();
                    precision_of(k)
                }
                _ => return self.unexpected("a precision qualifier"),
            };
            let ty = self.type_spec()?;
            self.expect_punct(Punct::Semicolon)?;
            return Ok(Declaration::Precision { precision, ty, loc });
        }
        // invariant a, b;
        if self.at_invariant_list() {
            self.bump();
            let mut names = Vec::new();
            loop {
                names.push(self.expect_ident()?);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_punct(Punct::Semicolon)?;
            return Ok(Declaration::Invariant { names, loc });
        }
        let qualifiers = self.qualifiers()?;
        self.declaration_rest(qualifiers, loc)
    }

    /// The rest of a declaration, after its qualifiers.
    fn declaration_rest(&mut self, qualifiers: Qualifiers, loc: Loc) -> PResult<Declaration> {
        // layout(std140) uniform;
        if !qualifiers.is_empty() && self.is_punct(Punct::Semicolon) {
            self.bump();
            return Ok(Declaration::Defaults { qualifiers, loc });
        }
        // uniform Block { ... } instance;
        if !qualifiers.is_empty()
            && matches!(self.peek(), Tk::Ident(_))
            && matches!(self.peek_at(1), Tk::Punct(Punct::LBrace))
        {
            let (name, _) = self.expect_ident()?;
            self.bump(); // {
            let members = self.member_list()?;
            let instance = match self.peek() {
                Tk::Ident(s) => {
                    let t = self.bump();
                    let array = self.array_size_opt()?;
                    Some((s, array, t.loc))
                }
                _ => None,
            };
            self.expect_punct(Punct::Semicolon)?;
            return Ok(Declaration::Block(Block { qualifiers, name, members, instance, loc }));
        }
        let ty = self.type_spec()?;
        let full = FullType { qualifiers, ty };
        let mut declarators = Vec::new();
        if self.eat_punct(Punct::Semicolon) {
            return Ok(Declaration::Variables { ty: full, declarators, loc });
        }
        // A prototype inside a function body.
        if matches!(self.peek(), Tk::Ident(_)) && matches!(self.peek_at(1), Tk::Punct(Punct::LParen)) {
            let (name, _) = self.expect_ident()?;
            let proto = self.prototype_rest(full, name, loc)?;
            self.expect_punct(Punct::Semicolon)?;
            return Ok(Declaration::Prototype(proto));
        }
        loop {
            let (name, dloc) = self.expect_ident()?;
            let array = self.array_size_opt()?;
            let init = if self.eat_punct(Punct::Assign) { Some(self.assignment()?) } else { None };
            declarators.push(Declarator { name, array, init, loc: dloc });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::Semicolon)?;
        Ok(Declaration::Variables { ty: full, declarators, loc })
    }

    /// A prototype's parameter list, from its `(` to its `)`.
    fn prototype_rest(&mut self, ret: FullType, name: Symbol, loc: Loc) -> PResult<Prototype> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        // f() and f(void)
        if self.is_punct(Punct::RParen) {
            self.bump();
            return Ok(Prototype { ret, name, params, loc });
        }
        if self.peek() == Tk::Type(crate::types::Basic::Void) && self.peek_at(1) == Tk::Punct(Punct::RParen) {
            self.bump();
            self.bump();
            return Ok(Prototype { ret, name, params, loc });
        }
        loop {
            let ploc = self.loc();
            let qualifiers = self.qualifiers()?;
            let ty = self.type_spec()?;
            let (pname, array) = match self.peek() {
                Tk::Ident(s) => {
                    self.bump();
                    (Some(s), self.array_size_opt()?)
                }
                _ => (None, None),
            };
            params.push(Param { qualifiers, ty, name: pname, array, loc: ploc });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(Prototype { ret, name, params, loc })
    }

    // ---- Statements ------------------------------------------------------

    /// `{ statements }`, the braces included.
    fn compound_body(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_punct(Punct::LBrace)?;
        self.enter()?;
        let mut body = Vec::new();
        loop {
            if self.eat_punct(Punct::RBrace) {
                break;
            }
            if self.at_eof() {
                self.leave();
                return self.unexpected("'}'");
            }
            if self.diags.saturated() {
                self.leave();
                return Err(Stop);
            }
            let start = self.pos;
            match self.statement() {
                Ok(s) => body.push(s),
                Err(Stop) => {
                    // Too deep: give up on the whole function.
                    if self.too_deep {
                        self.leave();
                        return Err(Stop);
                    }
                    self.recover_statement();
                    if self.pos == start {
                        self.bump();
                    }
                }
            }
        }
        self.leave();
        Ok(body)
    }

    fn statement(&mut self) -> PResult<Stmt> {
        self.enter()?;
        let r = self.statement_inner();
        self.leave();
        r
    }

    fn statement_inner(&mut self) -> PResult<Stmt> {
        let loc = self.loc();
        let kind = match self.peek() {
            Tk::Punct(Punct::LBrace) => StmtKind::Compound(self.compound_body()?),
            Tk::Punct(Punct::Semicolon) => {
                self.bump();
                StmtKind::Empty
            }
            Tk::Kw(Kw::If) => {
                self.bump();
                self.expect_punct(Punct::LParen)?;
                let cond = self.expression()?;
                self.expect_punct(Punct::RParen)?;
                let then = Box::new(self.statement()?);
                let otherwise = if self.eat_kw(Kw::Else) { Some(Box::new(self.statement()?)) } else { None };
                StmtKind::If { cond, then, otherwise }
            }
            Tk::Kw(Kw::Switch) => {
                self.bump();
                self.expect_punct(Punct::LParen)?;
                let selector = self.expression()?;
                self.expect_punct(Punct::RParen)?;
                let body = self.compound_body()?;
                StmtKind::Switch { selector, body }
            }
            Tk::Kw(Kw::Case) => {
                self.bump();
                let e = self.expression()?;
                self.expect_punct(Punct::Colon)?;
                StmtKind::Case(e)
            }
            Tk::Kw(Kw::Default) => {
                self.bump();
                self.expect_punct(Punct::Colon)?;
                StmtKind::Default
            }
            Tk::Kw(Kw::While) => {
                self.bump();
                self.expect_punct(Punct::LParen)?;
                let cond = self.condition()?;
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                StmtKind::While { cond, body }
            }
            Tk::Kw(Kw::Do) => {
                self.bump();
                let body = Box::new(self.statement()?);
                if !self.eat_kw(Kw::While) {
                    return self.unexpected("'while'");
                }
                self.expect_punct(Punct::LParen)?;
                let cond = self.expression()?;
                self.expect_punct(Punct::RParen)?;
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::DoWhile { body, cond }
            }
            Tk::Kw(Kw::For) => {
                self.bump();
                self.expect_punct(Punct::LParen)?;
                let init = if self.eat_punct(Punct::Semicolon) {
                    None
                } else if self.starts_declaration() {
                    let dloc = self.loc();
                    let d = self.declaration()?;
                    Some(Box::new(Stmt { kind: StmtKind::Declaration(d), loc: dloc }))
                } else {
                    let eloc = self.loc();
                    let e = self.expression()?;
                    self.expect_punct(Punct::Semicolon)?;
                    Some(Box::new(Stmt { kind: StmtKind::Expr(e), loc: eloc }))
                };
                let cond = if self.is_punct(Punct::Semicolon) { None } else { Some(self.condition()?) };
                self.expect_punct(Punct::Semicolon)?;
                let step = if self.is_punct(Punct::RParen) { None } else { Some(self.expression()?) };
                self.expect_punct(Punct::RParen)?;
                let body = Box::new(self.statement()?);
                StmtKind::For { init, cond, step, body }
            }
            Tk::Kw(Kw::Continue) => {
                self.bump();
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::Continue
            }
            Tk::Kw(Kw::Break) => {
                self.bump();
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::Break
            }
            Tk::Kw(Kw::Discard) => {
                self.bump();
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::Discard
            }
            Tk::Kw(Kw::Return) => {
                self.bump();
                let value = if self.is_punct(Punct::Semicolon) { None } else { Some(self.expression()?) };
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::Return(value)
            }
            _ if self.starts_declaration() => StmtKind::Declaration(self.declaration()?),
            _ => {
                let e = self.expression()?;
                self.expect_punct(Punct::Semicolon)?;
                StmtKind::Expr(e)
            }
        };
        Ok(Stmt { kind, loc })
    }

    /// A loop condition: an expression or `type name = initializer`.
    fn condition(&mut self) -> PResult<Condition> {
        if self.starts_declaration() {
            let loc = self.loc();
            let qualifiers = self.qualifiers()?;
            let ty = self.type_spec()?;
            let (name, _) = self.expect_ident()?;
            self.expect_punct(Punct::Assign)?;
            let init = self.assignment()?;
            return Ok(Condition::Decl { ty: FullType { qualifiers, ty }, name, init, loc });
        }
        Ok(Condition::Expr(self.expression()?))
    }

    // ---- Expressions -----------------------------------------------------

    /// `a, b, ...`
    fn expression(&mut self) -> PResult<Expr> {
        let mut e = self.assignment()?;
        while self.is_punct(Punct::Comma) {
            let loc = self.bump().loc;
            let rhs = self.assignment()?;
            e = Expr { kind: ExprKind::Comma(Box::new(e), Box::new(rhs)), loc };
        }
        Ok(e)
    }

    fn assignment(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.assignment_inner();
        self.leave();
        r
    }

    fn assignment_inner(&mut self) -> PResult<Expr> {
        let lhs = self.conditional()?;
        let op = match self.peek() {
            Tk::Punct(Punct::Assign) => None,
            Tk::Punct(Punct::AddAssign) => Some(BinaryOp::Add),
            Tk::Punct(Punct::SubAssign) => Some(BinaryOp::Sub),
            Tk::Punct(Punct::MulAssign) => Some(BinaryOp::Mul),
            Tk::Punct(Punct::DivAssign) => Some(BinaryOp::Div),
            Tk::Punct(Punct::ModAssign) => Some(BinaryOp::Mod),
            Tk::Punct(Punct::ShlAssign) => Some(BinaryOp::Shl),
            Tk::Punct(Punct::ShrAssign) => Some(BinaryOp::Shr),
            Tk::Punct(Punct::AndAssign) => Some(BinaryOp::BitAnd),
            Tk::Punct(Punct::XorAssign) => Some(BinaryOp::BitXor),
            Tk::Punct(Punct::OrAssign) => Some(BinaryOp::BitOr),
            _ => return Ok(lhs),
        };
        let loc = self.bump().loc;
        let rhs = self.assignment()?;
        Ok(Expr { kind: ExprKind::Assign(op, Box::new(lhs), Box::new(rhs)), loc })
    }

    fn conditional(&mut self) -> PResult<Expr> {
        let cond = self.binary(1)?;
        if !self.is_punct(Punct::Question) {
            return Ok(cond);
        }
        let loc = self.bump().loc;
        let a = self.expression()?;
        self.expect_punct(Punct::Colon)?;
        let b = self.assignment()?;
        Ok(Expr { kind: ExprKind::Ternary(Box::new(cond), Box::new(a), Box::new(b)), loc })
    }

    /// Binary operators of precedence `min` and higher.
    fn binary(&mut self, min: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let Some((op, prec)) = binary_op(self.peek()) else { return Ok(lhs) };
            if prec < min {
                return Ok(lhs);
            }
            let loc = self.bump().loc;
            self.enter()?;
            let rhs = self.binary(prec + 1);
            self.leave();
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs?)), loc };
        }
    }

    fn unary(&mut self) -> PResult<Expr> {
        let op = match self.peek() {
            Tk::Punct(Punct::Plus) => UnaryOp::Plus,
            Tk::Punct(Punct::Minus) => UnaryOp::Minus,
            Tk::Punct(Punct::Bang) => UnaryOp::Not,
            Tk::Punct(Punct::Tilde) => UnaryOp::BitNot,
            Tk::Punct(Punct::Inc) => UnaryOp::PreInc,
            Tk::Punct(Punct::Dec) => UnaryOp::PreDec,
            _ => return self.postfix(),
        };
        let loc = self.bump().loc;
        self.enter()?;
        let operand = self.unary();
        self.leave();
        Ok(Expr { kind: ExprKind::Unary(op, Box::new(operand?)), loc })
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            let loc = self.loc();
            match self.peek() {
                Tk::Punct(Punct::LBracket) => {
                    self.bump();
                    let index = self.expression()?;
                    self.expect_punct(Punct::RBracket)?;
                    // `S[N](...)`: an array constructor of a structure type.
                    if self.is_punct(Punct::LParen)
                        && let ExprKind::Ident(name) = e.kind
                    {
                        let args = self.arguments()?;
                        e = Expr {
                            kind: ExprKind::Call(Callee::NamedArray(name, Some(Box::new(index))), args),
                            loc: e.loc,
                        };
                        continue;
                    }
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(index)), loc };
                }
                Tk::Punct(Punct::Dot) => {
                    self.bump();
                    let (name, _) = self.expect_ident()?;
                    if Some(name) == self.length && self.is_punct(Punct::LParen) {
                        self.bump();
                        self.expect_punct(Punct::RParen)?;
                        e = Expr { kind: ExprKind::Length(Box::new(e)), loc };
                    } else if self.is_punct(Punct::LParen) {
                        let found = alloc::string::String::from(self.interner.get(name));
                        error!(self.diags, loc, "'{found}' is not a method: only length() is");
                        return Err(Stop);
                    } else {
                        e = Expr { kind: ExprKind::Field(Box::new(e), name), loc };
                    }
                }
                Tk::Punct(Punct::Inc) => {
                    self.bump();
                    e = Expr { kind: ExprKind::Unary(UnaryOp::PostInc, Box::new(e)), loc };
                }
                Tk::Punct(Punct::Dec) => {
                    self.bump();
                    e = Expr { kind: ExprKind::Unary(UnaryOp::PostDec, Box::new(e)), loc };
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let loc = self.loc();
        let kind = match self.peek() {
            Tk::Int(v) => {
                self.bump();
                ExprKind::Int(v)
            }
            Tk::Uint(v) => {
                self.bump();
                ExprKind::Uint(v)
            }
            Tk::Float(v) => {
                self.bump();
                ExprKind::Float(v)
            }
            Tk::Bool(b) => {
                self.bump();
                ExprKind::Bool(b)
            }
            Tk::Ident(s) => {
                self.bump();
                if self.is_punct(Punct::LParen) {
                    ExprKind::Call(Callee::Name(s), self.arguments()?)
                } else if self.is_punct(Punct::LBracket)
                    && self.peek_at(1) == Tk::Punct(Punct::RBracket)
                    && self.peek_at(2) == Tk::Punct(Punct::LParen)
                {
                    // S[](...)
                    self.bump();
                    self.bump();
                    ExprKind::Call(Callee::NamedArray(s, None), self.arguments()?)
                } else {
                    ExprKind::Ident(s)
                }
            }
            Tk::Type(_) => {
                let ty = self.type_spec()?;
                if !self.is_punct(Punct::LParen) {
                    return self.unexpected("'(' after a type in an expression");
                }
                ExprKind::Call(Callee::Type(ty), self.arguments()?)
            }
            Tk::Punct(Punct::LParen) => {
                self.bump();
                self.enter()?;
                let e = self.expression();
                self.leave();
                let e = e?;
                self.expect_punct(Punct::RParen)?;
                return Ok(e);
            }
            Tk::Kw(Kw::Struct) => {
                error!(self.diags, loc, "a structure cannot be defined in an expression");
                return Err(Stop);
            }
            _ => return self.unexpected("an expression"),
        };
        Ok(Expr { kind, loc })
    }

    /// `( args )`; `f()` and `f(void)` have none.
    fn arguments(&mut self) -> PResult<Vec<Expr>> {
        self.expect_punct(Punct::LParen)?;
        let mut args = Vec::new();
        if self.eat_punct(Punct::RParen) {
            return Ok(args);
        }
        if self.peek() == Tk::Type(crate::types::Basic::Void) && self.peek_at(1) == Tk::Punct(Punct::RParen) {
            self.bump();
            self.bump();
            return Ok(args);
        }
        loop {
            args.push(self.assignment()?);
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(args)
    }
}

fn precision_of(k: Kw) -> Precision {
    match k {
        Kw::Lowp => Precision::Low,
        Kw::Mediump => Precision::Medium,
        _ => Precision::High,
    }
}

/// A binary operator and its precedence (higher binds tighter).
fn binary_op(tk: Tk) -> Option<(BinaryOp, u8)> {
    let Tk::Punct(p) = tk else { return None };
    Some(match p {
        Punct::OrOr => (BinaryOp::Or, 1),
        Punct::XorXor => (BinaryOp::Xor, 2),
        Punct::AndAnd => (BinaryOp::And, 3),
        Punct::Pipe => (BinaryOp::BitOr, 4),
        Punct::Caret => (BinaryOp::BitXor, 5),
        Punct::Amp => (BinaryOp::BitAnd, 6),
        Punct::EqEq => (BinaryOp::Eq, 7),
        Punct::Ne => (BinaryOp::Ne, 7),
        Punct::Lt => (BinaryOp::Lt, 8),
        Punct::Gt => (BinaryOp::Gt, 8),
        Punct::Le => (BinaryOp::Le, 8),
        Punct::Ge => (BinaryOp::Ge, 8),
        Punct::Shl => (BinaryOp::Shl, 9),
        Punct::Shr => (BinaryOp::Shr, 9),
        Punct::Plus => (BinaryOp::Add, 10),
        Punct::Minus => (BinaryOp::Sub, 10),
        Punct::Star => (BinaryOp::Mul, 11),
        Punct::Slash => (BinaryOp::Div, 11),
        Punct::Percent => (BinaryOp::Mod, 11),
        _ => return None,
    })
}
