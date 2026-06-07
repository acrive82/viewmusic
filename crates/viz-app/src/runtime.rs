//! The artifact runtime: deterministic per-frame evaluation of a
//! [`viz_contract::LoadedArtifact`] into renderable geometry. See
//! `docs/reference/artifact-contract.md` (§2.3/§5/§6/§7) for the evaluation rules
//! this implements.
//!
//! An [`ArtifactRuntime`] owns one reusable [`viz_expr::Vm`] (sized to the
//! artifact's `slot_count`) plus staging buffers pre-allocated to the contract
//! maxima ([`viz_render::MAX_INSTANCES`] / [`viz_render::MAX_POINTS`] /
//! [`viz_render::MAX_FIELD_CELLS`]). After construction the hot path
//! ([`ArtifactRuntime::frame`]) performs **no heap allocation** beyond the tiny,
//! bounded `Vec<LayerDraw>` describing ≤16 layers — every per-element write goes
//! into the pre-allocated staging slices (the render loop must not allocate in
//! steady state).
//!
//! ## Determinism (see `docs/reference/artifact-contract.md` §9)
//! The output is a pure function of the supplied `(FeatureFrame, dt, aspect)`
//! sequence and the setting values. There is no wall-clock read and no
//! `HashMap`-order iteration anywhere on the eval path: vars and settings are
//! visited in [`IndexMap`](indexmap::IndexMap) declaration order, layers in
//! `scene` array order. The var read-semantics of contract §5 fall out of writing
//! each var's current-frame value into its slot **immediately** after evaluating
//! it, in declaration order (update-in-place): an earlier var reads its updated
//! value, a later var (or the var itself) reads last frame's value.
//!
//! ## Stage extras
//! Per element/point the runtime writes `i`, `n`, `u`; per field cell it writes
//! `x`, `y`, `i`, `n` (cell centers, row-major from bottom-left). These map onto
//! the named [`SlotLayout`] fields the loader published.

use indexmap::IndexMap;

use viz_contract::{
    Blend, ColorModel, ColorPrograms, CompiledFormula, Layer, LayerPrograms, LoadedArtifact,
    SettingSlots, Shape,
};
use viz_core::FeatureFrame;
use viz_expr::Vm;
use viz_render::{
    BlendMode, InstanceData, LayerDraw, PointData, SceneParams, ShapeKind, MAX_FIELD_CELLS,
    MAX_FIELD_RES, MAX_INSTANCES, MAX_POINTS,
};

/// Per-frame element-count clamp for an `instanced` layer (contract §6.1 / §12).
const INSTANCE_MIN: usize = 1;
/// Per-frame point-count clamp for a `polyline` layer (contract §6.2 / §12).
const POINT_MIN: usize = 2;
/// Per-frame resolution clamps for a `field` layer (contract §6.3 / §12).
const FIELD_RES_MIN: u32 = 2;
/// Thickness clamps for a `polyline` (contract §6.2): logical pixels 0.1..64.
const THICKNESS_MIN: f32 = 0.1;
const THICKNESS_MAX: f32 = 64.0;
/// Feedback decay clamp (contract §8).
const DECAY_MIN: f32 = 0.0;
const DECAY_MAX: f32 = 0.99;

/// The live value of one setting, applied to the VM's setting slots at
/// [`ArtifactRuntime::activate`] and whenever the UI mutates it.
///
/// Stored per the [`SettingSlots`] layout so the runtime never re-parses the
/// declaration on the hot path. Number/toggle/choice collapse to a single scalar
/// (number value, `0.0`/`1.0`, 0-based choice index — contract §4); color carries
/// four straight-alpha channels in `0..1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingValue {
    /// A `number`, `toggle`, or `choice` setting as one scalar.
    Scalar(f64),
    /// A `color` setting: red/green/blue/alpha in `0..1`.
    Color {
        /// Red channel, `0..1`.
        r: f64,
        /// Green channel, `0..1`.
        g: f64,
        /// Blue channel, `0..1`.
        b: f64,
        /// Alpha channel, `0..1`.
        a: f64,
    },
}

