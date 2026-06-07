//! SuperFlux onset detection.
//!
//! A pure, allocation-free onset detector that operates on per-hop magnitude
//! spectra. The classic SuperFlux recipe (Boeck & Widmer, 2013):
//!
//! 1. take the log-magnitude spectrum of each hop (`log1p`),
//! 2. apply a small max-filter across frequency to the *reference* spectrum
//!    (here 3 bins wide) so a slowly drifting partial does not register as flux,
//! 3. sum the positive differences between the current spectrum and the
//!    max-filtered reference taken `mu` frames earlier (`mu = 2`),
//! 4. peak-pick the resulting onset-detection function with a causal adaptive
//!    threshold (mean of a short past window times a sensitivity factor plus a
//!    small floor) and a refractory period.
//!
//! Everything is sized once at construction; [`Onset::process`] performs no heap
//! allocation and is safe to call from the audio thread.

/// Frequency max-filter half-width in bins (full width `2*MAX_FILTER_RADIUS+1`).
const MAX_FILTER_RADIUS: usize = 1; // -> 3-bin max filter
/// Frame lag for the flux reference (SuperFlux `mu`).
const FLUX_LAG: usize = 2;
/// Length of the past-flux window feeding the adaptive threshold.
const THRESHOLD_WINDOW: usize = 8;
/// Multiplier applied to the windowed mean flux for the threshold.
const THRESHOLD_SENSITIVITY: f32 = 2.0;
/// Constant floor added to the adaptive threshold (avoids firing on near-silence).
const THRESHOLD_FLOOR: f32 = 0.02;
/// Refractory period in seconds (~100 ms) — no second onset may fire inside it.
const REFRACTORY_SECS: f64 = 0.100;

/// The result of feeding one hop to the [`Onset`] detector.
#[derive(Clone, Copy, Debug, Default)]
pub struct OnsetResult {
    /// True when this hop is a detected onset.
    pub is_onset: bool,
    /// Peak-picker excess over the adaptive threshold (≥ 0); meaningful only
    /// when `is_onset` is true. Used as [`viz_core::BeatEvent::strength`].
    pub strength: f32,
}

/// Allocation-free SuperFlux detector. One instance per [`crate::Analyzer`].
pub struct Onset {
    bins: usize,
    /// Previous (max-filtered) spectra ring, `FLUX_LAG + 1` deep so we can read
    /// the reference `FLUX_LAG` frames back while writing the current frame.
    history: Vec<Vec<f32>>,
    /// Write cursor into `history`.
    hist_pos: usize,
    /// How many spectra have been pushed (warm-up gate).
    hist_filled: usize,
    /// Scratch for the current max-filtered log spectrum.
    max_filtered: Vec<f32>,
    /// Ring of recent flux values feeding the adaptive threshold.
    flux_window: [f32; THRESHOLD_WINDOW],
    flux_pos: usize,
    flux_filled: usize,
    /// Hop period in seconds (= hop / sample_rate), for the refractory clock.
    hop_secs: f64,
    /// Audio-clock time of the last accepted onset; `f64::NEG_INFINITY` = none.
    last_onset_t: f64,
}

impl Onset {
    /// Builds a detector for `bins`-bin magnitude spectra, given the hop period.
    pub fn new(bins: usize, hop_secs: f64) -> Self {
        let depth = FLUX_LAG + 1;
        Self {
            bins,
            history: (0..depth).map(|_| vec![0.0; bins]).collect(),
            hist_pos: 0,
            hist_filled: 0,
            max_filtered: vec![0.0; bins],
            flux_window: [0.0; THRESHOLD_WINDOW],
            flux_pos: 0,
            flux_filled: 0,
            hop_secs,
            last_onset_t: f64::NEG_INFINITY,
        }
    }

    /// Resets all internal state (used on capture restart / between test runs).
    pub fn reset(&mut self) {
        for h in &mut self.history {
            h.iter_mut().for_each(|v| *v = 0.0);
        }
        self.hist_pos = 0;
        self.hist_filled = 0;
        self.flux_window = [0.0; THRESHOLD_WINDOW];
        self.flux_pos = 0;
        self.flux_filled = 0;
        self.last_onset_t = f64::NEG_INFINITY;
    }

