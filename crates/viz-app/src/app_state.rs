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

/// Whether this platform gates system-audio capture behind a user-granted
/// permission. macOS does (the "Screen & System Audio Recording" TCC grant), so
/// the permission states render the consent/Settings guidance. Windows does not —
/// WASAPI loopback needs no consent — so there the same states render a neutral
/// "waiting for audio" hint instead (see [`select_view`]).
pub const AUDIO_PERMISSION_APPLIES: bool = cfg!(target_os = "macos");

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

/// Title of the neutral "waiting for audio" hint shown on platforms without an
/// audio-capture permission (Windows). It is informational, not an error: capture
/// is live and healthy, there simply is nothing playing yet.
pub const WAITING_FOR_AUDIO_TITLE: &str = "Waiting for audio";

/// The neutral, persistent, non-flashing body line shown on platforms without an
/// audio-capture permission (Windows) while no audio has been captured yet. No
/// consent or System-Settings language — just an invitation to play something.
pub const WAITING_FOR_AUDIO_HINT: &str = "Play some audio and the visualizer starts automatically.";

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
    /// Platforms without an audio-capture permission (Windows): capture is live
    /// and healthy but no audio has been observed yet. Renders a neutral,
    /// persistent, non-flashing "waiting for audio — play something" hint with no
    /// consent or System-Settings language. Replaces the permission guidance
    /// entirely on such platforms.
    WaitingForAudio,
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
/// * `permission` — the live [`PermissionState`] from the audio handle;
/// * `audio_permission_applies` — whether this platform gates capture behind a
///   user-granted permission ([`AUDIO_PERMISSION_APPLIES`]: `true` on macOS,
///   `false` on Windows).
///
/// Priority order: no-artifact → non-permission capture failure → the
/// permission/waiting state → visualization. There is no frame in which capture is
/// not yet producing audio and *some* explanatory screen is absent — the window
/// always says what it is doing.
///
/// **Platform-correct guidance.** On a platform that gates capture behind a
/// permission (`audio_permission_applies = true`, macOS), the non-granted states
/// map to the consent guidance screen: `Unknown → Waiting`, `Denied → Denied`
/// (with the Open-Settings button). On a platform with no such permission
/// (`audio_permission_applies = false`, Windows — WASAPI loopback needs no
/// consent), both non-granted states map instead to the neutral
/// [`AppView::WaitingForAudio`] hint — no consent or System-Settings language is
/// ever shown there.
///
/// A non-permission capture failure outranks the permission state: a genuine
/// device error should show the capture-health message even if the permission
/// machine has not yet promoted to `Granted`.
pub fn select_view(
    has_active_artifact: bool,
    capture_failure: Option<&str>,
    permission: PermissionState,
    audio_permission_applies: bool,
) -> AppView {
    if !has_active_artifact {
        return AppView::NoArtifacts;
    }
    if let Some(message) = capture_failure {
        return AppView::CaptureFailed {
            message: message.to_owned(),
        };
    }
    if permission == PermissionState::Granted {
        return AppView::Active;
    }
    // Not yet granted (Unknown grace window, or Denied). On platforms without an
    // audio-capture permission there is nothing to consent to — show the neutral
    // waiting hint instead of any consent/Settings copy.
    if !audio_permission_applies {
        return AppView::WaitingForAudio;
    }
    match permission {
        PermissionState::Unknown => AppView::PermissionGuidance {
            variant: GuidanceVariant::Waiting,
        },
        PermissionState::Denied => AppView::PermissionGuidance {
            variant: GuidanceVariant::Denied,
        },
        // Unreachable: Granted is handled above, but keep the match exhaustive.
        PermissionState::Granted => AppView::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macOS-style permission policy (capture gated behind a TCC grant), used
    /// independently of the host OS so the platform branches are testable anywhere.
    const PERMISSION: bool = true;
    /// The Windows-style policy (no audio-capture permission).
    const NO_PERMISSION: bool = false;

    #[test]
    fn no_artifact_takes_precedence() {
        // Even if capture failed / permission missing, with no artifact we show the
        // empty-library state.
        assert_eq!(
            select_view(false, None, PermissionState::Granted, PERMISSION),
            AppView::NoArtifacts
        );
        assert_eq!(
            select_view(false, Some("boom"), PermissionState::Denied, PERMISSION),
            AppView::NoArtifacts
        );
        // Same precedence on a platform without a permission concept.
        assert_eq!(
            select_view(false, None, PermissionState::Denied, NO_PERMISSION),
            AppView::NoArtifacts
        );
    }

    #[test]
    fn non_permission_failure_maps_to_capture_failed() {
        assert_eq!(
            select_view(
                true,
                Some("device error"),
                PermissionState::Unknown,
                PERMISSION
            ),
            AppView::CaptureFailed {
                message: "device error".to_owned()
            }
        );
        // A real device error still outranks the waiting hint on Windows too.
        assert_eq!(
            select_view(
                true,
                Some("device error"),
                PermissionState::Unknown,
                NO_PERMISSION
            ),
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
            select_view(
                true,
                Some("device error"),
                PermissionState::Granted,
                PERMISSION
            ),
            AppView::CaptureFailed {
                message: "device error".to_owned()
            }
        );
    }

    // --- macOS permission policy (audio_permission_applies = true) ----------

    #[test]
    fn unknown_maps_to_waiting_guidance() {
        assert_eq!(
            select_view(true, None, PermissionState::Unknown, PERMISSION),
            AppView::PermissionGuidance {
                variant: GuidanceVariant::Waiting
            }
        );
    }

    #[test]
    fn denied_maps_to_full_guidance() {
        assert_eq!(
            select_view(true, None, PermissionState::Denied, PERMISSION),
            AppView::PermissionGuidance {
                variant: GuidanceVariant::Denied
            }
        );
    }

    #[test]
    fn granted_with_healthy_capture_is_active() {
        assert_eq!(
            select_view(true, None, PermissionState::Granted, PERMISSION),
            AppView::Active
        );
    }

    #[test]
    fn silence_while_granted_is_active_not_guidance() {
        // Genuine silence surfaces as Granted + no capture failure; we must NOT
        // pause or show guidance — the artifact settles on zeroed features.
        assert_eq!(
            select_view(true, None, PermissionState::Granted, PERMISSION),
            AppView::Active
        );
    }

    // --- Windows policy (audio_permission_applies = false) ------------------

    #[test]
    fn windows_unknown_maps_to_waiting_for_audio_not_consent() {
        // On Windows there is no permission to ask for: the Unknown (grace) state
        // must surface the neutral waiting hint, never the consent guidance.
        assert_eq!(
            select_view(true, None, PermissionState::Unknown, NO_PERMISSION),
            AppView::WaitingForAudio
        );
    }

    #[test]
    fn windows_denied_maps_to_waiting_for_audio_not_settings() {
        // The permission machine can still land in Denied after the silent grace
        // window, but on Windows that must NOT show System-Settings/consent copy —
        // it is the same neutral "play something" hint.
        assert_eq!(
            select_view(true, None, PermissionState::Denied, NO_PERMISSION),
            AppView::WaitingForAudio
        );
    }

    #[test]
    fn windows_granted_is_active() {
        // Once audio flows, Windows shows the visualizer just like macOS.
        assert_eq!(
            select_view(true, None, PermissionState::Granted, NO_PERMISSION),
            AppView::Active
        );
    }

    #[test]
    fn windows_never_shows_permission_guidance() {
        // Exhaustive over the permission states: none of them yields the consent
        // guidance screen when the platform has no audio permission.
        for permission in [
            PermissionState::Unknown,
            PermissionState::Denied,
            PermissionState::Granted,
        ] {
            let view = select_view(true, None, permission, NO_PERMISSION);
            assert!(
                !matches!(view, AppView::PermissionGuidance { .. }),
                "Windows must never render permission consent guidance ({permission:?})"
            );
        }
    }

    #[test]
    fn platform_constant_matches_target() {
        assert_eq!(AUDIO_PERMISSION_APPLIES, cfg!(target_os = "macos"));
    }
}
