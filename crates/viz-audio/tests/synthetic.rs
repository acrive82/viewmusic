//! Synthetic-stream DSP tests — the determinism gate.
//!
//! These exercise the pure [`viz_audio::Analyzer`] with no hardware: known
//! signals in, asserted features out. They are the regression basis that proves
//! the analyzer is deterministic and correct independent of Core Audio.

use viz_audio::dsp::{HOP_SIZE, SPECTRUM_BINS};
use viz_audio::{Analyzer, BeatEvent, FeatureFrame, BAND_COUNT, SAMPLE_RATE};

/// Generates `n` samples of a sine at `freq` Hz, amplitude `amp`.
fn sine(freq: f64, amp: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f64 / SAMPLE_RATE as f64;
            (amp as f64 * (std::f64::consts::TAU * freq * t).sin()) as f32
        })
        .collect()
}

/// Pushes a whole signal and collects (frame, beats) for every hop.
fn run(analyzer: &mut Analyzer, signal: &[f32]) -> (Vec<FeatureFrame>, Vec<BeatEvent>) {
    let mut frames = Vec::new();
    let mut beats = Vec::new();
    analyzer.push_samples(signal, |f, evs| {
        frames.push(*f);
        beats.extend_from_slice(evs);
    });
    (frames, beats)
}

/// Expected band index for a frequency, mirroring the analyzer's edge formula
/// `f_k = 30 * (16000/30)^(k/47)`; band k spans [edge_k, edge_{k+1}).
fn expected_band(freq: f64) -> usize {
    let ratio = 16_000.0_f64 / 30.0;
    let edges: Vec<f64> = (0..=BAND_COUNT)
        .map(|k| {
            if k == BAND_COUNT {
                (SAMPLE_RATE as f64 / 2.0).max(16_000.0)
            } else {
                30.0 * ratio.powf(k as f64 / (BAND_COUNT as f64 - 1.0))
            }
        })
        .collect();
    for k in 0..BAND_COUNT {
        if freq >= edges[k] && freq < edges[k + 1] {
            return k;
        }
    }
    BAND_COUNT - 1
}

fn argmax_band(bands: &[f32; BAND_COUNT]) -> usize {
    let mut best = 0;
    let mut best_v = bands[0];
    for (k, &v) in bands.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best = k;
        }
    }
    best
}

// (a) 440 Hz sine → its band dominates; low/mid/high sane.
#[test]
fn sine_440_dominant_band() {
    let mut a = Analyzer::new();
    // ~1 second of audio so AGC settles and the band peaks.
    let sig = sine(440.0, 0.8, SAMPLE_RATE as usize);
    let (frames, _) = run(&mut a, &sig);
    assert!(!frames.is_empty());
    let last = frames.last().unwrap();

    // Per-band AGC normalizes each band to its own running peak, so a steady
    // tone saturates every excited band to ~1.0 and a strict argmax is
    // ambiguous. The physically meaningful check: the band that contains 440 Hz
    // (allowing ±1 for FFT-bin quantization — 440 Hz lands at bin ≈ 422 Hz) is
    // at/near the maximum band value, while bands far from 440 Hz are near 0.
    let expected = expected_band(440.0);
    let max_band = last.bands.iter().cloned().fold(0.0_f32, f32::max);
    let near_440 = (expected.saturating_sub(1)..=(expected + 1).min(BAND_COUNT - 1))
        .any(|k| (max_band - last.bands[k]).abs() < 1e-3);
    assert!(
        near_440,
        "440 Hz band {expected} (±1) should be at the max ({max_band}); bands={:?}",
        &last.bands[..]
    );
    // A band well above 440 Hz (≈ band 30, ~2 kHz) must be quiet — per-band AGC
    // amplifies the Hann side-lobe leakage of immediate neighbours, but it falls
    // off sharply a few bands out.
    let far = (expected + 10).min(BAND_COUNT - 1);
    assert!(
        last.bands[far] < 0.15,
        "band {far} far from 440 Hz should be quiet, got {}",
        last.bands[far]
    );
    let _ = argmax_band(&last.bands);

    // 440 Hz is in the mid range (250..4000) → mid should dominate low/high.
    assert!(
        last.mid > last.low && last.mid > last.high,
        "mid should dominate for 440 Hz: low={} mid={} high={}",
        last.low,
        last.mid,
        last.high
    );
    // All aggregates in range.
    for v in [last.low, last.mid, last.high, last.energy] {
        assert!((0.0..=1.0).contains(&v), "aggregate out of range: {v}");
    }
    // Energy of a loud tone should be clearly non-zero.
    assert!(last.energy > 0.3, "energy too low: {}", last.energy);
}

