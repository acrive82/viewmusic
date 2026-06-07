//! System-audio capture behind the [`CaptureSource`] trait.
//!
//! All Core Audio / cidre usage lives here so the rest of the crate compiles and
//! tests without hardware. The trait abstracts "start a capture that delivers
//! mono f32 hops to a sink" so the pipeline and watchdog can drive any backend.
//!
//! The macOS backend ([`CidreTap`]) builds a stereo global process tap, wraps it
//! in a private tap-backed aggregate device, requests a 128-frame device buffer,
//! and runs an `AudioDeviceIOProc` that mixes stereo → mono and pushes straight
//! into the caller's [`SampleSink`] — allocation-free, inside the callback.
//! Full teardown happens on `Drop` (aggregate device + tap guard).

use std::fmt;

/// A consumer of mono f32 samples, called from inside the audio IOProc.
///
/// Implementations MUST be allocation-free, lock-free, and non-blocking (the
/// audio callback must never block or allocate). The mixed-to-mono samples for
/// one IOProc callback are
/// delivered in a single `push` call.
pub trait SampleSink: Send + 'static {
    /// Pushes one IOProc's worth of mono samples. Real-time safe.
    fn push(&mut self, mono: &[f32]);
}

impl<F> SampleSink for F
where
    F: FnMut(&[f32]) + Send + 'static,
{
    fn push(&mut self, mono: &[f32]) {
        self(mono)
    }
}

/// A handle to a running capture. Dropping it tears the capture down fully.
pub trait CaptureSource: Send {
    /// The actual device buffer size granted by the OS, in frames.
    fn granted_buffer_frames(&self) -> u32;
}

/// Reads whether the system's **default output device** is currently running in
/// some process (`kAudioDevicePropertyDeviceIsRunningSomewhere`).
///
/// This is the silence-vs-revocation discriminator: when our tap is
/// silent but the output device *is* running somewhere, audio is genuinely playing
/// yet we receive zeros — evidence of a revoked grant. When the device is idle,
/// the silence is real. `None` means the property could not be read (no device,
/// or an OSStatus error) — the caller treats that as "cannot prove revocation"
/// and stays sticky-Granted. Polled off the hot path (watchdog thread).
pub fn output_device_running_somewhere() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::output_device_running_somewhere()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Failure to create or run a capture. Carries the raw `OSStatus` when one is
/// available so logs can point at <https://www.osstatus.com>.
#[derive(Clone, Debug)]
pub struct CaptureError {
    /// Where in the capture-setup pipeline the failure occurred.
    pub stage: CaptureStage,
    /// The raw Core Audio `OSStatus`, if the failure was an OSStatus error.
    pub os_status: Option<i32>,
    /// A human-readable English message.
    pub message: String,
}

/// The setup step that failed — useful for diagnostics and the watchdog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStage {
    /// Resolving the default output device.
    OutputDevice,
    /// Creating the process tap.
    CreateTap,
    /// Reading the tap's stream format.
    TapFormat,
    /// Creating the tap-backed aggregate device.
    CreateAggregate,
    /// Setting the requested device buffer size.
    BufferSize,
    /// Creating the IOProc.
    CreateIoProc,
    /// Starting the device.
    StartDevice,
    /// The backend is unavailable on this platform.
    Unsupported,
}

impl CaptureError {
    /// Builds an OSStatus-bearing error.
    pub fn os(stage: CaptureStage, status: i32, message: impl Into<String>) -> Self {
        Self {
            stage,
            os_status: Some(status),
            message: message.into(),
        }
    }

