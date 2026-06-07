//! viz-render — the GPU layer renderer for ViewMusic.
//!
//! Consumes the runtime's per-frame draw lists ([`LayerDraw`]) and rasterizes them
//! into the window surface via wgpu 29 (Metal on this target). The renderer is driven
//! from `viz-app`'s `RenderCallback`/`FrameContext` seam: the app acquires the surface
//! texture, records a dark clear pass, then hands us the same command encoder and
//! surface view. We render the whole scene and leave the surface ready for the egui
//! overlay pass, which loads on top with `LoadOp::Load`.
//!
//! ## Coordinate contract
//! Artifact space is `x ∈ −1..1` (right), `y ∈ −1..1` (up), mapping DIRECTLY to NDC.
//! Sizes `w`/`h` are in the same units; `rot` is radians CCW about the element center.
//! Colors arrive as straight-alpha `[f32; 4]` in `0..1` (already clamped/converted by
//! the runtime); the shaders premultiply before blending so alpha-over and additive
//! both behave.
//!
//! ## Real-time discipline
//! Every GPU buffer is pre-allocated at construction to its contract maximum
//! ([`MAX_INSTANCES`], [`MAX_POINTS`], [`MAX_FIELD_CELLS`]). The per-frame path only
//! calls [`wgpu::Queue::write_buffer`] into those buffers — never creates or grows
//! them. No heap allocation, no locks, no blocking I/O in [`Renderer::render`].
//!
//! ## Module map
//! - [`instanced`]: instanced quads with rect/circle/triangle masking, two blends.
//! - [`polyline`]: thick polylines as instanced screen-space segment quads.
//! - [`field`]: resolution×resolution color grid as instanced cell quads.
//! - [`feedback`]: persistent decay/trails framebuffer + blit-to-surface.
//! - [`degrade`]: pure adaptive-detail / pause ladder logic (no GPU).

mod degrade;
mod feedback;
mod field;
mod instanced;
mod polyline;

pub use degrade::DegradeController;

/// Maximum instances per `instanced` layer (contract §12: 4096).
pub const MAX_INSTANCES: usize = 4096;
/// Maximum points per `polyline` layer (contract §12: 4096).
pub const MAX_POINTS: usize = 4096;
/// Maximum `field` resolution per axis (contract §12: 128 ⇒ 16 384 cells).
pub const MAX_FIELD_RES: u32 = 128;
/// Maximum `field` cells (`MAX_FIELD_RES²`).
pub const MAX_FIELD_CELLS: usize = (MAX_FIELD_RES * MAX_FIELD_RES) as usize;

/// One instanced shape, in artifact/NDC coordinates.
///
/// `repr(C)`, `Pod`+`Zeroable` so it can be `bytemuck`-cast straight into a GPU
/// instance buffer with no per-frame allocation. The runtime fills a slice of these
/// and hands it over via [`LayerDraw::Instanced`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceData {
    /// Center x in NDC (−1..1, right).
    pub x: f32,
    /// Center y in NDC (−1..1, up).
    pub y: f32,
    /// Full width in NDC units.
    pub w: f32,
    /// Full height in NDC units.
    pub h: f32,
    /// Rotation in radians, CCW about the center.
    pub rot: f32,
    /// Straight-alpha color, `0..1`, RGBA.
    pub color: [f32; 4],
}

/// One polyline vertex, in artifact/NDC coordinates.
///
/// `repr(C)`, `Pod`+`Zeroable`. The renderer expands consecutive points into thick
/// screen-space segment quads (see [`polyline`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PointData {
    /// Point x in NDC (−1..1).
    pub x: f32,
    /// Point y in NDC (−1..1).
    pub y: f32,
    /// Straight-alpha color, `0..1`, RGBA.
    pub color: [f32; 4],
}

/// Fixed-function GPU blend mode for a layer.
///
/// Mapped from the contract's `blend` at the call site in the runtime (this crate does
/// NOT depend on `viz-contract`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlendMode {
    /// Standard alpha-over (premultiplied): `src + dst*(1 − src.a)`.
    Alpha,
    /// Additive (premultiplied): `src + dst` — for glow.
    Add,
}

impl BlendMode {
    /// Index into the two-element pipeline tables (`0 = Alpha`, `1 = Add`).
    #[inline]
    fn index(self) -> usize {
        match self {
            BlendMode::Alpha => 0,
            BlendMode::Add => 1,
        }
    }

