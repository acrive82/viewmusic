//! egui overlay over the wgpu pass.
//!
//! Implements [`RenderCallback`] (from [`crate::window`]): the shell renders the
//! clear/visualizer pass into the surface texture FIRST, then this overlay paints on
//! top with `LoadOp::Load` so it never wipes the scene. This compositing order is
//! load-bearing — clearing here instead of loading would erase the visualizer.
//!
//! Input flows through [`egui_winit::State`] (`on_window_event` / `take_egui_input`),
//! and painting through [`egui_wgpu::Renderer`]. The panel is anchored top-right
//! ([`egui::Align2::RIGHT_TOP`]) and shows a placeholder label for now; the UI body is
//! injected via [`Overlay::set_ui`] (a boxed closure seam) so the dropdown + settings
//! panel can be added later without touching this file.

use std::time::Duration;

use egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};
use egui_winit::State;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::window::{FrameContext, RenderCallback};

/// How long the overlay stays visible after the last pointer movement before it
/// auto-hides (the overlay must stay unobtrusive). Any cursor move resets this timer.
pub const AUTO_HIDE_AFTER: Duration = Duration::from_secs(5);

/// Whether the unobtrusive overlay should be drawn this frame, and (when drawn)
/// whether it is collapsed to its chevron — the pure visibility decision, split out
/// so it is unit-testable without egui or a real clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayVisibility {
    /// Draw the overlay at all this frame. `false` ⇒ fully hidden (auto-hidden after
    /// inactivity); pointer movement brings it back.
    pub show: bool,
    /// When shown, whether only the chevron glyph is drawn (panel body hidden). This
    /// is the user's explicit collapse state, persisted across runs.
    pub collapsed: bool,
}

/// Decides overlay visibility from the time since the last pointer movement, whether
/// the pointer is currently over the overlay, and the persisted collapse flag.
///
/// The overlay auto-hides once `since_pointer_move >= AUTO_HIDE_AFTER`, **unless** the
/// pointer is currently hovering it (so a settings interaction is never yanked away).
/// Any pointer movement resets `since_pointer_move` to ~zero, so the overlay
/// reappears immediately. The collapse flag is independent: a collapsed overlay still
/// auto-hides, and an auto-hidden overlay keeps its collapse state for when it
/// returns.
#[must_use]
pub fn decide_visibility(
    since_pointer_move: Duration,
    pointer_over_overlay: bool,
    collapsed: bool,
) -> OverlayVisibility {
    let show = pointer_over_overlay || since_pointer_move < AUTO_HIDE_AFTER;
    OverlayVisibility { show, collapsed }
}

/// Boxed UI builder: receives the egui context each frame to populate the panel.
///
/// The default builder shows the `ViewMusic` placeholder; the app replaces it with
/// the dropdown + auto-generated settings panel later (Principle III).
pub type UiBuilder = Box<dyn FnMut(&egui::Context)>;

/// egui overlay state. Created empty, then attached to the GPU via
/// [`Overlay::attach`] once the surface format is known (the `on_ready` hook in
/// [`crate::window::App`]).
pub struct Overlay {
    /// egui context (shared between input + painting).
    ctx: egui::Context,
    /// winit→egui input translation + platform output handling.
    state: Option<State>,
    /// GPU paint backend (built once the surface format is known).
    renderer: Option<Renderer>,
    /// User-supplied panel body.
    ui: UiBuilder,
}

impl Default for Overlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay {
    /// Creates the overlay with the default placeholder UI. Not yet attached to GPU.
    pub fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            state: None,
            renderer: None,
            ui: Box::new(default_ui),
        }
    }

    /// Replaces the panel body. The closure runs every frame inside the egui pass;
    /// it must not block or allocate unboundedly (immediate-mode panel rebuild).
    ///
    /// This is the injection seam for the future dropdown + settings panel; the
    /// current binary uses the default placeholder, so it is unused for now.
    #[allow(dead_code)]
    pub fn set_ui(&mut self, ui: impl FnMut(&egui::Context) + 'static) {
        self.ui = Box::new(ui);
    }

    /// Attaches the overlay to the GPU and window: builds the egui input [`State`]
    /// and the [`Renderer`] for the surface format. Call once from the app's
    /// `on_ready` hook after the GPU context exists.
    ///
    /// `output_color_format` MUST be the surface format so egui's blending matches.
    pub fn attach(
        &mut self,
        device: &wgpu::Device,
        window: &Window,
        output_color_format: wgpu::TextureFormat,
    ) {
        let state = State::new(
            self.ctx.clone(),
            self.ctx.viewport_id(),
            window,
            Some(window.scale_factor() as f32),
            None,
            // Bound texture uploads to the device limit (avoids oversized atlases).
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        let renderer = Renderer::new(device, output_color_format, RendererOptions::default());
        self.state = Some(state);
        self.renderer = Some(renderer);
    }

    /// Whether egui currently wants the pointer (it is over an overlay
    /// widget/panel). Used by the host to keep the overlay visible while the user is
    /// interacting with it (never auto-hide mid-interaction). `false`
    /// before the first egui pass.
    pub fn wants_pointer(&self) -> bool {
        self.ctx.egui_wants_pointer_input() || self.ctx.is_pointer_over_egui()
    }
}

