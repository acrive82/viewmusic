//! viz-audio — the system-audio capture and DSP side of ViewMusic.
//!
//! This crate turns the live macOS system audio mix into [`viz_core::FeatureFrame`]s
//! and [`viz_core::BeatEvent`]s for the render thread. It is built in strict
//! layers so the DSP is fully testable without hardware (the analyzer is
//! deterministic; see `tests/synthetic.rs`):
//!
//! * [`Analyzer`] ([`dsp`] + [`onset`]) — a **pure** analyzer with no Core Audio
//!   dependency: a 48 kHz, N=1024/hop=512, Hann-windowed `realfft` pipeline →
//!   48 log-spaced AGC bands, low/mid/high aggregates, EWMA energy, raw waveform,
//!   and SuperFlux onset detection. Allocation-free per hop.
//! * [`CaptureSource`] ([`tap`]) — the capture abstraction and its cidre
//!   process-tap implementation (macOS). All Core Audio code is isolated here.
//! * [`CaptureHealth`] / [`CaptureState`] ([`watchdog`]) — the capture-health
//!   state machine that distinguishes genuine silence from capture failure and
//!   drives tap rebuilds with backoff.
//! * [`AudioPipeline`] / [`AudioHandle`] ([`pipeline`]) — assembly: starts
//!   capture, runs the analyzer inside the IOProc, and hands frames to the render
//!   thread over a wait-free triple buffer (frames) and an SPSC queue (beats).
//!
//! The audio clock is derived from a monotonic sample counter
//! (`t = samples / 48 000`), so the whole pipeline is deterministic under the
//! synthetic-signal tests.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod dsp;
pub mod onset;
pub mod permission;
pub mod pipeline;
pub mod tap;
pub mod watchdog;

pub use dsp::{Analyzer, FFT_SIZE, HOP_SIZE, SPECTRUM_BINS};
pub use onset::{Onset, OnsetResult};
pub use permission::{
    Observation, PermissionCell, PermissionState, PermissionTracker, GRACE_WINDOW,
};
pub use pipeline::{AudioHandle, AudioPipeline, BEAT_QUEUE_CAP};
pub use tap::{CaptureError, CaptureSource, CaptureStage, SampleSink, REQUESTED_BUFFER_FRAMES};
pub use watchdog::{
    initial_backoff, next_backoff, CaptureHealth, CaptureState, PollDecision, RebuildAction,
    RebuildLog, RebuildScheduler, WatchdogPolicy, WatchdogVerdict, ZERO_BUFFER_REBUILD_SECS,
};

// Re-export the core vocabulary for downstream convenience.
pub use viz_core::{BeatEvent, FeatureFrame, BAND_COUNT, SAMPLE_RATE, WAVEFORM_LEN};
