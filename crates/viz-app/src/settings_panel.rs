//! The auto-generated, live-tunable settings panel.
//!
//! Given the active [`LoadedArtifact`]'s [`SettingDecl`](viz_contract::SettingDecl)
//! map, this module builds one egui control per setting, in **declaration order**
//! (see `docs/reference/artifact-contract.md` §4): `number` → [`egui::Slider`],
//! `toggle` → [`egui::Checkbox`],
//! `choice` → [`egui::ComboBox`] over its options (0-based index), `color` →
//! [`Ui::color_edit_button_srgba`](egui::Ui::color_edit_button_srgba) (with an
//! alpha channel when the declared default carried 8 hex digits).
//!
//! ## No per-frame allocation
//! The panel is **stateful**: a [`SettingsPanel`] caches, per setting, its label
//! `String`, the decl metadata it needs (range/step/options), and the *current
//! working value* the widget edits in place. That cache is (re)built only on an
//! artifact switch via [`SettingsPanel::rebind`]; rendering a frame mutates the
//! cached working values and pushes one tiny [`SettingChange`] per edit into a
//! caller-supplied, reused [`Vec`] — no heap traffic in the steady state.
//!
//! ## Where the values live
//! The panel's working values are the UI's source of truth between frames; the
//! [`ArtifactRuntime`](crate::runtime::ArtifactRuntime) is the evaluator's source
//! of truth. The host applies each emitted [`SettingChange`] to the runtime's
//! setting hooks *before* the next frame, so an edit is visible on the very next
//! frame (no deferred queue — the change takes effect immediately). The
//! "Reset to defaults" button restores both: it re-derives the working values from
//! the declarations and signals the host to reset the runtime.

use egui::Color32;

use viz_contract::{LoadedArtifact, SettingDecl};

use crate::runtime::SettingValue;

/// One setting's cached binding: enough to render its widget every frame without
/// touching the artifact declaration or allocating. Built once per artifact.
struct SettingBinding {
    /// The setting's name (the runtime key + persistence key). Owned so the panel
    /// outlives any borrow of the artifact.
    name: String,
    /// The pre-rendered widget label (cached; never rebuilt per frame).
    label: String,
    /// The control kind + the live working value the widget edits in place.
    control: Control,
}

/// The widget-specific state for one setting, holding the current working value.
enum Control {
    /// A numeric slider over `min..=max` with `step` drag granularity.
    Number {
        /// Current value (edited in place by the slider).
        value: f64,
        /// Inclusive minimum.
        min: f64,
        /// Inclusive maximum.
        max: f64,
        /// Optional drag step (`> 0`); `None` ⇒ continuous.
        step: Option<f64>,
    },
    /// A boolean checkbox.
    Toggle {
        /// Current state.
        value: bool,
    },
    /// A dropdown over `options`, holding the selected 0-based index.
    Choice {
        /// Selected 0-based index into `options`.
        index: usize,
        /// The selectable option labels (owned; built once).
        options: Vec<String>,
    },
    /// A color button. The working value is straight-alpha sRGB bytes so it round-
    /// trips losslessly through [`Color32`]; `has_alpha` mirrors the declared
    /// default's digit count (6 ⇒ opaque, no alpha editing).
    Color {
        /// Current straight-alpha sRGB color (egui's edit type).
        color: Color32,
        /// Whether the default carried an explicit alpha (8 hex digits).
        has_alpha: bool,
    },
}

/// One applied setting edit, emitted by [`SettingsPanel::ui`] for the host to push
/// into the runtime's setting hooks before the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SettingChange<'a> {
    /// The setting name (borrows the panel's cached key — no allocation).
    pub name: &'a str,
    /// The new value, already in the runtime's [`SettingValue`] representation.
    pub value: SettingValue,
}

