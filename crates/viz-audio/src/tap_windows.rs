//! Windows system-audio capture via WASAPI loopback.
//!
//! This is the Windows peer of the macOS process-tap backend in
//! [`crate::tap`]: it implements the same [`CaptureSource`] /
//! [`SampleSink`] seam so the pipeline and watchdog drive it unchanged. All
//! WASAPI / COM usage is isolated in this module behind `#[cfg(windows)]`.
//!
//! ## How it works
//!
//! WASAPI exposes "loopback" capture: opening the **default render endpoint**
//! (`eRender`, `eConsole`) with [`AUDCLNT_STREAMFLAGS_LOOPBACK`] makes its
//! [`IAudioCaptureClient`] deliver exactly the samples being played out of that
//! device — the system audio mix, in the device's shared-mode mix format. The
//! setup chain mirrors the canonical WASAPI sequence:
//!
//! ```text
//! CoInitializeEx(MTA)
//!   → CoCreateInstance(MMDeviceEnumerator)
//!   → GetDefaultAudioEndpoint(eRender, eConsole)
//!   → Activate(IAudioClient)
//!   → GetMixFormat()                       // device channels + sample rate
//!   → Initialize(SHARED, LOOPBACK | EVENTCALLBACK, …)
//!   → SetEventHandle(CreateEventW(…))      // the capture wake event
//!   → GetService(IAudioCaptureClient)
//!   → Start()
//! ```
//!
//! A dedicated capture thread then waits on the event and drains packets: each
//! packet is mixed to mono ([`crate::resample::mix_to_mono`], converting 16-bit
//! integer PCM to float when the mix format is integer), resampled to
//! [`viz_core::SAMPLE_RATE`] ([`crate::resample::LinearResampler`]), and pushed
//! into the caller's [`SampleSink`]. The hot path is allocation-free: the mono
//! and resample scratch [`Vec`]s are pre-sized once and reused.
//!
//! ## Silence keepalive
//!
//! Loopback delivers **no packets at all** while nothing is rendering ("when
//! nothing is playing, there is nothing to capture"). To keep the analyzer's
//! monotonic sample clock advancing through silence — exactly as the macOS tap's
//! zero-filled buffers do — the thread tracks the wall clock with
//! `QueryPerformanceCounter` and, on a wait timeout or an empty wake, pushes the
//! number of 48 kHz mono zero-samples that have elapsed since the last push
//! (capped per wake). The watchdog is configured to treat this idle silence as
//! healthy on Windows (see [`crate::watchdog::TREAT_IDLE_AS_FAULT`]).
//!
//! ## Device changes
//!
//! An [`IMMNotificationClient`] registered on the enumerator receives default-
//! endpoint and device-state changes. Its callback is deliberately trivial — it
//! only pulses [`CaptureHealth::note_device_changed`]; the watchdog thread
//! performs the actual teardown + rebuild against the new endpoint off the COM
//! thread, via the existing drop + [`crate::tap::start`] path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use viz_core::SAMPLE_RATE;

use crate::resample::{mix_to_mono, LinearResampler};
use crate::tap::{CaptureError, CaptureSource, CaptureStage, SampleSink};
use crate::watchdog::CaptureHealth;

/// Wave format tag for uncompressed integer PCM (`WAVE_FORMAT_PCM`). Defined
/// locally so this module does not depend on the `Win32_Media_Multimedia`
/// feature just for a single constant.
const WAVE_FORMAT_PCM: u16 = 1;
/// Wave format tag for 32-bit IEEE float samples (`WAVE_FORMAT_IEEE_FLOAT`).
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Extensible wave format tag (`WAVE_FORMAT_EXTENSIBLE`); the real sample type is
/// then carried in the `SubFormat` GUID of the [`WAVEFORMATEXTENSIBLE`] tail.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` — the extensible-format sub-GUID for float.
/// Layout `{00000003-0000-0010-8000-00aa00389b71}`. Declared locally to avoid the
/// `Win32_Media_KernelStreaming` / `Win32_Media_Multimedia` features.
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// How long to wait on the capture event before treating the wake as a timeout
/// and zero-filling. Sized to roughly half a hop ([`crate::dsp::HOP_SIZE`] at
/// 48 kHz ≈ 10.7 ms) so the analyzer clock advances smoothly through silence.
const WAIT_MS: u32 = 5;

