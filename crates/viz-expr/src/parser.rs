//! Pratt parser for the ViewMusic formula language (contract §2).
//!
//! Builds an [`Expr`] AST from the token stream. Precedence, lowest → highest:
//! `||` < `&&` < comparisons < add/sub < mul/div/rem < unary `-`/`!` < `^`
//! (right-associative) < call/primary. Notably **unary minus binds looser than
//! power**, so `-2^2` parses as `-(2^2)` — see the crate docs and the matching
//! reference evaluator. Arity of function calls is validated here against the
//! built-in table so authoring errors surface at compile time.

use crate::builtins::{lookup_builtin, BuiltinFn};
use crate::lexer::{Token, TokenKind};

/// A parsed expression node. The AST is intentionally public so the property
/// test's reference evaluator can walk the exact same shape the VM compiles.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// Numeric literal.
    Num(f64),
    /// A bare or dotted identifier (input, setting, var, or constant).
    Ident(String),
    /// Unary operator applied to one operand.
    Unary(UnaryOp, Box<Expr>),
    /// Binary operator applied to two operands.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// A built-in function call with its resolved descriptor and arguments.
    Call(BuiltinFn, Vec<Expr>),
}

/// Unary operators (contract §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation `-x`.
    Neg,
    /// Logical NOT `!x` (truthy when `x >= 0.5`; yields 0/1).
    Not,
}

/// Binary operators (contract §2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%` (f64 remainder)
    Rem,
    /// `^` (power, right-associative)
    Pow,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `&&` (operands truthy when `>= 0.5`; yields 0/1)
    And,
    /// `||` (operands truthy when `>= 0.5`; yields 0/1)
    Or,
}

/// A parse failure with the offending token text and its byte position.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    /// The error classification.
    pub kind: ParseErrorKind,
    /// Offending token text, verbatim (named in diagnostics).
    pub token: String,
    /// Byte offset of the offending token within the source.
    pub position: usize,
}

/// Classifications of parse failures.
#[derive(Clone, Debug, PartialEq)]
pub enum ParseErrorKind {
    /// Encountered a token where an expression/operand was expected.
    UnexpectedToken,
    /// Ran out of tokens mid-expression.
    UnexpectedEof,
    /// Expected a specific token (e.g. `)` or `,`) but found another.
    ExpectedToken,
    /// A function was called with the wrong number of arguments.
    ArityMismatch,
    /// An identifier used in call position is not a known function.
    UnknownFunction,
    /// Trailing tokens remained after a complete expression.
    TrailingTokens,
}

