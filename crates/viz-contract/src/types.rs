//! Typed artifact model — the in-memory mirror of the published contract
//! (`docs/reference/artifact.schema.json`, contract v1.0).
//!
//! These types deserialize one `.artifact.json` file and, via the
//! `schemars` `JsonSchema` derive, regenerate a schema that must stay semantically
//! equivalent to the hand-written reference (guarded by the round-trip test).
//!
//! Design rules baked in here:
//! - Every struct is `#[serde(deny_unknown_fields)]` — strict mode keeps authoring
//!   errors loud (contract §10): a typo like `"colour"` is rejected, never silently
//!   dropped (mirrors the schema's `additionalProperties: false`).
//! - `settings` and `vars` are [`IndexMap`]s: **declaration order is semantic**
//!   (`vars` evaluate in document order, contract §5). `IndexMap` preserves the JSON
//!   object key order on deserialization, independent of `serde_json`'s feature set.
//! - This module is shape only: numeric bounds, name-collision, choice-default-in-options,
//!   `min < max`, and formula compilation are enforced by the load/validation pass
//!   pass, not by serde.

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Load-time limits enforced by the validation pipeline (contract §12).
///
/// Declared here so the next task (the loader/validator) consumes a single source of
/// truth; serde itself does not enforce these — they are checked after deserialization.
pub mod limits {
    /// Maximum layers in a `scene` (contract §12).
    pub const MAX_LAYERS: usize = 16;
    /// Maximum instances per `instanced` layer / points per `polyline` (after clamping).
    pub const MAX_INSTANCES: usize = 4096;
    /// Maximum `field` resolution (cells per axis ⇒ ≤16 384 cells).
    pub const MAX_FIELD_RESOLUTION: usize = 128;
    /// Maximum `settings` entries.
    pub const MAX_SETTINGS: usize = 32;
    /// Maximum `vars` entries.
    pub const MAX_VARS: usize = 64;
    /// Maximum characters in a single formula string.
    pub const MAX_FORMULA_CHARS: usize = 1024;
    /// Maximum compiled bytecode operations per formula.
    pub const MAX_OPS_PER_FORMULA: usize = 256;
    /// Maximum total compiled operations across an entire artifact.
    pub const MAX_OPS_PER_ARTIFACT: usize = 16_384;
    /// Maximum `.artifact.json` file size in bytes (256 KiB).
    pub const MAX_FILE_BYTES: usize = 262_144;

    /// Maximum formula-expression nesting depth (contract §2).
    pub const MAX_FORMULA_NESTING: usize = 32;

    /// Minimum points in a `polyline` after clamping (contract §6.2).
    pub const MIN_POLYLINE_POINTS: usize = 2;
    /// Minimum `field` resolution after clamping (contract §6.3).
    pub const MIN_FIELD_RESOLUTION: usize = 2;
    /// Minimum instances in an `instanced` layer after clamping (contract §6.1).
    pub const MIN_INSTANCES: usize = 1;

    /// Regex source for valid `settings`/`vars` names (contract §4): `[a-z][a-zA-Z0-9_]*`.
    ///
    /// Semantics enforced by the validation pass (not by serde): names must be unique
    /// and must not shadow any built-in identifier reserved in contract §2.3
    /// (`t`, `dt`, `energy`, `low`, `mid`, `high`, `beat`, `beat_count`, `band`, `wave`,
    /// `aspect`, `i`, `n`, `u`, `x`, `y`, `pi`, `tau`, `e`, the `settings`/`rand` namespaces,
    /// and any declared var name). `meta.id` uses the stricter `[a-z][a-z0-9-]*` rule and is
    /// validated via [`viz_core::ArtifactId`].
    pub const SETTING_NAME_RE: &str = r"^[a-z][a-zA-Z0-9_]*$";
}

/// A numeric/color/geometry property: either a JSON number constant or a formula
/// expression string (contract §2). Compiled once at load.
///
/// Mirrors the schema's `formula` def (`oneOf: [number, string]`). `#[serde(untagged)]`
/// makes a bare JSON number deserialize to [`Formula::Number`] and a string to
/// [`Formula::Expr`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Formula {
    /// A constant value (JSON number).
    Number(f64),
    /// A formula expression string, compiled and identifier-checked at load.
    Expr(String),
}

impl Formula {
    /// Borrows the expression text, treating a numeric constant as `None`.
    ///
    /// The validation/compile pass uses this to decide whether a formula needs
    /// parsing (`Some`) or is already a constant (`None`).
    pub fn as_expr(&self) -> Option<&str> {
        match self {
            Formula::Number(_) => None,
            Formula::Expr(s) => Some(s),
        }
    }

