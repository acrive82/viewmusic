//! Artifact loading & validation pipeline (contract §1/§10/§11).
//!
//! [`load_artifact`] turns raw `.artifact.json` bytes into a [`LoadedArtifact`]:
//! the typed [`Artifact`], its [`viz_core::ArtifactId`] and `rand` seed, a
//! [`SlotLayout`] mapping every readable identifier to a dense VM slot, and an
//! [`ArtifactPrograms`] of compiled [`CompiledFormula`]s mirroring the artifact's
//! shape. Phase 3 (the runtime) is written against this module **sight-unseen**,
//! so the public types here are documented as a stable contract.
//!
//! # Pipeline order (first failure wins)
//!
//! 1. **Size** — reject files larger than [`limits::MAX_FILE_BYTES`], then JSON
//!    parse to [`serde_json::Value`] ([`LoadStep::Parse`]).
//! 2. **Version** — *before* schema validation: `contract` must be a string
//!    `"MAJOR.MINOR"` with `MAJOR == 1` and `MINOR <= 0`; otherwise reject with
//!    exactly `unsupported contract version X.Y` ([`LoadStep::Version`]).
//! 3. **Schema** — JSON Schema (draft 2020-12) validation against the schema
//!    generated once from [`Artifact`] via `schemars` and compiled once with the
//!    `jsonschema` crate ([`LoadStep::Schema`]); reports the failing instance path.
//! 4. **Deserialize** — typed [`serde_json::from_value`] into [`Artifact`]
//!    ([`LoadStep::Deserialize`]).
//! 5. **Semantic** — id format, setting/var name rules & reserved-name collisions
//!    (including color `_r/_g/_b/_a` expansions), choice-default-in-options,
//!    `min < max` / default-in-range / `step > 0`, color-default hex pattern, and
//!    the count caps ([`LoadStep::Semantic`]).
//! 6. **Formula** — compile *every* formula expression against a single master
//!    [`viz_expr::Scope`], enforce per-stage identifier restrictions and the
//!    artifact-wide op cap ([`LoadStep::Formula`]). Numeric constants are recorded
//!    as [`CompiledFormula::Const`] without compilation.
//!
//! Every failure is reported as a [`LoadDiagnostic`] whose `Display` is a single
//! English log line (so every rejection is explained in the log).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use indexmap::IndexMap;
use jsonschema::Validator;
use serde_json::Value;

use crate::types::{limits, Artifact, ColorSpec, Feedback, Formula, Layer, SettingDecl};
use viz_core::ArtifactId;
use viz_expr::{compile, lex, Program, Scope, SlotId, TokenKind, Vm};

// ---------------------------------------------------------------------------
// Shared inputs & reserved identifiers
// ---------------------------------------------------------------------------

/// The nine audio/time/render inputs available in **every** stage, in the exact
/// order they occupy the leading slots of the master [`Scope`] (contract §2.3).
///
/// This order is part of the published [`SlotLayout`] contract: the first nine
/// slots are always these, in this sequence.
pub const SHARED_INPUTS: [&str; 9] = [
    "t",
    "dt",
    "energy",
    "low",
    "mid",
    "high",
    "beat",
    "beat_count",
    "aspect",
];

/// Stage-extra identifiers that are only legal in *some* stages (contract §2.3):
/// `i`, `n`, `u` (element/point), `x`, `y` (cell). These always occupy the final
/// five slots of the master scope, in this order.
pub const STAGE_EXTRAS: [&str; 5] = ["i", "n", "u", "x", "y"];

/// Reserved identifier names a setting or var must not shadow: the shared inputs
/// plus the stage extras (contract §4 — §2.3 names are reserved). `viz-expr`
/// built-in function/constant names are additionally rejected by the scope
/// builder, but we check them here too for a precise [`LoadStep::Semantic`]
/// diagnostic rather than a later compile error.
const RESERVED_HOST_NAMES: [&str; 14] = [
    "t",
    "dt",
    "energy",
    "low",
    "mid",
    "high",
    "beat",
    "beat_count",
    "aspect", // shared
    "i",
    "n",
    "u",
    "x",
    "y", // stage extras
];

/// `viz-expr` built-in function and constant names (contract §2.2). A setting or
/// var bearing one of these is rejected at the semantic step.
const RESERVED_BUILTIN_NAMES: [&str; 32] = [
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "exp",
    "log",
    "log2",
    "log10",
    "pow",
    "sqrt",
    "abs",
    "sign",
    "floor",
    "ceil",
    "round",
    "fract",
    "min",
    "max",
    "clamp",
    "mix",
    "smoothstep",
    "step",
    "if",
    "band",
    "wave",
    "rand",
    "pi",
    "tau",
    "e",
];

/// True if `name` collides with any reserved host input, stage extra, or built-in.
fn is_reserved_name(name: &str) -> bool {
    RESERVED_HOST_NAMES.contains(&name) || RESERVED_BUILTIN_NAMES.contains(&name)
}

// ---------------------------------------------------------------------------
// Published schema (generated once, compiled once)
// ---------------------------------------------------------------------------

/// The schema generated from [`Artifact`] via `schemars`, as a `serde_json::Value`.
/// Built once (it is pure and read-only) for both [`published_schema`] and the
/// compiled [`SCHEMA_VALIDATOR`].
static SCHEMA_VALUE: LazyLock<Value> = LazyLock::new(|| {
    let schema = schemars::schema_for!(Artifact);
    serde_json::to_value(&schema).expect("Artifact schema serializes to JSON")
});