/// Per-layer draw metadata recorded each frame, used to build the borrowed
/// [`LayerDraw`] list in [`SceneOut`]. Holds the resolved length / shape / blend /
/// thickness so the borrow view is a cheap projection over the staging slices.
#[derive(Clone, Copy, Debug)]
enum LayerSlot {
    /// `instanced`: `len` instances starting at `offset` in the instance staging.
    Instanced {
        offset: usize,
        len: usize,
        shape: ShapeKind,
        blend: BlendMode,
    },
    /// `polyline`: `len` points starting at `offset` in the point staging.
    Polyline {
        offset: usize,
        len: usize,
        thickness_px: f32,
        closed: bool,
        blend: BlendMode,
    },
    /// `field`: a `resolution²` grid starting at `offset` in the cell staging.
    Field {
        offset: usize,
        resolution: u32,
        blend: BlendMode,
    },
    /// A layer skipped this frame (`visible < 0.5`, or paused) — emits nothing.
    Skipped,
}

/// One frame's renderable scene, borrowing the runtime's staging buffers.
///
/// The `layers` are in `scene` order (painter's algorithm); pass them straight to
/// [`viz_render::Renderer::render`] together with [`SceneOut::params`].
pub struct SceneOut<'a> {
    /// Scene-wide parameters (feedback decay).
    pub params: SceneParams,
    /// Per-layer draw lists, borrowing the staging slices. Bounded by ≤16 layers.
    pub layers: Vec<LayerDraw<'a>>,
}

/// The deterministic per-frame evaluator for one [`LoadedArtifact`].
///
/// Build with [`ArtifactRuntime::new`], call [`ArtifactRuntime::activate`] once
/// (runs `vars.init`), then [`ArtifactRuntime::frame`] per rendered frame.
pub struct ArtifactRuntime {
    /// The compiled artifact (programs, slot layout, shape metadata).
    art: LoadedArtifact,
    /// One reusable VM bank sized to `layout.slot_count`.
    vm: Vm,
    /// Live setting values in `layout.settings` declaration order.
    settings: IndexMap<String, SettingValue>,

    /// Pre-allocated instance staging (capacity [`MAX_INSTANCES`]).
    instances: Vec<InstanceData>,
    /// Pre-allocated point staging (capacity [`MAX_POINTS`]).
    points: Vec<PointData>,
    /// Pre-allocated field-cell staging (capacity [`MAX_FIELD_CELLS`]).
    cells: Vec<[f32; 4]>,

    /// Per-layer draw metadata for the current frame (capacity ≤16).
    layer_slots: Vec<LayerSlot>,
    /// Resolved feedback decay for the current frame (`None` ⇒ no trails).
    feedback_decay: Option<f32>,
}

impl ArtifactRuntime {
    /// Builds a runtime for `art`, pre-allocating every staging buffer to its
    /// contract maximum (no allocation occurs after this on the hot path).
    /// Setting values are initialized from the declared defaults.
    pub fn new(art: LoadedArtifact) -> Self {
        let vm = Vm::new(art.layout.slot_count, art.seed);
        let settings = default_setting_values(&art);

        let mut instances = Vec::new();
        instances.reserve_exact(MAX_INSTANCES);
        let mut points = Vec::new();
        points.reserve_exact(MAX_POINTS);
        let mut cells = Vec::new();
        cells.reserve_exact(MAX_FIELD_CELLS);
        let mut layer_slots = Vec::new();
        layer_slots.reserve_exact(art.programs.layers.len());

        Self {
            art,
            vm,
            settings,
            instances,
            points,
            cells,
            layer_slots,
            feedback_decay: None,
        }
    }

