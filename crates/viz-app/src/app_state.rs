//! Top-level app view state and its per-frame selection.
//!
//! This is the pure, GUI-independent decision logic for *what the window shows*.
//! It is evaluated **every frame** from the live audio handle state — there is no
//! one-shot startup snapshot, so the guidance screen is persistent by construction
//! (without re-evaluating every frame, the window could get stuck on a stale state
//! and flicker between the scene and the guidance):
//!
//! * [`AppView::NoArtifacts`] — zero artifacts loaded successfully. The window must
//!   render an explicit "No artifacts available" message, never a black screen
//!   (rejected artifacts are logged; with none loaded the app says so).
//! * [`AppView::CaptureFailed`] — system-audio capture failed for a *non-permission*
//!   reason (a device/OSStatus error, or the pipeline failed to start at all). This
//!   is the generic capture-health guidance, still live-rendered. Permission denial
//!   is **not** routed here — it is kept separate so the two cases get distinct copy.
//! * [`AppView::PermissionGuidance`] — the audio-capture permission is missing
//!   (`Unknown` = grace window / dialog possibly on screen; `Denied` = grace elapsed
//!   or revoked). The persistent guidance screen renders, with the variant deciding
//!   the extra line / the Open-Settings button.
//! * [`AppView::Active`] — an artifact is loaded, capture is healthy, and the
//!   permission is `Granted` (incl. genuine silence — zeroed features settle the
//!   scene). The visualizer renders.
//!
//! The selection function [`select_view`] is unit-tested without any GUI so the
//! state machine is verifiable in isolation.

use viz_audio::PermissionState;

/// The macOS System Settings path the permission guidance points the user to.
pub const PERMISSION_SETTINGS_PATH: &str =
    "System Settings → Privacy & Security → Screen & System Audio Recording";

/// Title of the permission guidance screen.
pub const PERMISSION_TITLE: &str = "System Audio Recording permission needed";

/// One-sentence "why" line for the guidance screen.
pub const PERMISSION_WHY: &str =
    "ViewMusic visualizes your Mac's audio. Nothing is recorded or stored.";

/// The extra line shown only in the `Unknown` (waiting) variant: the OS dialog may
/// be on screen right now.
pub const PERMISSION_WAITING_HINT: &str =
    "macOS may be showing a permission dialog right now — click Allow.";

/// The extra line shown only in the `Denied` (full) variant for a *returning* user.
/// Someone who granted in a past session but has no audio playing lands in `Denied`
/// after the 5 s grace; this softens the copy so they are
/// not pushed into Settings unnecessarily — they only need to start playback.
pub const PERMISSION_ALREADY_ALLOWED_HINT: &str =
    "Already allowed? Just play some music — the visualizer starts as soon as audio flows.";

/// Label of the control that deep-links into the correct System Settings pane.
pub const OPEN_SETTINGS_BUTTON: &str = "Open System Settings";

/// The `open(1)` URL that lands directly in the Screen & System Audio Recording
/// privacy pane. macOS exposes no `Privacy_AudioCapture` anchor, so we deep-link to
/// the Screen Capture pane (which is where system-audio recording lives).
pub const SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// The live message shown when the capture pipeline failed to *start* at all
/// (non-permission: e.g. unsupported platform or a device/OSStatus error). Routed
/// through the generic capture-health guidance branch and evaluated every frame.
pub const CAPTURE_START_FAILED_MESSAGE: &str =
    "System-audio capture could not be started. Enable system-audio capture in \
     System Settings > Privacy & Security > Screen & System Audio Recording, then \
     restart ViewMusic.";

/// The message shown when no artifacts loaded successfully (empty library).
pub const NO_ARTIFACTS_MESSAGE: &str = "No artifacts available";

/// What the window should display this frame (pure state; no GUI types).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppView {
    /// The library is empty: render the explicit no-artifacts message.
    NoArtifacts,
    /// Capture failed for a non-permission reason (generic capture-health handling):
    /// render the capture-health guidance with this message.
    CaptureFailed {
        /// The capture-layer message explaining the failure (English prose).
        message: String,
    },
    /// The audio-capture permission is missing: render the persistent
    /// permission guidance screen. The variant decides the extra hint line and
    /// whether the Open-Settings button shows.
    PermissionGuidance {
        /// Which guidance variant to render (waiting vs. denied/full).
        variant: GuidanceVariant,
    },
    /// An artifact is active, capture is healthy, and the permission is granted:
    /// render the visualizer.
    Active,
}