/// The compiled draft 2020-12 validator for [`SCHEMA_VALUE`]. Compiled once.
static SCHEMA_VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    jsonschema::draft202012::options()
        // The generated schema uses no `format` keywords meaningfully; ignore any
        // unknown ones rather than failing schema compilation.
        .should_ignore_unknown_formats(true)
        .build(&SCHEMA_VALUE)
        .expect("generated Artifact schema compiles as a draft 2020-12 validator")
});

/// The published JSON Schema (draft 2020-12) for the artifact contract, generated
/// from the typed [`Artifact`] model. Stable for the process lifetime.
pub fn published_schema() -> &'static Value {
    &SCHEMA_VALUE
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Which validation step rejected a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadStep {
    /// Size check / JSON syntax parse.
    Parse,
    /// Contract version gate (`unsupported contract version X.Y`).
    Version,
    /// JSON Schema (draft 2020-12) validation.
    Schema,
    /// Typed deserialization into [`Artifact`].
    Deserialize,
    /// Semantic checks (names, ranges, caps).
    Semantic,
    /// Formula compilation / stage restriction / op cap.
    Formula,
}

impl LoadStep {
    /// Lowercase English tag used in the [`LoadDiagnostic`] log line.
    fn tag(self) -> &'static str {
        match self {
            LoadStep::Parse => "parse",
            LoadStep::Version => "version",
            LoadStep::Schema => "schema",
            LoadStep::Deserialize => "deserialize",
            LoadStep::Semantic => "semantic",
            LoadStep::Formula => "formula",
        }
    }
}

impl std::fmt::Display for LoadStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

/// A single load failure, formatted as one English log line.
///
/// `Display` renders `file <name>: [<step>] at <json_path>: <message>`.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadDiagnostic {
    /// The file name (or other source label) passed to [`load_artifact`].
    pub source_name: String,
    /// The pipeline step that rejected the file.
    pub step: LoadStep,
    /// JSON path of the offending location (e.g. `scene[0].element.color.h`).
    /// Empty (`""`) when the failure has no meaningful location (e.g. file size).
    pub json_path: String,
    /// Human-readable English explanation, naming the offending token where known.
    pub message: String,
}

impl LoadDiagnostic {
    fn new(
        source_name: &str,
        step: LoadStep,
        json_path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source_name: source_name.to_owned(),
            step,
            json_path: json_path.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LoadDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file {}: [{}] at {}: {}",
            self.source_name, self.step, self.json_path, self.message
        )
    }
}

impl std::error::Error for LoadDiagnostic {}

// ---------------------------------------------------------------------------
// Slot layout
// ---------------------------------------------------------------------------

/// The slots a single setting occupies in the VM bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSlots {
    /// A `number`, `toggle`, or `choice` setting — one scalar slot read as
    /// `settings.<name>` (number value, `0.0`/`1.0`, or 0-based choice index).
    Scalar(SlotId),
    /// A `color` setting — four channel slots read as
    /// `settings.<name>_r/_g/_b/_a`, each in `0..1`.
    Color {
        /// Red channel slot (`settings.<name>_r`).
        r: SlotId,
        /// Green channel slot (`settings.<name>_g`).
        g: SlotId,
        /// Blue channel slot (`settings.<name>_b`).
        b: SlotId,
        /// Alpha channel slot (`settings.<name>_a`).
        a: SlotId,
    },
}

/// The complete name→slot mapping for one artifact's master [`viz_expr::Scope`].
///
/// **Slot order (stable contract)** — a single [`Vm`] bank of `slot_count` f64s
/// serves every stage. Slots are assigned in this exact sequence so the runtime
/// can rely on it:
///
/// 1. the nine [`SHARED_INPUTS`] (`t, dt, energy, low, mid, high, beat,
///    beat_count, aspect`), in that order — see the named fields below;
/// 2. per-setting slots in artifact declaration order: one [`SettingSlots::Scalar`]
///    for number/toggle/choice, four [`SettingSlots::Color`] channels for color
///    (`_r, _g, _b, _a`);
/// 3. one slot per var, in declaration order (`vars`);
/// 4. the five [`STAGE_EXTRAS`] (`i, n, u, x, y`), in that order.
///
/// The runtime writes shared inputs and stage extras directly via the named slot
/// fields, settings via [`SlotLayout::settings`], and var current-frame values via
/// [`SlotLayout::vars`]; then it evaluates the matching [`CompiledFormula`]s.
#[derive(Clone, Debug)]
pub struct SlotLayout {
    /// `t` — audio clock (seconds).
    pub t: SlotId,
    /// `dt` — audio-clock delta between frames (seconds).
    pub dt: SlotId,
    /// `energy` — smoothed loudness, 0..1.
    pub energy: SlotId,
    /// `low` — low-band aggregate, 0..1.
    pub low: SlotId,
    /// `mid` — mid-band aggregate, 0..1.
    pub mid: SlotId,
    /// `high` — high-band aggregate, 0..1.
    pub high: SlotId,
    /// `beat` — onset pulse, 0..1.
    pub beat: SlotId,
    /// `beat_count` — onset counter since start.
    pub beat_count: SlotId,
    /// `aspect` — render width / height.
    pub aspect: SlotId,

