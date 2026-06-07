//! Shared schema-equivalence corpus & validators (used by `roundtrip.rs` and
//! `schema_sync.rs`).
//!
//! Cargo compiles `tests/common/mod.rs` as an ordinary module (not its own test
//! binary), so both integration tests `#[path]`-include it without duplicating the
//! VALID/INVALID corpus or the validator helpers. The corpus exercises only the
//! constraints the generated (shape-only, via `schemars`) and the hand-written
//! reference (`docs/reference/artifact.schema.json`) schemas express **identically** —
//! see `roundtrip.rs`'s module docs for the scope rationale (the `contract`/`id`
//! patterns, hex pattern, numeric `min`/`max`, `scene` length, and string caps are
//! intentionally enforced by the loader's semantic pass, not the generated schema).

#![allow(dead_code)] // Each including test uses a subset of these helpers.

use std::path::PathBuf;

use jsonschema::Validator;
use serde_json::{json, Value};

use viz_contract::published_schema;

/// The shipped built-in fixture.
pub const SPECTRUM_BARS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/builtin-artifacts/spectrum-bars.artifact.json"
));

/// Repo-relative path to the committed reference schema (published under `docs/`).
pub fn reference_schema_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../docs/reference/artifact.schema.json");
    path
}

/// Compiles a draft 2020-12 validator, ignoring unknown formats.
pub fn compile(schema: &Value) -> Validator {
    jsonschema::draft202012::options()
        .should_ignore_unknown_formats(true)
        .build(schema)
        .expect("schema compiles as a draft 2020-12 validator")
}

/// The generated (published) validator.
pub fn generated_validator() -> Validator {
    compile(published_schema())
}

/// Loads and compiles the hand-written committed reference schema from the repo.
pub fn reference_validator() -> Validator {
    let path = reference_schema_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading reference schema at {}: {e}", path.display()));
    let value: Value = serde_json::from_str(&text).expect("reference schema is valid JSON");
    compile(&value)
}

// ---------------------------------------------------------------------------
// Equivalence corpus (shape-only constraints both schemas express identically)
// ---------------------------------------------------------------------------

/// A trivial valid `rgba` color (no alpha).
pub fn rgba() -> Value {
    json!({ "model": "rgba", "r": "0.5", "g": 0, "b": 1 })
}

/// A trivial valid field layer.
pub fn field_layer() -> Value {
    json!({ "type": "field", "resolution": 4, "cell": { "color": rgba() } })
}

/// Wraps a scene into a minimal valid document.
pub fn doc_with_scene(scene: Value) -> Value {
    json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": scene
    })
}

/// The VALID corpus: each must validate under BOTH schemas.
pub fn valid_corpus() -> Vec<(&'static str, Value)> {
    vec![
        // One per layer kind.
        ("field layer", doc_with_scene(json!([field_layer()]))),
        (
            "instanced layer (rect, rot omitted, no visible)",
            doc_with_scene(json!([{
                "type": "instanced", "shape": "rect", "count": "settings.n",
                "element": { "x": 0, "y": 0, "w": "0.1", "h": "0.1", "color": rgba() }
            }])),
        ),
        (
            "polyline layer (closed omitted)",
            doc_with_scene(json!([{
                "type": "polyline", "points": 64, "thickness": "2",
                "point": { "x": "-1 + 2*u", "y": "wave(u)", "color": rgba() }
            }])),
        ),
        // Optional fields present: rot, a, visible, blend, closed.
        (
            "instanced with rot, alpha, visible, blend=add",
            doc_with_scene(json!([{
                "type": "instanced", "shape": "circle", "count": 8, "blend": "add",
                "visible": "energy > 0.1",
                "element": {
                    "x": 0, "y": 0, "w": "0.1", "h": "0.1", "rot": "u * tau",
                    "color": { "model": "rgba", "r": 1, "g": 1, "b": 1, "a": "0.5" }
                }
            }])),
        ),
        (
            "polyline closed=true with visible and blend",
            doc_with_scene(json!([{
                "type": "polyline", "points": 16, "thickness": "1.5", "closed": true,
                "blend": "alpha", "visible": 1,
                "point": { "x": 0, "y": 0, "color": rgba() }
            }])),
        ),
        (
            "field with visible and blend",
            doc_with_scene(json!([{
                "type": "field", "resolution": "settings.res", "blend": "add", "visible": "1",
                "cell": { "color": rgba() }
            }])),
        ),
        // Both color models, hsva with and without alpha.
        (
            "hsva color without alpha",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "hsva", "h": "u*360", "s": 1, "v": "0.5" } }
            }])),
        ),
        (
            "hsva color with alpha",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "hsva", "h": 0, "s": 1, "v": 1, "a": 1 } }
            }])),
        ),
        // One per setting kind, with valid (schema-shape) defaults.
        (
            "number setting (with step)",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "settings": { "n": { "type": "number", "label": "N", "min": 0, "max": 10, "step": 1, "default": 5 } },
                "scene": [field_layer()]
            }),
        ),
        (
            "toggle setting",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "settings": { "g": { "type": "toggle", "label": "G", "default": true } },
                "scene": [field_layer()]
            }),
        ),
        (
            "choice setting",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "settings": { "s": { "type": "choice", "label": "S", "options": ["a", "b"], "default": "a" } },
                "scene": [field_layer()]
            }),
        ),
        (
            "color setting",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "settings": { "c": { "type": "color", "label": "C", "default": "#ffffff" } },
                "scene": [field_layer()]
            }),
        ),
        // vars present.
        (
            "vars present",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X", "description": "d", "author": "a" },
                "vars": { "v": { "init": "0", "frame": "v * 0.9 + low * 0.1" } },
                "scene": [field_layer()]
            }),
        ),
        // feedback present / absent.
        (
            "feedback present",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "scene": [field_layer()],
                "feedback": { "decay": "0.9" }
            }),
        ),
        ("feedback absent", doc_with_scene(json!([field_layer()]))),
    ]
}

