//! Linux system audio via the PulseAudio client API (including PipeWire-Pulse).
//!
//! A worker owns all PulseAudio objects. It captures the default output's monitor
//! as mono float32 at 48 kHz; the server performs channel/rate conversion. The
//! mainloop is polled without blocking so shutdown also works on an idle or dead
//! server. Default-output changes and server disconnects reopen capture, with
//! bounded waits and a retry delay. No microphone or audio-routing changes.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::ServerInfo;
use pulse::context::{Context, FlagSet as ContextFlags, State as ContextState};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::Mainloop;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, State as StreamState, Stream};

use crate::tap::{CaptureError, CaptureSource, CaptureStage, SampleSink, REQUESTED_BUFFER_FRAMES};
use crate::watchdog::{CaptureHealth, CaptureState};

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const SAMPLE_BYTES: usize = std::mem::size_of::<f32>();

type ServerQuery = pulse::operation::Operation<dyn FnMut(&ServerInfo)>;

thread_local! {
    static PENDING_HEALTH: RefCell<Option<CaptureHealth>> = const { RefCell::new(None) };
}

/// Same side channel as WASAPI: keeps the shared `tap::start(sink)` seam intact.
pub(crate) fn set_pending_health(health: CaptureHealth) {
    PENDING_HEALTH.with(|pending| *pending.borrow_mut() = Some(health));
}

/// Running Linux capture. Drop stops and joins the worker before freeing the sink.
pub struct PulseCapture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    granted_frames: u32,
}

impl CaptureSource for PulseCapture {
    fn granted_buffer_frames(&self) -> u32 {
        self.granted_frames
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn error(stage: CaptureStage, message: impl std::fmt::Display) -> CaptureError {
    CaptureError::msg(stage, format!("PulseAudio / PipeWire-Pulse: {message}"))
}

/// Connects to the default output monitor. Initial failure is returned to the UI;
/// failures after startup reconnect automatically on the worker.
pub fn start(mut sink: Box<dyn SampleSink>) -> Result<PulseCapture, CaptureError> {
    let health = PENDING_HEALTH.with(|pending| pending.borrow_mut().take());
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("viz-audio-pulse".into())
        .spawn(move || {
            let mut session = match Session::open(&worker_stop) {
                Ok(session) => session,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(session.granted_frames));
            loop {
                let result = session.capture(sink.as_mut(), &worker_stop);
                drop(session);
                if worker_stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Err(e) = result {
                    report_failure(health.as_ref(), &e);
                } else {
                    tracing::info!("default output changed; reopening Linux audio capture");
                }
                loop {
                    if worker_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match Session::open(&worker_stop) {
                        Ok(fresh) => {
                            session = fresh;
                            if let Some(health) = &health {
                                health.set_state(CaptureState::Active);
                            }
                            break;
                        }
                        Err(e) => {
                            report_failure(health.as_ref(), &e);
                            std::thread::park_timeout(RETRY_INTERVAL);
                        }
                    }
                }
            }
        })
        .map_err(|e| error(CaptureStage::CreateTap, e))?;
    let mut capture = PulseCapture {
        stop,
        worker: Some(worker),
        granted_frames: 0,
    };
    // Each setup stage shares one deadline. Drop also cancels a timed-out worker.
    capture.granted_frames = ready_rx
        .recv_timeout(SETUP_TIMEOUT + Duration::from_secs(1))
        .map_err(|e| {
            error(
                CaptureStage::CreateTap,
                format!("capture startup failed: {e}"),
            )
        })??;
    Ok(capture)
}

fn report_failure(health: Option<&CaptureHealth>, e: &CaptureError) {
    if let Some(health) = health {
        let next = CaptureState::Failed {
            message: format!("{e}. {}", CaptureError::guidance()),
        };
        if health.state() != next {
            tracing::warn!("{e}; retrying Linux audio capture");
            health.set_state(next);
        }
    } else {
        tracing::warn!("{e}; retrying Linux audio capture");
    }
}

struct Session {
    // Drop order: stream before context, context before its mainloop.
    stream: Option<Stream>,
    context: Context,
    mainloop: Mainloop,
    output: String,
    granted_frames: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.disconnect();
        }
        self.context.disconnect();
    }
}