    /// `i` — element/point/cell index (element, point, and cell stages).
    pub i: SlotId,
    /// `n` — element/point/cell count this frame.
    pub n: SlotId,
    /// `u` — normalized position `i/max(n-1,1)` (element and point stages).
    pub u: SlotId,
    /// `x` — cell-center x in −1..1 (field/cell stage only).
    pub x: SlotId,
    /// `y` — cell-center y in −1..1 (field/cell stage only).
    pub y: SlotId,

    /// Per-setting slots, keyed by setting name, in declaration order.
    pub settings: IndexMap<String, SettingSlots>,
    /// One slot per var, keyed by var name, in declaration order. The runtime
    /// writes each var's current-frame value here before reading it in formulas.
    pub vars: IndexMap<String, SlotId>,

    /// Total number of slots — the size of the [`Vm`] bank to allocate.
    pub slot_count: usize,
}

// ---------------------------------------------------------------------------
// Compiled formulas & programs
// ---------------------------------------------------------------------------

/// A compiled formula: either a numeric constant (no bytecode) or a [`Program`].
///
/// [`CompiledFormula::value`] evaluates it against a [`Vm`] — a constant is
/// returned directly (already sanitized to finite at compile time), a program is
/// run. No allocation occurs in [`CompiledFormula::value`].
#[derive(Clone, Debug)]
pub enum CompiledFormula {
    /// A JSON-number constant. Stored finite (non-finite collapses to `0.0`).
    Const(f64),
    /// A compiled expression program.
    Program(Program),
}

impl CompiledFormula {
    /// Evaluates this formula against `vm`, returning a sanitized finite value.
    #[inline]
    pub fn value(&self, vm: &mut Vm) -> f64 {
        match self {
            CompiledFormula::Const(c) => *c,
            CompiledFormula::Program(p) => vm.eval(p),
        }
    }

    /// Number of compiled ops (0 for a constant).
    pub fn op_count(&self) -> usize {
        match self {
            CompiledFormula::Const(_) => 0,
            CompiledFormula::Program(p) => p.op_count(),
        }
    }
}

/// The color model of a compiled [`ColorPrograms`] (mirrors [`ColorSpec`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorModel {
    /// `rgba`: channels are red/green/blue, each clamped 0..1 by the runtime.
    Rgba,
    /// `hsva`: channels are hue (degrees, wrapped) / saturation / value.
    Hsva,
}

/// Compiled color channels (contract §7). `c0/c1/c2` are `r/g/b` for [`ColorModel::Rgba`]
/// and `h/s/v` for [`ColorModel::Hsva`]; `a` is `None` when alpha was omitted
/// (runtime default 1).
#[derive(Clone, Debug)]
pub struct ColorPrograms {
    /// Which channel triple this represents.
    pub model: ColorModel,
    /// First channel (`r` or `h`).
    pub c0: CompiledFormula,
    /// Second channel (`g` or `s`).
    pub c1: CompiledFormula,
    /// Third channel (`b` or `v`).
    pub c2: CompiledFormula,
    /// Optional alpha channel; `None` ⇒ default 1.
    pub a: Option<CompiledFormula>,
}

/// Compiled programs for one layer, mirroring the [`Layer`] variants.
#[derive(Clone, Debug)]
pub enum LayerPrograms {
    /// An `instanced` layer (per-element programs run with `i`,`n`,`u`).
    Instanced {
        /// Per-frame instance count (frame stage: no stage extras).
        count: CompiledFormula,
        /// Optional per-frame visibility gate (frame stage).
        visible: Option<CompiledFormula>,
        /// Element center x (element stage).
        x: CompiledFormula,
        /// Element center y (element stage).
        y: CompiledFormula,
        /// Element width (element stage).
        w: CompiledFormula,
        /// Element height (element stage).
        h: CompiledFormula,
        /// Optional element rotation in radians (element stage); `None` ⇒ 0.
        rot: Option<CompiledFormula>,
        /// Element color (element stage).
        color: ColorPrograms,
    },
    /// A `polyline` layer (per-point programs run with `i`,`n`,`u`).
    Polyline {
        /// Per-frame point count (frame stage).
        points: CompiledFormula,
        /// Stroke thickness (frame stage).
        thickness: CompiledFormula,
        /// Optional per-frame visibility gate (frame stage).
        visible: Option<CompiledFormula>,
        /// Point x (point stage).
        x: CompiledFormula,
        /// Point y (point stage).
        y: CompiledFormula,
        /// Point color (point stage).
        color: ColorPrograms,
        /// Whether the strip closes last→first (static, from the JSON `closed`).
        closed: bool,
    },
    /// A `field` layer (per-cell color runs with `x`,`y`,`i`,`n`).
    Field {
        /// Per-frame grid resolution (frame stage).
        resolution: CompiledFormula,
        /// Optional per-frame visibility gate (frame stage).
        visible: Option<CompiledFormula>,
        /// Cell color (cell stage).
        color: ColorPrograms,
    },
}

/// All compiled programs for an artifact, grouped by stage.
#[derive(Clone, Debug)]
pub struct ArtifactPrograms {
    /// Var `init` programs, in [`SlotLayout::vars`] order (frame stage / no extras).
    pub vars_init: Vec<CompiledFormula>,
    /// Var `frame` programs, in the same order (frame stage / no extras).
    pub vars_frame: Vec<CompiledFormula>,
    /// Per-layer programs, in `scene` order.
    pub layers: Vec<LayerPrograms>,
    /// Optional `feedback.decay` program (frame stage / no extras).
    pub feedback_decay: Option<CompiledFormula>,
}