/// Upper bound on the number of 48 kHz mono zero-samples pushed in a single wake,
/// so a long scheduling stall cannot make one push allocate without bound or
/// flood the analyzer. One second of audio is far more than any normal wake gap.
const MAX_ZERO_FILL_PER_WAKE: usize = SAMPLE_RATE as usize;

/// A `Send` wrapper around a raw kernel [`HANDLE`].
///
/// `HANDLE` is `!Send` because it wraps a raw pointer, but a Win32 event handle is
/// a process-wide kernel object that is safe to wait on / signal from any thread.
/// We move the wake event into the capture thread (which owns and waits on it) and
/// keep a copy in [`WasapiCapture`] (which only ever signals it on shutdown), so a
/// `Send` marker is sound here.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);

// Safety: see the type docs — an event HANDLE is a kernel object usable from any
// thread; we never alias mutable Rust state through it.
unsafe impl Send for SendHandle {}

/// How the captured device format maps to a sample reader for the hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleKind {
    /// 32-bit IEEE float (the usual shared-mode mix format).
    Float32,
    /// 16-bit signed integer PCM (converted to float on the fly).
    Int16,
}

/// The device mix format we resample from, distilled to the few fields the hot
/// path needs. Parsed once from `GetMixFormat`.
#[derive(Clone, Copy, Debug)]
struct MixFormat {
    /// Device channel count (mixed down to mono).
    channels: usize,
    /// Device sample rate in Hz (resampled to [`SAMPLE_RATE`]).
    sample_rate: u32,
    /// How to read each interleaved sample.
    kind: SampleKind,
}

/// A running WASAPI loopback capture. Dropping it stops the capture thread,
/// unregisters the device-change callback, joins the thread, and closes the wake
/// event handle (full teardown).
pub struct WasapiCapture {
    /// Set to request the capture thread to stop; the thread also signals the
    /// wake event so the wait returns promptly.
    stop: Arc<AtomicBool>,
    /// The wake event the audio client signals (and we signal on shutdown).
    wake_event: HANDLE,
    /// The capture worker thread (joined on drop).
    thread: Option<JoinHandle<()>>,
    /// The device buffer size granted by `GetBufferSize`, in frames.
    granted_frames: u32,
}

// The wake `HANDLE` is only used to signal the capture thread on shutdown; the
// thread owns all the COM objects. The handle is a plain kernel object pointer
// that is safe to signal from another thread.
unsafe impl Send for WasapiCapture {}

impl CaptureSource for WasapiCapture {
    fn granted_buffer_frames(&self) -> u32 {
        self.granted_frames
    }
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the capture thread so it observes the stop flag without waiting out
        // the full event timeout.
        unsafe {
            let _ = SetEvent(self.wake_event);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // The thread has exited and dropped its COM objects; close our handle.
        unsafe {
            let _ = CloseHandle(self.wake_event);
        }
        tracing::info!("WASAPI loopback capture stopped");
    }
}