/// The INVALID corpus: each must be rejected by BOTH schemas (schema-level only —
/// no cases that only the semantic pass would catch).
pub fn invalid_corpus() -> Vec<(&'static str, Value)> {
    vec![
        // Missing required top-level fields.
        (
            "missing contract",
            json!({ "meta": { "id": "x", "name": "X" }, "scene": [field_layer()] }),
        ),
        (
            "missing meta",
            json!({ "contract": "1.0", "scene": [field_layer()] }),
        ),
        (
            "missing scene",
            json!({ "contract": "1.0", "meta": { "id": "x", "name": "X" } }),
        ),
        // Missing required meta field.
        (
            "missing meta.name",
            json!({ "contract": "1.0", "meta": { "id": "x" }, "scene": [field_layer()] }),
        ),
        // Missing required per-layer fields.
        (
            "instanced missing element",
            doc_with_scene(json!([{ "type": "instanced", "shape": "rect", "count": 4 }])),
        ),
        (
            "field missing cell",
            doc_with_scene(json!([{ "type": "field", "resolution": 4 }])),
        ),
        (
            "polyline missing thickness",
            doc_with_scene(json!([{
                "type": "polyline", "points": 8,
                "point": { "x": 0, "y": 0, "color": rgba() }
            }])),
        ),
        // Missing required element / color field.
        (
            "element missing w",
            doc_with_scene(json!([{
                "type": "instanced", "shape": "rect", "count": 4,
                "element": { "x": 0, "y": 0, "h": "0.1", "color": rgba() }
            }])),
        ),
        (
            "rgba color missing b",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "rgba", "r": 1, "g": 1 } }
            }])),
        ),
        (
            "hsva color missing v",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "hsva", "h": 0, "s": 1 } }
            }])),
        ),
        // Missing required setting field.
        (
            "number setting missing max",
            json!({
                "contract": "1.0",
                "meta": { "id": "x", "name": "X" },
                "settings": { "n": { "type": "number", "label": "N", "min": 0, "default": 1 } },
                "scene": [field_layer()]
            }),
        ),
        // Wrong enum / const values.
        (
            "bad layer type",
            doc_with_scene(
                json!([{ "type": "blob", "resolution": 4, "cell": { "color": rgba() } }]),
            ),
        ),
        (
            "bad shape value",
            doc_with_scene(json!([{
                "type": "instanced", "shape": "hexagon", "count": 4,
                "element": { "x": 0, "y": 0, "w": "0.1", "h": "0.1", "color": rgba() }
            }])),
        ),
        (
            "bad blend value",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4, "blend": "multiply",
                "cell": { "color": rgba() }
            }])),
        ),
        (
            "bad color model",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "cmyk", "c": 0, "m": 0, "y": 0, "k": 0 } }
            }])),
        ),
        // Unknown keys at several depths.
        (
            "unknown top-level key",
            json!({
                "contract": "1.0", "meta": { "id": "x", "name": "X" },
                "scene": [field_layer()], "colour": "oops"
            }),
        ),
        (
            "unknown meta key",
            json!({
                "contract": "1.0", "meta": { "id": "x", "name": "X", "bogus": 1 },
                "scene": [field_layer()]
            }),
        ),
        (
            "unknown layer key",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4, "bogus": 1, "cell": { "color": rgba() }
            }])),
        ),
        (
            "unknown element key",
            doc_with_scene(json!([{
                "type": "instanced", "shape": "rect", "count": 4,
                "element": { "x": 0, "y": 0, "w": "0.1", "h": "0.1", "color": rgba(), "z": 0 }
            }])),
        ),
        (
            "unknown color key",
            doc_with_scene(json!([{
                "type": "field", "resolution": 4,
                "cell": { "color": { "model": "rgba", "r": 1, "g": 1, "b": 1, "q": 1 } }
            }])),
        ),
        (
            "unknown feedback key",
            json!({
                "contract": "1.0", "meta": { "id": "x", "name": "X" },
                "scene": [field_layer()], "feedback": { "decay": "0.9", "rate": 1 }
            }),
        ),
        // Wrong primitive type for a typed field.
        (
            "toggle default wrong type",
            json!({
                "contract": "1.0", "meta": { "id": "x", "name": "X" },
                "settings": { "g": { "type": "toggle", "label": "G", "default": "yes" } },
                "scene": [field_layer()]
            }),
        ),
    ]
}

/// Asserts every corpus document classifies identically under both schemas. Shared
/// so `roundtrip.rs` and `schema_sync.rs` apply the exact same equivalence guard.
pub fn assert_corpus_classifies_identically() {
    let generated = generated_validator();
    let reference = reference_validator();

    for (label, doc) in valid_corpus() {
        let gen_errs: Vec<String> = generated.iter_errors(&doc).map(|e| e.to_string()).collect();
        let ref_errs: Vec<String> = reference.iter_errors(&doc).map(|e| e.to_string()).collect();
        assert!(
            generated.is_valid(&doc),
            "VALID corpus '{label}' rejected by generated schema: {gen_errs:?}"
        );
        assert!(
            reference.is_valid(&doc),
            "VALID corpus '{label}' rejected by reference schema: {ref_errs:?}"
        );
    }

    for (label, doc) in invalid_corpus() {
        assert!(
            !generated.is_valid(&doc),
            "INVALID corpus '{label}' wrongly accepted by generated schema"
        );
        assert!(
            !reference.is_valid(&doc),
            "INVALID corpus '{label}' wrongly accepted by reference schema"
        );
    }
}