    /// The artifact's id, for logging / persistence keys.
    pub fn id(&self) -> &viz_core::ArtifactId {
        &self.art.id
    }

    /// Read access to the live setting values (for panel binding + persistence).
    pub fn settings(&self) -> &IndexMap<String, SettingValue> {
        &self.settings
    }

    /// Sets one setting by name to `value` (a live UI edit). Returns `false`
    /// if the name is unknown or the value kind mismatches the setting's kind
    /// (scalar vs color). The new value is written into the VM slot(s) before the
    /// next [`ArtifactRuntime::frame`], so the edit is visible on that frame — there
    /// is no deferred queue, so an edit takes effect on the very next frame.
    pub fn set_setting(&mut self, name: &str, value: SettingValue) -> bool {
        match self.settings.get_mut(name) {
            Some(slot @ SettingValue::Scalar(_)) if matches!(value, SettingValue::Scalar(_)) => {
                *slot = value;
                true
            }
            Some(slot @ SettingValue::Color { .. })
                if matches!(value, SettingValue::Color { .. }) =>
            {
                *slot = value;
                true
            }
            _ => false,
        }
    }

    /// Sets a scalar setting (`number`/`toggle`/`choice`) by name. Returns `false`
    /// if the name is unknown or names a color setting. Convenience over
    /// [`ArtifactRuntime::set_setting`].
    pub fn set_setting_scalar(&mut self, name: &str, value: f64) -> bool {
        self.set_setting(name, SettingValue::Scalar(value))
    }

    /// Sets a `color` setting by name (channels in `0..1`). Returns `false` if the
    /// name is unknown or names a scalar setting. Convenience over
    /// [`ArtifactRuntime::set_setting`].
    pub fn set_setting_color(&mut self, name: &str, r: f64, g: f64, b: f64, a: f64) -> bool {
        self.set_setting(name, SettingValue::Color { r, g, b, a })
    }

    /// Restores every setting to its declared default (the reset action) and
    /// re-runs [`ArtifactRuntime::activate`] so var state restarts from `init`.
    pub fn reset(&mut self) {
        self.settings = default_setting_values(&self.art);
        self.activate();
    }

    /// Writes the current setting values into their VM slots and runs every
    /// `vars.init` program in declaration order, seeding each var's slot.
    ///
    /// Call once after [`ArtifactRuntime::new`] (and on settings reset). Does not
    /// depend on a feature frame: `init` formulas read settings/constants only.
    pub fn activate(&mut self) {
        self.write_settings();
        // Clear any stale shared/extra slots so `init` sees a clean, zeroed frame.
        self.vm.set_frame(&FeatureFrame::default());
        let layout = &self.art.layout;
        for (idx, slot) in layout.vars.values().enumerate() {
            let v = self.art.programs.vars_init[idx].value(&mut self.vm);
            self.vm.set_slot(*slot, v);
        }
    }

    /// Evaluates one frame and returns the renderable [`SceneOut`].
    ///
    /// `feature` supplies the audio inputs and `band()`/`wave()`; `dt` is the
    /// audio-clock delta (seconds); `aspect` is render width/height; `detail_factor`
    /// (0..1, from [`viz_render::DegradeController`]) scales element/point/cell
    /// counts for graceful degradation under load — counts never drop below their
    /// contract minimum.
    pub fn frame(
        &mut self,
        feature: &FeatureFrame,
        dt: f64,
        aspect: f32,
        detail_factor: f32,
    ) -> SceneOut<'_> {
        // 1) Shared input slots + the feature frame (powers band()/wave()).
        let layout = &self.art.layout;
        self.vm.set_slot(layout.t, feature.t);
        self.vm.set_slot(layout.dt, dt);
        self.vm.set_slot(layout.energy, feature.energy as f64);
        self.vm.set_slot(layout.low, feature.low as f64);
        self.vm.set_slot(layout.mid, feature.mid as f64);
        self.vm.set_slot(layout.high, feature.high as f64);
        self.vm.set_slot(layout.beat, feature.beat as f64);
        self.vm
            .set_slot(layout.beat_count, feature.beat_count as f64);
        self.vm.set_slot(layout.aspect, aspect as f64);
        self.vm.set_frame(feature);

