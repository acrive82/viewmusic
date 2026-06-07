//! Bytecode and the stack-machine evaluator.
//!
//! A [`Program`] is a flat `Vec<Op>` produced by [`compile`](crate::compile).
//! [`Vm::eval`] walks it over a fixed-size `[f64; STACK_SIZE]` stack with **zero
//! heap allocation** and **never panics**: the nesting cap (≤32) guarantees the
//! stack never overflows, and every op result is sanitized so that any non-finite
//! value (NaN, ±Inf) collapses to `0.0` at the step that produced it. Division by
//! zero, `log` of a non-positive number, `asin` out of range, etc. therefore all
//! degrade to `0.0` rather than corrupting downstream geometry/color.

use crate::builtins::BuiltinOp;
use viz_core::FeatureFrame;

/// Stack depth. The compiler enforces nesting ≤ 32; an expression tree of depth
/// `d` needs at most `d + 1` stack slots, so 64 is comfortably sufficient and
/// keeps the stack a fixed, cache-friendly array (no per-frame allocation).
pub const STACK_SIZE: usize = 64;

/// One bytecode instruction. Slot indices are resolved at compile time so the
/// hot loop performs no name lookups.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// Push a literal constant.
    Const(f64),
    /// Push the current value of slot `n`.
    Load(u16),
    /// Pop one operand, apply a unary operator, push the result.
    Unary(UnaryKind),
    /// Pop two operands (`a`, `b` with `b` on top), apply a binary operator.
    Binary(BinaryKind),
    /// Apply a built-in function to its operands on the stack.
    Builtin(BuiltinOp),
}

/// Unary VM operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryKind {
    /// Arithmetic negation.
    Neg,
    /// Logical NOT (truthy when operand `>= 0.5`; yields 0/1).
    Not,
}

/// Binary VM operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryKind {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

/// A compiled formula: flat bytecode plus the stack depth it requires.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub(crate) ops: Vec<Op>,
}

impl Program {
    /// Number of compiled operations (the cap-enforced metric — ≤ 256).
    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Read-only view of the bytecode (used by tests and downstream tooling).
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }
}

