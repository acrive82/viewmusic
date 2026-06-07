//! Window + GPU context and the winit event loop.
//!
//! Implements the winit 0.30 [`ApplicationHandler`] pattern: the [`Window`] and the
//! whole wgpu chain (Instance → Surface → Adapter → Device/Queue) are created in
//! [`ApplicationHandler::resumed`] on the main thread (winit requires the window to
//! be created from the event loop, not before it starts). The surface is
//! configured for low-latency vsync presentation: [`wgpu::PresentMode::Fifo`] with
//! `desired_maximum_frame_latency = 1`, the surface-preferred format, sized in
//! physical pixels.
//!
//! Frame pacing: macOS throttles `request_redraw` to the display link, but
//! on >60 Hz displays (ProMotion) that can be 120 Hz. We enforce a HARD 60 Hz update
//! rate by skipping presents that arrive sooner than [`MIN_FRAME_INTERVAL`] since the
//! last present, while still re-requesting the next redraw so the loop keeps running.
//! The skip decision is the pure function [`should_present`] so it can be unit-tested.
//!
//! The visualizer pass is injected through the [`RenderCallback`] seam: this shell
//! renders a dark clear pass into the surface texture FIRST, then hands the same
//! command encoder + the surface view to the callback (the egui overlay today, the
//! visualizer + overlay later) which composites on top with `LoadOp::Load`
//! (loading rather than clearing, so the overlay never wipes the scene beneath it).

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

/// Hard 60 Hz frame budget. Presents closer together than this are skipped so the
/// effective update rate stays at 60 even on 120 Hz displays.
///
/// Slightly under 1/60 s (≈16.67 ms) to avoid rounding a legitimate 60 Hz vsync into
/// a skip — we use 15.5 ms.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_micros(15_500);

/// Initial logical window size (physical size is derived via the scale factor).
const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;

/// Live GPU resources shared with the render path.
///
/// Held behind the app so the render callback can borrow `device`/`queue`/`config`
/// without re-acquiring them each frame. No per-frame allocation lives here
/// (the swapchain texture is acquired transiently in [`App::render`]).
pub struct GpuContext {
    /// The presentation surface bound to the window. `'static` because the surface
    /// owns an `Arc<Window>` (created via [`wgpu::SurfaceTarget`] from the Arc).
    pub surface: wgpu::Surface<'static>,
    /// Logical GPU device.
    pub device: wgpu::Device,
    /// Command submission queue.
    pub queue: wgpu::Queue,
    /// Current surface configuration (format, size, present mode). Reconfigured on
    /// resize / scale-factor change.
    pub config: wgpu::SurfaceConfiguration,
    /// The chosen surface texture format (cached for pipeline/overlay construction).
    pub format: wgpu::TextureFormat,
}

impl GpuContext {
    /// Builds the full wgpu chain for `window` at the given physical size.
    ///
    /// Blocks on adapter/device acquisition via `pollster` (one-time setup cost,
    /// off the hot path).
    fn new(window: Arc<Window>, size: PhysicalSize<u32>) -> Self {
        // `InstanceDescriptor` has no `Default` (its `display` field is a boxed
        // trait object); build it via the no-display constructor and force Metal
        // (with a portability fallback) for macOS.
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::METAL | wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = instance
            .create_surface(window)
            .expect("failed to create wgpu surface from window");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no suitable GPU adapter found");

        // Baseline limits, but raise the texture/resolution caps to what the adapter
        // actually supports: `downlevel_defaults` caps 2D textures at 2048, which is
        // smaller than a Retina window's physical size (e.g. 2560×1440) and would
        // make `Surface::configure` panic on large displays.
        let required_limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewmusic-device"),
            required_features: wgpu::Features::empty(),
            required_limits,
            ..Default::default()
        }))
        .expect("failed to acquire wgpu device");

        // Start from the surface-preferred config, then override pacing/latency knobs.
        // Clamp to the device's maximum texture extent (same rationale as `resize`).
        let max_dim = device.limits().max_texture_dimension_2d;
        let width = size.width.clamp(1, max_dim);
        let height = size.height.clamp(1, max_dim);
        let mut config = surface
            .get_default_config(&adapter, width, height)
            .expect("surface is incompatible with the selected adapter");
        config.present_mode = wgpu::PresentMode::Fifo;
        config.desired_maximum_frame_latency = 1;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let format = config.format;

        surface.configure(&device, &config);

        Self {
            surface,
            device,
            queue,
            config,
            format,
        }
    }

    /// Reconfigures the surface to a new physical size (resize / scale change).
    ///
    /// Zero-sized requests are ignored (minimized window) to avoid wgpu panics, and
    /// dimensions are clamped to the device's maximum texture extent so a move onto
    /// a display larger than the GPU supports degrades instead of panicking.
    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        let max_dim = self.device.limits().max_texture_dimension_2d;
        self.config.width = size.width.min(max_dim);
        self.config.height = size.height.min(max_dim);
        self.surface.configure(&self.device, &self.config);
    }
}

