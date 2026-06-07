//! The hostile artifact suite: malformed and adversarial artifacts that must
//! either be rejected cleanly or be contained at runtime, never crash.
//!
//! The corpus lives in `tests/contract/hostile/` (a plain folder, read by path —
//! not a crate). Every `.artifact.json` there is either:
//!
//! - **rejected** by [`load_artifact`] with a [`LoadDiagnostic`] whose `step`,
//!   `json_path`, and `message` are all non-empty, English, and actionable; or
//! - **runtime-contained** — it LOADS (the offending construct is a *value*, not a
//!   shape error: division-by-zero, `log(-1)`, `asin(2)`, an absurd `count`/
//!   `resolution`, zero `points`) and then survives 60 frames of VM evaluation
//!   producing **only finite outputs**, never a panic (NaN/Inf containment).
//!
//! No file in the suite may both fail to load *and* be on the runtime-containment
//! list; the suite asserts that partition is exact.
//!
//! ## Diagnostic-quality assertions
//! For a curated subset, [`diagnostic_quality_for_curated_subset`] asserts the
//! EXACT [`LoadStep`] classification (Version / Schema / Semantic / Formula), that
//! `json_path` points at the right node (e.g. `scene[0].cell.color.h`,
//! `vars.flash.frame`, `settings.beat`), and that the offending token appears in
//! the English message.
//!
//! ## Duplicate ids
//! Duplicate-id detection is a *library-level* rule, not a single-file load failure,
//! so the duplicate pair (`17-duplicate-id-a` / `-b`) is exercised separately in
//! [`duplicate_ids_rejected_via_library`] by copying both into a temp dir and
//! feeding them through [`ArtifactLibrary::push_source`] (the same path a folder
//! scan would use). Each file loads fine on its own; the *second* is rejected as a
//! duplicate. The suite stays fully deterministic — fixed inputs, fixed frame data.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use viz_contract::{
    load_artifact, ArtifactLibrary, ArtifactSource, CompiledFormula, EntryStatus, LayerPrograms,
    LoadStep, LoadedArtifact, SettingSlots, SlotLayout,
};
use viz_expr::Vm;

// ---------------------------------------------------------------------------
// Corpus location & the runtime-containment partition
// ---------------------------------------------------------------------------

/// Path to the hostile corpus folder (read by path, not compiled in).
fn hostile_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../tests/contract/hostile");
    p
}

/// File names that must LOAD and then be runtime-contained (finite over 60 frames),
/// rather than rejected at load. These carry numerically-degenerate *values*
/// (div-by-zero, `log(-1)`, `asin` out of range, absurd `count`/`resolution`, zero
/// `points`) that the loader accepts and the VM contains.
const RUNTIME_CONTAINED: &[&str] = &[
    "11-division-by-zero.artifact.json",
    "12-log-of-negative.artifact.json",
    "13-asin-out-of-range.artifact.json",
    "14-count-1e9.artifact.json",
    "15-resolution-1e6.artifact.json",
    "16-points-zero.artifact.json",
    "30-runtime-mixed-containment.artifact.json",
];

/// File names that load on their own but participate in the *library-level*
/// duplicate-id check; they are excluded from the per-file reject/contain partition
/// and tested in [`duplicate_ids_rejected_via_library`].
const DUPLICATE_PAIR: &[&str] = &[
    "17-duplicate-id-a.artifact.json",
    "18-duplicate-id-b.artifact.json",
];

/// Lists the corpus `.artifact.json` files, sorted for determinism.
fn corpus_files() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(hostile_dir())
        .expect("hostile corpus directory must exist")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n.ends_with(".artifact.json"))
        .collect();
    names.sort();
    names
}

