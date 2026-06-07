//! Live-tunable settings integration tests.
//!
//! These drive the **pure** runtime + persistence paths only (no GPU, no window, no
//! egui): a programmatic setting change must be reflected in the very next
//! [`ArtifactRuntime::frame`] output; "reset" restores the declared defaults; and the
//! persisted JSON round-trips with full revalidation against the current declarations
//! (an out-of-range number, an unknown setting name, and a stale artifact id are all
//! handled gracefully — each falls back to the declared default rather than failing).

use std::collections::BTreeMap;

use indexmap::IndexMap;

use viz_app::persist::{restore_setting_values, setting_values_to_json, AppState};
use viz_app::runtime::{ArtifactRuntime, SettingValue};
use viz_contract::{load_artifact, LoadedArtifact};
use viz_core::FeatureFrame;
use viz_render::LayerDraw;

/// An artifact whose instance `count` is driven by `settings.bars` and whose element
/// color is driven by a `color` setting — so a setting change is directly observable
/// in the staged geometry (instance count) and color output.
const SETTABLE: &str = r##"{
  "contract": "1.0",
  "meta": { "id": "settable", "name": "Settable" },
  "settings": {
    "bars":  { "type": "number", "label": "Bars", "min": 1, "max": 64, "step": 1, "default": 8 },
    "glow":  { "type": "toggle", "label": "Glow", "default": false },
    "style": { "type": "choice", "label": "Style", "options": ["thin", "wide"], "default": "thin" },
    "tint":  { "type": "color", "label": "Tint", "default": "#204080" }
  },
  "scene": [
    {
      "type": "instanced", "shape": "rect", "count": "settings.bars",
      "element": {
        "x": "-1 + (2 * i + 1) / n", "y": "0", "w": "0.1", "h": "0.5",
        "color": {
          "model": "rgba",
          "r": "settings.tint_r", "g": "settings.tint_g", "b": "settings.tint_b", "a": "1"
        }
      }
    }
  ]
}"##;

fn load(src: &str) -> LoadedArtifact {
    load_artifact("test", src.as_bytes()).expect("test artifact loads")
}

/// The instance count of the first (only) instanced layer in a fresh frame.
fn instance_count(rt: &mut ArtifactRuntime) -> usize {
    let scene = rt.frame(&FeatureFrame::default(), 1.0 / 60.0, 16.0 / 9.0, 1.0);
    for layer in &scene.layers {
        if let LayerDraw::Instanced { instances, .. } = layer {
            return instances.len();
        }
    }
    panic!("no instanced layer in scene");
}

/// The first instance's RGBA color in a fresh frame.
fn first_color(rt: &mut ArtifactRuntime) -> [f32; 4] {
    let scene = rt.frame(&FeatureFrame::default(), 1.0 / 60.0, 16.0 / 9.0, 1.0);
    for layer in &scene.layers {
        if let LayerDraw::Instanced { instances, .. } = layer {
            return instances[0].color;
        }
    }
    panic!("no instanced layer in scene");
}

#[test]
fn number_setting_change_reflects_in_next_frame() {
    let mut rt = ArtifactRuntime::new(load(SETTABLE));
    rt.activate();
    // Default `bars` = 8.
    assert_eq!(instance_count(&mut rt), 8);

    // Change it; the very next frame must show the new count (no deferred queue —
    // the slot is flushed before var/element evaluation).
    assert!(rt.set_setting_scalar("bars", 20.0));
    assert_eq!(instance_count(&mut rt), 20);

    // And again, downward.
    assert!(rt.set_setting_scalar("bars", 3.0));
    assert_eq!(instance_count(&mut rt), 3);
}

#[test]
fn color_setting_change_reflects_in_next_frame() {
    let mut rt = ArtifactRuntime::new(load(SETTABLE));
    rt.activate();
    // Default tint #204080 → (0x20, 0x40, 0x80) / 255.
    let c0 = first_color(&mut rt);
    assert!((c0[0] - 0x20 as f32 / 255.0).abs() < 1e-4);
    assert!((c0[2] - 0x80 as f32 / 255.0).abs() < 1e-4);

    // Pure red, opaque.
    assert!(rt.set_setting_color("tint", 1.0, 0.0, 0.0, 1.0));
    let c1 = first_color(&mut rt);
    assert!((c1[0] - 1.0).abs() < 1e-4, "red channel = {}", c1[0]);
    assert!(c1[1].abs() < 1e-4, "green channel = {}", c1[1]);
    assert!(c1[2].abs() < 1e-4, "blue channel = {}", c1[2]);
}