/// The auto-generated settings panel for the active artifact.
///
/// Construct empty with [`SettingsPanel::new`] (or [`Default`]); call
/// [`SettingsPanel::rebind`] whenever the active artifact changes to rebuild the
/// cached controls in declaration order; call [`SettingsPanel::ui`] each frame
/// inside the overlay to draw the controls and collect edits.
#[derive(Default)]
pub struct SettingsPanel {
    /// Cached per-setting bindings, in artifact declaration order.
    bindings: Vec<SettingBinding>,
}

impl SettingsPanel {
    /// Creates an empty panel (no artifact bound yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the panel currently has any controls to draw.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Rebuilds the cached controls from `art`'s declarations, in declaration order
    /// (the panel rebinds on every artifact switch). Working values
    /// are seeded from `values` (the runtime's current values, e.g. restored from
    /// persistence) when present, falling back to the declared default otherwise.
    ///
    /// This is the only place that allocates (labels, option lists); it runs once
    /// per switch, never per frame.
    pub fn rebind<'v>(
        &mut self,
        art: &LoadedArtifact,
        mut values: impl FnMut(&str) -> Option<&'v SettingValue>,
    ) {
        self.bindings.clear();
        self.bindings.reserve(art.artifact.settings.len());
        for (name, decl) in &art.artifact.settings {
            let current = values(name).copied();
            let control = build_control(decl, current);
            self.bindings.push(SettingBinding {
                name: name.clone(),
                label: decl_label(decl).to_owned(),
                control,
            });
        }
    }

    /// Reseeds the working values from the declared defaults without rebuilding the
    /// control structure (used by the host's reset-to-defaults path so the widgets
    /// snap back). The set of controls is unchanged.
    pub fn reset_to_defaults(&mut self, art: &LoadedArtifact) {
        for binding in &mut self.bindings {
            if let Some(decl) = art.artifact.settings.get(&binding.name) {
                binding.control = build_control(decl, None);
            }
        }
    }

    /// Draws every control (declaration order) below the dropdown and appends one
    /// [`SettingChange`] per edited control into `out` (which the caller clears and
    /// reuses each frame — no allocation in the steady state). Returns `true` if the
    /// user clicked "Reset to defaults" this frame.
    ///
    /// Borrows of the emitted names live as long as `&self`, so the caller must drain
    /// `out` before the next `&mut self` call.
    pub fn ui<'a>(&'a mut self, ui: &mut egui::Ui, out: &mut Vec<SettingChange<'a>>) -> bool {
        if self.bindings.is_empty() {
            return false;
        }

        ui.separator();
        for binding in &mut self.bindings {
            if let Some(value) = binding.control.ui(ui, &binding.label) {
                out.push(SettingChange {
                    name: &binding.name,
                    value,
                });
            }
        }

        ui.separator();
        ui.button("Reset to defaults")
            .on_hover_text("Restore every setting to the artifact's declared default")
            .clicked()
    }
}

impl Control {
    /// Renders this control with `label`, mutating its working value in place.
    /// Returns the new [`SettingValue`] iff the user changed it this frame.
    fn ui(&mut self, ui: &mut egui::Ui, label: &str) -> Option<SettingValue> {
        match self {
            Control::Number {
                value,
                min,
                max,
                step,
            } => {
                let mut slider = egui::Slider::new(value, *min..=*max).text(label);
                if let Some(s) = step {
                    slider = slider.step_by(*s);
                }
                let response = ui.add(slider);
                response.changed().then_some(SettingValue::Scalar(*value))
            }
            Control::Toggle { value } => {
                let response = ui.checkbox(value, label);
                response
                    .changed()
                    .then_some(SettingValue::Scalar(if *value { 1.0 } else { 0.0 }))
            }
            Control::Choice { index, options } => {
                let mut changed = false;
                egui::ComboBox::from_label(label)
                    .selected_text(options.get(*index).map(String::as_str).unwrap_or(""))
                    .show_ui(ui, |ui| {
                        for (i, opt) in options.iter().enumerate() {
                            if ui.selectable_label(i == *index, opt).clicked() && i != *index {
                                *index = i;
                                changed = true;
                            }
                        }
                    });
                changed.then_some(SettingValue::Scalar(*index as f64))
            }
            Control::Color { color, has_alpha } => {
                let response = if *has_alpha {
                    ui.horizontal(|ui| {
                        let r = ui.color_edit_button_srgba(color);
                        ui.label(label);
                        r
                    })
                    .inner
                } else {
                    // No alpha channel: edit RGB only, forcing alpha opaque so the
                    // picker never exposes transparency for a 6-digit default.
                    color[3] = 255;
                    ui.horizontal(|ui| {
                        let mut rgb = [color.r(), color.g(), color.b()];
                        let r = ui.color_edit_button_srgb(&mut rgb);
                        *color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                        ui.label(label);
                        r
                    })
                    .inner
                };
                response.changed().then(|| color_to_value(*color))
            }
        }
    }
}