        // Settings can change between frames; re-apply them each frame.
        self.write_settings();

        // 2) Vars, IN DECLARATION ORDER, update-in-place (contract §5 semantics).
        for (idx, slot) in self.art.layout.vars.values().enumerate() {
            let v = self.art.programs.vars_frame[idx].value(&mut self.vm);
            self.vm.set_slot(*slot, v);
        }

        // 3) Per-layer evaluation into the pre-allocated staging buffers.
        self.instances.clear();
        self.points.clear();
        self.cells.clear();
        self.layer_slots.clear();

        let detail = detail_factor.clamp(0.0, 1.0) as f64;

        // Borrow-split: the program/scene metadata is read-only, the VM + staging
        // are written. We index by position to avoid holding an iterator borrow on
        // `self.art` while mutating `self.vm`/staging.
        let n_layers = self.art.programs.layers.len();
        for li in 0..n_layers {
            let slot = self.eval_layer(li, detail);
            self.layer_slots.push(slot);
        }

        // 4) Feedback decay (frame stage).
        self.feedback_decay = match &self.art.programs.feedback_decay {
            Some(p) => Some((p.value(&mut self.vm) as f32).clamp(DECAY_MIN, DECAY_MAX)),
            None => None,
        };

        self.build_scene_out()
    }

    /// Renders this frame's idle/clear scene: no layers, no trails. Used when the
    /// degrade ladder has paused element evaluation under load — the renderer still
    /// clears the surface so the window is never black/garbage.
    pub fn idle_scene(&mut self) -> SceneOut<'_> {
        self.instances.clear();
        self.points.clear();
        self.cells.clear();
        self.layer_slots.clear();
        self.feedback_decay = None;
        SceneOut {
            params: SceneParams {
                feedback_decay: None,
            },
            layers: Vec::new(),
        }
    }

    /// Whether this artifact requests feedback/trails at all (so the host can let
    /// the degrade controller shed it first when shedding load).
    #[allow(dead_code)]
    pub fn has_feedback(&self) -> bool {
        self.art.programs.feedback_decay.is_some()
    }

    // ---- internals -------------------------------------------------------

    /// Writes each live setting value into its VM slot(s).
    fn write_settings(&mut self) {
        for (name, value) in &self.settings {
            // The slot layout and the live values share declaration order + keys.
            let Some(slots) = self.art.layout.settings.get(name) else {
                continue;
            };
            match (*slots, *value) {
                (SettingSlots::Scalar(s), SettingValue::Scalar(v)) => {
                    self.vm.set_slot(s, v);
                }
                (
                    SettingSlots::Color { r, g, b, a },
                    SettingValue::Color {
                        r: vr,
                        g: vg,
                        b: vb,
                        a: va,
                    },
                ) => {
                    self.vm.set_slot(r, vr);
                    self.vm.set_slot(g, vg);
                    self.vm.set_slot(b, vb);
                    self.vm.set_slot(a, va);
                }
                // Layout/value kind mismatch cannot happen (both built from the
                // same decls), but ignore defensively rather than panic.
                _ => {}
            }
        }
    }

    /// Evaluates one layer (`li`) into staging, returning its draw metadata.
    fn eval_layer(&mut self, li: usize, detail: f64) -> LayerSlot {
        // The named slot ids are `Copy`, so capture the ones the loop needs up
        // front — this releases the immutable borrow on `self.art.layout` so the
        // per-element loop can mutate `self.vm` and the staging buffers.
        let (s_i, s_n, s_u, s_x, s_y) = {
            let l = &self.art.layout;
            (l.i, l.n, l.u, l.x, l.y)
        };
        // `shape`/`blend`/`closed` live in the typed scene, not in LayerPrograms;
        // the loader builds both in the same order, so index `li` matches.
        let scene_layer = &self.art.artifact.scene[li];
        match &self.art.programs.layers[li] {
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
                if !visible_passes(visible, &mut self.vm) {
                    return LayerSlot::Skipped;
                }
                let raw = count.value(&mut self.vm);
                let n = scale_count(raw, INSTANCE_MIN, MAX_INSTANCES, detail);
                let offset = self.instances.len();
                let denom = (n.saturating_sub(1)).max(1) as f64;
                for i in 0..n {
                    self.vm.set_slot(s_i, i as f64);
                    self.vm.set_slot(s_n, n as f64);
                    self.vm.set_slot(s_u, i as f64 / denom);
                    let px = x.value(&mut self.vm) as f32;
                    let py = y.value(&mut self.vm) as f32;
                    let pw = w.value(&mut self.vm) as f32;
                    let ph = h.value(&mut self.vm) as f32;
                    let prot = rot
                        .as_ref()
                        .map(|p| p.value(&mut self.vm) as f32)
                        .unwrap_or(0.0);
                    let col = eval_color(color, &mut self.vm);
                    self.instances.push(InstanceData {
                        x: px,
                        y: py,
                        w: pw,
                        h: ph,
                        rot: prot,
                        color: col,
                    });
                }
                LayerSlot::Instanced {
                    offset,
                    len: n,
                    shape: shape_kind(scene_layer),
                    blend: blend_mode(scene_layer),
                }
            }
            LayerPrograms::Polyline {
                points,
                thickness,
                visible,
                x,
                y,
                color,
                closed,
            } => {
                if !visible_passes(visible, &mut self.vm) {
                    return LayerSlot::Skipped;
                }
                let raw = points.value(&mut self.vm);
                let n = scale_count(raw, POINT_MIN, MAX_POINTS, detail);
                let thickness_px =
                    (thickness.value(&mut self.vm) as f32).clamp(THICKNESS_MIN, THICKNESS_MAX);
                let offset = self.points.len();
                let denom = (n.saturating_sub(1)).max(1) as f64;
                for i in 0..n {
                    self.vm.set_slot(s_i, i as f64);
                    self.vm.set_slot(s_n, n as f64);
                    self.vm.set_slot(s_u, i as f64 / denom);
                    let px = x.value(&mut self.vm) as f32;
                    let py = y.value(&mut self.vm) as f32;
                    let col = eval_color(color, &mut self.vm);
                    self.points.push(PointData {
                        x: px,
                        y: py,
                        color: col,
                    });
                }
                LayerSlot::Polyline {
                    offset,
                    len: n,
                    thickness_px,
                    closed: *closed,
                    blend: blend_mode(scene_layer),
                }
            }
            LayerPrograms::Field {
                resolution,
                visible,
                color,
            } => {
                if !visible_passes(visible, &mut self.vm) {
                    return LayerSlot::Skipped;
                }
                let raw = resolution.value(&mut self.vm);
                let res = scale_resolution(raw, detail);
                let offset = self.cells.len();
                let total = (res * res) as usize;
                let n = total as f64;
                // Cells iterate row-major from bottom-left (contract §6.3): the
                // cell center maps to NDC −1..1 with y up. For res cells per axis,
                // cell (col,row) center = (-1 + (col+0.5)*2/res, -1 + (row+0.5)*2/res).
                let inv = 2.0 / res as f64;
                let mut idx = 0usize;
                for row in 0..res {
                    let cy = -1.0 + (row as f64 + 0.5) * inv;
                    for col in 0..res {
                        let cx = -1.0 + (col as f64 + 0.5) * inv;
                        self.vm.set_slot(s_x, cx);
                        self.vm.set_slot(s_y, cy);
                        self.vm.set_slot(s_i, idx as f64);
                        self.vm.set_slot(s_n, n);
                        let c = eval_color(color, &mut self.vm);
                        self.cells.push(c);
                        idx += 1;
                    }
                }
                LayerSlot::Field {
                    offset,
                    resolution: res,
                    blend: blend_mode(scene_layer),
                }
            }
        }
    }

    /// Builds the borrowed [`SceneOut`] over the just-filled staging buffers.
    fn build_scene_out(&self) -> SceneOut<'_> {
        let mut layers: Vec<LayerDraw> = Vec::with_capacity(self.layer_slots.len());
        for slot in &self.layer_slots {
            match *slot {
                LayerSlot::Skipped => {}
                LayerSlot::Instanced {
                    offset,
                    len,
                    shape,
                    blend,
                } => {
                    layers.push(LayerDraw::Instanced {
                        shape,
                        blend,
                        instances: &self.instances[offset..offset + len],
                    });
                }
                LayerSlot::Polyline {
                    offset,
                    len,
                    thickness_px,
                    closed,
                    blend,
                } => {
                    layers.push(LayerDraw::Polyline {
                        blend,
                        thickness_px,
                        closed,
                        points: &self.points[offset..offset + len],
                    });
                }
                LayerSlot::Field {
                    offset,
                    resolution,
                    blend,
                } => {
                    let total = (resolution * resolution) as usize;
                    layers.push(LayerDraw::Field {
                        blend,
                        resolution,
                        cells: &self.cells[offset..offset + total],
                    });
                }
            }
        }
        SceneOut {
            params: SceneParams {
                feedback_decay: self.feedback_decay,
            },
            layers,
        }
    }
}