#[test]
fn wrong_kind_setting_change_is_rejected() {
    let mut rt = ArtifactRuntime::new(load(SETTABLE));
    rt.activate();
    // `bars` is scalar; a color write must be rejected and leave it unchanged.
    assert!(!rt.set_setting_color("bars", 1.0, 0.0, 0.0, 1.0));
    // `tint` is a color; a scalar write must be rejected.
    assert!(!rt.set_setting_scalar("tint", 5.0));
    // Unknown name.
    assert!(!rt.set_setting_scalar("nope", 1.0));
    assert_eq!(instance_count(&mut rt), 8);
}

#[test]
fn reset_restores_declared_defaults() {
    let mut rt = ArtifactRuntime::new(load(SETTABLE));
    rt.activate();
    assert!(rt.set_setting_scalar("bars", 40.0));
    assert!(rt.set_setting_color("tint", 1.0, 1.0, 1.0, 1.0));
    assert_eq!(instance_count(&mut rt), 40);

    rt.reset();
    // Back to the declared default count (8) and the default tint.
    assert_eq!(instance_count(&mut rt), 8);
    let c = first_color(&mut rt);
    assert!((c[0] - 0x20 as f32 / 255.0).abs() < 1e-4);
}

/// Helper: build the runtime's current value map after a few edits.
fn edited_values() -> (LoadedArtifact, IndexMap<String, SettingValue>) {
    let art = load(SETTABLE);
    let mut rt = ArtifactRuntime::new(art.clone());
    rt.activate();
    rt.set_setting_scalar("bars", 16.0);
    rt.set_setting_scalar("glow", 1.0);
    rt.set_setting_scalar("style", 1.0); // "wide"
    rt.set_setting_color("tint", 1.0, 0.5, 0.0, 1.0);
    (art, rt.settings().clone())
}

#[test]
fn settings_round_trip_through_persistence() {
    let (art, values) = edited_values();

    // Serialize the runtime values → JSON block, store + reload via AppState.
    let mut state = AppState::default();
    state.set_artifact_settings(art.id.0.as_str(), &art.artifact.settings, &values);

    let json = serde_json::to_string(&state).expect("serialize");
    let back: AppState = serde_json::from_str(&json).expect("deserialize");

    // Restore + revalidate against the current declarations.
    let restored = back.artifact_settings(&art);

    assert_eq!(restored["bars"], SettingValue::Scalar(16.0));
    assert_eq!(restored["glow"], SettingValue::Scalar(1.0));
    assert_eq!(restored["style"], SettingValue::Scalar(1.0));
    match restored["tint"] {
        SettingValue::Color { r, g, b, a } => {
            assert!((r - 1.0).abs() < 1e-3);
            assert!((g - 0.5).abs() < 2e-2, "g = {g}"); // 0.5 → 0x80/255 ≈ 0.502
            assert!(b.abs() < 1e-3);
            assert!((a - 1.0).abs() < 1e-3);
        }
        other => panic!("expected color, got {other:?}"),
    }

    // And the restored values, applied to a fresh runtime, drive the next frame.
    let mut rt = ArtifactRuntime::new(art.clone());
    rt.activate();
    for (name, value) in &restored {
        rt.set_setting(name, *value);
    }
    assert_eq!(instance_count(&mut rt), 16);
}

#[test]
fn out_of_range_number_falls_back_to_default() {
    let art = load(SETTABLE);
    // A stored block with `bars` far above its declared max (64) → must fall back to
    // the declared default (8), not clamp silently to a different value.
    let mut block = BTreeMap::new();
    block.insert("bars".to_owned(), serde_json::json!(9999));
    let restored = restore_setting_values(&art, Some(&block));
    assert_eq!(restored["bars"], SettingValue::Scalar(8.0));

    // A below-min value falls back too.
    let mut block = BTreeMap::new();
    block.insert("bars".to_owned(), serde_json::json!(0));
    let restored = restore_setting_values(&art, Some(&block));
    assert_eq!(restored["bars"], SettingValue::Scalar(8.0));
}

