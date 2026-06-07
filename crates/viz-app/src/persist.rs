//! Persistent application state.
//!
//! Two small JSON documents live under the platform config directory resolved by
//! [`directories::ProjectDirs::from("io.github", "acrive82", "viewmusic")`].
//! `ProjectDirs::config_dir()` maps per platform:
//!
//! * **macOS** — the three components join with `.` into the reverse-DNS bundle
//!   id, giving `~/Library/Application Support/io.github.acrive82.viewmusic/`.
//! * **Windows** — the qualifier is dropped and the path is
//!   `%APPDATA%\acrive82\viewmusic\config\` (roaming application data, with the
//!   `config` leaf the `directories` crate appends on Windows).
//!
//! The two documents are:
//!
//! * `app_state.json` ([`AppState`]) — the last selected artifact id, per-artifact
//!   setting values, and the top-right overlay's collapsed state.
//! * `window_state.json` ([`WindowState`]) — the window position + size, so the
//!   window reopens where it was (winit does not persist this itself).
//!
//! ## Save policy
//! `app_state.json` is small and changes only on discrete user actions (artifact
//! switch, overlay collapse toggle, setting edit/reset); it is saved eagerly on each
//! such change and on quit. A setting drag fires many edits per second, so the host
//! coalesces those: it updates the in-memory [`AppState`] on every edit but only
//! flushes to disk on a ~1 s debounce, plus a forced flush on artifact switch and on
//! quit (so a drag finishing right before a switch/close is never lost). The
//! debounce decision reuses the same pure helper as the window save,
//! [`should_save_window`], so it is unit-tested without a clock or filesystem.
//! `window_state.json` is debounced the same way (it changes on every move/resize).
//!
//! ## Restore policy
//! [`AppState::resolve_artifact`] maps the saved `last_artifact_id` to a concrete
//! selection against the current library: the saved id if it still names a loaded
//! artifact, otherwise the first built-in (first-loaded) artifact — falling back
//! gracefully when an artifact was removed between runs.
//!
//! Per-artifact setting values are restored by [`restore_setting_values`], which
//! validates **each** stored value against the artifact's *current* declarations:
//! type, numeric range, and choice options. Any value that is missing, the wrong
//! type, or out of range falls back to the declared default; a stored entry naming a
//! setting the artifact no longer declares is dropped; a stored block for an artifact
//! id absent from the library is simply never consulted (graceful fallback so a
//! changed or removed artifact never breaks restore).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use viz_contract::{ArtifactLibrary, LoadedArtifact, SettingDecl};
use viz_core::ArtifactId;

use crate::runtime::{default_setting_value, SettingValue};

/// Qualifier/organization/application used to resolve the platform config dir.
/// On macOS `directories` joins these with `.` into the reverse-DNS bundle id
/// `io.github.acrive82.viewmusic`, giving the config dir
/// `~/Library/Application Support/io.github.acrive82.viewmusic/`; on Windows the
/// qualifier is dropped, yielding `%APPDATA%\acrive82\viewmusic\config\`.
const QUALIFIER: &str = "io.github";
const ORGANIZATION: &str = "acrive82";
const APPLICATION: &str = "viewmusic";

/// Minimum interval between `window_state.json` writes (debounce). Move/resize
/// events fire far faster than this; we coalesce them, plus a final flush on quit.
pub const WINDOW_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Persisted top-level app state (`app_state.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    /// The last selected artifact id (`meta.id`), or `None` on first run.
    pub last_artifact_id: Option<String>,
    /// Per-artifact setting overrides: `artifact_id -> (setting_name -> value)`.
    ///
    /// Populated + validated against the artifact's declarations on restore (invalid
    /// or missing → declared default; see [`restore_setting_values`]). Stored as JSON
    /// values so number / toggle / choice / color all round-trip without a schema
    /// here: number → JSON number, toggle → JSON bool, choice → the selected option
    /// string, color → a `#RRGGBB`/`#RRGGBBAA` hex string.
    pub setting_values: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    /// Whether the top-right overlay panel is collapsed to its chevron. The
    /// auto-hide timer is *not* persisted — only this explicit collapse state.
    pub overlay_collapsed: bool,
}