    /// Builds a message-only error (no OSStatus).
    pub fn msg(stage: CaptureStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            os_status: None,
            message: message.into(),
        }
    }

    /// Heuristic: does this error look like a denied "System Audio Recording"
    /// TCC grant? There is no preflight API for taps, so we treat tap-creation
    /// OSStatus failures as the likely permission case.
    ///
    /// `kAudioHardwareIllegalOperationError` (`'what'` = 1852797029) and
    /// `kAudioHardwareNotRunningError` are the values process-tap creation tends
    /// to return when the grant is missing.
    pub fn is_permission_denied(&self) -> bool {
        if self.stage != CaptureStage::CreateTap && self.stage != CaptureStage::CreateAggregate {
            return false;
        }
        match self.os_status {
            // 'what' (illegal operation) and 'stop' (not running) are the
            // observed denial codes for process taps.
            Some(s) => {
                const ILLEGAL_OP: i32 = i32::from_be_bytes(*b"what");
                const NOT_RUNNING: i32 = i32::from_be_bytes(*b"stop");
                s == ILLEGAL_OP || s == NOT_RUNNING
            }
            None => false,
        }
    }

    /// The user-facing guidance string for a denied/failed capture.
    pub fn guidance() -> &'static str {
        "Enable system-audio capture in System Settings > Privacy & Security > \
         Screen & System Audio Recording, then restart ViewMusic."
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.os_status {
            Some(s) => write!(
                f,
                "capture failed at {:?} (OSStatus {s}): {}",
                self.stage, self.message
            ),
            None => write!(f, "capture failed at {:?}: {}", self.stage, self.message),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Number of audio channels we mix down from (stereo global tap). macOS-only —
/// the Windows backend reads the device channel count from the mix format.
#[cfg(target_os = "macos")]
const TAP_CHANNELS: usize = 2;
/// Requested device buffer size in frames (small enough to keep latency low).
pub const REQUESTED_BUFFER_FRAMES: u32 = 128;

// ---------------------------------------------------------------------------
// macOS backend (cidre). Isolated behind cfg so non-macOS builds still compile.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use macos::CidreTap;

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use cidre::core_audio::aggregate_device_keys as agg_keys;
    use cidre::core_audio::sub_device_keys as sub_keys;
    use cidre::{cf, core_audio as ca};

    /// A running cidre process-tap capture. Teardown happens on `Drop`:
    /// the `StartedDevice` stops the IOProc, then the `AggregateDevice` and
    /// `TapGuard` destroy the aggregate device and tap respectively.
    pub struct CidreTap {
        // Field drop order is declaration order: stop the device first, then
        // destroy the aggregate device, then the tap.
        _started: ca::hardware::StartedDevice<ca::AggregateDevice>,
        _tap: ca::TapGuard,
        // The IOProc reads `*mut Ctx`; keep the box alive for the capture's life.
        _ctx: Box<Ctx>,
        granted_frames: u32,
    }

    // Safety: the IOProc context is only touched from the audio thread while the
    // capture is alive; CidreTap owns it and tears down on drop.
    unsafe impl Send for CidreTap {}

    /// IOProc client data: the mono mix scratch and the user's sink.
    struct Ctx {
        sink: Box<dyn SampleSink>,
        /// Pre-allocated mono scratch sized for the largest plausible IOProc.
        mono: Vec<f32>,
    }

    impl super::CaptureSource for CidreTap {
        fn granted_buffer_frames(&self) -> u32 {
            self.granted_frames
        }
    }

    /// Reads `kAudioDevicePropertyDeviceIsRunningSomewhere` on the default output
    /// device. cidre exposes the selector but no typed helper for
    /// this exact property, so we read it generically via `Obj::bool_prop` with the
    /// vendored `DEVICE_IS_RUNNING_SOMEWHERE` selector (cidre 0.15.2
    /// `core_audio::hardware`), the documented public path. `None` on any error so
    /// callers stay conservative (no false revocation).
    pub fn output_device_running_somewhere() -> Option<bool> {
        let device = ca::System::default_output_device().ok()?;
        device
            .bool_prop(&ca::PropSelector::DEVICE_IS_RUNNING_SOMEWHERE.global_addr())
            .ok()
    }

    /// Starts a stereo-global-tap capture feeding `sink`.
    pub fn start(sink: Box<dyn SampleSink>) -> Result<CidreTap, CaptureError> {
        // 1. Default output device + its UID (used as the aggregate's main sub).
        let output_device = ca::System::default_output_device().map_err(|e| {
            CaptureError::os(
                CaptureStage::OutputDevice,
                e.0.get(),
                "could not resolve the default output device",
            )
        })?;
        let output_uid = output_device.uid().map_err(|e| {
            CaptureError::os(
                CaptureStage::OutputDevice,
                e.0.get(),
                "could not read the output device UID",
            )
        })?;

        // 2. Stereo global tap excluding no processes (capture everything).
        let tap_desc =
            ca::TapDesc::with_stereo_global_tap_excluding_processes(&cidre::ns::Array::new());
        let tap = tap_desc.create_process_tap().map_err(|e| {
            CaptureError::os(
                CaptureStage::CreateTap,
                e.0.get(),
                "AudioHardwareCreateProcessTap failed (system-audio permission?)",
            )
        })?;
        let tap_uid = tap.uid().map_err(|e| {
            CaptureError::os(
                CaptureStage::CreateTap,
                e.0.get(),
                "could not read the tap UID",
            )
        })?;

        // 3. Private, auto-starting aggregate device backed by the tap.
        let sub_device =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[output_uid.as_type_ref()]);
        let sub_tap =
            cf::DictionaryOf::with_keys_values(&[sub_keys::uid()], &[tap_uid.as_type_ref()]);
        let dict = cf::DictionaryOf::with_keys_values(
            &[
                agg_keys::is_private(),
                agg_keys::is_stacked(),
                agg_keys::tap_auto_start(),
                agg_keys::name(),
                agg_keys::main_sub_device(),
                agg_keys::uid(),
                agg_keys::sub_device_list(),
                agg_keys::tap_list(),
            ],
            &[
                cf::Boolean::value_true().as_type_ref(),
                cf::Boolean::value_false(),
                cf::Boolean::value_true(),
                cf::str!(c"ViewMusic Tap"),
                &output_uid,
                &cf::Uuid::new().to_cf_string(),
                &cf::ArrayOf::from_slice(&[sub_device.as_ref()]),
                &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
            ],
        );
        let mut agg_device = ca::AggregateDevice::with_desc(&dict).map_err(|e| {
            CaptureError::os(
                CaptureStage::CreateAggregate,
                e.0.get(),
                "AudioHardwareCreateAggregateDevice failed",
            )
        })?;

        // 4. Request the 128-frame buffer; accept whatever is granted.
        if let Err(e) = agg_device.set_buf_frame_size(REQUESTED_BUFFER_FRAMES) {
            tracing::warn!(
                os_status = e.0.get(),
                requested = REQUESTED_BUFFER_FRAMES,
                "could not set device buffer frame size; using device default"
            );
        }
        let granted_frames = agg_device
            .buf_frame_size()
            .unwrap_or(REQUESTED_BUFFER_FRAMES);
        tracing::info!(
            requested = REQUESTED_BUFFER_FRAMES,
            granted = granted_frames,
            "audio capture buffer frame size"
        );

        // 5. IOProc context: pre-allocate mono scratch generously (granted frames
        //    plus headroom) so the callback never allocates.
        let mono_cap = (granted_frames as usize).max(REQUESTED_BUFFER_FRAMES as usize) * 8;
        let mut ctx = Box::new(Ctx {
            sink,
            mono: vec![0.0; mono_cap],
        });

        // 6. Create the IOProc and start the device.
        let proc_id = agg_device
            .create_io_proc_id(io_proc, Some(ctx.as_mut()))
            .map_err(|e| {
                CaptureError::os(
                    CaptureStage::CreateIoProc,
                    e.0.get(),
                    "AudioDeviceCreateIOProcID failed",
                )
            })?;

        let started = ca::device_start(agg_device, Some(proc_id)).map_err(|e| {
            CaptureError::os(
                CaptureStage::StartDevice,
                e.0.get(),
                "AudioDeviceStart failed",
            )
        })?;

        tracing::info!("system-audio capture started");
        Ok(CidreTap {
            _started: started,
            _tap: tap,
            _ctx: ctx,
            granted_frames,
        })
    }

    /// The Core Audio IOProc. Mixes stereo → mono and pushes to the sink.
    /// Allocation-free: writes into the pre-sized `ctx.mono` scratch.
    extern "C" fn io_proc(
        _device: ca::Device,
        _now: &cidre::cat::AudioTimeStamp,
        input_data: &cidre::cat::AudioBufList<1>,
        _input_time: &cidre::cat::AudioTimeStamp,
        _output_data: &mut cidre::cat::AudioBufList<1>,
        _output_time: &cidre::cat::AudioTimeStamp,
        ctx: Option<&mut Ctx>,
    ) -> cidre::os::Status {
        let Some(ctx) = ctx else {
            return cidre::os::Status::NO_ERR;
        };
        if input_data.number_buffers == 0 {
            return cidre::os::Status::NO_ERR;
        }
        let buf = &input_data.buffers[0];
        if buf.data.is_null() {
            return cidre::os::Status::NO_ERR;
        }
        let channels = buf.number_channels.max(1) as usize;
        let total_floats = (buf.data_bytes_size as usize) / std::mem::size_of::<f32>();
        if total_floats == 0 {
            return cidre::os::Status::NO_ERR;
        }
        // Interleaved float32 samples.
        let samples = unsafe { std::slice::from_raw_parts(buf.data as *const f32, total_floats) };

        let frames = total_floats / channels;
        let frames = frames.min(ctx.mono.len());

        if channels >= TAP_CHANNELS {
            // Mix L+R (first two channels) → mono = 0.5*(L+R).
            for i in 0..frames {
                let base = i * channels;
                let l = samples[base];
                let r = samples[base + 1];
                ctx.mono[i] = 0.5 * (l + r);
            }
        } else {
            // Mono source: copy through.
            for i in 0..frames {
                ctx.mono[i] = samples[i * channels];
            }
        }
        ctx.sink.push(&ctx.mono[..frames]);
        cidre::os::Status::NO_ERR
    }
}