/// Context handed to the render callback for one frame after the clear pass.
///
/// Borrows live GPU state plus the acquired surface view. The callback composites
/// on top of the already-cleared scene using `LoadOp::Load`.
pub struct FrameContext<'a> {
    /// GPU device (for transient resources the overlay may need).
    pub device: &'a wgpu::Device,
    /// Submission queue.
    pub queue: &'a wgpu::Queue,
    /// The command encoder already used for the clear pass; the callback records
    /// its own render pass(es) into it.
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// View of the current swapchain texture (the present target).
    pub view: &'a wgpu::TextureView,
    /// Current surface configuration (size in physical px, format).
    pub config: &'a wgpu::SurfaceConfiguration,
    /// The window, for input scale factor and redraw scheduling.
    pub window: &'a Window,
}

/// Seam for the overlay / visualizer pass.
///
/// Implemented by the app's overlay today; the future visualizer pass slots in the
/// same way. Invoked once per presented frame, after the clear pass.
pub trait RenderCallback {
    /// Records this frame's overlay/visualizer commands into `frame.encoder`.
    fn render(&mut self, frame: &mut FrameContext<'_>);

    /// Forwards a window event so the callback can update input state (egui).
    ///
    /// Returning `true` means the callback consumed the event (e.g. egui wants the
    /// pointer) — the shell still processes structural events regardless.
    fn on_window_event(&mut self, _window: &Window, _event: &WindowEvent) -> bool {
        false
    }
}

/// One-shot hook run once the GPU context first exists, so the host can build
/// GPU-dependent resources (e.g. the egui renderer needs the surface format).
type ReadyHook<C> = Box<dyn FnOnce(&GpuContext, &Window, &mut C)>;

/// Restored window geometry to apply at creation: outer position + inner size, both
/// in physical pixels. Supplied by the host from persisted state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitialWindow {
    /// Outer-position x in physical pixels.
    pub x: i32,
    /// Outer-position y in physical pixels.
    pub y: i32,
    /// Inner width in physical pixels.
    pub width: u32,
    /// Inner height in physical pixels.
    pub height: u32,
}

/// Decides whether to present this redraw given the hard 60 Hz cap.
///
/// Pure function (no I/O) so it is unit-testable. Returns `true` when at least
/// `min_interval` has elapsed since `last_present` (or there was no previous
/// present). On `false` the caller skips rendering but still re-requests a redraw.
#[must_use]
pub fn should_present(now: Instant, last_present: Option<Instant>, min_interval: Duration) -> bool {
    match last_present {
        None => true,
        Some(prev) => now.duration_since(prev) >= min_interval,
    }
}

/// The winit application: owns the window, GPU context, render callback, and pacing
/// state. A `&mut` reference is passed to [`winit::event_loop::EventLoop::run_app`].
pub struct App<C: RenderCallback> {
    window: Option<Arc<Window>>,
    gpu: Option<GpuContext>,
    callback: C,
    /// Instant of the last actually-presented frame (drives 60 Hz pacing).
    last_present: Option<Instant>,
    /// Hook invoked exactly once after the GPU context first exists, so the host can
    /// build GPU-dependent resources (e.g. the egui renderer needs the surface format).
    on_ready: Option<ReadyHook<C>>,
    /// Restored geometry to apply when the window is created; `None` ⇒ use
    /// the platform default size + placement.
    initial_window: Option<InitialWindow>,
}

impl<C: RenderCallback> App<C> {
    /// Creates the app with its render callback. The window/GPU are created later in
    /// [`ApplicationHandler::resumed`].
    pub fn new(callback: C) -> Self {
        Self {
            window: None,
            gpu: None,
            callback,
            last_present: None,
            on_ready: None,
            initial_window: None,
        }
    }

    /// Registers a one-shot hook run after the GPU context is created (so the host
    /// can build GPU-format-dependent state such as the egui renderer).
    pub fn on_ready(mut self, f: impl FnOnce(&GpuContext, &Window, &mut C) + 'static) -> Self {
        self.on_ready = Some(Box::new(f));
        self
    }

    /// Restores the window's geometry from persisted state. When set, the
    /// window opens at the saved position + size instead of the platform default.
    pub fn with_initial_window(mut self, initial: Option<InitialWindow>) -> Self {
        self.initial_window = initial;
        self
    }