/// Persisted window geometry (`window_state.json`).
///
/// Kept separate from [`AppState`] because it changes on every move/resize and is
/// written on a debounce, whereas [`AppState`] changes only on discrete actions.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Window top-left x in physical pixels (winit outer position).
    pub x: i32,
    /// Window top-left y in physical pixels.
    pub y: i32,
    /// Window width in physical pixels.
    pub width: u32,
    /// Window height in physical pixels.
    pub height: u32,
}

impl AppState {
    /// Loads `app_state.json`, returning [`AppState::default`] if it is absent or
    /// unreadable/corrupt (a missing/garbled file must never crash startup —
    /// persistence is best-effort).
    pub fn load() -> Self {
        read_json::<Self>("app_state.json").unwrap_or_default()
    }

    /// Saves `app_state.json` (best-effort: a write failure is logged, not fatal).
    pub fn save(&self) {
        write_json("app_state.json", self);
    }

    /// Resolves the artifact to activate at launch against the current library:
    /// the saved id if it still names a loaded artifact, else the first loaded
    /// (built-in) artifact, else `None` (empty library).
    pub fn resolve_artifact(&self, library: &ArtifactLibrary) -> Option<ArtifactId> {
        resolve_artifact(self.last_artifact_id.as_deref(), library)
    }
}

impl WindowState {
    /// Loads `window_state.json`, or `None` if absent/corrupt (fall back to the
    /// platform default size + placement).
    pub fn load() -> Option<Self> {
        read_json::<Self>("window_state.json")
    }

    /// Saves `window_state.json` (best-effort).
    pub fn save(&self) {
        write_json("window_state.json", self);
    }
}

/// Pure resolution of the launch artifact (extracted for testability).
///
/// `saved_id` is the persisted `last_artifact_id`. Returns the saved id if it is a
/// loaded artifact in `library`, otherwise the library's first loaded id, otherwise
/// `None`.
pub fn resolve_artifact(saved_id: Option<&str>, library: &ArtifactLibrary) -> Option<ArtifactId> {
    if let Some(raw) = saved_id {
        if let Ok(id) = ArtifactId::parse(raw) {
            if library.contains(&id) {
                return Some(id);
            }
        }
    }
    library.first_loaded_id().cloned()
}

/// Pure debounce decision for a `window_state.json` write (extracted for testing).
///
/// Returns `true` when there has been no prior write, or at least `debounce` has
/// elapsed since `last_saved`. The caller records `now` as the new `last_saved` only
/// when this returns `true` (and forces a final write on quit regardless).
#[must_use]
pub fn should_save_window(now: Instant, last_saved: Option<Instant>, debounce: Duration) -> bool {
    match last_saved {
        None => true,
        Some(prev) => now.duration_since(prev) >= debounce,
    }
}

// ---------------------------------------------------------------------------
// Per-artifact setting values
// ---------------------------------------------------------------------------

impl AppState {
    /// Stores the full current setting map for `artifact_id`, in the JSON-value form
    /// described on [`AppState::setting_values`]. Replaces any previous block
    /// for that id. Call on every edit/reset; the host then flushes to disk on a
    /// debounce (and on switch/quit).
    pub fn set_artifact_settings(
        &mut self,
        artifact_id: &str,
        decls: &IndexMap<String, SettingDecl>,
        values: &IndexMap<String, SettingValue>,
    ) {
        let block = setting_values_to_json(decls, values);
        self.setting_values.insert(artifact_id.to_owned(), block);
    }

    /// Restores the validated [`SettingValue`] map for `art` from this state: every
    /// declared setting gets either its validated stored value or its declared
    /// default. See [`restore_setting_values`] for the validation rules.
    pub fn artifact_settings(&self, art: &LoadedArtifact) -> IndexMap<String, SettingValue> {
        let stored = self.setting_values.get(art.id.0.as_str());
        restore_setting_values(art, stored)
    }
}

