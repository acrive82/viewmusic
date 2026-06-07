//! Compilation: source → [`Program`] bytecode, with cap enforcement.
//!
//! Pipeline: enforce source length → lex → parse (Pratt, arity-checked) → check
//! nesting depth → resolve every identifier against the [`Scope`] / built-in
//! constants → emit flat bytecode → enforce op count. Any unknown identifier or
//! cap violation is reported with the offending token verbatim so the diagnostic
//! log can point the author at the exact problem.

use crate::builtins::{is_reserved, lookup_constant};
use crate::lexer::lex;
use crate::parser::{parse, Expr, UnaryOp};
use crate::vm::{BinaryKind, Op, Program, UnaryKind};

/// Maximum formula source length, in characters.
pub const MAX_SOURCE_LEN: usize = 1024;
/// Maximum AST nesting depth (contract §12).
pub const MAX_DEPTH: usize = 32;
/// Maximum compiled operations per formula (contract §12).
pub const MAX_OPS: usize = 256;

/// A compile-time slot index for a declared identifier. `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u16);

/// The set of identifiers a formula may read, mapped to dense slot indices.
///
/// Built once per stage (the host assigns slots for `t`, `dt`, audio aggregates,
/// `settings.*`, vars, and stage extras like `i`/`u`/`n`/`x`/`y`). Built-in
/// function and constant names are reserved and rejected as slot names.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    /// name → slot id, in insertion order (slot ids are assigned densely).
    names: Vec<String>,
}

impl Scope {
    /// Starts building a scope.
    pub fn builder() -> ScopeBuilder {
        ScopeBuilder {
            scope: Scope { names: Vec::new() },
        }
    }

    /// Number of declared slots (the VM's slot-bank size).
    pub fn slot_count(&self) -> usize {
        self.names.len()
    }

    /// Resolves an identifier to its slot, or `None` if it is not declared.
    pub fn slot_id(&self, name: &str) -> Option<SlotId> {
        self.names
            .iter()
            .position(|n| n == name)
            .map(|i| SlotId(i as u16))
    }
}

/// Incremental builder for a [`Scope`].
pub struct ScopeBuilder {
    scope: Scope,
}

impl ScopeBuilder {
    /// Declares a slot named `name`, returning its assigned [`SlotId`].
    ///
    /// Rejects duplicates and names colliding with built-in functions or
    /// constants (contract §4: built-in identifiers are reserved). Also rejects
    /// the empty name and overflow past `u16::MAX` slots.
    pub fn slot(&mut self, name: &str) -> Result<SlotId, ScopeError> {
        if name.is_empty() {
            return Err(ScopeError::EmptyName);
        }
        if is_reserved(name) {
            return Err(ScopeError::ReservedName(name.to_owned()));
        }
        if self.scope.names.iter().any(|n| n == name) {
            return Err(ScopeError::Duplicate(name.to_owned()));
        }
        if self.scope.names.len() >= u16::MAX as usize {
            return Err(ScopeError::TooManySlots);
        }
        let id = SlotId(self.scope.names.len() as u16);
        self.scope.names.push(name.to_owned());
        Ok(id)
    }

    /// Finalizes the scope.
    pub fn build(self) -> Scope {
        self.scope
    }
}

/// Failure declaring a slot in a [`ScopeBuilder`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The name was already declared in this scope.
    Duplicate(String),
    /// The name collides with a built-in function or constant (reserved).
    ReservedName(String),
    /// An empty slot name was supplied.
    EmptyName,
    /// More than `u16::MAX` slots were declared.
    TooManySlots,
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeError::Duplicate(n) => write!(f, "duplicate slot name '{n}'"),
            ScopeError::ReservedName(n) => {
                write!(
                    f,
                    "slot name '{n}' collides with a built-in function or constant"
                )
            }
            ScopeError::EmptyName => f.write_str("slot name must not be empty"),
            ScopeError::TooManySlots => f.write_str("too many slots declared"),
        }
    }
}

impl std::error::Error for ScopeError {}

