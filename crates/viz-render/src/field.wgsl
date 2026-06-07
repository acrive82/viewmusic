// field: each cell is a unit quad expanded to its NDC rectangle by per-instance
// center + half-size. Color is a flat per-cell value (premultiplied for blending).

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) quad: vec2<f32>,   // unit quad [-0.5, 0.5]^2
    @location(1) center: vec2<f32>, // cell center (NDC)
    @location(2) half: vec2<f32>,   // cell half-size (NDC)
    @location(3) color: vec4<f32>,  // straight-alpha RGBA
) -> VsOut {
    let pos = center + quad * (half * 2.0);
    var out: VsOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
}
