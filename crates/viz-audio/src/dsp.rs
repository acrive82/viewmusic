//! Pure DSP analyzer — no Core Audio dependency.
//!
//! [`Analyzer`] turns a stream of mono f32 samples (at [`viz_core::SAMPLE_RATE`])
//! into [`FeatureFrame`]s on every hop. It is deliberately hardware-free so the
//! synthetic-signal tests in `tests/synthetic.rs` exercise the exact same code
//! the audio thread runs (so the analyzer is fully deterministic and testable).
//!
//! Pipeline per hop (10.67 ms at 48 kHz):
//! * sliding window N = 1024, Hann-windowed, FFT via `realfft` with all buffers
//!   pre-allocated and `process_with_scratch` (zero per-hop heap allocation);
//! * 513 power bins folded into 48 log-spaced bands
//!   (`f_k = 30 * (16000/30)^(k/47)`), with per-band AGC + asymmetric smoothing
//!   into `bands[48] ∈ 0..1`;
//! * low/mid/high aggregates from the same powers;
//! * RMS-over-hop fed through an EWMA into `energy ∈ 0..1`;
//! * SuperFlux onset detection ([`crate::onset`]) → [`BeatEvent`]s + the
//!   decaying `beat` pulse;
//! * a monotonic sample counter drives the audio clock `t = samples / 48 000`
//!   and a sustained-silence flag.

use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;
use viz_core::{BeatEvent, FeatureFrame, BAND_COUNT, SAMPLE_RATE, WAVEFORM_LEN};

use crate::onset::Onset;

/// FFT / analysis window length.
pub const FFT_SIZE: usize = 1024;
/// Hop size between consecutive windows.
pub const HOP_SIZE: usize = 512;
/// Number of magnitude bins produced by a real FFT of [`FFT_SIZE`].
pub const SPECTRUM_BINS: usize = FFT_SIZE / 2 + 1; // 513

/// Lowest band edge (Hz).
const BAND_LO_HZ: f64 = 30.0;
/// Highest band edge (Hz).
const BAND_HI_HZ: f64 = 16_000.0;

/// Aggregate cutoffs (Hz).
const LOW_HI_HZ: f64 = 250.0;
const MID_HI_HZ: f64 = 4_000.0;

// --- AGC / smoothing constants ---------------------------------------------

/// Per-band AGC peak decay applied every hop. Chosen so the running peak halves
/// in ~2.5 s: `decay^(2.5 / hop_secs) = 0.5` with hop_secs ≈ 0.01067 →
/// `decay = 0.5^(hop_secs / 2.5) ≈ 0.9970`.
const AGC_PEAK_DECAY: f32 = 0.9970;
/// Smallest peak the AGC will divide by — clamps the noise-floor gain so silence
/// is not amplified into visible bands.
const AGC_PEAK_FLOOR: f32 = 1.0e-4;
/// Fast attack coefficient for band smoothing (rises quickly toward new value).
const BAND_ATTACK: f32 = 0.35;
/// Slow release coefficient for band smoothing (falls slowly).
const BAND_RELEASE: f32 = 0.87;

/// EWMA coefficient for the energy aggregate.
const ENERGY_EWMA: f32 = 0.80;
/// Scale mapping raw RMS to the 0..1 energy range. 0 dBFS sine has RMS ≈ 0.707;
/// this maps that to ~1.0 (then clamped).
const ENERGY_SCALE: f32 = 1.414;

/// `beat` pulse exponential decay per hop (~0.85).
const BEAT_DECAY: f32 = 0.85;

// --- Silence detection ------------------------------------------------------

/// RMS below this for the whole silence window ⇒ silence.
const SILENCE_RMS: f32 = 1.0e-4;
/// Sustained-silence duration before the flag latches (seconds).
const SILENCE_SECS: f64 = 0.5;

/// Per-hop output bundle returned by [`Analyzer`] internals (test convenience).
struct HopOutput {
    frame: FeatureFrame,
    beat: Option<BeatEvent>,
}