/// Evaluates a [`ColorPrograms`] into a straight-alpha RGBA `[f32;4]`, clamped to
/// the documented ranges (contract §7). HSVA is converted to RGBA; alpha defaults
/// to 1 when omitted.
fn eval_color(color: &ColorPrograms, vm: &mut Vm) -> [f32; 4] {
    let c0 = color.c0.value(vm);
    let c1 = color.c1.value(vm);
    let c2 = color.c2.value(vm);
    let a = color
        .a
        .as_ref()
        .map(|p| p.value(vm))
        .unwrap_or(1.0)
        .clamp(0.0, 1.0) as f32;
    match color.model {
        ColorModel::Rgba => [
            c0.clamp(0.0, 1.0) as f32,
            c1.clamp(0.0, 1.0) as f32,
            c2.clamp(0.0, 1.0) as f32,
            a,
        ],
        ColorModel::Hsva => {
            // Hue wraps mod 360; saturation/value clamp 0..1.
            let h = c0.rem_euclid(360.0);
            let s = c1.clamp(0.0, 1.0);
            let v = c2.clamp(0.0, 1.0);
            let (r, g, b) = hsv_to_rgb(h, s, v);
            [r as f32, g as f32, b as f32, a]
        }
    }
}

/// HSV→RGB (h in degrees 0..360, s/v in 0..1) → r/g/b in 0..1.
fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    if s <= 0.0 {
        return (v, v, v);
    }
    let h = h / 60.0;
    let sector = h.floor();
    let f = h - sector;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (sector as i64).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// Evaluates an optional `visible` gate: absent ⇒ visible; present ⇒ `>= 0.5`.
