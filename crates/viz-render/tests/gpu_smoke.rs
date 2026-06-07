//! Headless GPU smoke test for the viz-render renderer.
//!
//! Creates a wgpu device with NO surface (Metal is always present on this Mac, so no env
//! guard), renders each [`LayerDraw`] kind — and the feedback path — into an offscreen
//! RGBA8 texture, reads the pixels back, and asserts:
//!  (a) no validation errors occurred (`device.on_uncaptured_error` panics),
//!  (b) drawn pixels differ from the clear color where geometry is,
//!  (c) additive blend brightens overlapping instances.
//!
//! Run directly: `cargo test -p viz-render`.

use std::sync::Arc;

use viz_render::{BlendMode, InstanceData, LayerDraw, PointData, Renderer, SceneParams, ShapeKind};

/// Offscreen target format. RGBA8 unorm — straightforward readback math.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const W: u32 = 256;
const H: u32 = 256;

/// A read-back RGBA8 framebuffer with row-major, top-left origin (matches wgpu texels).
struct Readback {
    pixels: Vec<[u8; 4]>,
    width: u32,
    height: u32,
}

impl Readback {
    /// Samples the pixel at NDC `(x, y)` (x right, y up). Returns the RGBA bytes.
    fn at_ndc(&self, x: f32, y: f32) -> [u8; 4] {
        // NDC → texel. Texture row 0 is the TOP of the framebuffer, so flip y.
        let px = (((x + 1.0) * 0.5) * (self.width as f32 - 1.0)).round() as i32;
        let py = ((1.0 - (y + 1.0) * 0.5) * (self.height as f32 - 1.0)).round() as i32;
        let px = px.clamp(0, self.width as i32 - 1) as u32;
        let py = py.clamp(0, self.height as i32 - 1) as u32;
        self.pixels[(py * self.width + px) as usize]
    }
}

/// Brings up a headless wgpu device (no surface) with a panicking error hook.
///
/// On macOS the real Metal adapter is always present. On a GPU-less CI runner
/// (Windows) set `VIEWMUSIC_FORCE_FALLBACK_ADAPTER=1` to request the software
/// fallback adapter (WARP under DX12), which `Backends::PRIMARY` already reaches.
fn make_device() -> (wgpu::Device, wgpu::Queue) {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::METAL | wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(desc);

    let force_fallback = std::env::var("VIEWMUSIC_FORCE_FALLBACK_ADAPTER").as_deref() == Ok("1");
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: force_fallback,
        compatible_surface: None,
    }))
    .expect("no GPU adapter for headless test");

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("gpu-smoke-device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        ..Default::default()
    }))
    .expect("failed to acquire wgpu device");

    // Any uncaptured validation error must fail the test loudly.
    device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
        panic!("wgpu validation error: {e}");
    }));

    (device, queue)
}

/// Creates an offscreen RGBA8 render target (sampleable not needed; COPY_SRC for readback).
fn make_target(device: &wgpu::Device) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gpu-smoke-target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

/// Copies `texture` (W×H RGBA8) to a mapped buffer and returns the pixels.
fn read_texture(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture) -> Readback {
    // bytes_per_row must be a multiple of 256.
    let unpadded = W * 4;
    let align = 256;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gpu-smoke-readback"),
        size: (padded * H) as wgpu::BufferAddress,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gpu-smoke-copy"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));

    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        tx.send(r).ok();
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll failed");
    rx.recv().expect("map channel").expect("map failed");

    let mut pixels = Vec::with_capacity((W * H) as usize);
    {
        let view = buffer.slice(..).get_mapped_range();
        for row in 0..H {
            let base = (row * padded) as usize;
            for col in 0..W {
                let off = base + (col * 4) as usize;
                pixels.push([view[off], view[off + 1], view[off + 2], view[off + 3]]);
            }
        }
    }
    buffer.unmap();

    Readback {
        pixels,
        width: W,
        height: H,
    }
}

/// Renders one scene to a fresh offscreen target and reads it back.
fn render_scene(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &mut Renderer,
    params: &SceneParams,
    layers: &[LayerDraw],
) -> Readback {
    let (texture, view) = make_target(device);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gpu-smoke-frame"),
    });
    renderer.render(device, queue, &mut encoder, &view, params, layers);
    queue.submit(std::iter::once(encoder.finish()));
    read_texture(device, queue, &texture)
}

