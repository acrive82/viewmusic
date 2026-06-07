//! User-artifacts folder + reload integration tests.
//!
//! These drive the **pure** library-scan + [`VizApp::reload`] paths (no GPU, no
//! window, no audio): a temp folder is scanned through [`ArtifactLibrary::scan_folder`]
//! and through a [`VizApp`] built headless, then files are added/removed between
//! reloads to prove:
//!
//! * a valid user file appears in the dropdown, appended after the built-ins, in
//!   filename-sorted order;
//! * an invalid file and a duplicate-of-built-in file are both rejected, each with
//!   the correct [`LoadDiagnostic`] (file + step + message);
//! * reload preserves the active artifact (its runtime + live setting values) when its
//!   id survives the rescan;
//! * reload falls back to the first built-in when the active artifact's file is
//!   removed, and to the no-artifacts state when nothing loads.
//!
//! Audio is never constructed, so a reload cannot interrupt it (the [`VizApp`] holds
//! `audio: None` here; in the app the handle is wholly independent of the library).

use std::path::{Path, PathBuf};

use viz_app::runtime::SettingValue;
use viz_app::viz::VizApp;
use viz_contract::{ArtifactLibrary, EntryStatus, LoadStep};
use viz_core::ArtifactId;

/// A built-in id present in every library these tests build.
const BUILTIN_ID: &str = "builtin-one";

/// A throwaway directory under the OS temp dir, unique per test + process, removed
/// recursively on drop (no `tempfile` dependency in this workspace).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("viz-app-reload-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.0.join(name), contents).expect("write temp file");
    }

    fn remove(&self, name: &str) {
        std::fs::remove_file(self.0.join(name)).expect("remove temp file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A minimal valid artifact whose instance count is a constant (no settings).
fn artifact_json(id: &str, name: &str) -> String {
    format!(
        r#"{{
            "contract": "1.0",
            "meta": {{ "id": "{id}", "name": "{name}" }},
            "scene": [
                {{
                    "type": "instanced", "shape": "rect", "count": "1",
                    "element": {{
                        "x": "0", "y": "0", "w": "0.1", "h": "0.1",
                        "color": {{ "model": "rgba", "r": "1", "g": "1", "b": "1" }}
                    }}
                }}
            ]
        }}"#
    )
}

/// A valid artifact with one number setting (so we can prove live setting values
/// survive a reload that preserves the active artifact).
fn settable_json(id: &str, name: &str) -> String {
    format!(
        r#"{{
            "contract": "1.0",
            "meta": {{ "id": "{id}", "name": "{name}" }},
            "settings": {{
                "bars": {{ "type": "number", "label": "Bars", "min": 1, "max": 64, "step": 1, "default": 8 }}
            }},
            "scene": [
                {{
                    "type": "instanced", "shape": "rect", "count": "settings.bars",
                    "element": {{
                        "x": "0", "y": "0", "w": "0.1", "h": "0.1",
                        "color": {{ "model": "rgba", "r": "1", "g": "1", "b": "1" }}
                    }}
                }}
            ]
        }}"#
    )
}

/// Builds a library from the built-in(s) + a scan of `dir` — exactly what the app's
/// `build_library` closure does (minus the platform folder resolution).
fn build_library(dir: &Path) -> ArtifactLibrary {
    let builtin = artifact_json(BUILTIN_ID, "Built-in One");
    let mut lib = ArtifactLibrary::load_builtins([("builtin-one (built-in)", builtin.as_str())]);
    lib.scan_folder(dir);
    lib
}

/// The loaded ids in dropdown order.
fn loaded_ids(lib: &ArtifactLibrary) -> Vec<String> {
    lib.loaded().map(|(id, _)| id.0.clone()).collect()
}

// ---------------------------------------------------------------------------
// scan: valid + invalid + duplicate-of-builtin
// ---------------------------------------------------------------------------

#[test]
fn scan_loads_valid_rejects_invalid_and_duplicate() {
    let tmp = TempDir::new("mixed");
    tmp.write("good.artifact.json", &artifact_json("good", "Good"));
    tmp.write("bad.artifact.json", "{ not valid json");
    // Re-declares the built-in id → must be rejected (built-ins are unshadowable).
    tmp.write("dupe.artifact.json", &artifact_json(BUILTIN_ID, "Shadow"));

    let lib = build_library(tmp.path());

    // Exactly the valid user file appears, appended after the built-in.
    assert_eq!(
        loaded_ids(&lib),
        vec![BUILTIN_ID.to_owned(), "good".to_owned()]
    );
    let builtin = ArtifactId::parse(BUILTIN_ID).unwrap();
    assert_eq!(
        lib.get(&builtin).unwrap().artifact.meta.name,
        "Built-in One"
    );

    // Two rejected user-file entries, each with a file-naming diagnostic.
    let rejected: Vec<_> = lib
        .entries()
        .iter()
        .filter_map(|e| match &e.status {
            EntryStatus::Rejected { diagnostic } => Some((e.source.label(), diagnostic)),
            EntryStatus::Loaded(_) => None,
        })
        .collect();
    assert_eq!(rejected.len(), 2);

    let (bad_label, bad_diag) = rejected
        .iter()
        .find(|(label, _)| label.ends_with("bad.artifact.json"))
        .expect("invalid file rejected");
    assert_eq!(bad_diag.step, LoadStep::Parse);
    assert!(bad_diag.source_name.ends_with("bad.artifact.json"));
    assert!(bad_label.ends_with("bad.artifact.json"));

    let (_, dupe_diag) = rejected
        .iter()
        .find(|(label, _)| label.ends_with("dupe.artifact.json"))
        .expect("duplicate file rejected");
    assert_eq!(dupe_diag.step, LoadStep::Semantic);
    assert!(
        dupe_diag
            .message
            .contains("duplicate artifact id 'builtin-one'"),
        "message: {}",
        dupe_diag.message
    );
    assert!(
        dupe_diag.message.contains("builtin-one (built-in)"),
        "names the kept built-in: {}",
        dupe_diag.message
    );
}

