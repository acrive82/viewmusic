//! Audio pipeline assembly.
//!
//! Wires the capture backend ([`crate::tap`]) → the pure [`Analyzer`] → the
//! lock-free handoff (triple-buffer frames + rtrb beat queue) → the render-thread
//! [`AudioHandle`]. A watchdog thread ([`crate::watchdog`]) observes capture
//! liveness and performs the **real** tap+aggregate rebuild on the macOS
//! zero-buffer fault — the only fix for the 14.x all-zero tap bug and the third
//! revocation condition (the rebuild that still produces zeros).
//!
//! Data flow (all hot-path work is allocation-free):
//! ```text
//! IOProc ─push(mono)─▶ SharedSink(Arc<Mutex<AudioSink>>)   try_lock, skip on contention
//!                              │ per hop (lock uncontended except the rebuild window)
//!                              ├─ Input::write(FeatureFrame)   (latest wins)
//!                              └─ Producer::push(BeatEvent)    (drop on full)
//! render thread ◀── AudioHandle { Output::read(), Consumer::pop() }
//! ```
//!
//! **Rebuild ownership.** The live [`crate::tap::CaptureSource`] is owned by the
//! watchdog thread, *not* the [`AudioHandle`]. On a [`NeedsRebuild`](crate::watchdog::WatchdogVerdict)
//! verdict the watchdog drops the live tap (full teardown of the device + tap) and calls
//! [`crate::tap::start`] again with a fresh clone of the **shared** sink. The
//! triple-buffer [`Input`]/rtrb [`Producer`]/[`CaptureHealth`] live behind an
//! `Arc<Mutex<…>>` so the new IOProc keeps publishing into the same channels the
//! render-thread [`AudioHandle`] already reads — the [`Output`]/[`Consumer`] sides
//! stay valid across rebuilds. Only ONE IOProc is alive at a time (the old tap is
//! dropped fully before the new one starts), so the mutex is uncontended outside
//! the brief rebuild window; the audio callback uses `try_lock` and **skips** the
//! buffer on contention — it never waits (the audio callback must never block).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rtrb::{Consumer, Producer, RingBuffer};
use triple_buffer::{Input, Output};
use viz_core::{BeatEvent, FeatureFrame};

use crate::dsp::Analyzer;
use crate::permission::{Observation, PermissionCell, PermissionState, PermissionTracker};
use crate::tap::{CaptureError, CaptureSource, SampleSink};
use crate::watchdog::{CaptureHealth, CaptureState};

/// Capacity of the beat-event SPSC queue. Beats are sparse; 64 is generous.
pub const BEAT_QUEUE_CAP: usize = 64;

/// The render-thread side of the pipeline. Cheap to poll every frame.
pub struct AudioHandle {
    frames: Output<FeatureFrame>,
    beats: Consumer<BeatEvent>,
    health: CaptureHealth,
    dropped_beats: Arc<AtomicU64>,
    /// Latest permission state, published by the watchdog thread.
    permission: PermissionCell,
    // Kept to tear down the capture + watchdog on `stop`/drop.
    shutdown: Option<Shutdown>,
}

/// Owns the things whose `Drop` stops the capture and joins the watchdog.
///
/// The live capture backend is **not** held here — it is owned by the watchdog
/// thread so that thread can drop and recreate it on a rebuild (MAJOR-1). Setting
/// `stop_flag` makes the watchdog drop its capture (full tap/aggregate teardown)
/// and return; joining it guarantees teardown completed.
struct Shutdown {
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
}

impl AudioHandle {
    /// Reads the most recent [`FeatureFrame`] (latest-value triple-buffer copy).
    /// Always returns a frame; before the first hop it is the default (silent).
    pub fn latest_frame(&mut self) -> FeatureFrame {
        *self.frames.read()
    }

