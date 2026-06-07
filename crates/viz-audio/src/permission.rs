//! Pure permission-state machine for audio capture.
//!
//! macOS has **no public status query** for the "System Audio Recording Only"
//! grant (`kTCCServiceAudioCapture`); the only clean signal is *attempt then
//! observe*. This module turns the app's observable signals — the
//! first non-zero captured sample, an elapsed-since-start clock, the output
//! device's "running somewhere" flag, and the result of a tap rebuild — into a
//! three-state machine the render loop reads each frame:
//!
//! ```text
//!             start pipeline (launch)
//!                      │
//!                  ┌───▼────┐   first non-zero sample (any time)
//!                  │ Unknown├────────────────────────────────────┐
//!                  └───┬────┘                                     │
//!    5 s grace elapsed │ (still all-zero)                        │
//!                  ┌───▼────┐   first non-zero sample        ┌───▼────┐
//!                  │ Denied ├───────────────────────────────►│ Granted│ (sticky)
//!                  └───▲────┘                                └───┬────┘
//!                      │  revocation verdict: sustained zeros    │
//!                      │  AND output device running somewhere    │
//!                      └─────────── AND post-rebuild still zero ─┘
//! ```
//!
//! The machine is **pure**: it has no Core Audio imports and no clock of its own.
//! The threaded watchdog feeds it observations; every transition is logged once
//! via `tracing`. It is exhaustively unit-tested with scripted inputs,
//! including the flicker regression (a rebuild cycle while `Granted` and silent
//! must never demote or bounce) and the silence-vs-revocation discrimination.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The app's live view of the audio-capture permission.
///
/// Derived purely from observable signals — there is no private SPI involved.
/// Exactly one on-screen state maps to each variant: `Unknown`/`Denied` ⇒ the
/// persistent guidance screen (different variant text), `Granted` ⇒ the
/// visualization (idle on silence).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PermissionState {
    /// Pipeline started, no non-zero sample yet, grace window (5 s) still running.
    /// The OS dialog may be on screen right now.
    #[default]
    Unknown,
    /// Grace elapsed all-zero, or a revocation verdict was reached. Capture is
    /// permission-unavailable → the full guidance screen with the Open-Settings
    /// button is shown.
    Denied,
    /// At least one non-zero sample has been observed this session. **Sticky**:
    /// plain silence (a pause, an idle output device) never demotes it — only the
    /// three-condition revocation verdict can.
    Granted,
}

impl PermissionState {
    /// Encodes the state as a `u8` for the lock-free [`PermissionCell`].
    const fn as_u8(self) -> u8 {
        match self {
            PermissionState::Unknown => 0,
            PermissionState::Denied => 1,
            PermissionState::Granted => 2,
        }
    }

    /// Decodes a [`PermissionCell`] byte (defaults unknown bytes to `Unknown`).
    const fn from_u8(v: u8) -> Self {
        match v {
            2 => PermissionState::Granted,
            1 => PermissionState::Denied,
            _ => PermissionState::Unknown,
        }
    }
}

/// A lock-free, shared cell holding the latest [`PermissionState`]. The watchdog
/// thread publishes via [`PermissionCell::set`]; the render thread reads via
/// [`PermissionCell::get`] every frame (cheap relaxed atomic load — off the audio
/// hot path entirely).
#[derive(Clone, Debug)]
pub struct PermissionCell {
    state: Arc<AtomicU8>,
}

impl Default for PermissionCell {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionCell {
    /// New cell in the [`PermissionState::Unknown`] state.
    pub fn new() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(PermissionState::Unknown.as_u8())),
        }
    }

    /// Publishes the latest state (watchdog thread).
    pub fn set(&self, state: PermissionState) {
        self.state.store(state.as_u8(), Ordering::Relaxed);
    }

    /// Reads the latest state (render thread; cheap).
    pub fn get(&self) -> PermissionState {
        PermissionState::from_u8(self.state.load(Ordering::Relaxed))
    }
}