/// A compilation failure. `token` and `position` name the offending source text
/// verbatim so the diagnostics log can point the author at the exact problem.
#[derive(Clone, Debug, PartialEq)]
pub struct CompileError {
    /// The error classification.
    pub kind: CompileErrorKind,
    /// Offending token text, verbatim (named in the diagnostics log entry).
    pub token: String,
    /// Byte offset of the offending token within the source.
    pub position: usize,
}

/// Classifications of compile failures.
#[derive(Clone, Debug, PartialEq)]
pub enum CompileErrorKind {
    /// Source exceeded [`MAX_SOURCE_LEN`] characters.
    SourceTooLong,
    /// A lexing error (unexpected character / malformed number).
    Lex,
    /// A parse/grammar error (includes arity and unknown-function errors).
    Parse,
    /// AST nesting exceeded [`MAX_DEPTH`].
    NestingTooDeep,
    /// An identifier is neither a declared slot nor a built-in constant.
    UnknownIdentifier,
    /// Compiled bytecode exceeded [`MAX_OPS`] operations.
    TooManyOps,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            CompileErrorKind::SourceTooLong => write!(
                f,
                "formula source exceeds {MAX_SOURCE_LEN} characters (at position {})",
                self.position
            ),
            CompileErrorKind::Lex => {
                write!(
                    f,
                    "unexpected character '{}' at position {}",
                    self.token, self.position
                )
            }
            CompileErrorKind::Parse => {
                write!(
                    f,
                    "syntax error near '{}' at position {}",
                    self.token, self.position
                )
            }
            CompileErrorKind::NestingTooDeep => write!(
                f,
                "expression nesting exceeds depth {MAX_DEPTH} near '{}' at position {}",
                self.token, self.position
            ),
            CompileErrorKind::UnknownIdentifier => write!(
                f,
                "unknown identifier '{}' at position {}",
                self.token, self.position
            ),
            CompileErrorKind::TooManyOps => {
                write!(f, "formula compiles to more than {MAX_OPS} operations")
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// Compiles `src` against `scope` into a [`Program`], enforcing all load-time
/// caps. Returns a [`CompileError`] naming the offending token on any failure.
pub fn compile(src: &str, scope: &Scope) -> Result<Program, CompileError> {
    // Cap 1: source length (counted in characters per the contract).
    let char_len = src.chars().count();
    if char_len > MAX_SOURCE_LEN {
        return Err(CompileError {
            kind: CompileErrorKind::SourceTooLong,
            token: String::new(),
            position: src.len(),
        });
    }

    // Lex.
    let tokens = lex(src).map_err(|e| CompileError {
        kind: CompileErrorKind::Lex,
        token: e.token,
        position: e.position,
    })?;

    // Parse (also checks function arity and unknown functions).
    let ast = parse(&tokens).map_err(|e| CompileError {
        kind: CompileErrorKind::Parse,
        token: e.token,
        position: e.position,
    })?;

    // Cap 2: nesting depth. We measure nesting as the **evaluation-stack depth**
    // the expression requires — the metric that actually bounds the VM's fixed
    // `[f64; STACK_SIZE]` stack. A long left-associative chain (`1+1+…+1`) stays
    // shallow (depth 2) and is bounded instead by the op-count cap; right-nested
    // or genuinely deep trees grow the stack and are caught here. With
    // MAX_DEPTH = 32 ≤ STACK_SIZE = 64, the stack can never overflow.
    check_depth(&ast)?;

    // Resolve + emit.
    let mut ops = Vec::new();
    emit(&ast, scope, &mut ops)?;

    // Cap 3: compiled op count.
    if ops.len() > MAX_OPS {
        return Err(CompileError {
            kind: CompileErrorKind::TooManyOps,
            token: String::new(),
            position: 0,
        });
    }

    Ok(Program { ops })
}

/// Verifies the expression's required evaluation-stack depth ≤ [`MAX_DEPTH`] and
/// returns it. The stack requirement is the classic Ershov number:
/// - leaf (literal/identifier): 1 slot;
/// - unary: same as its operand (the operand's slot is reused in place);
/// - binary `a op b`: `max(need(a), 1 + need(b))` — `a`'s result occupies one
///   slot while `b` is evaluated;
/// - call with args `a0..ak`: while arg `j` is computed, the `j` earlier results
///   sit on the stack, so `max_j (j + need(aj))`.
///
/// Returns [`CompileErrorKind::NestingTooDeep`] (naming a representative token)
/// if any subtree's requirement exceeds the cap.
fn check_depth(expr: &Expr) -> Result<usize, CompileError> {
    let need = match expr {
        Expr::Num(_) | Expr::Ident(_) => 1,
        Expr::Unary(_, a) => check_depth(a)?,
        Expr::Binary(_, a, b) => {
            let na = check_depth(a)?;
            let nb = check_depth(b)?;
            na.max(1 + nb)
        }
        Expr::Call(_, args) => {
            let mut max_need = 1;
            for (j, arg) in args.iter().enumerate() {
                let n = check_depth(arg)?;
                max_need = max_need.max(j + n);
            }
            max_need
        }
    };
    if need > MAX_DEPTH {
        return Err(CompileError {
            kind: CompileErrorKind::NestingTooDeep,
            token: token_for(expr),
            position: 0,
        });
    }
    Ok(need)
}

/// A representative token string for an AST node, for depth-overflow diagnostics.
fn token_for(expr: &Expr) -> String {
    match expr {
        Expr::Num(v) => v.to_string(),
        Expr::Ident(n) => n.clone(),
        Expr::Unary(_, _) => "(unary)".to_owned(),
        Expr::Binary(_, _, _) => "(operator)".to_owned(),
        Expr::Call(f, _) => f.name.to_owned(),
    }
}

/// Lowers an AST into post-order bytecode, resolving identifiers to slot loads
/// or constant pushes. Operands are emitted before their operator (stack order).
fn emit(expr: &Expr, scope: &Scope, ops: &mut Vec<Op>) -> Result<(), CompileError> {
    match expr {
        Expr::Num(v) => {
            ops.push(Op::Const(*v));
            Ok(())
        }
        Expr::Ident(name) => {
            if let Some(slot) = scope.slot_id(name) {
                ops.push(Op::Load(slot.0));
                Ok(())
            } else if let Some(value) = lookup_constant(name) {
                ops.push(Op::Const(value));
                Ok(())
            } else {
                Err(CompileError {
                    kind: CompileErrorKind::UnknownIdentifier,
                    token: name.clone(),
                    position: 0,
                })
            }
        }
        Expr::Unary(op, a) => {
            emit(a, scope, ops)?;
            ops.push(Op::Unary(match op {
                UnaryOp::Neg => UnaryKind::Neg,
                UnaryOp::Not => UnaryKind::Not,
            }));
            Ok(())
        }
        Expr::Binary(op, a, b) => {
            emit(a, scope, ops)?;
            emit(b, scope, ops)?;
            ops.push(Op::Binary(to_binary_kind(*op)));
            Ok(())
        }
        Expr::Call(func, args) => {
            for arg in args {
                emit(arg, scope, ops)?;
            }
            ops.push(Op::Builtin(func.op));
            Ok(())
        }
    }
}

/// Maps a parser binary operator to its VM op kind.
fn to_binary_kind(op: crate::parser::BinaryOp) -> BinaryKind {
    use crate::parser::BinaryOp as B;
    match op {
        B::Add => BinaryKind::Add,
        B::Sub => BinaryKind::Sub,
        B::Mul => BinaryKind::Mul,
        B::Div => BinaryKind::Div,
        B::Rem => BinaryKind::Rem,
        B::Pow => BinaryKind::Pow,
        B::Lt => BinaryKind::Lt,
        B::Le => BinaryKind::Le,
        B::Gt => BinaryKind::Gt,
        B::Ge => BinaryKind::Ge,
        B::Eq => BinaryKind::Eq,
        B::Ne => BinaryKind::Ne,
        B::And => BinaryKind::And,
        B::Or => BinaryKind::Or,
    }
}