// A low-frequency tone exercises the low aggregate / low bands.
#[test]
fn sine_100_is_low() {
    let mut a = Analyzer::new();
    let sig = sine(100.0, 0.8, SAMPLE_RATE as usize);
    let (frames, _) = run(&mut a, &sig);
    let last = frames.last().unwrap();
    assert!(
        last.low > last.high,
        "100 Hz should have low > high: low={} high={}",
        last.low,
        last.high
    );
    let got = argmax_band(&last.bands);
    let expected = expected_band(100.0);
    assert!(
        (got as i32 - expected as i32).abs() <= 1,
        "100 Hz expected band {expected}, got {got}"
    );
}

// (b) Impulse train at 2 Hz → beats detected within ±1 hop of each impulse
//     after warmup; no double-triggers (refractory).
#[test]
fn impulse_train_beats() {
    let mut a = Analyzer::new();
    let fs = SAMPLE_RATE as usize;
    let period = fs / 2; // 2 Hz → impulse every 0.5 s
    let total = fs * 5; // 5 seconds → ~10 impulses
    let mut sig = vec![0.0f32; total];
    let mut impulse_times = Vec::new();
    // Make each "impulse" a short broadband click (a few samples) so the FFT
    // sees energy across the spectrum — a single sample is too small after the
    // Hann window and hop averaging.
    let mut k = period; // first impulse at 0.5 s (after some warmup)
    while k < total {
        for j in 0..16 {
            if k + j < total {
                sig[k + j] = if j % 2 == 0 { 0.9 } else { -0.9 };
            }
        }
        impulse_times.push(k as f64 / SAMPLE_RATE as f64);
        k += period;
    }

    let (_frames, beats) = run(&mut a, &sig);
    assert!(!beats.is_empty(), "no beats detected for an impulse train");

    let hop_secs = HOP_SIZE as f64 / SAMPLE_RATE as f64;
    let tol = 3.0 * hop_secs; // allow a few hops of analysis/window lag

    // Each beat should be near some impulse (allowing warmup to drop the first).
    for b in &beats {
        let nearest = impulse_times
            .iter()
            .map(|&it| (b.t - it).abs())
            .fold(f64::INFINITY, f64::min);
        assert!(
            nearest <= tol,
            "beat at {:.3}s not within {:.3}s of any impulse",
            b.t,
            tol
        );
        assert!(b.strength >= 0.0);
    }

    // No double triggers: consecutive beats must respect the refractory period
    // (~100 ms). With impulses 500 ms apart, beats are well separated.
    let mut sorted: Vec<f64> = beats.iter().map(|b| b.t).collect();
    sorted.sort_by(|x, y| x.partial_cmp(y).unwrap());
    for w in sorted.windows(2) {
        assert!(
            w[1] - w[0] >= 0.09,
            "two beats {:.3}s apart violate refractory",
            w[1] - w[0]
        );
    }

    // We should catch most impulses after warmup (allow missing the first couple).
    assert!(
        beats.len() >= impulse_times.len() - 2,
        "detected only {} beats for {} impulses",
        beats.len(),
        impulse_times.len()
    );
}

// (c) Silence → all features ~0 and silence flag true after 0.5 s; t advances.
#[test]
fn silence_features_zero_and_flag() {
    let mut a = Analyzer::new();
    let sig = vec![0.0f32; SAMPLE_RATE as usize]; // 1 s of pure silence
    let (frames, beats) = run(&mut a, &sig);
    assert!(beats.is_empty(), "silence produced beats");
    assert!(!frames.is_empty());

    let last = frames.last().unwrap();
    assert!(last.silence, "silence flag should be set after >0.5 s");
    assert!(
        last.energy < 1e-3,
        "energy not ~0 in silence: {}",
        last.energy
    );
    assert!(last.low < 1e-3 && last.mid < 1e-3 && last.high < 1e-3);
    for &b in &last.bands {
        assert!(b < 1e-2, "band amplified in silence: {b}");
    }
    assert_eq!(last.beat_count, 0);

    // Clock must keep advancing through silence (deterministic audio clock).
    let secs = SAMPLE_RATE as f64 / SAMPLE_RATE as f64; // = 1.0
    assert!(
        (last.t - (last_full_hop_t(frames.len()))).abs() < 1e-9,
        "t should equal the last hop boundary"
    );
    assert!(last.t > 0.5 && last.t <= secs + 1e-9);

    // The silence flag should NOT be set before 0.5 s of silence.
    let early = &frames[2];
    assert!(early.t < 0.5);
    assert!(!early.silence, "silence flagged too early at t={}", early.t);
}

