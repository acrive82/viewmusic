//! The composite visualizer render callback that wires audio → runtime →
//! GPU renderer → egui overlay into the app shell's [`RenderCallback`] seam.
//!
//! Per presented frame this:
//! 1. polls the [`AudioHandle`] for the latest [`FeatureFrame`] and the live
//!    permission state (read every frame, never a one-shot snapshot);
//! 2. computes the audio-clock delta `dt` (clamped 0..0.1; first frame 0 — `dt` is
//!    the delta of the audio clock, never wall clock, so motion stays tied to the
//!    sample stream rather than the display);
//! 3. selects the [`AppView`] from the live state, in priority order: no-artifacts /
//!    capture-failed / permission-guidance {waiting, denied} / active;
//! 4. when `Active`, drives the [`DegradeController`] around an
//!    [`ArtifactRuntime::frame`] → [`viz_render::Renderer::render`] (under load it
//!    sheds feedback first, then detail, then pauses, to hold the frame budget);
//! 5. paints the egui overlay on top (`LoadOp::Load`): the normal panel, the
//!    "No artifacts available" message, the generic capture-failure message, or
//!    the persistent permission guidance (with the Open-Settings button).
//!
//! Silence is NOT special-cased beyond *not* pausing: zeroed features feed the
//! artifact and it settles naturally — the permission machine keeps the state
//! `Granted` through plain silence (sticky), so silence never shows guidance.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::event::WindowEvent;
use winit::window::Window;

use viz_audio::{AudioHandle, CaptureState, PermissionState};
use viz_contract::{ArtifactLibrary, LoadedArtifact};
use viz_core::{ArtifactId, FeatureFrame};
use viz_render::{DegradeController, Renderer};

use crate::app_state::{
    select_view, AppView, GuidanceVariant, AUDIO_PERMISSION_APPLIES, CAPTURE_START_FAILED_MESSAGE,
    NO_ARTIFACTS_MESSAGE, OPEN_SETTINGS_BUTTON, PERMISSION_ALREADY_ALLOWED_HINT,
    PERMISSION_SETTINGS_PATH, PERMISSION_TITLE, PERMISSION_WAITING_HINT, PERMISSION_WHY,
    SETTINGS_URL, WAITING_FOR_AUDIO_HINT, WAITING_FOR_AUDIO_TITLE,
};
use crate::overlay::{decide_visibility, Overlay};
use crate::persist::{should_save_window, WindowState, WINDOW_SAVE_DEBOUNCE};
use crate::runtime::{ArtifactRuntime, SettingValue};
use crate::settings_panel::{SettingChange, SettingsPanel};
use crate::window::{FrameContext, RenderCallback};

/// Maximum `dt` (seconds) handed to the artifact in any one frame. A long stall
/// (e.g. the app was backgrounded) must not teleport time-driven motion; the audio
/// clock delta is clamped to 100 ms.
const MAX_DT: f64 = 0.1;

/// One selectable artifact in the dropdown (id + display name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownEntry {
    /// The artifact id (used to drive a switch and to persist the selection).
    pub id: ArtifactId,
    /// The display name shown in the combo box (`meta.name`).
    pub name: String,
}

/// One setting edit the panel produced this frame, in owned form so it can leave the
/// egui closure and be applied to the runtime by [`VizApp`] after the pass (the
/// borrowed [`SettingChange`] cannot escape the panel borrow).
#[derive(Clone, Debug, PartialEq)]
pub struct SettingEdit {
    /// The edited setting's name.
    pub name: String,
    /// Its new value (runtime representation).
    pub value: SettingValue,
}

/// A user action the overlay closure requests this frame; applied by [`VizApp`]
/// *after* the egui pass (the closure cannot borrow `VizApp` mutably).
#[derive(Clone, Debug, Default)]
pub struct OverlayRequest {
    /// The id the user picked in the dropdown this frame (`None` ⇒ no change).
    pub select: Option<ArtifactId>,
    /// The new collapsed state the user toggled to via the chevron (`None` ⇒ no
    /// change).
    pub set_collapsed: Option<bool>,
    /// Setting edits made this frame. Drained + applied to the runtime
    /// by [`VizApp`] before the next frame.
    pub setting_edits: Vec<SettingEdit>,
    /// Whether the user clicked "Reset to defaults" this frame.
    pub reset_settings: bool,
    /// Whether the user clicked "Reload artifacts" this frame. Triggers
    /// a full re-scan of built-ins + the user folder.
    pub reload_artifacts: bool,
    /// Whether the user clicked "Open System Settings" on the permission guidance
    /// screen this frame. [`VizApp`] spawns `open(1)` after the egui pass.
    pub open_settings: bool,
}

impl OverlayRequest {
    /// True if there is nothing to apply.
    fn is_empty(&self) -> bool {
        self.select.is_none()
            && self.set_collapsed.is_none()
            && self.setting_edits.is_empty()
            && !self.reset_settings
            && !self.reload_artifacts
            && !self.open_settings
    }
}

/// Snapshot the egui UI builder reads each frame (shared via `Rc<RefCell<…>>`).
///
/// The composite callback updates this *before* invoking the overlay's egui pass,
/// so the panel content always reflects the current view without the closure
/// needing to own the audio/runtime state. The closure writes user input back into
/// [`OverlayModel::request`], which [`VizApp`] drains after the pass.
///
/// Not `Clone`/`Debug`: it owns the live [`SettingsPanel`] UI state, which must not
/// be duplicated. Only [`OverlayRequest`] (a small POD) is cloned out per frame.
#[derive(Default)]
pub struct OverlayModel {
    /// What the window is showing this frame.
    pub view: ViewSnapshot,
    /// The active artifact id (drives the combo box's current selection and the
    /// panel header label, via [`OverlayModel::entries`]).
    pub active_id: Option<ArtifactId>,
    /// Every successfully loaded artifact, in library order (the dropdown
    /// contents). Empty ⇒ the overlay shows the 'No artifacts available' state.
    pub entries: Vec<DropdownEntry>,
    /// Whether the top-right panel is collapsed to its chevron.
    pub collapsed: bool,
    /// Whether to draw the overlay at all this frame (auto-hide).
    pub show_overlay: bool,
    /// User actions requested this frame (drained + cleared by [`VizApp`]).
    pub request: OverlayRequest,
    /// The auto-generated settings panel for the active artifact. Lives
    /// in the model so the egui closure can mutate its working values; [`VizApp`]
    /// rebinds it on an artifact switch.
    pub settings: SettingsPanel,
    /// Perf-HUD display values (only populated with the `perf-hud` feature).
    #[cfg(feature = "perf-hud")]
    pub hud: crate::hud::HudSnapshot,
}

