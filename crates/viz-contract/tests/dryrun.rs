//! Authoring dry-run regression artifact.
//!
//! `aurora-rings.artifact.json` was authored from scratch against **only** the
//! published authoring contract
//! (`docs/reference/artifact-contract.md`) and the load
//! diagnostics — never by reading the built-ins or the Rust source. This test keeps
//! it as a permanent regression artifact: it must keep loading cleanly through the
//! full [`load_artifact`] pipeline, and it must keep producing finite geometry/color
//! over a synthetic audio stream (the determinism/containment guarantee of contract
//! §9).
//!
//! The artifact lives under `tests/contract/dryrun/` at the repo root (next to the
//! `tests/contract/hostile/` corpus), not in `assets/builtin-artifacts/`, so it is
//! NOT shipped — it is a test-only authoring exemplar.

use std::path::PathBuf;

use viz_contract::{
    load_artifact, ColorPrograms, LayerPrograms, LoadedArtifact, SettingSlots, SlotLayout,
};
use viz_core::FeatureFrame;
use viz_expr::Vm;

/// Absolute path to the dry-run artifact (repo-root `tests/contract/dryrun/`).
fn dryrun_artifact_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../tests/contract/dryrun/aurora-rings.artifact.json");
    path
}

/// Loads the dry-run artifact through the full pipeline, panicking with the
/// diagnostic on failure (which is exactly the log line a real author would see).
fn load_dryrun() -> LoadedArtifact {
    let path = dryrun_artifact_path();
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    match load_artifact("aurora-rings.artifact.json", &bytes) {
        Ok(loaded) => loaded,
        Err(diag) => panic!("dry-run artifact failed to load: {diag}"),
    }
}

#[test]
fn aurora_rings_loads_cleanly() {
    let loaded = load_dryrun();
    assert_eq!(loaded.id.0, "aurora-rings");
    // The authored scene is the dark field, three rings, and the beat core.
    assert_eq!(loaded.programs.layers.len(), 5, "expected 5 layers");
    // Three declared settings (ring_width, palette, core_glow).
    assert_eq!(loaded.layout.settings.len(), 3, "expected 3 settings");
    // Feedback (trails) was authored.
    assert!(
        loaded.programs.feedback_decay.is_some(),
        "expected a feedback.decay program"
    );
}

/// Writes the documented setting defaults into their slots (number → value,
/// toggle/choice → numeric encoding, color → channels). The dry-run artifact has
/// no color settings; this handles the scalar kinds it does use.
fn install_setting_defaults(vm: &mut Vm, loaded: &LoadedArtifact) {
    use viz_contract::SettingDecl;
    for (name, slots) in &loaded.layout.settings {
        let decl = &loaded.artifact.settings[name];
        match (slots, decl) {
            (SettingSlots::Scalar(slot), SettingDecl::Number { default, .. }) => {
                vm.set_slot(*slot, *default);
            }
            (SettingSlots::Scalar(slot), SettingDecl::Toggle { default, .. }) => {
                vm.set_slot(*slot, if *default { 1.0 } else { 0.0 });
            }
            (
                SettingSlots::Scalar(slot),
                SettingDecl::Choice {
                    options, default, ..
                },
            ) => {
                let idx = options.iter().position(|o| o == default).unwrap_or(0);
                vm.set_slot(*slot, idx as f64);
            }
            (SettingSlots::Color { r, g, b, a }, _) => {
                // No color settings in this artifact, but stay total.
                for slot in [r, g, b, a] {
                    vm.set_slot(*slot, 1.0);
                }
            }
            _ => unreachable!("setting slot/decl kind mismatch for {name}"),
        }
    }
}

/// Writes one synthetic audio frame's shared inputs into the VM slot bank.
#[allow(clippy::too_many_arguments)]
fn install_shared_inputs(
    vm: &mut Vm,
    layout: &SlotLayout,
    frame: &FeatureFrame,
    dt: f64,
    aspect: f64,
) {
    vm.set_slot(layout.t, frame.t);
    vm.set_slot(layout.dt, dt);
    vm.set_slot(layout.energy, frame.energy as f64);
    vm.set_slot(layout.low, frame.low as f64);
    vm.set_slot(layout.mid, frame.mid as f64);
    vm.set_slot(layout.high, frame.high as f64);
    vm.set_slot(layout.beat, frame.beat as f64);
    vm.set_slot(layout.beat_count, frame.beat_count as f64);
    vm.set_slot(layout.aspect, aspect);
}