    /// Drains all queued [`BeatEvent`]s (oldest first). Call once per frame.
    pub fn drain_beats(&mut self) -> Vec<BeatEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.beats.pop() {
            out.push(ev);
        }
        out
    }

    /// Drains queued beats into `buf` (allocation-free for the caller).
    /// Returns the number drained.
    pub fn drain_beats_into(&mut self, buf: &mut Vec<BeatEvent>) -> usize {
        let mut n = 0;
        while let Ok(ev) = self.beats.pop() {
            buf.push(ev);
            n += 1;
        }
        n
    }

    /// Current capture-health state.
    pub fn capture_state(&self) -> CaptureState {
        self.health.state()
    }

    /// Current permission state. Cheap lock-free read
    /// for the render loop — evaluated every frame to drive the guidance screen.
    pub fn permission_state(&self) -> PermissionState {
        self.permission.get()
    }

    /// Total beat events dropped because the queue was full (diagnostics).
    pub fn dropped_beats(&self) -> u64 {
        self.dropped_beats.load(Ordering::Relaxed)
    }

    /// Stops capture and joins the watchdog. Idempotent via `Drop`.
    pub fn stop(mut self) {
        self.teardown();
    }

    fn teardown(&mut self) {
        if let Some(mut sd) = self.shutdown.take() {
            sd.stop_flag.store(true, Ordering::Relaxed);
            // Joining waits for the watchdog to drop the live capture it owns →
            // full tap/aggregate teardown.
            if let Some(j) = sd.watchdog.take() {
                let _ = j.join();
            }
            tracing::info!("audio pipeline stopped");
        }
    }
}

