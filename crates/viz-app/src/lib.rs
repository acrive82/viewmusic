//! viz-app — the ViewMusic application library.
//!
//! The binary (`src/main.rs`, target `viewmusic`) is a thin shim over this
//! library so the runtime, app-state, and viz-wiring modules are reachable from
//! integration tests (`tests/`). The window/overlay/logging shell modules live
//! here too; only `main` owns the event loop.
//!
//! Modules:
//! - [`runtime`]: the deterministic artifact runtime.
//! - [`app_state`]: pure top-level view-state selection.
//! - [`settings_panel`]: the auto-generated, live-tunable settings panel.
//! - [`viz`]: the composite audio → runtime → renderer → overlay
//!   callback, including artifact switching from the dropdown, live setting
//!   edits, and the "Reload artifacts" re-scan action.
//! - [`persist`]: persisted app + window state.
//! - [`window`]/[`overlay`]/[`logging`]: the windowing shell; `overlay` also owns the
//!   overlay auto-hide/collapse logic.
//! - [`hud`]: optional perf HUD (behind the `perf-hud` feature).

pub mod app_state;
#[cfg(feature = "perf-hud")]
pub mod hud;
pub mod logging;
pub mod overlay;
pub mod persist;
pub mod runtime;
pub mod settings_panel;
pub mod viz;
pub mod window;
