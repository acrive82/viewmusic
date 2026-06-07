//! Optional performance HUD (behind the `perf-hud` cargo feature).
//!
//! A small top-left egui window (kept clear of the top-right overlay) showing:
//! rolling render FPS, frame cost percentiles (p50 / p90, ms), the wall-clock age
//! of the latest feature frame (ms), and the degrade state. It is a measurement
//! aid for the audio-to-photon perf gate (see `tests/perf/measure.sh`).
//!
//! `PerfHud` tracks its own per-frame wall-clock timing in a fixed ring buffer
//! (allocation-free after construction). The visualizer calls
//! [`PerfHud::record_frame`] once per presented frame, then renders
//! [`PerfHud::snapshot`] via [`draw`].

use std::time::Instant;

use crate::viz::HudStats;

/// Rolling window of frame timings (~2 s at 60 Hz).
const WINDOW: usize = 120;

/// Per-frame timing accumulator for the HUD.
pub struct PerfHud {
    /// Ring of frame intervals (ms) between consecutive presented frames.
    intervals: [f64; WINDOW],
    /// Valid sample count (saturates at `WINDOW`).
    filled: usize,
    /// Write cursor.
    cursor: usize,
    /// Instant of the previous recorded frame.
    last: Option<Instant>,
}

impl Default for PerfHud {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfHud {
    /// Creates an empty HUD accumulator.
    pub fn new() -> Self {
        Self {
            intervals: [0.0; WINDOW],
            filled: 0,
            cursor: 0,
            last: None,
        }
    }

    /// Records a presented frame at wall-clock `now`, accumulating the interval
    /// since the previous frame.
    pub fn record_frame(&mut self, now: Instant) {
        if let Some(prev) = self.last {
            let ms = now.saturating_duration_since(prev).as_secs_f64() * 1000.0;
            self.intervals[self.cursor] = ms;
            self.cursor = (self.cursor + 1) % WINDOW;
            if self.filled < WINDOW {
                self.filled += 1;
            }
        }
        self.last = Some(now);
    }

    /// Computes the current HUD display values from the timing window.
    pub fn snapshot(&self, stats: HudStats) -> HudSnapshot {
        let (p50, p90, fps) = self.percentiles();
        HudSnapshot {
            fps,
            frame_p50_ms: p50,
            frame_p90_ms: p90,
            feature_age_ms: stats.feature_age_ms,
            detail_factor: stats.detail_factor,
            shed_feedback: stats.shed_feedback,
            paused: stats.paused,
        }
    }

    /// Returns `(p50_ms, p90_ms, fps)` over the valid window (zeros when empty).
    fn percentiles(&self) -> (f64, f64, f64) {
        let n = self.filled;
        if n == 0 {
            return (0.0, 0.0, 0.0);
        }
        let mut scratch = [0.0f64; WINDOW];
        scratch[..n].copy_from_slice(&self.intervals[..n]);
        let s = &mut scratch[..n];
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| -> f64 {
            let idx = (((n as f64) * p).ceil() as usize)
                .saturating_sub(1)
                .min(n - 1);
            s[idx]
        };
        let p50 = pct(0.5);
        let p90 = pct(0.9);
        // FPS from the median interval (robust to spikes).
        let fps = if p50 > 0.0 { 1000.0 / p50 } else { 0.0 };
        (p50, p90, fps)
    }
}

/// The values rendered by the HUD this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct HudSnapshot {
    /// Rolling render FPS (from the median frame interval).
    pub fps: f64,
    /// Median frame interval (ms).
    pub frame_p50_ms: f64,
    /// 90th-percentile frame interval (ms).
    pub frame_p90_ms: f64,
    /// Wall-clock age of the latest feature frame (ms).
    pub feature_age_ms: f64,
    /// Degrade detail factor (1.0 = full).
    pub detail_factor: f32,
    /// Whether feedback/trails are shed.
    pub shed_feedback: bool,
    /// Whether element evaluation is paused.
    pub paused: bool,
}

/// Draws the HUD as a top-left egui window (away from the top-right overlay).
pub fn draw(ctx: &egui::Context, s: &HudSnapshot) {
    egui::Window::new("perf")
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .resizable(false)
        .collapsible(true)
        .title_bar(true)
        .show(ctx, |ui| {
            ui.monospace(format!("fps      {:6.1}", s.fps));
            ui.monospace(format!("frame ms p50 {:5.2}", s.frame_p50_ms));
            ui.monospace(format!("frame ms p90 {:5.2}", s.frame_p90_ms));
            ui.monospace(format!("feat age {:6.1} ms", s.feature_age_ms));
            ui.monospace(format!("detail   {:5.2}", s.detail_factor));
            let state = if s.paused {
                "PAUSED"
            } else if s.shed_feedback {
                "shed-trails"
            } else {
                "full"
            };
            ui.monospace(format!("degrade  {state}"));
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn percentiles_zero_when_empty() {
        let hud = PerfHud::new();
        let snap = hud.snapshot(HudStats::default());
        assert_eq!(snap.fps, 0.0);
        assert_eq!(snap.frame_p50_ms, 0.0);
    }

    #[test]
    fn steady_60hz_reports_about_60_fps() {
        let mut hud = PerfHud::new();
        let base = Instant::now();
        // 60 frames at a steady ~16.67 ms cadence.
        for k in 0..61u32 {
            hud.record_frame(base + Duration::from_micros(16_667 * k as u64));
        }
        let snap = hud.snapshot(HudStats::default());
        assert!(
            (snap.fps - 60.0).abs() < 1.0,
            "expected ~60 fps, got {}",
            snap.fps
        );
        assert!((snap.frame_p50_ms - 16.667).abs() < 0.5);
    }
}