/// GUI-friendly projection of [`AppView`] (owns its strings).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ViewSnapshot {
    /// Default before the first frame: behave like the empty-library state.
    #[default]
    NoArtifacts,
    /// Non-permission capture failure (generic handling); show the
    /// capture-health guidance message.
    CaptureFailed {
        /// The capture-layer failure message.
        message: String,
    },
    /// The audio-capture permission is missing: show the persistent
    /// permission guidance screen with the given variant.
    PermissionGuidance {
        /// Whether to render the waiting hint and/or the Open-Settings button.
        variant: GuidanceVariant,
    },
    /// Platforms without an audio-capture permission (Windows): show the neutral,
    /// persistent "waiting for audio — play something" hint (no consent copy).
    WaitingForAudio,
    /// Active visualizer.
    Active,
}

impl From<&AppView> for ViewSnapshot {
    fn from(v: &AppView) -> Self {
        match v {
            AppView::NoArtifacts => ViewSnapshot::NoArtifacts,
            AppView::CaptureFailed { message } => ViewSnapshot::CaptureFailed {
                message: message.clone(),
            },
            AppView::PermissionGuidance { variant } => {
                ViewSnapshot::PermissionGuidance { variant: *variant }
            }
            AppView::WaitingForAudio => ViewSnapshot::WaitingForAudio,
            AppView::Active => ViewSnapshot::Active,
        }
    }
}

/// HUD-visible counters (read by the optional perf HUD).
#[derive(Clone, Copy, Debug, Default)]
pub struct HudStats {
    /// Wall-clock age of the latest feature frame at this render, in milliseconds.
    pub feature_age_ms: f64,
    /// Current degrade detail factor (1.0 = full).
    pub detail_factor: f32,
    /// Whether feedback/trails are currently shed.
    pub shed_feedback: bool,
    /// Whether element evaluation is paused.
    pub paused: bool,
}

/// A callback invoked when the active artifact changes, so the host can persist the
/// new selection. Receives the newly active artifact id.
type ArtifactChangeHook = Box<dyn FnMut(&ArtifactId)>;

/// A callback invoked when the overlay collapse state changes, so the host can
/// persist it.
type CollapseChangeHook = Box<dyn FnMut(bool)>;

/// A callback invoked (debounced) when the window moves/resizes, so the host can
/// persist its geometry.
type WindowChangeHook = Box<dyn FnMut(WindowState)>;

/// A callback invoked when a setting is edited or reset, so the host can persist the
/// new per-artifact setting values. Receives the active artifact (for
/// its id + declarations, so the host can encode each value per kind) and its full
/// current setting map (declaration order).
type SettingChangeHook = Box<dyn FnMut(&LoadedArtifact, &indexmap::IndexMap<String, SettingValue>)>;

/// A callback invoked when an artifact becomes active (startup + each switch) to fetch
/// the persisted, already-validated setting overrides to seed its runtime with.
/// Returns the validated value map (declaration order); an empty map ⇒ use
/// the declared defaults. Keeping JSON validation in the host keeps [`VizApp`] free of
/// persistence concerns.
type SettingRestoreHook =
    Box<dyn FnMut(&LoadedArtifact) -> indexmap::IndexMap<String, SettingValue>>;

/// A callback that rebuilds the whole artifact library from scratch — embedded
/// built-ins first, then a fresh scan of the user artifacts folder.
///
/// Owned by the host so [`VizApp`] stays free of `include_str!` built-ins and of the
/// platform folder resolution (`directories`). Registered via
/// [`VizApp::on_reload_artifacts`]; invoked by [`VizApp::reload`] when the user
/// clicks "Reload artifacts". Every rejection is logged inside the library with its
/// full [`viz_contract::LoadDiagnostic`].
type LibraryBuilder = Box<dyn FnMut() -> ArtifactLibrary>;

/// The composite render callback.
pub struct VizApp {
    /// egui overlay plumbing (kept; we drive its UI builder via `model`).
    overlay: Overlay,
    /// Shared model the overlay's UI closure reads + writes each frame.
    model: Rc<RefCell<OverlayModel>>,

    /// The full artifact library (built-ins + user files); source of the dropdown
    /// list and of artifacts to switch to.
    library: ArtifactLibrary,
    /// The currently active artifact id (`None` ⇒ empty library).
    active_id: Option<ArtifactId>,
    /// Whether the top-right panel is collapsed to its chevron (persisted).
    collapsed: bool,
    /// Wall-clock instant of the last pointer movement (drives the auto-hide
    /// timer). `None` ⇒ never moved yet (treated as "just moved" so the overlay is
    /// initially visible).
    last_pointer_move: Option<Instant>,
    /// Set by `on_window_event` when the cursor moves; consumed in `render` to stamp
    /// `last_pointer_move` against the render clock (keeps all timing on one clock).
    pointer_moved_since_render: bool,
    /// Optional hook to persist a new selection.
    on_artifact_change: Option<ArtifactChangeHook>,
    /// Optional hook to persist a new collapse state.
    on_collapse_change: Option<CollapseChangeHook>,
    /// Optional hook to persist window geometry on move/resize.
    on_window_change: Option<WindowChangeHook>,
    /// Optional hook to persist per-artifact setting values on edit/reset.
    on_setting_change: Option<SettingChangeHook>,
    /// Optional hook to fetch persisted setting overrides when an artifact becomes
    /// active (startup + each switch), to seed its runtime.
    on_setting_restore: Option<SettingRestoreHook>,
    /// Optional hook to rebuild the library (built-ins + user folder) from scratch on
    /// the "Reload artifacts" action.
    rebuild_library: Option<LibraryBuilder>,
    /// Instant of the last window-state write (debounce).
    last_window_save: Option<Instant>,

