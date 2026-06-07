//! ViewMusic application binary entry point.
//!
//! Wires logging → the artifact library + persisted state → the winit app with the
//! dark clear pass and the egui overlay. The window/GPU are created lazily in the
//! winit `resumed` handler; the overlay is attached to the GPU via the app's
//! `on_ready` hook once the surface format is known.
//!
//! Startup: load the built-in artifacts into an
//! [`ArtifactLibrary`], then scan the user artifacts folder (created if missing) for
//! `*.artifact.json` files appended after the built-ins; read the persisted
//! [`AppState`] and [`WindowState`], resolve the artifact to activate (saved id if
//! still present, else the first built-in), and restore the overlay collapse and
//! window geometry. The dropdown selection, collapse toggle, and window moves are
//! persisted live through hooks on the [`VizApp`]; the "Reload artifacts" action
//! re-runs the same library-build closure to pick up folder changes without a
//! restart.
//!
//! NOTE: this binary must NOT be `cargo run` during development — it opens a window
//! and requests system-audio capture. All logic lives in the `viz_app` library so
//! it is reachable from integration tests; this file owns only the event loop.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::event_loop::{ControlFlow, EventLoop};

use indexmap::IndexMap;
use viz_app::logging;
use viz_app::persist::{ensure_artifacts_dir, should_save_window, AppState, WindowState};
use viz_app::runtime::SettingValue;
use viz_app::viz::VizApp;
use viz_app::window::{App, InitialWindow};
use viz_audio::AudioPipeline;
use viz_contract::{ArtifactLibrary, LoadedArtifact};

/// The embedded built-in artifacts: a spread of examples demonstrating the
/// contract's breadth. Built-ins load first so a user file can never shadow them.
/// Compiled into the binary so the app always has them. The label is the source
/// name shown in load diagnostics.
const BUILTIN_ARTIFACTS: &[(&str, &str)] = &[
    (
        "spectrum-bars (built-in)",
        include_str!("../../../assets/builtin-artifacts/spectrum-bars.artifact.json"),
    ),
    (
        "oscilloscope (built-in)",
        include_str!("../../../assets/builtin-artifacts/oscilloscope.artifact.json"),
    ),
    (
        "radial-pulse (built-in)",
        include_str!("../../../assets/builtin-artifacts/radial-pulse.artifact.json"),
    ),
    (
        "particle-burst (built-in)",
        include_str!("../../../assets/builtin-artifacts/particle-burst.artifact.json"),
    ),
    (
        "color-field (built-in)",
        include_str!("../../../assets/builtin-artifacts/color-field.artifact.json"),
    ),
    (
        "starfield-warp (built-in)",
        include_str!("../../../assets/builtin-artifacts/starfield-warp.artifact.json"),
    ),
    (
        "liquid-spectrum (built-in)",
        include_str!("../../../assets/builtin-artifacts/liquid-spectrum.artifact.json"),
    ),
    (
        "dna-helix (built-in)",
        include_str!("../../../assets/builtin-artifacts/dna-helix.artifact.json"),
    ),
    (
        "kaleido-petals (built-in)",
        include_str!("../../../assets/builtin-artifacts/kaleido-petals.artifact.json"),
    ),
    (
        "bass-tunnel (built-in)",
        include_str!("../../../assets/builtin-artifacts/bass-tunnel.artifact.json"),
    ),
    (
        "aurora-waves (built-in)",
        include_str!("../../../assets/builtin-artifacts/aurora-waves.artifact.json"),
    ),
    (
        "neon-gauges (built-in)",
        include_str!("../../../assets/builtin-artifacts/neon-gauges.artifact.json"),
    ),
    (
        "spectrum-galaxy (built-in)",
        include_str!("../../../assets/builtin-artifacts/spectrum-galaxy.artifact.json"),
    ),
    (
        "pixel-rain (built-in)",
        include_str!("../../../assets/builtin-artifacts/pixel-rain.artifact.json"),
    ),
    (
        "breathing-grid (built-in)",
        include_str!("../../../assets/builtin-artifacts/breathing-grid.artifact.json"),
    ),
];

/// Builds the artifact library from scratch: the embedded built-ins first (so they
/// are never shadowed), then a deterministic, filename-sorted scan of the user
/// artifacts folder (created if missing). Rejected files are logged inside the
/// library with their full diagnostics (to the log file only, never to the screen).
/// Reused for both the startup build and the "Reload artifacts" action, so the two
/// paths are identical.
fn build_library() -> ArtifactLibrary {
    let mut library = ArtifactLibrary::load_builtins(BUILTIN_ARTIFACTS.iter().copied());
    // Resolve + create the user artifacts folder, then scan it. A missing
    // platform dir / create failure is logged inside the resolver and we simply run
    // on built-ins only.
    if let Some(dir) = ensure_artifacts_dir() {
        library.scan_folder(&dir);
    }
    library
}

