//! Render-state selection tests.
//!
//! Drives the **pure** [`select_view`] function across scripted
//! `(has_artifact, capture_failure, PermissionState)` timelines and asserts
//! exactly one expected screen variant per frame. No GUI, no window, no audio
//! hardware — the selection is verifiable in isolation, exercising render-state
//! selection over time (which no unit-level test covers on its own).
//!
//! Coverage:
//! * the priority order, frame by frame;
//! * the persistence regression — a simulated 5-minute unpermissioned timeline
//!   (Denied throughout, with watchdog rebuild events firing) never selects
//!   anything but the permission guidance;
//! * the ≤ 5 s appearance bounds at the machine level (Unknown→Denied at exactly
//!   the 5 s grace, Denied guidance from the first frame after);
//! * the full grant → revoke → grant cycle at the integrated level (the tracker
//!   feeding the selector).

use std::time::Duration;

use viz_app::app_state::{select_view, AppView, GuidanceVariant};
use viz_audio::permission::{Observation, PermissionTracker, GRACE_WINDOW};
use viz_audio::PermissionState;

/// The macOS-style permission policy (capture gated behind a TCC grant). These
/// tests exercise the permission-guidance behaviour, so they drive the selector in
/// the permission-applicable mode explicitly, independent of the host OS.
const PERMISSION: bool = true;

/// Convenience: select the view for a *permission* timeline (artifact present,
/// no non-permission capture failure — the common permission-handling case) under
/// the permission-applicable (macOS-style) policy.
fn view_for(permission: PermissionState) -> AppView {
    select_view(true, None, permission, PERMISSION)
}

// --- Priority order, frame by frame -----------------------------------------

#[test]
fn priority_no_artifact_beats_everything() {
    for p in [
        PermissionState::Unknown,
        PermissionState::Denied,
        PermissionState::Granted,
    ] {
        assert_eq!(
            select_view(false, None, p, PERMISSION),
            AppView::NoArtifacts
        );
        assert_eq!(
            select_view(false, Some("boom"), p, PERMISSION),
            AppView::NoArtifacts
        );
    }
}

#[test]
fn priority_non_permission_failure_beats_permission_state() {
    // A non-permission capture failure outranks the permission state, but never
    // the no-artifact state (covered above).
    for p in [
        PermissionState::Unknown,
        PermissionState::Denied,
        PermissionState::Granted,
    ] {
        assert_eq!(
            select_view(true, Some("device error"), p, PERMISSION),
            AppView::CaptureFailed {
                message: "device error".to_owned()
            }
        );
    }
}

#[test]
fn priority_unknown_is_waiting_guidance() {
    assert_eq!(
        view_for(PermissionState::Unknown),
        AppView::PermissionGuidance {
            variant: GuidanceVariant::Waiting
        }
    );
}

#[test]
fn priority_denied_is_full_guidance() {
    assert_eq!(
        view_for(PermissionState::Denied),
        AppView::PermissionGuidance {
            variant: GuidanceVariant::Denied
        }
    );
}

#[test]
fn priority_granted_is_active() {
    assert_eq!(view_for(PermissionState::Granted), AppView::Active);
}

// --- ≤ 5 s appearance bounds (Unknown→Denied at the grace) ------------------

#[test]
fn unknown_to_denied_at_exactly_the_grace_and_guidance_from_first_frame_after() {
    let mut tracker = PermissionTracker::new();

    // Before the grace: Unknown ⇒ waiting guidance (never blank).
    let p = tracker.observe(&Observation::silent(
        GRACE_WINDOW - Duration::from_millis(1),
    ));
    assert_eq!(
        view_for(p),
        AppView::PermissionGuidance {
            variant: GuidanceVariant::Waiting
        },
        "just before grace must be the waiting guidance, never blank"
    );

    // At exactly the 5 s grace: Denied ⇒ full guidance (the bound is inclusive).
    let p = tracker.observe(&Observation::silent(GRACE_WINDOW));
    assert_eq!(
        view_for(p),
        AppView::PermissionGuidance {
            variant: GuidanceVariant::Denied
        },
        "Denied guidance must be selected at exactly the 5 s grace boundary"
    );

    // Every frame after stays on the full guidance.
    for ms in [5_001u64, 5_250, 6_000, 10_000] {
        let p = tracker.observe(&Observation::silent(Duration::from_millis(ms)));
        assert_eq!(
            view_for(p),
            AppView::PermissionGuidance {
                variant: GuidanceVariant::Denied
            },
            "Denied guidance must persist at {ms} ms"
        );
    }
}

