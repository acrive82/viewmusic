//! Capture-health watchdog.
//!
//! macOS process taps occasionally degrade to all-zero buffers on long sessions
//! (the 14.x all-zero tap bug, Apple Forums #825780); they can also fall silent
//! if the "System Audio Recording" grant is revoked mid-session. The watchdog
//! distinguishes *genuine silence* (healthy tap, no signal) from *capture failure*
//! and drives a small state machine:
//!
//! ```text
//! Active ──(zero RMS for ~3 s while running)──▶ rebuild tap+aggregate (full teardown)
//!   ▲                                              │ ok
//!   └──────────────────────────────────────────────┘
//!        rebuild error ▶ Failed{message} ▶ retry with backoff (2,4,8..30 s)
//! ```
//!
//! This module is the **pure policy** half ([`WatchdogPolicy`], backoff helpers,
//! [`CaptureHealth`]); the *threaded driver that actually owns and rebuilds the
//! live tap* lives in [`crate::pipeline`]. The real fix for the 14.x all-zero bug
//! is a **full teardown + restart** of the tap+aggregate device; the driver drops
//! the live [`crate::tap::CaptureSource`] and calls [`crate::tap::start`] again. A
//! completed-rebuild-that-is-still-zero is also the third revocation condition for
//! the permission machine.
//!
//! **Platform-aware idle policy.** Whether sustained zero buffers while running are
//! a *fault* depends on the backend (see [`TREAT_IDLE_AS_FAULT`]). The macOS process
//! tap keeps delivering buffers, so all-zero hops signal the bug or a revoked grant
//! — a fault worth rebuilding. WASAPI loopback on Windows delivers **no packets at
//! all** during silence; that idle is normal, not broken, so the Windows policy never
//! schedules a rebuild for pure silence. There the only rebuild trigger is an
//! **explicit device change** (a new default endpoint), pulsed through
//! [`CaptureHealth::note_device_changed`] and forced through
//! [`WatchdogPolicy::observe`]'s `device_changed` input regardless of the idle policy.
//!
//! The watchdog runs on its own thread; it never touches the IOProc hot path. It
//! observes capture liveness through an atomic "frames seen with non-zero RMS"
//! signal published by the analyzer side, and exposes [`CaptureState`] via a
//! shared [`CaptureHealth`] handle that the app polls.
//!
//! **False-revocation limitation (accepted risk).** The permission
//! machine's silence-vs-revocation discriminator reads
//! `kAudioDevicePropertyDeviceIsRunningSomewhere`
//! ([`crate::tap::output_device_running_somewhere`]), which reflects **any**
//! process on the output device — not specifically ours. A third-party process
//! holding the output device, combined with our own genuine silence and an
//! (eventually) completed rebuild that is still zero, can therefore demote a true
//! `Granted` state to `Denied` spuriously. We accept this: it needs a rare
//! three-way coincidence, the resulting guidance is harmless, and the **first**
//! real non-zero sample re-promotes to `Granted` instantly, so the misfire
//! self-heals the moment our capture produces audio again.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::tap::CaptureError;

/// Capture-health state exposed to the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    /// Capture is healthy (may be feeding zeroed features during genuine silence).
    Active,
    /// Capture failed and could not be rebuilt; the message carries user guidance.
    Failed {
        /// English, user-facing explanation plus System Settings path.
        message: String,
    },
}

/// Zero-RMS duration that triggers a rebuild while capture claims to be running.
pub const ZERO_BUFFER_REBUILD_SECS: f64 = 3.0;

/// Backoff schedule for rebuild retries after a failure (seconds), capped at 30.
const BACKOFF_START_SECS: u64 = 2;
const BACKOFF_CAP_SECS: u64 = 30;

/// Shared, lock-light capture-health signal.
///
/// The analyzer side calls [`CaptureHealth::note_rms`] each hop (or
/// [`CaptureHealth::note_activity`] for a boolean). The watchdog reads the
/// atomics; the published [`CaptureState`] is behind a `Mutex` read only off the
/// hot path.
#[derive(Clone)]
pub struct CaptureHealth {
    inner: Arc<Inner>,
}