impl Session {
    fn open(stop: &AtomicBool) -> Result<Self, CaptureError> {
        let deadline = Instant::now() + SETUP_TIMEOUT;
        let mainloop = Mainloop::new()
            .ok_or_else(|| error(CaptureStage::CreateTap, "cannot create mainloop"))?;
        let context = Context::new(&mainloop, "ViewMusic")
            .ok_or_else(|| error(CaptureStage::CreateTap, "cannot create context"))?;
        let mut session = Self {
            stream: None,
            context,
            mainloop,
            output: String::new(),
            granted_frames: 0,
        };
        session
            .context
            .connect(None, ContextFlags::NOAUTOSPAWN, None)
            .map_err(|e| {
                error(
                    CaptureStage::CreateTap,
                    format!("cannot connect to audio server: {e}"),
                )
            })?;
        while session.context.get_state() != ContextState::Ready {
            session.pump(deadline, stop, CaptureStage::CreateTap)?;
        }

        let output = Rc::new(RefCell::new(None));
        let result = output.clone();
        let mut op = session.context.introspect().get_server_info(move |info| {
            *result.borrow_mut() = Some(info.default_sink_name.as_deref().map(str::to_owned));
        });
        while output.borrow().is_none() {
            if let Err(e) = session.pump(deadline, stop, CaptureStage::OutputDevice) {
                op.cancel();
                return Err(e);
            }
        }
        session.output = output.borrow_mut().take().flatten().ok_or_else(|| {
            error(
                CaptureStage::OutputDevice,
                "no default audio output is available",
            )
        })?;

        let monitor = Rc::new(RefCell::new(None));
        let result = monitor.clone();
        let mut op =
            session
                .context
                .introspect()
                .get_sink_info_by_name(&session.output, move |info| match info {
                    ListResult::Item(info) => {
                        *result.borrow_mut() =
                            Some(info.monitor_source_name.as_deref().map(str::to_owned))
                    }
                    ListResult::End | ListResult::Error if result.borrow().is_none() => {
                        *result.borrow_mut() = Some(None)
                    }
                    _ => {}
                });
        while monitor.borrow().is_none() {
            if let Err(e) = session.pump(deadline, stop, CaptureStage::OutputDevice) {
                op.cancel();
                return Err(e);
            }
        }
        let monitor = monitor.borrow_mut().take().flatten().ok_or_else(|| {
            error(
                CaptureStage::OutputDevice,
                "default audio output has no monitor source",
            )
        })?;
        let spec = Spec {
            format: Format::FLOAT32NE,
            channels: 1,
            rate: viz_core::SAMPLE_RATE,
        };
        let stream = Stream::new(
            &mut session.context,
            "System audio visualization",
            &spec,
            None,
        )
        .ok_or_else(|| error(CaptureStage::CreateIoProc, "cannot create recording stream"))?;
        session.stream = Some(stream);
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: REQUESTED_BUFFER_FRAMES * SAMPLE_BYTES as u32,
        };
        session
            .stream
            .as_mut()
            .unwrap()
            .connect_record(
                Some(&monitor),
                Some(&attr),
                StreamFlags::ADJUST_LATENCY | StreamFlags::DONT_MOVE,
            )
            .map_err(|e| error(CaptureStage::StartDevice, e))?;
        while session.stream.as_ref().unwrap().get_state() != StreamState::Ready {
            session.pump(deadline, stop, CaptureStage::StartDevice)?;
            if matches!(
                session.stream.as_ref().unwrap().get_state(),
                StreamState::Failed | StreamState::Terminated
            ) {
                return Err(error(CaptureStage::StartDevice, session.context.errno()));
            }
        }
        session.granted_frames = session
            .stream
            .as_mut()
            .unwrap()
            .get_buffer_attr()
            .map(|attr| attr.fragsize / SAMPLE_BYTES as u32)
            .unwrap_or(REQUESTED_BUFFER_FRAMES)
            .max(1);
        tracing::info!(output = %session.output, monitor, granted_buffer_frames = session.granted_frames, "Linux system-audio capture connected");
        Ok(session)
    }

    fn pump(
        &mut self,
        deadline: Instant,
        stop: &AtomicBool,
        stage: CaptureStage,
    ) -> Result<(), CaptureError> {
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Err(error(
                stage,
                "audio server operation cancelled or timed out",
            ));
        }
        if !self.mainloop.iterate(false).is_success()
            || matches!(
                self.context.get_state(),
                ContextState::Failed | ContextState::Terminated
            )
        {
            return Err(error(
                stage,
                format!("audio server disconnected: {}", self.context.errno()),
            ));
        }
        std::thread::park_timeout(POLL_INTERVAL);
        Ok(())
    }

    /// Ok means a default-output change (or shutdown), Err means server/stream failure.
    fn capture(
        &mut self,
        sink: &mut dyn SampleSink,
        stop: &AtomicBool,
    ) -> Result<(), CaptureError> {
        let mut scratch = [0.0f32; 1024];
        let default_output = Rc::new(RefCell::new(None::<Option<String>>));
        let mut next_output_check = Instant::now() + OUTPUT_POLL_INTERVAL;
        let mut pending: Option<ServerQuery> = None;
        let mut query_started = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            self.pump(
                Instant::now() + SETUP_TIMEOUT,
                stop,
                CaptureStage::StartDevice,
            )?;
            let stream = self.stream.as_mut().unwrap();
            if stream.get_state() != StreamState::Ready {
                return Err(error(
                    CaptureStage::StartDevice,
                    "output monitor disconnected",
                ));
            }
            // Bound each drain so a busy server cannot starve stop/device checks.
            for _ in 0..64 {
                match stream
                    .peek()
                    .map_err(|e| error(CaptureStage::StartDevice, e))?
                {
                    PeekResult::Empty => break,
                    PeekResult::Hole(bytes) => {
                        scratch.fill(0.0);
                        for chunk in (0..bytes / SAMPLE_BYTES).step_by(scratch.len()) {
                            sink.push(
                                &scratch[..(bytes / SAMPLE_BYTES - chunk).min(scratch.len())],
                            );
                        }
                    }
                    PeekResult::Data(bytes) => {
                        for chunk in bytes.chunks(scratch.len() * SAMPLE_BYTES) {
                            let count = chunk.len() / SAMPLE_BYTES;
                            for (sample, bytes) in scratch[..count]
                                .iter_mut()
                                .zip(chunk.chunks_exact(SAMPLE_BYTES))
                            {
                                *sample = f32::from_ne_bytes(bytes.try_into().unwrap());
                            }
                            sink.push(&scratch[..count]);
                        }
                    }
                }
                stream
                    .discard()
                    .map_err(|e| error(CaptureStage::StartDevice, e))?;
            }
            if let Some(output) = default_output.borrow_mut().take() {
                pending = None;
                if output.as_deref() != Some(self.output.as_str()) {
                    return Ok(());
                }
            }
            let now = Instant::now();
            if pending.is_some() && now.duration_since(query_started) >= SETUP_TIMEOUT {
                if let Some(mut op) = pending.take() {
                    op.cancel();
                }
                return Err(error(
                    CaptureStage::OutputDevice,
                    "default-output query timed out",
                ));
            }
            if now >= next_output_check && pending.is_none() {
                let result = default_output.clone();
                pending = Some(self.context.introspect().get_server_info(move |info| {
                    *result.borrow_mut() =
                        Some(info.default_sink_name.as_deref().map(str::to_owned));
                }));
                query_started = now;
                next_output_check = now + OUTPUT_POLL_INTERVAL;
            }
        }
        if let Some(mut op) = pending {
            op.cancel();
        }
        Ok(())
    }
}