/// Evaluates a color block at the current VM state, asserting every channel finite.
fn check_color_finite(vm: &mut Vm, color: &ColorPrograms, label: &str) {
    for (ch, prog) in [("c0", &color.c0), ("c1", &color.c1), ("c2", &color.c2)] {
        let v = prog.value(vm);
        assert!(v.is_finite(), "{label}.{ch} not finite: {v}");
    }
    if let Some(a) = &color.a {
        let v = a.value(vm);
        assert!(v.is_finite(), "{label}.a not finite: {v}");
    }
}

/// Drives the artifact over 60 synthetic frames and asserts that every var, every
/// per-frame layer scalar, and a sample of per-element/point/cell geometry & color
/// outputs are finite (contract §9). Mirrors the var read-semantics of
/// contract §5: vars update in declaration order, each reading earlier vars'
/// already-updated values via the shared slot bank.
#[test]
fn aurora_rings_runs_60_synthetic_frames_finite() {
    let loaded = load_dryrun();
    let layout = &loaded.layout;
    let programs = &loaded.programs;

    let mut vm = Vm::new(layout.slot_count, loaded.seed);
    install_setting_defaults(&mut vm, &loaded);

    let aspect = 16.0 / 9.0;
    let dt = 1.0 / 60.0;

    // Initialize vars (contract §5: `init` runs once at activation).
    for (slot, prog) in layout.vars.values().zip(&programs.vars_init) {
        let v = prog.value(&mut vm);
        assert!(v.is_finite(), "var init not finite: {v}");
        vm.set_slot(*slot, v);
    }

    for frame_idx in 0..60u32 {
        let t = frame_idx as f64 * dt;
        // A synthetic but exercising stream: pulsing bands, periodic beats, a
        // silence stretch (frames 40..50) to exercise the zeroed-feature idle path.
        let active = !(40..50).contains(&frame_idx);
        let mut frame = FeatureFrame {
            t,
            ..FeatureFrame::default()
        };
        if active {
            let phase = t * 6.0;
            frame.low = (0.5 + 0.5 * (phase).sin()).clamp(0.0, 1.0) as f32;
            frame.mid = (0.5 + 0.5 * (phase * 1.7 + 1.0).sin()).clamp(0.0, 1.0) as f32;
            frame.high = (0.5 + 0.5 * (phase * 2.3 + 2.0).sin()).clamp(0.0, 1.0) as f32;
            frame.energy = ((frame.low + frame.mid + frame.high) / 3.0).clamp(0.0, 1.0);
            for (k, b) in frame.bands.iter_mut().enumerate() {
                let u = k as f64 / 47.0;
                *b = (0.5 + 0.5 * (u * 9.0 + phase).sin()).clamp(0.0, 1.0) as f32;
            }
            for (k, w) in frame.waveform.iter_mut().enumerate() {
                let u = k as f64 / 255.0;
                *w = (u * std::f64::consts::TAU * 3.0 + phase).sin() as f32;
            }
            // A beat every 12 frames (frame 0, 12, 24, 36 within the active span).
            if frame_idx % 12 == 0 {
                frame.beat = 1.0;
                frame.beat_count = frame_idx / 12 + 1;
            } else {
                frame.beat = (0.9f32).powi((frame_idx % 12) as i32);
                frame.beat_count = frame_idx / 12 + 1;
            }
            frame.silence = false;
        }

        vm.set_frame(&frame);
        install_shared_inputs(&mut vm, layout, &frame, dt, aspect);

        // Per-frame: update vars in declaration order (contract §5 read semantics).
        for (slot, prog) in layout.vars.values().zip(&programs.vars_frame) {
            let v = prog.value(&mut vm);
            assert!(
                v.is_finite(),
                "frame {frame_idx}: var frame not finite: {v}"
            );
            vm.set_slot(*slot, v);
        }

        // feedback.decay (frame stage).
        if let Some(decay) = &programs.feedback_decay {
            let v = decay.value(&mut vm);
            assert!(
                v.is_finite(),
                "frame {frame_idx}: feedback.decay not finite: {v}"
            );
        }

        // Each layer: frame-stage scalars, then a sample of per-instance/point/cell.
        for (li, layer) in programs.layers.iter().enumerate() {
            match layer {
                LayerPrograms::Instanced {
                    count,
                    visible,
                    x,
                    y,
                    w,
                    h,
                    rot,
                    color,
                } => {
                    let n = count.value(&mut vm);
                    assert!(
                        n.is_finite(),
                        "frame {frame_idx} layer {li}: count not finite"
                    );
                    if let Some(vis) = visible {
                        assert!(vis.value(&mut vm).is_finite());
                    }
                    let count_i = (n.floor() as i64).clamp(1, 4096);
                    let samples = sample_indices(count_i);
                    for i in samples {
                        let u = i as f64 / (count_i.max(2) - 1) as f64;
                        vm.set_slot(layout.i, i as f64);
                        vm.set_slot(layout.n, count_i as f64);
                        vm.set_slot(layout.u, u);
                        for (name, prog) in [("x", x), ("y", y), ("w", w), ("h", h)] {
                            let v = prog.value(&mut vm);
                            assert!(
                                v.is_finite(),
                                "frame {frame_idx} layer {li} elem {i}: {name} not finite: {v}"
                            );
                        }
                        if let Some(rot) = rot {
                            assert!(rot.value(&mut vm).is_finite());
                        }
                        check_color_finite(
                            &mut vm,
                            color,
                            &format!("frame {frame_idx} layer {li} elem {i}"),
                        );
                    }
                }
                LayerPrograms::Polyline {
                    points,
                    thickness,
                    visible,
                    x,
                    y,
                    color,
                    closed: _,
                } => {
                    let p = points.value(&mut vm);
                    assert!(
                        p.is_finite(),
                        "frame {frame_idx} layer {li}: points not finite"
                    );
                    assert!(
                        thickness.value(&mut vm).is_finite(),
                        "frame {frame_idx} layer {li}: thickness not finite"
                    );
                    if let Some(vis) = visible {
                        assert!(vis.value(&mut vm).is_finite());
                    }
                    let count_i = (p.floor() as i64).clamp(2, 4096);
                    for i in sample_indices(count_i) {
                        let u = i as f64 / (count_i - 1) as f64;
                        vm.set_slot(layout.i, i as f64);
                        vm.set_slot(layout.n, count_i as f64);
                        vm.set_slot(layout.u, u);
                        for (name, prog) in [("x", x), ("y", y)] {
                            let v = prog.value(&mut vm);
                            assert!(
                                v.is_finite(),
                                "frame {frame_idx} layer {li} point {i}: {name} not finite: {v}"
                            );
                        }
                        check_color_finite(
                            &mut vm,
                            color,
                            &format!("frame {frame_idx} layer {li} point {i}"),
                        );
                    }
                }
                LayerPrograms::Field {
                    resolution,
                    visible,
                    color,
                } => {
                    let r = resolution.value(&mut vm);
                    assert!(
                        r.is_finite(),
                        "frame {frame_idx} layer {li}: resolution not finite"
                    );
                    if let Some(vis) = visible {
                        assert!(vis.value(&mut vm).is_finite());
                    }
                    let res = (r.floor() as i64).clamp(2, 128);
                    let cells = res * res;
                    for i in sample_indices(cells) {
                        let row = i / res;
                        let col = i % res;
                        // Cell centers in −1..1 (contract §6.3; y up).
                        let cx = -1.0 + (2.0 * col as f64 + 1.0) / res as f64;
                        let cy = -1.0 + (2.0 * row as f64 + 1.0) / res as f64;
                        vm.set_slot(layout.i, i as f64);
                        vm.set_slot(layout.n, cells as f64);
                        vm.set_slot(layout.x, cx);
                        vm.set_slot(layout.y, cy);
                        check_color_finite(
                            &mut vm,
                            color,
                            &format!("frame {frame_idx} layer {li} cell {i}"),
                        );
                    }
                }
            }
        }
    }
}

/// A small, deterministic sample of indices in `0..count` (first, last, and a few
/// interior points) — keeps the test fast while exercising the `u`/`x`/`y` range.
fn sample_indices(count: i64) -> Vec<i64> {
    if count <= 1 {
        return vec![0];
    }
    let mut out = vec![0, count - 1];
    for d in 1..8 {
        out.push((count * d / 8).clamp(0, count - 1));
    }
    out.sort_unstable();
    out.dedup();
    out
}