fn main() {
    // Keep the worker guard alive for the whole process so logs flush on exit.
    let _log_guard = logging::init();
    tracing::info!(target: "viewmusic", "starting ViewMusic");

    // Build the artifact library: embedded built-ins first, then a scan of the user
    // artifacts folder. Rejected files are logged inside the library; with zero
    // loaded artifacts the app renders the explicit "No artifacts available" state.
    let library = build_library();
    tracing::info!(
        target: "viewmusic",
        loaded = library.loaded_count(),
        "built artifact library"
    );

    // Restore persisted state: last artifact, overlay collapse, window geometry.
    let app_state = AppState::load();
    let initial_window = WindowState::load().map(|w| InitialWindow {
        x: w.x,
        y: w.y,
        width: w.width,
        height: w.height,
    });
    let active_id = app_state.resolve_artifact(&library);
    if let Some(id) = active_id.as_ref() {
        tracing::info!(target: "viewmusic", id = %id, "resolved active artifact");
    }
    let overlay_collapsed = app_state.overlay_collapsed;

    // Start system-audio capture. This always runs the full create-tap → aggregate
    // → IOProc → AudioDeviceStart sequence — that *is* what triggers the macOS TCC
    // permission prompt. On a creation error there is no handle; the VizApp renders
    // the live capture-failure guidance every frame (re-evaluated each frame, not a
    // one-shot snapshot, so the window never gets stuck on a stale state).
    let audio = match AudioPipeline::start() {
        Ok(handle) => {
            tracing::info!(target: "viewmusic", "audio capture started");
            Some(handle)
        }
        Err(e) => {
            tracing::error!(target: "viewmusic", error = %e, "audio capture failed to start");
            None
        }
    };

    let event_loop = EventLoop::new().expect("failed to create winit event loop");
    // Poll keeps re-requesting redraws driving the (paced) render loop.
    event_loop.set_control_flow(ControlFlow::Poll);

    // Shared, mutable persisted state: the VizApp hooks update it live; we save the
    // relevant document immediately on each change (small, discrete writes — the
    // chatty window geometry is debounced inside VizApp before reaching its hook).
    let state = Rc::new(RefCell::new(app_state));

    // Debounce for the chatty setting-edit writes: a slider drag fires
    // many edits per second. We update the in-memory AppState on every edit but only
    // flush to disk on a ~1 s debounce; a switch (on_select) and quit force a flush.
    const SETTING_SAVE_DEBOUNCE: Duration = Duration::from_secs(1);
    let last_setting_save: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));

    let on_select = {
        let state = state.clone();
        move |id: &viz_core::ArtifactId| {
            let mut s = state.borrow_mut();
            s.last_artifact_id = Some(id.to_string());
            // A switch forces a flush so a setting drag just before it is not lost.
            s.save();
        }
    };
    let on_collapse = {
        let state = state.clone();
        move |collapsed: bool| {
            let mut s = state.borrow_mut();
            s.overlay_collapsed = collapsed;
            s.save();
        }
    };
    let on_window = |ws: WindowState| ws.save();

    // Restore validated per-artifact setting overrides for the active artifact at
    // startup + on every switch. Validation lives in persist.rs.
    let on_setting_restore = {
        let state = state.clone();
        move |art: &LoadedArtifact| -> IndexMap<String, SettingValue> {
            state.borrow().artifact_settings(art)
        }
    };

    // Persist a setting edit/reset: update the in-memory block, then flush on the
    // ~1 s debounce (the switch/quit paths force a flush).
    let on_setting_change = {
        let state = state.clone();
        let last_setting_save = last_setting_save.clone();
        move |art: &LoadedArtifact, values: &IndexMap<String, SettingValue>| {
            let mut s = state.borrow_mut();
            s.set_artifact_settings(art.id.0.as_str(), &art.artifact.settings, values);
            let now = Instant::now();
            let mut last = last_setting_save.borrow_mut();
            if should_save_window(now, *last, SETTING_SAVE_DEBOUNCE) {
                s.save();
                *last = Some(now);
            }
        }
    };

    let viz_app = VizApp::new(library, active_id, overlay_collapsed, audio)
        .on_artifact_change(on_select)
        .on_collapse_change(on_collapse)
        .on_window_change(on_window)
        .on_setting_change(on_setting_change)
        .on_setting_restore(on_setting_restore)
        // "Reload artifacts" rebuilds the library exactly as at startup.
        .on_reload_artifacts(build_library);

    let mut app = App::new(viz_app)
        .with_initial_window(initial_window)
        .on_ready(|gpu, win, viz_app| {
            viz_app.attach_gpu(&gpu.device, &gpu.queue, win, gpu.format);
        });

    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!(target: "viewmusic", error = %e, "event loop terminated with error");
    }

    // Final save of the top-level app state on quit. Window geometry was
    // flushed by the VizApp on CloseRequested.
    state.borrow().save();

    // Dropping `app` here drops the VizApp → AudioHandle::Drop runs the capture
    // teardown (stops the IOProc and releases the tap). `_log_guard` drops last,
    // flushing the log writer.
}