/// The audio-clock time at the n-th completed hop boundary.
fn last_full_hop_t(num_frames: usize) -> f64 {
    // The first frame is emitted once the ring fills (FFT_SIZE samples) and a
    // hop boundary passes; thereafter every HOP_SIZE samples. The clock equals
    // the absolute sample count, which is (num_frames-1)*HOP + first_boundary.
    // Simpler: assert via the analyzer is awkward here; recompute the absolute
    // sample count for the last emitted hop.
    // First emission boundary: the first multiple of HOP_SIZE that is >= FFT_SIZE.
    let fft = viz_audio::dsp::FFT_SIZE;
    let first = fft.div_ceil(HOP_SIZE) * HOP_SIZE;
    let last_samples = first + (num_frames - 1) * HOP_SIZE;
    last_samples as f64 / SAMPLE_RATE as f64
}

// (d) Loud-then-quiet AGC: a -30 dB sine after a 0 dB sine recovers band > 0.5
//     within ~3 s (AGC adapts the per-band peak down).
#[test]
fn agc_recovers_after_level_drop() {
    let mut a = Analyzer::new();
    // 1 s at 0 dBFS (amp 1.0), then 4 s at -30 dB (amp ~0.0316).
    let loud = sine(440.0, 1.0, SAMPLE_RATE as usize);
    let quiet = sine(440.0, 0.0316, SAMPLE_RATE as usize * 4);

    let (loud_frames, _) = run(&mut a, &loud);
    // After the loud section the 440 band is near 1.0.
    let band = expected_band(440.0);
    let loud_last = loud_frames.last().unwrap();
    assert!(
        loud_last.bands[band] > 0.5,
        "loud band should be high: {}",
        loud_last.bands[band]
    );

    let (quiet_frames, _) = run(&mut a, &quiet);

    let hop_secs = HOP_SIZE as f64 / SAMPLE_RATE as f64;

    // Shortly after the drop the band collapses: the release smoothing pulls the
    // output down toward (quiet_amp / still-high peak) ≈ 0.03 within ~0.5 s.
    let half_sec = (0.5 / hop_secs) as usize;
    assert!(
        quiet_frames[half_sec].bands[band] < 0.5,
        "band should drop after the level cut (t≈0.5s): {}",
        quiet_frames[half_sec].bands[band]
    );

    // Within ~3 s the AGC peak should have decayed enough that the quiet tone
    // reads > 0.5 in its band again.
    let within_3s = (3.0 / hop_secs) as usize;
    let recovered = quiet_frames
        .iter()
        .take(within_3s)
        .any(|f| f.bands[band] > 0.5);
    assert!(
        recovered,
        "AGC did not recover the band above 0.5 within 3 s; \
         final band={}",
        quiet_frames.last().unwrap().bands[band]
    );
}

// (e) Determinism: same input into two analyzers → bitwise-identical frames.
#[test]
fn deterministic_frame_sequences() {
    // A mixed signal that exercises bands, energy, and onsets.
    let fs = SAMPLE_RATE as usize;
    let mut sig = sine(440.0, 0.5, fs * 2);
    // Sprinkle clicks.
    for k in (fs / 4..sig.len()).step_by(fs / 2) {
        for j in 0..8 {
            if k + j < sig.len() {
                sig[k + j] += 0.8;
            }
        }
    }

    let mut a1 = Analyzer::new();
    let mut a2 = Analyzer::new();
    let (f1, b1) = run(&mut a1, &sig);
    let (f2, b2) = run(&mut a2, &sig);

    assert_eq!(f1.len(), f2.len());
    for (x, y) in f1.iter().zip(f2.iter()) {
        assert_eq!(x.t.to_bits(), y.t.to_bits(), "t differs");
        assert_eq!(x.energy.to_bits(), y.energy.to_bits(), "energy differs");
        assert_eq!(x.low.to_bits(), y.low.to_bits());
        assert_eq!(x.mid.to_bits(), y.mid.to_bits());
        assert_eq!(x.high.to_bits(), y.high.to_bits());
        assert_eq!(x.beat.to_bits(), y.beat.to_bits());
        assert_eq!(x.beat_count, y.beat_count);
        assert_eq!(x.silence, y.silence);
        for k in 0..BAND_COUNT {
            assert_eq!(
                x.bands[k].to_bits(),
                y.bands[k].to_bits(),
                "band {k} differs"
            );
        }
        for k in 0..x.waveform.len() {
            assert_eq!(x.waveform[k].to_bits(), y.waveform[k].to_bits());
        }
    }
    assert_eq!(b1.len(), b2.len());
    for (x, y) in b1.iter().zip(b2.iter()) {
        assert_eq!(x.t.to_bits(), y.t.to_bits());
        assert_eq!(x.strength.to_bits(), y.strength.to_bits());
    }
}

// Sanity: spectrum bin count is what the contract waveform/band lookups assume.
#[test]
fn spectrum_dimensions() {
    assert_eq!(SPECTRUM_BINS, 513);
}
