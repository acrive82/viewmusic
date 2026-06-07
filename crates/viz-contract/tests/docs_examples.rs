//! Manual example gate: every authoring-manual example loads.
//!
//! Mirrors the built-in smoke gate (`smoke.rs`) but points at the manual's
//! standalone example corpus under `docs/authoring/examples/`. Every
//! `*.artifact.json` there must pass the full [`load_artifact`] pipeline
//! unchanged (size + parse + version + schema + deserialize + semantics +
//! formula compilation + op caps); a failure names the offending file with the
//! exact diagnostic line a real author would see in the log.
//!
//! This is the drift guard for the documentation: if the contract or loader
//! evolves and an embedded example stops loading, this test fails the build
//! rather than letting a reader hit the breakage (guarding against example
//! drift).
//!
//! Two structural guards back up the per-file load check:
//! - a floor on the total example count, so an accidentally emptied (or
//!   un-committed) examples folder fails loudly rather than passing vacuously;
//! - the five tutorial-step files are all present, so the paste-any-step
//!   tutorial chain can never silently lose a rung.

use std::collections::BTreeSet;
use std::path::PathBuf;

use viz_contract::load_artifact;

/// Absolute path to the manual's example directory.
///
/// `CARGO_MANIFEST_DIR` is `crates/viz-contract`; the examples live at the repo
/// root under `docs/authoring/examples/`.
fn examples_dir() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../../docs/authoring/examples");
    path
}

/// Collects every `*.artifact.json` under the examples directory, sorted for a
/// stable iteration order.
fn example_paths() -> Vec<PathBuf> {
    let dir = examples_dir();
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

/// Every manual example loads through the real pipeline unchanged.
#[test]
fn all_docs_examples_load() {
    let paths = example_paths();

    assert!(
        !paths.is_empty(),
        "no example artifacts found in {} — the manual's examples are missing",
        examples_dir().display()
    );

    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("reading {name}: {e}"));
        match load_artifact(&name, &bytes) {
            Ok(loaded) => {
                assert!(
                    !loaded.id.0.is_empty(),
                    "manual example {name} loaded with an empty id"
                );
            }
            // The diagnostic's Display is the exact log line a real author sees.
            Err(diag) => panic!("manual example {name} failed to load: {diag}"),
        }
    }
}

/// Floor guard: the manual ships 5 tutorial steps, ≥9 reference
/// demos, and 6 recipes — at least 20 example files. An emptied folder must
/// fail this even if the few remaining files happen to load.
#[test]
fn docs_examples_meet_floor_count() {
    let count = example_paths().len();
    assert!(
        count >= 20,
        "expected at least 20 manual example artifacts (5 tutorial + ≥9 ref + 6 recipe), \
         found {count} in {} — has the examples folder lost files?",
        examples_dir().display()
    );
}

/// Tutorial-chain guard: the five paste-any-step files
/// `tutorial-step-1..5` must all be present so the getting-started chapter's
/// chain stays complete.
#[test]
fn tutorial_chain_is_complete() {
    let names: BTreeSet<String> = example_paths()
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        .collect();

    for step in 1..=5 {
        let expected = format!("tutorial-step-{step}.artifact.json");
        assert!(
            names.contains(&expected),
            "missing tutorial step file {expected} in {}",
            examples_dir().display()
        );
    }
}