#[test]
fn unknown_to_granted_within_one_hop_keeps_no_guidance_flash_after_grant() {
    // A grant promotes within one observation; the very next frame is Active, no
    // lingering guidance.
    let mut tracker = PermissionTracker::new();
    let p = tracker.observe(&Observation::silent(Duration::from_millis(100)));
    assert!(matches!(view_for(p), AppView::PermissionGuidance { .. }));
    let p = tracker.observe(&Observation {
        saw_nonzero: true,
        elapsed_since_start: Duration::from_millis(120),
        sustained_zeros: false,
        output_device_running: true,
        rebuild_completed_still_zero: false,
    });
    assert_eq!(view_for(p), AppView::Active);
}

// --- 5-minute unpermissioned timeline never leaves guidance -----------------

#[test]
fn five_minute_unpermissioned_timeline_is_always_guidance() {
    // A realistic Denied session: the user never grants. The watchdog's 3 s
    // zero-window keeps firing rebuild events (sustained_zeros + the one-shot
    // rebuild-completed-still-zero), with the output device alternating between
    // running (music playing on another route) and idle (paused). Across the full
    // 5 minutes, at a 250 ms poll cadence, the selected screen MUST stay the
    // permission guidance for every single frame — zero flashes, zero blanks.
    // This is the regression guard: the guidance must never flicker away or blank.
    let mut tracker = PermissionTracker::new();
    let poll = Duration::from_millis(250);
    let total = Duration::from_secs(5 * 60);
    let mut elapsed = Duration::ZERO;
    let mut frame = 0u64;

    while elapsed < total {
        // Watchdog bookkeeping: a sustained zero window is reached early and stays
        // (the grant is missing, so capture is always zero). A rebuild event fires
        // roughly every ~3 s (every 12 polls).
        let sustained_zeros = elapsed >= Duration::from_secs(3);
        let rebuild_completed_still_zero = sustained_zeros && frame.is_multiple_of(12);
        // Output device flips state every ~10 s to exercise both branches.
        let output_device_running = (elapsed.as_secs() / 10).is_multiple_of(2);

        let obs = Observation {
            saw_nonzero: false, // never granted ⇒ always zero
            elapsed_since_start: elapsed,
            sustained_zeros,
            output_device_running,
            rebuild_completed_still_zero,
        };
        let p = tracker.observe(&obs);

        // Before the grace it is Unknown (waiting), after it is Denied (full) — both
        // are the permission guidance screen. The invariant under test is that it is
        // ALWAYS one of those two — never Active, never blank, never CaptureFailed.
        let view = view_for(p);
        assert!(
            matches!(view, AppView::PermissionGuidance { .. }),
            "frame {frame} at {:?}: expected permission guidance, got {view:?}",
            elapsed
        );

        elapsed += poll;
        frame += 1;
    }

    // After the whole timeline the machine is firmly Denied (grace long elapsed,
    // never granted).
    assert_eq!(tracker.state(), PermissionState::Denied);
}

// --- grant → revoke → grant cycle through the selector ----------------------