    /// Borrows the numeric constant, treating an expression as `None`.
    pub fn as_const(&self) -> Option<f64> {
        match self {
            Formula::Number(n) => Some(*n),
            Formula::Expr(_) => None,
        }
    }

    /// Returns the formula source as a string slice when it is an expression, or the
    /// number constant when it is numeric — i.e. the canonical text to feed the compiler.
    ///
    /// `Ok(expr)` for an expression, `Err(value)` for a constant. Convenience for callers
    /// that special-case constants (no parse needed) from expressions (parse + resolve).
    pub fn as_expr_or_const(&self) -> Result<&str, f64> {
        match self {
            Formula::Expr(s) => Ok(s),
            Formula::Number(n) => Err(*n),
        }
    }
}

/// A complete visual artifact — the deserialized, schema-valid form of one
/// `.artifact.json` file. Immutable after load.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Contract version `"MAJOR.MINOR"` (contract §1). The version gate runs before
    /// schema validation; this field carries the raw string for that check.
    pub contract: String,
    /// Identity and display metadata.
    pub meta: Meta,
    /// User-adjustable controls (≤32). Empty when absent. Key order is not semantic
    /// but is preserved for stable panel rendering.
    #[serde(default)]
    pub settings: IndexMap<String, SettingDecl>,
    /// Named per-frame state variables (≤64). Empty when absent.
    /// **Declaration order is semantic** — it drives `frame` evaluation order (contract §5).
    #[serde(default)]
    pub vars: IndexMap<String, VarDecl>,
    /// 1–16 layers, drawn in array order (later layers on top, contract §6).
    pub scene: Vec<Layer>,
    /// Optional scene-level trails (contract §8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<Feedback>,
}

/// Identity and display metadata (`meta`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    /// Unique id, `[a-z][a-z0-9-]*`, ≤64 chars (validated via [`viz_core::ArtifactId`]).
    pub id: String,
    /// Display name shown in the artifact dropdown.
    pub name: String,
    /// Optional longer description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional author attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// A typed, user-adjustable setting declaration (contract §4).
///
/// Tagged on `type`; the auto-generated panel renders each kind as a widget. The
/// `default` is required for every kind. `min`/`max`/`step` (number),
/// `options` (choice), boolean/string `default`s carry the kind-specific shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SettingDecl {
    /// Numeric slider. Exposed in formulas as `settings.<name>`.
    Number {
        /// Panel label.
        label: String,
        /// Minimum value (must be `< max`, checked at load).
        min: f64,
        /// Maximum value.
        max: f64,
        /// Optional step granularity (`> 0`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
        /// Initial value (within `[min, max]`, checked at load).
        default: f64,
    },
    /// Boolean checkbox. Exposed in formulas as `0.0`/`1.0`.
    Toggle {
        /// Panel label.
        label: String,
        /// Initial state.
        default: bool,
    },
    /// Dropdown of 2–16 unique options. Exposed as the selected 0-based index (f64).
    Choice {
        /// Panel label.
        label: String,
        /// The selectable options (2–16, unique, checked at load).
        options: Vec<String>,
        /// Initial selection — must be one of `options` (checked at load).
        default: String,
    },
    /// Color picker. Exposed as `settings.<name>_r/_g/_b/_a` ∈ 0..1.
    Color {
        /// Panel label.
        label: String,
        /// Initial color, `"#RRGGBB"` or `"#RRGGBBAA"`.
        default: String,
    },
}

/// A named per-frame state variable (contract §5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VarDecl {
    /// Evaluated once at artifact start (and on settings reset).
    pub init: Formula,
    /// Evaluated once per rendered frame, in declaration order.
    pub frame: Formula,
}

/// GPU blend mode for a layer (contract §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Blend {
    /// Standard alpha compositing (default).
    #[default]
    Alpha,
    /// Additive blending — for glow.
    Add,
}

/// Primitive shape for an `instanced` layer (contract §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// Axis-aligned (pre-rotation) rectangle.
    Rect,
    /// Circle.
    Circle,
    /// Triangle.
    Triangle,
}

