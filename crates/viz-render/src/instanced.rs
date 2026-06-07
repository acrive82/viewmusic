//! Instanced quads (rect / circle / triangle), two blend modes.
//!
//! ## Pipeline strategy (justification)
//! Shape masking and blend mode are both *per-layer* constants, never per-instance. The
//! shape mask is cheapest to select at pipeline level (the fragment shader for each shape
//! has a distinct, branchless discard test), and wgpu's `downlevel_defaults` limits — the
//! device this runs on — do NOT guarantee push constants, so a per-draw push-constant
//! "shape id" is not portable here. We therefore build a small **3 shapes × 2 blends = 6
//! pipeline** table sharing one vertex shader and one WGSL module. This avoids a per-draw
//! uniform rebind (and the within-frame aliasing that a single shared uniform buffer would
//! cause when many draws record into one encoder), keeps the hot path to "set pipeline →
//! set vertex buffer slice → draw", and costs only 6 cheap pipeline objects at startup.
//!
//! No viewport uniform is needed here: shapes are defined entirely in NDC, so there is no
//! screen-space conversion (unlike polylines). Instanced draws use no bind groups at all.
//!
//! ## Geometry
//! One unit quad (two triangles, 6 vertices in `[-0.5, 0.5]²` "local" space) is expanded
//! per instance in the vertex shader: scale by `w`/`h`, rotate by `rot`, translate to
//! `x`/`y`. The local coordinate is forwarded so the fragment shader can mask circle and
//! triangle shapes. Colors are premultiplied in the shader before blending.

use wgpu::util::DeviceExt;

use crate::{BlendMode, InstanceData, ShapeKind, MAX_INSTANCES};

/// Max instanced layers we track per frame (contract §12: 16 layers per scene; even if
/// every layer were instanced this is the ceiling). Pre-sized to avoid per-frame alloc.
const MAX_LAYERS: usize = 16;

/// Local-space unit quad (two triangles). Forwarded to the fragment shader for masking.
const QUAD: [[f32; 2]; 6] = [
    [-0.5, -0.5],
    [0.5, -0.5],
    [0.5, 0.5],
    [-0.5, -0.5],
    [0.5, 0.5],
    [-0.5, 0.5],
];

/// One instanced layer recorded this frame: where its data sits and how to draw it.
#[derive(Clone, Copy)]
struct LayerRecord {
    shape: ShapeKind,
    blend: BlendMode,
    /// First instance index into the shared instance buffer.
    base: u32,
    /// Instance count.
    count: u32,
}

/// All instanced pipelines and the shared, pre-allocated buffers.
pub struct InstancedPipelines {
    /// `pipelines[shape][blend]`.
    pipelines: [[wgpu::RenderPipeline; 2]; 3],
    /// Static unit-quad vertex buffer.
    quad: wgpu::Buffer,
    /// Shared instance buffer, capacity `MAX_LAYERS * MAX_INSTANCES`.
    instances: wgpu::Buffer,
    /// Per-frame layer records (fixed-size; `len` tracks how many are live).
    records: [LayerRecord; MAX_LAYERS],
    len: usize,
    /// Running write cursor (in instances) into the shared buffer for the current frame.
    cursor: u32,
}

impl InstancedPipelines {
    /// Builds the 6 pipelines and allocates the static quad + shared instance buffer.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("instanced-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("instanced.wgsl").into()),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("instanced-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });

        // Vertex layout: buffer 0 = unit quad (per-vertex), buffer 1 = instance data.
        let quad_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2],
        };
        let inst_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<InstanceData>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            // x,y,w,h,rot packed as a vec4 (1) + f32 (2); color vec4 (3).
            attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32, 3 => Float32x4],
        };

        let fs_entries = ["fs_rect", "fs_circle", "fs_triangle"];
        let blends = [BlendMode::Alpha, BlendMode::Add];

        let make = |fs: &str, blend: BlendMode| -> wgpu::RenderPipeline {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("instanced-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[quad_layout.clone(), inst_layout.clone()],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
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

        let pipelines =
            std::array::from_fn(|s| std::array::from_fn(|b| make(fs_entries[s], blends[b])));

        let quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instanced-quad"),
            contents: bytemuck::cast_slice(&QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instanced-data"),
            size: (std::mem::size_of::<InstanceData>() * MAX_LAYERS * MAX_INSTANCES)
                as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let zero = LayerRecord {
            shape: ShapeKind::Rect,
            blend: BlendMode::Alpha,
            base: 0,
            count: 0,
        };
        Self {
            pipelines,
            quad,
            instances,
            records: [zero; MAX_LAYERS],
            len: 0,
            cursor: 0,
        }
    }

    /// Resets per-frame bookkeeping. Call once before pushing this frame's layers.
    pub fn begin_frame(&mut self) {
        self.len = 0;
        self.cursor = 0;
    }

    /// Uploads one instanced layer's data and records how to draw it.
    ///
    /// Slices longer than the remaining capacity are truncated (defence in depth; the
    /// runtime already clamps to 4096). Empty layers are skipped (zero draw).
    pub fn push(
        &mut self,
        queue: &wgpu::Queue,
        shape: ShapeKind,
        blend: BlendMode,
        instances: &[InstanceData],
    ) {
        if self.len >= MAX_LAYERS {
            return;
        }
        let cap_total = (MAX_LAYERS * MAX_INSTANCES) as u32;
        let remaining = cap_total.saturating_sub(self.cursor) as usize;
        let take = instances.len().min(MAX_INSTANCES).min(remaining);
        let base = self.cursor;
        if take > 0 {
            let offset = base as u64 * std::mem::size_of::<InstanceData>() as u64;
            queue.write_buffer(
                &self.instances,
                offset,
                bytemuck::cast_slice(&instances[..take]),
            );
        }
        self.records[self.len] = LayerRecord {
            shape,
            blend,
            base,
            count: take as u32,
        };
        self.len += 1;
        self.cursor += take as u32;
    }

    /// Records the draw for the `idx`-th instanced layer pushed this frame.
    pub fn draw<'p>(&'p self, pass: &mut wgpu::RenderPass<'p>, idx: usize) {
        if idx >= self.len {
            return;
        }
        let rec = self.records[idx];
        if rec.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipelines[rec.shape.index()][rec.blend.index()]);
        pass.set_vertex_buffer(0, self.quad.slice(..));
        let start = rec.base as u64 * std::mem::size_of::<InstanceData>() as u64;
        let end = (rec.base + rec.count) as u64 * std::mem::size_of::<InstanceData>() as u64;
        pass.set_vertex_buffer(1, self.instances.slice(start..end));
        pass.draw(0..QUAD.len() as u32, 0..rec.count);
    }
}