/// Serializes a runtime [`SettingValue`] map into the persisted JSON form, using the
/// declarations to choose the per-kind encoding (choice → option string, color →
/// hex). Settings present in `values` but not in `decls` are skipped. The result is a
/// [`BTreeMap`] so the on-disk key order is stable.
pub fn setting_values_to_json(
    decls: &IndexMap<String, SettingDecl>,
    values: &IndexMap<String, SettingValue>,
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    for (name, decl) in decls {
        let Some(value) = values.get(name) else {
            continue;
        };
        if let Some(json) = setting_value_to_json(decl, value) {
            out.insert(name.clone(), json);
        }
    }
    out
}

/// Encodes one [`SettingValue`] to its persisted JSON per its declaration kind.
/// Returns `None` on a kind/value mismatch (cannot happen for values produced by the
/// runtime, but handled defensively).
fn setting_value_to_json(decl: &SettingDecl, value: &SettingValue) -> Option<serde_json::Value> {
    match (decl, value) {
        (SettingDecl::Number { .. }, SettingValue::Scalar(v)) => Some(serde_json::json!(v)),
        (SettingDecl::Toggle { .. }, SettingValue::Scalar(v)) => Some(serde_json::json!(*v >= 0.5)),
        (SettingDecl::Choice { options, .. }, SettingValue::Scalar(v)) => {
            // Store the selected option *string* (robust to option reordering).
            let idx = *v as usize;
            options.get(idx).map(|o| serde_json::json!(o))
        }
        (SettingDecl::Color { default, .. }, SettingValue::Color { r, g, b, a }) => Some(
            serde_json::json!(color_to_hex(*r, *g, *b, *a, hex_has_alpha(default))),
        ),
        // Kind mismatch: skip rather than persist a corrupt entry.
        _ => None,
    }
}

/// Validates a stored per-artifact setting block against `art`'s current declarations
/// and returns the resolved [`SettingValue`] map in declaration order.
///
/// For every declared setting: if `stored` carries a value of the matching type and
/// within range/options, that value is used; otherwise the declared default. A stored
/// key naming a setting `art` no longer declares is silently dropped (it is never
/// visited). `stored == None` (no block for this artifact) yields all defaults.
pub fn restore_setting_values(
    art: &LoadedArtifact,
    stored: Option<&BTreeMap<String, serde_json::Value>>,
) -> IndexMap<String, SettingValue> {
    let mut out = IndexMap::with_capacity(art.artifact.settings.len());
    for (name, decl) in &art.artifact.settings {
        let resolved = stored
            .and_then(|m| m.get(name))
            .and_then(|json| validate_setting_value(decl, json))
            .unwrap_or_else(|| default_setting_value(decl));
        out.insert(name.clone(), resolved);
    }
    out
}

/// Validates one stored JSON value against `decl`, returning the [`SettingValue`] if
/// it is the right type and within range/options, else `None` (caller falls back to
/// the declared default).
fn validate_setting_value(decl: &SettingDecl, json: &serde_json::Value) -> Option<SettingValue> {
    match decl {
        SettingDecl::Number { min, max, .. } => {
            let v = json.as_f64()?;
            // Reject NaN/inf and out-of-range values (→ default).
            if v.is_finite() && (*min..=*max).contains(&v) {
                Some(SettingValue::Scalar(v))
            } else {
                None
            }
        }
        SettingDecl::Toggle { .. } => {
            let b = json.as_bool()?;
            Some(SettingValue::Scalar(if b { 1.0 } else { 0.0 }))
        }
        SettingDecl::Choice { options, .. } => {
            // Stored as the selected option string; must still be a valid option.
            let s = json.as_str()?;
            let idx = options.iter().position(|o| o == s)?;
            Some(SettingValue::Scalar(idx as f64))
        }
        SettingDecl::Color { .. } => {
            let s = json.as_str()?;
            parse_hex_color(s).map(|(r, g, b, a)| SettingValue::Color { r, g, b, a })
        }
    }
}

/// Whether a `#RRGGBB`/`#RRGGBBAA` default carries an explicit alpha (8 hex digits).
fn hex_has_alpha(s: &str) -> bool {
    s.strip_prefix('#').unwrap_or(s).len() == 8
}

