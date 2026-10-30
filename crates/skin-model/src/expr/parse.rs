//! The Pratt parser: tokens to an arena syntax tree with spans.
//!
//! Two guards bound the tree. [`Parser::enter`] runs before every
//! recursive `expr` call (a binary operator's right operand, a parenthesis,
//! a unary operand, a call's arguments, an `if`) and refuses to pass
//! [`MAX_NESTING`] levels. [`Parser::push`] refuses a node whose operator
//! height passes [`MAX_NESTING`], which catches the left chains the Pratt
//! loop builds without recursing. Every later recursive pass is bounded
//! by the height.

use super::MAX_NESTING;
use super::error::{CompileError, ErrorKind, Span};
use super::lex::{MIN_MAGNITUDE, Tok, Token};

/// A node's index in the arena.
pub(super) type Id = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Func {
    Min,
    Max,
    Clamp,
    Abs,
    On,
    Elapsed,
    Option,
}

impl Func {
    pub(super) const ALL: [Self; 7] = [
        Self::Min,
        Self::Max,
        Self::Clamp,
        Self::Abs,
        Self::On,
        Self::Elapsed,
        Self::Option,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Min => "min",
            Self::Max => "max",
            Self::Clamp => "clamp",
            Self::Abs => "abs",
            Self::On => "on",
            Self::Elapsed => "elapsed",
            Self::Option => "option",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Kind {
    Int(i32),
    Bool(bool),
    /// A name and the span of its text (the node's span widens to any
    /// parentheses around it).
    Name(Span),
    Un(UnOp, Id),
    Bin(BinOp, Id, Id),
    If(Id, Id, Id),
    /// A built-in call with its argument count checked; unused argument
    /// slots are 0.
    Call(Func, [Id; 3], u8),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Node {
    pub(super) kind: Kind,
    pub(super) span: Span,
    /// Operators on the longest path to a leaf after `clamp` is lowered; a
    /// leaf is 0.
    height: u8,
}

/// A parsed expression.
#[derive(Debug)]
pub(super) struct Ast<'s> {
    src: &'s str,
    nodes: Vec<Node>,
    root: Id,
}

impl<'s> Ast<'s> {
    pub(super) fn root(&self) -> Id {
        self.root
    }

    pub(super) fn node(&self, id: Id) -> Node {
        self.nodes[usize::from(id)]
    }

    pub(super) fn span(&self, id: Id) -> Span {
        self.node(id).span
    }

    /// The text of name node `id`; empty for any other node.
    pub(super) fn text(&self, id: Id) -> &'s str {
        match self.node(id).kind {
            Kind::Name(span) => span.text(self.src),
            _ => "",
        }
    }
}

pub(super) fn parse<'s>(src: &'s str, tokens: &[Token]) -> Result<Ast<'s>, CompileError> {
    let mut p = Parser {
        src,
        tokens,
        pos: 0,
        depth: 0,
        nodes: Vec::new(),
    };
    let root = p.expr(0)?;
    let next = p.peek();
    if next.tok != Tok::Eof {
        return Err(CompileError::new(
            next.span,
            ErrorKind::Expected {
                what: "an operator or the end of the expression",
            },
        ));
    }
    Ok(Ast {
        src,
        nodes: p.nodes,
        root,
    })
}

struct Parser<'s, 't> {
    src: &'s str,
    tokens: &'t [Token],
    pos: usize,
    depth: usize,
    nodes: Vec<Node>,
}

/// The binding power of a unary operand: above every binary operator.
const UNARY_BP: u8 = 14;