struct Inner {
    /// Monotonic counter of hops observed to carry non-zero signal.
    nonzero_hops: AtomicU64,
    /// Monotonic counter of all hops observed.
    total_hops: AtomicU64,
    /// True while the capture backend believes it is running.
    running: AtomicBool,
    /// Sticky latch: set `true` the first time any non-zero sample is observed
    /// this session (the unambiguous "permission granted" signal). The audio
    /// thread does a single relaxed store; the watchdog
    /// reads it to feed [`crate::permission::Observation::saw_nonzero`]. Zero
    /// hot-path cost beyond one branch + relaxed store on active hops.
    saw_nonzero: AtomicBool,
    /// Edge-triggered "the audio device changed" request. Pulsed `true` by a
    /// device-change notification (the Windows `IMMNotificationClient` default-device
    /// / device-state callback) and consumed once by the watchdog, which then
    /// rebuilds the capture against the new default endpoint. Backend-agnostic: any
    /// platform that can detect an endpoint change may pulse it. A single relaxed
    /// store from the (trivial) notification callback; the watchdog swaps it back to
    /// `false` when it acts. See [`CaptureHealth::note_device_changed`].
    device_changed: AtomicBool,
    /// Published state (read off the hot path only).
    state: Mutex<CaptureState>,
}

impl Default for CaptureHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureHealth {
    /// Creates a health handle in the [`CaptureState::Active`] state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                nonzero_hops: AtomicU64::new(0),
                total_hops: AtomicU64::new(0),
                running: AtomicBool::new(true),
                saw_nonzero: AtomicBool::new(false),
                device_changed: AtomicBool::new(false),
                state: Mutex::new(CaptureState::Active),
            }),
        }
    }

    /// Records one hop's RMS (audio thread). Lock-free; cheap.
    #[inline]
    pub fn note_rms(&self, rms: f32) {
        self.note_activity(rms > 1.0e-6);
    }

    /// Records one hop's activity boolean (audio thread). Lock-free.
    #[inline]
    pub fn note_activity(&self, nonzero: bool) {
        self.inner.total_hops.fetch_add(1, Ordering::Relaxed);
        if nonzero {
            self.inner.nonzero_hops.fetch_add(1, Ordering::Relaxed);
            // Latch the sticky "ever saw audio" flag for the permission machine.
            // A single relaxed store; no read-modify-write.
            self.inner.saw_nonzero.store(true, Ordering::Relaxed);
        }
    }

    /// Whether any non-zero sample has been observed this session (the sticky
    /// "granted" latch). Read off the hot path by the watchdog.
    pub fn saw_nonzero(&self) -> bool {
        self.inner.saw_nonzero.load(Ordering::Relaxed)
    }

    /// Requests a capture rebuild because the audio device changed.
    ///
    /// This is the explicit, backend-agnostic device-change trigger. The Windows
    /// `IMMNotificationClient` calls it from its (deliberately trivial) COM callback
    /// when the default render endpoint or a device's state changes; the watchdog
    /// thread consumes the request via [`CaptureHealth::take_device_changed`] and
    /// performs the actual teardown + rebuild off the COM thread. The request is
    /// edge-triggered (one pending rebuild regardless of how many notifications
    /// arrive before the watchdog acts) and is independent of the idle-silence
    /// policy, so it fires even on a platform where pure silence is healthy.
    ///
    /// Lock-free: a single relaxed store. Safe to call from any thread, including a
    /// COM notification callback that must do as little as possible.
    #[inline]
    pub fn note_device_changed(&self) {
        self.inner.device_changed.store(true, Ordering::Relaxed);
    }

    /// Consumes a pending device-change rebuild request, returning `true` exactly
    /// once per pulse. Called by the watchdog thread each poll; on `true` it forces a
    /// rebuild via [`WatchdogPolicy::observe`]'s `device_changed` input regardless of
    /// the idle policy. Lock-free (a relaxed swap).
    pub fn take_device_changed(&self) -> bool {
        self.inner.device_changed.swap(false, Ordering::Relaxed)
    }

    /// Sets whether the backend currently believes it is running.
    pub fn set_running(&self, running: bool) {
        self.inner.running.store(running, Ordering::Relaxed);
    }

    /// Returns whether the backend believes it is running.
    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Relaxed)
    }

    /// Snapshot of `(total_hops, nonzero_hops)`.
    pub fn counters(&self) -> (u64, u64) {
        (
            self.inner.total_hops.load(Ordering::Relaxed),
            self.inner.nonzero_hops.load(Ordering::Relaxed),
        )
    }

    /// Reads the current published [`CaptureState`] (off hot path).
    pub fn state(&self) -> CaptureState {
        self.inner
            .state
            .lock()
            .expect("health mutex poisoned")
            .clone()
    }

    /// Publishes a new [`CaptureState`], logging the transition (English).
    pub fn set_state(&self, next: CaptureState) {
        let mut guard = self.inner.state.lock().expect("health mutex poisoned");
        if *guard != next {
            match &next {
                CaptureState::Active => tracing::info!("capture state: Active"),
                CaptureState::Failed { message } => {
                    tracing::error!(%message, "capture state: Failed")
                }
            }
            *guard = next;
        }
    }
}