#[test]
fn grant_revoke_grant_cycle_selects_correct_screen_each_phase() {
    let mut tracker = PermissionTracker::new();

    // Phase 1: grant — music starts flowing → Active.
    let p = tracker.observe(&Observation {
        saw_nonzero: true,
        elapsed_since_start: Duration::from_millis(50),
        sustained_zeros: false,
        output_device_running: true,
        rebuild_completed_still_zero: false,
    });
    assert_eq!(view_for(p), AppView::Active);

    // Phase 2: revoke — sustained zeros + device running + rebuild-still-zero →
    // Denied full guidance (within the 5 s budget: 3 s window + rebuild + verdict).
    let p = tracker.observe(&Observation {
        saw_nonzero: false,
        elapsed_since_start: Duration::from_secs(10),
        sustained_zeros: true,
        output_device_running: true,
        rebuild_completed_still_zero: true,
    });
    assert_eq!(
        view_for(p),
        AppView::PermissionGuidance {
            variant: GuidanceVariant::Denied
        }
    );

    // Phase 3: re-grant — first non-zero hop after the user re-enables → Active,
    // no restart needed.
    let p = tracker.observe(&Observation {
        saw_nonzero: true,
        elapsed_since_start: Duration::from_secs(15),
        sustained_zeros: false,
        output_device_running: true,
        rebuild_completed_still_zero: false,
    });
    assert_eq!(view_for(p), AppView::Active);
}

// --- Windows policy: no audio permission, neutral waiting hint --------------

/// The Windows-style policy: capture is not gated behind any user permission, so
/// the non-granted states must surface the neutral waiting hint, never consent.
const NO_PERMISSION: bool = false;

#[test]
fn windows_unknown_and_denied_select_waiting_for_audio_never_consent() {
    // Drive the same tracker timeline that lands in Unknown then Denied on macOS,
    // but select under the Windows policy: every non-granted frame is the neutral
    // WaitingForAudio hint — the consent guidance never appears.
    let mut tracker = PermissionTracker::new();

    // Before the grace: Unknown.
    let p = tracker.observe(&Observation::silent(
        GRACE_WINDOW - Duration::from_millis(1),
    ));
    assert_eq!(p, PermissionState::Unknown);
    assert_eq!(
        select_view(true, None, p, NO_PERMISSION),
        AppView::WaitingForAudio
    );

    // At/after the grace: Denied — still the neutral hint on Windows.
    let p = tracker.observe(&Observation::silent(GRACE_WINDOW));
    assert_eq!(p, PermissionState::Denied);
    assert_eq!(
        select_view(true, None, p, NO_PERMISSION),
        AppView::WaitingForAudio
    );
}

#[test]
fn windows_first_audio_promotes_to_active() {
    // The first non-zero sample promotes to Granted; on Windows that is Active,
    // exactly like macOS — only the *non-granted* copy differs by platform.
    let mut tracker = PermissionTracker::new();
    let p = tracker.observe(&Observation::silent(Duration::from_millis(100)));
    assert_eq!(
        select_view(true, None, p, NO_PERMISSION),
        AppView::WaitingForAudio
    );
    let p = tracker.observe(&Observation {
        saw_nonzero: true,
        elapsed_since_start: Duration::from_millis(120),
        sustained_zeros: false,
        output_device_running: true,
        rebuild_completed_still_zero: false,
    });
    assert_eq!(select_view(true, None, p, NO_PERMISSION), AppView::Active);
}

#[test]
fn granted_with_plain_silence_stays_active_never_flips_to_guidance() {
    // The "music not playing while granted" case: long silence with an idle
    // output device must remain Active (idle visuals), never guidance.
    let mut tracker = PermissionTracker::new();
    let p = tracker.observe(&Observation {
        saw_nonzero: true,
        elapsed_since_start: Duration::from_millis(50),
        sustained_zeros: false,
        output_device_running: true,
        rebuild_completed_still_zero: false,
    });
    assert_eq!(view_for(p), AppView::Active);

    for secs in [5u64, 30, 120, 300] {
        // Sustained silence + rebuild events, but device idle ⇒ genuine silence.
        let p = tracker.observe(&Observation {
            saw_nonzero: false,
            elapsed_since_start: Duration::from_secs(secs),
            sustained_zeros: true,
            output_device_running: false,
            rebuild_completed_still_zero: secs.is_multiple_of(2),
        });
        assert_eq!(
            view_for(p),
            AppView::Active,
            "plain silence at {secs}s must stay Active, not guidance"
        );
    }
}
