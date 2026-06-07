//! Thick polylines as instanced screen-space segment quads.
//!
//! Each consecutive point pair `(p0, p1)` becomes one instanced quad. The quad is a unit
//! template in `[0,1] × [-0.5, 0.5]`: the x axis runs along the segment, the y axis is the
//! perpendicular offset scaled by `thickness`. Thickness is specified in **logical
//! pixels** and converted to NDC using the current target size — the segment direction is
//! computed in pixel space so the line keeps a constant visual width regardless of aspect.
//! Per-point colors are interpolated along the segment. A `closed` loop adds the final
//! `last → first` segment.
//!
//! Pre-allocated for `MAX_POINTS` points per layer ⇒ `MAX_POINTS` segments (closed).

use wgpu::util::DeviceExt;

use crate::{BlendMode, PointData, MAX_POINTS};

/// Max polyline layers tracked per frame (contract §12: ≤16 layers per scene).
const MAX_LAYERS: usize = 16;

/// One expanded segment instance: endpoints (NDC) + endpoint colors + half-thickness in
/// NDC for each axis (pre-divided by the viewport so the shader stays trivial).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Segment {
    /// Segment start, NDC.
    p0: [f32; 2],
    /// Segment end, NDC.
    p1: [f32; 2],
    /// Half-thickness in NDC along x and y (= half_px / (width/2), half_px / (height/2)).
    half_ndc: [f32; 2],
    /// Padding to keep the struct 16-byte aligned for tidy vertex attributes.
    _pad: [f32; 2],
    /// Start color (straight alpha).
    c0: [f32; 4],
    /// End color (straight alpha).
    c1: [f32; 4],
}

/// One polyline layer recorded this frame.
#[derive(Clone, Copy)]
struct LayerRecord {
    blend: BlendMode,
    base: u32,
    count: u32,
}

/// Polyline pipelines (one per blend) + the shared segment instance buffer.
pub struct PolylinePipelines {
    pipelines: [wgpu::RenderPipeline; 2],
    /// Static template quad ([0,1] × [-0.5,0.5]) as 6 vertices.
    template: wgpu::Buffer,
    /// Shared segment instance buffer (capacity `MAX_LAYERS * MAX_POINTS`).
    segments: wgpu::Buffer,
    /// CPU staging for the current layer's segments (pre-allocated, no per-frame alloc).
    staging: Vec<Segment>,
    records: [LayerRecord; MAX_LAYERS],
    len: usize,
    cursor: u32,
}

/// Template quad: x along segment in [0,1], y perpendicular in [-0.5, 0.5].
const TEMPLATE: [[f32; 2]; 6] = [
    [0.0, -0.5],
    [1.0, -0.5],
    [1.0, 0.5],
    [0.0, -0.5],
    [1.0, 0.5],
    [0.0, 0.5],
];

impl PolylinePipelines {
    /// Builds the two pipelines and allocates buffers + CPU staging.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("polyline-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("polyline.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("polyline-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });

        let template_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2],
        };
        let seg_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Segment>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            // p0 (1), p1 (2), half_ndc+pad (3 vec4), c0 (4), c1 (5).
            attributes: &wgpu::vertex_attr_array![
                1 => Float32x2,
                2 => Float32x2,
                3 => Float32x4,
                4 => Float32x4,
                5 => Float32x4
            ],
        };

        let make = |blend: BlendMode| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("polyline-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[template_layout.clone(), seg_layout.clone()],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend.state()),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let pipelines = [make(BlendMode::Alpha), make(BlendMode::Add)];

        let template = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("polyline-template"),
            contents: bytemuck::cast_slice(&TEMPLATE),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let segments = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("polyline-segments"),
            size: (std::mem::size_of::<Segment>() * MAX_LAYERS * MAX_POINTS) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let zero = LayerRecord {
            blend: BlendMode::Alpha,
            base: 0,
            count: 0,
        };
        Self {
            pipelines,
            template,
            segments,
            staging: Vec::with_capacity(MAX_POINTS),
            records: [zero; MAX_LAYERS],
            len: 0,
            cursor: 0,
        }
    }

    /// Resets per-frame bookkeeping.
    pub fn begin_frame(&mut self) {
        self.len = 0;
        self.cursor = 0;
    }

    /// Builds and uploads the segments for one polyline layer.
    ///
    /// `thickness_px` is logical pixels, clamped to a sane floor (≥0.1). `width`/`height`
    /// are the current target dimensions in physical pixels (used for the px→NDC scale).
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        queue: &wgpu::Queue,
        blend: BlendMode,
        thickness_px: f32,
        closed: bool,
        points: &[PointData],
        width: u32,
        height: u32,
    ) {
        if self.len >= MAX_LAYERS {
            return;
        }
        self.staging.clear();

        let n = points.len().min(MAX_POINTS);
        // A line needs at least two points.
        if n >= 2 {
            // Half-thickness in NDC: NDC spans 2 units across width/height px, so 1 px =
            // 2/dim in NDC, and half-thickness px = thickness/2 px.
            let half_px = (thickness_px.max(0.1)) * 0.5;
            let half_ndc = [
                half_px * 2.0 / width.max(1) as f32,
                half_px * 2.0 / height.max(1) as f32,
            ];
            let seg_count = if closed { n } else { n - 1 };
            for s in 0..seg_count {
                let a = &points[s];
                let b = &points[(s + 1) % n];
                self.staging.push(Segment {
                    p0: [a.x, a.y],
                    p1: [b.x, b.y],
                    half_ndc,
                    _pad: [0.0, 0.0],
                    c0: a.color,
                    c1: b.color,
                });
            }
        }

        let cap_total = (MAX_LAYERS * MAX_POINTS) as u32;
        let remaining = cap_total.saturating_sub(self.cursor) as usize;
        let take = self.staging.len().min(remaining);
        let base = self.cursor;
        if take > 0 {
            let offset = base as u64 * std::mem::size_of::<Segment>() as u64;
            queue.write_buffer(
                &self.segments,
                offset,
                bytemuck::cast_slice(&self.staging[..take]),
            );
        }
        self.records[self.len] = LayerRecord {
            blend,
            base,
            count: take as u32,
        };
        self.len += 1;
        self.cursor += take as u32;
    }

    /// Records the draw for the `idx`-th polyline layer pushed this frame.
    pub fn draw<'p>(&'p self, pass: &mut wgpu::RenderPass<'p>, idx: usize) {
        if idx >= self.len {
            return;
        }
        let rec = self.records[idx];
        if rec.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipelines[rec.blend.index()]);
        pass.set_vertex_buffer(0, self.template.slice(..));
        let start = rec.base as u64 * std::mem::size_of::<Segment>() as u64;
        let end = (rec.base + rec.count) as u64 * std::mem::size_of::<Segment>() as u64;
        pass.set_vertex_buffer(1, self.segments.slice(start..end));
        pass.draw(0..TEMPLATE.len() as u32, 0..rec.count);
    }
}
