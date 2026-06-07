//! Property tests for the formula engine.
//!
//! Strategy:
//! 1. **VM vs reference.** Generate random depth-bounded expression ASTs that
//!    reference only a known scope, render each to fully-parenthesized source,
//!    compile it, and compare [`Vm::eval`] against a straightforward AST-walking
//!    reference evaluator. The reference reuses the crate's *public* scalar
//!    helpers (`apply_unary`, `apply_binary`, `Vm::apply_builtin`, `Vm::rand`,
//!    `sanitize`) so the two must agree **bitwise** — including on special values
//!    (NaN/±Inf inputs, division by zero, etc.).
//! 2. **Parser robustness.** Random garbage strings never panic the lexer/parser
//!    (they return an error or a valid program — never unwind).
//! 3. **NaN containment.** Explicit cases (`1/0`, `log(-1)`, `asin(2)`, `0/0`,
//!    `(-1)^0.5`) all evaluate to a finite number.
//! 4. **rand determinism.** Same `(seed, k)` is equal across VM instances and
//!    repeated evals; different `k` differs.
//! 5. **Precedence sanity.** The contract's worked examples.

use proptest::prelude::*;
use viz_core::FeatureFrame;
use viz_expr::{
    apply_binary, apply_unary, compile, sanitize, BinaryOp, Expr, Scope, SlotId, UnaryOp, Vm,
};

/// The fixed identifier set the generated ASTs may reference. Mirrors a typical
/// element-stage scope (shared inputs + stage extras + a dotted setting).
const SLOT_NAMES: &[&str] = &[
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
];

/// Built-in constants the generator may emit as identifiers.
const CONST_NAMES: &[&str] = &["pi", "tau", "e"];

fn make_scope() -> Scope {
    let mut b = Scope::builder();
    for name in SLOT_NAMES {
        b.slot(name).unwrap();
    }
    b.build()
}

// ---------------------------------------------------------------------------
// AST generation
// ---------------------------------------------------------------------------

/// Function descriptor for generation: name + arity.
const GEN_FUNCS: &[(&str, usize)] = &[
    ("sin", 1),
    ("cos", 1),
    ("tan", 1),
    ("asin", 1),
    ("acos", 1),
    ("atan", 1),
    ("atan2", 2),
    ("exp", 1),
    ("log", 1),
    ("log2", 1),
    ("log10", 1),
    ("pow", 2),
    ("sqrt", 1),
    ("abs", 1),
    ("sign", 1),
    ("floor", 1),
    ("ceil", 1),
    ("round", 1),
    ("fract", 1),
    ("min", 2),
    ("max", 2),
    ("clamp", 3),
    ("mix", 3),
    ("smoothstep", 3),
    ("step", 2),
    ("if", 3),
    ("band", 1),
    ("wave", 1),
    ("rand", 1),
];

const GEN_BINOPS: &[BinaryOp] = &[
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Rem,
    BinaryOp::Pow,
    BinaryOp::Lt,
    BinaryOp::Le,
    BinaryOp::Gt,
    BinaryOp::Ge,
    BinaryOp::Eq,
    BinaryOp::Ne,
    BinaryOp::And,
    BinaryOp::Or,
];

/// A recursive AST strategy, depth-bounded so the rendered source stays within
/// the compile caps (length, nesting, op count).
fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        // Numeric literals, including a few special / extreme values.
        prop_oneof![
            (-1000.0f64..1000.0).prop_map(Expr::Num),
            Just(Expr::Num(0.0)),
            Just(Expr::Num(1.0)),
            Just(Expr::Num(-1.0)),
            Just(Expr::Num(0.5)),
            Just(Expr::Num(2.0)),
            Just(Expr::Num(1e9)),
            Just(Expr::Num(1e-9)),
        ],
        // Slot identifiers.
        (0..SLOT_NAMES.len()).prop_map(|i| Expr::Ident(SLOT_NAMES[i].to_owned())),
        // Constant identifiers.
        (0..CONST_NAMES.len()).prop_map(|i| Expr::Ident(CONST_NAMES[i].to_owned())),
    ];

    leaf.prop_recursive(
        6,   // up to 6 levels deep
        128, // up to ~128 total nodes
        3,   // up to 3 children per node
        |inner| {
            prop_oneof![
                // Unary.
                (
                    prop_oneof![Just(UnaryOp::Neg), Just(UnaryOp::Not)],
                    inner.clone(),
                )
                    .prop_map(|(op, a)| Expr::Unary(op, Box::new(a))),
                // Binary.
                ((0..GEN_BINOPS.len()), inner.clone(), inner.clone(),).prop_map(|(oi, a, b)| {
                    Expr::Binary(GEN_BINOPS[oi], Box::new(a), Box::new(b))
                }),
                // Function call (arity-correct by construction).
                (0..GEN_FUNCS.len()).prop_flat_map(move |fi| {
                    let (name, arity) = GEN_FUNCS[fi];
                    let func = viz_expr::lookup_builtin(name).unwrap();
                    proptest::collection::vec(inner.clone(), arity)
                        .prop_map(move |args| Expr::Call(func, args))
                }),
            ]
        },
    )
}