/// Precomputed mapping of each FFT bin to a band index (or `usize::MAX` if the
/// bin is outside the 30 Hz–16 kHz range and ignored).
fn build_bin_to_band() -> Vec<usize> {
    let bin_hz = SAMPLE_RATE as f64 / FFT_SIZE as f64; // 46.875 Hz
    let ratio = BAND_HI_HZ / BAND_LO_HZ;
    // edges[k] = lower frequency of band k; band k spans [edges[k], edges[k+1]).
    let edges: Vec<f64> = (0..=BAND_COUNT)
        .map(|k| {
            if k == BAND_COUNT {
                // upper edge of the last band -> Nyquist so high content lands in band 47.
                (SAMPLE_RATE as f64 / 2.0).max(BAND_HI_HZ)
            } else {
                BAND_LO_HZ * ratio.powf(k as f64 / (BAND_COUNT as f64 - 1.0))
            }
        })
        .collect();

    let mut map = vec![usize::MAX; SPECTRUM_BINS];
    for (bin, slot) in map.iter_mut().enumerate() {
        let f = bin as f64 * bin_hz;
        if f < edges[0] || f >= edges[BAND_COUNT] {
            continue;
        }
        // Linear scan (48 bands) — only runs once at construction.
        for k in 0..BAND_COUNT {
            if f >= edges[k] && f < edges[k + 1] {
                *slot = k;
                break;
            }
        }
    }
    map
}

/// The pure analyzer. Construct once, then drive with [`Analyzer::push_samples`].
pub struct Analyzer {
    // FFT machinery (all pre-allocated).
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,        // Hann window, length FFT_SIZE
    ring: Vec<f32>,          // sliding sample ring, length FFT_SIZE
    ring_filled: usize,      // samples written before the ring first fills
    write_pos: usize,        // next write index into `ring`
    since_hop: usize,        // samples accumulated since the last hop
    fft_in: Vec<f32>,        // windowed frame (FFT input scratch)
    fft_out: Vec<Complex32>, // spectrum (length SPECTRUM_BINS)
    fft_scratch: Vec<Complex32>,
    power: Vec<f32>, // per-bin power, length SPECTRUM_BINS
    mag: Vec<f32>,   // per-bin magnitude for the onset detector

    // Band mapping & state.
    bin_to_band: Vec<usize>,
    band_count_per: [u32; BAND_COUNT], // bins per band (for averaging)
    band_peak: [f32; BAND_COUNT],      // AGC running peak per band
    band_smooth: [f32; BAND_COUNT],    // smoothed output per band

    // Aggregate cutoffs in bins.
    low_hi_bin: usize,
    mid_hi_bin: usize,

    // Energy / loudness.
    energy_ewma: f32,

    // Onset.
    onset: Onset,
    beat_pulse: f32,
    beat_count: u32,

    // Clock & silence.
    samples_processed: u64,
    silence_run: u64, // consecutive samples whose hop RMS was sub-silence
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    /// Builds a fully pre-allocated analyzer. No allocation happens afterwards.
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_in = fft.make_input_vec();
        let fft_out = fft.make_output_vec();
        let fft_scratch = fft.make_scratch_vec();
        debug_assert_eq!(fft_out.len(), SPECTRUM_BINS);

        // Periodic Hann window (matches typical STFT analysis).
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|n| {
                let x = std::f32::consts::TAU * n as f32 / FFT_SIZE as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();

        let bin_to_band = build_bin_to_band();
        let mut band_count_per = [0u32; BAND_COUNT];
        for &b in &bin_to_band {
            if b != usize::MAX {
                band_count_per[b] += 1;
            }
        }

        let bin_hz = SAMPLE_RATE as f64 / FFT_SIZE as f64;
        let low_hi_bin = (LOW_HI_HZ / bin_hz).round() as usize;
        let mid_hi_bin = (MID_HI_HZ / bin_hz).round() as usize;

        let hop_secs = HOP_SIZE as f64 / SAMPLE_RATE as f64;

        Self {
            fft,
            window,
            ring: vec![0.0; FFT_SIZE],
            ring_filled: 0,
            write_pos: 0,
            since_hop: 0,
            fft_in,
            fft_out,
            fft_scratch,
            power: vec![0.0; SPECTRUM_BINS],
            mag: vec![0.0; SPECTRUM_BINS],
            bin_to_band,
            band_count_per,
            band_peak: [AGC_PEAK_FLOOR; BAND_COUNT],
            band_smooth: [0.0; BAND_COUNT],
            low_hi_bin,
            mid_hi_bin,
            energy_ewma: 0.0,
            onset: Onset::new(SPECTRUM_BINS, hop_secs),
            beat_pulse: 0.0,
            beat_count: 0,
            samples_processed: 0,
            silence_run: 0,
        }
    }

