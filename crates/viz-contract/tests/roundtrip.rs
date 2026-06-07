//! Schema round-trip / equivalence guard.
//!
//! Two checks:
//!
//! (a) [`published_schema`] (generated from the typed model via `schemars`) accepts
//!     the shipped `spectrum-bars` built-in via the `jsonschema` crate.
//!
//! (b) Equivalence guard: a corpus of small VALID and INVALID documents must
//!     classify **identically** under both the generated schema and the
//!     hand-written reference (`contracts/artifact.schema.json`).
//!
//! ## Scope of the equivalence guard
//!
//! The corpus exercises only constraints the two schemas express **identically**:
//! document structure (required fields, `additionalProperties: false`), the
//! tagged-union variant shapes, and the closed enum/`const` values (layer `type`,
//! `shape`, `blend`, color `model`).
//!
//! It deliberately does **not** exercise constraints that exist in the reference
//! schema but are intentionally enforced by this crate's *semantic pass* instead
//! of by the generated schema (`schemars` does not emit them): the `contract` and
//! `meta.id` patterns, the color-`default` hex pattern, number `min`/`max` value
//! relationships, choice `options` count/uniqueness, `scene` min/max length, and
//! string length caps. Those would diverge between the two schemas by design — the
//! generated schema is shape-only, the loader's [`load_artifact`] catches the rest.
//! Each such case is covered by the load-pipeline unit tests (`src/load/tests.rs`)
//! and, for the color pattern, by [`color_default_pattern_is_semantic_not_schema`]
//! below.
//!
//! The `Formula` def differs syntactically (generated `anyOf[number,string]` vs
//! reference `oneOf[number,string]`) but is semantically equal for these inputs;
//! the corpus uses both number and string formulas, so this is covered naturally.
//!
//! The VALID/INVALID corpus and the validator helpers live in
//! [`common`](mod@common) so the schema-export sync test (`schema_sync.rs`) reuses
//! the same equivalence guard without duplicating it.

#[path = "common/mod.rs"]
mod common;

use serde_json::{json, Value};

use viz_contract::published_schema;

/// (a) The published (generated) schema accepts the built-in.
#[test]
fn published_schema_accepts_builtin() {
    let validator = common::compile(published_schema());
    let doc: Value = serde_json::from_str(common::SPECTRUM_BARS).expect("built-in is valid JSON");
    let errors: Vec<String> = validator.iter_errors(&doc).map(|e| e.to_string()).collect();
    assert!(
        errors.is_empty(),
        "published schema must accept the built-in; errors: {errors:?}"
    );
}

/// The reference schema also accepts the built-in (sanity for the corpus baseline).
#[test]
fn reference_schema_accepts_builtin() {
    let validator = common::reference_validator();
    let doc: Value = serde_json::from_str(common::SPECTRUM_BARS).expect("built-in is valid JSON");
    let errors: Vec<String> = validator.iter_errors(&doc).map(|e| e.to_string()).collect();
    assert!(
        errors.is_empty(),
        "reference schema must accept the built-in; errors: {errors:?}"
    );
}

/// (b) Every corpus document classifies identically under both schemas.
#[test]
fn corpus_classifies_identically_under_both_schemas() {
    common::assert_corpus_classifies_identically();
}

/// The color-`default` hex pattern is a *semantic-pass* check in this crate, not a
/// generated-schema check (documents the intentional generated-vs-reference
/// divergence excluded from the equivalence corpus above). The reference schema
/// rejects a bad hex at schema level; the generated schema does not — so the load
/// pipeline catches it at the semantic step instead.
#[test]
fn color_default_pattern_is_semantic_not_schema() {
    let bad = json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": { "c": { "type": "color", "label": "C", "default": "not-a-hex" } },
        "scene": [common::field_layer()]
    });

    // Reference schema: rejected at schema level (it carries the hex pattern).
    let reference = common::reference_validator();
    assert!(
        !reference.is_valid(&bad),
        "reference schema must reject a bad color default at schema level"
    );

    // Generated schema: shape-only, so it accepts the bad hex...
    let generated = common::generated_validator();
    assert!(
        generated.is_valid(&bad),
        "generated schema is shape-only and does not encode the color hex pattern"
    );

    // ...but the full load pipeline rejects it at the semantic step.
    let bytes = serde_json::to_vec(&bad).unwrap();
    let err = viz_contract::load_artifact("bad-color.artifact.json", &bytes)
        .expect_err("bad color default must be rejected by the loader");
    assert_eq!(err.step, viz_contract::LoadStep::Semantic);
}