/// Sanitizes a value so the engine can never produce a non-finite result: any
/// non-finite value (NaN, ±Inf) becomes `0.0`.
///
/// Public so the property test's reference evaluator applies the identical
/// sanitization at every step and matches the VM bitwise.
#[inline]
pub fn sanitize(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

/// True if a scalar is "truthy" for logical operators (contract §2.1: `>= 0.5`).
#[inline]
fn truthy(x: f64) -> bool {
    x >= 0.5
}

/// The formula evaluator. Holds the slot bank, the latest [`FeatureFrame`] for
/// `band()`/`wave()`, and the artifact seed for `rand()`. One `Vm` is reused
/// across frames and elements (no per-frame allocation): set
/// the shared/element slots and the frame, then call [`Vm::eval`] repeatedly.
pub struct Vm {
    /// Slot bank: one f64 per declared identifier in the [`Scope`](crate::Scope).
    slots: Vec<f64>,
    /// Latest audio features (powers `band(u)`/`wave(u)`).
    frame: FeatureFrame,
    /// Artifact seed for deterministic `rand(k)`.
    seed: u64,
    /// Fixed-size evaluation stack (never heap-allocated per call).
    stack: [f64; STACK_SIZE],
}

impl Vm {
    /// Creates a VM with `slot_count` zeroed slots and the artifact `seed`.
    pub fn new(slot_count: usize, seed: u64) -> Self {
        Self {
            slots: vec![0.0; slot_count],
            frame: FeatureFrame::default(),
            seed,
            stack: [0.0; STACK_SIZE],
        }
    }

    /// Number of slots this VM was constructed with.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Writes a slot value. Out-of-range ids are ignored (defensive; the
    /// compiler only ever emits in-range ids).
    pub fn set_slot(&mut self, slot: crate::SlotId, value: f64) {
        if let Some(s) = self.slots.get_mut(slot.0 as usize) {
            *s = value;
        }
    }

    /// Reads a slot value (0.0 if the id is out of range).
    pub fn get_slot(&self, slot: crate::SlotId) -> f64 {
        self.slots.get(slot.0 as usize).copied().unwrap_or(0.0)
    }

    /// Installs the latest audio feature frame (POD copy). Powers `band()` and
    /// `wave()` for subsequent evaluations.
    pub fn set_frame(&mut self, frame: &FeatureFrame) {
        self.frame = *frame;
    }

    /// Deterministic stateless `rand(k)`: a splitmix64-style hash mixing the
    /// artifact `seed` with `k.to_bits()`, finished into a uniform value in
    /// `[0, 1)`. Same `(seed, k)` always yields the same value — stable across
    /// frames, elements, and VM instances unless `k` changes. Public so the
    /// reference evaluator reproduces it exactly.
    #[inline]
    pub fn rand(&self, k: f64) -> f64 {
        // Normalize the bit pattern so that -0.0 and +0.0 (and any NaN) map to a
        // single stable key: callers reason about `rand(k)` numerically, not by
        // bit pattern.
        let key = if k == 0.0 { 0.0 } else { k };
        let kbits = if key.is_nan() { 0 } else { key.to_bits() };
        let mut z = self.seed ^ kbits.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // splitmix64 finisher.
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Top 53 bits → uniform [0, 1) (the standard f64 construction).
        ((z >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// Evaluates a compiled [`Program`] against the current slots/frame/seed.
    /// Returns the single result, sanitized to a finite number. Never panics:
    /// the stack depth is statically bounded by the compiler's nesting cap, and
    /// a malformed (non-net-+1) program degrades to `0.0` rather than indexing
    /// out of bounds.
    pub fn eval(&mut self, program: &Program) -> f64 {
        let mut sp: usize = 0;
        for op in &program.ops {
            match op {
                Op::Const(v) => {
                    if sp < STACK_SIZE {
                        self.stack[sp] = sanitize(*v);
                        sp += 1;
                    }
                }
                Op::Load(n) => {
                    let v = self.slots.get(*n as usize).copied().unwrap_or(0.0);
                    if sp < STACK_SIZE {
                        self.stack[sp] = sanitize(v);
                        sp += 1;
                    }
                }
                Op::Unary(kind) => {
                    if sp < 1 {
                        return 0.0;
                    }
                    let a = self.stack[sp - 1];
                    self.stack[sp - 1] = sanitize(apply_unary(*kind, a));
                }
                Op::Binary(kind) => {
                    if sp < 2 {
                        return 0.0;
                    }
                    let b = self.stack[sp - 1];
                    let a = self.stack[sp - 2];
                    sp -= 1;
                    self.stack[sp - 1] = sanitize(apply_binary(*kind, a, b));
                }
                Op::Builtin(b) => {
                    let arity = builtin_arity(*b);
                    if sp < arity {
                        return 0.0;
                    }
                    // Operands occupy stack[sp-arity .. sp]; result replaces them.
                    let base = sp - arity;
                    // Copy into a fixed 3-slot array (max builtin arity) — no heap.
                    let mut args = [0.0f64; 3];
                    args[..arity].copy_from_slice(&self.stack[base..base + arity]);
                    let result = self.apply_builtin(*b, &args[..arity]);
                    self.stack[base] = sanitize(result);
                    sp = base + 1;
                }
            }
        }
        if sp == 0 {
            0.0
        } else {
            sanitize(self.stack[sp - 1])
        }
    }

    /// Applies a built-in to its operand slice `a` (length == the op's arity).
    /// All inputs are already sanitized (finite); the caller sanitizes the
    /// result. Public so the property test's reference evaluator uses identical
    /// semantics, including the frame-dependent `band`/`wave`/`rand`.
    #[inline]
    pub fn apply_builtin(&self, b: BuiltinOp, a: &[f64]) -> f64 {
        match b {
            BuiltinOp::Sin => a[0].sin(),
            BuiltinOp::Cos => a[0].cos(),
            BuiltinOp::Tan => a[0].tan(),
            BuiltinOp::Asin => a[0].asin(),
            BuiltinOp::Acos => a[0].acos(),
            BuiltinOp::Atan => a[0].atan(),
            BuiltinOp::Atan2 => a[0].atan2(a[1]),
            BuiltinOp::Exp => a[0].exp(),
            BuiltinOp::Log => a[0].ln(),
            BuiltinOp::Log2 => a[0].log2(),
            BuiltinOp::Log10 => a[0].log10(),
            BuiltinOp::Pow => a[0].powf(a[1]),
            BuiltinOp::Sqrt => a[0].sqrt(),
            BuiltinOp::Abs => a[0].abs(),
            BuiltinOp::Sign => sign(a[0]),
            BuiltinOp::Floor => a[0].floor(),
            BuiltinOp::Ceil => a[0].ceil(),
            BuiltinOp::Round => a[0].round(),
            BuiltinOp::Fract => a[0].fract(),
            BuiltinOp::Min => a[0].min(a[1]),
            BuiltinOp::Max => a[0].max(a[1]),
            BuiltinOp::Clamp => clamp(a[0], a[1], a[2]),
            BuiltinOp::Mix => mix(a[0], a[1], a[2]),
            BuiltinOp::Smoothstep => smoothstep(a[0], a[1], a[2]),
            BuiltinOp::Step => step(a[0], a[1]),
            BuiltinOp::If => {
                // Both branches are already evaluated (pure dataflow, §2.2).
                if truthy(a[0]) {
                    a[1]
                } else {
                    a[2]
                }
            }
            BuiltinOp::Band => self.frame.band(a[0]),
            BuiltinOp::Wave => self.frame.wave(a[0]),
            BuiltinOp::Rand => self.rand(a[0]),
        }
    }
}

/// Arity of a built-in op (mirrors the table in `builtins`; kept local so the VM
/// has no name lookups in the hot path). Public for the reference evaluator.
#[inline]
pub fn builtin_arity(b: BuiltinOp) -> usize {
    match b {
        BuiltinOp::Atan2 | BuiltinOp::Pow | BuiltinOp::Min | BuiltinOp::Max | BuiltinOp::Step => 2,
        BuiltinOp::Clamp | BuiltinOp::Mix | BuiltinOp::Smoothstep | BuiltinOp::If => 3,
        _ => 1,
    }
}

/// Applies a unary operator (result sanitized by the caller). Public so the
/// reference evaluator shares the exact semantics.
#[inline]
pub fn apply_unary(kind: UnaryKind, a: f64) -> f64 {
    match kind {
        UnaryKind::Neg => -a,
        UnaryKind::Not => bool_val(!truthy(a)),
    }
}

/// Applies a binary operator (result sanitized by the caller). Public so the
/// reference evaluator shares the exact semantics.
#[inline]
pub fn apply_binary(kind: BinaryKind, a: f64, b: f64) -> f64 {
    match kind {
        BinaryKind::Add => a + b,
        BinaryKind::Sub => a - b,
        BinaryKind::Mul => a * b,
        BinaryKind::Div => a / b,
        BinaryKind::Rem => a % b,
        BinaryKind::Pow => a.powf(b),
        BinaryKind::Lt => bool_val(a < b),
        BinaryKind::Le => bool_val(a <= b),
        BinaryKind::Gt => bool_val(a > b),
        BinaryKind::Ge => bool_val(a >= b),
        BinaryKind::Eq => bool_val(a == b),
        BinaryKind::Ne => bool_val(a != b),
        BinaryKind::And => bool_val(truthy(a) && truthy(b)),
        BinaryKind::Or => bool_val(truthy(a) || truthy(b)),
    }
}

/// Maps a boolean to the contract's 0.0/1.0 numeric truth values.
#[inline]
fn bool_val(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

/// `sign(x)`: −1, 0, or +1. `sign(0) == 0`; non-finite handled by sanitization.
#[inline]
fn sign(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// `clamp(x, lo, hi)`. If `lo > hi` the result follows `x.clamp` semantics after
/// ordering the bounds, so a degenerate range never panics.
#[inline]
fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    x.clamp(lo, hi)
}

/// `mix(a, b, k)`: linear interpolation `a + (b - a) * k`.
#[inline]
fn mix(a: f64, b: f64, k: f64) -> f64 {
    a + (b - a) * k
}

/// `smoothstep(e0, e1, x)`: 0 below `e0`, 1 above `e1`, Hermite in between.
#[inline]
fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let denom = e1 - e0;
    if denom == 0.0 {
        // Degenerate edges: behave like a hard step at the shared edge.
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / denom).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `step(edge, x)`: 0 when `x < edge`, else 1.
#[inline]
fn step(edge: f64, x: f64) -> f64 {
    if x < edge {
        0.0
    } else {
        1.0
    }
}
