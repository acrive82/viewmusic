//! Feedback / trails: a persistent offscreen accumulator.
//!
//! When feedback is active each frame does three things:
//! 1. **Decay** — multiply the previous frame's accumulated image by `decay`.
//! 2. **Draw** — render this frame's layers on top (handled by [`crate::Renderer`]).
//! 3. **Blit** — copy the accumulator onto the surface.
//!
//! ## Why ping-pong
//! A pass cannot sample the texture it is rendering to. We keep **two** accumulator
//! textures and alternate roles each active frame: the decay pass renders into `curr`
//! while sampling `prev` (writing `prev * decay`), the layers then draw into `curr`, the
//! blit samples `curr`, and finally the roles swap so next frame's `prev` is this frame's
//! result. Both textures use the surface format and are both sampleable + renderable.
//!
//! ## Glitch-free resize
//! [`Feedback::resize`] reallocates both textures and **clears** them in the same call, so
//! no garbage from a stale-sized texture ever flashes during a resize. The roles are also
//! reset deterministically.

use crate::CLEAR_COLOR;

/// Two-texture ping-pong accumulator + the fullscreen decay/blit pipelines.
///
/// Sizing is owned by [`crate::Renderer`]; this struct only holds the GPU resources and
/// reallocates them on [`Feedback::resize`].
pub struct Feedback {
    /// The two accumulator textures and their views. Index by the `prev`/`curr` selectors.
    textures: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
    /// Bind group that samples `textures[k]` (so `sample_bind[prev]` reads the prev image).
    sample_bind: [wgpu::BindGroup; 2],

    /// Which texture index currently holds the previous frame's accumulation.
    prev: usize,

    sampler: wgpu::Sampler,
    bind_layout: wgpu::BindGroupLayout,
    /// Pipeline that multiplies the sampled texture by a decay uniform.
    decay_pipeline: wgpu::RenderPipeline,
    /// Pipeline that copies the sampled texture straight through (blit).
    blit_pipeline: wgpu::RenderPipeline,
    /// Uniform holding the current `decay` factor (one f32, padded to 16 bytes).
    decay_uniform: wgpu::Buffer,
    /// Bind group for the decay uniform.
    decay_uniform_bind: wgpu::BindGroup,

    /// Set after (re)allocation: both accumulators have undefined contents and must be
    /// cleared before they are first sampled. Cleared lazily at the start of the next
    /// active feedback frame (the encoder is available there; no queue needed for a
    /// clear-load render pass).
    dirty: bool,
}

impl Feedback {
    /// Builds the pipelines, sampler, and the two accumulator textures at `width × height`.
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("feedback-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("feedback.wgsl").into()),
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("feedback-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Bind group 0: sampled texture + sampler.
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("feedback-sample-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        // Bind group 1 (decay pass only): the decay scalar uniform.
        let decay_uniform_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("feedback-decay-uniform-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let decay_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("feedback-decay-uniform"),
            size: 16, // one f32, padded to 16 bytes (uniform alignment).
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let decay_uniform_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("feedback-decay-uniform-bind"),
            layout: &decay_uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: decay_uniform.as_entire_binding(),
            }],
        });