    /// The GPU scene renderer (built in `attach_gpu`, once the format is known).
    renderer: Option<Renderer>,
    /// The active artifact runtime (`None` ⇒ empty library / no-artifacts state).
    runtime: Option<ArtifactRuntime>,
    /// Adaptive degradation controller.
    degrade: DegradeController,

    /// Live audio capture handle (`None` if capture failed to start at all).
    audio: Option<AudioHandle>,
    /// Whether `AudioPipeline::start` failed outright (no handle). A *live* flag:
    /// every frame with no handle renders the generic capture-failure guidance —
    /// no one-shot snapshot, so the guidance reflects the current state each frame.
    start_failed: bool,

    /// Last audio-clock `t` seen (for the `dt` delta); `None` before first frame.
    prev_t: Option<f64>,
    /// Wall-clock instant the latest distinct feature frame was first observed.
    latest_frame_at: Option<Instant>,
    /// Last audio-clock `t` for which we recorded an arrival instant.
    latest_frame_t: f64,
    /// Whether we have logged entering the paused state (log once, so the log file
    /// is not spammed every frame while paused).
    logged_paused: bool,

    /// Reusable beat-drain scratch (allocation-free per frame after first use).
    beat_scratch: Vec<viz_core::BeatEvent>,

    /// Latest HUD stats (read by the perf HUD when enabled).
    hud: HudStats,

    /// Perf-HUD timing accumulator (only with the `perf-hud` feature).
    #[cfg(feature = "perf-hud")]
    perf_hud: crate::hud::PerfHud,
}

impl VizApp {
    /// Builds the composite callback from the loaded pieces.
    ///
    /// `library` holds every loaded + rejected artifact (the dropdown lists the
    /// loaded ones). `active_id` is the artifact to start on (already
    /// resolved against the library / persisted state); `None` ⇒ empty
    /// library → no-artifacts state. `overlay_collapsed` restores the persisted
    /// panel collapse state. `audio` is the live capture handle from
    /// [`viz_audio::AudioPipeline::start`]; `None` means it failed to start, which
    /// the render loop turns into a *live* generic capture-failure state every
    /// frame (re-evaluated each frame, never a one-shot startup snapshot).
    pub fn new(
        library: ArtifactLibrary,
        active_id: Option<ArtifactId>,
        overlay_collapsed: bool,
        audio: Option<AudioHandle>,
    ) -> Self {
        let start_failed = audio.is_none();
        // The dropdown list is static for VizApp's lifetime (the library does not
        // change after construction), so build it once here rather than per-frame
        // (no per-frame allocation in the hot render path).
        let entries: Vec<DropdownEntry> = library
            .loaded()
            .map(|(id, name)| DropdownEntry {
                id: id.clone(),
                name: name.to_owned(),
            })
            .collect();

        let model = Rc::new(RefCell::new(OverlayModel {
            entries,
            ..OverlayModel::default()
        }));
        let mut overlay = Overlay::new();
        let ui_model = model.clone();
        overlay.set_ui(move |ctx| render_overlay(ctx, &mut ui_model.borrow_mut()));

        // Build the initial runtime for the active id (compile already happened at
        // library load; switching = constructing runtime state + activate()).
        let runtime = active_id
            .as_ref()
            .and_then(|id| library.get(id))
            .map(|loaded| ArtifactRuntime::new(loaded.clone()));

        // Bind the settings panel to the initial artifact, seeding its working values
        // from the runtime's current setting values. The runtime's
        // values already reflect any persisted overrides applied by the host before
        // building VizApp via `set_setting`.
        if let (Some(id), Some(rt)) = (active_id.as_ref(), runtime.as_ref()) {
            if let Some(loaded) = library.get(id) {
                let values = rt.settings();
                model
                    .borrow_mut()
                    .settings
                    .rebind(loaded, |name| values.get(name));
            }
        }

        Self {
            overlay,
            model,
            library,
            active_id,
            collapsed: overlay_collapsed,
            last_pointer_move: None,
            pointer_moved_since_render: false,
            on_artifact_change: None,
            on_collapse_change: None,
            on_window_change: None,
            on_setting_change: None,
            on_setting_restore: None,
            rebuild_library: None,
            last_window_save: None,
            renderer: None,
            runtime,
            degrade: DegradeController::new(),
            audio,
            start_failed,
            prev_t: None,
            latest_frame_at: None,
            latest_frame_t: f64::NEG_INFINITY,
            logged_paused: false,
            beat_scratch: Vec::new(),
            hud: HudStats::default(),
            #[cfg(feature = "perf-hud")]
            perf_hud: crate::hud::PerfHud::new(),
        }
    }

    /// The currently active artifact id, or `None` in the no-artifacts state. Useful
    /// for tests and host introspection after a reload.
    pub fn active_id(&self) -> Option<&ArtifactId> {
        self.active_id.as_ref()
    }

    /// The artifact library backing the dropdown (built-ins + scanned user files).
    /// Reflects the most recent [`Self::reload`].
    pub fn library(&self) -> &ArtifactLibrary {
        &self.library
    }

    /// The active runtime's live setting values in declaration order, or `None` when
    /// there is no active runtime. Lets a test confirm that a reload preserved the
    /// running artifact's settings.
    pub fn active_settings(&self) -> Option<&indexmap::IndexMap<String, SettingValue>> {
        self.runtime.as_ref().map(|rt| rt.settings())
    }

    /// Registers a hook called whenever the active artifact changes (so the host can
    /// persist the new selection across restarts).
    #[must_use]
    pub fn on_artifact_change(mut self, f: impl FnMut(&ArtifactId) + 'static) -> Self {
        self.on_artifact_change = Some(Box::new(f));
        self
    }

    /// Registers a hook called whenever the overlay collapse state changes (so the
    /// host can persist whether the panel is collapsed across restarts).
    #[must_use]
    pub fn on_collapse_change(mut self, f: impl FnMut(bool) + 'static) -> Self {
        self.on_collapse_change = Some(Box::new(f));
        self
    }

    /// Registers a hook called (debounced) when the window moves/resizes, with the
    /// new geometry to persist.
    #[must_use]
    pub fn on_window_change(mut self, f: impl FnMut(WindowState) + 'static) -> Self {
        self.on_window_change = Some(Box::new(f));
        self
    }