/// The decision the watchdog reaches after observing the health counters for a
/// window. Pure and unit-testable; separated from the threaded driver below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// Capture looks healthy (or genuinely silent) — no action.
    Healthy,
    /// Zero buffers persisted while running — rebuild the tap + aggregate device.
    NeedsRebuild,
}

/// Whether sustained zero buffers *while the backend is running* should be treated
/// as a capture fault that schedules a rebuild.
///
/// On macOS this is `true`: the process tap is expected to keep delivering buffers
/// (zero-filled during real silence), so a sustained run of all-zero hops is the
/// signature of the 14.x all-zero tap bug or a revoked grant — a genuine fault that
/// the watchdog repairs with a full teardown + restart.
///
/// On Windows it is `false`: WASAPI loopback delivers **no packets at all** while
/// nothing is rendering ("when nothing is playing, there is nothing to capture").
/// The backend keeps the analyzer clock advancing with zero-fill, but the capture
/// itself is idle, not broken. Treating that idle silence as a fault would spin the
/// rebuild backoff for the entire time the user is not playing audio. On Windows the
/// only thing that warrants a rebuild is an *explicit* device change (see
/// [`WatchdogPolicy::observe`]'s `device_changed` input), which the
/// `IMMNotificationClient` pulses through [`CaptureHealth::note_device_changed`].
pub const TREAT_IDLE_AS_FAULT: bool = cfg!(target_os = "macos");

/// Pure watchdog policy. Tracks how long the capture has shown zero signal while
/// claiming to run, and decides when a rebuild is warranted. The threaded driver
/// in `pipeline` wraps this with timing and rebuild execution.
pub struct WatchdogPolicy {
    /// Last observed nonzero-hop count.
    last_nonzero: u64,
    /// Wall-clock instant of the last observed nonzero hop (or start).
    last_activity: Instant,
    /// Rebuild trigger threshold.
    zero_window: Duration,
    /// Whether sustained idle-silence (zero hops while running) is a rebuild fault.
    /// `true` on macOS (the all-zero tap bug is real), `false` on Windows (loopback
    /// legitimately delivers no packets during silence). An explicit device change
    /// always forces a rebuild regardless of this flag.
    treat_idle_as_fault: bool,
}

impl WatchdogPolicy {
    /// New policy with the platform-correct idle policy and the default ~3 s
    /// zero-buffer window. macOS treats idle-silence as a fault; Windows does not
    /// (see [`TREAT_IDLE_AS_FAULT`]).
    pub fn new() -> Self {
        Self::with_window(Duration::from_secs_f64(ZERO_BUFFER_REBUILD_SECS))
    }

    /// New policy with a custom zero-buffer window and the platform-correct idle
    /// policy (used by tests that only need to tune the timing).
    pub fn with_window(zero_window: Duration) -> Self {
        Self::with_window_and_idle_policy(zero_window, TREAT_IDLE_AS_FAULT)
    }

    /// New policy with both the zero-buffer window and the idle policy chosen
    /// explicitly. Used by unit tests to exercise *both* the macOS-style
    /// (`treat_idle_as_fault = true`) and Windows-style (`= false`) branches on a
    /// single host, independent of the host OS.
    pub fn with_window_and_idle_policy(zero_window: Duration, treat_idle_as_fault: bool) -> Self {
        Self {
            last_nonzero: 0,
            last_activity: Instant::now(),
            zero_window,
            treat_idle_as_fault,
        }
    }

    /// Whether this policy treats sustained idle-silence while running as a fault.
    pub fn treats_idle_as_fault(&self) -> bool {
        self.treat_idle_as_fault
    }

