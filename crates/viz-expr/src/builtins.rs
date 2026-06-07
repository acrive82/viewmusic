//! Built-in functions and constants (contract §2.2).
//!
//! A single source of truth shared by the parser (arity checking), the compiler
//! (op selection), the VM (evaluation), and the [`Scope`](crate::Scope) builder
//! (reserved-name rejection). Each built-in maps to one [`Op`](crate::vm::Op)
//! that the VM knows how to apply with NaN/±Inf sanitization.

/// A built-in function descriptor: its name, fixed arity, and the VM op it
/// lowers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinFn {
    /// Source name (e.g. `"clamp"`).
    pub name: &'static str,
    /// Fixed number of arguments.
    pub arity: usize,
    /// The VM operation this call lowers to.
    pub op: BuiltinOp,
}

/// The set of built-in function operations. Listed in arity-agnostic order; the
/// VM `match` pops the corresponding number of operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinOp {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Atan2,
    Exp,
    Log,
    Log2,
    Log10,
    Pow,
    Sqrt,
    Abs,
    Sign,
    Floor,
    Ceil,
    Round,
    Fract,
    Min,
    Max,
    Clamp,
    Mix,
    Smoothstep,
    Step,
    If,
    Band,
    Wave,
    Rand,
}

/// The complete built-in function table (contract §2.2). Order is irrelevant;
/// lookups are linear over this small fixed slice.
pub const BUILTINS: &[BuiltinFn] = &[
    BuiltinFn {
        name: "sin",
        arity: 1,
        op: BuiltinOp::Sin,
    },
    BuiltinFn {
        name: "cos",
        arity: 1,
        op: BuiltinOp::Cos,
    },
    BuiltinFn {
        name: "tan",
        arity: 1,
        op: BuiltinOp::Tan,
    },
    BuiltinFn {
        name: "asin",
        arity: 1,
        op: BuiltinOp::Asin,
    },
    BuiltinFn {
        name: "acos",
        arity: 1,
        op: BuiltinOp::Acos,
    },
    BuiltinFn {
        name: "atan",
        arity: 1,
        op: BuiltinOp::Atan,
    },
    BuiltinFn {
        name: "atan2",
        arity: 2,
        op: BuiltinOp::Atan2,
    },
    BuiltinFn {
        name: "exp",
        arity: 1,
        op: BuiltinOp::Exp,
    },
    BuiltinFn {
        name: "log",
        arity: 1,
        op: BuiltinOp::Log,
    },
    BuiltinFn {
        name: "log2",
        arity: 1,
        op: BuiltinOp::Log2,
    },
    BuiltinFn {
        name: "log10",
        arity: 1,
        op: BuiltinOp::Log10,
    },
    BuiltinFn {
        name: "pow",
        arity: 2,
        op: BuiltinOp::Pow,
    },
    BuiltinFn {
        name: "sqrt",
        arity: 1,
        op: BuiltinOp::Sqrt,
    },
    BuiltinFn {
        name: "abs",
        arity: 1,
        op: BuiltinOp::Abs,
    },
    BuiltinFn {
        name: "sign",
        arity: 1,
        op: BuiltinOp::Sign,
    },
    BuiltinFn {
        name: "floor",
        arity: 1,
        op: BuiltinOp::Floor,
    },
    BuiltinFn {
        name: "ceil",
        arity: 1,
        op: BuiltinOp::Ceil,
    },
    BuiltinFn {
        name: "round",
        arity: 1,
        op: BuiltinOp::Round,
    },
    BuiltinFn {
        name: "fract",
        arity: 1,
        op: BuiltinOp::Fract,
    },
    BuiltinFn {
        name: "min",
        arity: 2,
        op: BuiltinOp::Min,
    },
    BuiltinFn {
        name: "max",
        arity: 2,
        op: BuiltinOp::Max,
    },
    BuiltinFn {
        name: "clamp",
        arity: 3,
        op: BuiltinOp::Clamp,
    },
    BuiltinFn {
        name: "mix",
        arity: 3,
        op: BuiltinOp::Mix,
    },
    BuiltinFn {
        name: "smoothstep",
        arity: 3,
        op: BuiltinOp::Smoothstep,
    },
    BuiltinFn {
        name: "step",
        arity: 2,
        op: BuiltinOp::Step,
    },
    BuiltinFn {
        name: "if",
        arity: 3,
        op: BuiltinOp::If,
    },
    BuiltinFn {
        name: "band",
        arity: 1,
        op: BuiltinOp::Band,
    },
    BuiltinFn {
        name: "wave",
        arity: 1,
        op: BuiltinOp::Wave,
    },
    BuiltinFn {
        name: "rand",
        arity: 1,
        op: BuiltinOp::Rand,
    },
];

/// Built-in constants (contract §2.2): `pi`, `tau`, `e`.
pub const CONSTANTS: &[(&str, f64)] = &[
    ("pi", std::f64::consts::PI),
    ("tau", std::f64::consts::TAU),
    ("e", std::f64::consts::E),
];

/// Resolves a built-in function by name, or `None` if it is not a built-in.
pub fn lookup_builtin(name: &str) -> Option<BuiltinFn> {
    BUILTINS.iter().copied().find(|b| b.name == name)
}

/// Resolves a built-in constant by name, or `None`.
pub fn lookup_constant(name: &str) -> Option<f64> {
    CONSTANTS
        .iter()
        .copied()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v)
}

/// True if `name` is reserved by the language (a built-in function or constant).
/// Used by the [`Scope`](crate::Scope) builder to reject colliding slot names.
pub fn is_reserved(name: &str) -> bool {
    lookup_builtin(name).is_some() || lookup_constant(name).is_some()
}