// ---------------------------------------------------------------------------
// Loaded artifact
// ---------------------------------------------------------------------------

/// A fully validated, compiled artifact ready for the runtime.
#[derive(Clone, Debug)]
pub struct LoadedArtifact {
    /// The typed, schema-valid source model.
    pub artifact: Artifact,
    /// Parsed `meta.id`.
    pub id: ArtifactId,
    /// Deterministic `rand(k)` seed derived from `id` (contract §2.2).
    pub seed: u64,
    /// Name→slot mapping for the single [`Vm`] bank.
    pub layout: SlotLayout,
    /// Compiled programs mirroring the artifact shape.
    pub programs: ArtifactPrograms,
}

// ---------------------------------------------------------------------------
// Stage restriction
// ---------------------------------------------------------------------------

/// The evaluation stage of a formula — decides which stage-extra identifiers
/// (`i`,`n`,`u`,`x`,`y`) are legal (contract §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// `vars` init/frame, layer `count`/`points`/`resolution`/`thickness`/
    /// `visible`, and `feedback.decay`: **no** stage extras allowed.
    Frame,
    /// `instanced.element` / `polyline.point`: `i`,`n`,`u` allowed; `x`,`y` not.
    ElementOrPoint,
    /// `field.cell`: `x`,`y`,`i`,`n` allowed; `u` not.
    Cell,
}

