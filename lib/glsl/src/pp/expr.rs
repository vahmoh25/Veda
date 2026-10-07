//! `#if`, `#elif` and `#line` expressions.
//!
//! Integer constants and the operators of GLSL ES's preprocessor table
//! (C's, without `?:`, `,` or character constants), evaluated in 64-bit
//! arithmetic. `&&` and `||` evaluate their right operand only when it
//! matters, and errors in an operand that is not evaluated (undefined
//! names, division by zero) are not reported, as the specification asks.

use alloc::string::String;

use super::Punct;
use super::lex::{PpKind, PpTok};
use crate::intern::{Interner, Symbol};

/// Why an expression could not be evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExprError {
    /// An identifier that is not a defined macro.
    Undefined(Symbol),
    /// A token that cannot appear here (described).
    Unexpected(Option<PpKind>),
    /// A number that is not an integer constant, or too large.
    BadNumber(Symbol),
    DivideByZero,
    Overflow,
    ShiftOutOfRange,
}

impl ExprError {
    /// A message for the information log.
    pub fn message(&self, interner: &Interner) -> String {
        match *self {
            ExprError::Undefined(s) => alloc::format!("'{}' is not defined", interner.get(s)),
            ExprError::Unexpected(None) => "unexpected end of expression".into(),
            ExprError::Unexpected(Some(k)) => alloc::format!("unexpected '{}' in expression", spell(k, interner)),
            ExprError::BadNumber(s) => alloc::format!("'{}' is not an integer constant", interner.get(s)),
            ExprError::DivideByZero => "division by zero in preprocessor expression".into(),
            ExprError::Overflow => "integer overflow in preprocessor expression".into(),
            ExprError::ShiftOutOfRange => "shift by a negative or too large amount".into(),
        }
    }
}

fn spell(k: PpKind, interner: &Interner) -> String {
    match k {
        PpKind::Ident(s) | PpKind::Number(s) => interner.get(s).into(),
        PpKind::Punct(p) => p.as_str().into(),
        PpKind::Invalid(c) => {
            let mut s = String::new();
            s.push(c);
            s
        }
        PpKind::Newline => "end of line".into(),
    }
}

/// Evaluates a whole `#if` expression.
pub fn evaluate(tokens: &[PpTok], interner: &Interner, defined: Symbol) -> Result<i64, ExprError> {
    let mut p = Parser { tokens, pos: 0, interner, defined };
    let v = p.expr(true)?;
    match p.peek() {
        None => Ok(v),
        Some(k) => Err(ExprError::Unexpected(Some(k))),
    }
}

/// Evaluates `#line`'s operands: a line number and an optional source
/// string number.
pub fn evaluate_line(tokens: &[PpTok], interner: &Interner, defined: Symbol) -> Result<(i64, Option<i64>), ExprError> {
    let mut p = Parser { tokens, pos: 0, interner, defined };
    let line = p.expr(true)?;
    if p.peek().is_none() {
        return Ok((line, None));
    }
    let string = p.expr(true)?;
    match p.peek() {
        None => Ok((line, Some(string))),
        Some(k) => Err(ExprError::Unexpected(Some(k))),
    }
}

/// Parses an integer constant: decimal, octal (leading 0) or hexadecimal,
/// with an optional `u` suffix; at most 32 bits.
pub fn integer(text: &str) -> Option<i64> {
    let t = text.strip_suffix(['u', 'U']).unwrap_or(text);
    let (digits, radix) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (h, 16)
    } else if t.len() > 1 && t.starts_with('0') {
        (&t[1..], 8)
    } else {
        (t, 10)
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok().filter(|&v| v <= u64::from(u32::MAX)).map(|v| v as i64)
}

struct Parser<'t, 'i> {
    tokens: &'t [PpTok],
    pos: usize,
    interner: &'i Interner,
    defined: Symbol,
}

/// Binary operators by precedence level, lowest first.
const LEVELS: [&[Punct]; 10] = [
    &[Punct::OrOr],
    &[Punct::AndAnd],
    &[Punct::Pipe],
    &[Punct::Caret],
    &[Punct::Amp],
    &[Punct::EqEq, Punct::Ne],
    &[Punct::Lt, Punct::Gt, Punct::Le, Punct::Ge],
    &[Punct::Shl, Punct::Shr],
    &[Punct::Plus, Punct::Minus],
    &[Punct::Star, Punct::Slash, Punct::Percent],
];

