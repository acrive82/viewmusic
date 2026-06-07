//! viz-core — shared types for ViewMusic.
//!
//! The vocabulary every crate builds on. Kept dependency-light on purpose:
//! the audio thread publishes [`FeatureFrame`]s, the render side consumes them.

/// Number of log-spaced spectrum bands exposed to artifacts (contract v1 fixes 48).
pub const BAND_COUNT: usize = 48;
/// Number of waveform sample positions exposed to artifacts (contract v1 fixes 256).
pub const WAVEFORM_LEN: usize = 256;
/// Analysis sample rate. The audio clock is `samples / SAMPLE_RATE` seconds.
pub const SAMPLE_RATE: u32 = 48_000;

/// Snapshot of musical features published by the audio thread every hop (~10.7 ms).
///
/// POD by design: `Copy`, fixed-size arrays, no heap, no `Drop` — required for the
/// wait-free triple-buffer handoff and the zero-allocation per-frame path.
#[derive(Clone, Copy, Debug)]
pub struct FeatureFrame {
    /// Audio-derived clock in seconds (monotonic sample counter / 48 000).
    /// Never wall clock; dropouts zero-fill and still advance it, so the same
    /// audio always produces the same clock (visuals stay deterministic).
    pub t: f64,
    /// Log-spaced bands, 30 Hz–16 kHz, per-band AGC normalized 0..1.
    pub bands: [f32; BAND_COUNT],
    /// Aggregate < 250 Hz, 0..1.
    pub low: f32,
    /// Aggregate 250 Hz–4 kHz, 0..1.
    pub mid: f32,
    /// Aggregate > 4 kHz, 0..1.
    pub high: f32,
    /// Smoothed (EWMA) RMS loudness, 0..1.
    pub energy: f32,
    /// Most recent analysis window, −1..1.
    pub waveform: [f32; WAVEFORM_LEN],
    /// 1.0 on the frame containing an onset, exponential decay afterwards.
    pub beat: f32,
    /// Monotonic onset counter since capture start.
    pub beat_count: u32,
    /// Sustained-silence flag (healthy capture, no signal). Drives the app-level
    /// idle state; NOT exposed to artifact formulas.
    pub silence: bool,
}

impl Default for FeatureFrame {
    fn default() -> Self {
        Self {
            t: 0.0,
            bands: [0.0; BAND_COUNT],
            low: 0.0,
            mid: 0.0,
            high: 0.0,
            energy: 0.0,
            waveform: [0.0; WAVEFORM_LEN],
            beat: 0.0,
            beat_count: 0,
            silence: true,
        }
    }
}

impl FeatureFrame {
    /// Interpolated band lookup at normalized position `u` ∈ 0..1 (contract `band(u)`).
    /// Exact band k (0..47) is `band(k/47)`. Out-of-range `u` is clamped; NaN → 0.
    pub fn band(&self, u: f64) -> f64 {
        lookup_lerp(&self.bands, u)
    }

    /// Interpolated waveform lookup at `u` ∈ 0..1 (contract `wave(u)`).
    /// Exact sample k (0..255) is `wave(k/255)`.
    pub fn wave(&self, u: f64) -> f64 {
        lookup_lerp(&self.waveform, u)
    }
}

fn lookup_lerp(values: &[f32], u: f64) -> f64 {
    if values.is_empty() || !u.is_finite() {
        return 0.0;
    }
    let u = u.clamp(0.0, 1.0);
    let pos = u * (values.len() - 1) as f64;
    let i = pos.floor() as usize;
    let frac = pos - i as f64;
    let a = values[i] as f64;
    let b = values[(i + 1).min(values.len() - 1)] as f64;
    a + (b - a) * frac
}

/// A discrete onset, queued from the audio thread (rtrb SPSC).
#[derive(Clone, Copy, Debug)]
pub struct BeatEvent {
    /// Audio-clock timestamp of the onset.
    pub t: f64,
    /// Relative onset strength (peak-picker excess over threshold), ≥ 0.
    pub strength: f32,
}

/// Artifact identifier (`meta.id`): `[a-z][a-z0-9-]*`, ≤ 64 chars, unique per library.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ArtifactId(pub String);

impl ArtifactId {
    /// Validates the contract id rules: a lowercase ASCII first character, then
    /// lowercase letters, digits, or hyphens, up to 64 characters total.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let mut chars = s.chars();
        let valid_first = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let valid_rest = s
            .chars()
            .skip(1)
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid_first || !valid_rest {
            return Err(IdError::InvalidFormat);
        }
        if s.len() > 64 {
            return Err(IdError::TooLong);
        }
        Ok(Self(s.to_owned()))
    }

    /// Stable seed for the artifact's deterministic `rand(k)` (FNV-1a over the id).
    pub fn rand_seed(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.0.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Artifact id validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    /// Must match `[a-z][a-z0-9-]*`.
    InvalidFormat,
    /// Longer than 64 characters.
    TooLong,
}

impl std::fmt::Display for IdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdError::InvalidFormat => f.write_str("artifact id must match [a-z][a-z0-9-]*"),
            IdError::TooLong => f.write_str("artifact id exceeds 64 characters"),
        }
    }
}

impl std::error::Error for IdError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_frame_is_pod_sized() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<FeatureFrame>();
        assert_copy::<BeatEvent>();
    }

    #[test]
    fn band_lookup_interpolates_and_clamps() {
        let mut f = FeatureFrame::default();
        f.bands[0] = 0.0;
        f.bands[1] = 1.0;
        let step = 1.0 / 47.0;
        assert!((f.band(0.0) - 0.0).abs() < 1e-9);
        assert!((f.band(step) - 1.0).abs() < 1e-9);
        assert!((f.band(step / 2.0) - 0.5).abs() < 1e-9);
        assert_eq!(f.band(-1.0), 0.0);
        assert_eq!(f.band(f64::NAN), 0.0);
    }

    #[test]
    fn id_rules() {
        assert!(ArtifactId::parse("spectrum-bars").is_ok());
        assert!(ArtifactId::parse("Spectrum").is_err());
        assert!(ArtifactId::parse("9bars").is_err());
        assert!(ArtifactId::parse("a_b").is_err());
        assert!(ArtifactId::parse(&"a".repeat(65)).is_err());
        let a = ArtifactId::parse("aaa").unwrap().rand_seed();
        let b = ArtifactId::parse("aab").unwrap().rand_seed();
        assert_ne!(a, b);
        assert_eq!(a, ArtifactId::parse("aaa").unwrap().rand_seed());
    }
}