/// Encodes straight-alpha `0..1` channels as a `#RRGGBB`/`#RRGGBBAA` hex string.
/// `with_alpha` selects the 8-digit form (mirrors the declared default's width).
fn color_to_hex(r: f64, g: f64, b: f64, a: f64, with_alpha: bool) -> String {
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    if with_alpha {
        format!(
            "#{:02x}{:02x}{:02x}{:02x}",
            byte(r),
            byte(g),
            byte(b),
            byte(a)
        )
    } else {
        format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
    }
}

/// Parses a `#RRGGBB`/`#RRGGBBAA` hex color into straight-alpha `0..1` channels, or
/// `None` if the string is not a valid 6/8-digit hex color (so a corrupt stored value
/// falls back to the declared default rather than to opaque white).
fn parse_hex_color(s: &str) -> Option<(f64, f64, f64, f64)> {
    let body = s.strip_prefix('#').unwrap_or(s);
    if !body.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| -> Option<f64> {
        u8::from_str_radix(&body[i * 2..i * 2 + 2], 16)
            .ok()
            .map(|n| n as f64 / 255.0)
    };
    match body.len() {
        6 => Some((byte(0)?, byte(1)?, byte(2)?, 1.0)),
        8 => Some((byte(0)?, byte(1)?, byte(2)?, byte(3)?)),
        _ => None,
    }
}

/// Sub-directory of the project config dir holding user `.artifact.json` files.
/// The full path is the config dir (see module docs) joined with `artifacts`:
/// `~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/` on
/// macOS, `%APPDATA%\acrive82\viewmusic\config\artifacts\` on Windows.
const ARTIFACTS_SUBDIR: &str = "artifacts";

/// Resolves the project config directory, creating it if needed. `None` if the
/// platform yields no home/config dir or the directory cannot be created.
fn config_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)?;
    let dir = dirs.config_dir().to_path_buf();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "viz_app::persist",
            dir = %dir.display(),
            error = %e,
            "could not create config directory; persistence disabled this run"
        );
        return None;
    }
    Some(dir)
}

/// Resolves the user artifacts folder, **creating it if missing**, and returns its
/// path: `~/Library/Application Support/io.github.acrive82.viewmusic/artifacts/` on
/// macOS, `%APPDATA%\acrive82\viewmusic\config\artifacts\` on Windows.
///
/// Returns `None` when the platform yields no config dir or the directory cannot be
/// created — the app then runs on built-ins only (best-effort, never fatal). Call at
/// launch so a first-run user has a folder to drop files into, and again before each
/// reload scan.
pub fn ensure_artifacts_dir() -> Option<PathBuf> {
    let dir = config_dir()?.join(ARTIFACTS_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "viz_app::persist",
            dir = %dir.display(),
            error = %e,
            "could not create artifacts folder; user artifacts disabled this run"
        );
        return None;
    }
    Some(dir)
}

/// Reads + deserializes a JSON document from the config dir, or `None` on any
/// failure (absent file, parse error, no config dir). Best-effort by design.
fn read_json<T: for<'de> Deserialize<'de>>(file: &str) -> Option<T> {
    let path = config_dir()?.join(file);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        // A missing file is the normal first-run case — not worth a warning.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                target: "viz_app::persist",
                path = %path.display(),
                error = %e,
                "could not read persisted state; using defaults"
            );
            return None;
        }
    };
    match serde_json::from_slice::<T>(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                target: "viz_app::persist",
                path = %path.display(),
                error = %e,
                "persisted state is corrupt; using defaults"
            );
            None
        }
    }
}