    /// Resets all running state (clock, AGC, onset history) to start fresh.
    pub fn reset(&mut self) {
        self.ring.iter_mut().for_each(|v| *v = 0.0);
        self.ring_filled = 0;
        self.write_pos = 0;
        self.since_hop = 0;
        self.band_peak = [AGC_PEAK_FLOOR; BAND_COUNT];
        self.band_smooth = [0.0; BAND_COUNT];
        self.energy_ewma = 0.0;
        self.onset.reset();
        self.beat_pulse = 0.0;
        self.beat_count = 0;
        self.samples_processed = 0;
        self.silence_run = 0;
    }

    /// Current audio-clock time in seconds (`samples_processed / SAMPLE_RATE`).
    pub fn clock_secs(&self) -> f64 {
        self.samples_processed as f64 / SAMPLE_RATE as f64
    }

    /// Feeds a block of mono samples. For every completed hop, `on_hop` is called
    /// with the freshly computed [`FeatureFrame`] and the (0 or 1) beat events
    /// produced on that hop.
    ///
    /// Allocation-free in the steady state: all buffers are pre-sized in
    /// [`Analyzer::new`]. The sample counter advances for *every* pushed sample,
    /// including zero-fill, so the clock stays aligned through dropouts (keeping
    /// the audio clock deterministic).
    pub fn push_samples(
        &mut self,
        mono: &[f32],
        mut on_hop: impl FnMut(&FeatureFrame, &[BeatEvent]),
    ) {
        for &s in mono {
            self.ring[self.write_pos] = s;
            self.write_pos = (self.write_pos + 1) % FFT_SIZE;
            if self.ring_filled < FFT_SIZE {
                self.ring_filled += 1;
            }
            self.samples_processed += 1;
            self.since_hop += 1;

            if self.since_hop >= HOP_SIZE && self.ring_filled >= FFT_SIZE {
                self.since_hop = 0;
                let out = self.analyze_hop();
                match out.beat {
                    Some(ev) => on_hop(&out.frame, std::slice::from_ref(&ev)),
                    None => on_hop(&out.frame, &[]),
                }
            } else if self.since_hop >= HOP_SIZE {
                // Ring not yet full (very first window) — still consume the hop
                // boundary so timing does not drift, but emit nothing.
                self.since_hop = 0;
            }
        }
    }