/// Clear color the renderer uses (dark). Approximate bytes for comparison.
fn is_clearish(p: [u8; 4]) -> bool {
    p[0] < 24 && p[1] < 24 && p[2] < 24
}

#[test]
fn instanced_rect_draws_and_clears_elsewhere() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    // One opaque red rect centered, covering the middle.
    let inst = [InstanceData {
        x: 0.0,
        y: 0.0,
        w: 0.6,
        h: 0.6,
        rot: 0.0,
        color: [1.0, 0.0, 0.0, 1.0],
    }];
    let layers = [LayerDraw::Instanced {
        shape: ShapeKind::Rect,
        blend: BlendMode::Alpha,
        instances: &inst,
    }];
    let rb = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &layers,
    );

    // (b) center is drawn (red), corner is the clear color.
    let center = rb.at_ndc(0.0, 0.0);
    assert!(center[0] > 200, "center should be red, got {center:?}");
    assert!(!is_clearish(center), "center must differ from clear");
    let corner = rb.at_ndc(-0.95, 0.95);
    assert!(
        is_clearish(corner),
        "corner should be clear, got {corner:?}"
    );
}

#[test]
fn instanced_circle_masks_corners() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    let inst = [InstanceData {
        x: 0.0,
        y: 0.0,
        w: 1.2,
        h: 1.2,
        rot: 0.0,
        color: [0.0, 1.0, 0.0, 1.0],
    }];
    let layers = [LayerDraw::Instanced {
        shape: ShapeKind::Circle,
        blend: BlendMode::Alpha,
        instances: &inst,
    }];
    let rb = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &layers,
    );

    // Center inside the inscribed circle is green; the quad's corner is masked → clear.
    let center = rb.at_ndc(0.0, 0.0);
    assert!(
        center[1] > 200,
        "circle center should be green, got {center:?}"
    );
    // A point near the quad corner (well outside radius 0.5) must be discarded.
    let corner = rb.at_ndc(0.55, 0.55);
    assert!(
        is_clearish(corner),
        "circle corner should be masked, got {corner:?}"
    );
}

#[test]
fn instanced_triangle_masks_top_corners() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    let inst = [InstanceData {
        x: 0.0,
        y: 0.0,
        w: 1.5,
        h: 1.5,
        rot: 0.0,
        color: [0.2, 0.4, 1.0, 1.0],
    }];
    let layers = [LayerDraw::Instanced {
        shape: ShapeKind::Triangle,
        blend: BlendMode::Alpha,
        instances: &inst,
    }];
    let rb = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &layers,
    );

    // Bottom-center is inside the triangle; the top corners are outside the slanted edges.
    let bottom = rb.at_ndc(0.0, -0.5);
    assert!(
        bottom[2] > 200,
        "triangle bottom should be drawn, got {bottom:?}"
    );
    let top_left = rb.at_ndc(-0.6, 0.6);
    assert!(
        is_clearish(top_left),
        "triangle top-left corner should be masked, got {top_left:?}"
    );
}

#[test]
fn additive_blend_brightens_overlap() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    // Two half-bright red rects fully overlapping with additive blend → should sum brighter
    // than a single one.
    let dim = [0.4f32, 0.0, 0.0, 1.0];
    let one = [InstanceData {
        x: 0.0,
        y: 0.0,
        w: 0.6,
        h: 0.6,
        rot: 0.0,
        color: dim,
    }];
    let two = [
        InstanceData {
            x: 0.0,
            y: 0.0,
            w: 0.6,
            h: 0.6,
            rot: 0.0,
            color: dim,
        },
        InstanceData {
            x: 0.0,
            y: 0.0,
            w: 0.6,
            h: 0.6,
            rot: 0.0,
            color: dim,
        },
    ];

    let rb_one = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &[LayerDraw::Instanced {
            shape: ShapeKind::Rect,
            blend: BlendMode::Add,
            instances: &one,
        }],
    );
    let rb_two = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &[LayerDraw::Instanced {
            shape: ShapeKind::Rect,
            blend: BlendMode::Add,
            instances: &two,
        }],
    );

    let r1 = rb_one.at_ndc(0.0, 0.0)[0];
    let r2 = rb_two.at_ndc(0.0, 0.0)[0];
    assert!(
        r2 > r1 + 20,
        "additive overlap must brighten: one={r1}, two={r2}"
    );
}

