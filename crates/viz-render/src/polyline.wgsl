// polyline: expand each segment instance into a thick quad in NDC.
//
// The template quad has x in [0,1] along the segment and y in [-0.5, 0.5] across it.
// The segment direction is computed in NDC but the perpendicular offset uses a
// per-axis half-thickness already expressed in NDC (half_ndc) so the visual width is
// constant in pixels regardless of aspect. Endpoint colors interpolate along x.

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) tmpl: vec2<f32>,     // x in [0,1] along segment, y in [-0.5,0.5] across
    @location(1) p0: vec2<f32>,       // segment start (NDC)
    @location(2) p1: vec2<f32>,       // segment end (NDC)
    @location(3) half_ndc: vec4<f32>, // .xy half-thickness in NDC; .zw padding
    @location(4) c0: vec4<f32>,       // start color
    @location(5) c1: vec4<f32>,       // end color
) -> VsOut {
    // Point along the segment.
    let along = mix(p0, p1, tmpl.x);

    // Segment direction; guard zero-length segments.
    let d = p1 - p0;
    let len = max(length(d), 1e-6);
    let dir = d / len;
    // Perpendicular (rotate dir by 90 degrees).
    let perp = vec2<f32>(-dir.y, dir.x);

    // Offset across the segment, scaled per-axis by the NDC half-thickness so width is
    // constant in pixels. tmpl.y is in [-0.5, 0.5]; *2 turns it into [-1, 1].
    let offset = perp * half_ndc.xy * (tmpl.y * 2.0);

    let pos = along + offset;

    var out: VsOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.color = mix(c0, c1, tmpl.x);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Premultiply straight-alpha for the blend states.
    return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
}
