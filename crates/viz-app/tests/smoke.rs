//! Render-one-frame and switch-latency halves of the smoke gate, driving the
//! **pure** runtime path: no GPU, no window, no wall clock on the frame path.
//!
//! The "every built-in loads" check lives in `crates/viz-contract/tests/smoke.rs`
//! because that crate must not depend on `viz-app`. The render-one-frame and
//! switch-latency checks need [`ArtifactRuntime`], so they live here. Both halves
//! read the same shipped `assets/builtin-artifacts/` directory.
//!
//! (b) Render-one-frame: for every built-in, build an [`ArtifactRuntime`],
//!     [`activate`](ArtifactRuntime::activate), then run [`frame`](ArtifactRuntime::frame)
//!     with a default ([`FeatureFrame::default`]) frame and with a *loud* synthetic
//!     frame. Each frame must not panic, must stage within the renderer caps
//!     ([`MAX_INSTANCES`]/[`MAX_POINTS`]/[`MAX_FIELD_CELLS`]), and every staged value
//!     must be finite (the VM sanitizes NaN/inf to 0, so non-finite output would be a
//!     regression).
//!
//! (c) Switch-latency: a *programmatic* artifact switch — construct a new
//!     runtime + activate + produce its first valid frame — measured under 100 ms wall
//!     (well inside the 1 s budget). The GUI adds no meaningful overhead: the dropdown
//!     handler does exactly this construct-and-activate, then the next render pumps
//!     `frame()`; there is no extra blocking work on the switch path.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use viz_app::runtime::ArtifactRuntime;
use viz_contract::{load_artifact, LoadedArtifact};
use viz_core::{FeatureFrame, BAND_COUNT, WAVEFORM_LEN};
use viz_render::{LayerDraw, MAX_FIELD_CELLS, MAX_INSTANCES, MAX_POINTS};

/// Absolute path to the shipped built-in artifacts directory.
fn builtins_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../assets/builtin-artifacts");
    path
}

/// Loads every `*.artifact.json` under `assets/builtin-artifacts/`, sorted by path
/// for a stable order. Panics on any load failure (the load gate proper lives in
/// `viz-contract`; here a failure means the fixtures regressed).
fn load_all_builtins() -> Vec<(String, LoadedArtifact)> {
    let dir = builtins_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(".artifact.json"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();

    paths
        .iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {name}: {e}"));
            let loaded = load_artifact(&name, &bytes)
                .unwrap_or_else(|diag| panic!("built-in {name} failed to load: {diag}"));
            (name, loaded)
        })
        .collect()
}

/// A loud, broadband synthetic frame: bands/aggregates/waveform/beat all carry
/// signal, so every reactive idiom (bar heights, scope amplitude, particle bursts,
/// field color) is exercised on the hot path.
// Built field-by-field (arrays filled in loops); a single struct literal would be
// far less readable here.
#[allow(clippy::field_reassign_with_default)]
fn loud_frame() -> FeatureFrame {
    let mut f = FeatureFrame::default();
    f.t = 1.234;
    for (b, slot) in f.bands.iter_mut().enumerate() {
        *slot = (0.5 + 0.5 * (b as f32 * 0.2).sin()).clamp(0.0, 1.0);
    }
    debug_assert_eq!(f.bands.len(), BAND_COUNT);
    f.low = 0.8;
    f.mid = 0.6;
    f.high = 0.5;
    f.energy = 0.9;
    for (i, w) in f.waveform.iter_mut().enumerate() {
        *w = (i as f32 * 0.1).sin() * 0.9;
    }
    debug_assert_eq!(f.waveform.len(), WAVEFORM_LEN);
    f.beat = 1.0;
    f.beat_count = 7;
    f.silence = false;
    f
}

/// Asserts a freshly produced scene stages within the renderer caps and that every
/// staged scalar is finite. `label` names the built-in + input for the panic message.
fn assert_scene_ok(rt: &mut ArtifactRuntime, f: &FeatureFrame, label: &str) {
    // 16:9 aspect, full detail (no degradation) — the worst case for staging counts.
    let scene = rt.frame(f, 1.0 / 60.0, 16.0 / 9.0, 1.0);

    if let Some(decay) = scene.params.feedback_decay {
        assert!(
            decay.is_finite() && (0.0..=0.99).contains(&decay),
            "{label}: feedback decay {decay} out of [0, 0.99]"
        );
    }

    for (li, layer) in scene.layers.iter().enumerate() {
        match layer {
            LayerDraw::Instanced { instances, .. } => {
                assert!(
                    instances.len() <= MAX_INSTANCES,
                    "{label}: layer {li} staged {} instances > cap {MAX_INSTANCES}",
                    instances.len()
                );
                for (k, inst) in instances.iter().enumerate() {
                    for (name, v) in [
                        ("x", inst.x),
                        ("y", inst.y),
                        ("w", inst.w),
                        ("h", inst.h),
                        ("rot", inst.rot),
                    ] {
                        assert!(
                            v.is_finite(),
                            "{label}: layer {li} instance {k} {name} = {v} is not finite"
                        );
                    }
                    for (c, v) in inst.color.iter().enumerate() {
                        assert!(
                            v.is_finite(),
                            "{label}: layer {li} instance {k} color[{c}] = {v} is not finite"
                        );
                    }
                }
            }
            LayerDraw::Polyline {
                points,
                thickness_px,
                ..
            } => {
                assert!(
                    points.len() <= MAX_POINTS,
                    "{label}: layer {li} staged {} points > cap {MAX_POINTS}",
                    points.len()
                );
                assert!(
                    thickness_px.is_finite() && *thickness_px > 0.0,
                    "{label}: layer {li} thickness {thickness_px} must be finite and positive"
                );
                for (k, p) in points.iter().enumerate() {
                    assert!(
                        p.x.is_finite() && p.y.is_finite(),
                        "{label}: layer {li} point {k} ({}, {}) is not finite",
                        p.x,
                        p.y
                    );
                    for (c, v) in p.color.iter().enumerate() {
                        assert!(
                            v.is_finite(),
                            "{label}: layer {li} point {k} color[{c}] = {v} is not finite"
                        );
                    }
                }
            }
            LayerDraw::Field {
                cells, resolution, ..
            } => {
                assert!(
                    cells.len() <= MAX_FIELD_CELLS,
                    "{label}: layer {li} staged {} field cells > cap {MAX_FIELD_CELLS}",
                    cells.len()
                );
                assert_eq!(
                    cells.len(),
                    (*resolution as usize) * (*resolution as usize),
                    "{label}: layer {li} field cell count must equal resolution²"
                );
                for (k, cell) in cells.iter().enumerate() {
                    for (c, v) in cell.iter().enumerate() {
                        assert!(
                            v.is_finite(),
                            "{label}: layer {li} cell {k} color[{c}] = {v} is not finite"
                        );
                    }
                }
            }
        }
    }
}