    /// Feeds the latest counters, the explicit device-change signal, and the current
    /// time, returning a verdict.
    ///
    /// `running` reflects whether the backend believes capture is live; genuine
    /// stopped capture is never treated as a zero-buffer fault here.
    ///
    /// `device_changed` is the explicit, backend-agnostic rebuild trigger (the
    /// Windows `IMMNotificationClient` default-device / device-state change). It
    /// forces a [`WatchdogVerdict::NeedsRebuild`] **regardless of the idle policy** —
    /// a device change means the endpoint we were capturing from is gone or replaced,
    /// so the live capture must be torn down and rebuilt against the new default
    /// endpoint even on a platform where idle-silence is healthy. Fresh non-zero
    /// audio observed on the same poll still wins (it cannot be a stale endpoint), so
    /// the latch self-heals.
    pub fn observe(
        &mut self,
        nonzero_hops: u64,
        running: bool,
        device_changed: bool,
        now: Instant,
    ) -> WatchdogVerdict {
        if nonzero_hops > self.last_nonzero {
            self.last_nonzero = nonzero_hops;
            self.last_activity = now;
            return WatchdogVerdict::Healthy;
        }
        // An explicit device change always warrants a rebuild, independent of the
        // idle policy — even on Windows where pure silence is healthy.
        if device_changed {
            self.last_activity = now;
            return WatchdogVerdict::NeedsRebuild;
        }
        // Sustained zero buffers while running are a fault only where the platform
        // guarantees the backend keeps delivering buffers (macOS). On Windows the
        // backend stays silent during idle by design, so this branch is skipped.
        if self.treat_idle_as_fault
            && running
            && now.duration_since(self.last_activity) >= self.zero_window
        {
            // Reset the clock so we don't re-trigger every tick after firing.
            self.last_activity = now;
            return WatchdogVerdict::NeedsRebuild;
        }
        WatchdogVerdict::Healthy
    }

    /// Resets the activity clock (call after a successful rebuild).
    pub fn reset(&mut self, now: Instant) {
        self.last_activity = now;
    }
}

impl Default for WatchdogPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes the next backoff delay given the current one (doubling, capped).
pub fn next_backoff(current: Duration) -> Duration {
    let secs = current.as_secs().max(BACKOFF_START_SECS);
    Duration::from_secs((secs * 2).min(BACKOFF_CAP_SECS))
}

/// The initial backoff delay.
pub fn initial_backoff() -> Duration {
    Duration::from_secs(BACKOFF_START_SECS)
}

/// What the threaded driver should DO this poll about the tap rebuild — the pure
/// decision, decoupled from the actual Core Audio teardown/restart (MAJOR-1/2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildAction {
    /// Nothing to do this poll (healthy, or inside the backoff window).
    None,
    /// Drop the live tap and start a fresh one now (the backoff has elapsed). The
    /// driver reports the outcome back via [`RebuildScheduler::record_result`].
    Attempt,
}

/// A one-shot logging edge the driver should emit (state-edge logging, MAJOR-2):
/// each fires exactly once per episode, never per poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildLog {
    /// No log this poll.
    None,
    /// We just *entered* a zero-buffer episode (warn once).
    EnteredZeroEpisode,
    /// Capture just *recovered* (info once) — non-zero buffers are flowing again.
    Recovered,
}

/// The pure, hardware-free half of the watchdog's rebuild loop (MAJOR-1/2).
///
/// It owns all the bookkeeping that used to be loose locals in the threaded driver
/// — the backoff schedule (2,4,8..30 s), the once-per-episode log edges, the
/// sustained-zeros latch, and the post-rebuild "still zero" arming — so every
/// branch is unit-testable with a scripted clock and no Core Audio (the driver in
/// [`crate::pipeline`] only performs the actual `drop` + [`crate::tap::start`]).
///
/// Per poll the driver calls [`RebuildScheduler::poll`] with the
/// [`WatchdogVerdict`], the current non-zero-hop count, and the clock; it returns a
/// [`PollDecision`] telling the driver whether to attempt a rebuild, what to log,
/// and the `rebuild_completed_still_zero` event + `sustained_zeros` flag to feed the
/// permission machine. When [`PollDecision::action`] is [`RebuildAction::Attempt`]
/// the driver performs the rebuild and reports success/failure back via
/// [`RebuildScheduler::record_result`].
#[derive(Debug)]
pub struct RebuildScheduler {
    /// True between a completed rebuild and the next non-zero sample: lets us emit
    /// `rebuild_completed_still_zero` once when capture is still silent afterwards.
    rebuild_pending: bool,
    /// Latches once a sustained zero window is seen; cleared by fresh audio.
    sustained_zeros: bool,
    /// True while inside a zero-buffer episode (drives once-per-episode logging).
    in_zero_episode: bool,
    /// Earliest instant the next rebuild attempt may run (backoff gate).
    next_attempt_at: Instant,
    /// Current backoff delay (doubles up to the cap; reset by fresh audio).
    backoff: Duration,
}

/// The result of one [`RebuildScheduler::poll`] (MAJOR-1/2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollDecision {
    /// Whether to attempt a real rebuild this poll.
    pub action: RebuildAction,
    /// A once-per-episode log edge for the driver to emit.
    pub log: RebuildLog,
    /// True while a sustained zero window is in effect (permission-machine input).
    pub sustained_zeros: bool,
    /// The one-shot "a completed rebuild is still zero" event (third revocation
    /// condition) — true on exactly the poll it fires.
    pub rebuild_completed_still_zero: bool,
}

