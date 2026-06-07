//! Pure, cross-platform sample-rate and channel adaptation for capture backends.
//!
//! Capture backends deliver interleaved float32 frames in the device mix format
//! (an arbitrary channel count at the device's native rate). The analyzer expects
//! mono samples at [`viz_core::SAMPLE_RATE`]. This module bridges the two with two
//! small, allocation-free primitives:
//!
//! * [`mix_to_mono`] — averages the N interleaved channels of each frame into a
//!   single mono sample, generalizing the stereo `0.5 * (L + R)` mix to any
//!   channel count.
//! * [`LinearResampler`] — streaming linear interpolation from an input rate to an
//!   output rate, carrying the fractional read position across calls so a stream
//!   split into arbitrary blocks resamples identically to the whole. It is an
//!   identity pass-through when the rates match.
//!
//! Both write into a caller-owned `Vec<f32>` that is cleared and reused, so after
//! the buffer has grown to its working size there is no per-call heap allocation —
//! matching the real-time discipline of the capture callbacks that drive them.

/// Mixes interleaved `channels`-channel frames down to mono by averaging the
/// channels of each frame, writing the result into `out`.
///
/// `out` is cleared and then extended, so it is reused across calls without a
/// fresh allocation once it has grown to the required length. The number of mono
/// samples written equals `interleaved.len() / channels` (the whole-frame count);
/// any trailing partial frame is ignored.
///
/// Averaging (rather than summing) keeps the mono amplitude in the same range as
/// the source channels, which is what the analyzer's energy/AGC stages expect.
/// `channels == 0` is treated as a no-op (clears `out`).
pub fn mix_to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels == 0 {
        return;
    }
    let frames = interleaved.len() / channels;
    out.reserve(frames);
    if channels == 1 {
        // Mono source: a straight copy is the average of one channel.
        out.extend_from_slice(&interleaved[..frames]);
        return;
    }
    let inv = 1.0 / channels as f32;
    for f in 0..frames {
        let base = f * channels;
        let mut sum = 0.0f32;
        for c in 0..channels {
            sum += interleaved[base + c];
        }
        out.push(sum * inv);
    }
}

/// Streaming linear resampler from `in_rate` to `out_rate`.
///
/// Feed mono samples in blocks of any size via [`process`](LinearResampler::process);
/// the resampler interpolates to `out_rate` and carries the fractional read
/// position (and the last input sample) across calls, so concatenated blocks
/// produce the same output as resampling the whole stream at once.
///
/// When `in_rate == out_rate` the resampler is a transparent pass-through (the
/// input is copied through unchanged). All scratch is the caller-owned output
/// buffer; after warmup `process` performs no heap allocation beyond growing that
/// buffer to its steady-state length.
pub struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    /// Step in input samples per output sample (`in_rate / out_rate`). Unused in
    /// pass-through mode.
    step: f64,
    /// Fractional read position measured from the start of the current input
    /// block, carried between calls. In `[0, in_len)` it indexes the block; values
    /// in `[-1, 0)` interpolate between `last_sample` (the final sample of the
    /// previous block) and the first sample of the current block.
    pos: f64,
    /// The last input sample of the previous block, used to bridge block edges.
    last_sample: f32,
    /// Whether any block has been processed yet (so the first block has no
    /// preceding sample to interpolate from).
    primed: bool,
}

