// instanced shapes: one vertex shader, three fragment masks (rect/circle/triangle).
//
// Coordinate contract: artifact space maps directly to NDC (x right, y up, -1..1).
// The unit quad arrives in local space [-0.5, 0.5]^2; we scale by (w,h), rotate by
// `rot` (radians, CCW), then translate to (x,y). Local coords are forwarded so the
// fragment stage can mask circle / triangle. Output color is premultiplied so the
// alpha-over and additive blend states (set on the pipeline) both behave.

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    // Local position within the quad, in [-0.5, 0.5]^2.
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) quad: vec2<f32>,        // unit-quad vertex, [-0.5, 0.5]^2
    @location(1) xywh: vec4<f32>,        // x, y, w, h
    @location(2) rot: f32,               // radians, CCW
    @location(3) color: vec4<f32>,       // straight-alpha RGBA, 0..1
) -> VsOut {
    let center = xywh.xy;
    let size = xywh.zw;
    // Scale into NDC half-extents.
    let scaled = quad * size;
    // Rotate CCW about the center.
    let c = cos(rot);
    let s = sin(rot);
    let rotated = vec2<f32>(
        scaled.x * c - scaled.y * s,
        scaled.x * s + scaled.y * c,
    );
    let pos = center + rotated;

    var out: VsOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.local = quad;
    out.color = color;
    return out;
}

// Premultiply straight-alpha color for blending.
fn premul(c: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(c.rgb * c.a, c.a);
}

@fragment
fn fs_rect(in: VsOut) -> @location(0) vec4<f32> {
    // Whole quad is filled.
    return premul(in.color);
}

@fragment
fn fs_circle(in: VsOut) -> @location(0) vec4<f32> {
    // Inscribed circle: discard fragments outside radius 0.5 of the local quad.
    if (dot(in.local, in.local) > 0.25) {
        discard;
    }
    return premul(in.color);
}

@fragment
fn fs_triangle(in: VsOut) -> @location(0) vec4<f32> {
    // Upward isoceles triangle inscribed in the quad: apex at local (0, +0.5),
    // base from (-0.5, -0.5) to (+0.5, -0.5). Keep fragments inside the two slanted
    // edges (the base edge is the quad bottom, already inside).
    //
    // Edges (apex A=(0,0.5)):
    //   left  edge A -> (-0.5,-0.5): keep points to its right
    //   right edge A -> ( 0.5,-0.5): keep points to its left
    // Using the line through A with the base corner, the inside test reduces to:
    //   |x| <= 0.5 * (0.5 - y)   for y in [-0.5, 0.5]
    let half_width = 0.5 * (0.5 - in.local.y);
    if (abs(in.local.x) > half_width) {
        discard;
    }
    return premul(in.color);
}
