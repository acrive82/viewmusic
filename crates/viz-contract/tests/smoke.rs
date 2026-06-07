//! The preset gate: every shipped built-in must load.
//!
//! Supersedes the earlier `builtins_check.rs`. This half of the smoke gate lives in
//! `viz-contract`, which must NOT depend on `viz-app`; it therefore covers only the
//! load/validation pipeline (schema validation + formula compilation). The
//! render-one-frame and switch-latency halves live in
//! `crates/viz-app/tests/smoke.rs`, where `ArtifactRuntime` is reachable.
//!
//! It iterates every `*.artifact.json` under `assets/builtin-artifacts/` through the
//! full [`load_artifact`] pipeline (size + parse + version + schema + deserialize +
//! semantics + formula compilation + stage restriction + op caps) and asserts each
//! one loads cleanly, with at least the five presets the contract requires shipped.

use std::path::PathBuf;

use viz_contract::load_artifact;

/// Absolute path to the shipped built-in artifacts directory.
fn builtins_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../assets/builtin-artifacts");
    path
}

/// Collects every `*.artifact.json` under `assets/builtin-artifacts/`, sorted for a
/// stable iteration order.
fn builtin_paths() -> Vec<PathBuf> {
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
}

/// Preset gate: every shipped preset loads, and we ship at least five.
#[test]
fn all_builtin_artifacts_load() {
    let paths = builtin_paths();

    assert!(
        paths.len() >= 5,
        "expected at least 5 built-in artifacts, found {} in {}",
        paths.len(),
        builtins_dir().display()
    );

    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {name}: {e}"));
        match load_artifact(&name, &bytes) {
            Ok(loaded) => {
                // The loaded id must be a valid, non-empty contract id (schema + the
                // semantic pass already enforced the pattern; assert it is populated
                // so a silently-empty id never slips through the gate).
                assert!(
                    !loaded.id.0.is_empty(),
                    "built-in {name} loaded with an empty id"
                );
            }
            Err(diag) => panic!("built-in {name} failed to load: {diag}"),
        }
    }
}
