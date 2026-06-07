//! Determinism, reactivity, empty-library, and rand-stability tests for the
//! artifact runtime.
//!
//! These tests drive the **pure** runtime path only: a scripted sequence of
//! `(FeatureFrame, dt)` fed through [`ArtifactRuntime::frame`], comparing the
//! staged geometry bit-for-bit. No GPU, no window, no wall clock.

use viz_app::app_state::{select_view, AppView, NO_ARTIFACTS_MESSAGE};
use viz_app::runtime::ArtifactRuntime;
use viz_audio::PermissionState;
use viz_contract::load_artifact;
use viz_core::FeatureFrame;
use viz_render::LayerDraw;

/// The shipped built-in (compiled in for the test).
const SPECTRUM_BARS: &str =
    include_str!("../../../assets/builtin-artifacts/spectrum-bars.artifact.json");

/// A tiny artifact whose element x position is driven by `rand(i)` — used to prove
/// `rand(k)` is stable across frames with identical inputs (see
/// `docs/reference/artifact-contract.md` §2.2).
const RAND_ARTIFACT: &str = r#"{
  "contract": "1.0",
  "meta": { "id": "rand-dots", "name": "Rand Dots" },
  "scene": [
    {
      "type": "instanced",
      "shape": "circle",
      "count": "32",
      "element": {
        "x": "-1 + 2 * rand(i)",
        "y": "-1 + 2 * rand(i + 100)",
        "w": "0.05",
        "h": "0.05",
        "color": { "model": "rgba", "r": "1", "g": "1", "b": "1", "a": "1" }
      }
    }
  ]
}"#;