impl Drop for AudioHandle {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// The audio-thread sink: owns the [`Analyzer`] and publishes results. Reached
/// from the IOProc through a [`SharedSink`]; every method is allocation-free and
/// lock-free once the (uncontended) mutex is held.
struct AudioSink {
    analyzer: Analyzer,
    frames: Input<FeatureFrame>,
    beats: Producer<BeatEvent>,
    health: CaptureHealth,
    dropped_beats: Arc<AtomicU64>,
}

impl AudioSink {
    /// Processes one IOProc's worth of mono samples (called with the mutex held).
    fn process(&mut self, mono: &[f32]) {
        // Borrow the publisher fields so the closure can use them without
        // capturing `self` (which `push_samples` borrows mutably).
        let frames = &mut self.frames;
        let beats = &mut self.beats;
        let health = &self.health;
        let dropped = &self.dropped_beats;
        self.analyzer.push_samples(mono, |frame, evs| {
            // Publish the latest feature frame (overwrites; latest wins).
            frames.write(*frame);
            // Note liveness for the watchdog (energy>0 ⇒ nonzero activity).
            health.note_activity(frame.energy > 1.0e-6 || !frame.silence);
            // Queue beats; drop (and count) if the consumer is behind.
            for ev in evs {
                if beats.push(*ev).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
}

/// A shareable handle to the one [`AudioSink`], cloned into each tap the watchdog
/// builds (MAJOR-1). Wrapping the analyzer + triple-buffer [`Input`] + rtrb
/// [`Producer`] + [`CaptureHealth`] behind an `Arc<Mutex<…>>` lets a rebuild swap
/// the IOProc while the same publishing channels (and thus the render-thread
/// [`AudioHandle`]'s [`Output`]/[`Consumer`]) stay valid.
///
/// **Real-time discipline.** [`SampleSink::push`] uses
/// `try_lock` and **skips** the buffer on contention — it never blocks the audio
/// thread. Contention can only occur during the brief rebuild window (the old tap
/// is dropped fully before the new one starts, so at most one IOProc is alive),
/// and a skipped buffer there is **by design**: a few dropped hops while we swap
/// the tap is invisible next to the rebuild itself.
#[derive(Clone)]
struct SharedSink {
    inner: Arc<Mutex<AudioSink>>,
}

impl SharedSink {
    fn new(sink: AudioSink) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
        }
    }
}

impl SampleSink for SharedSink {
    fn push(&mut self, mono: &[f32]) {
        // Non-blocking: if the watchdog holds the lock (rebuild window) we drop
        // this buffer rather than wait — the audio callback must never block.
        if let Ok(mut sink) = self.inner.try_lock() {
            sink.process(mono);
        }
    }
}

/// The pipeline factory.
pub struct AudioPipeline;

impl AudioPipeline {
    /// Starts capture and returns the render-thread [`AudioHandle`].
    ///
    /// On macOS this builds the cidre process-tap capture; on other platforms it
    /// returns the unsupported [`CaptureError`]. The DSP and handoff are platform
    /// independent.
    pub fn start() -> Result<AudioHandle, CaptureError> {
        // Handoff primitives.
        let (frame_in, frame_out) = triple_buffer::triple_buffer(&FeatureFrame::default());
        let (beat_tx, beat_rx) = RingBuffer::<BeatEvent>::new(BEAT_QUEUE_CAP);
        let health = CaptureHealth::new();
        let dropped_beats = Arc::new(AtomicU64::new(0));

        let sink = SharedSink::new(AudioSink {
            analyzer: Analyzer::new(),
            frames: frame_in,
            beats: beat_tx,
            health: health.clone(),
            dropped_beats: dropped_beats.clone(),
        });

        // Start the capture backend (macOS: cidre tap; Windows: WASAPI loopback;
        // else: error). The sink is shared so the watchdog can rebuild the tap
        // against the same channels.
        //
        // On Windows the WASAPI backend wires its device-change notification client
        // to the shared CaptureHealth; the seam carries only a sink (to keep the
        // macOS/Windows `tap::start` signatures identical), so the health handle is
        // deposited just before the call. No-op on macOS.
        wire_device_change_health(&health);
        let capture = crate::tap::start(Box::new(sink.clone()))?;
        health.set_running(true);
        let granted = capture.granted_buffer_frames();
        tracing::info!(granted_buffer_frames = granted, "audio pipeline started");

        // Spawn the watchdog thread (also drives the permission machine). The
        // pipeline ran the full create-tap → aggregate → IOProc →
        // AudioDeviceStart sequence above — that *is* the TCC prompt trigger; a
        // creation error already returned via `?` and is handled as the generic
        // Failed path.
        //
        // The watchdog OWNS the live capture (so it can drop+recreate it on a
        // rebuild, MAJOR-1) and a clone of the shared sink to feed each new tap.
        let permission = PermissionCell::new();
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog = spawn_watchdog(
            Box::new(capture),
            sink,
            health.clone(),
            permission.clone(),
            stop_flag.clone(),
        );

        Ok(AudioHandle {
            frames: frame_out,
            beats: beat_rx,
            health,
            dropped_beats,
            permission,
            shutdown: Some(Shutdown {
                stop_flag,
                watchdog: Some(watchdog),
            }),
        })
    }
}

/// Spawns the watchdog thread. It polls capture liveness on a ~250 ms cadence and,
/// on a sustained zero-buffer fault, performs the **real** tap+aggregate rebuild —
/// it owns the live [`CaptureSource`], so it drops it (full teardown) and calls
/// [`crate::tap::start`] again with a fresh clone of the shared sink. A skipped
/// audio buffer during that swap is by design (see
/// [`SharedSink`]); only one IOProc is ever alive at a time.
///
/// Successive rebuild attempts are spaced by the existing backoff helpers
/// ([`initial_backoff`](crate::watchdog::initial_backoff) →
/// [`next_backoff`](crate::watchdog::next_backoff): 2, 4, 8 … 30 s) so a steady
/// unpermissioned state never spins on rebuilds or spams logs (MAJOR-2). The first
/// fresh non-zero sample resets the backoff. Zero-buffer/recovery logs fire once
/// per episode (state-edge), never per poll.
///
/// It also drives the [`PermissionTracker`]: each poll it assembles an
/// [`Observation`] (sticky `saw_nonzero` latch, elapsed-since-start, the sustained
/// zero window, the default output device's "running somewhere" flag, and a
/// one-shot rebuild-completed-still-zero event) and publishes the resulting
/// [`PermissionState`] to the shared cell. The `DeviceIsRunningSomewhere` read is
/// the silence-vs-revocation discriminator (D6); it stays off the hot path.
///
/// **False-revocation limitation (MINOR-2 / accepted risk).** The
/// `DeviceIsRunningSomewhere` discriminator reflects **any** process on the output
/// device, not ours. So a third-party process holding the output device, *plus*
/// our own genuine silence, *plus* an (eventually) completed rebuild that is still
/// zero, can in principle demote a true `Granted` to `Denied` spuriously. This is
/// accepted: it requires a rare three-way coincidence, the guidance it shows is
/// harmless, and the **first** real non-zero sample re-promotes to `Granted`
/// instantly — so the misfire self-heals the moment our
/// capture produces audio again.
///
/// `sink` keeps a clone of the shared sink alive for the thread's lifetime and is
/// the source of the fresh sink handed to each rebuilt tap.
fn spawn_watchdog(
    mut capture: Box<dyn CaptureSource>,
    sink: SharedSink,
    health: CaptureHealth,
    permission: PermissionCell,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    use crate::watchdog::{
        RebuildAction, RebuildLog, RebuildScheduler, WatchdogPolicy, WatchdogVerdict,
    };
    use std::time::{Duration, Instant};

    std::thread::Builder::new()
        .name("viz-audio-watchdog".into())
        .spawn(move || {
            let mut policy = WatchdogPolicy::new();
            let mut tracker = PermissionTracker::new();
            let poll = Duration::from_millis(250);
            let start = Instant::now();
            // All the rebuild bookkeeping (backoff, episode-edge logging, sustained
            // zeros, post-rebuild arming) lives in this pure, unit-tested scheduler
            // (MAJOR-1/2); the driver below only performs the actual Core Audio
            // teardown/restart it asks for.
            let mut scheduler = RebuildScheduler::new(start);
            tracing::debug!("watchdog thread started");
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(poll);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let (_total, nonzero) = health.counters();
                let running = health.is_running();
                let now = Instant::now();

                // An explicit device-change notification forces a rebuild regardless
                // of the idle policy (on Windows the IMMNotificationClient pulses it;
                // on macOS it stays false). Consumed once per pulse.
                let device_changed = health.take_device_changed();
                let verdict = policy.observe(nonzero, running, device_changed, now);
                let decision = scheduler.poll(verdict, nonzero, now);

                // Emit the once-per-episode log edge the scheduler decided on.
                match decision.log {
                    RebuildLog::None => {}
                    RebuildLog::EnteredZeroEpisode => tracing::warn!(
                        "watchdog: capture produced zero buffers for {:.0}s while \
                         running — starting tap rebuilds",
                        crate::watchdog::ZERO_BUFFER_REBUILD_SECS
                    ),
                    RebuildLog::Recovered => {
                        tracing::info!(
                            "watchdog: capture recovered (non-zero buffers are \
                             flowing again)"
                        );
                    }
                }

                // Recover the published CaptureState as soon as fresh audio returns.
                if verdict == WatchdogVerdict::Healthy
                    && nonzero > 0
                    && matches!(health.state(), CaptureState::Failed { .. })
                {
                    health.set_state(CaptureState::Active);
                }

                if matches!(verdict, WatchdogVerdict::NeedsRebuild) {
                    // Surface as a recoverable failure (feature-001 generic path);
                    // the live permission state is the user-facing view.
                    health.set_state(CaptureState::Failed {
                        message: format!(
                            "System-audio capture stalled (silent buffers). {}",
                            CaptureError::guidance()
                        ),
                    });
                }

                // Perform the real rebuild if (and only if) the scheduler's backoff
                // gate says so this poll (MAJOR-1/2).
                if decision.action == RebuildAction::Attempt {
                    match rebuild_capture(&mut capture, &sink, &health) {
                        Ok(()) => scheduler.record_result(true, now),
                        Err(e) => {
                            // Rebuild failed: publish the generic Failed state
                            // (feature-001 path) and back off. Keep `running` TRUE so
                            // the policy keeps flagging the zero-buffer fault and the
                            // backed-off retries actually fire (without this the
                            // `running=false` set by the failed teardown would silence
                            // the policy and strand us with no further attempts).
                            health.set_running(true);
                            health.set_state(CaptureState::Failed {
                                message: crate::watchdog::failure_message(&e),
                            });
                            scheduler.record_result(false, now);
                        }
                    }
                    // Reset the zero-window so the policy does not immediately re-fire
                    // NeedsRebuild before the new (or retried) tap has had a window to
                    // warm up — the scheduler's backoff gate then spaces the retries.
                    policy.reset(now);
                }

                // The silence-vs-revocation discriminator (D6). `None` (unreadable)
                // is treated as "not running" so we never declare a false
                // revocation on a read error (stays sticky-Granted).
                let output_device_running =
                    crate::tap::output_device_running_somewhere().unwrap_or(false);

                let obs = Observation {
                    saw_nonzero: health.saw_nonzero(),
                    elapsed_since_start: now.duration_since(start),
                    sustained_zeros: decision.sustained_zeros,
                    output_device_running,
                    rebuild_completed_still_zero: decision.rebuild_completed_still_zero,
                };
                let next = tracker.observe(&obs);
                permission.set(next);
            }
            tracing::debug!("watchdog thread stopped");
            // `capture` drops here → full tap/aggregate teardown.
        })
        .expect("failed to spawn watchdog thread")
}

/// Performs the real tap+aggregate rebuild owned by the watchdog (MAJOR-1).
///
/// Drops the live capture FIRST (full teardown — only one IOProc alive at a time,
/// research 001 R1), then starts a fresh tap against the **same** shared sink, so
/// the render-thread [`AudioHandle`]'s channels stay valid. On success the new
/// capture replaces the old; on error the old capture is already gone and the
/// caller publishes the failure + backs off.
fn rebuild_capture(
    capture: &mut Box<dyn CaptureSource>,
    sink: &SharedSink,
    health: &CaptureHealth,
) -> Result<(), CaptureError> {
    tracing::info!("watchdog: rebuilding tap+aggregate device (full teardown + restart)");
    // Drop the old IOProc/tap fully before starting the new one. We replace the
    // boxed source with a placeholder that is immediately overwritten on success;
    // on failure the caller leaves `capture` holding it (a dead handle whose Drop
    // is a no-op) and retries on the next backoff tick.
    let old = std::mem::replace(capture, Box::new(DeadCapture));
    drop(old);
    // Mark not-running during the swap so a stray poll in this window cannot
    // mis-read the dead placeholder as a live fault. On success we set it back to
    // true below; on FAILURE the caller restores `running = true` so the policy
    // keeps flagging the fault and the backed-off retries continue.
    health.set_running(false);

    // Re-wire the device-change health for the rebuilt Windows backend (no-op on
    // macOS). Deposited on this (watchdog) thread right before the start call.
    wire_device_change_health(health);
    let fresh = crate::tap::start(Box::new(sink.clone()))?;
    let granted = fresh.granted_buffer_frames();
    *capture = Box::new(fresh);
    health.set_running(true);
    health.set_state(CaptureState::Active);
    tracing::info!(
        granted_buffer_frames = granted,
        "watchdog: tap rebuild succeeded"
    );
    Ok(())
}

/// Deposits the shared [`CaptureHealth`] for the Windows WASAPI backend's
/// device-change notification client to pulse, immediately before a `tap::start`
/// call on the current thread. The capture seam carries only a sink (to keep the
/// macOS and Windows `tap::start` signatures identical), so the health handle is
/// handed over through this side channel. A no-op on every non-Windows platform.
#[cfg(target_os = "windows")]
fn wire_device_change_health(health: &CaptureHealth) {
    crate::tap_windows::set_pending_health(health.clone());
}

/// No-op on platforms whose backend has no device-change notification client
/// (macOS and the unsupported fallback).
#[cfg(not(target_os = "windows"))]
fn wire_device_change_health(_health: &CaptureHealth) {}

/// A no-op [`CaptureSource`] placeholder held by the watchdog only in the window
/// between dropping a failed rebuild's old tap and the next retry. It owns no Core
/// Audio resources, so its `Drop` is a no-op.
struct DeadCapture;

impl CaptureSource for DeadCapture {
    fn granted_buffer_frames(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_handle_defaults_before_first_frame() {
        // Build a handle without real capture to exercise the handoff types.
        let (frame_in, frame_out) = triple_buffer::triple_buffer(&FeatureFrame::default());
        let (_beat_tx, beat_rx) = RingBuffer::<BeatEvent>::new(BEAT_QUEUE_CAP);
        let health = CaptureHealth::new();
        let dropped = Arc::new(AtomicU64::new(0));
        // Keep the producer alive by leaking it into a no-op shutdown-less handle.
        drop(frame_in);
        let mut handle = AudioHandle {
            frames: frame_out,
            beats: beat_rx,
            health,
            dropped_beats: dropped,
            permission: PermissionCell::new(),
            shutdown: None,
        };
        let f = handle.latest_frame();
        assert!(f.silence);
        assert_eq!(f.beat_count, 0);
        assert!(handle.drain_beats().is_empty());
        assert_eq!(handle.capture_state(), CaptureState::Active);
        assert_eq!(handle.permission_state(), PermissionState::Unknown);
    }

    #[test]
    fn beat_queue_drops_count_when_full() {
        let (tx, rx) = RingBuffer::<BeatEvent>::new(2);
        let mut tx = tx;
        let dropped = Arc::new(AtomicU64::new(0));
        for i in 0..5 {
            let ev = BeatEvent {
                t: i as f64,
                strength: 1.0,
            };
            if tx.push(ev).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3); // cap 2 → 3 dropped
        drop(rx);
    }
}