/// The trivial device-change notification sink.
///
/// COM may invoke these callbacks on an arbitrary MTA thread, so the handler does
/// as little as possible: it only pulses the shared [`CaptureHealth`]
/// device-change flag (a single relaxed atomic store). The watchdog thread reads
/// that pulse and performs the real teardown + rebuild off this thread, which
/// keeps the callback safe against the historical windows-rs re-entrancy crash.
#[implement(IMMNotificationClient)]
struct DeviceChangeClient {
    health: CaptureHealth,
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceChangeClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.health.note_device_changed();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.health.note_device_changed();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only the render endpoint matters for loopback capture.
        if flow == eRender {
            self.health.note_device_changed();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Starts a WASAPI loopback capture feeding `sink`.
///
/// Mirrors the macOS [`crate::tap::start`] signature exactly: the same
/// `Box<dyn SampleSink>` in, the same [`CaptureError`] on failure. There is no
/// permission concept on Windows — loopback capture of the default render
/// endpoint requires no consent — so this never returns a permission-style error.
///
/// The capture is driven by a dedicated thread (created here) that owns all the
/// COM objects for the capture's lifetime. The returned handle's `Drop` stops and
/// joins that thread.
pub fn start(sink: Box<dyn SampleSink>) -> Result<WasapiCapture, CaptureError> {
    // The device-change notifications need a CaptureHealth to pulse. The pipeline
    // shares a single CaptureHealth; on Windows it deposits the handle in a
    // thread-local immediately before calling start (see `set_pending_health`).
    // Absent one (e.g. a direct test call) we fall back to a detached handle so
    // capture still runs — device changes simply will not trigger a rebuild.
    let health = take_pending_health().unwrap_or_default();

    // The wake event is created here so we own its lifetime independent of the
    // thread; the thread receives a copy and the client signals it.
    let wake_event = unsafe {
        CreateEventW(None, false, false, PCWSTR::null()).map_err(|e| {
            CaptureError::os(
                CaptureStage::CreateIoProc,
                e.code().0,
                "CreateEventW failed for the capture wake event",
            )
        })?
    };

    let stop = Arc::new(AtomicBool::new(false));

    // Setup runs on the capture thread (COM objects must be created and used on the
    // same MTA thread that drains them), but the caller needs the granted buffer
    // size and the success/failure synchronously — so a one-shot channel hands the
    // setup result back.
    let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<u32, CaptureError>>();

    let thread_stop = stop.clone();
    let thread_event = SendHandle(wake_event);
    let thread = std::thread::Builder::new()
        .name("viz-audio-wasapi".into())
        .spawn(move || {
            // Capture the whole `SendHandle` (Send), not its inner `!Send` field —
            // bind it inside so disjoint closure capture does not reach for `.0`.
            let event = thread_event;
            capture_thread(sink, health, event.0, thread_stop, result_tx);
        })
        .map_err(|e| {
            unsafe {
                let _ = CloseHandle(wake_event);
            }
            CaptureError::msg(
                CaptureStage::CreateIoProc,
                format!("failed to spawn the WASAPI capture thread: {e}"),
            )
        })?;

    // Wait for the thread's setup result. On error the thread has already torn down
    // and exited, so we join it and close the event before returning.
    match result_rx.recv() {
        Ok(Ok(granted_frames)) => {
            tracing::info!(granted_frames, "WASAPI loopback capture started");
            Ok(WasapiCapture {
                stop,
                wake_event,
                thread: Some(thread),
                granted_frames,
            })
        }
        Ok(Err(e)) => {
            let _ = thread.join();
            unsafe {
                let _ = CloseHandle(wake_event);
            }
            Err(e)
        }
        Err(_) => {
            // The thread dropped the sender without sending (panic during setup).
            let _ = thread.join();
            unsafe {
                let _ = CloseHandle(wake_event);
            }
            Err(CaptureError::msg(
                CaptureStage::StartDevice,
                "the WASAPI capture thread exited before reporting setup status",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Pending-health handoff.
//
// The shared `CaptureHealth` whose device-change flag the notification client
// pulses is owned by the pipeline, but `tap::start` takes only a sink (to keep
// the macOS and Windows signatures identical). The pipeline deposits the handle
// here immediately before each `tap::start` call; this module takes it. The slot
// is per-thread, matching how the pipeline calls start (initial start on the
// caller thread, rebuilds on the watchdog thread).
// ---------------------------------------------------------------------------

thread_local! {
    static PENDING_HEALTH: std::cell::RefCell<Option<CaptureHealth>> =
        const { std::cell::RefCell::new(None) };
}

/// Deposits the shared [`CaptureHealth`] the next [`start`] call on this thread
/// should wire its device-change notifications to. Called by the pipeline right
/// before [`crate::tap::start`]. Backend-internal (Windows only).
pub(crate) fn set_pending_health(health: CaptureHealth) {
    PENDING_HEALTH.with(|cell| *cell.borrow_mut() = Some(health));
}

/// Takes the pending [`CaptureHealth`] deposited by [`set_pending_health`], if any.
fn take_pending_health() -> Option<CaptureHealth> {
    PENDING_HEALTH.with(|cell| cell.borrow_mut().take())
}

/// The capture worker thread: performs COM setup, reports the result, then runs
/// the packet/zero-fill loop until asked to stop. Owns every COM object for the
/// capture's lifetime so teardown is a simple unwind on return.
fn capture_thread(
    mut sink: Box<dyn SampleSink>,
    health: CaptureHealth,
    wake_event: HANDLE,
    stop: Arc<AtomicBool>,
    result_tx: std::sync::mpsc::Sender<Result<u32, CaptureError>>,
) {
    // COM is initialized for this thread only; uninitialized on the way out.
    let co_initialized = match unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        Ok(()) => true,
        Err(e) => {
            let _ = result_tx.send(Err(CaptureError::os(
                CaptureStage::OutputDevice,
                e.code().0,
                "CoInitializeEx failed",
            )));
            return;
        }
    };

    let session = match unsafe { setup_capture(&health, wake_event) } {
        Ok(session) => session,
        Err(e) => {
            let _ = result_tx.send(Err(e));
            if co_initialized {
                unsafe { CoUninitialize() };
            }
            return;
        }
    };

    // Setup succeeded: report the granted buffer size to the caller.
    if result_tx.send(Ok(session.granted_frames)).is_err() {
        // The caller went away before reading the result; tear down and exit.
        unsafe { session.teardown() };
        if co_initialized {
            unsafe { CoUninitialize() };
        }
        return;
    }

    let mut scratch = Scratch::new(&session.format, session.granted_frames);
    unsafe { run_capture_loop(&mut sink, &session, &mut scratch, wake_event, &stop) };

    unsafe { session.teardown() };
    if co_initialized {
        unsafe { CoUninitialize() };
    }
}

/// The live COM objects + parsed format owned by the capture thread. Immutable for
/// the loop's duration; the mutable hot-path buffers live in [`Scratch`].
struct Session {
    enumerator: IMMDeviceEnumerator,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    notification: IMMNotificationClient,
    format: MixFormat,
    granted_frames: u32,
    /// QPC ticks per second (for the silence keepalive clock).
    qpc_freq: i64,
}

impl Session {
    /// Stops the stream and unregisters the notification callback. Errors are
    /// logged, never propagated (this is teardown).
    ///
    /// # Safety
    /// COM must be initialized on the calling thread.
    unsafe fn teardown(&self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.notification);
        }
    }
}

/// The mutable hot-path scratch: pre-sized once, reused every packet/zero-fill so
/// the capture loop never allocates in the steady state.
struct Scratch {
    /// Mono mix scratch.
    mono: Vec<f32>,
    /// Resampler output scratch (also reused for zero-fill).
    resampled: Vec<f32>,
    /// Float conversion scratch for integer mix formats.
    float_scratch: Vec<f32>,
    /// Streaming resampler from the device rate to 48 kHz.
    resampler: LinearResampler,
    /// QPC tick of the last push to the sink (audio or zero-fill).
    last_push_qpc: i64,
}

impl Scratch {
    fn new(format: &MixFormat, granted_frames: u32) -> Self {
        // Pre-size generously so the loop never reallocates: a packet is at most a
        // few device buffers, and the resampler grows the output by at most the
        // rate ratio. One tenth of a second of headroom covers any plausible wake.
        let frame_cap = (granted_frames as usize).max(SAMPLE_RATE as usize / 10);
        let mut now_qpc = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut now_qpc);
        }
        Self {
            mono: Vec::with_capacity(frame_cap),
            resampled: Vec::with_capacity(frame_cap * 2),
            float_scratch: Vec::with_capacity(frame_cap * format.channels.max(1)),
            resampler: LinearResampler::new(format.sample_rate, SAMPLE_RATE),
            last_push_qpc: now_qpc,
        }
    }
}

/// Runs the full WASAPI setup chain. Returns a live [`Session`] on success.
///
/// # Safety
/// COM must be initialized on the calling thread.
unsafe fn setup_capture(
    health: &CaptureHealth,
    wake_event: HANDLE,
) -> Result<Session, CaptureError> {
    // 1. Device enumerator.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| {
            CaptureError::os(
                CaptureStage::OutputDevice,
                e.code().0,
                "CoCreateInstance(MMDeviceEnumerator) failed",
            )
        })?;

