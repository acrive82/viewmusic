//! viz-expr — the ViewMusic formula language: lexer, Pratt parser, bytecode
//! compiler, and a zero-allocation, never-panic stack-machine VM.
//!
//! This crate implements the expression language of the artifact contract
//! (`docs/reference/artifact-contract.md` §2) used
//! for every geometry, color, and state formula. A formula string is compiled
//! once at artifact load into a flat [`Program`] of bytecode [`Op`]s, then
//! evaluated thousands of times per frame by a reusable [`Vm`] that performs no
//! heap allocation in the hot path (the per-frame path must never allocate or
//! block) and never panics.
//!
//! # Language summary (contract §2)
//!
//! - **Operators**: `+ - * / %`, `^` (power, right-associative), unary `-`,
//!   comparisons `< <= > >= == !=` (yield `0`/`1`), logical `&& || !` (operands
//!   truthy when `>= 0.5`; yield `0`/`1`), and parentheses.
//! - **Functions**: `sin cos tan asin acos atan atan2 exp log log2 log10 pow
//!   sqrt abs sign floor ceil round fract min max clamp mix smoothstep step if
//!   band wave rand`. Constants: `pi tau e`. Function arity is checked at
//!   parse/compile time. `if(cond, then, else)` evaluates **both** branches
//!   (pure dataflow, contract §2.2) and selects on `cond >= 0.5`.
//! - **Numbers**: JSON-style decimal literals (`12`, `1.5`, `.5`, `2.5e-3`).
//! - **Identifiers**: `[a-zA-Z_][a-zA-Z0-9_]*` optionally followed by a single
//!   `.ident` suffix — `settings.bars` is one atomic identifier token.
//!
//! # Precedence (lowest → highest binds tighter)
//!
//! `||` < `&&` < comparisons < `+ -` < `* / %` < unary `-` `!` < `^` <
//! call/primary.
//!
//! **Unary minus binds looser than power** — a deliberate, documented choice
//! (and the one the property test's reference evaluator matches): `-2^2` parses
//! as `-(2^2)` and evaluates to `-4` (not `4`). `2+3*4 == 14`, `1<2 && 3>2 == 1`,
//! `if(0.4, 1, 2) == 2`, `if(0.6, 1, 2) == 1`.
//!
//! # NaN / error containment
//!
//! Every op result is sanitized: any non-finite value (NaN, ±Inf) collapses to
//! `0.0` at the step that produced it. Division by zero, `log` of a non-positive
//! number, `asin` out of range, `sqrt` of a negative, `(-1)^0.5`, `0/0`, etc. all
//! evaluate to a finite number. A faulty formula degrades visibly but can never
//! crash the app.
//!
//! # Example
//!
//! ```
//! use viz_expr::{compile, Scope, Vm};
//!
//! let mut b = Scope::builder();
//! let t = b.slot("t").unwrap();
//! let scope = b.build();
//!
//! let program = compile("sin(t) * 0.5 + 0.5", &scope).unwrap();
//!
//! let mut vm = Vm::new(scope.slot_count(), /* seed */ 1234);
//! vm.set_slot(t, 0.0);
//! assert!((vm.eval(&program) - 0.5).abs() < 1e-12);
//! ```

mod builtins;
mod compile;
mod lexer;
mod parser;
mod vm;

// Built-in tables — exposed for downstream tooling (e.g. schema/doc generation)
// and so callers can mirror the reserved-name set when building scopes.
pub use builtins::{
    is_reserved, lookup_builtin, lookup_constant, BuiltinFn, BuiltinOp, BUILTINS, CONSTANTS,
};

// Compilation surface.
pub use compile::{
    compile, CompileError, CompileErrorKind, Scope, ScopeBuilder, ScopeError, SlotId, MAX_DEPTH,
    MAX_OPS, MAX_SOURCE_LEN,
};

// AST — public so a reference evaluator (and downstream analyses) can walk the
// exact shape the compiler lowers.
pub use parser::{parse, BinaryOp, Expr, ParseError, ParseErrorKind, UnaryOp};

