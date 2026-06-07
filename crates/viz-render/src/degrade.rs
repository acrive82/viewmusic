//! Adaptive performance degradation ladder.
//!
//! Pure logic, no GPU: feed it per-frame wall-clock timings and it decides how much detail
//! the renderer/runtime should shed to hold the 60 Hz budget. The decision is driven by the
//! **rolling p90 frame cost** so a few stray slow frames do not trigger a downgrade, and
//! recovery is deliberately slow (requires sustained headroom) to avoid oscillation.
//!
//! ## Ladder
//! `detail_factor()` walks `1.0 → 0.8 → 0.6 → 0.4`. The runtime multiplies element/cell
//! counts (and similar costs) by this factor. When already at the bottom (`0.4`) and the
//! p90 is *still* over budget, the controller enters the **paused** state ([`is_paused`]).
//! The very first downgrade step also sheds the feedback/trails pass ([`shed_feedback`]):
//! trails are the first feature dropped because they are the most expensive and the least
//! essential.
//!
//! ## Thresholds
//! - **Down** when rolling p90 > [`DOWN_BUDGET_MS`] (~14 ms — leaves margin under 16.7 ms).
//! - **Up** (recover) when rolling p90 < [`UP_BUDGET_MS`] (~10 ms) for
//!   [`RECOVER_FRAMES`] consecutive evaluations.
//!
//! State transitions are logged exactly once each, via `tracing` (diagnostics go to the
//! log file only, never to the screen).

use std::time::Instant;

/// Downgrade when the rolling p90 frame cost exceeds this (ms).
const DOWN_BUDGET_MS: f64 = 14.0;
/// Recover (upgrade) only when the rolling p90 is under this (ms)…
const UP_BUDGET_MS: f64 = 10.0;
/// …for at least this many consecutive evaluations (slow recovery, no thrash).
const RECOVER_FRAMES: u32 = 90;
/// Rolling window of frame costs used for the p90 estimate (~1 s at 60 Hz).
const WINDOW: usize = 60;
/// Minimum samples before the ladder is allowed to move (warm-up).
const MIN_SAMPLES: usize = 12;

/// The detail steps of the ladder (index 0 = full detail).
const STEPS: [f32; 4] = [1.0, 0.8, 0.6, 0.4];

/// Adaptive performance state machine. Construct with [`DegradeController::new`], call
/// [`DegradeController::begin_frame`]/[`DegradeController::end_frame`] around the frame's
/// work, then read [`DegradeController::detail_factor`]/[`shed_feedback`]/[`is_paused`].
pub struct DegradeController {
    /// Ring buffer of recent frame costs in milliseconds.
    costs: [f64; WINDOW],
    /// Number of valid samples written so far (saturates at `WINDOW`).
    filled: usize,
    /// Write cursor into the ring.
    cursor: usize,
    /// Current ladder index into [`STEPS`].
    level: usize,
    /// Whether we are in the paused state (ladder exhausted + still over budget).
    paused: bool,
    /// Consecutive evaluations the p90 has stayed under the recovery budget.
    recover_streak: u32,
    /// Start instant of the in-flight frame (set by `begin_frame`).
    frame_start: Option<Instant>,
}

impl Default for DegradeController {
    fn default() -> Self {
        Self::new()
    }
}

impl DegradeController {
    /// Creates a controller at full detail with an empty timing window.
    pub fn new() -> Self {
        Self {
            costs: [0.0; WINDOW],
            filled: 0,
            cursor: 0,
            level: 0,
            paused: false,
            recover_streak: 0,
            frame_start: None,
        }
    }

    /// Marks the start of a frame's measured work.
    pub fn begin_frame(&mut self, now: Instant) {
        self.frame_start = Some(now);
    }

    /// Marks the end of a frame's measured work, records the cost, and re-evaluates the
    /// ladder. A missing matching `begin_frame` is ignored (no sample recorded).
    pub fn end_frame(&mut self, now: Instant) {
        let Some(start) = self.frame_start.take() else {
            return;
        };
        let cost_ms = now.saturating_duration_since(start).as_secs_f64() * 1000.0;
        self.costs[self.cursor] = cost_ms;
        self.cursor = (self.cursor + 1) % WINDOW;
        if self.filled < WINDOW {
            self.filled += 1;
        }
        self.evaluate();
    }

    /// Re-evaluates the ladder against the rolling p90. Logs transitions once each.
    fn evaluate(&mut self) {
        if self.filled < MIN_SAMPLES {
            return;
        }
        let p90 = self.p90();

        if p90 > DOWN_BUDGET_MS {
            self.recover_streak = 0;
            if self.level + 1 < STEPS.len() {
                let from = STEPS[self.level];
                self.level += 1;
                tracing::info!(
                    p90_ms = p90,
                    from,
                    to = STEPS[self.level],
                    "viz-render: degrading detail (over budget)"
                );
                if self.level == 1 {
                    tracing::info!("viz-render: shedding feedback/trails (first ladder step)");
                }
            } else if !self.paused {
                // Bottom of the ladder and still over budget → pause.
                self.paused = true;
                tracing::warn!(
                    p90_ms = p90,
                    "viz-render: pausing element evaluation (ladder exhausted, over budget)"
                );
            }
        } else if p90 < UP_BUDGET_MS {
            self.recover_streak = self.recover_streak.saturating_add(1);
            if self.recover_streak >= RECOVER_FRAMES {
                self.recover_streak = 0;
                if self.paused {
                    self.paused = false;
                    tracing::info!(
                        p90_ms = p90,
                        "viz-render: resuming element evaluation (recovered)"
                    );
                } else if self.level > 0 {
                    let from = STEPS[self.level];
                    self.level -= 1;
                    tracing::info!(
                        p90_ms = p90,
                        from,
                        to = STEPS[self.level],
                        "viz-render: recovering detail (under budget)"
                    );
                }
            }
        } else {
            // In the hysteresis band: hold, reset the recovery streak.
            self.recover_streak = 0;
        }
    }