        let decay_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("feedback-decay-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_layout), Some(&decay_uniform_layout)],
            immediate_size: 0,
        });
        let blit_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("feedback-blit-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        // Fullscreen passes overwrite the whole target → no blending needed.
        let target = wgpu::ColorTargetState {
            format,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        };

        let decay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("feedback-decay-pipeline"),
            layout: Some(&decay_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_fullscreen"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_decay"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(target.clone())],
            }),
            multiview_mask: None,
            cache: None,
        });

        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("feedback-blit-pipeline"),
            layout: Some(&blit_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_fullscreen"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_blit"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(target)],
            }),
            multiview_mask: None,
            cache: None,
        });

        let (textures, views, sample_bind) =
            Self::make_targets(device, format, width, height, &bind_layout, &sampler);

        Self {
            textures,
            views,
            sample_bind,
            prev: 0,
            sampler,
            bind_layout,
            decay_pipeline,
            blit_pipeline,
            decay_uniform,
            decay_uniform_bind,
            dirty: true,
        }
    }

    /// Allocates the two accumulator textures + their views + sampling bind groups.
    fn make_targets(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        bind_layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) -> (
        [wgpu::Texture; 2],
        [wgpu::TextureView; 2],
        [wgpu::BindGroup; 2],
    ) {
        let make_tex = |label: &str| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    // COPY_SRC so the GPU smoke test can read this texture back too.
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let textures = [make_tex("feedback-a"), make_tex("feedback-b")];
        let views = [
            textures[0].create_view(&wgpu::TextureViewDescriptor::default()),
            textures[1].create_view(&wgpu::TextureViewDescriptor::default()),
        ];
        let make_bind = |view: &wgpu::TextureView| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("feedback-sample-bind"),
                layout: bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        };
        let sample_bind = [make_bind(&views[0]), make_bind(&views[1])];
        (textures, views, sample_bind)
    }

    /// Reallocates both accumulators to a new size and clears them (glitch-free).
    pub fn resize(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let (textures, views, sample_bind) = Self::make_targets(
            device,
            format,
            width,
            height,
            &self.bind_layout,
            &self.sampler,
        );
        self.textures = textures;
        self.views = views;
        self.sample_bind = sample_bind;
        self.prev = 0;
        // Fresh textures have undefined contents; defer the clear to the next active frame
        // (encoder available there), so resize needs no queue and never flashes garbage.
        self.dirty = true;
    }

    /// Clears both accumulator textures to the background color if they are dirty
    /// (post-allocation). Records into the caller's encoder; no queue needed.
    fn clear_if_dirty(&mut self, encoder: &mut wgpu::CommandEncoder) {
        if !self.dirty {
            return;
        }
        for view in &self.views {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("feedback-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
        }
        self.dirty = false;
    }

    /// Index of the texture currently acting as the render target (`curr`).
    fn curr(&self) -> usize {
        1 - self.prev
    }

    /// The view of the current render target (`curr`) — layers draw here.
    pub fn target_view(&self) -> &wgpu::TextureView {
        &self.views[self.curr()]
    }

    /// Records the decay pass: render `curr = prev * decay`, sampling `prev`. After this
    /// the layers draw into `curr` with `LoadOp::Load`.
    ///
    /// The decay scalar must already be in the uniform via [`Feedback::set_decay`]
    /// (queued before recording). Clears freshly (re)allocated accumulators first.
    pub fn decay_pass(&mut self, encoder: &mut wgpu::CommandEncoder) {
        self.clear_if_dirty(encoder);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("feedback-decay"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.views[self.curr()],
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    // Overwritten entirely by the fullscreen pass.
                    load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.decay_pipeline);
        pass.set_bind_group(0, &self.sample_bind[self.prev], &[]);
        pass.set_bind_group(1, &self.decay_uniform_bind, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Writes the decay scalar into the uniform (queued; call before recording the frame).
    pub fn set_decay(&self, queue: &wgpu::Queue, decay: f32) {
        let v = [decay.clamp(0.0, 0.99), 0.0, 0.0, 0.0];
        queue.write_buffer(&self.decay_uniform, 0, bytemuck::cast_slice(&v));
    }

    /// Records the blit pass: copy `curr` onto `dst` (the surface). Then advances the
    /// ping-pong so next frame's `prev` is this frame's accumulator.
    pub fn blit_to(&mut self, encoder: &mut wgpu::CommandEncoder, dst: &wgpu::TextureView) {
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("feedback-blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: dst,
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
            pass.set_pipeline(&self.blit_pipeline);
            pass.set_bind_group(0, &self.sample_bind[self.curr()], &[]);
            pass.draw(0..3, 0..1);
        }
        // Swap: this frame's `curr` becomes next frame's `prev`.
        self.prev = self.curr();
    }
}