    /// The wgpu blend state for this mode. Shaders output premultiplied-alpha color, so
    /// alpha-over uses `src_factor = One` (not `SrcAlpha`).
    fn state(self) -> wgpu::BlendState {
        match self {
            BlendMode::Alpha => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
            },
            BlendMode::Add => wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
            },
        }
    }
}

/// Which primitive an [`LayerDraw::Instanced`] layer rasterizes.
///
/// Selects a fragment mask (circle radius / triangle half-plane); see [`instanced`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    /// Axis-then-rotated rectangle filling the full `w×h` quad.
    Rect,
    /// Inscribed circle (fragments outside radius 0.5 of the quad are discarded).
    Circle,
    /// Upward isoceles triangle inscribed in the quad (half-plane mask).
    Triangle,
}

impl ShapeKind {
    /// Index into the three-element pipeline table (`0 = Rect`, `1 = Circle`, `2 = Tri`).
    #[inline]
    fn index(self) -> usize {
        match self {
            ShapeKind::Rect => 0,
            ShapeKind::Circle => 1,
            ShapeKind::Triangle => 2,
        }
    }
}

/// One layer's worth of geometry to draw, borrowed from the runtime's staging buffers.
///
/// The renderer iterates a `&[LayerDraw]` in order (painter's algorithm) each frame.
/// Slices are truncated to the pre-allocated capacities if a caller ever exceeds them
/// (defence in depth; the runtime already clamps to contract limits).
#[derive(Debug)]
pub enum LayerDraw<'a> {
    /// N instanced shapes of one kind/blend.
    Instanced {
        /// Primitive kind.
        shape: ShapeKind,
        /// Blend mode.
        blend: BlendMode,
        /// Per-instance transform + color.
        instances: &'a [InstanceData],
    },
    /// A connected strip (optionally closed) of thick points.
    Polyline {
        /// Blend mode.
        blend: BlendMode,
        /// Line thickness in logical pixels (converted to NDC using the target size).
        thickness_px: f32,
        /// `true` joins last→first (radial scopes).
        closed: bool,
        /// Ordered points with per-point color (interpolated along each segment).
        points: &'a [PointData],
    },
    /// A `resolution × resolution` color grid (row-major from bottom-left).
    Field {
        /// Blend mode.
        blend: BlendMode,
        /// Cells per axis (clamped to [`MAX_FIELD_RES`]).
        resolution: u32,
        /// `resolution²` straight-alpha RGBA colors, row-major from bottom-left.
        cells: &'a [[f32; 4]],
    },
}

/// Scene-wide parameters for one frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct SceneParams {
    /// Feedback/trails decay in `0..0.99`. `None` disables trails (layers drawn directly
    /// to the surface over a dark clear). `Some(d)` multiplies the previous frame by `d`
    /// before drawing this frame's layers (contract §8).
    pub feedback_decay: Option<f32>,
}

/// Background clear color used both for the no-feedback direct path and for clearing the
/// feedback texture on (re)allocation. Matches the app shell's dark clear.
const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.02,
    g: 0.02,
    b: 0.03,
    a: 1.0,
};

/// The GPU scene renderer.
///
/// Holds all pipelines and pre-allocated buffers/targets. Construct once against the
/// app's `device`/`queue`/`surface_format`; drive [`Renderer::render`] each frame from
/// the `RenderCallback`; call [`Renderer::resize`] on surface resize.
pub struct Renderer {
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,

    instanced: instanced::InstancedPipelines,
    polyline: polyline::PolylinePipelines,
    field: field::FieldPipelines,
    feedback: feedback::Feedback,
}