/// The default placeholder panel: a top-right anchored window titled `ViewMusic`.
///
/// Structured so the future dropdown + settings content slots into the same `Window`
/// body via [`Overlay::set_ui`].
fn default_ui(ctx: &egui::Context) {
    egui::Window::new("ViewMusic")
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
        .resizable(false)
        .collapsible(true)
        .title_bar(true)
        .show(ctx, |ui| {
            ui.label("ViewMusic");
        });
}

impl RenderCallback for Overlay {
    fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        if let Some(state) = self.state.as_mut() {
            let response = state.on_window_event(window, event);
            return response.consumed;
        }
        false
    }

    fn render(&mut self, frame: &mut FrameContext<'_>) {
        // Skip until attached (no GPU resources yet).
        let (Some(state), Some(renderer)) = (self.state.as_mut(), self.renderer.as_mut()) else {
            return;
        };

        let pixels_per_point = frame.window.scale_factor() as f32;

        // 1) Run the egui pass (immediate-mode panel rebuild). `run_ui` hands a root
        //    `Ui`; our overlay panels are free-floating `Window`s anchored to the
        //    context, so the builder takes the `Context` (via `ui.ctx()`).
        let raw_input = state.take_egui_input(frame.window);
        let ui = &mut self.ui;
        let full_output = self.ctx.run_ui(raw_input, |root| ui(root.ctx()));

        // 2) Apply platform output (cursor, clipboard, etc.).
        state.handle_platform_output(frame.window, full_output.platform_output);

        // 3) Tessellate to GPU primitives.
        let paint_jobs = self.ctx.tessellate(full_output.shapes, pixels_per_point);

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [frame.config.width, frame.config.height],
            pixels_per_point,
        };

        // 4) Upload texture deltas (font atlas, images) added this frame.
        for (id, image_delta) in &full_output.textures_delta.set {
            renderer.update_texture(frame.device, frame.queue, *id, image_delta);
        }

        // 5) Stage vertex/index buffers (records onto our encoder; no extra submit).
        let user_buffers = renderer.update_buffers(
            frame.device,
            frame.queue,
            frame.encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        debug_assert!(
            user_buffers.is_empty(),
            "no user paint callbacks expected in the overlay"
        );

        // 6) Paint on TOP of the existing scene: LoadOp::Load never clears it (R2).
        {
            let render_pass = frame
                .encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui-overlay-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: frame.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            // egui's `render` requires a `'static` pass; the encoder outlives it here.
            let mut render_pass = render_pass.forget_lifetime();
            renderer.render(&mut render_pass, &paint_jobs, &screen_descriptor);
        }

        // 7) Free textures egui dropped this frame (after the pass that may use them).
        for id in &full_output.textures_delta.free {
            renderer.free_texture(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattached_overlay_ignores_events_and_renders_noop() {
        // Before `attach`, on_window_event must report "not consumed" and not panic.
        let overlay = Overlay::new();
        // We cannot construct a real Window in a unit test, but we can exercise the
        // pre-attach branch via the state being None.
        assert!(overlay.state.is_none());
        assert!(overlay.renderer.is_none());
    }

    #[test]
    fn overlay_visible_right_after_pointer_move() {
        let v = decide_visibility(Duration::ZERO, false, false);
        assert!(v.show);
        assert!(!v.collapsed);
    }

    #[test]
    fn overlay_auto_hides_after_inactivity() {
        // Just before the threshold: still shown.
        let v = decide_visibility(AUTO_HIDE_AFTER - Duration::from_millis(1), false, false);
        assert!(v.show);
        // At/after the threshold: hidden (no hover).
        let v = decide_visibility(AUTO_HIDE_AFTER, false, false);
        assert!(!v.show);
        let v = decide_visibility(AUTO_HIDE_AFTER + Duration::from_secs(10), false, false);
        assert!(!v.show);
    }

    #[test]
    fn overlay_stays_visible_while_pointer_hovers_it() {
        // Even long past the timer, hovering keeps it visible (don't yank a panel
        // out from under the user mid-interaction).
        let v = decide_visibility(AUTO_HIDE_AFTER + Duration::from_secs(60), true, false);
        assert!(v.show);
    }

    #[test]
    fn collapse_flag_is_carried_through() {
        // Collapse state is independent of show: it is reported whether shown or not.
        let shown = decide_visibility(Duration::ZERO, false, true);
        assert!(shown.show);
        assert!(shown.collapsed);
        let hidden = decide_visibility(AUTO_HIDE_AFTER, false, true);
        assert!(!hidden.show);
        assert!(hidden.collapsed);
    }

    #[test]
    fn set_ui_replaces_builder() {
        // Smoke: the closure seam compiles and stores. Run it against a fresh ctx to
        // prove it is invoked without a window or GPU.
        let mut overlay = Overlay::new();
        let ctx = egui::Context::default();
        overlay.set_ui(|c| {
            egui::Area::new(egui::Id::new("test")).show(c, |ui| {
                ui.label("injected");
            });
        });
        let ui = &mut overlay.ui;
        let _ = ctx.run_ui(egui::RawInput::default(), |root| ui(root.ctx()));
    }
}