    /// Rolling 90th-percentile frame cost (ms) over the valid window.
    fn p90(&self) -> f64 {
        let n = self.filled;
        // Copy valid samples into a small stack scratch and sort. `WINDOW` is tiny (60).
        let mut scratch = [0.0f64; WINDOW];
        scratch[..n].copy_from_slice(&self.costs[..n]);
        let s = &mut scratch[..n];
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // p90 index (nearest-rank): ceil(0.9 * n) - 1, clamped.
        let idx = (((n as f64) * 0.9).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        s[idx]
    }

    /// Current detail multiplier: `1.0` at full detail, stepping `0.8 / 0.6 / 0.4`.
    pub fn detail_factor(&self) -> f32 {
        STEPS[self.level]
    }

    /// Whether feedback/trails should be shed (true once the ladder has taken its first
    /// step — trails are the first feature dropped).
    pub fn shed_feedback(&self) -> bool {
        self.level >= 1 || self.paused
    }

    /// Whether element evaluation should pause (ladder exhausted and still over budget).
    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Feeds `count` frames each costing `cost_ms` into the controller.
    fn feed(c: &mut DegradeController, base: Instant, count: u32, cost_ms: u64) {
        for k in 0..count {
            // Use distinct instants so each begin/end pair is independent.
            let t0 = base + Duration::from_millis(k as u64 * 1000);
            c.begin_frame(t0);
            c.end_frame(t0 + Duration::from_millis(cost_ms));
        }
    }

    #[test]
    fn starts_at_full_detail() {
        let c = DegradeController::new();
        assert_eq!(c.detail_factor(), 1.0);
        assert!(!c.shed_feedback());
        assert!(!c.is_paused());
    }

    #[test]
    fn warmup_does_not_degrade() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // Fewer than MIN_SAMPLES heavy frames → no movement yet.
        feed(&mut c, base, (MIN_SAMPLES as u32) - 1, 30);
        assert_eq!(c.detail_factor(), 1.0);
    }

    #[test]
    fn sustained_overrun_steps_down_then_pauses() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // Heavy frames (30 ms ≫ 14 ms budget). Each evaluation past warm-up steps down.
        feed(&mut c, base, WINDOW as u32, 30);
        // After enough overrun the ladder bottoms out and pauses.
        assert_eq!(c.detail_factor(), 0.4);
        assert!(c.shed_feedback());
        assert!(c.is_paused());
    }

    #[test]
    fn first_step_sheds_feedback_before_pause() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // Warm up the window with borderline-heavy frames, then push exactly enough heavy
        // frames to trigger the first downgrade but observe shed_feedback at level 1.
        feed(&mut c, base, MIN_SAMPLES as u32, 30);
        // One evaluation past warm-up has moved to level >= 1.
        assert!(c.detail_factor() < 1.0);
        assert!(c.shed_feedback());
    }

    #[test]
    fn recovers_slowly_when_under_budget() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // Drive to the bottom + paused.
        feed(&mut c, base, WINDOW as u32, 30);
        assert!(c.is_paused());

        // Now feed fast frames (5 ms ≪ 10 ms). Need to flush the heavy samples out of the
        // window AND accumulate RECOVER_FRAMES under-budget evaluations.
        let later = base + Duration::from_secs(10_000);
        feed(&mut c, later, WINDOW as u32, 5); // flush window to all-fast
                                               // First recovery step: unpause.
        feed(&mut c, later, RECOVER_FRAMES, 5);
        assert!(!c.is_paused());
        // Continue recovering up the ladder.
        feed(&mut c, later, RECOVER_FRAMES * 4, 5);
        assert_eq!(c.detail_factor(), 1.0);
        assert!(!c.shed_feedback());
    }

    #[test]
    fn hysteresis_band_holds_level() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // 12 ms is between UP (10) and DOWN (14): should neither degrade nor recover.
        feed(&mut c, base, WINDOW as u32 * 2, 12);
        assert_eq!(c.detail_factor(), 1.0);
        assert!(!c.is_paused());
    }

    #[test]
    fn missing_begin_frame_is_ignored() {
        let mut c = DegradeController::new();
        let now = Instant::now();
        // end without begin → no sample, no panic.
        c.end_frame(now);
        assert_eq!(c.detail_factor(), 1.0);
    }

    #[test]
    fn p90_ignores_isolated_spikes() {
        let mut c = DegradeController::new();
        let base = Instant::now();
        // Fast frames with rare spikes well under the 10th-percentile tail (1 spike per 20
        // frames = 5% ⇒ below p90): the rolling p90 stays fast, so no degrade ever fires.
        for k in 0..(WINDOW as u32 * 3) {
            let t0 = base + Duration::from_millis(k as u64 * 1000);
            c.begin_frame(t0);
            let cost = if k % 20 == 0 { 40 } else { 5 };
            c.end_frame(t0 + Duration::from_millis(cost));
        }
        assert_eq!(c.detail_factor(), 1.0, "isolated spikes must not degrade");
    }
}
