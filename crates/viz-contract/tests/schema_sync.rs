//! Schema export & sync guard.
//!
//! The published artifact schema is *generated* from the typed [`Artifact`] model
//! via `schemars` ([`viz_contract::published_schema`]) and exported by the
//! `export-schema` binary. This test guards two things:
//!
//! 1. **Export fidelity** — running `export-schema -- <path>` writes exactly the
//!    pretty-printed [`published_schema`] (so the binary is a faithful dump of the
//!    in-process schema, never a stale snapshot).
//! 2. **Sync with the committed reference schema** — the generated schema stays
//!    equivalent to the hand-written reference at
//!    `docs/reference/artifact.schema.json` via the same
//!    VALID/INVALID corpus-equivalence guard `roundtrip.rs` uses. Both files
//!    `#[path]`-include the shared [`common`](mod@common) corpus, so this reuses —
//!    rather than duplicates — that guard (the corpus exercises only the
//!    shape-level constraints both schemas express identically; the patterns and
//!    numeric/length bounds are loader semantics by design — see `roundtrip.rs`).
//!
//! Together these mean: the bytes the binary writes == the schema the equivalence
//! guard validates == a schema equivalent to the committed reference.

#[path = "common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

use viz_contract::published_schema;

/// Path to the `export-schema` binary built for this test (provided by Cargo).
fn export_schema_bin() -> &'static str {
    env!("CARGO_BIN_EXE_export-schema")
}

/// A unique scratch path under the OS temp dir for the exported schema.
fn scratch_path() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "viz-contract-schema-export-{}-{n}.json",
        std::process::id()
    ))
}

/// (1) `export-schema -- <path>` writes the pretty-printed published schema exactly.
#[test]
fn export_schema_bin_writes_published_schema() {
    let out = scratch_path();
    // Best-effort cleanup of any stale file from a previous aborted run.
    let _ = std::fs::remove_file(&out);

    let status = Command::new(export_schema_bin())
        .arg(&out)
        .status()
        .expect("running the export-schema binary");
    assert!(status.success(), "export-schema must exit successfully");

    let written = std::fs::read_to_string(&out).expect("export-schema must have written the file");
    let _ = std::fs::remove_file(&out);

    // Byte-for-byte: the file is the pretty-printed published schema.
    let expected = serde_json::to_string_pretty(published_schema())
        .expect("published schema serializes to JSON");
    assert_eq!(
        written, expected,
        "export-schema output must be exactly the pretty-printed published schema"
    );

    // And it parses back to a Value identical to the in-process schema (no drift
    // through serialization).
    let written_value: Value =
        serde_json::from_str(&written).expect("exported schema is valid JSON");
    assert_eq!(
        &written_value,
        published_schema(),
        "exported schema must round-trip to the in-process published schema"
    );
}

/// (1b) With no path argument the binary prints the schema to stdout (parses back
/// to the published schema).
#[test]
fn export_schema_bin_prints_to_stdout() {
    let output = Command::new(export_schema_bin())
        .output()
        .expect("running the export-schema binary (stdout mode)");
    assert!(
        output.status.success(),
        "export-schema (stdout) must exit successfully"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    let printed: Value = serde_json::from_str(stdout.trim()).expect("stdout is valid JSON");
    assert_eq!(
        &printed,
        published_schema(),
        "export-schema stdout must be the published schema"
    );
}

/// (2) The generated schema stays in sync with the committed reference schema: the
/// shared VALID/INVALID corpus classifies identically under both.
#[test]
fn generated_schema_matches_committed_reference() {
    common::assert_corpus_classifies_identically();
}

/// Anchor sanity for the sync guard: both schemas accept the shipped built-in. If
/// either drifts so the canonical artifact no longer validates, this fails loudly
/// alongside the corpus guard.
#[test]
fn both_schemas_accept_builtin() {
    let doc: Value = serde_json::from_str(common::SPECTRUM_BARS).expect("built-in is valid JSON");

    let generated = common::generated_validator();
    let gen_errs: Vec<String> = generated.iter_errors(&doc).map(|e| e.to_string()).collect();
    assert!(
        gen_errs.is_empty(),
        "generated schema rejects built-in: {gen_errs:?}"
    );

    let reference = common::reference_validator();
    let ref_errs: Vec<String> = reference.iter_errors(&doc).map(|e| e.to_string()).collect();
    assert!(
        ref_errs.is_empty(),
        "reference schema rejects built-in: {ref_errs:?}"
    );
}
