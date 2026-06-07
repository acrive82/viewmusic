//! Unit tests for the load/validation pipeline.

use super::*;

/// The shipped built-in fixture, as raw bytes.
const SPECTRUM_BARS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/builtin-artifacts/spectrum-bars.artifact.json"
));

/// A minimal valid single-field-layer document, as a reusable base for negatives.
fn minimal_doc() -> serde_json::Value {
    serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    })
}

/// Happy path: the built-in loads, layout/var order and programs are as expected.
#[test]
fn loads_spectrum_bars_builtin() {
    let loaded = load_artifact("spectrum-bars.artifact.json", SPECTRUM_BARS.as_bytes())
        .expect("built-in must load");

    assert_eq!(loaded.id.0, "spectrum-bars");
    assert_eq!(loaded.seed, loaded.id.rand_seed());

    // Slot layout: the first nine slots are the shared inputs in order.
    let layout = &loaded.layout;
    assert_eq!(layout.t, SlotId(0));
    assert_eq!(layout.dt, SlotId(1));
    assert_eq!(layout.energy, SlotId(2));
    assert_eq!(layout.low, SlotId(3));
    assert_eq!(layout.mid, SlotId(4));
    assert_eq!(layout.high, SlotId(5));
    assert_eq!(layout.beat, SlotId(6));
    assert_eq!(layout.beat_count, SlotId(7));
    assert_eq!(layout.aspect, SlotId(8));

    // Settings declaration order is preserved in the layout map.
    let setting_keys: Vec<&str> = layout.settings.keys().map(String::as_str).collect();
    assert_eq!(
        setting_keys,
        ["bars", "hueShift", "glow", "style", "baseColor"]
    );

    // bars/hueShift/glow/style are scalar; baseColor is a 4-channel color.
    assert!(matches!(layout.settings["bars"], SettingSlots::Scalar(_)));
    assert!(matches!(layout.settings["glow"], SettingSlots::Scalar(_)));
    assert!(matches!(layout.settings["style"], SettingSlots::Scalar(_)));
    match layout.settings["baseColor"] {
        SettingSlots::Color { r, g, b, a } => {
            // The four channels are contiguous and ordered r,g,b,a.
            assert_eq!(g.0, r.0 + 1);
            assert_eq!(b.0, r.0 + 2);
            assert_eq!(a.0, r.0 + 3);
        }
        _ => panic!("baseColor must be a color setting"),
    }

    // Slot order: shared(9) + scalar settings(4) + color(4) + vars(1) + extras(5) = 23.
    assert_eq!(layout.slot_count, 9 + 4 + 4 + 1 + 5);

    // The color channels come before the var slot, which comes before stage extras.
    let var_slot = layout.vars["flash"];
    match layout.settings["baseColor"] {
        SettingSlots::Color { a, .. } => assert!(a.0 < var_slot.0),
        _ => unreachable!(),
    }
    assert!(var_slot.0 < layout.i.0);
    assert_eq!(layout.i.0 + 1, layout.n.0);
    assert_eq!(layout.n.0 + 1, layout.u.0);
    assert_eq!(layout.u.0 + 1, layout.x.0);
    assert_eq!(layout.x.0 + 1, layout.y.0);
    assert_eq!(layout.y.0 as usize, layout.slot_count - 1);

    // Var order: single var "flash".
    let var_keys: Vec<&str> = layout.vars.keys().map(String::as_str).collect();
    assert_eq!(var_keys, ["flash"]);

    // Programs: one init + one frame for the single var. In this fixture both are
    // string formulas ("0" and "max(flash * 0.88, beat)"), so both compile to
    // programs (a bare JSON number would record a `Const`).
    assert_eq!(loaded.programs.vars_init.len(), 1);
    assert_eq!(loaded.programs.vars_frame.len(), 1);
    assert!(matches!(
        loaded.programs.vars_init[0],
        CompiledFormula::Program(_)
    ));
    assert!(matches!(
        loaded.programs.vars_frame[0],
        CompiledFormula::Program(_)
    ));

    // Three layers in scene order: field, instanced, polyline.
    assert_eq!(loaded.programs.layers.len(), 3);
    assert!(matches!(
        loaded.programs.layers[0],
        LayerPrograms::Field { .. }
    ));
    match &loaded.programs.layers[1] {
        LayerPrograms::Instanced { rot, color, .. } => {
            assert!(rot.is_none(), "element.rot omitted ⇒ None");
            assert_eq!(color.model, ColorModel::Hsva);
            assert!(color.a.is_some());
        }
        _ => panic!("scene[1] must be instanced"),
    }
    match &loaded.programs.layers[2] {
        LayerPrograms::Polyline { points, closed, .. } => {
            assert!(!closed);
            // points: 256 numeric constant.
            assert!(matches!(points, CompiledFormula::Const(c) if *c == 256.0));
        }
        _ => panic!("scene[2] must be polyline"),
    }

    // feedback.decay present, a constant-string formula "0.85" → a one-op program.
    assert!(loaded.programs.feedback_decay.is_some());
}