/// The two permission-guidance variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuidanceVariant {
    /// `PermissionState::Unknown`: grace window running — the OS dialog may be on
    /// screen. Adds the "macOS may be showing a permission dialog" hint; no button
    /// (the user should respond to the dialog, not dig into Settings yet).
    Waiting,
    /// `PermissionState::Denied`: grace elapsed all-zero, or a revocation verdict.
    /// The full variant with the Open System Settings button.
    Denied,
}

/// Selects the [`AppView`] from the live state, evaluated **every frame**. Inputs:
/// * `has_active_artifact` — whether a runtime is loaded (else no-artifacts);
/// * `capture_failure` — `Some(msg)` for a *non-permission* capture failure
///   (a generic `CaptureState::Failed`, or a failed-to-start pipeline);
///   `None` otherwise;
/// * `permission` — the live [`PermissionState`] from the audio handle.
///
/// Priority order: no-artifact → non-permission capture failure → `Unknown`
/// guidance → `Denied` guidance → visualization. There is no frame in which capture
/// is permission-unavailable and the guidance is absent — guidance is always on
/// screen until the permission is actually granted.
///
/// A non-permission capture failure outranks the permission state: a genuine
/// device error should show the capture-health message even if the permission
/// machine has not yet promoted to `Granted`.
pub fn select_view(
    has_active_artifact: bool,
    capture_failure: Option<&str>,
    permission: PermissionState,
) -> AppView {
    if !has_active_artifact {
        return AppView::NoArtifacts;
    }
    if let Some(message) = capture_failure {
        return AppView::CaptureFailed {
            message: message.to_owned(),
        };
    }
    match permission {
        PermissionState::Unknown => AppView::PermissionGuidance {
            variant: GuidanceVariant::Waiting,
        },
        PermissionState::Denied => AppView::PermissionGuidance {
            variant: GuidanceVariant::Denied,
        },
        PermissionState::Granted => AppView::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_artifact_takes_precedence() {
        // Even if capture failed / permission missing, with no artifact we show the
        // empty-library state.
        assert_eq!(
            select_view(false, None, PermissionState::Granted),
            AppView::NoArtifacts
        );
        assert_eq!(
            select_view(false, Some("boom"), PermissionState::Denied),
            AppView::NoArtifacts
        );
    }

    #[test]
    fn non_permission_failure_maps_to_capture_failed() {
        assert_eq!(
            select_view(true, Some("device error"), PermissionState::Unknown),
            AppView::CaptureFailed {
                message: "device error".to_owned()
            }
        );
    }

    #[test]
    fn non_permission_failure_outranks_permission_state() {
        // A genuine device error shows the capture-health message even when the
        // permission machine is still Granted.
        assert_eq!(
            select_view(true, Some("device error"), PermissionState::Granted),
            AppView::CaptureFailed {
                message: "device error".to_owned()
            }
        );
    }

    #[test]
    fn unknown_maps_to_waiting_guidance() {
        assert_eq!(
            select_view(true, None, PermissionState::Unknown),
            AppView::PermissionGuidance {
                variant: GuidanceVariant::Waiting
            }
        );
    }

    #[test]
    fn denied_maps_to_full_guidance() {
        assert_eq!(
            select_view(true, None, PermissionState::Denied),
            AppView::PermissionGuidance {
                variant: GuidanceVariant::Denied
            }
        );
    }

    #[test]
    fn granted_with_healthy_capture_is_active() {
        assert_eq!(
            select_view(true, None, PermissionState::Granted),
            AppView::Active
        );
    }

    #[test]
    fn silence_while_granted_is_active_not_guidance() {
        // Genuine silence surfaces as Granted + no capture failure; we must NOT
        // pause or show guidance — the artifact settles on zeroed features.
        assert_eq!(
            select_view(true, None, PermissionState::Granted),
            AppView::Active
        );
    }
}