impl RebuildScheduler {
    /// New scheduler armed to attempt immediately on the first fault (`start` seeds
    /// the backoff gate so the first NeedsRebuild attempts without waiting).
    pub fn new(start: Instant) -> Self {
        Self {
            rebuild_pending: false,
            sustained_zeros: false,
            in_zero_episode: false,
            next_attempt_at: start,
            backoff: initial_backoff(),
        }
    }

    /// Current backoff delay (diagnostics / tests).
    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    /// Whether a completed-rebuild "still zero" confirmation is armed (tests).
    pub fn rebuild_pending(&self) -> bool {
        self.rebuild_pending
    }

    /// Advances the scheduler one poll. Pure: no Core Audio, no logging — it only
    /// decides. The driver acts on [`PollDecision`] and, for an
    /// [`RebuildAction::Attempt`], reports the outcome via [`Self::record_result`].
    pub fn poll(&mut self, verdict: WatchdogVerdict, nonzero: u64, now: Instant) -> PollDecision {
        // The completed-rebuild-still-zero event fires when a rebuild was pending
        // from a prior attempt and capture is still silent now.
        let rebuild_completed_still_zero = self.rebuild_pending && nonzero == 0;

        let (action, log) = match verdict {
            WatchdogVerdict::Healthy => {
                let mut log = RebuildLog::None;
                if nonzero > 0 {
                    // Fresh audio: a grant resets all fault bookkeeping + backoff.
                    if self.in_zero_episode {
                        log = RebuildLog::Recovered;
                        self.in_zero_episode = false;
                    }
                    self.sustained_zeros = false;
                    self.rebuild_pending = false;
                    self.backoff = initial_backoff();
                    self.next_attempt_at = now;
                }
                (RebuildAction::None, log)
            }
            WatchdogVerdict::NeedsRebuild => {
                self.sustained_zeros = true;
                let log = if self.in_zero_episode {
                    RebuildLog::None
                } else {
                    self.in_zero_episode = true;
                    RebuildLog::EnteredZeroEpisode
                };
                // Backed-off attempt: only once the delay has elapsed (MAJOR-2).
                let action = if now >= self.next_attempt_at {
                    RebuildAction::Attempt
                } else {
                    RebuildAction::None
                };
                (action, log)
            }
        };

        // Consume the one-shot so it influences exactly the poll that reported it.
        if rebuild_completed_still_zero {
            self.rebuild_pending = false;
        }

        PollDecision {
            action,
            log,
            sustained_zeros: self.sustained_zeros,
            rebuild_completed_still_zero,
        }
    }

    /// Reports the outcome of an attempted rebuild and advances the backoff. Call
    /// only after a [`RebuildAction::Attempt`].
    ///
    /// * `ok == true` — the rebuild completed: arm the post-rebuild "still zero"
    ///   observation (a later still-zero poll becomes the third revocation
    ///   condition).
    /// * `ok == false` — the rebuild failed: leave it disarmed; the driver
    ///   publishes the generic Failed state.
    ///
    /// Either way the next attempt is spaced by the doubling backoff (2,4,8..30 s).
    pub fn record_result(&mut self, ok: bool, now: Instant) {
        if ok {
            self.rebuild_pending = true;
        }
        self.next_attempt_at = now + self.backoff;
        self.backoff = next_backoff(self.backoff);
    }
}