    // 2. Default render endpoint (loopback requires a render endpoint).
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|e| {
        CaptureError::os(
            CaptureStage::OutputDevice,
            e.code().0,
            "GetDefaultAudioEndpoint(eRender, eConsole) failed (no output device?)",
        )
    })?;

    // 3. Activate the audio client.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| {
        CaptureError::os(
            CaptureStage::CreateTap,
            e.code().0,
            "IMMDevice::Activate(IAudioClient) failed",
        )
    })?;

    // 4. Mix format. The pointer is allocated by WASAPI and must be freed with
    //    CoTaskMemFree; we parse what we need then free it after Initialize.
    let mix_ptr = unsafe { client.GetMixFormat() }.map_err(|e| {
        CaptureError::os(
            CaptureStage::TapFormat,
            e.code().0,
            "IAudioClient::GetMixFormat failed",
        )
    })?;
    if mix_ptr.is_null() {
        return Err(CaptureError::msg(
            CaptureStage::TapFormat,
            "GetMixFormat returned a null format pointer",
        ));
    }
    let format = unsafe { parse_mix_format(mix_ptr) };

    // 5. Initialize shared loopback + event callback. hnsBufferDuration 0 lets
    //    WASAPI pick the default period; loopback ignores periodicity.
    let init = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            mix_ptr,
            None,
        )
    };
    // Free the mix format before checking the result so we never leak it.
    unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
    init.map_err(|e| {
        CaptureError::os(
            CaptureStage::CreateAggregate,
            e.code().0,
            "IAudioClient::Initialize(SHARED, LOOPBACK | EVENTCALLBACK) failed",
        )
    })?;

    let format = format.ok_or_else(|| {
        CaptureError::msg(
            CaptureStage::TapFormat,
            "unsupported device mix format (not float32 or 16-bit PCM)",
        )
    })?;

    // 6. Bind the wake event.
    unsafe { client.SetEventHandle(wake_event) }.map_err(|e| {
        CaptureError::os(
            CaptureStage::CreateIoProc,
            e.code().0,
            "IAudioClient::SetEventHandle failed",
        )
    })?;

    // 7. Capture service + granted buffer size.
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| {
        CaptureError::os(
            CaptureStage::CreateIoProc,
            e.code().0,
            "IAudioClient::GetService(IAudioCaptureClient) failed",
        )
    })?;
    let granted_frames = unsafe { client.GetBufferSize() }.unwrap_or(0);

    // 8. Register the trivial device-change notification client.
    let notification: IMMNotificationClient = DeviceChangeClient {
        health: health.clone(),
    }
    .into();
    if let Err(e) = unsafe { enumerator.RegisterEndpointNotificationCallback(&notification) } {
        // Non-fatal: capture still works, only auto-rebuild on device change is
        // lost. Log and continue rather than fail the whole start.
        tracing::warn!(
            os_status = e.code().0,
            "RegisterEndpointNotificationCallback failed; device-change rebuild disabled"
        );
    }

    // 9. Start the stream.
    unsafe { client.Start() }.map_err(|e| {
        CaptureError::os(
            CaptureStage::StartDevice,
            e.code().0,
            "IAudioClient::Start failed",
        )
    })?;

    let mut qpc_freq = 0i64;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut qpc_freq);
    }

    Ok(Session {
        enumerator,
        client,
        capture,
        notification,
        format,
        granted_frames,
        qpc_freq: qpc_freq.max(1),
    })
}