// ---------------------------------------------------------------------------
// Source rendering (fully parenthesized → unambiguous re-parse)
// ---------------------------------------------------------------------------

fn render(expr: &Expr, out: &mut String) {
    match expr {
        Expr::Num(v) => render_num(*v, out),
        Expr::Ident(name) => out.push_str(name),
        Expr::Unary(op, a) => {
            out.push('(');
            out.push(match op {
                UnaryOp::Neg => '-',
                UnaryOp::Not => '!',
            });
            out.push('(');
            render(a, out);
            out.push(')');
            out.push(')');
        }
        Expr::Binary(op, a, b) => {
            out.push('(');
            render(a, out);
            out.push_str(binop_str(*op));
            render(b, out);
            out.push(')');
        }
        Expr::Call(func, args) => {
            out.push_str(func.name);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render(arg, out);
            }
            out.push(')');
        }
    }
}

/// Renders a number so it round-trips exactly through the f64 parser **under the
/// engine's precedence**. The grammar has no negative literals — a leading `-`
/// is a unary operator that binds looser than `^` — so a negative value must be
/// rendered as a fully-parenthesized unary negation `(-(mag))`; otherwise
/// `-2.0^0.5` would re-parse as `-(2.0^0.5)`. Non-finite generator outputs (there
/// are none, but be defensive) render as `0`.
fn render_num(v: f64, out: &mut String) {
    if !v.is_finite() {
        out.push('0');
        return;
    }
    if v.is_sign_negative() {
        // Covers negative values and -0.0; wrap so re-parse yields the same atom.
        out.push_str("(-(");
        // `{:?}` emits a round-trippable representation (e.g. "1.0", "1e-9").
        out.push_str(&format!("{:?}", v.abs()));
        out.push_str("))");
    } else {
        out.push_str(&format!("{v:?}"));
    }
}