/// Builds the user-facing failure message for a capture error.
pub fn failure_message(err: &CaptureError) -> String {
    if err.is_permission_denied() {
        format!(
            "System-audio capture was denied. {} (details: {err})",
            CaptureError::guidance()
        )
    } else {
        format!(
            "System-audio capture could not be (re)started. {} (details: {err})",
            CaptureError::guidance()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// macOS-style policy (`treat_idle_as_fault = true`), independent of the host OS.
    fn macos_policy(window: Duration) -> WatchdogPolicy {
        WatchdogPolicy::with_window_and_idle_policy(window, true)
    }

    /// Windows-style policy (`treat_idle_as_fault = false`), independent of host OS.
    fn windows_policy(window: Duration) -> WatchdogPolicy {
        WatchdogPolicy::with_window_and_idle_policy(window, false)
    }

    #[test]
    fn genuine_silence_is_not_a_fault_when_not_running() {
        let mut p = macos_policy(Duration::from_millis(50));
        let start = Instant::now();
        // Not running: zero buffers must never trigger a rebuild.
        let later = start + Duration::from_secs(10);
        assert_eq!(p.observe(0, false, false, later), WatchdogVerdict::Healthy);
    }

    #[test]
    fn zero_buffers_while_running_triggers_rebuild() {
        // macOS-style: sustained zeros while running IS a fault.
        let mut p = macos_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        // First observation seeds the activity clock.
        assert_eq!(p.observe(0, true, false, t0), WatchdogVerdict::Healthy);
        // Before the window elapses: still healthy.
        assert_eq!(
            p.observe(0, true, false, t0 + Duration::from_millis(50)),
            WatchdogVerdict::Healthy
        );
        // After the window with no new activity: rebuild.
        assert_eq!(
            p.observe(0, true, false, t0 + Duration::from_millis(150)),
            WatchdogVerdict::NeedsRebuild
        );
    }

    #[test]
    fn activity_keeps_it_healthy() {
        let mut p = macos_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        p.observe(0, true, false, t0);
        // New nonzero hops keep resetting the clock.
        for k in 1..10u64 {
            let t = t0 + Duration::from_millis(50 * k);
            assert_eq!(p.observe(k, true, false, t), WatchdogVerdict::Healthy);
        }
    }

    // --- Windows idle policy (D2 / risk 1) -----------------------------------

    #[test]
    fn windows_idle_silence_is_not_a_fault() {
        // Windows-style: WASAPI loopback delivers no packets during silence, so a
        // sustained run of zero hops while running must STAY healthy — never a
        // rebuild fault (the highest-impact adaptation, research risk 1).
        let mut p = windows_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        assert_eq!(p.observe(0, true, false, t0), WatchdogVerdict::Healthy);
        // Far beyond the window, still running, still silent: healthy, not a rebuild.
        for secs in [1u64, 5, 30, 300] {
            assert_eq!(
                p.observe(0, true, false, t0 + Duration::from_secs(secs)),
                WatchdogVerdict::Healthy,
                "Windows idle silence must never schedule a rebuild ({secs}s)"
            );
        }
    }

    #[test]
    fn windows_first_nonzero_still_promotes_after_idle() {
        // After long idle silence, the first non-zero hop is still observed normally
        // (it resets the activity clock and stays healthy) — so the sticky
        // permission "granted" latch promotes the moment real audio arrives, exactly
        // like macOS. The Windows idle policy only suppresses the *fault*, not the
        // normal liveness accounting.
        let mut p = windows_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        // Idle for a long time: healthy throughout.
        assert_eq!(
            p.observe(0, true, false, t0 + Duration::from_secs(60)),
            WatchdogVerdict::Healthy
        );
        // First audio arrives: healthy (clock reset), and subsequent zeros are again
        // treated as healthy idle — never a rebuild.
        let t1 = t0 + Duration::from_secs(61);
        assert_eq!(p.observe(1, true, false, t1), WatchdogVerdict::Healthy);
        assert_eq!(
            p.observe(1, true, false, t1 + Duration::from_secs(10)),
            WatchdogVerdict::Healthy
        );
    }

    // --- Explicit device-change trigger (D4) ---------------------------------

    #[test]
    fn device_changed_forces_rebuild_even_when_idle_policy_is_healthy() {
        // On Windows the idle policy reports Healthy during silence, yet an explicit
        // device change must still force a rebuild against the new endpoint.
        let mut p = windows_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        // Silent + running: healthy without a device change.
        assert_eq!(
            p.observe(0, true, false, t0 + Duration::from_secs(5)),
            WatchdogVerdict::Healthy
        );
        // Same silence, but a device change pulses: rebuild regardless of the policy.
        assert_eq!(
            p.observe(0, true, true, t0 + Duration::from_secs(6)),
            WatchdogVerdict::NeedsRebuild,
            "an explicit device change must rebuild even when idle is healthy"
        );
    }

    #[test]
    fn device_changed_forces_rebuild_on_macos_policy_too() {
        // The device-change trigger is backend-agnostic: it forces a rebuild on the
        // macOS-style policy as well, before the zero window has elapsed.
        let mut p = macos_policy(Duration::from_secs(3));
        let t0 = Instant::now();
        assert_eq!(p.observe(0, true, false, t0), WatchdogVerdict::Healthy);
        // Well within the 3 s zero window — idle alone would NOT fire yet, but the
        // device change does.
        assert_eq!(
            p.observe(0, true, true, t0 + Duration::from_millis(100)),
            WatchdogVerdict::NeedsRebuild
        );
    }

    #[test]
    fn fresh_audio_wins_over_device_change_on_same_poll() {
        // If real audio arrives on the same poll as a device-change pulse, the audio
        // takes precedence (a non-zero hop cannot have come from a stale endpoint),
        // so the verdict is Healthy and the latch self-heals.
        let mut p = windows_policy(Duration::from_millis(100));
        let t0 = Instant::now();
        assert_eq!(p.observe(0, true, false, t0), WatchdogVerdict::Healthy);
        assert_eq!(
            p.observe(1, true, true, t0 + Duration::from_millis(10)),
            WatchdogVerdict::Healthy
        );
    }

    #[test]
    fn capture_health_device_change_is_edge_triggered() {
        // note_device_changed pulses; take_device_changed consumes exactly once.
        let h = CaptureHealth::new();
        assert!(!h.take_device_changed(), "no request initially");
        h.note_device_changed();
        assert!(h.take_device_changed(), "first take consumes the pulse");
        assert!(!h.take_device_changed(), "second take sees nothing");
        // Multiple notifications before a take collapse to a single pending request.
        h.note_device_changed();
        h.note_device_changed();
        assert!(h.take_device_changed());
        assert!(!h.take_device_changed());
    }

    #[test]
    fn idle_policy_default_matches_platform() {
        // The default policy picks the platform-correct idle policy.
        assert_eq!(
            WatchdogPolicy::new().treats_idle_as_fault(),
            TREAT_IDLE_AS_FAULT
        );
        assert_eq!(TREAT_IDLE_AS_FAULT, cfg!(target_os = "macos"));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = initial_backoff();
        assert_eq!(d, Duration::from_secs(2));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(4));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(8));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(16));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(30)); // 32 capped to 30
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn health_handle_publishes_transitions() {
        let h = CaptureHealth::new();
        assert_eq!(h.state(), CaptureState::Active);
        h.set_state(CaptureState::Failed {
            message: "x".into(),
        });
        assert!(matches!(h.state(), CaptureState::Failed { .. }));
        h.note_rms(0.0);
        h.note_rms(0.5);
        let (total, nonzero) = h.counters();
        assert_eq!(total, 2);
        assert_eq!(nonzero, 1);
    }

    // --- RebuildScheduler: the pure rebuild-loop policy (MAJOR-1/2) ----------

    #[test]
    fn first_fault_attempts_rebuild_immediately_and_logs_episode_once() {
        // MAJOR-1: the first NeedsRebuild verdict attempts a real rebuild right away
        // (the gate is seeded to `start`). MAJOR-2: the zero-episode warn fires once.
        let t0 = Instant::now();
        let mut s = RebuildScheduler::new(t0);

        let d = s.poll(WatchdogVerdict::NeedsRebuild, 0, t0);
        assert_eq!(d.action, RebuildAction::Attempt);
        assert_eq!(d.log, RebuildLog::EnteredZeroEpisode);
        assert!(d.sustained_zeros);
        assert!(!d.rebuild_completed_still_zero);

        // A second NeedsRebuild while still in the episode does NOT re-log.
        let d = s.poll(
            WatchdogVerdict::NeedsRebuild,
            0,
            t0 + Duration::from_millis(250),
        );
        assert_eq!(d.log, RebuildLog::None, "episode warn must fire only once");
    }

    #[test]
    fn rebuild_success_arms_post_rebuild_observation_then_fires_still_zero_once() {
        // A *completed* rebuild that is still zero on a later poll yields the
        // third revocation condition exactly once.
        let t0 = Instant::now();
        let mut s = RebuildScheduler::new(t0);

        let d = s.poll(WatchdogVerdict::NeedsRebuild, 0, t0);
        assert_eq!(d.action, RebuildAction::Attempt);
        // Driver reports success → arms the post-rebuild "still zero" observation.
        s.record_result(true, t0);
        assert!(s.rebuild_pending());

        // Next poll, capture still silent ⇒ the one-shot fires exactly once.
        let t1 = t0 + Duration::from_millis(250);
        let d = s.poll(WatchdogVerdict::NeedsRebuild, 0, t1);
        assert!(
            d.rebuild_completed_still_zero,
            "a completed rebuild that is still zero must arm the tracker"
        );
        assert!(!s.rebuild_pending(), "the one-shot is consumed");

        // It does not fire again on the following poll.
        let t2 = t1 + Duration::from_millis(250);
        let d = s.poll(WatchdogVerdict::NeedsRebuild, 0, t2);
        assert!(!d.rebuild_completed_still_zero);
    }

    #[test]
    fn rebuild_failure_backs_off_attempts_2_4_8_capped() {
        // MAJOR-2: successive rebuild attempts back off (2,4,8..30 s); we never spin.
        let t0 = Instant::now();
        let mut s = RebuildScheduler::new(t0);

        // Attempt #1 at t0, fails → next attempt is t0 + 2 s, backoff now 4 s.
        assert_eq!(
            s.poll(WatchdogVerdict::NeedsRebuild, 0, t0).action,
            RebuildAction::Attempt
        );
        s.record_result(false, t0);

        // Within the 2 s window: NO attempt (this is the anti-spam guarantee).
        for ms in [250u64, 500, 1_000, 1_900] {
            let t = t0 + Duration::from_millis(ms);
            assert_eq!(
                s.poll(WatchdogVerdict::NeedsRebuild, 0, t).action,
                RebuildAction::None,
                "must not re-attempt before the 2 s backoff at {ms} ms"
            );
        }

        // At t0 + 2 s: attempt #2; fails → next at +4 s.
        let t = t0 + Duration::from_secs(2);
        assert_eq!(
            s.poll(WatchdogVerdict::NeedsRebuild, 0, t).action,
            RebuildAction::Attempt
        );
        s.record_result(false, t);

        // +1 s later (3 s total): still within the 4 s window ⇒ no attempt.
        assert_eq!(
            s.poll(
                WatchdogVerdict::NeedsRebuild,
                0,
                t0 + Duration::from_secs(3)
            )
            .action,
            RebuildAction::None
        );
        // At 6 s total (2 + 4): attempt #3.
        let t = t0 + Duration::from_secs(6);
        assert_eq!(
            s.poll(WatchdogVerdict::NeedsRebuild, 0, t).action,
            RebuildAction::Attempt
        );
        s.record_result(false, t);

        // The backoff keeps doubling toward the 30 s cap across many failures.
        let mut now = t;
        for _ in 0..8 {
            now += s.backoff();
            assert_eq!(
                s.poll(WatchdogVerdict::NeedsRebuild, 0, now).action,
                RebuildAction::Attempt
            );
            s.record_result(false, now);
        }
        assert_eq!(s.backoff(), Duration::from_secs(30), "backoff caps at 30 s");
    }

    #[test]
    fn fresh_audio_resets_backoff_and_logs_recovery_once() {
        // MAJOR-2: a grant (first non-zero) resets the backoff and emits the recovery
        // log exactly once; the post-rebuild arming is also cleared.
        let t0 = Instant::now();
        let mut s = RebuildScheduler::new(t0);

        // Enter a fault and let the backoff grow a couple of steps.
        s.poll(WatchdogVerdict::NeedsRebuild, 0, t0);
        s.record_result(false, t0);
        let t = t0 + Duration::from_secs(2);
        s.poll(WatchdogVerdict::NeedsRebuild, 0, t);
        s.record_result(false, t);
        assert!(s.backoff() > initial_backoff());

        // Fresh audio arrives → Healthy with nonzero > 0: recovery logged once,
        // backoff reset, sustained_zeros cleared.
        let t = t + Duration::from_secs(1);
        let d = s.poll(WatchdogVerdict::Healthy, 5, t);
        assert_eq!(d.log, RebuildLog::Recovered);
        assert!(!d.sustained_zeros);
        assert_eq!(s.backoff(), initial_backoff(), "a grant resets the backoff");

        // No double recovery log on the next healthy poll.
        let d = s.poll(WatchdogVerdict::Healthy, 6, t + Duration::from_millis(250));
        assert_eq!(d.log, RebuildLog::None);

        // And a brand-new fault attempts immediately again (the gate was reset).
        let t = t + Duration::from_secs(1);
        let d = s.poll(WatchdogVerdict::NeedsRebuild, 6, t);
        assert_eq!(d.action, RebuildAction::Attempt);
        assert_eq!(
            d.log,
            RebuildLog::EnteredZeroEpisode,
            "new episode logs again"
        );
    }

    #[test]
    fn healthy_without_audio_does_nothing() {
        // A Healthy verdict with no fresh audio (e.g. genuine silence, not running)
        // is a no-op: no attempt, no log, no spurious still-zero event.
        let t0 = Instant::now();
        let mut s = RebuildScheduler::new(t0);
        let d = s.poll(WatchdogVerdict::Healthy, 0, t0);
        assert_eq!(d.action, RebuildAction::None);
        assert_eq!(d.log, RebuildLog::None);
        assert!(!d.sustained_zeros);
        assert!(!d.rebuild_completed_still_zero);
    }
}