fn visible_passes(visible: &Option<CompiledFormula>, vm: &mut Vm) -> bool {
    match visible {
        None => true,
        Some(p) => p.value(vm) >= 0.5,
    }
}

/// Floors a raw count, clamps it into `[min, max]`, then scales by `detail`
/// (degradation), never dropping below `min`.
fn scale_count(raw: f64, min: usize, max: usize, detail: f64) -> usize {
    let floored = raw.floor();
    let base = if floored.is_finite() {
        (floored as i64).clamp(min as i64, max as i64) as usize
    } else {
        min
    };
    let scaled = ((base as f64) * detail).floor() as i64;
    scaled.clamp(min as i64, max as i64) as usize
}

/// Floors a raw resolution, clamps 2..=128, then scales by `detail` (min 2).
fn scale_resolution(raw: f64, detail: f64) -> u32 {
    let floored = raw.floor();
    let base = if floored.is_finite() {
        (floored as i64).clamp(FIELD_RES_MIN as i64, MAX_FIELD_RES as i64) as u32
    } else {
        FIELD_RES_MIN
    };
    let scaled = ((base as f64) * detail).floor() as i64;
    scaled.clamp(FIELD_RES_MIN as i64, MAX_FIELD_RES as i64) as u32
}

/// Maps the typed [`Shape`] of an `instanced` layer to a [`ShapeKind`].
fn shape_kind(layer: &Layer) -> ShapeKind {
    match layer {
        Layer::Instanced { shape, .. } => match shape {
            Shape::Rect => ShapeKind::Rect,
            Shape::Circle => ShapeKind::Circle,
            Shape::Triangle => ShapeKind::Triangle,
        },
        // Non-instanced layers never call this; default to Rect defensively.
        _ => ShapeKind::Rect,
    }
}