// ---------------------------------------------------------------------------
// Windows backend (WASAPI loopback). Isolated in `tap_windows` behind cfg; this
// module just re-exports its constructor so callers see one `tap::start`.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub use crate::tap_windows::{start, WasapiCapture};

// ---------------------------------------------------------------------------
// Unsupported-OS placeholder: keeps the crate buildable for tooling elsewhere.
// macOS and Windows have real backends above; any other target errors clearly.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod fallback {
    use super::*;

    /// Capture backend constructor on unsupported targets — always unsupported.
    pub fn start(_sink: Box<dyn SampleSink>) -> Result<NoTap, CaptureError> {
        Err(CaptureError::msg(
            CaptureStage::Unsupported,
            "system-audio capture is only implemented on macOS and Windows",
        ))
    }

    /// A capture handle that never exists (constructor always errors).
    pub struct NoTap;

    impl CaptureSource for NoTap {
        fn granted_buffer_frames(&self) -> u32 {
            0
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use fallback::{start, NoTap};

#[cfg(target_os = "macos")]
pub use macos::start;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_heuristic() {
        const ILLEGAL_OP: i32 = i32::from_be_bytes(*b"what");
        let e = CaptureError::os(CaptureStage::CreateTap, ILLEGAL_OP, "denied");
        assert!(e.is_permission_denied());

        // Same status at an unrelated stage is not treated as permission denial.
        let e2 = CaptureError::os(CaptureStage::StartDevice, ILLEGAL_OP, "x");
        assert!(!e2.is_permission_denied());

        // A generic error is not a permission denial.
        let e3 = CaptureError::os(CaptureStage::CreateTap, -50, "x");
        assert!(!e3.is_permission_denied());
    }

    #[test]
    fn closure_is_a_sample_sink() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let total = Arc::new(AtomicUsize::new(0));
        let t2 = total.clone();
        let mut sink = move |m: &[f32]| {
            t2.fetch_add(m.len(), Ordering::Relaxed);
        };
        sink.push(&[0.0, 1.0, 2.0]);
        assert_eq!(total.load(Ordering::Relaxed), 3);
    }
}