// Lexer — public so tooling can tokenize independently.
pub use lexer::{lex, LexError, Token, TokenKind};

// VM and bytecode. The scalar-application helpers are public so the property
// test's reference evaluator reproduces the VM's semantics bit-for-bit.
pub use vm::{
    apply_binary, apply_unary, builtin_arity, sanitize, BinaryKind, Op, Program, UnaryKind, Vm,
    STACK_SIZE,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a scope with the common shared inputs used across tests.
    fn test_scope() -> Scope {
        let mut b = Scope::builder();
        for name in [
            "t",
            "dt",
            "energy",
            "low",
            "mid",
            "high",
            "i",
            "u",
            "n",
            "settings.bars",
        ] {
            b.slot(name).unwrap();
        }
        b.build()
    }

    fn eval(src: &str) -> f64 {
        let scope = test_scope();
        let program = compile(src, &scope).expect("compile");
        let mut vm = Vm::new(scope.slot_count(), 0);
        vm.eval(&program)
    }

    #[test]
    fn precedence_arithmetic() {
        assert_eq!(eval("2+3*4"), 14.0);
        assert_eq!(eval("(2+3)*4"), 20.0);
    }

    #[test]
    fn unary_binds_looser_than_power() {
        // Documented choice: -2^2 == -(2^2) == -4.
        assert_eq!(eval("-2^2"), -4.0);
        // Power is right-associative: 2^3^2 == 2^(3^2) == 512.
        assert_eq!(eval("2^3^2"), 512.0);
    }

    #[test]
    fn logical_and_comparison_precedence() {
        assert_eq!(eval("1<2&&3>2"), 1.0);
        assert_eq!(eval("1>2||3>2"), 1.0);
        assert_eq!(eval("1>2&&3>2"), 0.0);
        // Comparisons bind tighter than &&: 1<2 && 3>2 groups as (1<2)&&(3>2).
        assert_eq!(eval("0||1&&1"), 1.0); // || looser than &&
    }

    #[test]
    fn if_selects_on_half_threshold() {
        assert_eq!(eval("if(0.4,1,2)"), 2.0);
        assert_eq!(eval("if(0.6,1,2)"), 1.0);
        assert_eq!(eval("if(0.5,1,2)"), 1.0); // >= 0.5 is true
    }

    #[test]
    fn logical_not_and_truthiness() {
        assert_eq!(eval("!0"), 1.0);
        assert_eq!(eval("!1"), 0.0);
        assert_eq!(eval("!0.4"), 1.0);
        assert_eq!(eval("!0.6"), 0.0);
    }

    #[test]
    fn nan_inf_contained_to_zero() {
        // Every one of these is non-finite at some op and must collapse to 0.
        assert_eq!(eval("1/0"), 0.0);
        assert_eq!(eval("0/0"), 0.0);
        assert_eq!(eval("log(-1)"), 0.0);
        assert_eq!(eval("asin(2)"), 0.0);
        assert_eq!(eval("sqrt(-1)"), 0.0);
        assert_eq!(eval("(-1)^0.5"), 0.0);
        assert!(eval("1/0").is_finite());
    }

    #[test]
    fn pow_and_caret_are_equivalent() {
        assert_eq!(eval("2^10"), eval("pow(2,10)"));
        assert_eq!(eval("3^3"), 27.0);
    }

    #[test]
    fn rem_is_f64_remainder() {
        assert_eq!(eval("7%3"), 1.0);
        assert_eq!(eval("(-7)%3"), -1.0);
        // x % 0 is NaN → sanitized to 0.
        assert_eq!(eval("5%0"), 0.0);
    }

    #[test]
    fn constants_resolve() {
        assert_eq!(eval("pi"), std::f64::consts::PI);
        assert_eq!(eval("tau"), std::f64::consts::TAU);
        assert_eq!(eval("e"), std::f64::consts::E);
    }

    #[test]
    fn shaping_functions() {
        assert_eq!(eval("clamp(5, 0, 1)"), 1.0);
        assert_eq!(eval("clamp(-5, 0, 1)"), 0.0);
        assert_eq!(eval("mix(0, 10, 0.5)"), 5.0);
        assert_eq!(eval("step(0.5, 0.7)"), 1.0);
        assert_eq!(eval("step(0.5, 0.3)"), 0.0);
        assert_eq!(eval("smoothstep(0, 1, 0.5)"), 0.5);
        assert_eq!(eval("sign(-3)"), -1.0);
        assert_eq!(eval("sign(0)"), 0.0);
        assert_eq!(eval("min(2,5)"), 2.0);
        assert_eq!(eval("max(2,5)"), 5.0);
        assert_eq!(eval("abs(-4)"), 4.0);
        assert_eq!(eval("floor(2.7)"), 2.0);
        assert_eq!(eval("ceil(2.2)"), 3.0);
        assert_eq!(eval("round(2.5)"), 3.0);
    }

    #[test]
    fn slots_load() {
        let scope = test_scope();
        let program = compile("t * 2 + energy", &scope).unwrap();
        let mut vm = Vm::new(scope.slot_count(), 0);
        vm.set_slot(scope.slot_id("t").unwrap(), 3.0);
        vm.set_slot(scope.slot_id("energy").unwrap(), 0.5);
        assert_eq!(vm.eval(&program), 6.5);
    }

    #[test]
    fn atomic_dotted_identifier() {
        let scope = test_scope();
        let program = compile("settings.bars * 2", &scope).unwrap();
        let mut vm = Vm::new(scope.slot_count(), 0);
        vm.set_slot(scope.slot_id("settings.bars").unwrap(), 16.0);
        assert_eq!(vm.eval(&program), 32.0);
        // The lexer treats `settings.bars` as ONE token: a single ident.
        let toks = lex("settings.bars").unwrap();
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Ident("settings.bars".to_owned()));
    }

    #[test]
    fn band_wave_use_frame() {
        use viz_core::FeatureFrame;
        let scope = test_scope();
        let prog = compile("band(0) + wave(0)", &scope).unwrap();
        let mut frame = FeatureFrame::default();
        frame.bands[0] = 0.25;
        frame.waveform[0] = -0.5;
        let mut vm = Vm::new(scope.slot_count(), 0);
        vm.set_frame(&frame);
        assert!((vm.eval(&prog) - (0.25 - 0.5)).abs() < 1e-7);
    }

    // ---- Caps ------------------------------------------------------------

    #[test]
    fn cap_source_too_long_rejected() {
        let scope = test_scope();
        // 1025 characters → rejected (length is checked before parsing).
        let too_long = format!("1{}", " ".repeat(1024)); // 1 + 1024 = 1025 chars
        assert_eq!(too_long.chars().count(), 1025);
        let err = compile(&too_long, &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::SourceTooLong);

        // Exactly 1024 chars (trailing whitespace, one op) is accepted.
        let ok_len = format!("1{}", " ".repeat(1023));
        assert_eq!(ok_len.chars().count(), MAX_SOURCE_LEN);
        assert!(compile(&ok_len, &scope).is_ok());
    }

    #[test]
    fn cap_nesting_too_deep_rejected() {
        let scope = test_scope();
        // Nesting is measured as required evaluation-stack depth. A right-
        // associative power chain `2^2^…^2` nests on the right: depth = (#^) + 1.
        // 32 carets → depth 33 > MAX_DEPTH (32) → rejected.
        let deep = format!("2{}", "^2".repeat(32));
        let err = compile(&deep, &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::NestingTooDeep);

        // 31 carets → depth 32, accepted.
        let ok = format!("2{}", "^2".repeat(31));
        assert!(compile(&ok, &scope).is_ok());

        // A long LEFT-associative chain stays shallow (depth 2) and is NOT a
        // nesting violation regardless of length (it is bounded by the op cap).
        let flat = format!("1{}", "+1".repeat(100));
        assert!(compile(&flat, &scope).is_ok());
    }

    #[test]
    fn cap_too_many_ops_rejected() {
        let scope = test_scope();
        // A long flat sum: "1+1+1+..." . Each "+1" adds a Const and a Binary op,
        // plus the leading Const. 128 "+1" → 1 + 256 = 257 ops > MAX_OPS (256).
        let big = format!("1{}", "+1".repeat(128));
        let err = compile(&big, &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::TooManyOps);

        // 127 "+1" → 1 + 254 = 255 ops, accepted.
        let ok = format!("1{}", "+1".repeat(127));
        let prog = compile(&ok, &scope).unwrap();
        assert!(prog.op_count() <= MAX_OPS);
    }

    // ---- Errors ----------------------------------------------------------

    #[test]
    fn arity_error_named() {
        let scope = test_scope();
        let err = compile("clamp(1, 2)", &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::Parse);
        assert_eq!(err.token, "clamp"); // the diagnostic names the offending token

        let err2 = compile("sin(1, 2)", &scope).unwrap_err();
        assert_eq!(err2.kind, CompileErrorKind::Parse);
        assert_eq!(err2.token, "sin");
    }

    #[test]
    fn unknown_identifier_named() {
        let scope = test_scope();
        let err = compile("bogus + 1", &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::UnknownIdentifier);
        assert_eq!(err.token, "bogus");
    }

    #[test]
    fn unknown_function_named() {
        let scope = test_scope();
        let err = compile("frobnicate(1)", &scope).unwrap_err();
        assert_eq!(err.kind, CompileErrorKind::Parse);
        assert_eq!(err.token, "frobnicate");
    }

    #[test]
    fn syntax_error_does_not_panic() {
        let scope = test_scope();
        assert!(compile("2 +", &scope).is_err());
        assert!(compile("(1 + 2", &scope).is_err());
        assert!(compile("1 2", &scope).is_err());
        assert!(compile("* 3", &scope).is_err());
        assert!(compile("", &scope).is_err());
    }

    // ---- Scope builder ---------------------------------------------------

    #[test]
    fn scope_rejects_duplicate_and_reserved() {
        let mut b = Scope::builder();
        assert!(b.slot("foo").is_ok());
        assert!(matches!(b.slot("foo"), Err(ScopeError::Duplicate(_))));
        assert!(matches!(b.slot("sin"), Err(ScopeError::ReservedName(_))));
        assert!(matches!(b.slot("pi"), Err(ScopeError::ReservedName(_))));
        assert!(matches!(b.slot(""), Err(ScopeError::EmptyName)));
    }

    #[test]
    fn slot_ids_are_dense_and_ordered() {
        let mut b = Scope::builder();
        assert_eq!(b.slot("a").unwrap(), SlotId(0));
        assert_eq!(b.slot("b").unwrap(), SlotId(1));
        assert_eq!(b.slot("c").unwrap(), SlotId(2));
        let scope = b.build();
        assert_eq!(scope.slot_count(), 3);
        assert_eq!(scope.slot_id("b"), Some(SlotId(1)));
        assert_eq!(scope.slot_id("missing"), None);
    }

    // ---- rand determinism ------------------------------------------------

    #[test]
    fn rand_is_deterministic_and_in_range() {
        let scope = test_scope();
        let prog = compile("rand(7)", &scope).unwrap();
        let mut a = Vm::new(scope.slot_count(), 42);
        let mut b = Vm::new(scope.slot_count(), 42);
        let va = a.eval(&prog);
        let vb = b.eval(&prog);
        assert_eq!(va, vb); // same seed, same k, same value across instances
        assert_eq!(va, a.eval(&prog)); // stable across evals
        assert!((0.0..1.0).contains(&va));

        // Different k differs (with overwhelming probability for this hash).
        let prog2 = compile("rand(8)", &scope).unwrap();
        assert_ne!(va, a.eval(&prog2));

        // Different seed differs.
        let mut c = Vm::new(scope.slot_count(), 43);
        assert_ne!(va, c.eval(&prog));
    }
}