#[test]
fn wrong_typed_stored_value_falls_back_to_default() {
    let art = load(SETTABLE);
    // `bars` stored as a string (wrong type), `glow` as a number (wrong type),
    // `style` as an option that no longer exists → all fall back to defaults.
    let mut block = BTreeMap::new();
    block.insert("bars".to_owned(), serde_json::json!("lots"));
    block.insert("glow".to_owned(), serde_json::json!(3));
    block.insert("style".to_owned(), serde_json::json!("rainbow"));
    block.insert("tint".to_owned(), serde_json::json!("not-a-color"));
    let restored = restore_setting_values(&art, Some(&block));

    assert_eq!(restored["bars"], SettingValue::Scalar(8.0));
    assert_eq!(restored["glow"], SettingValue::Scalar(0.0)); // default false
    assert_eq!(restored["style"], SettingValue::Scalar(0.0)); // default "thin" = index 0
                                                              // tint falls back to the declared default #204080.
    match restored["tint"] {
        SettingValue::Color { r, b, .. } => {
            assert!((r - 0x20 as f32 as f64 / 255.0).abs() < 1e-3);
            assert!((b - 0x80 as f32 as f64 / 255.0).abs() < 1e-3);
        }
        other => panic!("expected color, got {other:?}"),
    }
}

#[test]
fn unknown_setting_name_is_dropped_on_restore() {
    let art = load(SETTABLE);
    // A stored key the artifact no longer declares must be silently dropped: the
    // restored map contains only the declared settings, in declaration order.
    let mut block = BTreeMap::new();
    block.insert("bars".to_owned(), serde_json::json!(12));
    block.insert("legacySetting".to_owned(), serde_json::json!(42));
    let restored = restore_setting_values(&art, Some(&block));

    assert!(!restored.contains_key("legacySetting"));
    let names: Vec<&str> = restored.keys().map(String::as_str).collect();
    assert_eq!(names, ["bars", "glow", "style", "tint"]);
    assert_eq!(restored["bars"], SettingValue::Scalar(12.0));
}

#[test]
fn stale_artifact_id_block_is_ignored() {
    let art = load(SETTABLE);
    // AppState holds a setting block only for a *different* artifact id; restoring
    // the current artifact must yield all declared defaults (graceful fallback).
    let mut state = AppState::default();
    let mut other_block = BTreeMap::new();
    other_block.insert("bars".to_owned(), serde_json::json!(33));
    state
        .setting_values
        .insert("some-old-artifact".to_owned(), other_block);

    let restored = state.artifact_settings(&art);
    assert_eq!(restored["bars"], SettingValue::Scalar(8.0)); // default, not 33
}

#[test]
fn no_stored_block_yields_all_defaults() {
    let art = load(SETTABLE);
    let restored = restore_setting_values(&art, None);
    assert_eq!(restored["bars"], SettingValue::Scalar(8.0));
    assert_eq!(restored["glow"], SettingValue::Scalar(0.0));
    assert_eq!(restored["style"], SettingValue::Scalar(0.0));
}

#[test]
fn to_json_skips_settings_not_in_decls() {
    let art = load(SETTABLE);
    // A value map carrying an extra (undeclared) key must not be persisted.
    let mut values: IndexMap<String, SettingValue> = IndexMap::new();
    values.insert("bars".to_owned(), SettingValue::Scalar(10.0));
    values.insert("ghost".to_owned(), SettingValue::Scalar(1.0));
    let block = setting_values_to_json(&art.artifact.settings, &values);
    assert!(block.contains_key("bars"));
    assert!(!block.contains_key("ghost"));
    // The choice/color defaults are absent here because `values` only had `bars`.
    assert_eq!(block.len(), 1);
}

#[test]
fn choice_persisted_as_option_string() {
    let art = load(SETTABLE);
    let mut values: IndexMap<String, SettingValue> = IndexMap::new();
    values.insert("style".to_owned(), SettingValue::Scalar(1.0)); // index 1 = "wide"
    let block = setting_values_to_json(&art.artifact.settings, &values);
    assert_eq!(block["style"], serde_json::json!("wide"));
}