    /// Registers a hook called whenever a setting is edited or reset, with the active
    /// artifact id and its full current setting map, so the host can persist the
    /// per-artifact overrides.
    #[must_use]
    pub fn on_setting_change(
        mut self,
        f: impl FnMut(&LoadedArtifact, &indexmap::IndexMap<String, SettingValue>) + 'static,
    ) -> Self {
        self.on_setting_change = Some(Box::new(f));
        self
    }

    /// Registers a hook that supplies the persisted, validated setting overrides for
    /// an artifact when it becomes active. Registering it also seeds the
    /// **startup** artifact's runtime immediately (the startup runtime was built in
    /// [`VizApp::new`] with declared defaults), then rebinds the panel — so a restored
    /// session shows its saved values on the first frame.
    #[must_use]
    pub fn on_setting_restore(
        mut self,
        f: impl FnMut(&LoadedArtifact) -> indexmap::IndexMap<String, SettingValue> + 'static,
    ) -> Self {
        self.on_setting_restore = Some(Box::new(f));
        self.seed_active_settings();
        self
    }

    /// Registers a hook that rebuilds the library (embedded built-ins + a fresh user
    /// folder scan) from scratch when the user clicks "Reload artifacts". Without it,
    /// the reload button is a no-op (the dropdown stays as built at startup). The host
    /// owns the built-ins and the folder path so [`VizApp`] stays platform-agnostic.
    #[must_use]
    pub fn on_reload_artifacts(mut self, f: impl FnMut() -> ArtifactLibrary + 'static) -> Self {
        self.rebuild_library = Some(Box::new(f));
        self
    }

    /// Re-scans built-ins + the user folder from scratch: rebuilds the library via
    /// [`Self::rebuild_library`], rebuilds the dropdown list, and reconciles the
    /// active selection.
    ///
    /// **Active-artifact preservation.** If the previously active id still names a
    /// loaded artifact after the rescan, the current [`ArtifactRuntime`] is *kept
    /// untouched* — its live setting values and var state survive, so a reload does
    /// not visibly disturb the running visualizer. Otherwise (the active artifact was
    /// removed or became invalid) we fall back to the first built-in / first loaded,
    /// building a fresh runtime for it and seeding its settings; if nothing loaded,
    /// the runtime is dropped and the window shows the no-artifacts state. **Audio is
    /// never touched** (the [`AudioHandle`] is independent of the library).
    ///
    /// Every rejected file was already logged once with its full
    /// [`viz_contract::LoadDiagnostic`] inside the library load path (artifact
    /// diagnostics go to the log file only, never to the screen). A no-op (logged)
    /// when no [`Self::on_reload_artifacts`] hook is registered.
    pub fn reload(&mut self) {
        let Some(build) = self.rebuild_library.as_mut() else {
            tracing::warn!(target: "viz_app::viz", "reload requested but no library builder registered");
            return;
        };
        let new_library = build();
        tracing::info!(
            target: "viz_app::viz",
            loaded = new_library.loaded_count(),
            "reloaded artifact library"
        );
        self.library = new_library;

        // Rebuild the dropdown list from the new library (discrete user action — not
        // the steady-state hot path, so this allocation is acceptable).
        let entries: Vec<DropdownEntry> = self
            .library
            .loaded()
            .map(|(id, name)| DropdownEntry {
                id: id.clone(),
                name: name.to_owned(),
            })
            .collect();
        self.model.borrow_mut().entries = entries;

        // Decide the active artifact after the rescan.
        let active_still_loaded = self
            .active_id
            .as_ref()
            .is_some_and(|id| self.library.contains(id));

        if active_still_loaded {
            // The active artifact survived: keep the current runtime + setting values
            // exactly as they were (no rebuild, no disturbance). Nothing else to do.
            tracing::info!(
                target: "viz_app::viz",
                id = %self.active_id.as_ref().expect("active id present"),
                "active artifact preserved across reload"
            );
            return;
        }

        // The active artifact is gone (removed/invalidated): fall back to the first
        // loaded (built-in) artifact, or the no-artifacts state if none loaded.
        match self.library.first_loaded_id().cloned() {
            Some(fallback) => {
                if let Some(prev) = self.active_id.as_ref() {
                    tracing::info!(
                        target: "viz_app::viz",
                        removed = %prev,
                        fallback = %fallback,
                        "active artifact no longer present; falling back"
                    );
                }
                let loaded = self
                    .library
                    .get(&fallback)
                    .expect("first_loaded_id names a loaded artifact")
                    .clone();
                let mut runtime = ArtifactRuntime::new(loaded);
                if self.renderer.is_some() {
                    runtime.activate();
                }
                self.runtime = Some(runtime);
                self.active_id = Some(fallback.clone());
                // Seed the fallback's settings (persisted overrides) + rebind panel.
                self.seed_active_settings();
                // Persist the new selection so a restart reopens on it.
                if let Some(hook) = self.on_artifact_change.as_mut() {
                    hook(&fallback);
                }
            }
            None => {
                tracing::warn!(
                    target: "viz_app::viz",
                    "reload left zero loaded artifacts; entering no-artifacts state"
                );
                self.runtime = None;
                self.active_id = None;
            }
        }
    }

    /// Seeds the active runtime's setting values from [`Self::on_setting_restore`]
    /// (if registered), then rebinds the settings panel to reflect the runtime's
    /// current values. Always rebinds the panel (even without a restore hook), so it
    /// is the single rebind path for both startup and every artifact switch. A no-op
    /// when there is no active runtime / artifact.
    fn seed_active_settings(&mut self) {
        let Some(id) = self.active_id.clone() else {
            return;
        };
        let Some(loaded) = self.library.get(&id).cloned() else {
            return;
        };
        // Apply persisted overrides to the runtime (if a restore hook is registered).
        if let Some(hook) = self.on_setting_restore.as_mut() {
            let restored = hook(&loaded);
            if let Some(rt) = self.runtime.as_mut() {
                for (name, value) in &restored {
                    rt.set_setting(name, *value);
                }
            }
        }
        // Rebind the panel from the runtime's (now possibly restored) current values.
        if let Some(rt) = self.runtime.as_ref() {
            let values = rt.settings();
            self.model
                .borrow_mut()
                .settings
                .rebind(&loaded, |name| values.get(name));
        }
    }