/// The compiled programs actually evaluate against a VM built from the layout.
#[test]
fn programs_evaluate_against_vm() {
    let loaded = load_artifact("spectrum-bars.artifact.json", SPECTRUM_BARS.as_bytes()).unwrap();
    let mut vm = Vm::new(loaded.layout.slot_count, loaded.seed);

    // vars.flash.init = "0".
    let init = loaded.programs.vars_init[0].value(&mut vm);
    assert_eq!(init, 0.0);

    // feedback.decay = "0.85".
    let decay = loaded
        .programs
        .feedback_decay
        .as_ref()
        .unwrap()
        .value(&mut vm);
    assert!((decay - 0.85).abs() < 1e-12);
}

/// Contract "2.0" is rejected at the Version step with the exact message.
#[test]
fn rejects_unsupported_major_version() {
    let mut doc = minimal_doc();
    doc["contract"] = serde_json::json!("2.0");
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("v2.artifact.json", &bytes).expect_err("2.0 must be rejected");
    assert_eq!(err.step, LoadStep::Version);
    assert_eq!(err.message, "unsupported contract version 2.0");
}

/// A newer minor (1.1) is also rejected at the Version step (loader supports ≤ 1.0).
#[test]
fn rejects_newer_minor_version() {
    let mut doc = minimal_doc();
    doc["contract"] = serde_json::json!("1.1");
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("v11.artifact.json", &bytes).expect_err("1.1 must be rejected");
    assert_eq!(err.step, LoadStep::Version);
    assert_eq!(err.message, "unsupported contract version 1.1");
}

/// The version gate runs BEFORE schema validation: a newer version with an
/// otherwise schema-invalid body still reports the clear version message.
#[test]
fn version_gate_precedes_schema() {
    let doc = serde_json::json!({
        "contract": "1.5",
        "meta": { "id": "x", "name": "X" },
        "scene": [],
        "bogus_key": 1
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("future.artifact.json", &bytes).expect_err("1.5 must be rejected");
    assert_eq!(err.step, LoadStep::Version);
    assert_eq!(err.message, "unsupported contract version 1.5");
}

/// An unknown top-level key is rejected at the Schema step with a path.
#[test]
fn rejects_unknown_key_at_schema_step() {
    let mut doc = minimal_doc();
    doc["colour"] = serde_json::json!("oops");
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("typo.artifact.json", &bytes).expect_err("unknown key rejected");
    assert_eq!(err.step, LoadStep::Schema);
    // The failing instance is the document root; the message names additional props.
    assert!(
        err.message.to_lowercase().contains("colour")
            || err.message.to_lowercase().contains("additional"),
        "schema error should mention the offending property: {}",
        err.message
    );
}

/// An unknown key nested in a layer reports a nested instance path.
#[test]
fn schema_error_reports_nested_path() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": 4, "bogus": 1,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("nested.artifact.json", &bytes).expect_err("nested unknown key");
    assert_eq!(err.step, LoadStep::Schema);
    assert!(
        err.json_path.contains("scene[0]"),
        "path should localize the failing layer, got: {}",
        err.json_path
    );
}

/// A bad formula token is rejected at the Formula step, naming the token and path.
#[test]
fn rejects_bad_formula_token() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "hsva",
                "h": "bogus_ident + 1", "s": 1, "v": 1 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("bad.artifact.json", &bytes).expect_err("bad token rejected");
    assert_eq!(err.step, LoadStep::Formula);
    assert_eq!(err.json_path, "scene[0].cell.color.h");
    assert!(
        err.message.contains("bogus_ident"),
        "message should name the offending token: {}",
        err.message
    );
}