/// The binary operator `tok` stands for and its precedence level (1 is
/// `||`, 6 is `* / %`).
fn infix(tok: Tok) -> Option<(BinOp, u8)> {
    Some(match tok {
        Tok::OrOr => (BinOp::Or, 1),
        Tok::AndAnd => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::Ne => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::Le => (BinOp::Le, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::Ge => (BinOp::Ge, 4),
        Tok::Plus => (BinOp::Add, 5),
        Tok::Minus => (BinOp::Sub, 5),
        Tok::Star => (BinOp::Mul, 6),
        Tok::Slash => (BinOp::Div, 6),
        Tok::Percent => (BinOp::Rem, 6),
        _ => return None,
    })
}

impl Parser<'_, '_> {
    fn peek(&self) -> Token {
        self.peek_at(0)
    }

    fn peek_at(&self, ahead: usize) -> Token {
        // The token list ends with `Eof`, which is never consumed.
        let last = self.tokens.len() - 1;
        self.tokens[(self.pos + ahead).min(last)]
    }

    fn bump(&mut self) -> Token {
        let t = self.peek();
        if t.tok != Tok::Eof {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, tok: Tok, what: &'static str) -> Result<Token, CompileError> {
        let t = self.peek();
        if t.tok == tok {
            Ok(self.bump())
        } else {
            Err(CompileError::new(t.span, ErrorKind::Expected { what }))
        }
    }

    /// One more level of recursion, refused at [`MAX_NESTING`].
    fn enter(&mut self) -> Result<(), CompileError> {
        if self.depth >= MAX_NESTING {
            return Err(CompileError::new(self.peek().span, ErrorKind::TooDeep));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Adds a node, refusing one whose height passes [`MAX_NESTING`].
    fn push(&mut self, kind: Kind, span: Span) -> Result<Id, CompileError> {
        let h = |id: Id| usize::from(self.nodes[usize::from(id)].height);
        let height = match kind {
            Kind::Int(_) | Kind::Bool(_) | Kind::Name(_) => 0,
            Kind::Un(_, a) => h(a) + 1,
            Kind::Bin(_, a, b) => h(a).max(h(b)) + 1,
            Kind::If(c, a, b) => h(c).max(h(a)).max(h(b)) + 1,
            // `clamp(x, l, h)` is checked as `min(max(x, l), h)`.
            Kind::Call(Func::Clamp, [x, l, hi], _) => (h(x).max(h(l)) + 2).max(h(hi) + 1),
            Kind::Call(_, args, n) => {
                args.iter()
                    .take(usize::from(n))
                    .map(|&a| h(a))
                    .max()
                    .unwrap_or(0)
                    + 1
            }
        };
        if height > MAX_NESTING {
            return Err(CompileError::new(span, ErrorKind::TooDeep));
        }
        let id = Id::try_from(self.nodes.len()).map_err(|_| {
            CompileError::new(
                span,
                ErrorKind::TooLong {
                    len: self.src.len(),
                },
            )
        })?;
        self.nodes.push(Node {
            kind,
            span,
            height: u8::try_from(height).unwrap_or(u8::MAX),
        });
        Ok(id)
    }

    fn span(&self, id: Id) -> Span {
        self.nodes[usize::from(id)].span
    }

    fn expr(&mut self, min_bp: u8) -> Result<Id, CompileError> {
        let mut lhs = self.prefix()?;
        // The level of the operator that built `lhs` in this loop, so a
        // non-associative operator does not chain.
        let mut built_by = None;
        while let Some((op, level)) = infix(self.peek().tok) {
            if level * 2 < min_bp {
                break;
            }
            let op_tok = self.bump();
            if matches!(level, 3 | 4) && built_by == Some(level) {
                return Err(CompileError::new(op_tok.span, ErrorKind::NonAssociative));
            }
            self.enter()?;
            let rhs = self.expr(level * 2 + 1)?;
            self.leave();
            let span = self.span(lhs).to(self.span(rhs));
            lhs = self.push(Kind::Bin(op, lhs, rhs), span)?;
            built_by = Some(level);
        }
        Ok(lhs)
    }

    fn prefix(&mut self) -> Result<Id, CompileError> {
        let t = self.bump();
        match t.tok {
            Tok::Int(v) => match i32::try_from(v) {
                Ok(v) => self.push(Kind::Int(v), t.span),
                Err(_) => Err(CompileError::new(t.span, ErrorKind::IntTooLarge)),
            },
            Tok::Duration(v) => self.push(Kind::Int(v), t.span),
            Tok::True => self.push(Kind::Bool(true), t.span),
            Tok::False => self.push(Kind::Bool(false), t.span),
            Tok::Minus => {
                let next = self.peek();
                if next.tok == Tok::Int(MIN_MAGNITUDE) {
                    self.bump();
                    return self.push(Kind::Int(i32::MIN), t.span.to(next.span));
                }
                self.unary(UnOp::Neg, t.span)
            }
            Tok::Bang => self.unary(UnOp::Not, t.span),
            Tok::LParen => {
                self.enter()?;
                let inner = self.expr(0)?;
                let close = self.expect(Tok::RParen, "`)`")?;
                self.leave();
                self.nodes[usize::from(inner)].span = t.span.to(close.span);
                Ok(inner)
            }
            Tok::If => {
                self.enter()?;
                let c = self.expr(0)?;
                self.expect(Tok::Then, "`then`")?;
                let a = self.expr(0)?;
                self.expect(Tok::Else, "`else`")?;
                let b = self.expr(0)?;
                self.leave();
                let span = t.span.to(self.span(b));
                self.push(Kind::If(c, a, b), span)
            }
            Tok::Name if self.peek().tok == Tok::LParen => self.call(t),
            Tok::Name => self.push(Kind::Name(t.span), t.span),
            _ => Err(CompileError::new(
                t.span,
                ErrorKind::Expected {
                    what: "an expression",
                },
            )),
        }
    }

    fn unary(&mut self, op: UnOp, op_span: Span) -> Result<Id, CompileError> {
        self.enter()?;
        let operand = self.expr(UNARY_BP)?;
        self.leave();
        let span = op_span.to(self.span(operand));
        self.push(Kind::Un(op, operand), span)
    }

    fn call(&mut self, name: Token) -> Result<Id, CompileError> {
        let text = name.span.text(self.src);
        let Some(func) = Func::ALL.into_iter().find(|f| f.name() == text) else {
            return Err(CompileError::new(
                name.span,
                ErrorKind::UnknownFunction { name: text.into() },
            ));
        };
        self.bump(); // `(`
        self.enter()?;
        let mut args = Vec::new();
        let close = if self.peek().tok == Tok::RParen {
            self.bump()
        } else {
            loop {
                args.push(self.expr(0)?);
                let t = self.bump();
                match t.tok {
                    Tok::Comma => {}
                    Tok::RParen => break t,
                    _ => {
                        return Err(CompileError::new(
                            t.span,
                            ErrorKind::Expected { what: "`,` or `)`" },
                        ));
                    }
                }
            }
        };
        self.leave();
        let span = name.span.to(close.span);
        let expected: u8 = match func {
            Func::Min | Func::Max => 2,
            Func::Clamp => 3,
            Func::Abs | Func::On | Func::Elapsed => 1,
            // `option(name)` or `option(name, choice)`.
            Func::Option => {
                if args.len() >= 2 {
                    2
                } else {
                    1
                }
            }
        };
        if args.len() != usize::from(expected) {
            return Err(CompileError::new(
                span,
                ErrorKind::Arity {
                    func: func.name(),
                    expected,
                    found: args.len(),
                },
            ));
        }
        let mut slots = [0; 3];
        for (slot, arg) in slots.iter_mut().zip(&args) {
            *slot = *arg;
        }
        self.push(Kind::Call(func, slots, expected), span)
    }
}

#[cfg(test)]
mod tests {
    use super::Func;
    use crate::expr::RESERVED;

    #[test]
    fn built_ins_and_keywords_are_reserved() {
        for func in Func::ALL {
            assert!(RESERVED.contains(&func.name()), "{}", func.name());
        }
        for keyword in ["if", "then", "else", "true", "false"] {
            assert!(RESERVED.contains(&keyword), "{keyword}");
        }
        assert_eq!(RESERVED.len(), Func::ALL.len() + 5);
    }
}