    /// Reads the window's current geometry into a [`WindowState`] (physical pixels).
    /// `None` if the platform cannot report the outer position.
    fn current_window_state(window: &Window) -> Option<WindowState> {
        let pos = window.outer_position().ok()?;
        let size = window.inner_size();
        Some(WindowState {
            x: pos.x,
            y: pos.y,
            width: size.width,
            height: size.height,
        })
    }

    /// Switches the active artifact to `id`: builds a fresh
    /// [`ArtifactRuntime`] (the artifact is already compiled in the library) and, if
    /// the GPU is attached, runs `activate()`. Audio is untouched. A no-op when `id`
    /// is already active or not a loaded artifact. Returns `true` if a switch
    /// happened.
    fn switch_to(&mut self, id: &ArtifactId) -> bool {
        if self.active_id.as_ref() == Some(id) {
            return false;
        }
        let Some(loaded) = self.library.get(id) else {
            tracing::warn!(target: "viz_app::viz", requested = %id, "ignoring switch to unloaded artifact");
            return false;
        };
        let mut runtime = ArtifactRuntime::new(loaded.clone());
        // Only activate now if the GPU is up (otherwise attach_gpu activates). The
        // var init does not actually need the GPU, but matching attach order keeps
        // the activation contract identical to the startup path.
        if self.renderer.is_some() {
            runtime.activate();
        }
        self.runtime = Some(runtime);
        self.active_id = Some(id.clone());

        // Restore any persisted overrides for the new artifact, then rebind the
        // settings panel to it: rebuild its controls in the new declaration order,
        // seeded from the runtime's current values.
        self.seed_active_settings();

        tracing::info!(target: "viz_app::viz", id = %id, "switched active artifact");
        if let Some(hook) = self.on_artifact_change.as_mut() {
            hook(id);
        }
        true
    }

    /// Applies this frame's setting request to the active runtime: resets to declared
    /// defaults when `reset` is set, then applies each individual `edits` entry. A
    /// change writes straight into the runtime's setting slot (flushed before the next
    /// frame's var evaluation — naturally next-frame, no deferred queue). When anything
    /// changed, notifies the persistence hook once with the full current setting map.
    fn apply_setting_request(&mut self, reset: bool, edits: &[SettingEdit]) {
        if self.runtime.is_none() {
            return;
        }
        let mut changed = false;

        if reset {
            // Scope the runtime borrow so we can also touch `library`/`model` here.
            if let Some(rt) = self.runtime.as_mut() {
                rt.reset();
            }
            changed = true;
            // Snap the panel's working values back to the artifact defaults so the
            // widgets reflect the reset. The control structure is unchanged.
            if let Some(id) = self.active_id.as_ref() {
                if let Some(loaded) = self.library.get(id) {
                    self.model.borrow_mut().settings.reset_to_defaults(loaded);
                }
            }
        }

        if let Some(rt) = self.runtime.as_mut() {
            for edit in edits {
                if rt.set_setting(&edit.name, edit.value) {
                    changed = true;
                }
            }
        }

        if changed {
            if let (Some(hook), Some(id), Some(rt)) = (
                self.on_setting_change.as_mut(),
                self.active_id.as_ref(),
                self.runtime.as_ref(),
            ) {
                if let Some(loaded) = self.library.get(id) {
                    hook(loaded, rt.settings());
                }
            }
        }
    }

    /// Opens the macOS Screen & System Audio Recording privacy pane directly via
    /// `open(1)` — shelling out keeps this dependency-free (no extra crate just to
    /// launch a URL). Logs once on success and on failure; a spawn failure does not
    /// crash the app (the guidance text still names the path so the user can navigate
    /// there manually).
    fn open_settings() {
        match std::process::Command::new("open").arg(SETTINGS_URL).spawn() {
            Ok(_) => tracing::info!(
                target: "viz_app::viz",
                url = SETTINGS_URL,
                "opened System Settings privacy pane"
            ),
            Err(e) => tracing::error!(
                target: "viz_app::viz",
                error = %e,
                url = SETTINGS_URL,
                "failed to open System Settings privacy pane"
            ),
        }
    }

    /// Applies the persisted-side effects of a collapse toggle.
    fn set_collapsed(&mut self, collapsed: bool) {
        if self.collapsed == collapsed {
            return;
        }
        self.collapsed = collapsed;
        if let Some(hook) = self.on_collapse_change.as_mut() {
            hook(collapsed);
        }
    }

    /// Builds the GPU renderer and attaches the overlay once the surface format is
    /// known (called from the app's `on_ready` hook).
    pub fn attach_gpu(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        window: &Window,
        format: wgpu::TextureFormat,
    ) {
        let mut renderer = Renderer::new(device, queue, format);
        let size = window.inner_size();
        renderer.resize(device, size.width, size.height);
        self.renderer = Some(renderer);
        self.overlay.attach(device, window, format);
        // Run the artifact's var init now that everything is wired.
        if let Some(rt) = self.runtime.as_mut() {
            rt.activate();
        }
    }

    /// Polls audio + clock and returns `(feature, dt, capture_failure, permission)`.
    ///
    /// `capture_failure` is computed by [`non_permission_capture_failure`] — see its
    /// docs and the pinning test `running_failed_capture_is_not_a_render_state`
    /// (MINOR-3): a *running* handle reporting [`viz_audio::CaptureState::Failed`] maps
    /// to `capture_failure = None` (it is a permission-machine input, fed inside the
    /// watchdog, not a UI state), which is what fixes the root-cause R1.2 flicker.
    fn poll_audio(&mut self) -> (FeatureFrame, f64, Option<String>, PermissionState) {
        let now = Instant::now();
        let (feature, capture_failure, permission) = match self.audio.as_mut() {
            Some(handle) => {
                // Drain beats (informational; `beat` is already in the frame).
                self.beat_scratch.clear();
                let _ = handle.drain_beats_into(&mut self.beat_scratch);
                let frame = handle.latest_frame();
                let permission = handle.permission_state();
                // A running handle is never a *non-permission* failure here; the
                // permission machine owns the silence/revocation verdict (the
                // watchdog feeds `CaptureState::Failed` into it as an observation).
                let capture_failure =
                    non_permission_capture_failure(true, handle.capture_state(), self.start_failed);
                (frame, capture_failure, permission)
            }
            // Capture never started: a silent default frame plus (per the helper)
            // the live feature-001 capture-failure message (re-evaluated every
            // frame). With no handle, the permission state is irrelevant
            // (CaptureFailed outranks it in `select_view`).
            None => {
                let capture_failure =
                    non_permission_capture_failure(false, CaptureState::Active, self.start_failed);
                (
                    FeatureFrame::default(),
                    capture_failure,
                    PermissionState::Unknown,
                )
            }
        };

        // Record the wall-clock arrival of a *new* feature frame (distinct `t`) for
        // the HUD's feature-frame age. The audio clock only advances when new
        // samples arrive, so a changed `t` marks a fresh frame.
        if feature.t != self.latest_frame_t {
            self.latest_frame_t = feature.t;
            self.latest_frame_at = Some(now);
        }

        // dt = delta of the audio clock between rendered frames, clamped 0..0.1;
        // first frame dt = 0.
        let dt = match self.prev_t {
            None => 0.0,
            Some(prev) => (feature.t - prev).clamp(0.0, MAX_DT),
        };
        self.prev_t = Some(feature.t);

        (feature, dt, capture_failure, permission)
    }
}