#[test]
fn polyline_draws_a_visible_line() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    // Horizontal thick line across the middle.
    let pts = [
        PointData {
            x: -0.8,
            y: 0.0,
            color: [1.0, 1.0, 1.0, 1.0],
        },
        PointData {
            x: 0.8,
            y: 0.0,
            color: [1.0, 1.0, 1.0, 1.0],
        },
    ];
    let layers = [LayerDraw::Polyline {
        blend: BlendMode::Alpha,
        thickness_px: 16.0,
        closed: false,
        points: &pts,
    }];
    let rb = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &layers,
    );

    // On the line: drawn (white). Far above the line: clear.
    let on_line = rb.at_ndc(0.0, 0.0);
    assert!(
        on_line[0] > 200 && on_line[1] > 200,
        "line should be white, got {on_line:?}"
    );
    let off_line = rb.at_ndc(0.0, 0.8);
    assert!(
        is_clearish(off_line),
        "above the line should be clear, got {off_line:?}"
    );
}

#[test]
fn field_fills_cells_with_color() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    // 2x2 grid: bottom-left red, bottom-right green, top-left blue, top-right white.
    // Row-major from bottom-left: i=0 BL, i=1 BR, i=2 TL, i=3 TR.
    let cells = [
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 1.0],
    ];
    let layers = [LayerDraw::Field {
        blend: BlendMode::Alpha,
        resolution: 2,
        cells: &cells,
    }];
    let rb = render_scene(
        &device,
        &queue,
        &mut renderer,
        &SceneParams::default(),
        &layers,
    );

    // Sample the center of each quadrant (NDC). Bottom-left should be red.
    let bl = rb.at_ndc(-0.5, -0.5);
    assert!(
        bl[0] > 200 && bl[1] < 60,
        "bottom-left should be red, got {bl:?}"
    );
    let br = rb.at_ndc(0.5, -0.5);
    assert!(
        br[1] > 200 && br[0] < 60,
        "bottom-right should be green, got {br:?}"
    );
    let tl = rb.at_ndc(-0.5, 0.5);
    assert!(
        tl[2] > 200 && tl[0] < 60,
        "top-left should be blue, got {tl:?}"
    );
}

#[test]
fn feedback_path_runs_and_draws() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);

    // Draw a rect with feedback active over two frames; the surface must show the geometry,
    // and no validation error must fire (the panicking hook guards that).
    let inst = [InstanceData {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 0.5,
        rot: 0.0,
        color: [1.0, 0.5, 0.0, 1.0],
    }];
    let layers = [LayerDraw::Instanced {
        shape: ShapeKind::Rect,
        blend: BlendMode::Alpha,
        instances: &inst,
    }];
    let params = SceneParams {
        feedback_decay: Some(0.9),
    };

    // Two frames so the ping-pong + decay path is exercised, plus the surface blit.
    let (texture, view) = make_target(&device);
    for _ in 0..2 {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpu-smoke-fb-frame"),
        });
        renderer.render(&device, &queue, &mut encoder, &view, &params, &layers);
        queue.submit(std::iter::once(encoder.finish()));
    }
    let rb = read_texture(&device, &queue, &texture);

    // Center should show the (accumulated) orange geometry, not clear.
    let center = rb.at_ndc(0.0, 0.0);
    assert!(
        !is_clearish(center),
        "feedback center must be drawn, got {center:?}"
    );
    assert!(
        center[0] > 100,
        "feedback center should be orange-ish, got {center:?}"
    );
}

#[test]
fn resize_is_glitch_free_clear() {
    let (device, queue) = make_device();
    let mut renderer = Renderer::new(&device, &queue, FORMAT);
    renderer.resize(&device, W, H);
    // Resize again (reallocates feedback targets) then render with feedback: must not error
    // and the empty scene must read back as the clear color (no garbage flash).
    renderer.resize(&device, W, H + 64); // change height to force reallocation
    renderer.resize(&device, W, H); // back to the readback size

    let params = SceneParams {
        feedback_decay: Some(0.9),
    };
    let rb = render_scene(&device, &queue, &mut renderer, &params, &[]);
    let center = rb.at_ndc(0.0, 0.0);
    assert!(
        is_clearish(center),
        "empty feedback scene after resize must be clear, got {center:?}"
    );
}