    /// Feeds one magnitude spectrum (`mag.len() == bins`) at audio-clock time `t`.
    ///
    /// Allocation-free. Returns whether this hop is an onset and its strength.
    pub fn process(&mut self, mag: &[f32], t: f64) -> OnsetResult {
        debug_assert_eq!(mag.len(), self.bins);

        // 1. log-magnitude of the current spectrum, max-filtered across frequency.
        //    `max_filtered[k] = max over [k-r, k+r] of log1p(mag)`.
        for (k, slot) in self.max_filtered.iter_mut().enumerate() {
            let lo = k.saturating_sub(MAX_FILTER_RADIUS);
            let hi = (k + MAX_FILTER_RADIUS).min(self.bins - 1);
            let mut m = f32::NEG_INFINITY;
            for &v in &mag[lo..=hi] {
                let lv = v.max(0.0).ln_1p();
                if lv > m {
                    m = lv;
                }
            }
            *slot = m;
        }

        // 2. positive flux of the *current raw log spectrum* against the
        //    max-filtered reference FLUX_LAG frames ago.
        let mut flux = 0.0f32;
        if self.hist_filled >= FLUX_LAG {
            // Index of the reference frame: FLUX_LAG slots before the write head.
            let depth = self.history.len();
            let ref_idx = (self.hist_pos + depth - FLUX_LAG) % depth;
            let reference = &self.history[ref_idx];
            for (&m, &r) in mag.iter().zip(reference.iter()) {
                let cur = m.max(0.0).ln_1p();
                let diff = cur - r;
                if diff > 0.0 {
                    flux += diff;
                }
            }
        }

        // 3. store the current max-filtered spectrum as future reference.
        self.history[self.hist_pos].copy_from_slice(&self.max_filtered);
        self.hist_pos = (self.hist_pos + 1) % self.history.len();
        if self.hist_filled < self.history.len() {
            self.hist_filled += 1;
        }

        // 4. adaptive threshold = sensitivity * mean(past flux window) + floor.
        let threshold = if self.flux_filled == 0 {
            THRESHOLD_FLOOR
        } else {
            let n = self.flux_filled as f32;
            let sum: f32 = self.flux_window.iter().take(self.flux_filled).sum();
            THRESHOLD_SENSITIVITY * (sum / n) + THRESHOLD_FLOOR
        };

        // Update the flux window AFTER computing the (causal) threshold.
        self.flux_window[self.flux_pos] = flux;
        self.flux_pos = (self.flux_pos + 1) % THRESHOLD_WINDOW;
        if self.flux_filled < THRESHOLD_WINDOW {
            self.flux_filled += 1;
        }

        // 5. peak pick: above threshold AND outside the refractory period.
        let _ = self.hop_secs; // documented field; refractory uses absolute t.
        let beyond_refractory = t - self.last_onset_t >= REFRACTORY_SECS;
        if flux > threshold && beyond_refractory {
            self.last_onset_t = t;
            OnsetResult {
                is_onset: true,
                strength: flux - threshold,
            }
        } else {
            OnsetResult::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_input_never_fires() {
        let mut o = Onset::new(64, 512.0 / 48_000.0);
        let mag = vec![0.0f32; 64];
        for i in 0..200 {
            let t = i as f64 * (512.0 / 48_000.0);
            assert!(!o.process(&mag, t).is_onset);
        }
    }

    #[test]
    fn step_increase_fires_once_then_refractory() {
        let mut o = Onset::new(64, 512.0 / 48_000.0);
        let hop = 512.0 / 48_000.0;
        let low = vec![0.01f32; 64];
        let high = vec![5.0f32; 64];
        let mut onsets = 0;
        // Warm up on low energy.
        for i in 0..16 {
            o.process(&low, i as f64 * hop);
        }
        // A single sustained jump should produce exactly one onset (the rise),
        // not one per subsequent frame, because the reference catches up.
        for i in 16..40 {
            if o.process(&high, i as f64 * hop).is_onset {
                onsets += 1;
            }
        }
        assert_eq!(onsets, 1, "sustained step should fire exactly once");
    }
}