/// Decides the *non-permission* `capture_failure` message for [`select_view`] from
/// the live audio state (pure, so it can be unit-tested in isolation).
///
/// The only non-permission failure surfaced as a render state today is the pipeline
/// failing to **start** at all (no handle + `start_failed`) — the generic
/// capture-health guidance, re-evaluated every frame.
///
/// Crucially, a **running** handle's [`CaptureState::Failed`] (the sustained-zero
/// fault the watchdog raises while it rebuilds the tap) is *not* a render state: it
/// is an input to the permission machine (the watchdog feeds it into the
/// silence-vs-revocation verdict), so it must map to `None` here and surface only as
/// the live [`PermissionState`]. Routing it to `CaptureFailed` instead would make the
/// window flicker between the active scene and the failure message every time the
/// watchdog rebuilds the tap — this function pins it shut: while a handle is present,
/// `CaptureState` is ignored entirely.
fn non_permission_capture_failure(
    handle_present: bool,
    _running_capture_state: CaptureState,
    start_failed: bool,
) -> Option<String> {
    if handle_present {
        // A live handle is never a non-permission failure, whatever its
        // CaptureState (the permission machine owns the verdict).
        None
    } else if start_failed {
        Some(CAPTURE_START_FAILED_MESSAGE.to_owned())
    } else {
        None
    }
}

impl RenderCallback for VizApp {
    fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        // Any pointer movement resets the overlay auto-hide timer. We flag
        // it here and stamp it against the render clock in `render`, keeping all
        // timing on a single clock. `CursorEntered` also counts so the overlay
        // reappears when the cursor returns to the window.
        if matches!(
            event,
            WindowEvent::CursorMoved { .. } | WindowEvent::CursorEntered { .. }
        ) {
            self.pointer_moved_since_render = true;
        }

        // Persist window geometry on move/resize, debounced so a drag does not write
        // on every event.
        if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
            let now = Instant::now();
            if should_save_window(now, self.last_window_save, WINDOW_SAVE_DEBOUNCE) {
                if let (Some(hook), Some(state)) = (
                    self.on_window_change.as_mut(),
                    Self::current_window_state(window),
                ) {
                    hook(state);
                    self.last_window_save = Some(now);
                }
            }
        }

        // Final flush on quit: capture the latest geometry unconditionally so a
        // move within the debounce window just before closing is not lost.
        if matches!(event, WindowEvent::CloseRequested) {
            if let (Some(hook), Some(state)) = (
                self.on_window_change.as_mut(),
                Self::current_window_state(window),
            ) {
                hook(state);
                self.last_window_save = Some(Instant::now());
            }
        }

        // Renderer target resizing needs the wgpu device, which is only available
        // in `render`; it reconciles the renderer size against `frame.config` every
        // frame, so a `Resized` event needs no special handling here. We just
        // forward input to the egui overlay.
        self.overlay.on_window_event(window, event)
    }

    fn render(&mut self, frame: &mut FrameContext<'_>) {
        // Resize the renderer's targets to the current surface each frame (cheap
        // no-op when unchanged; covers initial size + window resizes uniformly).
        if let Some(r) = self.renderer.as_mut() {
            r.resize(frame.device, frame.config.width, frame.config.height);
        }

        let (feature, dt, capture_failure, permission) = self.poll_audio();
        let has_artifact = self.runtime.is_some();
        let view = select_view(
            has_artifact,
            capture_failure.as_deref(),
            permission,
            AUDIO_PERMISSION_APPLIES,
        );

        let aspect = if frame.config.height > 0 {
            frame.config.width as f32 / frame.config.height as f32
        } else {
            1.0
        };

        // Feature-frame age (wall clock) for the HUD.
        self.hud.feature_age_ms = self
            .latest_frame_at
            .map(|t| Instant::now().saturating_duration_since(t).as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        // Render the scene only in the Active view; the other views leave the dark
        // clear pass in place and rely on the egui overlay to paint the message
        // (the window must always explain its state — never a silent black screen).
        if let (AppView::Active, Some(renderer), Some(rt)) =
            (&view, self.renderer.as_mut(), self.runtime.as_mut())
        {
            self.degrade.begin_frame(Instant::now());
            let detail = self.degrade.detail_factor();
            let paused = self.degrade.is_paused();
            let shed_feedback = self.degrade.shed_feedback();

            if paused {
                if !self.logged_paused {
                    tracing::warn!("viz-app: rendering idle clear (degrade paused)");
                    self.logged_paused = true;
                }
                let scene = rt.idle_scene();
                renderer.render(
                    frame.device,
                    frame.queue,
                    frame.encoder,
                    frame.view,
                    &scene.params,
                    &scene.layers,
                );
            } else {
                self.logged_paused = false;
                let mut scene = rt.frame(&feature, dt, aspect, detail);
                // Shed feedback first when degrading: drop trails before anything else.
                if shed_feedback {
                    scene.params.feedback_decay = None;
                }
                renderer.render(
                    frame.device,
                    frame.queue,
                    frame.encoder,
                    frame.view,
                    &scene.params,
                    &scene.layers,
                );
            }
            self.degrade.end_frame(Instant::now());

            self.hud.detail_factor = detail;
            self.hud.shed_feedback = shed_feedback;
            self.hud.paused = paused;
        }

        // Perf HUD: record this presented frame's timing and snapshot it.
        #[cfg(feature = "perf-hud")]
        let hud_snapshot = {
            self.perf_hud.record_frame(Instant::now());
            self.perf_hud.snapshot(self.hud)
        };

        // Stamp the pointer-move timer for overlay auto-hide. Done once per render on
        // the render clock so visibility timing is independent of the input rate.
        let render_now = Instant::now();
        if self.pointer_moved_since_render {
            self.pointer_moved_since_render = false;
            self.last_pointer_move = Some(render_now);
        }
        // Time since the last pointer move; `None` (never moved) ⇒ treat as "just
        // moved" so the overlay starts visible.
        let since_move = self
            .last_pointer_move
            .map(|t| render_now.saturating_duration_since(t))
            .unwrap_or(Duration::ZERO);
        // The overlay is interactive: while egui reports the pointer over a panel we
        // keep it shown so a dropdown/settings interaction is never yanked away.
        let pointer_over_overlay = self.overlay.wants_pointer();
        let vis = decide_visibility(since_move, pointer_over_overlay, self.collapsed);

        // Update the overlay model, then paint the egui overlay on top.
        {
            let mut model = self.model.borrow_mut();
            model.view = ViewSnapshot::from(&view);
            // Only re-clone the active id when it actually changed (it changes only
            // on a dropdown switch), so the steady-state frame allocates nothing.
            if model.active_id != self.active_id {
                model.active_id = self.active_id.clone();
            }
            // `model.entries` (the dropdown list) is built once in `new`; the library
            // is immutable for VizApp's lifetime, so we never rebuild it per-frame —
            // the render loop must not allocate in steady state.
            model.collapsed = vis.collapsed;
            model.show_overlay = vis.show;
            model.request = OverlayRequest::default();
            #[cfg(feature = "perf-hud")]
            {
                model.hud = hud_snapshot;
            }
        }
        self.overlay.render(frame);

        // Drain any user action the overlay closure recorded this frame and apply it
        // (the closure cannot borrow `self` mutably, so it queues into the model).
        let request = {
            let model = self.model.borrow();
            if model.request.is_empty() {
                None
            } else {
                Some(model.request.clone())
            }
        };
        if let Some(req) = request {
            // Reload first: it rebuilds the library + dropdown and may change the
            // active artifact, so a same-frame `select` (below) resolves against the
            // freshly scanned library.
            if req.reload_artifacts {
                self.reload();
            }
            if let Some(id) = req.select {
                self.switch_to(&id);
            }
            if let Some(collapsed) = req.set_collapsed {
                self.set_collapsed(collapsed);
            }
            // Apply setting edits / reset to the runtime. Each edit writes straight
            // into the runtime's setting slot, flushed before the next frame's var
            // evaluation — no deferred queue. We then notify the persistence hook
            // once with the full current map.
            self.apply_setting_request(req.reset_settings, &req.setting_edits);
            // Open the System Settings privacy pane.
            if req.open_settings {
                Self::open_settings();
            }
        }
    }
}