/// The grace window for `Unknown → Denied`: if no non-zero sample arrives within
/// this window after the pipeline starts, the permission is treated as missing
/// (the guidance screen must appear within 5 s of a missing grant).
pub const GRACE_WINDOW: Duration = Duration::from_secs(5);

/// One observation snapshot fed to [`PermissionTracker::observe`]. Assembled by
/// the watchdog thread from the per-hop atomic, its own clock, and the cidre
/// device-running poll; `rebuild_completed_still_zero` is a one-shot event set on
/// the poll right after a tap rebuild finished and still produced zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// True if **any** non-zero sample has been seen since the pipeline started
    /// (cumulative — the per-hop atomic latches on the first non-zero hop). The
    /// first-ever `true` is the unambiguous "granted" signal.
    pub saw_nonzero: bool,
    /// Wall-clock elapsed since the capture pipeline started. Drives the
    /// `Unknown → Denied` grace timeout.
    pub elapsed_since_start: Duration,
    /// True while capture has shown a sustained zero window (the existing 3 s
    /// watchdog zero-buffer detection). One of the three revocation conditions.
    pub sustained_zeros: bool,
    /// True if the system **output** device is running in some process
    /// (`kAudioDevicePropertyDeviceIsRunningSomewhere`). Discriminates genuine
    /// silence (device idle ⇒ stay `Granted`) from revocation (device running yet
    /// our tap is silent). One of the three revocation conditions.
    ///
    /// **Accepted false-revocation risk (MINOR-2).** This flag reflects *any*
    /// process on the output device, not ours. So a third-party process holding
    /// the output device, plus our own genuine silence, plus an (eventually)
    /// completed rebuild still yielding zeros, can demote a true `Granted` to
    /// `Denied` spuriously. Accepted: it needs a rare coincidence, the guidance is
    /// harmless, and the first real non-zero sample re-promotes to `Granted`
    /// instantly (rule 3) — the misfire self-heals as soon as audio flows again.
    pub output_device_running: bool,
    /// One-shot event: a tap+aggregate rebuild has just completed and capture is
    /// *still* zero. The third revocation condition — it guards against the macOS
    /// 14.x all-zero tap bug by demanding a rebuild before declaring
    /// revocation. Only meaningful in the same observation that reports it.
    pub rebuild_completed_still_zero: bool,
}

impl Observation {
    /// A convenience constructor for the common "still all-zero, nothing else
    /// happening" observation (used heavily in tests and the grace path).
    pub fn silent(elapsed_since_start: Duration) -> Self {
        Self {
            saw_nonzero: false,
            elapsed_since_start,
            sustained_zeros: false,
            output_device_running: false,
            rebuild_completed_still_zero: false,
        }
    }
}

/// The pure permission policy: holds the current [`PermissionState`] and advances
/// it from [`Observation`]s. No clock, no Core Audio — the caller supplies all
/// timing and signals, so the whole machine is deterministic under unit tests.
///
/// Transitions are logged once on change; non-transitions never log (no spam
/// while the user repeatedly retries).
#[derive(Debug)]
pub struct PermissionTracker {
    state: PermissionState,
}

impl Default for PermissionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionTracker {
    /// New tracker in the [`PermissionState::Unknown`] state (the pipeline has
    /// just started; the grace window is running).
    pub fn new() -> Self {
        Self {
            state: PermissionState::Unknown,
        }
    }

    /// The current permission state (cheap copy).
    pub fn state(&self) -> PermissionState {
        self.state
    }