/// Builds a [`Control`] for `decl`, seeding its working value from `current` (the
/// runtime's current value) when present, else from the declared default.
fn build_control(decl: &SettingDecl, current: Option<SettingValue>) -> Control {
    match decl {
        SettingDecl::Number {
            min,
            max,
            step,
            default,
            ..
        } => {
            let value = match current {
                Some(SettingValue::Scalar(v)) => v.clamp(*min, *max),
                _ => *default,
            };
            Control::Number {
                value,
                min: *min,
                max: *max,
                step: *step,
            }
        }
        SettingDecl::Toggle { default, .. } => {
            let value = match current {
                Some(SettingValue::Scalar(v)) => v >= 0.5,
                _ => *default,
            };
            Control::Toggle { value }
        }
        SettingDecl::Choice {
            options, default, ..
        } => {
            let default_index = options.iter().position(|o| o == default).unwrap_or(0);
            let index = match current {
                Some(SettingValue::Scalar(v)) => {
                    let i = v as i64;
                    if (0..options.len() as i64).contains(&i) {
                        i as usize
                    } else {
                        default_index
                    }
                }
                _ => default_index,
            };
            Control::Choice {
                index,
                options: options.clone(),
            }
        }
        SettingDecl::Color { default, .. } => {
            let has_alpha = hex_has_alpha(default);
            let color = match current {
                Some(SettingValue::Color { r, g, b, a }) => value_to_color(r, g, b, a, has_alpha),
                _ => {
                    let (r, g, b, a) = parse_hex_color(default);
                    value_to_color(r, g, b, a, has_alpha)
                }
            };
            Control::Color { color, has_alpha }
        }
    }
}

/// Extracts the panel label from any [`SettingDecl`] variant.
fn decl_label(decl: &SettingDecl) -> &str {
    match decl {
        SettingDecl::Number { label, .. }
        | SettingDecl::Toggle { label, .. }
        | SettingDecl::Choice { label, .. }
        | SettingDecl::Color { label, .. } => label,
    }
}

/// Converts the runtime's straight-alpha `0..1` color channels into an egui
/// [`Color32`] (straight-alpha sRGB bytes). When `has_alpha` is false the color is
/// forced opaque so a 6-digit default never shows transparency.
fn value_to_color(r: f64, g: f64, b: f64, a: f64, has_alpha: bool) -> Color32 {
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let alpha = if has_alpha { byte(a) } else { 255 };
    Color32::from_rgba_unmultiplied(byte(r), byte(g), byte(b), alpha)
}

/// Converts an egui [`Color32`] back into the runtime's straight-alpha `0..1`
/// [`SettingValue::Color`].
fn color_to_value(color: Color32) -> SettingValue {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    SettingValue::Color {
        r: r as f64 / 255.0,
        g: g as f64 / 255.0,
        b: b as f64 / 255.0,
        a: a as f64 / 255.0,
    }
}