/// One drawn layer (contract §6). Tagged on `type`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Layer {
    /// N shapes drawn from one per-element program (bars, particles, radial patterns).
    Instanced {
        /// Primitive drawn for every instance.
        shape: Shape,
        /// Per-frame instance count — floored, then clamped 1..4096.
        count: Formula,
        /// Blend mode (default `alpha`).
        #[serde(default)]
        blend: Blend,
        /// Optional per-frame visibility gate (default 1; `< 0.5` skips the layer).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible: Option<Formula>,
        /// Per-element geometry + color program.
        element: ElementBlock,
    },
    /// A connected strip through N points (oscilloscope, curves).
    Polyline {
        /// Per-frame point count — floored, then clamped 2..4096.
        points: Formula,
        /// Stroke width in logical pixels — clamped 0.1..64.
        thickness: Formula,
        /// Join last→first into a loop (radial scopes). Default `false`.
        #[serde(default)]
        closed: bool,
        /// Blend mode (default `alpha`).
        #[serde(default)]
        blend: Blend,
        /// Optional per-frame visibility gate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible: Option<Formula>,
        /// Per-point position + color program.
        point: PointBlock,
    },
    /// A coarse full-canvas color grid (plasma, gradients).
    Field {
        /// Cells per axis — floored, then clamped 2..128 (cost is resolution²).
        resolution: Formula,
        /// Blend mode (default `alpha`).
        #[serde(default)]
        blend: Blend,
        /// Optional per-frame visibility gate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible: Option<Formula>,
        /// Per-cell color program (receives cell-center `x`,`y`).
        cell: CellBlock,
    },
}

/// Per-instance geometry + color program for an `instanced` layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementBlock {
    /// Center x, normalized −1..1 coordinates.
    pub x: Formula,
    /// Center y, normalized −1..1 (y up).
    pub y: Formula,
    /// Width, same units as coordinates.
    pub w: Formula,
    /// Height.
    pub h: Formula,
    /// Optional rotation in radians (CCW about center); default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rot: Option<Formula>,
    /// Element color.
    pub color: ColorSpec,
}

/// Per-point position + color program for a `polyline` layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PointBlock {
    /// Point x, normalized −1..1.
    pub x: Formula,
    /// Point y, normalized −1..1 (y up).
    pub y: Formula,
    /// Point color (interpolated along the strip).
    pub color: ColorSpec,
}

/// Per-cell color program for a `field` layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CellBlock {
    /// Cell color, evaluated at the cell center (`x`,`y` inputs).
    pub color: ColorSpec,
}

/// A color value (contract §7). Tagged on `model`.
///
/// Channels are formulas. `a` is optional (default 1). `rgba` channels clamp 0..1;
/// `hsva` wraps `h` mod 360 and clamps `s`/`v`/`a` to 0..1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "model", rename_all = "lowercase", deny_unknown_fields)]
pub enum ColorSpec {
    /// Red/green/blue/alpha, each 0..1.
    Rgba {
        /// Red channel.
        r: Formula,
        /// Green channel.
        g: Formula,
        /// Blue channel.
        b: Formula,
        /// Optional alpha (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a: Option<Formula>,
    },
    /// Hue (degrees, wrapped)/saturation/value/alpha.
    Hsva {
        /// Hue in degrees (wrapped mod 360).
        h: Formula,
        /// Saturation 0..1.
        s: Formula,
        /// Value 0..1.
        v: Formula,
        /// Optional alpha (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        a: Option<Formula>,
    },
}