impl Renderer {
    /// Builds the renderer: all pipelines, buffers, and the feedback target.
    ///
    /// `surface_format` MUST match the surface the encoder's view targets (and the egui
    /// overlay that follows). Blocking-free; safe to call during app `on_ready`.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
    ) -> Self {
        // Start at a 1×1 target; the app calls resize() with the real size before/at the
        // first frame. Feedback allocates lazily to the current size.
        let width = 1;
        let height = 1;
        let instanced = instanced::InstancedPipelines::new(device, surface_format);
        let polyline = polyline::PolylinePipelines::new(device, surface_format);
        let field = field::FieldPipelines::new(device, surface_format);
        let feedback = feedback::Feedback::new(device, surface_format, width, height);
        let _ = queue; // queue not needed at construction; kept for API symmetry.
        Self {
            format: surface_format,
            width,
            height,
            instanced,
            polyline,
            field,
            feedback,
        }
    }

    /// Reallocates size-dependent targets for a new surface size (glitch-free: the
    /// feedback texture is recreated and cleared so no garbage flashes during a resize).
    ///
    /// Zero dimensions are clamped to 1 to keep wgpu happy on a minimized window.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.feedback.resize(device, self.format, width, height);
        tracing::debug!(width, height, "viz-render: resized targets");
    }

    /// Renders the whole scene for one frame.
    ///
    /// Records into the caller's `encoder` and targets `surface_view`. After this returns
    /// the surface holds the composited scene and is ready for the egui overlay pass
    /// (which uses `LoadOp::Load`). No allocation occurs on this path.
    ///
    /// Two paths:
    /// - `feedback_decay = None`: clear the surface dark, draw layers directly onto it.
    /// - `feedback_decay = Some(d)`: decay the persistent feedback texture by `d`, draw
    ///   layers INTO it, then blit it onto the surface.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        surface_view: &wgpu::TextureView,
        params: &SceneParams,
        layers: &[LayerDraw],
    ) {
        let _ = device; // sizes are fixed by resize(); no per-frame device use.

        // Upload all per-layer geometry once, up front, so render passes only bind+draw.
        self.upload(queue, layers);

        match params.feedback_decay {
            None => {
                // Direct path: clear surface, draw layers on top.
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("viz-render-direct"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: surface_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                self.draw_layers(&mut pass, layers);
            }
            Some(decay) => {
                let decay = decay.clamp(0.0, 0.99);
                // Queue the decay scalar before recording the pass that reads it.
                self.feedback.set_decay(queue, decay);
                // 1) Multiply the previous accumulator by decay into `curr` (ping-pong).
                self.feedback.decay_pass(encoder);
                // 2) Draw this frame's layers INTO the feedback texture (load, don't clear).
                {
                    let view = self.feedback.target_view();
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("viz-render-into-feedback"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view,
                            resolve_target: None,
                            depth_slice: None,
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
                    self.draw_layers(&mut pass, layers);
                }
                // 3) Blit feedback → surface (clear the surface first, then copy).
                self.feedback.blit_to(encoder, surface_view);
            }
        }
    }

    /// Uploads all geometry for this frame into the pre-allocated GPU buffers.
    fn upload(&mut self, queue: &wgpu::Queue, layers: &[LayerDraw]) {
        // Reset per-frame draw bookkeeping, then append each layer's data.
        self.instanced.begin_frame();
        self.polyline.begin_frame();
        self.field.begin_frame();
        for layer in layers {
            match layer {
                LayerDraw::Instanced {
                    shape,
                    blend,
                    instances,
                } => {
                    self.instanced.push(queue, *shape, *blend, instances);
                }
                LayerDraw::Polyline {
                    blend,
                    thickness_px,
                    closed,
                    points,
                } => {
                    self.polyline.push(
                        queue,
                        *blend,
                        *thickness_px,
                        *closed,
                        points,
                        self.width,
                        self.height,
                    );
                }
                LayerDraw::Field {
                    blend,
                    resolution,
                    cells,
                } => {
                    self.field.push(queue, *blend, *resolution, cells);
                }
            }
        }
    }

    /// Records draw calls for every layer, in order, into an open render pass.
    fn draw_layers<'p>(&'p self, pass: &mut wgpu::RenderPass<'p>, layers: &[LayerDraw]) {
        // Each sub-module tracks how many of its layers it has emitted this frame so the
        // per-layer GPU buffer offsets line up with the upload() order.
        let mut inst_idx = 0;
        let mut poly_idx = 0;
        let mut field_idx = 0;
        for layer in layers {
            match layer {
                LayerDraw::Instanced { .. } => {
                    self.instanced.draw(pass, inst_idx);
                    inst_idx += 1;
                }
                LayerDraw::Polyline { .. } => {
                    self.polyline.draw(pass, poly_idx);
                    poly_idx += 1;
                }
                LayerDraw::Field { .. } => {
                    self.field.draw(pass, field_idx);
                    field_idx += 1;
                }
            }
        }
    }
}