/// Serializes + writes a JSON document into the config dir (best-effort; logs on
/// failure, never panics).
fn write_json<T: Serialize>(file: &str, value: &T) {
    let Some(dir) = config_dir() else {
        return;
    };
    let path = dir.join(file);
    let json = match serde_json::to_vec_pretty(value) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(
                target: "viz_app::persist",
                path = %path.display(),
                error = %e,
                "could not serialize persisted state"
            );
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, json) {
        tracing::warn!(
            target: "viz_app::persist",
            path = %path.display(),
            error = %e,
            "could not write persisted state"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn library() -> ArtifactLibrary {
        let a = artifact_json("alpha", "Alpha");
        let b = artifact_json("bravo", "Bravo");
        ArtifactLibrary::load_builtins([
            ("alpha (built-in)", a.as_str()),
            ("bravo (built-in)", b.as_str()),
        ])
    }

    #[test]
    fn resolve_keeps_saved_id_when_still_present() {
        let lib = library();
        let id = resolve_artifact(Some("bravo"), &lib).unwrap();
        assert_eq!(id.0, "bravo");
    }

    #[test]
    fn resolve_falls_back_to_first_when_saved_missing() {
        let lib = library();
        // Saved id is gone: fall back to the first built-in.
        let id = resolve_artifact(Some("charlie"), &lib).unwrap();
        assert_eq!(id.0, "alpha");
    }

    #[test]
    fn resolve_falls_back_when_no_saved_id() {
        let lib = library();
        let id = resolve_artifact(None, &lib).unwrap();
        assert_eq!(id.0, "alpha");
    }

    #[test]
    fn resolve_falls_back_when_saved_id_is_malformed() {
        let lib = library();
        // A malformed saved id cannot match any loaded id → fall back to first.
        let id = resolve_artifact(Some("Not A Valid Id!"), &lib).unwrap();
        assert_eq!(id.0, "alpha");
    }

    #[test]
    fn resolve_none_for_empty_library() {
        let lib = ArtifactLibrary::new();
        assert!(resolve_artifact(Some("alpha"), &lib).is_none());
        assert!(resolve_artifact(None, &lib).is_none());
    }

    #[test]
    fn window_save_debounce() {
        let start = Instant::now();
        // First write always allowed.
        assert!(should_save_window(start, None, WINDOW_SAVE_DEBOUNCE));
        // Too soon after a save → skip.
        let soon = start + Duration::from_millis(100);
        assert!(!should_save_window(soon, Some(start), WINDOW_SAVE_DEBOUNCE));
        // Past the debounce window → save.
        let later = start + WINDOW_SAVE_DEBOUNCE + Duration::from_millis(1);
        assert!(should_save_window(later, Some(start), WINDOW_SAVE_DEBOUNCE));
        // Exactly at the boundary → save.
        let at = start + WINDOW_SAVE_DEBOUNCE;
        assert!(should_save_window(at, Some(start), WINDOW_SAVE_DEBOUNCE));
    }

    #[test]
    fn app_state_roundtrips_through_json() {
        let mut state = AppState {
            last_artifact_id: Some("bravo".to_owned()),
            setting_values: BTreeMap::new(),
            overlay_collapsed: true,
        };
        state
            .setting_values
            .entry("bravo".to_owned())
            .or_default()
            .insert("speed".to_owned(), serde_json::json!(1.5));

        let json = serde_json::to_string(&state).unwrap();
        let back: AppState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn app_state_default_when_fields_missing() {
        // An empty object deserializes via serde(default) — forward compatible.
        let back: AppState = serde_json::from_str("{}").unwrap();
        assert_eq!(back, AppState::default());
        assert!(back.last_artifact_id.is_none());
        assert!(!back.overlay_collapsed);
    }

    #[test]
    fn window_state_roundtrips() {
        let ws = WindowState {
            x: 100,
            y: 200,
            width: 1280,
            height: 720,
        };
        let json = serde_json::to_string(&ws).unwrap();
        let back: WindowState = serde_json::from_str(&json).unwrap();
        assert_eq!(ws, back);
    }

    // ---- per-artifact setting persistence -----------------------------------

    /// A settings-bearing artifact covering every kind, for the persist tests.
    fn settings_artifact() -> LoadedArtifact {
        let src = r##"{
            "contract": "1.0",
            "meta": { "id": "sa", "name": "SA" },
            "settings": {
                "num":   { "type": "number", "label": "N", "min": 0, "max": 10, "default": 5 },
                "on":    { "type": "toggle", "label": "On", "default": false },
                "pick":  { "type": "choice", "label": "Pick", "options": ["a", "b"], "default": "a" },
                "col":   { "type": "color", "label": "Col", "default": "#102030" },
                "cola":  { "type": "color", "label": "Cola", "default": "#10203040" }
            },
            "scene": [
                { "type": "instanced", "shape": "rect", "count": "settings.num",
                  "element": { "x": "0", "y": "0", "w": "0.1", "h": "0.1",
                    "color": { "model": "rgba", "r": "settings.col_r", "g": "1", "b": "1" } } }
            ]
        }"##;
        viz_contract::load_artifact("test", src.as_bytes()).expect("test artifact loads")
    }

    #[test]
    fn to_json_encodes_each_kind() {
        let art = settings_artifact();
        let mut values: IndexMap<String, SettingValue> = IndexMap::new();
        values.insert("num".into(), SettingValue::Scalar(7.0));
        values.insert("on".into(), SettingValue::Scalar(1.0));
        values.insert("pick".into(), SettingValue::Scalar(1.0)); // "b"
        values.insert(
            "col".into(),
            SettingValue::Color {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        );
        values.insert(
            "cola".into(),
            SettingValue::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            },
        );
        let block = setting_values_to_json(&art.artifact.settings, &values);
        assert_eq!(block["num"], serde_json::json!(7.0));
        assert_eq!(block["on"], serde_json::json!(true));
        assert_eq!(block["pick"], serde_json::json!("b"));
        assert_eq!(block["col"], serde_json::json!("#ff0000")); // 6-digit default
                                                                // 8-digit default keeps alpha; 0.5 → 0x80.
        assert_eq!(block["cola"], serde_json::json!("#00000080"));
    }

    #[test]
    fn round_trip_validates_back_to_values() {
        let art = settings_artifact();
        let mut values: IndexMap<String, SettingValue> = IndexMap::new();
        values.insert("num".into(), SettingValue::Scalar(3.0));
        values.insert("on".into(), SettingValue::Scalar(1.0));
        values.insert("pick".into(), SettingValue::Scalar(1.0));
        values.insert(
            "col".into(),
            SettingValue::Color {
                r: 1.0,
                g: 0.5,
                b: 0.0,
                a: 1.0,
            },
        );
        values.insert(
            "cola".into(),
            SettingValue::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.5,
            },
        );

        let block = setting_values_to_json(&art.artifact.settings, &values);
        let restored = restore_setting_values(&art, Some(&block));
        assert_eq!(restored["num"], SettingValue::Scalar(3.0));
        assert_eq!(restored["on"], SettingValue::Scalar(1.0));
        assert_eq!(restored["pick"], SettingValue::Scalar(1.0));
        match restored["cola"] {
            SettingValue::Color { a, .. } => assert!((a - 0.5).abs() < 2e-2),
            other => panic!("expected color, got {other:?}"),
        }
    }

    #[test]
    fn restore_falls_back_for_invalid_and_missing() {
        let art = settings_artifact();
        let mut block = BTreeMap::new();
        block.insert("num".to_owned(), serde_json::json!(999)); // out of range
        block.insert("pick".to_owned(), serde_json::json!("z")); // not an option
                                                                 // `on`, `col`, `cola` missing → defaults.
        let restored = restore_setting_values(&art, Some(&block));
        assert_eq!(restored["num"], SettingValue::Scalar(5.0)); // default
        assert_eq!(restored["pick"], SettingValue::Scalar(0.0)); // default "a"
        assert_eq!(restored["on"], SettingValue::Scalar(0.0)); // default false
    }

    #[test]
    fn app_state_setting_values_round_trip_through_json() {
        let art = settings_artifact();
        let mut values: IndexMap<String, SettingValue> = IndexMap::new();
        values.insert("num".into(), SettingValue::Scalar(2.0));
        let mut state = AppState::default();
        state.set_artifact_settings("sa", &art.artifact.settings, &values);

        let json = serde_json::to_string(&state).unwrap();
        let back: AppState = serde_json::from_str(&json).unwrap();
        let restored = back.artifact_settings(&art);
        assert_eq!(restored["num"], SettingValue::Scalar(2.0));
    }
}