impl LinearResampler {
    /// Builds a resampler converting `in_rate` Hz to `out_rate` Hz.
    ///
    /// Both rates must be non-zero; a zero rate is clamped to 1 to avoid a
    /// divide-by-zero (callers read real device rates, so this only guards against
    /// degenerate input).
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        Self {
            in_rate,
            out_rate,
            step: in_rate as f64 / out_rate as f64,
            pos: 0.0,
            last_sample: 0.0,
            primed: false,
        }
    }

    /// The configured input rate (Hz).
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// The configured output rate (Hz).
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// True when input and output rates match (pass-through mode).
    pub fn is_passthrough(&self) -> bool {
        self.in_rate == self.out_rate
    }

    /// Resamples one block of mono input into `out` (cleared then filled).
    ///
    /// `out` is reused across calls; once it reaches its steady-state size this is
    /// allocation-free. In pass-through mode the input is copied through verbatim.
    pub fn process(&mut self, mono_in: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if self.is_passthrough() {
            out.extend_from_slice(mono_in);
            return;
        }
        if mono_in.is_empty() {
            return;
        }
        let in_len = mono_in.len();

        // The first block has no preceding sample: start reading at index 0 and
        // treat its first sample as the bridge value once the block is consumed.
        if !self.primed {
            self.pos = 0.0;
            self.primed = true;
        }

        // Conservative capacity estimate; growth (if any) happens once.
        out.reserve((in_len as f64 / self.step).ceil() as usize + 1);

        // Emit output samples while the read position lies within the span this
        // block can interpolate: from `last_sample` (at index -1) up to the final
        // input sample (at index in_len - 1).
        let last_idx = (in_len - 1) as f64;
        while self.pos <= last_idx {
            let floor = self.pos.floor();
            let frac = (self.pos - floor) as f32;
            // `floor` is in [-1, in_len - 1]. -1 selects the bridge sample.
            let i = floor as isize;
            let a = if i < 0 {
                self.last_sample
            } else {
                mono_in[i as usize]
            };
            let b_idx = i + 1;
            // b_idx is in [0, in_len]; in_len would be past the block, but the loop
            // guard (pos <= last_idx) keeps b_idx <= in_len - 1 except exactly at
            // pos == last_idx where frac == 0 and b is unused.
            let b = if (b_idx as usize) < in_len {
                mono_in[b_idx as usize]
            } else {
                a
            };
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }

        // Shift the carried position to be relative to the next block's start.
        // The next block's index 0 corresponds to this block's index `in_len`,
        // so subtract `in_len`; the result lands in [-1, 0) and selects the new
        // bridge sample for the first interpolation of the next call.
        self.pos -= in_len as f64;
        self.last_sample = mono_in[in_len - 1];
    }

    /// Resets the streaming state (fractional position and bridge sample) so the
    /// next block is treated as the start of a fresh stream. Rates are unchanged.
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.last_sample = 0.0;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use viz_core::SAMPLE_RATE;

    /// Sum of squares — a rough energy proxy for comparing signals of differing
    /// length on a per-sample basis.
    fn mean_square(xs: &[f32]) -> f64 {
        if xs.is_empty() {
            return 0.0;
        }
        xs.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / xs.len() as f64
    }

    #[test]
    fn passthrough_is_identity_at_same_rate() {
        let mut r = LinearResampler::new(SAMPLE_RATE, SAMPLE_RATE);
        assert!(r.is_passthrough());
        let input: Vec<f32> = (0..1000).map(|n| (n as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn passthrough_reuses_buffer_without_growth_after_warmup() {
        let mut r = LinearResampler::new(SAMPLE_RATE, SAMPLE_RATE);
        let input = vec![0.5f32; 512];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let cap = out.capacity();
        r.process(&input, &mut out);
        assert_eq!(
            out.capacity(),
            cap,
            "no reallocation on the second equal block"
        );
        assert_eq!(out.len(), 512);
    }

    #[test]
    fn upsample_length_is_proportional() {
        let mut r = LinearResampler::new(44_100, 48_000);
        assert!(!r.is_passthrough());
        let in_len = 44_100usize; // one second
        let input = vec![0.0f32; in_len];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let expected = (in_len as f64 * 48_000.0 / 44_100.0).round() as i64;
        let got = out.len() as i64;
        assert!(
            (got - expected).abs() <= 1,
            "44100->48000 length {got}, expected ~{expected}"
        );
    }

    #[test]
    fn preserves_dc_level() {
        let mut r = LinearResampler::new(44_100, 48_000);
        let input = vec![0.75f32; 4410];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // Linear interpolation of a constant is that constant everywhere.
        for &s in &out {
            assert!((s - 0.75).abs() < 1e-6, "DC drifted to {s}");
        }
    }

    #[test]
    fn preserves_sine_energy_roughly() {
        // A 440 Hz tone resampled 44.1k -> 48k should keep roughly the same
        // per-sample energy (mean square ~0.5 for a unit sine).
        let in_rate = 44_100u32;
        let out_rate = 48_000u32;
        let freq = 440.0f64;
        let input: Vec<f32> = (0..in_rate as usize)
            .map(|n| {
                let t = n as f64 / in_rate as f64;
                (std::f64::consts::TAU * freq * t).sin() as f32
            })
            .collect();
        let mut r = LinearResampler::new(in_rate, out_rate);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let ein = mean_square(&input);
        let eout = mean_square(&out);
        assert!((ein - 0.5).abs() < 0.05, "input mean-square {ein}");
        assert!(
            (eout - ein).abs() < 0.05,
            "output mean-square {eout} drifted from input {ein}"
        );
    }

    #[test]
    fn stereo_mix_is_half_sum() {
        // Interleaved L,R: average equals 0.5 * (L + R).
        let interleaved = [1.0f32, 0.0, 0.0, 1.0, 0.5, 0.5, -1.0, 1.0];
        let mut out = Vec::new();
        mix_to_mono(&interleaved, 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5, 0.5, 0.0]);
    }

    #[test]
    fn mono_mix_copies_through() {
        let interleaved = [0.1f32, -0.2, 0.3, -0.4];
        let mut out = Vec::new();
        mix_to_mono(&interleaved, 1, &mut out);
        assert_eq!(out, interleaved.to_vec());
    }

    #[test]
    fn six_channel_mix_averages_all_channels() {
        // Two frames of 6 channels each.
        let frame0 = [0.0f32, 6.0, 0.0, 0.0, 0.0, 0.0]; // sum 6 -> avg 1.0
        let frame1 = [1.0f32, 1.0, 1.0, 1.0, 1.0, 1.0]; // sum 6 -> avg 1.0
        let mut interleaved = Vec::new();
        interleaved.extend_from_slice(&frame0);
        interleaved.extend_from_slice(&frame1);
        let mut out = Vec::new();
        mix_to_mono(&interleaved, 6, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mix_ignores_trailing_partial_frame() {
        // 7 samples, 2 channels -> 3 whole frames, last sample dropped.
        let interleaved = [1.0f32, 1.0, 2.0, 2.0, 3.0, 3.0, 9.0];
        let mut out = Vec::new();
        mix_to_mono(&interleaved, 2, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn mix_reuses_buffer_without_growth_after_warmup() {
        let interleaved = vec![0.25f32; 512 * 2];
        let mut out = Vec::new();
        mix_to_mono(&interleaved, 2, &mut out);
        let cap = out.capacity();
        mix_to_mono(&interleaved, 2, &mut out);
        assert_eq!(
            out.capacity(),
            cap,
            "no reallocation on the second equal block"
        );
        assert_eq!(out.len(), 512);
    }

    #[test]
    fn streaming_two_halves_match_whole() {
        // Resampling a stream split into two blocks must equal resampling it whole.
        let in_rate = 44_100u32;
        let out_rate = 48_000u32;
        let input: Vec<f32> = (0..2000).map(|n| (n as f64 * 0.03).sin() as f32).collect();

        let mut whole = Vec::new();
        let mut r_whole = LinearResampler::new(in_rate, out_rate);
        r_whole.process(&input, &mut whole);

        let mid = input.len() / 2;
        let mut r_split = LinearResampler::new(in_rate, out_rate);
        let mut a = Vec::new();
        let mut b = Vec::new();
        r_split.process(&input[..mid], &mut a);
        r_split.process(&input[mid..], &mut b);
        let mut split = a.clone();
        split.extend_from_slice(&b);

        // The two-halves output approximates the whole output: same length within
        // one sample and matching samples within interpolation tolerance.
        assert!(
            (split.len() as i64 - whole.len() as i64).abs() <= 1,
            "split len {} vs whole len {}",
            split.len(),
            whole.len()
        );
        let n = split.len().min(whole.len());
        for i in 0..n {
            assert!(
                (split[i] - whole[i]).abs() < 1e-5,
                "sample {i} differs: split {} whole {}",
                split[i],
                whole[i]
            );
        }
    }

    #[test]
    fn streaming_many_small_blocks_match_whole() {
        // Stronger continuity check: many tiny irregular blocks must reconstruct
        // the same stream as one whole call.
        let in_rate = 44_100u32;
        let out_rate = 48_000u32;
        let input: Vec<f32> = (0..1500).map(|n| (n as f64 * 0.017).sin() as f32).collect();

        let mut r_whole = LinearResampler::new(in_rate, out_rate);
        let mut whole = Vec::new();
        r_whole.process(&input, &mut whole);

        let mut r_chunked = LinearResampler::new(in_rate, out_rate);
        let mut chunked = Vec::new();
        let mut block = Vec::new();
        let sizes = [1usize, 7, 32, 100, 3, 257];
        let mut idx = 0;
        let mut s = 0;
        while idx < input.len() {
            let len = sizes[s % sizes.len()].min(input.len() - idx);
            r_chunked.process(&input[idx..idx + len], &mut block);
            chunked.extend_from_slice(&block);
            idx += len;
            s += 1;
        }

        assert!(
            (chunked.len() as i64 - whole.len() as i64).abs() <= 1,
            "chunked len {} vs whole len {}",
            chunked.len(),
            whole.len()
        );
        let n = chunked.len().min(whole.len());
        for i in 0..n {
            assert!(
                (chunked[i] - whole[i]).abs() < 1e-5,
                "sample {i} differs: chunked {} whole {}",
                chunked[i],
                whole[i]
            );
        }
    }

    #[test]
    fn downsample_length_is_proportional() {
        let mut r = LinearResampler::new(96_000, 48_000);
        let in_len = 9600usize;
        let input = vec![0.0f32; in_len];
        let mut out = Vec::new();
        r.process(&input, &mut out);
        let expected = (in_len as f64 * 48_000.0 / 96_000.0).round() as i64;
        assert!((out.len() as i64 - expected).abs() <= 1);
    }

    #[test]
    fn zero_channels_clears_output() {
        let mut out = vec![1.0f32, 2.0, 3.0];
        mix_to_mono(&[1.0, 2.0], 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let mut r = LinearResampler::new(44_100, 48_000);
        let mut out = vec![1.0f32];
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }
}