    /// Renders one frame: dark clear pass into the surface texture, then the render
    /// callback composites on top. Returns `true` if a frame was actually presented.
    fn render(&mut self) -> bool {
        let (Some(window), Some(gpu)) = (self.window.as_ref(), self.gpu.as_mut()) else {
            return false;
        };

        let surface_texture = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            // Surface needs reconfiguration; do it and try again next redraw.
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                let size = window.inner_size();
                gpu.resize(size);
                return false;
            }
            // Transient: skip this frame.
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return false,
        };

        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewmusic-frame"),
            });

        // Clear pass FIRST — establishes the dark scene. The overlay/visualizer
        // callback loads (LoadOp::Load) on top so the scene is never wiped (R2).
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        // Hand the encoder + view to the overlay/visualizer pass.
        {
            let mut frame = FrameContext {
                device: &gpu.device,
                queue: &gpu.queue,
                encoder: &mut encoder,
                view: &view,
                config: &gpu.config,
                window,
            };
            self.callback.render(&mut frame);
        }

        gpu.queue.submit(std::iter::once(encoder.finish()));
        // Tell winit we are about to present so it can pace the next redraw.
        window.pre_present_notify();
        surface_texture.present();
        true
    }
}

impl<C: RenderCallback> ApplicationHandler for App<C> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Recreate only if we have no window yet (resume after suspend keeps it).
        if self.window.is_some() {
            return;
        }

        let mut attrs = Window::default_attributes().with_title("ViewMusic");
        // Restore persisted geometry (physical px) when available, else the platform
        // default logical size.
        match self.initial_window {
            Some(win) => {
                attrs = attrs
                    .with_inner_size(PhysicalSize::new(win.width.max(1), win.height.max(1)))
                    .with_position(winit::dpi::PhysicalPosition::new(win.x, win.y));
            }
            None => {
                attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(
                    INITIAL_WIDTH as f64,
                    INITIAL_HEIGHT as f64,
                ));
            }
        }
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );

        let size = window.inner_size();
        let gpu = GpuContext::new(window.clone(), size);

        // One-shot host hook (e.g. build the egui renderer against the surface format).
        if let Some(f) = self.on_ready.take() {
            f(&gpu, &window, &mut self.callback);
        }

        self.gpu = Some(gpu);
        self.window = Some(window.clone());
        window.request_redraw();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        // Let the overlay see the event first (input routing).
        if let Some(window) = self.window.as_ref() {
            let _consumed = self.callback.on_window_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // Physical inner size already reflects the new scale; reconfigure.
                if let (Some(gpu), Some(window)) = (self.gpu.as_mut(), self.window.as_ref()) {
                    gpu.resize(window.inner_size());
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                // Always schedule the next redraw so the loop stays alive (macOS
                // display-link throttled). Pacing decides whether we present.
                let now = Instant::now();
                let present = should_present(now, self.last_present, MIN_FRAME_INTERVAL);
                if present && self.render() {
                    self.last_present = Some(now);
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_always_presents() {
        let now = Instant::now();
        assert!(should_present(now, None, MIN_FRAME_INTERVAL));
    }

    #[test]
    fn too_soon_skips() {
        let last = Instant::now();
        // 5 ms later — well under the 15.5 ms budget → skip (120 Hz half-step).
        let now = last + Duration::from_millis(5);
        assert!(!should_present(now, Some(last), MIN_FRAME_INTERVAL));
    }

    #[test]
    fn at_budget_presents() {
        let last = Instant::now();
        let now = last + MIN_FRAME_INTERVAL;
        assert!(should_present(now, Some(last), MIN_FRAME_INTERVAL));
    }

    #[test]
    fn just_over_budget_presents() {
        let last = Instant::now();
        let now = last + MIN_FRAME_INTERVAL + Duration::from_micros(200);
        assert!(should_present(now, Some(last), MIN_FRAME_INTERVAL));
    }

    #[test]
    fn pacing_halves_120hz_to_60hz() {
        // Simulate a 120 Hz display: vsync every ~8.33 ms. Across a sequence we
        // expect alternating present/skip so the effective rate is ~60 Hz.
        let step = Duration::from_micros(8_333);
        let mut last: Option<Instant> = None;
        let start = Instant::now();
        let mut presents = 0;
        for k in 0..120u32 {
            let now = start + step * k;
            if should_present(now, last, MIN_FRAME_INTERVAL) {
                presents += 1;
                last = Some(now);
            }
        }
        // 120 vsyncs over ~1 s should yield ~60 presents (allow a small margin).
        assert!(
            (58..=62).contains(&presents),
            "expected ~60 presents, got {presents}"
        );
    }
}