/// The capture loop: wait on the event, drain packets, zero-fill on silence,
/// until `stop` is set. Allocation-free in the steady state.
///
/// # Safety
/// COM must be initialized; `session` must be live for the loop's duration.
unsafe fn run_capture_loop(
    sink: &mut Box<dyn SampleSink>,
    session: &Session,
    scratch: &mut Scratch,
    wake_event: HANDLE,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        let wait = unsafe { WaitForSingleObject(wake_event, WAIT_MS) };
        if stop.load(Ordering::Relaxed) {
            break;
        }

        if wait == WAIT_OBJECT_0 {
            // Signalled: drain every queued packet.
            unsafe { drain_packets(sink, session, scratch) };
        } else {
            // Timeout (or any non-signalled wake): nothing rendered. Advance the
            // analyzer clock with the elapsed silence so motion stays tied to the
            // sample stream rather than stalling.
            push_zero_fill(sink, session, scratch);
        }
    }
}

/// Drains all queued capture packets, mixing + resampling each into the sink.
///
/// # Safety
/// `session` must be live and COM initialized.
unsafe fn drain_packets(sink: &mut Box<dyn SampleSink>, session: &Session, scratch: &mut Scratch) {
    loop {
        let next = unsafe { session.capture.GetNextPacketSize() }.unwrap_or(0);
        if next == 0 {
            break;
        }

        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        let got = unsafe {
            session
                .capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
        };
        if got.is_err() || frames == 0 {
            // Nothing usable; release whatever was given and stop draining.
            let _ = unsafe { session.capture.ReleaseBuffer(frames) };
            break;
        }

        unsafe { process_packet(sink, session, scratch, data, frames, flags) };

        if let Err(e) = unsafe { session.capture.ReleaseBuffer(frames) } {
            tracing::warn!(
                os_status = e.code().0,
                "IAudioCaptureClient::ReleaseBuffer failed"
            );
            break;
        }
    }
}