/// `x` is illegal inside an instanced element formula (stage restriction).
#[test]
fn rejects_stage_extra_x_in_element() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "instanced", "shape": "rect", "count": 4,
              "element": {
                "x": "x * 0.5", "y": "0", "w": "0.1", "h": "0.1",
                "color": { "model": "rgba", "r": 1, "g": 1, "b": 1 }
              } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("stage.artifact.json", &bytes).expect_err("x in element rejected");
    assert_eq!(err.step, LoadStep::Formula);
    assert_eq!(err.json_path, "scene[0].element.x");
    assert!(
        err.message.contains("'x'"),
        "message should name 'x': {}",
        err.message
    );
}

/// `u` is illegal inside a field cell formula (cell allows x,y,i,n only).
#[test]
fn rejects_stage_extra_u_in_cell() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": "u", "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("cellu.artifact.json", &bytes).expect_err("u in cell rejected");
    assert_eq!(err.step, LoadStep::Formula);
    assert_eq!(err.json_path, "scene[0].cell.color.r");
}

/// `i` is illegal in the frame stage (a var frame formula).
#[test]
fn rejects_stage_extra_in_frame() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "vars": { "v": { "init": "0", "frame": "i + 1" } },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("framei.artifact.json", &bytes).expect_err("i in frame rejected");
    assert_eq!(err.step, LoadStep::Formula);
    assert_eq!(err.json_path, "vars.v.frame");
}

/// `x`/`y` ARE allowed in the cell stage (positive control for the restriction).
#[test]
fn allows_x_y_in_cell() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "hsva",
                "h": "x * 180 + y * 180", "s": 1, "v": 1 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    assert!(load_artifact("xy.artifact.json", &bytes).is_ok());
}

/// A setting named `beat` collides with a reserved input at the Semantic step.
#[test]
fn rejects_setting_named_beat() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": {
            "beat": { "type": "number", "label": "Beat", "min": 0, "max": 1, "default": 0 }
        },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("beat.artifact.json", &bytes).expect_err("setting 'beat' rejected");
    assert_eq!(err.step, LoadStep::Semantic);
    assert_eq!(err.json_path, "settings.beat");
    assert!(err.message.contains("reserved"), "message: {}", err.message);
}

/// A color setting whose `_r` expansion collides with another setting is rejected.
#[test]
fn rejects_color_channel_collision() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": {
            "tint": { "type": "color", "label": "Tint", "default": "#ffffff" },
            "tint_r": { "type": "number", "label": "TR", "min": 0, "max": 1, "default": 0 }
        },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err =
        load_artifact("collide.artifact.json", &bytes).expect_err("channel collision rejected");
    assert_eq!(err.step, LoadStep::Semantic);
}

/// A choice default not in options is rejected at the Semantic step.
#[test]
fn rejects_choice_default_not_in_options() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": {
            "mode": { "type": "choice", "label": "Mode", "options": ["a", "b"], "default": "c" }
        },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("choice.artifact.json", &bytes).expect_err("bad choice default");
    assert_eq!(err.step, LoadStep::Semantic);
    assert_eq!(err.json_path, "settings.mode");
}

/// A number setting with min >= max is rejected at the Semantic step.
#[test]
fn rejects_number_min_ge_max() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": {
            "v": { "type": "number", "label": "V", "min": 10, "max": 1, "default": 5 }
        },
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("minmax.artifact.json", &bytes).expect_err("min>=max rejected");
    assert_eq!(err.step, LoadStep::Semantic);
}

/// An invalid meta.id is rejected at the Semantic step.
#[test]
fn rejects_invalid_meta_id() {
    let mut doc = minimal_doc();
    doc["meta"]["id"] = serde_json::json!("Bad_Id");
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("id.artifact.json", &bytes).expect_err("bad id rejected");
    // Bad id pattern is caught at the schema step (pattern ^[a-z][a-z0-9-]*$).
    assert!(matches!(err.step, LoadStep::Schema | LoadStep::Semantic));
}

