//! The artifact library: a deterministic, ordered collection of loaded and
//! rejected artifacts.
//!
//! [`ArtifactLibrary`] enumerates **built-ins first** (embedded assets that the
//! application passes in as `(name, json)` pairs via [`ArtifactLibrary::load_builtins`]),
//! then user-folder files appended in deterministic (filename-sorted) order by
//! [`ArtifactLibrary::scan_folder`]. The built-in loader takes already-read JSON so the
//! validation core stays filesystem-free; the user-folder scan is the one
//! filesystem-aware method, reading `*.artifact.json` files and feeding their bytes
//! through the same de-duplication path as built-ins
//! ([`UserFile`](ArtifactSource::UserFile) entries).
//!
//! ## Determinism & the duplicate-id rule
//! Entries keep their insertion order. The id-uniqueness rule is enforced as each
//! entry is added: **first-loaded wins**. Built-ins load before user files, so a
//! built-in is never shadowed by a user file. A second source declaring an
//! already-loaded id is recorded as a [`LibraryEntry`] with status
//! [`EntryStatus::Rejected`] carrying a duplicate-id [`LoadDiagnostic`], and a
//! warning is logged naming **both** sources.
//!
//! ## Reading the library
//! - [`ArtifactLibrary::loaded`] yields `(&ArtifactId, &str display name)` for every
//!   successfully loaded entry, in order — exactly the dropdown contents (rejected
//!   entries are excluded).
//! - [`ArtifactLibrary::get`] fetches a loaded artifact by id.
//! - [`ArtifactLibrary::first_loaded_id`] returns the default selection.

use std::path::{Path, PathBuf};

use viz_core::ArtifactId;

use crate::load::{load_artifact, LoadDiagnostic, LoadStep, LoadedArtifact};
use crate::types::limits;

/// The required file-name suffix for user artifacts in the artifacts folder
/// Only files ending in this are scanned; everything else is ignored.
pub const ARTIFACT_FILE_SUFFIX: &str = ".artifact.json";

/// Where a [`LibraryEntry`] came from. Built-ins are embedded assets identified by a
/// label; user files carry their path (used in diagnostics and the future scan).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtifactSource {
    /// An embedded built-in artifact, identified by the source label passed to
    /// [`load_artifact`] (e.g. `"spectrum-bars (built-in)"`).
    BuiltIn(String),
    /// A user-supplied file from the artifacts folder (added by the later scan).
    UserFile(PathBuf),
}

impl ArtifactSource {
    /// A short English label naming this source, for diagnostics and logs.
    pub fn label(&self) -> String {
        match self {
            ArtifactSource::BuiltIn(name) => name.clone(),
            ArtifactSource::UserFile(path) => path.display().to_string(),
        }
    }
}

/// The outcome of loading one source into the library.
#[derive(Clone, Debug)]
pub enum EntryStatus {
    /// The source loaded successfully; the compiled artifact is held here.
    ///
    /// Boxed so the `Rejected` variant (just a diagnostic) does not inflate every
    /// entry to the size of a whole [`LoadedArtifact`].
    Loaded(Box<LoadedArtifact>),
    /// The source was rejected (validation failure or a duplicate id); the
    /// diagnostic explains why.
    Rejected {
        /// The single English diagnostic line for this rejection.
        diagnostic: LoadDiagnostic,
    },
}

/// One source in the library, in insertion order, with its load outcome.
#[derive(Clone, Debug)]
pub struct LibraryEntry {
    /// Where this entry came from.
    pub source: ArtifactSource,
    /// Whether it loaded, and the artifact or the rejection diagnostic.
    pub status: EntryStatus,
}

impl LibraryEntry {
    /// The compiled artifact if this entry loaded, else `None`.
    pub fn loaded(&self) -> Option<&LoadedArtifact> {
        match &self.status {
            EntryStatus::Loaded(art) => Some(art),
            EntryStatus::Rejected { .. } => None,
        }
    }
}