// ---------------------------------------------------------------------------
// reload via VizApp: preserve active, then fall back on removal
// ---------------------------------------------------------------------------

#[test]
fn reload_appears_in_dropdown_and_preserves_active() {
    let tmp = TempDir::new("preserve");
    // Start with one user file that has a setting; it is the first non-built-in.
    tmp.write("zeta.artifact.json", &settable_json("zeta", "Zeta"));

    // Build the app already active on the user artifact "zeta" (the realistic case:
    // the user had selected it before reloading the folder).
    let zeta = ArtifactId::parse("zeta").unwrap();
    let library = build_library(tmp.path());
    let dir_owned = tmp.path().to_path_buf();
    let mut app = VizApp::new(library, Some(zeta.clone()), false, None)
        .on_reload_artifacts(move || build_library(&dir_owned));

    assert_eq!(app.active_id(), Some(&zeta));

    // Snapshot the active runtime's live setting values; reload must not disturb them
    // when the active artifact survives the rescan.
    let before: Vec<(String, SettingValue)> = app
        .active_settings()
        .expect("active runtime")
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();

    // Add a *new* valid file, then reload. zeta still exists → active preserved.
    tmp.write("apple.artifact.json", &artifact_json("apple", "Apple"));
    app.reload();

    // Dropdown now lists built-in, then apple, then zeta (filename-sorted user files).
    assert_eq!(
        loaded_ids(app.library()),
        vec![BUILTIN_ID.to_owned(), "apple".to_owned(), "zeta".to_owned()]
    );
    // Active artifact preserved across the reload (id unchanged).
    assert_eq!(app.active_id(), Some(&zeta));
    // Its runtime + live setting values are untouched.
    let after: Vec<(String, SettingValue)> = app
        .active_settings()
        .expect("active runtime preserved")
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    assert_eq!(
        before, after,
        "active artifact's setting values must survive reload"
    );
}

#[test]
fn reload_after_removal_falls_back_to_first_builtin() {
    let tmp = TempDir::new("removal");
    tmp.write("zeta.artifact.json", &artifact_json("zeta", "Zeta"));

    let zeta = ArtifactId::parse("zeta").unwrap();
    let library = build_library(tmp.path());
    let dir_owned = tmp.path().to_path_buf();
    let mut app = VizApp::new(library, Some(zeta.clone()), false, None)
        .on_reload_artifacts(move || build_library(&dir_owned));
    assert_eq!(app.active_id(), Some(&zeta));

    // Remove the active artifact's file, then reload: zeta is gone → fall back to the
    // first built-in. Audio is untouched (there is none here, by construction).
    tmp.remove("zeta.artifact.json");
    app.reload();

    let builtin = ArtifactId::parse(BUILTIN_ID).unwrap();
    assert_eq!(loaded_ids(app.library()), vec![BUILTIN_ID.to_owned()]);
    assert_eq!(app.active_id(), Some(&builtin));
    assert!(app.active_settings().is_some(), "fallback runtime built");
}

#[test]
fn reload_to_empty_enters_no_artifacts_state() {
    let tmp = TempDir::new("empty");
    tmp.write("only.artifact.json", &artifact_json("only", "Only"));

    // A library builder that, after the file is removed AND with no built-ins, yields
    // an empty library — to exercise the no-artifacts fallback branch of reload().
    let only = ArtifactId::parse("only").unwrap();
    let dir_owned = tmp.path().to_path_buf();
    // Build initial library WITHOUT built-ins so removal can empty it entirely.
    let mut initial = ArtifactLibrary::new();
    initial.scan_folder(tmp.path());
    let mut app =
        VizApp::new(initial, Some(only.clone()), false, None).on_reload_artifacts(move || {
            let mut lib = ArtifactLibrary::new();
            lib.scan_folder(&dir_owned);
            lib
        });
    assert_eq!(app.active_id(), Some(&only));

    tmp.remove("only.artifact.json");
    app.reload();

    assert!(app.library().first_loaded_id().is_none());
    assert_eq!(app.active_id(), None);
    assert!(
        app.active_settings().is_none(),
        "no runtime in no-artifacts state"
    );
}