impl Parser<'_, '_> {
    fn peek(&self) -> Option<PpKind> {
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    fn expr(&mut self, eval: bool) -> Result<i64, ExprError> {
        self.binary(0, eval)
    }

    fn binary(&mut self, level: usize, eval: bool) -> Result<i64, ExprError> {
        if level == LEVELS.len() {
            return self.unary(eval);
        }
        let mut lhs = self.binary(level + 1, eval)?;
        loop {
            let op = match self.peek() {
                Some(PpKind::Punct(p)) if LEVELS[level].contains(&p) => p,
                _ => return Ok(lhs),
            };
            self.pos += 1;
            lhs = match op {
                Punct::OrOr => {
                    let rhs = self.binary(level + 1, eval && lhs == 0)?;
                    i64::from(lhs != 0 || rhs != 0)
                }
                Punct::AndAnd => {
                    let rhs = self.binary(level + 1, eval && lhs != 0)?;
                    i64::from(lhs != 0 && rhs != 0)
                }
                _ => {
                    let rhs = self.binary(level + 1, eval)?;
                    if eval { apply(op, lhs, rhs)? } else { 0 }
                }
            };
        }
    }

    fn unary(&mut self, eval: bool) -> Result<i64, ExprError> {
        match self.peek() {
            Some(PpKind::Punct(op @ (Punct::Plus | Punct::Minus | Punct::Tilde | Punct::Bang))) => {
                self.pos += 1;
                let v = self.unary(eval)?;
                Ok(match op {
                    Punct::Plus => v,
                    Punct::Minus => v.checked_neg().ok_or(ExprError::Overflow)?,
                    Punct::Tilde => !v,
                    _ => i64::from(v == 0),
                })
            }
            Some(PpKind::Punct(Punct::LParen)) => {
                self.pos += 1;
                let v = self.expr(eval)?;
                match self.peek() {
                    Some(PpKind::Punct(Punct::RParen)) => {
                        self.pos += 1;
                        Ok(v)
                    }
                    other => Err(ExprError::Unexpected(other)),
                }
            }
            Some(PpKind::Number(s)) => {
                self.pos += 1;
                integer(self.interner.get(s)).ok_or(ExprError::BadNumber(s))
            }
            Some(PpKind::Ident(s)) if s == self.defined => Err(ExprError::Unexpected(Some(PpKind::Ident(s)))),
            Some(PpKind::Ident(s)) => {
                self.pos += 1;
                if eval { Err(ExprError::Undefined(s)) } else { Ok(0) }
            }
            other => Err(ExprError::Unexpected(other)),
        }
    }
}

fn apply(op: Punct, a: i64, b: i64) -> Result<i64, ExprError> {
    let v = match op {
        Punct::Pipe => a | b,
        Punct::Caret => a ^ b,
        Punct::Amp => a & b,
        Punct::EqEq => i64::from(a == b),
        Punct::Ne => i64::from(a != b),
        Punct::Lt => i64::from(a < b),
        Punct::Gt => i64::from(a > b),
        Punct::Le => i64::from(a <= b),
        Punct::Ge => i64::from(a >= b),
        Punct::Shl | Punct::Shr => {
            if !(0..64).contains(&b) {
                return Err(ExprError::ShiftOutOfRange);
            }
            if op == Punct::Shl { a.checked_shl(b as u32).ok_or(ExprError::Overflow)? } else { a >> b }
        }
        Punct::Plus => a.checked_add(b).ok_or(ExprError::Overflow)?,
        Punct::Minus => a.checked_sub(b).ok_or(ExprError::Overflow)?,
        Punct::Star => a.checked_mul(b).ok_or(ExprError::Overflow)?,
        Punct::Slash | Punct::Percent => {
            if b == 0 {
                return Err(ExprError::DivideByZero);
            }
            if op == Punct::Slash {
                a.checked_div(b).ok_or(ExprError::Overflow)?
            } else {
                a.checked_rem(b).ok_or(ExprError::Overflow)?
            }
        }
        _ => unreachable!("not a binary operator: {op:?}"),
    };
    Ok(v)
}