fn binop_str(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Pow => "^",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

// ---------------------------------------------------------------------------
// Reference evaluator — must match the VM bitwise
// ---------------------------------------------------------------------------

/// Walks the AST applying the crate's public scalar helpers, with the same
/// per-step sanitization the VM performs.
fn reference_eval(expr: &Expr, scope: &Scope, slots: &[f64], vm: &Vm) -> f64 {
    let raw = match expr {
        Expr::Num(v) => *v,
        Expr::Ident(name) => {
            if let Some(SlotId(idx)) = scope.slot_id(name) {
                slots[idx as usize]
            } else {
                // Generator only emits known identifiers; unknown → 0.
                viz_expr::lookup_constant(name).unwrap_or(0.0)
            }
        }
        Expr::Unary(op, a) => {
            let av = reference_eval(a, scope, slots, vm);
            let kind = match op {
                UnaryOp::Neg => viz_expr::UnaryKind::Neg,
                UnaryOp::Not => viz_expr::UnaryKind::Not,
            };
            apply_unary(kind, av)
        }
        Expr::Binary(op, a, b) => {
            let av = reference_eval(a, scope, slots, vm);
            let bv = reference_eval(b, scope, slots, vm);
            apply_binary(to_kind(*op), av, bv)
        }
        Expr::Call(func, args) => {
            let mut argv = [0.0f64; 3];
            for (i, arg) in args.iter().enumerate() {
                argv[i] = reference_eval(arg, scope, slots, vm);
            }
            vm.apply_builtin(func.op, &argv[..args.len()])
        }
    };
    sanitize(raw)
}

fn to_kind(op: BinaryOp) -> viz_expr::BinaryKind {
    use viz_expr::BinaryKind as K;
    match op {
        BinaryOp::Add => K::Add,
        BinaryOp::Sub => K::Sub,
        BinaryOp::Mul => K::Mul,
        BinaryOp::Div => K::Div,
        BinaryOp::Rem => K::Rem,
        BinaryOp::Pow => K::Pow,
        BinaryOp::Lt => K::Lt,
        BinaryOp::Le => K::Le,
        BinaryOp::Gt => K::Gt,
        BinaryOp::Ge => K::Ge,
        BinaryOp::Eq => K::Eq,
        BinaryOp::Ne => K::Ne,
        BinaryOp::And => K::And,
        BinaryOp::Or => K::Or,
    }
}

/// A frame with non-trivial band/wave content so `band()`/`wave()` are exercised.
fn make_frame() -> FeatureFrame {
    let mut f = FeatureFrame::default();
    for (k, b) in f.bands.iter_mut().enumerate() {
        *b = (k as f32 / 47.0).sin().abs();
    }
    for (k, w) in f.waveform.iter_mut().enumerate() {
        *w = (k as f32 / 255.0 * std::f32::consts::TAU).sin();
    }
    f
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// (1) VM matches the reference AST walker bitwise for random expressions and
    /// random slot/frame inputs (including special values).
    #[test]
    fn vm_matches_reference(
        expr in arb_expr(),
        slot_vals in proptest::collection::vec(
            prop_oneof![
                (-1000.0f64..1000.0),
                Just(0.0),
                Just(1.0),
                Just(-1.0),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
                Just(f64::NAN),
            ],
            SLOT_NAMES.len()
        ),
        seed in any::<u64>(),
    ) {
        let scope = make_scope();
        let mut src = String::new();
        render(&expr, &mut src);

        // The fully-parenthesized source may, for the deepest generated trees,
        // exceed a cap; if compilation is rejected we simply skip (the generator
        // is not constrained to the caps — caps are tested separately).
        let program = match compile(&src, &scope) {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };

        let frame = make_frame();

        let mut vm = Vm::new(scope.slot_count(), seed);
        vm.set_frame(&frame);
        for (i, v) in slot_vals.iter().enumerate() {
            vm.set_slot(SlotId(i as u16), *v);
        }
        let got = vm.eval(&program);

        // Reference uses the SAME vm for frame/seed-dependent builtins.
        let want = reference_eval(&expr, &scope, &slot_vals, &vm);

        // Bitwise equality (both are sanitized → always finite, so no NaN!=NaN).
        prop_assert_eq!(got.to_bits(), want.to_bits(),
            "src={} got={} want={}", src, got, want);
    }

    /// (2) The parser never panics on arbitrary input — it returns Ok or Err.
    #[test]
    fn parser_never_panics_on_garbage(s in ".{0,200}") {
        let scope = make_scope();
        // Just must not panic/unwind. Result is irrelevant.
        let _ = compile(&s, &scope);
    }

    /// (2b) The parser never panics on strings drawn from the language alphabet
    /// (stresses the grammar far more than fully-random unicode).
    #[test]
    fn parser_never_panics_on_tokenish(
        s in "[-+*/%^()<>=!&|,a-z0-9. ]{0,120}"
    ) {
        let scope = make_scope();
        let _ = compile(&s, &scope);
    }
}

/// (3) Explicit NaN-containment cases all evaluate to a finite number.
#[test]
fn nan_containment_cases_are_finite() {
    let scope = make_scope();
    for src in [
        "1/0",
        "log(-1)",
        "asin(2)",
        "0/0",
        "(-1)^0.5",
        "sqrt(-1)",
        "5%0",
        "tan(pi/2)",
    ] {
        let program = compile(src, &scope).unwrap_or_else(|e| panic!("compile {src}: {e}"));
        let mut vm = Vm::new(scope.slot_count(), 7);
        let v = vm.eval(&program);
        assert!(v.is_finite(), "{src} produced non-finite {v}");
        // All of the listed cases collapse to exactly 0.0 at their faulting op.
        if src != "tan(pi/2)" {
            assert_eq!(v, 0.0, "{src} expected 0.0, got {v}");
        }
    }
}

/// (4) rand determinism: same (seed,k) equal across instances and evals;
/// different k differs.
#[test]
fn rand_determinism() {
    let scope = make_scope();
    let prog_k7 = compile("rand(7)", &scope).unwrap();
    let prog_k8 = compile("rand(8)", &scope).unwrap();

    let mut a = Vm::new(scope.slot_count(), 99);
    let mut b = Vm::new(scope.slot_count(), 99);

    let va1 = a.eval(&prog_k7);
    let va2 = a.eval(&prog_k7);
    let vb = b.eval(&prog_k7);

    assert_eq!(va1, va2, "stable across evals");
    assert_eq!(va1, vb, "stable across instances with same seed");
    assert!((0.0..1.0).contains(&va1), "in [0,1)");

    let vk8 = a.eval(&prog_k8);
    assert_ne!(va1, vk8, "different k yields different value");

    // Different seed yields a different stream.
    let mut c = Vm::new(scope.slot_count(), 100);
    assert_ne!(
        va1,
        c.eval(&prog_k7),
        "different seed yields different value"
    );
}

/// (4b) rand over many k values stays in [0,1) and is well distributed enough to
/// produce many distinct outputs (sanity, not a statistical test).
#[test]
fn rand_range_and_spread() {
    let scope = make_scope();
    let mut vm = Vm::new(scope.slot_count(), 12345);
    let mut seen = std::collections::HashSet::new();
    for k in 0..1000 {
        let prog = compile(&format!("rand({k})"), &scope).unwrap();
        let v = vm.eval(&prog);
        assert!((0.0..1.0).contains(&v), "rand({k}) = {v} out of range");
        seen.insert(v.to_bits());
    }
    // Expect essentially all distinct.
    assert!(
        seen.len() > 990,
        "rand produced only {} distinct values",
        seen.len()
    );
}

/// (5) Precedence sanity — the contract's worked examples.
#[test]
fn precedence_examples() {
    let scope = make_scope();
    let cases: &[(&str, f64)] = &[
        ("2+3*4", 14.0),
        ("-2^2", -4.0), // unary binds looser than ^
        ("1<2&&3>2", 1.0),
        ("if(0.4,1,2)", 2.0),
        ("if(0.6,1,2)", 1.0),
    ];
    for (src, expected) in cases {
        let prog = compile(src, &scope).unwrap();
        let mut vm = Vm::new(scope.slot_count(), 0);
        let got = vm.eval(&prog);
        assert_eq!(got, *expected, "{src} => {got}, expected {expected}");
    }
}