    /// Runs the analysis on the current window. Allocation-free.
    fn analyze_hop(&mut self) -> HopOutput {
        // 1. Copy the ring (oldest→newest) into the windowed FFT input, and
        //    capture the most-recent WAVEFORM_LEN raw samples for the waveform.
        //    The oldest sample is at `write_pos` (next-to-overwrite).
        let mut frame = FeatureFrame::default();
        let mut rms_acc = 0.0f64;
        for i in 0..FFT_SIZE {
            let idx = (self.write_pos + i) % FFT_SIZE;
            let raw = self.ring[idx];
            self.fft_in[i] = raw * self.window[i];
            rms_acc += (raw as f64) * (raw as f64);
        }
        // Waveform = the most recent WAVEFORM_LEN raw samples of the window.
        for (j, slot) in frame.waveform.iter_mut().enumerate() {
            let i = FFT_SIZE - WAVEFORM_LEN + j;
            let idx = (self.write_pos + i) % FFT_SIZE;
            *slot = self.ring[idx];
        }

        // 2. FFT (zero allocation).
        self.fft
            .process_with_scratch(&mut self.fft_in, &mut self.fft_out, &mut self.fft_scratch)
            .expect("FFT lengths are fixed at construction");

        // 3. Per-bin power & magnitude.
        let norm = 1.0 / FFT_SIZE as f32;
        for (k, c) in self.fft_out.iter().enumerate() {
            let p = (c.re * c.re + c.im * c.im) * norm * norm;
            self.power[k] = p;
            self.mag[k] = p.sqrt();
        }

        // 4. Fold bins into 48 band powers (mean power per band).
        let mut band_raw = [0.0f32; BAND_COUNT];
        for (bin, &b) in self.bin_to_band.iter().enumerate() {
            if b != usize::MAX {
                band_raw[b] += self.power[bin];
            }
        }
        // 5. Per-band normalize → AGC → asymmetric smoothing → bands[48] ∈ 0..1.
        for (k, raw) in band_raw.iter_mut().enumerate() {
            if self.band_count_per[k] > 0 {
                *raw /= self.band_count_per[k] as f32;
            }
            // Use amplitude (sqrt of mean power) for a perceptually flatter scale.
            let amp = raw.sqrt();

            // Decay the running peak, then raise it to the new value if larger.
            let peak = (self.band_peak[k] * AGC_PEAK_DECAY).max(AGC_PEAK_FLOOR);
            let peak = peak.max(amp);
            self.band_peak[k] = peak;

            let target = (amp / peak).clamp(0.0, 1.0);
            let prev = self.band_smooth[k];
            // Asymmetric one-pole: fast attack (rising), slow release (falling).
            // `coeff` is the retention factor toward `prev`.
            let smoothed = if target > prev {
                prev + (target - prev) * BAND_ATTACK
            } else {
                prev * BAND_RELEASE + target * (1.0 - BAND_RELEASE)
            };
            self.band_smooth[k] = smoothed;
            frame.bands[k] = sanitize(smoothed).clamp(0.0, 1.0);
        }

        // 6. Aggregates low/mid/high from the same powers (amplitude scale).
        let (mut low_p, mut mid_p, mut high_p) = (0.0f32, 0.0f32, 0.0f32);
        let (mut low_n, mut mid_n, mut high_n) = (0u32, 0u32, 0u32);
        for (bin, &p) in self.power.iter().enumerate() {
            if bin == 0 {
                continue; // skip DC
            }
            if bin <= self.low_hi_bin {
                low_p += p;
                low_n += 1;
            } else if bin <= self.mid_hi_bin {
                mid_p += p;
                mid_n += 1;
            } else {
                high_p += p;
                high_n += 1;
            }
        }
        frame.low = aggregate_level(low_p, low_n);
        frame.mid = aggregate_level(mid_p, mid_n);
        frame.high = aggregate_level(high_p, high_n);

        // 7. Energy from hop RMS through an EWMA.
        let rms = (rms_acc / FFT_SIZE as f64).sqrt() as f32;
        let raw_energy = (rms * ENERGY_SCALE).clamp(0.0, 1.0);
        self.energy_ewma = self.energy_ewma * ENERGY_EWMA + raw_energy * (1.0 - ENERGY_EWMA);
        frame.energy = sanitize(self.energy_ewma).clamp(0.0, 1.0);

        // 8. Onset detection on the magnitude spectrum.
        let t = self.clock_secs();
        let onset = self.onset.process(&self.mag, t);
        let mut beat_ev = None;
        if onset.is_onset {
            self.beat_pulse = 1.0;
            self.beat_count += 1;
            beat_ev = Some(BeatEvent {
                t,
                strength: onset.strength.max(0.0),
            });
        } else {
            self.beat_pulse *= BEAT_DECAY;
        }
        frame.beat = sanitize(self.beat_pulse).clamp(0.0, 1.0);
        frame.beat_count = self.beat_count;

        // 9. Clock + silence flag.
        frame.t = t;
        let silence_hop_samples = HOP_SIZE as u64;
        if rms < SILENCE_RMS {
            self.silence_run = self.silence_run.saturating_add(silence_hop_samples);
        } else {
            self.silence_run = 0;
        }
        let silence_thresh = (SILENCE_SECS * SAMPLE_RATE as f64) as u64;
        frame.silence = self.silence_run >= silence_thresh;

        HopOutput {
            frame,
            beat: beat_ev,
        }
    }
}

/// Maps an accumulated power sum over `n` bins to a 0..1 level via amplitude.
fn aggregate_level(power_sum: f32, n: u32) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let amp = (power_sum / n as f32).sqrt();
    // Mild fixed gain so a 0 dBFS tone in-band reads near 1.0 without an AGC.
    sanitize(amp * 2.0).clamp(0.0, 1.0)
}

/// Replaces NaN/±Inf with 0.0 (sanitizes non-finite values at the source).
#[inline]
fn sanitize(x: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_band_map_covers_all_bands() {
        let map = build_bin_to_band();
        let mut counts = [0u32; BAND_COUNT];
        for &b in &map {
            if b != usize::MAX {
                counts[b] += 1;
            }
        }
        // Higher bands (wider in Hz) must contain at least one bin.
        for (k, &c) in counts.iter().enumerate().skip(BAND_COUNT / 2) {
            assert!(c >= 1, "band {k} has no bins");
        }
    }

    #[test]
    fn clock_advances_with_zero_fill() {
        let mut a = Analyzer::new();
        let zeros = vec![0.0f32; HOP_SIZE * 4];
        a.push_samples(&zeros, |_, _| {});
        assert_eq!(a.samples_processed, (HOP_SIZE * 4) as u64);
        assert!((a.clock_secs() - (HOP_SIZE * 4) as f64 / SAMPLE_RATE as f64).abs() < 1e-12);
    }
}