    /// Feeds one [`Observation`] and returns the (possibly unchanged) state.
    ///
    /// Implements the five transition rules exactly:
    /// 1. `Unknown → Granted` on the first non-zero sample.
    /// 2. `Unknown → Denied` once the 5 s grace elapses with no non-zero sample.
    /// 3. `Denied → Granted` on the first non-zero sample (re-grant; no restart).
    /// 4. `Granted → Denied` only when **all three** revocation conditions hold:
    ///    sustained zeros, output device running somewhere, and a completed
    ///    rebuild that is still zero. A plain pause/silence never demotes.
    /// 5. `Granted` stays `Granted` on silence with an idle output device.
    pub fn observe(&mut self, obs: &Observation) -> PermissionState {
        let next = match self.state {
            PermissionState::Unknown => {
                if obs.saw_nonzero {
                    // Rule 1: first non-zero sample promotes immediately.
                    PermissionState::Granted
                } else if obs.elapsed_since_start >= GRACE_WINDOW {
                    // Rule 2: grace elapsed all-zero ⇒ treat as missing.
                    PermissionState::Denied
                } else {
                    PermissionState::Unknown
                }
            }
            PermissionState::Denied => {
                if obs.saw_nonzero {
                    // Rule 3: a grant (dialog Allow or Settings) recovers us.
                    PermissionState::Granted
                } else {
                    PermissionState::Denied
                }
            }
            PermissionState::Granted => {
                // Rule 4: revocation needs ALL THREE conditions. Sticky-Granted
                // otherwise (rule 5): plain silence — even a rebuild cycle — never
                // demotes. `saw_nonzero` being cumulative means once granted we
                // only leave on the explicit revocation verdict.
                if obs.sustained_zeros
                    && obs.output_device_running
                    && obs.rebuild_completed_still_zero
                {
                    PermissionState::Denied
                } else {
                    PermissionState::Granted
                }
            }
        };

        if next != self.state {
            log_transition(self.state, next, obs);
            self.state = next;
        }
        self.state
    }
}