/// Whether a `#RRGGBB`/`#RRGGBBAA` default carries an explicit alpha (8 hex digits).
fn hex_has_alpha(s: &str) -> bool {
    s.strip_prefix('#').unwrap_or(s).len() == 8
}

/// Parses a `#RRGGBB`/`#RRGGBBAA` hex color into straight-alpha `0..1` channels
/// (mirrors the runtime's parser; the loader already validated the format, so a
/// malformed value falls back to opaque white rather than panicking).
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

// Debug impl for Control so test panics print useful diagnostics without deriving
// Debug on the whole (Color32-containing) enum in non-test builds.
#[cfg(test)]
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Control::Number {
                value,
                min,
                max,
                step,
            } => f
                .debug_struct("Number")
                .field("value", value)
                .field("min", min)
                .field("max", max)
                .field("step", step)
                .finish(),
            Control::Toggle { value } => f.debug_struct("Toggle").field("value", value).finish(),
            Control::Choice { index, options } => f
                .debug_struct("Choice")
                .field("index", index)
                .field("options", options)
                .finish(),
            Control::Color { color, has_alpha } => f
                .debug_struct("Color")
                .field("color", &color.to_array())
                .field("has_alpha", has_alpha)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viz_contract::load_artifact;

    /// An artifact exercising all four setting kinds, in a deliberate declaration
    /// order so we can assert the panel preserves it.
    const ALL_KINDS: &str = r##"{
      "contract": "1.0",
      "meta": { "id": "all-kinds", "name": "All Kinds" },
      "settings": {
        "speed":  { "type": "number", "label": "Speed", "min": 0, "max": 10, "step": 0.5, "default": 2 },
        "glow":   { "type": "toggle", "label": "Glow", "default": true },
        "style":  { "type": "choice", "label": "Style", "options": ["a", "b", "c"], "default": "b" },
        "tint":   { "type": "color", "label": "Tint", "default": "#ff8000" },
        "fade":   { "type": "color", "label": "Fade", "default": "#11223344" }
      },
      "scene": [
        {
          "type": "instanced", "shape": "rect", "count": "settings.speed",
          "element": {
            "x": "0", "y": "0", "w": "0.1", "h": "0.1",
            "color": { "model": "rgba", "r": "settings.tint_r", "g": "settings.tint_g", "b": "settings.tint_b" }
          }
        }
      ]
    }"##;

    fn load(src: &str) -> LoadedArtifact {
        load_artifact("test", src.as_bytes()).expect("test artifact loads")
    }

    #[test]
    fn rebind_preserves_declaration_order() {
        let art = load(ALL_KINDS);
        let mut panel = SettingsPanel::new();
        panel.rebind(&art, |_| None);
        let names: Vec<&str> = panel.bindings.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["speed", "glow", "style", "tint", "fade"]);
    }

    #[test]
    fn rebind_seeds_defaults_when_no_current_values() {
        let art = load(ALL_KINDS);
        let mut panel = SettingsPanel::new();
        panel.rebind(&art, |_| None);

        match &panel.bindings[0].control {
            Control::Number {
                value,
                min,
                max,
                step,
            } => {
                assert_eq!(*value, 2.0);
                assert_eq!(*min, 0.0);
                assert_eq!(*max, 10.0);
                assert_eq!(*step, Some(0.5));
            }
            other => panic!("expected number, got {other:?}"),
        }
        match &panel.bindings[1].control {
            Control::Toggle { value } => assert!(*value),
            other => panic!("expected toggle, got {other:?}"),
        }
        match &panel.bindings[2].control {
            Control::Choice { index, options } => {
                assert_eq!(*index, 1); // "b" is index 1
                assert_eq!(options, &["a", "b", "c"]);
            }
            other => panic!("expected choice, got {other:?}"),
        }
        match &panel.bindings[3].control {
            // #ff8000 → opaque (6 digits ⇒ no alpha editing).
            Control::Color { color, has_alpha } => {
                assert!(!has_alpha);
                assert_eq!(color.r(), 0xff);
                assert_eq!(color.a(), 0xff);
            }
            other => panic!("expected color, got {other:?}"),
        }
        match &panel.bindings[4].control {
            // #11223344 → 8 digits ⇒ alpha editing on.
            Control::Color { has_alpha, .. } => assert!(*has_alpha),
            other => panic!("expected color, got {other:?}"),
        }
    }

    #[test]
    fn rebind_seeds_from_supplied_values_and_clamps() {
        let art = load(ALL_KINDS);
        let mut panel = SettingsPanel::new();
        // Supply an out-of-range number, a flipped toggle, an out-of-range choice.
        let speed = SettingValue::Scalar(99.0);
        let glow = SettingValue::Scalar(0.0);
        let style = SettingValue::Scalar(7.0);
        panel.rebind(&art, |name| match name {
            "speed" => Some(&speed),
            "glow" => Some(&glow),
            "style" => Some(&style),
            _ => None,
        });

        match &panel.bindings[0].control {
            // Clamped into [0, 10].
            Control::Number { value, .. } => assert_eq!(*value, 10.0),
            other => panic!("expected number, got {other:?}"),
        }
        match &panel.bindings[1].control {
            Control::Toggle { value } => assert!(!*value),
            other => panic!("expected toggle, got {other:?}"),
        }
        match &panel.bindings[2].control {
            // Out-of-range index falls back to the default index (1 = "b").
            Control::Choice { index, .. } => assert_eq!(*index, 1),
            other => panic!("expected choice, got {other:?}"),
        }
    }

    #[test]
    fn color_round_trips_through_color32() {
        // The straight-alpha sRGB byte path is lossless for these values.
        let v = color_to_value(value_to_color(1.0, 0.5, 0.0, 1.0, true));
        match v {
            SettingValue::Color { r, g, b, a } => {
                assert_eq!((r * 255.0).round() as u8, 255);
                assert_eq!((g * 255.0).round() as u8, 128);
                assert_eq!((b * 255.0).round() as u8, 0);
                assert_eq!((a * 255.0).round() as u8, 255);
            }
            other => panic!("expected color, got {other:?}"),
        }
    }

    #[test]
    fn hex_alpha_detection() {
        assert!(!hex_has_alpha("#ffffff"));
        assert!(hex_has_alpha("#ffffffff"));
        assert!(!hex_has_alpha("ffffff"));
    }

    #[test]
    fn reset_restores_defaults() {
        let art = load(ALL_KINDS);
        let mut panel = SettingsPanel::new();
        let speed = SettingValue::Scalar(5.0);
        panel.rebind(&art, |name| (name == "speed").then_some(&speed));
        match &panel.bindings[0].control {
            Control::Number { value, .. } => assert_eq!(*value, 5.0),
            other => panic!("expected number, got {other:?}"),
        }
        panel.reset_to_defaults(&art);
        match &panel.bindings[0].control {
            Control::Number { value, .. } => assert_eq!(*value, 2.0),
            other => panic!("expected number, got {other:?}"),
        }
    }

    #[test]
    fn empty_panel_for_artifact_without_settings() {
        const NO_SETTINGS: &str = r#"{
          "contract": "1.0",
          "meta": { "id": "no-settings", "name": "No Settings" },
          "scene": [
            { "type": "instanced", "shape": "rect", "count": "1",
              "element": { "x": "0", "y": "0", "w": "0.1", "h": "0.1",
                "color": { "model": "rgba", "r": "1", "g": "1", "b": "1" } } }
          ]
        }"#;
        let art = load(NO_SETTINGS);
        let mut panel = SettingsPanel::new();
        panel.rebind(&art, |_| None);
        assert!(panel.is_empty());
    }
}