/// Mixes + resamples one captured packet into the sink. Allocation-free once the
/// scratch has warmed.
///
/// # Safety
/// `data` must point to `frames * channels` interleaved samples in the device mix
/// format (validated by the caller via `GetBuffer`).
unsafe fn process_packet(
    sink: &mut Box<dyn SampleSink>,
    session: &Session,
    scratch: &mut Scratch,
    data: *mut u8,
    frames: u32,
    flags: u32,
) {
    let channels = session.format.channels.max(1);
    let frames = frames as usize;
    let total = frames * channels;
    let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;

    if silent || data.is_null() {
        // A silent packet still represents real elapsed time: feed zeros for the
        // frame count so the clock advances exactly with the device.
        scratch.mono.clear();
        scratch.mono.resize(frames, 0.0);
    } else {
        match session.format.kind {
            SampleKind::Float32 => {
                let samples = unsafe { std::slice::from_raw_parts(data as *const f32, total) };
                mix_to_mono(samples, channels, &mut scratch.mono);
            }
            SampleKind::Int16 => {
                let samples = unsafe { std::slice::from_raw_parts(data as *const i16, total) };
                scratch.float_scratch.clear();
                scratch.float_scratch.reserve(total);
                const INV: f32 = 1.0 / 32768.0;
                for &s in samples {
                    scratch.float_scratch.push(s as f32 * INV);
                }
                mix_to_mono(&scratch.float_scratch, channels, &mut scratch.mono);
            }
        }
    }

    scratch
        .resampler
        .process(&scratch.mono, &mut scratch.resampled);
    if !scratch.resampled.is_empty() {
        sink.push(&scratch.resampled);
        let mut now_qpc = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut now_qpc);
        }
        scratch.last_push_qpc = now_qpc;
    }
}