/// Logs a single state transition with the from→to pair and the triggering
/// signal. English, one line per change (no spam on non-transitions).
fn log_transition(from: PermissionState, to: PermissionState, obs: &Observation) {
    match (from, to) {
        (PermissionState::Unknown, PermissionState::Granted)
        | (PermissionState::Denied, PermissionState::Granted) => {
            tracing::info!(
                ?from,
                ?to,
                "permission state: granted (first non-zero sample observed — capture is live)"
            );
        }
        (PermissionState::Unknown, PermissionState::Denied) => {
            tracing::warn!(
                ?from,
                ?to,
                grace_secs = GRACE_WINDOW.as_secs(),
                "permission state: denied (grace window elapsed with no audio — \
                 the System Audio Recording grant is missing)"
            );
        }
        (PermissionState::Granted, PermissionState::Denied) => {
            tracing::warn!(
                ?from,
                ?to,
                sustained_zeros = obs.sustained_zeros,
                output_device_running = obs.output_device_running,
                rebuild_completed_still_zero = obs.rebuild_completed_still_zero,
                "permission state: revoked (sustained silence while the output device \
                 is active and a rebuilt tap is still silent)"
            );
        }
        // Other pairs are not reachable (the machine only emits the above).
        _ => {
            tracing::info!(?from, ?to, "permission state changed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonzero(elapsed: Duration) -> Observation {
        Observation {
            saw_nonzero: true,
            elapsed_since_start: elapsed,
            sustained_zeros: false,
            output_device_running: false,
            rebuild_completed_still_zero: false,
        }
    }

    /// The full three-condition revocation observation.
    fn revocation(elapsed: Duration) -> Observation {
        Observation {
            saw_nonzero: false,
            elapsed_since_start: elapsed,
            sustained_zeros: true,
            output_device_running: true,
            rebuild_completed_still_zero: true,
        }
    }

    #[test]
    fn starts_unknown() {
        let t = PermissionTracker::new();
        assert_eq!(t.state(), PermissionState::Unknown);
    }

    // --- Rule 1: Unknown → Granted on first non-zero ------------------------

    #[test]
    fn unknown_to_granted_on_first_nonzero() {
        let mut t = PermissionTracker::new();
        // Well within the grace window, audio flows.
        let s = t.observe(&nonzero(Duration::from_millis(20)));
        assert_eq!(s, PermissionState::Granted);
    }

    #[test]
    fn unknown_to_granted_beats_the_grace_deadline_when_nonzero_arrives_late() {
        let mut t = PermissionTracker::new();
        // Still silent just under the deadline.
        assert_eq!(
            t.observe(&Observation::silent(Duration::from_millis(4_900))),
            PermissionState::Unknown
        );
        // A non-zero sample at exactly the deadline still promotes (rule 1 has
        // priority over rule 2 within the same observation).
        assert_eq!(t.observe(&nonzero(GRACE_WINDOW)), PermissionState::Granted);
    }

    // --- Rule 2: Unknown → Denied at the grace boundary ---------------------

    #[test]
    fn unknown_stays_unknown_before_grace_elapses() {
        let mut t = PermissionTracker::new();
        for ms in [0u64, 100, 1_000, 2_500, 4_999] {
            assert_eq!(
                t.observe(&Observation::silent(Duration::from_millis(ms))),
                PermissionState::Unknown,
                "should still be Unknown at {ms} ms"
            );
        }
    }

    #[test]
    fn unknown_to_denied_at_exactly_the_grace_window() {
        let mut t = PermissionTracker::new();
        // Just before: Unknown.
        assert_eq!(
            t.observe(&Observation::silent(
                GRACE_WINDOW - Duration::from_millis(1)
            )),
            PermissionState::Unknown
        );
        // At exactly 5 s: Denied (bound is inclusive — `>=`).
        assert_eq!(
            t.observe(&Observation::silent(GRACE_WINDOW)),
            PermissionState::Denied
        );
    }

    // --- Rule 3: Denied → Granted on first non-zero (re-grant) --------------

    #[test]
    fn denied_to_granted_on_first_nonzero() {
        let mut t = PermissionTracker::new();
        t.observe(&Observation::silent(GRACE_WINDOW)); // → Denied
        assert_eq!(t.state(), PermissionState::Denied);
        // User enables in Settings; audio flows → instant re-promotion.
        assert_eq!(
            t.observe(&nonzero(Duration::from_secs(30))),
            PermissionState::Granted
        );
    }

    #[test]
    fn denied_stays_denied_while_silent() {
        let mut t = PermissionTracker::new();
        t.observe(&Observation::silent(GRACE_WINDOW)); // → Denied
        for secs in [6u64, 10, 60, 300] {
            assert_eq!(
                t.observe(&Observation::silent(Duration::from_secs(secs))),
                PermissionState::Denied
            );
        }
    }

    // --- Rule 4: Granted → Denied needs ALL THREE conditions ----------------

    #[test]
    fn granted_to_denied_only_with_full_revocation_verdict() {
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20))); // → Granted
        assert_eq!(
            t.observe(&revocation(Duration::from_secs(10))),
            PermissionState::Denied
        );
    }

    #[test]
    fn granted_survives_sustained_zeros_alone() {
        // Only one of the three conditions: sustained zeros (a long pause).
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20)));
        let obs = Observation {
            saw_nonzero: false,
            elapsed_since_start: Duration::from_secs(10),
            sustained_zeros: true,
            output_device_running: false,
            rebuild_completed_still_zero: false,
        };
        assert_eq!(t.observe(&obs), PermissionState::Granted);
    }

    #[test]
    fn granted_survives_zeros_with_idle_output_device() {
        // Sustained zeros + rebuild-still-zero, but the output device is IDLE
        // (genuine silence — the output-device-running discriminator). Must stay Granted.
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20)));
        let obs = Observation {
            saw_nonzero: false,
            elapsed_since_start: Duration::from_secs(10),
            sustained_zeros: true,
            output_device_running: false,
            rebuild_completed_still_zero: true,
        };
        assert_eq!(t.observe(&obs), PermissionState::Granted);
    }

    #[test]
    fn granted_survives_running_device_without_rebuild_evidence() {
        // Sustained zeros + device running, but no completed-rebuild-still-zero
        // event yet (the 14.x all-zero tap bug guard). Must stay Granted until the
        // rebuild confirms.
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20)));
        let obs = Observation {
            saw_nonzero: false,
            elapsed_since_start: Duration::from_secs(10),
            sustained_zeros: true,
            output_device_running: true,
            rebuild_completed_still_zero: false,
        };
        assert_eq!(t.observe(&obs), PermissionState::Granted);
    }

    // --- Rule 5 / flicker regression ----------------------------------------

    #[test]
    fn granted_is_sticky_through_plain_silence() {
        // The canonical "music not playing while granted" case: long silence, no
        // revocation conditions ⇒ stay Granted (idle visuals continue).
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20)));
        for secs in [1u64, 5, 30, 120, 300] {
            assert_eq!(
                t.observe(&Observation::silent(Duration::from_secs(secs))),
                PermissionState::Granted,
                "Granted must survive {secs} s of plain silence"
            );
        }
    }

    #[test]
    fn flicker_regression_rebuild_cycle_while_granted_silent_never_bounces() {
        // Regression for root cause R1.2: a rebuild cycle while Granted+silent
        // must NOT demote or bounce the state. Here we simulate the watchdog's
        // 3 s-zero → rebuild loop firing repeatedly while music is merely paused
        // (output device idle), exactly the scenario that produced "the image
        // that disappears". The state must remain Granted for every observation.
        let mut t = PermissionTracker::new();
        t.observe(&nonzero(Duration::from_millis(20))); // music started → Granted

        for cycle in 0..20u64 {
            let base = Duration::from_secs(5 + cycle * 4);
            // Sustained zeros accumulate (paused), device idle (nobody playing).
            assert_eq!(
                t.observe(&Observation {
                    saw_nonzero: false,
                    elapsed_since_start: base,
                    sustained_zeros: true,
                    output_device_running: false,
                    rebuild_completed_still_zero: false,
                }),
                PermissionState::Granted
            );
            // Rebuild completes, still zero — but the device is idle, so this is
            // genuine silence, not revocation.
            assert_eq!(
                t.observe(&Observation {
                    saw_nonzero: false,
                    elapsed_since_start: base + Duration::from_millis(500),
                    sustained_zeros: true,
                    output_device_running: false,
                    rebuild_completed_still_zero: true,
                }),
                PermissionState::Granted,
                "rebuild-still-zero on an idle device must not demote (cycle {cycle})"
            );
        }
    }

    #[test]
    fn revocation_then_regrant_full_cycle() {
        // Full revoke-then-regrant at the machine level: Granted → Denied
        // (revocation) → Granted (re-grant), no bouncing in between.
        let mut t = PermissionTracker::new();
        assert_eq!(
            t.observe(&nonzero(Duration::from_millis(20))),
            PermissionState::Granted
        );
        // Revoke (all three conditions).
        assert_eq!(
            t.observe(&revocation(Duration::from_secs(20))),
            PermissionState::Denied
        );
        // Still revoked while silent.
        assert_eq!(
            t.observe(&Observation::silent(Duration::from_secs(25))),
            PermissionState::Denied
        );
        // Re-grant: audio flows again.
        assert_eq!(
            t.observe(&nonzero(Duration::from_secs(30))),
            PermissionState::Granted
        );
    }

    #[test]
    fn five_consecutive_grant_revoke_cycles_are_stable() {
        // At least 5 grant→revoke→grant cycles, each landing correctly.
        let mut t = PermissionTracker::new();
        let mut elapsed = Duration::from_secs(0);
        for cycle in 0..5 {
            elapsed += Duration::from_secs(1);
            assert_eq!(
                t.observe(&nonzero(elapsed)),
                PermissionState::Granted,
                "grant {cycle}"
            );
            elapsed += Duration::from_secs(3);
            assert_eq!(
                t.observe(&revocation(elapsed)),
                PermissionState::Denied,
                "revoke {cycle}"
            );
        }
    }

    #[test]
    fn genuine_silence_at_first_launch_lands_in_denied() {
        // Rule 2 note: genuine silence at launch lands in Denied — acceptable, the
        // guidance asks for the (in fact ungranted) permission; the OS dialog is
        // likely on screen. After 5 s of silence we are Denied even if the user
        // simply was not playing audio yet.
        let mut t = PermissionTracker::new();
        assert_eq!(
            t.observe(&Observation::silent(Duration::from_secs(6))),
            PermissionState::Denied
        );
    }
}