/// An op-cap bomb: a generated formula exceeding 256 ops is rejected at Formula.
#[test]
fn rejects_op_cap_bomb() {
    // "1+1+1+...": 128 "+1" → 257 ops > MAX_OPS_PER_FORMULA (256).
    let bomb = format!("1{}", "+1".repeat(200));
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "scene": [
            { "type": "field", "resolution": bomb,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("bomb.artifact.json", &bytes).expect_err("op bomb rejected");
    assert_eq!(err.step, LoadStep::Formula);
    assert_eq!(err.json_path, "scene[0].resolution");
}

/// The artifact-wide op cap is enforced across many formulas, not just one.
#[test]
fn rejects_artifact_wide_op_cap() {
    // Each formula stays under the per-formula cap (256 ops) but together they
    // blow the 16 384 artifact cap. A scene caps at 16 layers, so the clean
    // artifact-wide trigger is many vars: 64 vars (the max) each with a ~251-op
    // init + frame ≈ 64 × 502 ≈ 32 000 ops, well over the cap.
    // 251-op formula: "1" + 125×"+1" = 1 + 250 = 251 ops (< 256 per-formula cap).
    let big = format!("1{}", "+1".repeat(125));
    let mut vars = serde_json::Map::new();
    for k in 0..64 {
        vars.insert(
            format!("v{k}"),
            serde_json::json!({ "init": big, "frame": big }),
        );
    }
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "vars": vars,
        "scene": [
            { "type": "field", "resolution": 4,
              "cell": { "color": { "model": "rgba", "r": 1, "g": 0, "b": 0 } } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let err = load_artifact("wide.artifact.json", &bytes).expect_err("artifact op cap");
    assert_eq!(err.step, LoadStep::Formula);
    assert!(
        err.message.contains("total-operation"),
        "message should mention the artifact-wide cap: {}",
        err.message
    );
}

/// An over-size file is rejected at the Parse step before any JSON parsing.
#[test]
fn rejects_oversize_file() {
    let big = vec![b' '; limits::MAX_FILE_BYTES + 1];
    let err = load_artifact("huge.artifact.json", &big).expect_err("oversize rejected");
    assert_eq!(err.step, LoadStep::Parse);
}

/// Invalid JSON syntax is rejected at the Parse step.
#[test]
fn rejects_invalid_json() {
    let err = load_artifact("broken.artifact.json", b"{ not json").expect_err("bad json");
    assert_eq!(err.step, LoadStep::Parse);
}

/// `LoadDiagnostic` Display matches the documented English format.
#[test]
fn diagnostic_display_format() {
    let d = LoadDiagnostic {
        source_name: "a.artifact.json".to_owned(),
        step: LoadStep::Formula,
        json_path: "scene[0].element.color.h".to_owned(),
        message: "unknown identifier 'foo'".to_owned(),
    };
    assert_eq!(
        d.to_string(),
        "file a.artifact.json: [formula] at scene[0].element.color.h: unknown identifier 'foo'"
    );
}

/// `settings.x` (a dotted color channel) does NOT trip the stage `x` restriction:
/// the lexer treats it as one atomic token, never matching the bare `x` extra.
#[test]
fn dotted_settings_x_not_confused_with_stage_x() {
    let doc = serde_json::json!({
        "contract": "1.0",
        "meta": { "id": "x", "name": "X" },
        "settings": {
            "tint": { "type": "color", "label": "Tint", "default": "#112233" }
        },
        "scene": [
            { "type": "instanced", "shape": "rect", "count": 4,
              "element": {
                "x": "settings.tint_r - 0.5", "y": "0", "w": "0.1", "h": "0.1",
                "color": { "model": "rgba", "r": "settings.tint_r", "g": "settings.tint_g",
                           "b": "settings.tint_b", "a": "settings.tint_a" }
              } }
        ]
    });
    let bytes = serde_json::to_vec(&doc).unwrap();
    let loaded = load_artifact("dotted.artifact.json", &bytes)
        .expect("settings.tint_* must not trip the stage-x restriction");
    assert!(matches!(
        loaded.programs.layers[0],
        LayerPrograms::Instanced { .. }
    ));
}