/// Glyphs for the collapse chevron: expanded ⇒ click to collapse;
/// collapsed ⇒ click to expand.
const CHEVRON_COLLAPSE: &str = "\u{25BE}"; // ▾ pointing down (panel open)
const CHEVRON_EXPAND: &str = "\u{25B8}"; // ▸ pointing right (panel collapsed)

/// Builds the egui overlay content for the current [`OverlayModel`] view, writing
/// any user action back into [`OverlayModel::request`] for [`VizApp`] to apply.
///
/// Active: the top-right panel with the artifact dropdown and a chevron
/// that collapses the panel to a small glyph; the whole panel auto-hides
/// when `show_overlay` is false. The other views render full-window centered
/// guidance so the window is never blank.
fn render_overlay(ctx: &egui::Context, model: &mut OverlayModel) {
    // The perf HUD (top-left) draws in every view when the feature is enabled.
    #[cfg(feature = "perf-hud")]
    crate::hud::draw(ctx, &model.hud);

    match &model.view {
        ViewSnapshot::Active => {
            // Auto-hidden: draw nothing. Pointer movement reappears it.
            if !model.show_overlay {
                return;
            }
            egui::Area::new(egui::Id::new("viewmusic-overlay"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        if model.collapsed {
                            // Collapsed: just the chevron glyph; click to expand.
                            if ui
                                .button(CHEVRON_EXPAND)
                                .on_hover_text("Show panel")
                                .clicked()
                            {
                                model.request.set_collapsed = Some(false);
                            }
                        } else {
                            render_panel_body(ui, model);
                        }
                    });
                });
        }
        ViewSnapshot::NoArtifacts => {
            centered_message(ctx, "no-artifacts", |ui| {
                ui.heading(NO_ARTIFACTS_MESSAGE);
                ui.label(
                    "No visual artifacts loaded. Add a valid .artifact.json to the \
                     artifacts folder and reload.",
                );
            });
        }
        ViewSnapshot::CaptureFailed { message } => {
            // Generic capture-health guidance (non-permission failure).
            centered_message(ctx, "capture-failed", |ui| {
                ui.heading("System-audio capture unavailable");
                ui.add_space(8.0);
                ui.label(message);
                ui.add_space(8.0);
                ui.label(PERMISSION_SETTINGS_PATH);
            });
        }
        ViewSnapshot::WaitingForAudio => {
            // Neutral, persistent, non-flashing hint for platforms without an
            // audio-capture permission (Windows): capture is live, nothing is
            // playing yet. No consent or System-Settings language.
            centered_message(ctx, "waiting-for-audio", |ui| {
                ui.heading(WAITING_FOR_AUDIO_TITLE);
                ui.add_space(8.0);
                ui.label(WAITING_FOR_AUDIO_HINT);
            });
        }
        ViewSnapshot::PermissionGuidance { variant } => {
            // Persistent permission guidance. Same screen for both variants; the
            // Waiting variant adds the dialog hint, the Denied variant adds the
            // Open-Settings button.
            let variant = *variant;
            centered_message(ctx, "permission-guidance", |ui| {
                ui.heading(PERMISSION_TITLE);
                ui.add_space(8.0);
                ui.label(PERMISSION_WHY);
                ui.add_space(8.0);
                ui.label(PERMISSION_SETTINGS_PATH);
                if variant == GuidanceVariant::Waiting {
                    ui.add_space(8.0);
                    ui.label(PERMISSION_WAITING_HINT);
                }
                if variant == GuidanceVariant::Denied {
                    // Returning-user softener (MINOR-1): a user who already granted
                    // but is simply not playing audio lands here after the grace.
                    ui.add_space(8.0);
                    ui.label(PERMISSION_ALREADY_ALLOWED_HINT);
                    ui.add_space(12.0);
                    if ui.button(OPEN_SETTINGS_BUTTON).clicked() {
                        model.request.open_settings = true;
                    }
                }
            });
        }
    }
}