/// Parses a full token stream into an [`Expr`], consuming every token.
pub fn parse(tokens: &[Token]) -> Result<Expr, ParseError> {
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr(0)?;
    if p.pos != tokens.len() {
        let tok = &tokens[p.pos];
        return Err(ParseError {
            kind: ParseErrorKind::TrailingTokens,
            token: describe(&tok.kind),
            position: tok.position,
        });
    }
    Ok(expr)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&Token> {
        let t = self.tokens.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eof_error(&self) -> ParseError {
        // Position points just past the last token consumed (end of input).
        let position = self
            .tokens
            .last()
            .map(|t| t.position + token_len(&t.kind))
            .unwrap_or(0);
        ParseError {
            kind: ParseErrorKind::UnexpectedEof,
            token: String::new(),
            position,
        }
    }

    /// Pratt loop: parse a prefix, then fold infix operators whose left binding
    /// power exceeds `min_bp`.
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_prefix()?;
        while let Some(op_tok) = self.peek() {
            let (op, l_bp, r_bp) = match infix_binding_power(&op_tok.kind) {
                Some(v) => v,
                None => break,
            };
            if l_bp < min_bp {
                break;
            }
            self.pos += 1; // consume the operator
            let rhs = self.parse_expr(r_bp)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Parses a prefix expression: literals, identifiers, calls, grouping, and
    /// the unary operators `-` and `!`.
    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let tok = self.peek().ok_or_else(|| self.eof_error())?.clone();
        match &tok.kind {
            TokenKind::Number(v) => {
                self.pos += 1;
                Ok(Expr::Num(*v))
            }
            TokenKind::Minus => {
                self.pos += 1;
                // Unary minus binds looser than `^` (contract choice).
                let operand = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(operand)))
            }
            TokenKind::Bang => {
                self.pos += 1;
                let operand = self.parse_expr(UNARY_BP)?;
                Ok(Expr::Unary(UnaryOp::Not, Box::new(operand)))
            }
            TokenKind::LParen => {
                self.pos += 1;
                let inner = self.parse_expr(0)?;
                self.expect(&TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Ident(name) => {
                self.pos += 1;
                // Call iff immediately followed by `(`.
                if matches!(self.peek().map(|t| &t.kind), Some(TokenKind::LParen)) {
                    self.parse_call(name, tok.position)
                } else {
                    Ok(Expr::Ident(name.clone()))
                }
            }
            _ => Err(ParseError {
                kind: ParseErrorKind::UnexpectedToken,
                token: describe(&tok.kind),
                position: tok.position,
            }),
        }
    }

    /// Parses `name(arg, arg, …)` given the leading identifier was a function.
    /// Validates the function name and arity against the built-in table.
    fn parse_call(&mut self, name: &str, name_pos: usize) -> Result<Expr, ParseError> {
        let func = lookup_builtin(name).ok_or_else(|| ParseError {
            kind: ParseErrorKind::UnknownFunction,
            token: name.to_owned(),
            position: name_pos,
        })?;
        self.expect(&TokenKind::LParen)?;
        let mut args = Vec::new();
        if !matches!(self.peek().map(|t| &t.kind), Some(TokenKind::RParen)) {
            loop {
                args.push(self.parse_expr(0)?);
                match self.peek().map(|t| &t.kind) {
                    Some(TokenKind::Comma) => {
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
        }
        self.expect(&TokenKind::RParen)?;
        if args.len() != func.arity {
            return Err(ParseError {
                kind: ParseErrorKind::ArityMismatch,
                token: name.to_owned(),
                position: name_pos,
            });
        }
        Ok(Expr::Call(func, args))
    }

    /// Consumes the next token, requiring it to equal `expected`.
    fn expect(&mut self, expected: &TokenKind) -> Result<(), ParseError> {
        match self.next() {
            Some(t) if &t.kind == expected => Ok(()),
            Some(t) => Err(ParseError {
                kind: ParseErrorKind::ExpectedToken,
                token: describe(&t.kind),
                position: t.position,
            }),
            None => Err(self.eof_error()),
        }
    }
}

/// Binding power of the unary prefix operators. Lower than `^` so that `-2^2`
/// parses as `-(2^2)` and `!a == b` as `!(a == b)`... actually `!` is prefix and
/// binds the smallest complete expression at this power; the only operator that
/// out-binds it is `^`.
const UNARY_BP: u8 = 9;

/// Returns `(op, left_bp, right_bp)` for an infix operator token, or `None` if
/// the token does not begin an infix operator. Higher numbers bind tighter.
/// Right-associative operators have `right_bp < left_bp`.
fn infix_binding_power(kind: &TokenKind) -> Option<(BinaryOp, u8, u8)> {
    Some(match kind {
        TokenKind::OrOr => (BinaryOp::Or, 1, 2),
        TokenKind::AndAnd => (BinaryOp::And, 3, 4),
        TokenKind::Lt => (BinaryOp::Lt, 5, 6),
        TokenKind::Le => (BinaryOp::Le, 5, 6),
        TokenKind::Gt => (BinaryOp::Gt, 5, 6),
        TokenKind::Ge => (BinaryOp::Ge, 5, 6),
        TokenKind::EqEq => (BinaryOp::Eq, 5, 6),
        TokenKind::Ne => (BinaryOp::Ne, 5, 6),
        TokenKind::Plus => (BinaryOp::Add, 7, 8),
        TokenKind::Minus => (BinaryOp::Sub, 7, 8),
        TokenKind::Star => (BinaryOp::Mul, 9, 10),
        // Note: mul/div/rem (left_bp 9) sit at the same level as UNARY_BP so a
        // unary operand stops before a following `*` — `-a*b` = `(-a)*b`. The
        // `^` operator (left_bp 11) still binds tighter than unary, giving
        // `-2^2 = -(2^2)`.
        TokenKind::Slash => (BinaryOp::Div, 9, 10),
        TokenKind::Percent => (BinaryOp::Rem, 9, 10),
        // Power is right-associative: right_bp < left_bp.
        TokenKind::Caret => (BinaryOp::Pow, 12, 11),
        _ => return None,
    })
}

/// Byte length of a token's source text (used to compute the EOF position).
fn token_len(kind: &TokenKind) -> usize {
    match kind {
        TokenKind::Le
        | TokenKind::Ge
        | TokenKind::EqEq
        | TokenKind::Ne
        | TokenKind::AndAnd
        | TokenKind::OrOr => 2,
        TokenKind::Ident(s) => s.len(),
        TokenKind::Number(_) => 1,
        _ => 1,
    }
}

/// Human-readable rendering of a token for diagnostics.
fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Number(v) => v.to_string(),
        TokenKind::Ident(s) => s.clone(),
        TokenKind::Plus => "+".to_owned(),
        TokenKind::Minus => "-".to_owned(),
        TokenKind::Star => "*".to_owned(),
        TokenKind::Slash => "/".to_owned(),
        TokenKind::Percent => "%".to_owned(),
        TokenKind::Caret => "^".to_owned(),
        TokenKind::LParen => "(".to_owned(),
        TokenKind::RParen => ")".to_owned(),
        TokenKind::Comma => ",".to_owned(),
        TokenKind::Lt => "<".to_owned(),
        TokenKind::Le => "<=".to_owned(),
        TokenKind::Gt => ">".to_owned(),
        TokenKind::Ge => ">=".to_owned(),
        TokenKind::EqEq => "==".to_owned(),
        TokenKind::Ne => "!=".to_owned(),
        TokenKind::AndAnd => "&&".to_owned(),
        TokenKind::OrOr => "||".to_owned(),
        TokenKind::Bang => "!".to_owned(),
    }
}