/// Scene-level feedback / trails (contract §8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    /// Per-frame decay multiplier applied to the previous frame — clamped 0..0.99.
    pub decay: Formula,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped built-in deserializes cleanly and preserves settings declaration order.
    #[test]
    fn deserializes_spectrum_bars_builtin() {
        let json = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/builtin-artifacts/spectrum-bars.artifact.json"
        ));
        let artifact: Artifact =
            serde_json::from_str(json).expect("spectrum-bars built-in must deserialize");

        assert_eq!(artifact.contract, "1.0");
        assert_eq!(artifact.meta.id, "spectrum-bars");
        assert_eq!(artifact.meta.name, "Spectrum Bars");
        assert!(artifact.meta.description.is_some());
        assert!(artifact.meta.author.is_some());

        // Settings order is semantic for the panel — IndexMap must preserve JSON order.
        let setting_keys: Vec<&str> = artifact.settings.keys().map(String::as_str).collect();
        assert_eq!(
            setting_keys,
            ["bars", "hueShift", "glow", "style", "baseColor"],
            "settings must keep declaration order"
        );

        // Spot-check each setting kind tagged correctly.
        match &artifact.settings["bars"] {
            SettingDecl::Number {
                min,
                max,
                step,
                default,
                ..
            } => {
                assert_eq!(*min, 8.0);
                assert_eq!(*max, 96.0);
                assert_eq!(*step, Some(1.0));
                assert_eq!(*default, 48.0);
            }
            _ => panic!("bars must be a number setting"),
        }
        assert!(matches!(
            artifact.settings["glow"],
            SettingDecl::Toggle { default: true, .. }
        ));
        match &artifact.settings["style"] {
            SettingDecl::Choice {
                options, default, ..
            } => {
                assert_eq!(options, &["thin", "wide"]);
                assert_eq!(default, "wide");
            }
            _ => panic!("style must be a choice setting"),
        }
        assert!(matches!(
            artifact.settings["baseColor"],
            SettingDecl::Color { .. }
        ));

        // vars order is semantic for evaluation.
        let var_keys: Vec<&str> = artifact.vars.keys().map(String::as_str).collect();
        assert_eq!(var_keys, ["flash"]);

        // Scene layers: field, instanced, polyline (in order).
        assert_eq!(artifact.scene.len(), 3);
        assert!(matches!(artifact.scene[0], Layer::Field { .. }));
        match &artifact.scene[1] {
            Layer::Instanced {
                shape,
                blend,
                count,
                element,
                visible,
            } => {
                assert_eq!(*shape, Shape::Rect);
                assert_eq!(*blend, Blend::Add);
                assert_eq!(count.as_expr(), Some("settings.bars"));
                assert!(visible.is_none());
                // rot omitted ⇒ None.
                assert!(element.rot.is_none());
            }
            _ => panic!("scene[1] must be an instanced layer"),
        }
        match &artifact.scene[2] {
            Layer::Polyline {
                points,
                closed,
                blend,
                ..
            } => {
                // Numeric constant deserializes to Formula::Number.
                assert_eq!(points.as_const(), Some(256.0));
                assert!(!*closed, "closed defaults to false when absent");
                assert_eq!(*blend, Blend::Alpha, "blend defaults to alpha when absent");
            }
            _ => panic!("scene[2] must be a polyline layer"),
        }

        // feedback.decay is a constant string formula in the fixture.
        let fb = artifact.feedback.as_ref().expect("feedback present");
        assert_eq!(fb.decay.as_expr(), Some("0.85"));
    }

    /// Unknown keys are rejected (deny_unknown_fields ↔ schema additionalProperties:false).
    #[test]
    fn rejects_unknown_top_level_key() {
        let json = r#"{
            "contract": "1.0",
            "meta": { "id": "x", "name": "X" },
            "scene": [
                { "type": "field", "resolution": 4,
                  "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
            ],
            "colour": "oops"
        }"#;
        let err = serde_json::from_str::<Artifact>(json)
            .expect_err("unknown top-level key must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("colour") || msg.contains("unknown field"),
            "error should name the unknown field, got: {msg}"
        );
    }

    /// Unknown keys nested inside a tagged-union layer are also rejected.
    #[test]
    fn rejects_unknown_nested_key() {
        let json = r#"{
            "contract": "1.0",
            "meta": { "id": "x", "name": "X" },
            "scene": [
                { "type": "field", "resolution": 4, "bogus": 1,
                  "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
            ]
        }"#;
        assert!(
            serde_json::from_str::<Artifact>(json).is_err(),
            "unknown field inside a layer must be rejected"
        );
    }

    /// Absent optional maps default to empty, not an error.
    #[test]
    fn defaults_empty_settings_and_vars() {
        let json = r#"{
            "contract": "1.0",
            "meta": { "id": "x", "name": "X" },
            "scene": [
                { "type": "field", "resolution": 4,
                  "cell": { "color": { "model": "hsva", "h": 0, "s": 1, "v": 1 } } }
            ]
        }"#;
        let artifact: Artifact = serde_json::from_str(json).unwrap();
        assert!(artifact.settings.is_empty());
        assert!(artifact.vars.is_empty());
        assert!(artifact.feedback.is_none());
    }

    /// `Formula` round-trips both number and string forms via the untagged enum.
    #[test]
    fn formula_untagged_roundtrip() {
        let n: Formula = serde_json::from_str("3.5").unwrap();
        assert_eq!(n, Formula::Number(3.5));
        assert_eq!(n.as_const(), Some(3.5));
        assert_eq!(n.as_expr(), None);
        assert_eq!(n.as_expr_or_const(), Err(3.5));

        let s: Formula = serde_json::from_str(r#""sin(t)""#).unwrap();
        assert_eq!(s, Formula::Expr("sin(t)".to_owned()));
        assert_eq!(s.as_expr(), Some("sin(t)"));
        assert_eq!(s.as_const(), None);
        assert_eq!(s.as_expr_or_const(), Ok("sin(t)"));
    }

    /// The JsonSchema derive produces a schema for the root type (smoke test; full
    /// semantic-equivalence vs the reference is a later task).
    #[test]
    fn schema_generates() {
        let schema = schemars::schema_for!(Artifact);
        let json = serde_json::to_value(&schema).unwrap();
        assert!(json.get("$defs").is_some() || json.get("definitions").is_some());
    }
}