/// Maps the typed [`Blend`] of a layer to a [`BlendMode`].
fn blend_mode(layer: &Layer) -> BlendMode {
    let blend = match layer {
        Layer::Instanced { blend, .. } => blend,
        Layer::Polyline { blend, .. } => blend,
        Layer::Field { blend, .. } => blend,
    };
    match blend {
        Blend::Alpha => BlendMode::Alpha,
        Blend::Add => BlendMode::Add,
    }
}

/// Builds the default [`SettingValue`] map from an artifact's declarations, in
/// declaration order (see `docs/reference/artifact-contract.md` §4).
fn default_setting_values(art: &LoadedArtifact) -> IndexMap<String, SettingValue> {
    let mut out = IndexMap::with_capacity(art.artifact.settings.len());
    for (name, decl) in &art.artifact.settings {
        out.insert(name.clone(), default_setting_value(decl));
    }
    out
}

/// The declared-default [`SettingValue`] for a single [`SettingDecl`] (contract §4).
/// Exposed so persistence can fall back to the default for a missing/invalid stored
/// value without duplicating the kind→value mapping.
pub fn default_setting_value(decl: &viz_contract::SettingDecl) -> SettingValue {
    use viz_contract::SettingDecl;
    match decl {
        SettingDecl::Number { default, .. } => SettingValue::Scalar(*default),
        SettingDecl::Toggle { default, .. } => {
            SettingValue::Scalar(if *default { 1.0 } else { 0.0 })
        }
        SettingDecl::Choice {
            options, default, ..
        } => {
            let idx = options.iter().position(|o| o == default).unwrap_or(0);
            SettingValue::Scalar(idx as f64)
        }
        SettingDecl::Color { default, .. } => {
            let (r, g, b, a) = parse_hex_color(default);
            SettingValue::Color { r, g, b, a }
        }
    }
}

/// Parses a `#RRGGBB` / `#RRGGBBAA` hex color into straight-alpha `0..1` channels.
/// The loader has already validated the format; a malformed value falls back to
/// opaque white so a default never panics.
fn parse_hex_color(s: &str) -> (f64, f64, f64, f64) {
    let body = s.strip_prefix('#').unwrap_or(s);
    let byte = |i: usize| -> f64 {
        u8::from_str_radix(&body[i * 2..i * 2 + 2], 16).unwrap_or(255) as f64 / 255.0
    };
    match body.len() {
        6 => (byte(0), byte(1), byte(2), 1.0),
        8 => (byte(0), byte(1), byte(2), byte(3)),
        _ => (1.0, 1.0, 1.0, 1.0),
    }
}