/// Pushes the number of 48 kHz mono zero-samples that have elapsed since the last
/// push, so the analyzer clock keeps advancing through silence. Capped per wake.
/// A no-op when no whole sample has elapsed yet.
fn push_zero_fill(sink: &mut Box<dyn SampleSink>, session: &Session, scratch: &mut Scratch) {
    let mut now_qpc = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut now_qpc);
    }
    let last = scratch.last_push_qpc;
    let elapsed_ticks = now_qpc.saturating_sub(last);
    if elapsed_ticks <= 0 {
        return;
    }
    // samples = elapsed_seconds * SAMPLE_RATE = elapsed_ticks * RATE / freq.
    let samples = (elapsed_ticks as i128 * SAMPLE_RATE as i128 / session.qpc_freq as i128) as usize;
    if samples == 0 {
        return;
    }
    let samples = samples.min(MAX_ZERO_FILL_PER_WAKE);

    scratch.resampled.clear();
    scratch.resampled.resize(samples, 0.0);
    sink.push(&scratch.resampled);

    // Advance the push clock by exactly the samples we emitted (converted back to
    // ticks) so rounding does not drift the clock over long silences.
    let consumed_ticks = (samples as i128 * session.qpc_freq as i128 / SAMPLE_RATE as i128) as i64;
    scratch.last_push_qpc = last + consumed_ticks;
}

/// Parses a `WAVEFORMATEX` (possibly `WAVEFORMATEXTENSIBLE`) into the few fields
/// the hot path needs. Returns `None` for an unsupported sample type.
///
/// # Safety
/// `ptr` must point to a valid `WAVEFORMATEX` (with a valid extensible tail when
/// `wFormatTag == WAVE_FORMAT_EXTENSIBLE` and `cbSize >= 22`).
unsafe fn parse_mix_format(ptr: *const WAVEFORMATEX) -> Option<MixFormat> {
    let wf = unsafe { *ptr };
    let channels = wf.nChannels as usize;
    let sample_rate = wf.nSamplesPerSec;
    let bits = wf.wBitsPerSample;

    let kind = match wf.wFormatTag {
        WAVE_FORMAT_IEEE_FLOAT if bits == 32 => SampleKind::Float32,
        WAVE_FORMAT_PCM if bits == 16 => SampleKind::Int16,
        WAVE_FORMAT_EXTENSIBLE => {
            // The real sample type is in the SubFormat GUID of the extensible tail.
            // Reinterpret the header as the extensible struct (the allocation is
            // sized for it whenever cbSize >= 22).
            if wf.cbSize < 22 {
                return None;
            }
            let ext = ptr as *const WAVEFORMATEXTENSIBLE;
            let subformat = unsafe { (*ext).SubFormat };
            if subformat == SUBTYPE_IEEE_FLOAT && bits == 32 {
                SampleKind::Float32
            } else if bits == 16 {
                // 16-bit extensible PCM (KSDATAFORMAT_SUBTYPE_PCM) handled as Int16.
                SampleKind::Int16
            } else {
                return None;
            }
        }
        _ => return None,
    };

    if channels == 0 || sample_rate == 0 {
        return None;
    }
    Some(MixFormat {
        channels,
        sample_rate,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_float32_format() {
        let wf = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let parsed = unsafe { parse_mix_format(&wf) }.expect("float32 supported");
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.kind, SampleKind::Float32);
    }

    #[test]
    fn parse_int16_format() {
        let wf = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 44_100,
            nAvgBytesPerSec: 44_100 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let parsed = unsafe { parse_mix_format(&wf) }.expect("int16 supported");
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.kind, SampleKind::Int16);
    }

    #[test]
    fn parse_rejects_unsupported_bit_depth() {
        let wf = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 6,
            nBlockAlign: 6,
            wBitsPerSample: 24, // 24-bit packed PCM is not a mix format we read
            cbSize: 0,
        };
        assert!(unsafe { parse_mix_format(&wf) }.is_none());
    }
}