/// A deterministic, ordered collection of loaded + rejected artifacts.
///
/// Build it by loading built-ins first (and, later, scanning the user folder); then
/// read it via [`loaded`](Self::loaded) / [`get`](Self::get) /
/// [`first_loaded_id`](Self::first_loaded_id).
#[derive(Clone, Debug, Default)]
pub struct ArtifactLibrary {
    /// Entries in insertion order (built-ins first, then user files). Holds both
    /// loaded and rejected outcomes so callers can surface load failures.
    entries: Vec<LibraryEntry>,
}

impl ArtifactLibrary {
    /// An empty library (no built-ins, no user files).
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the embedded built-ins (in the given order) into a fresh library.
    ///
    /// Each pair is `(source_label, json_text)` — the application supplies
    /// `include_str!` content so this crate stays filesystem-free. Built-ins are the
    /// first entries, so they are never shadowed by later user files.
    /// Duplicate ids *among* built-ins follow the same first-loaded-wins rule.
    pub fn load_builtins<'a, I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut lib = Self::new();
        for (name, json) in entries {
            lib.push_source(ArtifactSource::BuiltIn(name.to_owned()), json.as_bytes());
        }
        lib
    }

    /// Loads one source (built-in or, later, a user file) and appends its entry,
    /// enforcing the first-loaded-wins duplicate-id rule.
    ///
    /// On a validation failure the entry is recorded as
    /// [`EntryStatus::Rejected`]. On a duplicate id the entry is rejected
    /// with a synthesized duplicate-id diagnostic and a warning is logged naming
    /// both the kept and the rejected source.
    ///
    /// Used by [`load_builtins`](Self::load_builtins) and by the future
    /// `scan_folder`; exposed so the scan can extend the library through the same
    /// de-duplication path.
    pub fn push_source(&mut self, source: ArtifactSource, bytes: &[u8]) {
        let source_label = source.label();
        let status = match load_artifact(&source_label, bytes) {
            Ok(loaded) => {
                // Duplicate-id check: first-loaded wins.
                if let Some(owner) = self.find_loaded_source(&loaded.id) {
                    let owner_label = owner.label();
                    tracing::warn!(
                        target: "viz_contract::library",
                        duplicate_id = %loaded.id,
                        kept = %owner_label,
                        rejected = %source_label,
                        "duplicate artifact id: keeping first-loaded source, rejecting later one"
                    );
                    EntryStatus::Rejected {
                        diagnostic: LoadDiagnostic {
                            source_name: source_label.clone(),
                            step: LoadStep::Semantic,
                            json_path: "meta.id".to_owned(),
                            message: format!(
                                "duplicate artifact id '{}' already loaded from '{owner_label}'; \
                                 keeping the first-loaded artifact",
                                loaded.id
                            ),
                        },
                    }
                } else {
                    EntryStatus::Loaded(Box::new(loaded))
                }
            }
            Err(diagnostic) => {
                tracing::warn!(
                    target: "viz_contract::library",
                    source = %source_label,
                    %diagnostic,
                    "rejected artifact"
                );
                EntryStatus::Rejected { diagnostic }
            }
        };
        self.entries.push(LibraryEntry { source, status });
    }

    /// Scans `dir` for `*.artifact.json` user files and appends them to the library
    /// **after** any existing entries, in deterministic filename-sorted
    /// order so the dropdown is stable run-to-run.
    ///
    /// Each file is fed through [`push_source`](Self::push_source), so the same
    /// first-loaded-wins duplicate-id rule applies: a user file re-declaring a
    /// built-in id is rejected (built-ins load first and are unshadowable),
    /// and the rejection is logged naming both sources. Invalid files are rejected
    /// with their full [`LoadDiagnostic`] logged and the scan
    /// continues — one bad file never blocks the rest.
    ///
    /// **Size guard.** Each file's byte length is checked against
    /// [`limits::MAX_FILE_BYTES`](crate::types::limits::MAX_FILE_BYTES) *before*
    /// reading its contents, so an oversized file is rejected without loading it into
    /// memory (`load_artifact` re-checks the same limit defensively).
    ///
    /// A missing directory, an unreadable directory, or an unreadable file is logged
    /// and skipped (best-effort, never fatal — the app keeps running on built-ins).
    /// Subdirectories and non-matching files are ignored.
    pub fn scan_folder(&mut self, dir: &Path) {
        // Read the directory entries; a missing/unreadable dir is non-fatal.
        let read_dir = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    target: "viz_contract::library",
                    dir = %dir.display(),
                    error = %e,
                    "could not read artifacts folder; skipping user artifacts"
                );
                return;
            }
        };

        // Collect matching file paths, then sort by file name for determinism.
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(
                        target: "viz_contract::library",
                        dir = %dir.display(),
                        error = %e,
                        "could not read a directory entry; skipping it"
                    );
                    continue;
                }
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let matches = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(ARTIFACT_FILE_SUFFIX));
            if matches {
                paths.push(path);
            }
        }
        paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for path in paths {
            self.scan_file(&path);
        }
    }

    /// Loads one user file from disk into the library: enforces the file-size limit
    /// pre-read, then feeds the bytes through [`push_source`](Self::push_source). An
    /// oversized or unreadable file is recorded as a rejected
    /// [`UserFile`](ArtifactSource::UserFile) entry with a logged diagnostic.
    fn scan_file(&mut self, path: &Path) {
        let source = ArtifactSource::UserFile(path.to_path_buf());
        let source_label = source.label();

        // Size guard before reading the file into memory.
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > limits::MAX_FILE_BYTES as u64 => {
                let diagnostic = LoadDiagnostic {
                    source_name: source_label.clone(),
                    step: LoadStep::Parse,
                    json_path: String::new(),
                    message: format!(
                        "file is {} bytes, exceeding the {} byte limit",
                        meta.len(),
                        limits::MAX_FILE_BYTES
                    ),
                };
                tracing::warn!(
                    target: "viz_contract::library",
                    source = %source_label,
                    %diagnostic,
                    "rejected artifact"
                );
                self.entries.push(LibraryEntry {
                    source,
                    status: EntryStatus::Rejected { diagnostic },
                });
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let diagnostic = LoadDiagnostic {
                    source_name: source_label.clone(),
                    step: LoadStep::Parse,
                    json_path: String::new(),
                    message: format!("could not stat file: {e}"),
                };
                tracing::warn!(
                    target: "viz_contract::library",
                    source = %source_label,
                    %diagnostic,
                    "rejected artifact"
                );
                self.entries.push(LibraryEntry {
                    source,
                    status: EntryStatus::Rejected { diagnostic },
                });
                return;
            }
        }

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                let diagnostic = LoadDiagnostic {
                    source_name: source_label.clone(),
                    step: LoadStep::Parse,
                    json_path: String::new(),
                    message: format!("could not read file: {e}"),
                };
                tracing::warn!(
                    target: "viz_contract::library",
                    source = %source_label,
                    %diagnostic,
                    "rejected artifact"
                );
                self.entries.push(LibraryEntry {
                    source,
                    status: EntryStatus::Rejected { diagnostic },
                });
                return;
            }
        };

        self.push_source(source, &bytes);
    }

    /// Returns the source that already owns `id` among the loaded entries, if any.
    fn find_loaded_source(&self, id: &ArtifactId) -> Option<&ArtifactSource> {
        self.entries.iter().find_map(|e| match &e.status {
            EntryStatus::Loaded(art) if &art.id == id => Some(&e.source),
            _ => None,
        })
    }

    /// All entries in insertion order (loaded and rejected), for surfacing load
    /// outcomes.
    pub fn entries(&self) -> &[LibraryEntry] {
        &self.entries
    }

    /// Iterates the successfully loaded artifacts as `(id, display name)` pairs, in
    /// library order — the dropdown contents (rejected entries excluded).
    pub fn loaded(&self) -> impl Iterator<Item = (&ArtifactId, &str)> {
        self.entries.iter().filter_map(|e| match &e.status {
            EntryStatus::Loaded(art) => Some((&art.id, art.artifact.meta.name.as_str())),
            EntryStatus::Rejected { .. } => None,
        })
    }

    /// Fetches a loaded artifact by id, or `None` if no loaded entry has that id.
    pub fn get(&self, id: &ArtifactId) -> Option<&LoadedArtifact> {
        self.entries.iter().find_map(|e| match &e.status {
            EntryStatus::Loaded(art) if &art.id == id => Some(art.as_ref()),
            _ => None,
        })
    }

    /// The id of the first successfully loaded artifact in library order (the default
    /// selection / fallback), or `None` when nothing loaded.
    pub fn first_loaded_id(&self) -> Option<&ArtifactId> {
        self.entries
            .iter()
            .find_map(|e| e.loaded().map(|art| &art.id))
    }

    /// Whether `id` names a successfully loaded artifact in this library.
    pub fn contains(&self, id: &ArtifactId) -> bool {
        self.get(id).is_some()
    }

    /// Number of successfully loaded artifacts.
    pub fn loaded_count(&self) -> usize {
        self.entries.iter().filter(|e| e.loaded().is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid artifact JSON with the given id + display name.
    fn artifact_json(id: &str, name: &str) -> String {
        format!(
            r#"{{
                "contract": "1.0",
                "meta": {{ "id": "{id}", "name": "{name}" }},
                "scene": [
                    {{
                        "type": "instanced",
                        "shape": "rect",
                        "count": "1",
                        "element": {{
                            "x": "0", "y": "0", "w": "0.1", "h": "0.1",
                            "color": {{ "model": "rgba", "r": "1", "g": "1", "b": "1" }}
                        }}
                    }}
                ]
            }}"#
        )
    }

    #[test]
    fn empty_library_has_no_loaded_and_no_default() {
        let lib = ArtifactLibrary::new();
        assert_eq!(lib.loaded_count(), 0);
        assert!(lib.first_loaded_id().is_none());
        assert!(lib.loaded().next().is_none());
    }

    #[test]
    fn loads_builtins_in_order() {
        let a = artifact_json("alpha", "Alpha");
        let b = artifact_json("bravo", "Bravo");
        let lib = ArtifactLibrary::load_builtins([
            ("alpha (built-in)", a.as_str()),
            ("bravo (built-in)", b.as_str()),
        ]);

        let names: Vec<(&str, &str)> = lib.loaded().map(|(id, n)| (id.0.as_str(), n)).collect();
        assert_eq!(names, vec![("alpha", "Alpha"), ("bravo", "Bravo")]);
        assert_eq!(lib.first_loaded_id().unwrap().0, "alpha");
        assert_eq!(lib.loaded_count(), 2);

        let id = ArtifactId::parse("bravo").unwrap();
        assert!(lib.contains(&id));
        assert_eq!(lib.get(&id).unwrap().artifact.meta.name, "Bravo");
    }

    #[test]
    fn rejected_artifact_excluded_from_loaded_but_kept_as_entry() {
        // Second entry is invalid JSON → rejected, excluded from `loaded()`.
        let a = artifact_json("alpha", "Alpha");
        let lib = ArtifactLibrary::load_builtins([
            ("alpha (built-in)", a.as_str()),
            ("broken (built-in)", "{ not valid json"),
        ]);

        assert_eq!(lib.loaded_count(), 1);
        assert_eq!(lib.entries().len(), 2);
        assert!(matches!(
            lib.entries()[1].status,
            EntryStatus::Rejected { .. }
        ));
        // Excluded from the dropdown list.
        let ids: Vec<&str> = lib.loaded().map(|(id, _)| id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha"]);
    }

    #[test]
    fn duplicate_id_first_loaded_wins() {
        // Two sources declare id "dup": the first is kept, the second rejected.
        let first = artifact_json("dup", "First Wins");
        let second = artifact_json("dup", "Second Loses");
        let lib = ArtifactLibrary::load_builtins([
            ("first (built-in)", first.as_str()),
            ("second (built-in)", second.as_str()),
        ]);

        // Only the first loaded; the second is a rejected entry.
        assert_eq!(lib.loaded_count(), 1);
        assert_eq!(lib.entries().len(), 2);

        let id = ArtifactId::parse("dup").unwrap();
        // The kept artifact is the FIRST one.
        assert_eq!(lib.get(&id).unwrap().artifact.meta.name, "First Wins");

        // The second entry is rejected with a duplicate-id semantic diagnostic
        // naming the kept source.
        match &lib.entries()[1].status {
            EntryStatus::Rejected { diagnostic } => {
                assert_eq!(diagnostic.step, LoadStep::Semantic);
                assert!(
                    diagnostic.message.contains("duplicate artifact id 'dup'"),
                    "message: {}",
                    diagnostic.message
                );
                assert!(
                    diagnostic.message.contains("first (built-in)"),
                    "message names kept source: {}",
                    diagnostic.message
                );
            }
            EntryStatus::Loaded(_) => panic!("second duplicate must be rejected"),
        }

        // Dropdown lists the id exactly once.
        let ids: Vec<&str> = lib.loaded().map(|(id, _)| id.0.as_str()).collect();
        assert_eq!(ids, vec!["dup"]);
    }

    #[test]
    fn push_source_appends_user_file_after_builtins() {
        // The future scan path: built-ins first, then a user file with a fresh id.
        let a = artifact_json("alpha", "Alpha");
        let mut lib = ArtifactLibrary::load_builtins([("alpha (built-in)", a.as_str())]);
        let user = artifact_json("user-one", "User One");
        lib.push_source(
            ArtifactSource::UserFile(PathBuf::from("/artifacts/user-one.artifact.json")),
            user.as_bytes(),
        );

        let ids: Vec<&str> = lib.loaded().map(|(id, _)| id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "user-one"]);
    }

    #[test]
    fn builtin_unshadowable_by_user_file_with_same_id() {
        // A user file re-declaring a built-in id is rejected (built-in loaded first).
        let a = artifact_json("alpha", "Built-in Alpha");
        let mut lib = ArtifactLibrary::load_builtins([("alpha (built-in)", a.as_str())]);
        let shadow = artifact_json("alpha", "User Alpha");
        lib.push_source(
            ArtifactSource::UserFile(PathBuf::from("/artifacts/alpha.artifact.json")),
            shadow.as_bytes(),
        );

        assert_eq!(lib.loaded_count(), 1);
        let id = ArtifactId::parse("alpha").unwrap();
        assert_eq!(lib.get(&id).unwrap().artifact.meta.name, "Built-in Alpha");
        assert!(matches!(
            lib.entries()[1].status,
            EntryStatus::Rejected { .. }
        ));
    }

    // ---- scan_folder -------------------------------------------------------

    /// A throwaway directory under the OS temp dir, unique per test + process, that
    /// recursively removes itself on drop so tests leave nothing behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Monotonic counter keeps two TempDirs in one test process distinct.
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "viz-contract-scan-{tag}-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.0.join(name), contents).expect("write temp file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scan_folder_appends_valid_user_files_after_builtins_sorted() {
        let tmp = TempDir::new("valid");
        // Write out of alphabetical order to prove filename-sorted determinism.
        tmp.write("zebra.artifact.json", &artifact_json("zebra", "Zebra"));
        tmp.write("apple.artifact.json", &artifact_json("apple", "Apple"));
        // A non-matching file is ignored.
        tmp.write("notes.txt", "not an artifact");
        tmp.write("readme.json", "{ not even valid }");

        let builtin = artifact_json("alpha", "Alpha");
        let mut lib = ArtifactLibrary::load_builtins([("alpha (built-in)", builtin.as_str())]);
        lib.scan_folder(tmp.path());

        // Built-in first, then user files in filename order (apple before zebra).
        let ids: Vec<&str> = lib.loaded().map(|(id, _)| id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "apple", "zebra"]);
    }

    #[test]
    fn scan_folder_valid_invalid_and_builtin_duplicate() {
        // The acceptance shape: a valid + an invalid + a duplicate-of-builtin
        // file → exactly the valid one is loaded; the other two are rejected with
        // correct diagnostics.
        let tmp = TempDir::new("mixed");
        tmp.write("good.artifact.json", &artifact_json("good", "Good"));
        tmp.write("bad.artifact.json", "{ this is not valid json");
        // Duplicate of the built-in id "alpha" → rejected (built-in unshadowable).
        tmp.write("dupe.artifact.json", &artifact_json("alpha", "User Alpha"));

        let builtin = artifact_json("alpha", "Built-in Alpha");
        let mut lib = ArtifactLibrary::load_builtins([("alpha (built-in)", builtin.as_str())]);
        lib.scan_folder(tmp.path());

        // Loaded: built-in alpha + the one valid user file.
        let ids: Vec<&str> = lib.loaded().map(|(id, _)| id.0.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "good"]);
        let alpha = ArtifactId::parse("alpha").unwrap();
        assert_eq!(
            lib.get(&alpha).unwrap().artifact.meta.name,
            "Built-in Alpha"
        );

        // Two rejected user-file entries with diagnostics that name the file.
        let rejected: Vec<&LibraryEntry> = lib
            .entries()
            .iter()
            .filter(|e| matches!(e.status, EntryStatus::Rejected { .. }))
            .collect();
        assert_eq!(rejected.len(), 2);

        // The invalid file is rejected at the parse step.
        let bad = rejected
            .iter()
            .find(|e| e.source.label().ends_with("bad.artifact.json"))
            .expect("bad file rejected");
        match &bad.status {
            EntryStatus::Rejected { diagnostic } => {
                assert_eq!(diagnostic.step, LoadStep::Parse);
                assert!(diagnostic.source_name.ends_with("bad.artifact.json"));
            }
            EntryStatus::Loaded(_) => unreachable!(),
        }

        // The duplicate file is rejected at the semantic step naming the built-in.
        let dupe = rejected
            .iter()
            .find(|e| e.source.label().ends_with("dupe.artifact.json"))
            .expect("duplicate file rejected");
        match &dupe.status {
            EntryStatus::Rejected { diagnostic } => {
                assert_eq!(diagnostic.step, LoadStep::Semantic);
                assert!(
                    diagnostic.message.contains("duplicate artifact id 'alpha'"),
                    "message: {}",
                    diagnostic.message
                );
                assert!(
                    diagnostic.message.contains("alpha (built-in)"),
                    "names the kept built-in source: {}",
                    diagnostic.message
                );
            }
            EntryStatus::Loaded(_) => unreachable!(),
        }
    }

    #[test]
    fn scan_folder_rejects_oversized_file_at_parse() {
        let tmp = TempDir::new("oversize");
        // A file larger than the limit, but still valid-looking JSON shape: padded
        // with whitespace inside the document so it parses-shaped but is too big.
        let mut big = artifact_json("big", "Big");
        // Pad to exceed MAX_FILE_BYTES.
        let pad = " ".repeat(limits::MAX_FILE_BYTES + 1);
        big.push_str(&pad);
        tmp.write("big.artifact.json", &big);

        let mut lib = ArtifactLibrary::new();
        lib.scan_folder(tmp.path());

        assert_eq!(lib.loaded_count(), 0);
        assert_eq!(lib.entries().len(), 1);
        match &lib.entries()[0].status {
            EntryStatus::Rejected { diagnostic } => {
                assert_eq!(diagnostic.step, LoadStep::Parse);
                assert!(
                    diagnostic.message.contains("exceeding"),
                    "message: {}",
                    diagnostic.message
                );
            }
            EntryStatus::Loaded(_) => panic!("oversized file must be rejected"),
        }
    }

    #[test]
    fn scan_folder_missing_directory_is_noop() {
        let missing = std::env::temp_dir().join(format!(
            "viz-contract-scan-missing-{}-does-not-exist",
            std::process::id()
        ));
        let mut lib = ArtifactLibrary::new();
        // Must not panic; library stays empty.
        lib.scan_folder(&missing);
        assert_eq!(lib.entries().len(), 0);
    }
}