/// A flattened, owned copy of one frame's staged geometry, for bit comparison.
#[derive(Clone, Debug, PartialEq)]
struct FrameCapture {
    /// `(kind_tag, bit-pattern stream)` per drawn layer, in scene order.
    layers: Vec<LayerCapture>,
    /// Feedback decay bit pattern (`None` ⇒ no trails).
    decay: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
enum LayerCapture {
    Instanced(Vec<u32>),
    Polyline(Vec<u32>),
    Field(Vec<u32>),
}

/// Captures one frame's staging as bit patterns so comparison is exact (no f32 ==
/// pitfalls). The VM sanitizes NaN to 0, so no NaN bit patterns appear.
fn capture(rt: &mut ArtifactRuntime, f: &FeatureFrame, dt: f64) -> FrameCapture {
    let scene = rt.frame(f, dt, 16.0 / 9.0, 1.0);
    let decay = scene.params.feedback_decay.map(f32::to_bits);
    let mut layers = Vec::new();
    for layer in &scene.layers {
        match layer {
            LayerDraw::Instanced { instances, .. } => {
                let mut bits = Vec::with_capacity(instances.len() * 9);
                for inst in *instances {
                    bits.push(inst.x.to_bits());
                    bits.push(inst.y.to_bits());
                    bits.push(inst.w.to_bits());
                    bits.push(inst.h.to_bits());
                    bits.push(inst.rot.to_bits());
                    for c in inst.color {
                        bits.push(c.to_bits());
                    }
                }
                layers.push(LayerCapture::Instanced(bits));
            }
            LayerDraw::Polyline { points, .. } => {
                let mut bits = Vec::with_capacity(points.len() * 6);
                for p in *points {
                    bits.push(p.x.to_bits());
                    bits.push(p.y.to_bits());
                    for c in p.color {
                        bits.push(c.to_bits());
                    }
                }
                layers.push(LayerCapture::Polyline(bits));
            }
            LayerDraw::Field { cells, .. } => {
                let mut bits = Vec::with_capacity(cells.len() * 4);
                for cell in *cells {
                    for c in cell {
                        bits.push(c.to_bits());
                    }
                }
                layers.push(LayerCapture::Field(bits));
            }
        }
    }
    FrameCapture { layers, decay }
}

/// Builds a fresh, activated runtime from JSON (panics on load failure — the test
/// fixtures are known-good).
fn fresh_runtime(json: &str, name: &str) -> ArtifactRuntime {
    let loaded = load_artifact(name, json.as_bytes()).expect("fixture loads");
    let mut rt = ArtifactRuntime::new(loaded);
    rt.activate();
    rt
}

/// Scripts a 120-frame synthetic sequence of `(FeatureFrame, dt)`.
///
/// `loud` toggles whether the bands/energy/beat carry signal so the same script
/// can produce both a silent and a loud-broadband sequence.
// The frame is built field-by-field (arrays are filled in loops); a single struct
// literal would be far less readable here.
#[allow(clippy::field_reassign_with_default)]
fn scripted_sequence(loud: bool) -> Vec<(FeatureFrame, f64)> {
    let dt = 1.0 / 60.0;
    let mut out = Vec::with_capacity(120);
    let mut t = 0.0;
    for k in 0..120u32 {
        let mut f = FeatureFrame::default();
        f.t = t;
        if loud {
            // Broadband, time-varying bands and aggregates.
            let phase = k as f32 * 0.1;
            for (b, slot) in f.bands.iter_mut().enumerate() {
                *slot = (0.5 + 0.5 * (phase + b as f32 * 0.2).sin()).clamp(0.0, 1.0);
            }
            f.low = 0.6 + 0.3 * (phase).sin();
            f.mid = 0.5 + 0.3 * (phase * 1.3).sin();
            f.high = 0.4 + 0.3 * (phase * 1.7).sin();
            f.energy = 0.7;
            for (i, w) in f.waveform.iter_mut().enumerate() {
                *w = ((i as f32 * 0.1 + phase).sin()) * 0.8;
            }
            // A beat pulse every 15 frames.
            f.beat = if k % 15 == 0 { 1.0 } else { 0.0 };
            f.beat_count = k / 15;
            f.silence = false;
        }
        out.push((f, if k == 0 { 0.0 } else { dt }));
        t += dt;
    }
    out
}

/// Sums all instance heights across all instanced layers of a capture frame (a
/// cheap reactivity metric — bar heights track the spectrum).
fn sum_instance_heights(rt: &mut ArtifactRuntime, f: &FeatureFrame, dt: f64) -> f64 {
    let scene = rt.frame(f, dt, 16.0 / 9.0, 1.0);
    let mut sum = 0.0;
    for layer in &scene.layers {
        if let LayerDraw::Instanced { instances, .. } = layer {
            for inst in *instances {
                sum += inst.h.abs() as f64;
            }
        }
    }
    sum
}

#[test]
fn identical_inputs_produce_bit_identical_output() {
    let seq = scripted_sequence(true);
    let mut a = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");
    let mut b = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");

    for (i, (f, dt)) in seq.iter().enumerate() {
        let ca = capture(&mut a, f, *dt);
        let cb = capture(&mut b, f, *dt);
        assert_eq!(ca, cb, "frame {i} diverged between two fresh runtimes");
    }
}

#[test]
fn replaying_the_same_sequence_is_reproducible() {
    // Determinism: re-running the whole sequence on a fresh runtime reproduces every
    // frame (not just two runtimes stepped in lockstep).
    let seq = scripted_sequence(true);

    let mut first = Vec::with_capacity(seq.len());
    {
        let mut rt = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");
        for (f, dt) in &seq {
            first.push(capture(&mut rt, f, *dt));
        }
    }
    let mut second = Vec::with_capacity(seq.len());
    {
        let mut rt = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");
        for (f, dt) in &seq {
            second.push(capture(&mut rt, f, *dt));
        }
    }
    assert_eq!(first, second, "the full replay must be bit-identical");
}

#[test]
fn loud_input_reacts_more_than_silence() {
    // A silent sequence vs a loud-broadband sequence must differ measurably (the
    // visualizer must visibly react to audio). Bar heights scale with band energy, so
    // the summed heights of the loud run vastly exceed the silent run.
    let silent = scripted_sequence(false);
    let loud = scripted_sequence(true);

    let mut rt_silent = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");
    let mut rt_loud = fresh_runtime(SPECTRUM_BARS, "spectrum-bars");

    let mut silent_sum = 0.0;
    let mut loud_sum = 0.0;
    for ((sf, sdt), (lf, ldt)) in silent.iter().zip(loud.iter()) {
        silent_sum += sum_instance_heights(&mut rt_silent, sf, *sdt);
        loud_sum += sum_instance_heights(&mut rt_loud, lf, *ldt);
    }

    // Silence → zeroed bands → bar height `2*band(u)` ≈ 0; loud is clearly larger.
    assert!(
        loud_sum > silent_sum + 10.0,
        "loud sum {loud_sum} should exceed silent sum {silent_sum} by a clear margin"
    );
}

#[test]
fn empty_library_state_branch_exists() {
    // Garbage JSON is rejected with a diagnostic…
    let err = load_artifact("garbage", b"this is not json").expect_err("garbage rejected");
    assert!(
        !err.to_string().is_empty(),
        "rejection must carry an English diagnostic"
    );

    // …and the app's no-artifacts state branch selects the explicit message
    // (unit-testing the state-selection logic, not the GUI).
    assert_eq!(
        select_view(false, None, PermissionState::Granted),
        AppView::NoArtifacts
    );
    assert_eq!(NO_ARTIFACTS_MESSAGE, "No artifacts available");
}

#[test]
fn rand_positions_are_stable_across_frames() {
    // rand(i) is a stateless hash of (seed, i) (see
    // `docs/reference/artifact-contract.md` §2.2), so element positions are identical
    // across frames given identical inputs.
    let mut rt = fresh_runtime(RAND_ARTIFACT, "rand-dots");

    let f = FeatureFrame::default();
    let dt = 1.0 / 60.0;

    let frame_a = capture(&mut rt, &f, 0.0);
    let frame_b = capture(&mut rt, &f, dt);
    let frame_c = capture(&mut rt, &f, dt);

    assert_eq!(
        frame_a, frame_b,
        "rand-driven positions must be identical across frames with identical inputs"
    );
    assert_eq!(frame_b, frame_c);

    // And a fresh runtime with the same artifact id reproduces them (seed from id).
    let mut rt2 = fresh_runtime(RAND_ARTIFACT, "rand-dots");
    let frame_d = capture(&mut rt2, &f, 0.0);
    assert_eq!(
        frame_a, frame_d,
        "the same artifact id yields the same rand seed → same positions"
    );
}