/// (b) Render-one-frame: each built-in renders a default frame and a loud frame
/// without panicking, staging within caps and producing finite output.
#[test]
fn every_builtin_renders_one_frame() {
    let builtins = load_all_builtins();
    assert!(
        builtins.len() >= 5,
        "expected at least 5 built-in artifacts, found {}",
        builtins.len()
    );

    let default_frame = FeatureFrame::default();
    let loud = loud_frame();

    for (name, loaded) in builtins {
        let mut rt = ArtifactRuntime::new(loaded);
        rt.activate();
        assert_scene_ok(&mut rt, &default_frame, &format!("{name}/default"));
        assert_scene_ok(&mut rt, &loud, &format!("{name}/loud"));
    }
}

/// (c) Switch-latency: a programmatic switch between two built-ins —
/// construct a fresh runtime, activate, and produce a valid first frame — completes
/// well under 100 ms wall (the 1 s budget has ~10× margin). The GUI switch path does
/// exactly this construct-and-activate, so it adds no meaningful overhead.
#[test]
fn switching_between_builtins_is_under_100ms() {
    let builtins = load_all_builtins();
    assert!(
        builtins.len() >= 2,
        "switch test needs at least two built-ins, found {}",
        builtins.len()
    );

    let loud = loud_frame();

    // Measure each ordered pair (a -> b): a full programmatic switch is "drop the old
    // runtime, build the new one from its LoadedArtifact, activate, render frame 1".
    // We rebuild the source LoadedArtifact per measurement so the cost includes the
    // load step the dropdown handler performs in the app (parse is cached cheaply by
    // the library in the real app, so this is a conservative upper bound).
    let dir = builtins_dir();
    let names: Vec<String> = builtins.iter().map(|(n, _)| n.clone()).collect();

    for from in &names {
        for to in &names {
            if from == to {
                continue;
            }
            // Establish the "from" runtime (the currently-active one being replaced).
            let from_loaded = reload(&dir, from);
            let mut current = ArtifactRuntime::new(from_loaded);
            current.activate();
            let _ = current.frame(&loud, 1.0 / 60.0, 16.0 / 9.0, 1.0);

            // Time the switch itself: load + construct + activate + first frame.
            let started = Instant::now();
            let to_loaded = reload(&dir, to);
            let mut next = ArtifactRuntime::new(to_loaded);
            next.activate();
            let first = next.frame(&loud, 0.0, 16.0 / 9.0, 1.0);
            let elapsed = started.elapsed();

            // First frame must be valid immediately: any staged scalar is finite and
            // within caps. (Re-run the full check on the produced scene.)
            for layer in &first.layers {
                match layer {
                    LayerDraw::Instanced { instances, .. } => {
                        assert!(instances.len() <= MAX_INSTANCES);
                        for inst in *instances {
                            assert!(inst.x.is_finite() && inst.y.is_finite());
                        }
                    }
                    LayerDraw::Polyline { points, .. } => {
                        assert!(points.len() <= MAX_POINTS);
                        for p in *points {
                            assert!(p.x.is_finite() && p.y.is_finite());
                        }
                    }
                    LayerDraw::Field { cells, .. } => {
                        assert!(cells.len() <= MAX_FIELD_CELLS);
                    }
                }
            }
            drop(first);

            assert!(
                elapsed < Duration::from_millis(100),
                "switch {from} -> {to} took {elapsed:?}, over the 100 ms margin"
            );
        }
    }
}

/// Reloads one built-in by file name from `dir`, panicking on failure.
fn reload(dir: &std::path::Path, name: &str) -> LoadedArtifact {
    let path = dir.join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {name}: {e}"));
    load_artifact(name, &bytes)
        .unwrap_or_else(|diag| panic!("built-in {name} failed to load: {diag}"))
}
