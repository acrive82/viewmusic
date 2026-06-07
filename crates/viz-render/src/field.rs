//! `field` layer: a `resolution × resolution` color grid.
//!
//! Each cell is one instanced quad covering its NDC rectangle. Cells are laid out
//! row-major from the bottom-left (`i = row * resolution + col`), matching the contract
//! so `cells[i]` lands in the right place. To stay bind-group-free and avoid per-layer
//! uniform aliasing across multiple field layers in one frame, each cell instance carries
//! its own center + half-size in NDC alongside its color; the vertex shader just expands a
//! unit quad. Pre-allocated for `MAX_FIELD_CELLS` cells per layer.

use wgpu::util::DeviceExt;

use crate::{BlendMode, MAX_FIELD_CELLS, MAX_FIELD_RES};

/// Max field layers tracked per frame (contract §12: ≤16 layers per scene).
const MAX_LAYERS: usize = 16;

/// One cell instance: NDC center + half-size + straight-alpha color.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Cell {
    /// Cell center, NDC.
    center: [f32; 2],
    /// Cell half-size, NDC.
    half: [f32; 2],
    /// Straight-alpha RGBA.
    color: [f32; 4],
}

/// One field layer recorded this frame.
#[derive(Clone, Copy)]
struct LayerRecord {
    blend: BlendMode,
    base: u32,
    count: u32,
}

/// Field pipelines (one per blend) + shared cell instance buffer.
pub struct FieldPipelines {
    pipelines: [wgpu::RenderPipeline; 2],
    /// Static unit quad ([-0.5,0.5]^2) as 6 vertices.
    quad: wgpu::Buffer,
    /// Shared cell instance buffer (capacity `MAX_LAYERS * MAX_FIELD_CELLS`).
    cells_buf: wgpu::Buffer,
    /// CPU staging for the current layer's cells.
    staging: Vec<Cell>,
    records: [LayerRecord; MAX_LAYERS],
    len: usize,
    cursor: u32,
}

const QUAD: [[f32; 2]; 6] = [
    [-0.5, -0.5],
    [0.5, -0.5],
    [0.5, 0.5],
    [-0.5, -0.5],
    [0.5, 0.5],
    [-0.5, 0.5],
];

impl FieldPipelines {
    /// Builds the two pipelines and allocates buffers + CPU staging.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("field-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("field.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("field-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });

        let quad_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2],
        };
        let cell_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Cell>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            // center (1), half (2), color (3).
            attributes: &wgpu::vertex_attr_array![1 => Float32x2, 2 => Float32x2, 3 => Float32x4],
        };

        let make = |blend: BlendMode| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("field-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[quad_layout.clone(), cell_layout.clone()],
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

        let quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("field-quad"),
            contents: bytemuck::cast_slice(&QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let cells_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("field-cells"),
            size: (std::mem::size_of::<Cell>() * MAX_LAYERS * MAX_FIELD_CELLS)
                as wgpu::BufferAddress,
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
            quad,
            cells_buf,
            staging: Vec::with_capacity(MAX_FIELD_CELLS),
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

    /// Builds and uploads the cells for one field layer.
    ///
    /// `resolution` is clamped to `2..=MAX_FIELD_RES`. `cells` must hold `resolution²`
    /// colors row-major from bottom-left; a shorter slice fills as many cells as provided.
    pub fn push(
        &mut self,
        queue: &wgpu::Queue,
        blend: BlendMode,
        resolution: u32,
        cells: &[[f32; 4]],
    ) {
        if self.len >= MAX_LAYERS {
            return;
        }
        self.staging.clear();

        let res = resolution.clamp(2, MAX_FIELD_RES);
        let res_f = res as f32;
        // Cell spans 2/res in NDC; half-size is 1/res.
        let half = 1.0 / res_f;
        let total = (res * res) as usize;
        let avail = cells.len().min(total);
        for (i, &color) in cells.iter().enumerate().take(avail) {
            let col = (i as u32) % res;
            let row = (i as u32) / res;
            // Center of cell (col,row): bottom-left origin, NDC −1..1.
            let cx = -1.0 + (col as f32 + 0.5) * 2.0 / res_f;
            let cy = -1.0 + (row as f32 + 0.5) * 2.0 / res_f;
            self.staging.push(Cell {
                center: [cx, cy],
                half: [half, half],
                color,
            });
        }

        let cap_total = (MAX_LAYERS * MAX_FIELD_CELLS) as u32;
        let remaining = cap_total.saturating_sub(self.cursor) as usize;
        let take = self.staging.len().min(remaining);
        let base = self.cursor;
        if take > 0 {
            let offset = base as u64 * std::mem::size_of::<Cell>() as u64;
            queue.write_buffer(
                &self.cells_buf,
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

    /// Records the draw for the `idx`-th field layer pushed this frame.
    pub fn draw<'p>(&'p self, pass: &mut wgpu::RenderPass<'p>, idx: usize) {
        if idx >= self.len {
            return;
        }
        let rec = self.records[idx];
        if rec.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipelines[rec.blend.index()]);
        pass.set_vertex_buffer(0, self.quad.slice(..));
        let start = rec.base as u64 * std::mem::size_of::<Cell>() as u64;
        let end = (rec.base + rec.count) as u64 * std::mem::size_of::<Cell>() as u64;
        pass.set_vertex_buffer(1, self.cells_buf.slice(start..end));
        pass.draw(0..QUAD.len() as u32, 0..rec.count);
    }
}