/// Renders the expanded top-right panel body: header row with the chevron, then the
/// artifact dropdown. Records any selection / collapse request into `model.request`.
fn render_panel_body(ui: &mut egui::Ui, model: &mut OverlayModel) {
    ui.horizontal(|ui| {
        ui.strong("ViewMusic");
        // Push the chevron to the right edge of the header row.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(CHEVRON_COLLAPSE)
                .on_hover_text("Collapse panel")
                .clicked()
            {
                model.request.set_collapsed = Some(true);
            }
        });
    });

    if model.entries.is_empty() {
        // Loaded library is empty: mirror the no-artifacts copy in-panel. (The
        // Active view normally implies a runtime, but stay robust.) Still offer the
        // reload action so a user who has just dropped a file in can re-scan.
        ui.label(NO_ARTIFACTS_MESSAGE);
        if ui
            .button("Reload artifacts")
            .on_hover_text("Re-scan the artifacts folder")
            .clicked()
        {
            model.request.reload_artifacts = true;
        }
        return;
    }

    // The current selection's display name (falls back to the id, then a prompt).
    let selected_label = model
        .active_id
        .as_ref()
        .and_then(|id| model.entries.iter().find(|e| &e.id == id))
        .map(|e| e.name.clone())
        .unwrap_or_else(|| "Select artifact".to_owned());

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("artifact-dropdown")
            .selected_text(selected_label)
            .show_ui(ui, |ui| {
                for entry in &model.entries {
                    let is_active = model.active_id.as_ref() == Some(&entry.id);
                    // `selectable_label` returns a click response; on click we
                    // *request* the switch — VizApp applies it after the egui pass.
                    if ui.selectable_label(is_active, &entry.name).clicked() && !is_active {
                        model.request.select = Some(entry.id.clone());
                    }
                }
            });

        // "Reload artifacts" re-scans built-ins + the user folder from scratch.
        // VizApp applies it after the egui pass (it rebuilds the library).
        if ui
            .button("\u{21bb}") // ↻ refresh glyph
            .on_hover_text("Reload artifacts (re-scan the artifacts folder)")
            .clicked()
        {
            model.request.reload_artifacts = true;
        }
    });

    // The auto-generated settings panel below the dropdown. Split the
    // model borrow: the panel owns its working values; we collect borrowed changes
    // into a local scratch (no allocation when there are no edits — the common
    // case), then translate to owned `SettingEdit`s the host applies after the pass.
    let OverlayModel {
        settings, request, ..
    } = model;
    let mut changes: Vec<SettingChange> = Vec::new();
    let reset = settings.ui(ui, &mut changes);
    for change in &changes {
        request.setting_edits.push(SettingEdit {
            name: change.name.to_owned(),
            value: change.value,
        });
    }
    if reset {
        request.reset_settings = true;
    }
}

/// Renders a screen-centered message block (used for the no-artifacts and
/// permission-guidance full-window states). Uses a center-anchored
/// [`egui::Area`] so the window is never blank, without the deprecated
/// `CentralPanel::show(ctx, …)` path.
fn centered_message(ctx: &egui::Context, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Area::new(egui::Id::new(id))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| add(ui));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::{select_view, AppView};

    /// MINOR-3 (pins root cause R1.2). A *running* audio handle reporting
    /// [`CaptureState::Failed`] (the sustained-zero fault the watchdog raises while
    /// it rebuilds the tap) must NOT become a `capture_failure` render input — it is
    /// a permission-machine input, surfaced only as the live [`PermissionState`].
    /// Routing it to `CaptureFailed` was the original flicker; this test locks it
    /// shut so it can never be reintroduced silently.
    #[test]
    fn running_failed_capture_is_not_a_render_state() {
        // Running handle, CaptureState::Failed ⇒ no render-state capture failure.
        let failed = CaptureState::Failed {
            message: "System-audio capture stalled (silent buffers).".to_owned(),
        };
        assert_eq!(
            non_permission_capture_failure(true, failed.clone(), false),
            None,
            "a running handle's CaptureState::Failed must map to capture_failure=None"
        );
        // And it is ignored even if start_failed somehow reads true.
        assert_eq!(
            non_permission_capture_failure(true, failed.clone(), true),
            None,
            "with a live handle present, CaptureState is ignored entirely"
        );
        // Active is likewise None (sanity).
        assert_eq!(
            non_permission_capture_failure(true, CaptureState::Active, false),
            None
        );

        // Fed through select_view with a Granted permission (the steady state during
        // a rebuild while music was playing): the view stays Active — no guidance
        // flash. This is the end-to-end shape of the R1.2 fix.
        let capture_failure = non_permission_capture_failure(true, failed, false);
        assert_eq!(
            select_view(
                true,
                capture_failure.as_deref(),
                PermissionState::Granted,
                AUDIO_PERMISSION_APPLIES,
            ),
            AppView::Active,
            "a rebuild-in-progress running fault must not flip a Granted view away \
             from Active (root-cause R1.2 regression)"
        );
    }

    /// The complementary case: a pipeline that never started (no handle) DOES surface
    /// the feature-001 generic capture-failure guidance, live every frame.
    #[test]
    fn never_started_pipeline_surfaces_generic_failure() {
        assert_eq!(
            non_permission_capture_failure(false, CaptureState::Active, true),
            Some(CAPTURE_START_FAILED_MESSAGE.to_owned())
        );
        // No handle and not start_failed (shouldn't happen, but pin it): None.
        assert_eq!(
            non_permission_capture_failure(false, CaptureState::Active, false),
            None
        );
    }
}