impl Stage {
    /// Returns the stage-extra identifiers that are **disallowed** in this stage.
    fn disallowed_extras(self) -> &'static [&'static str] {
        match self {
            // Frame stage forbids every stage extra.
            Stage::Frame => &["i", "n", "u", "x", "y"],
            // Element/point allow i,n,u — forbid x,y.
            Stage::ElementOrPoint => &["x", "y"],
            // Cell allows x,y,i,n — forbid u.
            Stage::Cell => &["u"],
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Loads, validates, and compiles one `.artifact.json` document from raw bytes.
///
/// Runs the full validation pipeline; the first failing step yields a
/// [`LoadDiagnostic`]. On success returns a [`LoadedArtifact`] whose
/// [`SlotLayout`] and [`ArtifactPrograms`] are the runtime's compiled view.
///
/// `source_name` is the file name (or other label) used in diagnostics only.
pub fn load_artifact(source_name: &str, bytes: &[u8]) -> Result<LoadedArtifact, LoadDiagnostic> {
    // Step 1: size, then JSON parse.
    if bytes.len() > limits::MAX_FILE_BYTES {
        return Err(LoadDiagnostic::new(
            source_name,
            LoadStep::Parse,
            "",
            format!(
                "file is {} bytes, exceeding the {} byte limit",
                bytes.len(),
                limits::MAX_FILE_BYTES
            ),
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|e| {
        LoadDiagnostic::new(
            source_name,
            LoadStep::Parse,
            "",
            format!("invalid JSON: {e}"),
        )
    })?;

    // Step 2: contract version gate (BEFORE schema validation).
    check_version(source_name, &value)?;

    // Step 3: JSON Schema validation.
    if let Some(err) = SCHEMA_VALIDATOR.iter_errors(&value).next() {
        let json_path = pointer_to_path(err.instance_path().as_str());
        return Err(LoadDiagnostic::new(
            source_name,
            LoadStep::Schema,
            json_path,
            format!("schema validation failed: {err}"),
        ));
    }

    // Step 4: typed deserialize.
    let artifact: Artifact = serde_json::from_value(value).map_err(|e| {
        LoadDiagnostic::new(
            source_name,
            LoadStep::Deserialize,
            "",
            format!("typed deserialization failed: {e}"),
        )
    })?;

    // Step 5: semantic checks.
    let id = check_semantics(source_name, &artifact)?;
    let seed = id.rand_seed();

    // Step 6: build the master scope, compile every formula, enforce caps.
    let (layout, programs) = compile_all(source_name, &artifact)?;

    Ok(LoadedArtifact {
        artifact,
        id,
        seed,
        layout,
        programs,
    })
}

// ---------------------------------------------------------------------------
// Step 2: version gate
// ---------------------------------------------------------------------------

/// Verifies `contract == "1.<minor>"` with `minor <= 0` (contract §10). Runs
/// before schema validation so an out-of-range version produces the clear
/// `unsupported contract version X.Y` message, not a pattern error.
fn check_version(source_name: &str, value: &Value) -> Result<(), LoadDiagnostic> {
    let raw = value
        .get("contract")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            LoadDiagnostic::new(
                source_name,
                LoadStep::Version,
                "contract",
                "missing or non-string 'contract' field (expected \"MAJOR.MINOR\")",
            )
        })?;

    let unsupported = |s: &str| {
        LoadDiagnostic::new(
            source_name,
            LoadStep::Version,
            "contract",
            format!("unsupported contract version {s}"),
        )
    };

    let (major_s, minor_s) = raw.split_once('.').ok_or_else(|| {
        LoadDiagnostic::new(
            source_name,
            LoadStep::Version,
            "contract",
            format!("malformed contract version {raw:?} (expected \"MAJOR.MINOR\")"),
        )
    })?;

    let (Ok(major), Ok(minor)) = (major_s.parse::<u32>(), minor_s.parse::<u32>()) else {
        return Err(LoadDiagnostic::new(
            source_name,
            LoadStep::Version,
            "contract",
            format!("malformed contract version {raw:?} (expected \"MAJOR.MINOR\")"),
        ));
    };

    if major != 1 || minor > 0 {
        return Err(unsupported(raw));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 5: semantic checks
// ---------------------------------------------------------------------------

/// Hex color-default pattern (contract §4): `#RRGGBB` or `#RRGGBBAA`.
fn is_hex_color(s: &str) -> bool {
    let body = match s.strip_prefix('#') {
        Some(b) => b,
        None => return false,
    };
    (body.len() == 6 || body.len() == 8) && body.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Setting/var name rule (contract §4): `[a-z][a-zA-Z0-9_]*`.
fn is_valid_decl_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Runs all post-deserialization semantic checks (pipeline step 5) and
/// returns the parsed [`ArtifactId`]. Counts caps, name rules + reserved-name
/// collisions (including color channel expansions), choice/number/color default
/// validity, and a non-empty scene.
fn check_semantics(source_name: &str, artifact: &Artifact) -> Result<ArtifactId, LoadDiagnostic> {
    let err =
        |path: String, msg: String| LoadDiagnostic::new(source_name, LoadStep::Semantic, path, msg);

    // meta.id format (length/charset via viz_core).
    let id = ArtifactId::parse(&artifact.meta.id)
        .map_err(|e| err("meta.id".to_owned(), format!("invalid artifact id: {e}")))?;

    // Count caps.
    if artifact.settings.len() > limits::MAX_SETTINGS {
        return Err(err(
            "settings".to_owned(),
            format!(
                "{} settings exceed the limit of {}",
                artifact.settings.len(),
                limits::MAX_SETTINGS
            ),
        ));
    }
    if artifact.vars.len() > limits::MAX_VARS {
        return Err(err(
            "vars".to_owned(),
            format!(
                "{} vars exceed the limit of {}",
                artifact.vars.len(),
                limits::MAX_VARS
            ),
        ));
    }
    if artifact.scene.is_empty() {
        return Err(err(
            "scene".to_owned(),
            "scene must contain at least one layer".to_owned(),
        ));
    }
    if artifact.scene.len() > limits::MAX_LAYERS {
        return Err(err(
            "scene".to_owned(),
            format!(
                "{} layers exceed the limit of {}",
                artifact.scene.len(),
                limits::MAX_LAYERS
            ),
        ));
    }

    // Build the set of declared identifier names as we go, detecting any
    // collision among settings (incl. color channel expansions) and vars.
    // `taken` maps a reserved/declared identifier name → the JSON path that owns
    // it, for a precise collision diagnostic.
    let mut taken: BTreeMap<String, String> = BTreeMap::new();

    // Settings.
    for (name, decl) in &artifact.settings {
        let base_path = format!("settings.{name}");
        check_decl_name(name, &base_path, &err)?;

        // The identifiers this setting publishes into the formula namespace.
        let published: Vec<String> = match decl {
            SettingDecl::Color { .. } => ["_r", "_g", "_b", "_a"]
                .iter()
                .map(|suffix| format!("{name}{suffix}"))
                .collect(),
            _ => vec![name.clone()],
        };

        for ident in &published {
            if is_reserved_name(ident) {
                return Err(err(
                    base_path.clone(),
                    format!(
                        "setting '{name}' publishes identifier '{ident}', which collides with a reserved name"
                    ),
                ));
            }
            if let Some(owner) = taken.get(ident) {
                return Err(err(
                    base_path.clone(),
                    format!(
                        "setting '{name}' publishes identifier '{ident}', which collides with '{owner}'"
                    ),
                ));
            }
            taken.insert(ident.clone(), base_path.clone());
        }

        // Per-kind default validity.
        check_setting_defaults(name, &base_path, decl, &err)?;
    }

    // Vars.
    for (name, _decl) in &artifact.vars {
        let path = format!("vars.{name}");
        check_decl_name(name, &path, &err)?;
        if is_reserved_name(name) {
            return Err(err(
                path.clone(),
                format!("var '{name}' collides with a reserved name"),
            ));
        }
        if let Some(owner) = taken.get(name) {
            return Err(err(
                path.clone(),
                format!("var '{name}' collides with '{owner}'"),
            ));
        }
        taken.insert(name.clone(), path);
    }

    Ok(id)
}

/// Validates a setting/var declaration name and rejects collisions with reserved
/// names. (Collisions among declared names are handled by the `taken` map in the
/// caller; this only covers the name *rule* and reserved-name shadowing for the
/// bare name itself.)
fn check_decl_name(
    name: &str,
    path: &str,
    err: &impl Fn(String, String) -> LoadDiagnostic,
) -> Result<(), LoadDiagnostic> {
    if !is_valid_decl_name(name) {
        return Err(err(
            path.to_owned(),
            format!("name '{name}' does not match the required pattern [a-z][a-zA-Z0-9_]*"),
        ));
    }
    Ok(())
}

/// Validates a setting's kind-specific default (and number `min<max`/`step>0`).
fn check_setting_defaults(
    name: &str,
    path: &str,
    decl: &SettingDecl,
    err: &impl Fn(String, String) -> LoadDiagnostic,
) -> Result<(), LoadDiagnostic> {
    match decl {
        SettingDecl::Number {
            min,
            max,
            step,
            default,
            ..
        } => {
            // JSON Schema already guarantees these are JSON numbers (never NaN),
            // so the positive comparisons are exact.
            if min >= max {
                return Err(err(
                    path.to_owned(),
                    format!("number setting '{name}' requires min ({min}) < max ({max})"),
                ));
            }
            if default < min || default > max {
                return Err(err(
                    path.to_owned(),
                    format!("number setting '{name}' default {default} is outside [{min}, {max}]"),
                ));
            }
            if let Some(step) = step {
                if *step <= 0.0 {
                    return Err(err(
                        path.to_owned(),
                        format!("number setting '{name}' step {step} must be > 0"),
                    ));
                }
            }
        }
        SettingDecl::Choice {
            options, default, ..
        } => {
            if !options.iter().any(|o| o == default) {
                return Err(err(
                    path.to_owned(),
                    format!(
                        "choice setting '{name}' default {default:?} is not one of its options"
                    ),
                ));
            }
        }
        SettingDecl::Color { default, .. } => {
            if !is_hex_color(default) {
                return Err(err(
                    path.to_owned(),
                    format!(
                        "color setting '{name}' default {default:?} must match #RRGGBB or #RRGGBBAA"
                    ),
                ));
            }
        }
        SettingDecl::Toggle { .. } => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 6: scope construction + formula compilation
// ---------------------------------------------------------------------------

/// Builds the master scope and the [`SlotLayout`], compiles every formula with
/// per-stage restriction enforcement, and enforces the artifact-wide op cap.
fn compile_all(
    source_name: &str,
    artifact: &Artifact,
) -> Result<(SlotLayout, ArtifactPrograms), LoadDiagnostic> {
    // Build the master scope in the documented stable order:
    //   shared inputs → per-setting slots → vars → stage extras.
    let mut builder = Scope::builder();

    // We map errors from the scope builder to Semantic-or-Formula diagnostics;
    // they should not occur because names were validated in step 5, but we keep a
    // precise diagnostic just in case.
    let scope_err =
        |path: String, msg: String| LoadDiagnostic::new(source_name, LoadStep::Semantic, path, msg);

    // 1. shared inputs.
    for name in SHARED_INPUTS {
        builder.slot(name).map_err(|e| {
            scope_err(
                "".to_owned(),
                format!("internal scope error for '{name}': {e}"),
            )
        })?;
    }

    // 2. per-setting slots, recording each SettingSlots.
    let mut settings: IndexMap<String, SettingSlots> = IndexMap::new();
    for (name, decl) in &artifact.settings {
        let base_path = format!("settings.{name}");
        let slots =
            match decl {
                SettingDecl::Color { .. } => {
                    let mut chan = |suffix: &str| {
                        builder
                            .slot(&format!("settings.{name}{suffix}"))
                            .map_err(|e| {
                                scope_err(base_path.clone(), format!("internal scope error: {e}"))
                            })
                    };
                    SettingSlots::Color {
                        r: chan("_r")?,
                        g: chan("_g")?,
                        b: chan("_b")?,
                        a: chan("_a")?,
                    }
                }
                _ => SettingSlots::Scalar(builder.slot(&format!("settings.{name}")).map_err(
                    |e| scope_err(base_path.clone(), format!("internal scope error: {e}")),
                )?),
            };
        settings.insert(name.clone(), slots);
    }

    // 3. vars, in declaration order.
    let mut vars: IndexMap<String, SlotId> = IndexMap::new();
    for name in artifact.vars.keys() {
        let slot = builder
            .slot(name)
            .map_err(|e| scope_err(format!("vars.{name}"), format!("internal scope error: {e}")))?;
        vars.insert(name.clone(), slot);
    }

    // 4. stage extras.
    for name in STAGE_EXTRAS {
        builder.slot(name).map_err(|e| {
            scope_err(
                "".to_owned(),
                format!("internal scope error for '{name}': {e}"),
            )
        })?;
    }

    let scope = builder.build();

    // Resolve the named shared-input/stage-extra slots (always present).
    let id_of = |n: &str| scope.slot_id(n).expect("slot present in master scope");
    let layout = SlotLayout {
        t: id_of("t"),
        dt: id_of("dt"),
        energy: id_of("energy"),
        low: id_of("low"),
        mid: id_of("mid"),
        high: id_of("high"),
        beat: id_of("beat"),
        beat_count: id_of("beat_count"),
        aspect: id_of("aspect"),
        i: id_of("i"),
        n: id_of("n"),
        u: id_of("u"),
        x: id_of("x"),
        y: id_of("y"),
        settings,
        vars,
        slot_count: scope.slot_count(),
    };

    // A running op total enforced against MAX_OPS_PER_ARTIFACT.
    let mut total_ops: usize = 0;

    // Compile var init/frame programs (frame stage).
    let mut vars_init = Vec::with_capacity(artifact.vars.len());
    let mut vars_frame = Vec::with_capacity(artifact.vars.len());
    for (name, decl) in &artifact.vars {
        vars_init.push(compile_formula(
            source_name,
            &decl.init,
            &scope,
            Stage::Frame,
            &format!("vars.{name}.init"),
            &mut total_ops,
        )?);
        vars_frame.push(compile_formula(
            source_name,
            &decl.frame,
            &scope,
            Stage::Frame,
            &format!("vars.{name}.frame"),
            &mut total_ops,
        )?);
    }

    // Compile each layer.
    let mut layers = Vec::with_capacity(artifact.scene.len());
    for (li, layer) in artifact.scene.iter().enumerate() {
        layers.push(compile_layer(
            source_name,
            layer,
            li,
            &scope,
            &mut total_ops,
        )?);
    }

    // Compile feedback.decay (frame stage).
    let feedback_decay = match &artifact.feedback {
        Some(Feedback { decay }) => Some(compile_formula(
            source_name,
            decay,
            &scope,
            Stage::Frame,
            "feedback.decay",
            &mut total_ops,
        )?),
        None => None,
    };

    let programs = ArtifactPrograms {
        vars_init,
        vars_frame,
        layers,
        feedback_decay,
    };

    Ok((layout, programs))
}

/// Compiles one layer's formulas into a [`LayerPrograms`].
fn compile_layer(
    source_name: &str,
    layer: &Layer,
    layer_index: usize,
    scope: &Scope,
    total_ops: &mut usize,
) -> Result<LayerPrograms, LoadDiagnostic> {
    let base = format!("scene[{layer_index}]");
    match layer {
        Layer::Instanced {
            count,
            visible,
            element,
            ..
        } => {
            let count = compile_formula(
                source_name,
                count,
                scope,
                Stage::Frame,
                &format!("{base}.count"),
                total_ops,
            )?;
            let visible = compile_opt(
                source_name,
                visible.as_ref(),
                scope,
                Stage::Frame,
                &format!("{base}.visible"),
                total_ops,
            )?;
            let estage = Stage::ElementOrPoint;
            let ep = format!("{base}.element");
            let x = compile_formula(
                source_name,
                &element.x,
                scope,
                estage,
                &format!("{ep}.x"),
                total_ops,
            )?;
            let y = compile_formula(
                source_name,
                &element.y,
                scope,
                estage,
                &format!("{ep}.y"),
                total_ops,
            )?;
            let w = compile_formula(
                source_name,
                &element.w,
                scope,
                estage,
                &format!("{ep}.w"),
                total_ops,
            )?;
            let h = compile_formula(
                source_name,
                &element.h,
                scope,
                estage,
                &format!("{ep}.h"),
                total_ops,
            )?;
            let rot = compile_opt(
                source_name,
                element.rot.as_ref(),
                scope,
                estage,
                &format!("{ep}.rot"),
                total_ops,
            )?;
            let color = compile_color(
                source_name,
                &element.color,
                scope,
                estage,
                &format!("{ep}.color"),
                total_ops,
            )?;
            Ok(LayerPrograms::Instanced {
                count,
                visible,
                x,
                y,
                w,
                h,
                rot,
                color,
            })
        }
        Layer::Polyline {
            points,
            thickness,
            visible,
            point,
            closed,
            ..
        } => {
            let points = compile_formula(
                source_name,
                points,
                scope,
                Stage::Frame,
                &format!("{base}.points"),
                total_ops,
            )?;
            let thickness = compile_formula(
                source_name,
                thickness,
                scope,
                Stage::Frame,
                &format!("{base}.thickness"),
                total_ops,
            )?;
            let visible = compile_opt(
                source_name,
                visible.as_ref(),
                scope,
                Stage::Frame,
                &format!("{base}.visible"),
                total_ops,
            )?;
            let pstage = Stage::ElementOrPoint;
            let pp = format!("{base}.point");
            let x = compile_formula(
                source_name,
                &point.x,
                scope,
                pstage,
                &format!("{pp}.x"),
                total_ops,
            )?;
            let y = compile_formula(
                source_name,
                &point.y,
                scope,
                pstage,
                &format!("{pp}.y"),
                total_ops,
            )?;
            let color = compile_color(
                source_name,
                &point.color,
                scope,
                pstage,
                &format!("{pp}.color"),
                total_ops,
            )?;
            Ok(LayerPrograms::Polyline {
                points,
                thickness,
                visible,
                x,
                y,
                color,
                closed: *closed,
            })
        }
        Layer::Field {
            resolution,
            visible,
            cell,
            ..
        } => {
            let resolution = compile_formula(
                source_name,
                resolution,
                scope,
                Stage::Frame,
                &format!("{base}.resolution"),
                total_ops,
            )?;
            let visible = compile_opt(
                source_name,
                visible.as_ref(),
                scope,
                Stage::Frame,
                &format!("{base}.visible"),
                total_ops,
            )?;
            let color = compile_color(
                source_name,
                &cell.color,
                scope,
                Stage::Cell,
                &format!("{base}.cell.color"),
                total_ops,
            )?;
            Ok(LayerPrograms::Field {
                resolution,
                visible,
                color,
            })
        }
    }
}

/// Compiles a [`ColorSpec`] into a [`ColorPrograms`] at the given stage.
fn compile_color(
    source_name: &str,
    color: &ColorSpec,
    scope: &Scope,
    stage: Stage,
    base_path: &str,
    total_ops: &mut usize,
) -> Result<ColorPrograms, LoadDiagnostic> {
    let (model, c0, c1, c2, a, n0, n1, n2) = match color {
        ColorSpec::Rgba { r, g, b, a } => (ColorModel::Rgba, r, g, b, a, "r", "g", "b"),
        ColorSpec::Hsva { h, s, v, a } => (ColorModel::Hsva, h, s, v, a, "h", "s", "v"),
    };
    let c0 = compile_formula(
        source_name,
        c0,
        scope,
        stage,
        &format!("{base_path}.{n0}"),
        total_ops,
    )?;
    let c1 = compile_formula(
        source_name,
        c1,
        scope,
        stage,
        &format!("{base_path}.{n1}"),
        total_ops,
    )?;
    let c2 = compile_formula(
        source_name,
        c2,
        scope,
        stage,
        &format!("{base_path}.{n2}"),
        total_ops,
    )?;
    let a = compile_opt(
        source_name,
        a.as_ref(),
        scope,
        stage,
        &format!("{base_path}.a"),
        total_ops,
    )?;
    Ok(ColorPrograms {
        model,
        c0,
        c1,
        c2,
        a,
    })
}

/// Compiles an optional formula, propagating `None`.
fn compile_opt(
    source_name: &str,
    formula: Option<&Formula>,
    scope: &Scope,
    stage: Stage,
    json_path: &str,
    total_ops: &mut usize,
) -> Result<Option<CompiledFormula>, LoadDiagnostic> {
    match formula {
        Some(f) => Ok(Some(compile_formula(
            source_name,
            f,
            scope,
            stage,
            json_path,
            total_ops,
        )?)),
        None => Ok(None),
    }
}

/// Compiles one [`Formula`] at the given stage into a [`CompiledFormula`].
///
/// Constants are recorded directly (sanitized to finite). Expressions are
/// compiled against the master `scope`; then the source is re-lexed and rejected
/// if it uses any stage-extra identifier disallowed in this stage (see the
/// module/stage docs). The artifact-wide op cap is enforced on `total_ops`.
///
/// **Stage restriction approach** — rather than building per-stage scopes (which
/// would renumber slots and break the shared bank), we compile against the single
/// master scope, then re-lex the source and look for any disallowed stage-extra
/// identifier token. The lexer treats `i`/`n`/`u`/`x`/`y` as whole identifier
/// tokens (a dotted name like `settings.x` is one atomic token, never matched),
/// so a token-level check is exact and cheap.
fn compile_formula(
    source_name: &str,
    formula: &Formula,
    scope: &Scope,
    stage: Stage,
    json_path: &str,
    total_ops: &mut usize,
) -> Result<CompiledFormula, LoadDiagnostic> {
    let src = match formula.as_expr_or_const() {
        // Numeric constant: record it (sanitized), no program, no ops.
        Err(value) => {
            let finite = if value.is_finite() { value } else { 0.0 };
            return Ok(CompiledFormula::Const(finite));
        }
        Ok(src) => src,
    };

    // Stage restriction: reject any disallowed stage-extra identifier token.
    if let Some(bad) = first_disallowed_extra(src, stage) {
        return Err(LoadDiagnostic::new(
            source_name,
            LoadStep::Formula,
            json_path,
            format!(
                "identifier '{bad}' is not available in this stage ({})",
                stage_label(stage)
            ),
        ));
    }

    // Compile against the master scope.
    let program = compile(src, scope).map_err(|e| {
        LoadDiagnostic::new(
            source_name,
            LoadStep::Formula,
            json_path,
            // `CompileError`'s Display names the token + position in English.
            e.to_string(),
        )
    })?;

    // Artifact-wide op cap.
    *total_ops += program.op_count();
    if *total_ops > limits::MAX_OPS_PER_ARTIFACT {
        return Err(LoadDiagnostic::new(
            source_name,
            LoadStep::Formula,
            json_path,
            format!(
                "artifact exceeds the {} total-operation limit",
                limits::MAX_OPS_PER_ARTIFACT
            ),
        ));
    }

    Ok(CompiledFormula::Program(program))
}

/// English label for a stage, used in stage-restriction diagnostics.
fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Frame => "frame",
        Stage::ElementOrPoint => "element/point",
        Stage::Cell => "cell",
    }
}

/// Re-lexes `src` and returns the first stage-extra identifier token that is
/// disallowed in `stage`, or `None`. A lex error here is not reported (the
/// subsequent `compile` call will surface it with a proper diagnostic).
fn first_disallowed_extra(src: &str, stage: Stage) -> Option<&'static str> {
    let disallowed = stage.disallowed_extras();
    let tokens = lex(src).ok()?;
    for tok in &tokens {
        if let TokenKind::Ident(name) = &tok.kind {
            if let Some(d) = disallowed.iter().find(|d| **d == name.as_str()) {
                return Some(d);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Converts a JSON Pointer (`/scene/0/element/color/h`) into the contract's
/// dotted/bracketed JSON path form (`scene[0].element.color.h`). An empty pointer
/// (the document root) maps to `""`.
fn pointer_to_path(pointer: &str) -> String {
    let mut out = String::new();
    for seg in pointer.split('/').filter(|s| !s.is_empty()) {
        if let Ok(idx) = seg.parse::<usize>() {
            out.push('[');
            out.push_str(&idx.to_string());
            out.push(']');
        } else {
            if !out.is_empty() {
                out.push('.');
            }
            // Unescape JSON Pointer tokens (~1 → /, ~0 → ~).
            out.push_str(&seg.replace("~1", "/").replace("~0", "~"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