fn read_corpus(name: &str) -> Vec<u8> {
    std::fs::read(hostile_dir().join(name)).unwrap_or_else(|e| panic!("reading {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Suite size & partition sanity
// ---------------------------------------------------------------------------

/// The corpus has at least 20 hostile files and the partition lists name
/// only real files.
#[test]
fn corpus_has_at_least_twenty_files() {
    let files: BTreeSet<String> = corpus_files().into_iter().collect();
    assert!(
        files.len() >= 20,
        "hostile corpus must hold >= 20 files, found {}: {files:?}",
        files.len()
    );

    for name in RUNTIME_CONTAINED.iter().chain(DUPLICATE_PAIR) {
        assert!(
            files.contains(*name),
            "partition references missing corpus file: {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Every file: rejected-with-quality OR loads-and-stays-finite, no panic
// ---------------------------------------------------------------------------

/// Core guarantee: for **every** corpus file, either it is
/// rejected with a well-formed [`LoadDiagnostic`] (non-empty English step / path /
/// message), or it is a runtime-containment file that loads and then runs 60 frames
/// of VM evaluation producing only finite values. Zero panics overall.
#[test]
fn every_hostile_file_rejected_cleanly_or_runtime_contained() {
    for name in corpus_files() {
        // The duplicate pair is a library-level rule, tested separately.
        if DUPLICATE_PAIR.contains(&name.as_str()) {
            continue;
        }

        let bytes = read_corpus(&name);
        let must_contain = RUNTIME_CONTAINED.contains(&name.as_str());

        match load_artifact(&name, &bytes) {
            Ok(loaded) => {
                assert!(
                    must_contain,
                    "file '{name}' loaded but is not on the runtime-containment list; \
                     a hostile file must either be rejected or explicitly contained"
                );
                // Runtime containment: 60 frames, all outputs finite, no panic.
                run_sixty_frames_finite(&name, &loaded);
            }
            Err(diag) => {
                assert!(
                    !must_contain,
                    "file '{name}' was rejected at load but is listed as runtime-contained: {diag}"
                );
                assert_quality(&name, &diag.step, &diag.json_path, &diag.message);
            }
        }
    }
}

/// A rejection diagnostic must be high quality: a real step, a non-empty English
/// message, and (for every step except size/parse, which have no instance node) a
/// non-empty `json_path`.
fn assert_quality(name: &str, step: &LoadStep, json_path: &str, message: &str) {
    // Message: non-empty, and looks like English (has a letter and a space).
    assert!(
        !message.trim().is_empty(),
        "file '{name}': diagnostic message must be non-empty"
    );
    assert!(
        message.chars().any(|c| c.is_ascii_alphabetic()) && message.contains(' '),
        "file '{name}': diagnostic message must read as English, got: {message:?}"
    );
    assert!(
        message.is_ascii(),
        "file '{name}': diagnostic message must be plain ASCII English, got: {message:?}"
    );

    // Path localization. Some steps target the whole document and legitimately
    // report an empty path (the offending location IS the root), with the message
    // carrying the detail instead:
    //   - Parse:       the size gate / JSON syntax — no instance node at all;
    //   - Schema:      a failing instance *may* be the document root (e.g. an
    //                  unknown top-level key — the property sits at the root);
    //   - Deserialize: the loader does not synthesize a serde path.
    // Every other step (Version / Semantic / Formula) localizes to a specific node
    // and must carry a non-empty path.
    if matches!(
        step,
        LoadStep::Version | LoadStep::Semantic | LoadStep::Formula
    ) {
        assert!(
            !json_path.trim().is_empty(),
            "file '{name}': step {step:?} must report a non-empty json_path"
        );
    } else if json_path.trim().is_empty() {
        // A root-level rejection must still localize via the message (it names the
        // offending key / construct), so the diagnostic stays actionable.
        assert!(
            message.len() > 10,
            "file '{name}': root-level {step:?} rejection must carry a detailed message, \
             got: {message:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Runtime-containment harness — mirrors the runtime's per-frame VM writes
// ---------------------------------------------------------------------------

/// Adversarial-but-deterministic feature inputs for frame `k` (60 frames). The
/// values deliberately include zeros and ones so that div-by-zero / log(0) etc.
/// are actually exercised, and `t` advances so `tan(pi/2)`-style poles are hit.
struct Feat {
    t: f64,
    dt: f64,
    energy: f64,
    low: f64,
    mid: f64,
    high: f64,
    beat: f64,
    beat_count: f64,
    aspect: f64,
}

fn feature_for(k: usize) -> Feat {
    let kf = k as f64;
    Feat {
        // Pass through pi/2 so tan() hits a pole on some frame.
        t: kf * (std::f64::consts::FRAC_PI_2 / 7.0),
        dt: 1.0 / 60.0,
        // Spread across 0..1 including the exact endpoints 0 and 1.
        energy: if k.is_multiple_of(5) {
            0.0
        } else {
            (kf % 11.0) / 10.0
        },
        low: if k.is_multiple_of(3) { 0.0 } else { 1.0 },
        mid: (kf * 0.37) % 1.0,
        high: 1.0 - ((kf * 0.19) % 1.0),
        beat: if k.is_multiple_of(8) { 1.0 } else { 0.0 },
        beat_count: kf,
        aspect: 16.0 / 9.0,
    }
}

/// Writes the nine shared inputs for one frame.
fn write_shared(vm: &mut Vm, layout: &SlotLayout, f: &Feat) {
    vm.set_slot(layout.t, f.t);
    vm.set_slot(layout.dt, f.dt);
    vm.set_slot(layout.energy, f.energy);
    vm.set_slot(layout.low, f.low);
    vm.set_slot(layout.mid, f.mid);
    vm.set_slot(layout.high, f.high);
    vm.set_slot(layout.beat, f.beat);
    vm.set_slot(layout.beat_count, f.beat_count);
    vm.set_slot(layout.aspect, f.aspect);
}

/// Writes each setting's current value to its slot(s) from its declared default,
/// mirroring the runtime so setting-driven formulas evaluate against real inputs.
fn write_settings(vm: &mut Vm, loaded: &LoadedArtifact) {
    use viz_contract::SettingDecl;
    for (name, decl) in &loaded.artifact.settings {
        let slots = &loaded.layout.settings[name];
        match (decl, slots) {
            (SettingDecl::Number { default, .. }, SettingSlots::Scalar(s)) => {
                vm.set_slot(*s, *default);
            }
            (SettingDecl::Toggle { default, .. }, SettingSlots::Scalar(s)) => {
                vm.set_slot(*s, if *default { 1.0 } else { 0.0 });
            }
            (
                SettingDecl::Choice {
                    options, default, ..
                },
                SettingSlots::Scalar(s),
            ) => {
                let idx = options.iter().position(|o| o == default).unwrap_or(0);
                vm.set_slot(*s, idx as f64);
            }
            (SettingDecl::Color { .. }, SettingSlots::Color { r, g, b, a }) => {
                // Exact channel decoding is the runtime's job; for containment we
                // only need finite inputs — mid-grey is fine and deterministic.
                vm.set_slot(*r, 0.5);
                vm.set_slot(*g, 0.5);
                vm.set_slot(*b, 0.5);
                vm.set_slot(*a, 1.0);
            }
            _ => unreachable!("setting kind / slot shape mismatch is impossible by construction"),
        }
    }
}

/// Evaluates one [`CompiledFormula`] and asserts the result is finite.
fn check_finite(name: &str, label: &str, frame: usize, f: &CompiledFormula, vm: &mut Vm) {
    let v = f.value(vm);
    assert!(
        v.is_finite(),
        "file '{name}': formula '{label}' produced a non-finite value {v} on frame {frame}"
    );
}

/// Evaluates a color program's channels for finiteness.
fn check_color(
    name: &str,
    base: &str,
    frame: usize,
    color: &viz_contract::ColorPrograms,
    vm: &mut Vm,
) {
    check_finite(name, &format!("{base}.c0"), frame, &color.c0, vm);
    check_finite(name, &format!("{base}.c1"), frame, &color.c1, vm);
    check_finite(name, &format!("{base}.c2"), frame, &color.c2, vm);
    if let Some(a) = &color.a {
        check_finite(name, &format!("{base}.a"), frame, a, vm);
    }
}

/// Drives 60 frames through a VM built from `loaded`, mirroring the runtime's slot
/// writes (shared inputs, settings, var init/frame, layer count/visible/resolution/
/// thickness, and a handful of per-element/point/cell indices with `i`/`n`/`u`/`x`/
/// `y`). Every evaluated value must be finite — the NaN/Inf containment guarantee.
fn run_sixty_frames_finite(name: &str, loaded: &LoadedArtifact) {
    let layout = &loaded.layout;
    let programs = &loaded.programs;
    let mut vm = Vm::new(layout.slot_count, loaded.seed);

    // Activate: write settings, then run var init programs into their slots.
    let f0 = feature_for(0);
    write_shared(&mut vm, layout, &f0);
    write_settings(&mut vm, loaded);
    for (idx, slot) in layout.vars.values().enumerate() {
        check_finite(name, "vars.init", 0, &programs.vars_init[idx], &mut vm);
        let v = programs.vars_init[idx].value(&mut vm);
        vm.set_slot(*slot, v);
    }

    for k in 0..60 {
        let f = feature_for(k);
        write_shared(&mut vm, layout, &f);
        write_settings(&mut vm, loaded);

        // Var frame programs, written back into their slots (declaration order).
        for (idx, slot) in layout.vars.values().enumerate() {
            check_finite(name, "vars.frame", k, &programs.vars_frame[idx], &mut vm);
            let v = programs.vars_frame[idx].value(&mut vm);
            vm.set_slot(*slot, v);
        }

        // feedback.decay (frame stage).
        if let Some(decay) = &programs.feedback_decay {
            check_finite(name, "feedback.decay", k, decay, &mut vm);
        }

        // Each layer: frame-stage scalars, then a few element/point/cell samples.
        for (li, layer) in programs.layers.iter().enumerate() {
            eval_layer_finite(name, li, k, layer, layout, &mut vm);
        }
    }
}

/// A small fixed set of `(i, n)` index samples to exercise per-element/point/cell
/// programs — including `i == 0`, `n == 1` (so `u = i/max(n-1,1)` divides cleanly),
/// and a mid-range pair.
const INDEX_SAMPLES: &[(usize, usize)] = &[(0, 1), (0, 8), (3, 8), (7, 8)];

fn eval_layer_finite(
    name: &str,
    li: usize,
    frame: usize,
    layer: &LayerPrograms,
    layout: &SlotLayout,
    vm: &mut Vm,
) {
    let base = format!("scene[{li}]");
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
            check_finite(name, &format!("{base}.count"), frame, count, vm);
            if let Some(v) = visible {
                check_finite(name, &format!("{base}.visible"), frame, v, vm);
            }
            for &(i, n) in INDEX_SAMPLES {
                write_extras(vm, layout, i, n, 0.0, 0.0);
                let ep = format!("{base}.element");
                check_finite(name, &format!("{ep}.x"), frame, x, vm);
                check_finite(name, &format!("{ep}.y"), frame, y, vm);
                check_finite(name, &format!("{ep}.w"), frame, w, vm);
                check_finite(name, &format!("{ep}.h"), frame, h, vm);
                if let Some(r) = rot {
                    check_finite(name, &format!("{ep}.rot"), frame, r, vm);
                }
                check_color(name, &format!("{ep}.color"), frame, color, vm);
            }
        }
        LayerPrograms::Polyline {
            points,
            thickness,
            visible,
            x,
            y,
            color,
            ..
        } => {
            check_finite(name, &format!("{base}.points"), frame, points, vm);
            check_finite(name, &format!("{base}.thickness"), frame, thickness, vm);
            if let Some(v) = visible {
                check_finite(name, &format!("{base}.visible"), frame, v, vm);
            }
            for &(i, n) in INDEX_SAMPLES {
                write_extras(vm, layout, i, n, 0.0, 0.0);
                let pp = format!("{base}.point");
                check_finite(name, &format!("{pp}.x"), frame, x, vm);
                check_finite(name, &format!("{pp}.y"), frame, y, vm);
                check_color(name, &format!("{pp}.color"), frame, color, vm);
            }
        }
        LayerPrograms::Field {
            resolution,
            visible,
            color,
        } => {
            check_finite(name, &format!("{base}.resolution"), frame, resolution, vm);
            if let Some(v) = visible {
                check_finite(name, &format!("{base}.visible"), frame, v, vm);
            }
            // Sample a few cell centers including the exact extremes (-1, 0, 1).
            let coords = [(-1.0_f64, -1.0_f64), (0.0, 0.0), (1.0, 1.0), (-1.0, 1.0)];
            for (ci, &(cx, cy)) in coords.iter().enumerate() {
                write_extras(vm, layout, ci, coords.len(), cx, cy);
                check_color(name, &format!("{base}.cell.color"), frame, color, vm);
            }
        }
    }
}

/// Writes the five stage-extra slots (`i`, `n`, `u`, `x`, `y`) for one sample,
/// computing `u = i / max(n-1, 1)` exactly as the runtime does.
fn write_extras(vm: &mut Vm, layout: &SlotLayout, i: usize, n: usize, x: f64, y: f64) {
    let denom = (n.max(2) - 1) as f64;
    vm.set_slot(layout.i, i as f64);
    vm.set_slot(layout.n, n as f64);
    vm.set_slot(layout.u, i as f64 / denom);
    vm.set_slot(layout.x, x);
    vm.set_slot(layout.y, y);
}

// ---------------------------------------------------------------------------
// Exact diagnostic-quality assertions for a curated subset
// ---------------------------------------------------------------------------

/// One expected diagnostic for a curated file: exact step, exact `json_path`, and a
/// substring that MUST appear in the message (the offending token).
struct Expected {
    file: &'static str,
    step: LoadStep,
    json_path: &'static str,
    message_contains: &'static str,
}

/// The curated subset — one representative per pipeline step, each pinning the
/// exact [`LoadStep`], the exact `json_path` (pointing at the offending node), and
/// the offending token in the message.
const CURATED: &[Expected] = &[
    // Version step: the raw version string is named verbatim.
    Expected {
        file: "06-contract-2-0.artifact.json",
        step: LoadStep::Version,
        json_path: "contract",
        message_contains: "2.0",
    },
    Expected {
        file: "07-contract-1-99.artifact.json",
        step: LoadStep::Version,
        json_path: "contract",
        message_contains: "1.99",
    },
    // Schema step: the offending property is localized at the layer it sits in.
    Expected {
        file: "02-unknown-top-level-key.artifact.json",
        step: LoadStep::Schema,
        json_path: "",
        message_contains: "colour",
    },
    Expected {
        file: "03-unknown-nested-key.artifact.json",
        step: LoadStep::Schema,
        json_path: "scene[0]",
        message_contains: "oneOf",
    },
    // Semantic step: each names the offending setting/identifier and its path.
    Expected {
        file: "20-setting-named-beat.artifact.json",
        step: LoadStep::Semantic,
        json_path: "settings.beat",
        message_contains: "beat",
    },
    Expected {
        file: "21-bad-color-hex.artifact.json",
        step: LoadStep::Semantic,
        json_path: "settings.tint",
        message_contains: "#zzz",
    },
    Expected {
        file: "22-choice-default-not-in-options.artifact.json",
        step: LoadStep::Semantic,
        json_path: "settings.mode",
        message_contains: "mode",
    },
    Expected {
        file: "23-min-greater-than-max.artifact.json",
        step: LoadStep::Semantic,
        json_path: "settings.size",
        message_contains: "min",
    },
    Expected {
        file: "26-empty-scene.artifact.json",
        step: LoadStep::Semantic,
        json_path: "scene",
        message_contains: "at least one layer",
    },
    Expected {
        file: "27-seventeen-layers.artifact.json",
        step: LoadStep::Semantic,
        json_path: "scene",
        message_contains: "16",
    },
    Expected {
        file: "29-bad-meta-id.artifact.json",
        step: LoadStep::Semantic,
        json_path: "meta.id",
        message_contains: "artifact id",
    },
    // Formula step: the json_path drills to the exact channel/scalar, and the
    // offending token (the identifier, the disallowed extra) is named.
    Expected {
        file: "10-unknown-identifier.artifact.json",
        step: LoadStep::Formula,
        json_path: "scene[0].cell.color.h",
        message_contains: "totally_undeclared",
    },
    Expected {
        file: "25-stage-violation-x-in-element.artifact.json",
        step: LoadStep::Formula,
        json_path: "scene[0].element.x",
        message_contains: "'x'",
    },
    Expected {
        file: "08-op-cap-bomb.artifact.json",
        step: LoadStep::Formula,
        json_path: "scene[0].resolution",
        message_contains: "256",
    },
    Expected {
        file: "09-nesting-bomb.artifact.json",
        step: LoadStep::Formula,
        json_path: "scene[0].resolution",
        message_contains: "nesting",
    },
];

/// Every curated file is rejected at the EXACT step, with the EXACT json_path
/// pointing at the offending node, and the offending token present in the message.
#[test]
fn diagnostic_quality_for_curated_subset() {
    for exp in CURATED {
        let bytes = read_corpus(exp.file);
        let diag = load_artifact(exp.file, &bytes)
            .map(|_| ())
            .expect_err(&format!("curated file '{}' must be rejected", exp.file));

        assert_eq!(
            diag.step, exp.step,
            "file '{}': wrong step (got {:?}, want {:?}); message: {}",
            exp.file, diag.step, exp.step, diag.message
        );
        assert_eq!(
            diag.json_path, exp.json_path,
            "file '{}': wrong json_path (got {:?}, want {:?})",
            exp.file, diag.json_path, exp.json_path
        );
        assert!(
            diag.message.contains(exp.message_contains),
            "file '{}': message must name '{}', got: {}",
            exp.file,
            exp.message_contains,
            diag.message
        );
    }
}

// ---------------------------------------------------------------------------
// Duplicate ids — the library-level rule, exercised via a temp dir + push_source
// ---------------------------------------------------------------------------

/// The duplicate-id pair: each file loads on its own, but feeding both through the
/// library (the same de-duplication path a folder scan uses) keeps the first and
/// rejects the second with a duplicate-id [`LoadStep::Semantic`] diagnostic naming
/// the kept source. Copied into a temp dir so the pairing is controlled
/// and deterministic.
#[test]
fn duplicate_ids_rejected_via_library() {
    let [a, b]: [&str; 2] = DUPLICATE_PAIR
        .try_into()
        .expect("exactly two duplicate files");

    // Sanity: each loads fine on its own (the conflict is only across the pair).
    assert!(
        load_artifact(a, &read_corpus(a)).is_ok(),
        "duplicate file '{a}' must be individually valid"
    );
    assert!(
        load_artifact(b, &read_corpus(b)).is_ok(),
        "duplicate file '{b}' must be individually valid"
    );

    // Controlled pairing via a temp dir: copy both, then push them in order.
    let tmp = TempDir::new("vm-hostile-dup");
    let pa = tmp.path().join(a);
    let pb = tmp.path().join(b);
    std::fs::copy(hostile_dir().join(a), &pa).expect("copy first duplicate");
    std::fs::copy(hostile_dir().join(b), &pb).expect("copy second duplicate");

    let mut lib = ArtifactLibrary::new();
    lib.push_source(
        ArtifactSource::UserFile(pa.clone()),
        &std::fs::read(&pa).unwrap(),
    );
    lib.push_source(
        ArtifactSource::UserFile(pb.clone()),
        &std::fs::read(&pb).unwrap(),
    );

    // First-loaded wins: exactly one loaded, the second rejected as a duplicate.
    assert_eq!(
        lib.loaded_count(),
        1,
        "duplicate id must collapse to one loaded entry"
    );
    assert_eq!(
        lib.entries().len(),
        2,
        "both sources are recorded as entries"
    );

    match &lib.entries()[1].status {
        EntryStatus::Rejected { diagnostic } => {
            assert_eq!(diagnostic.step, LoadStep::Semantic);
            assert_eq!(diagnostic.json_path, "meta.id");
            assert!(
                diagnostic.message.contains("duplicate artifact id 'twin'"),
                "duplicate diagnostic must name the id: {}",
                diagnostic.message
            );
            assert!(
                diagnostic.message.contains(&pa.display().to_string()),
                "duplicate diagnostic must name the kept source: {}",
                diagnostic.message
            );
        }
        EntryStatus::Loaded(_) => panic!("the second duplicate must be rejected, not loaded"),
    }

    // The kept artifact is the FIRST file's content.
    let kept = lib.entries()[0].loaded().expect("first entry loaded");
    assert_eq!(kept.artifact.meta.name, "Twin First");
}

// ---------------------------------------------------------------------------
// Minimal self-cleaning temp dir (no external dev-dependency)
// ---------------------------------------------------------------------------

/// A tiny RAII temp directory under the OS temp dir. Created so the duplicate-id
/// test can copy files into a controlled location without adding a dev-dependency;
/// removed on drop. The name is suffixed with the process id + a counter for
/// uniqueness across concurrent test binaries.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
