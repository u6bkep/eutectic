// Composite pass (renderer-spec §4): lay one plane's resolved coverage into
// the pane texture, back-to-front, with per-plane uniforms from the style
// tables. color = mix(plane, emphasis, G/R); alpha = R × plane_alpha × dim.
// The G/R division is guarded at R ≈ 0 (min representable coverage is
// 1/255; below that the emphasis mix is meaningless and clamps to 0).
// Output is premultiplied; the pipeline blends src + dst·(1−src.a).

struct PlaneU {
    color: vec4f,    // rgb + the color's own alpha (straight)
    emphasis: vec4f,
    params: vec4f,   // x = plane alpha, y = dim, z/w unused
}

@group(0) @binding(0) var<uniform> P: PlaneU;
@group(0) @binding(1) var cov: texture_2d<f32>;

struct FsIn {
    @builtin(position) pos: vec4f,
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4f {
    // One clipping triangle covering the viewport.
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    return vec4f(x, y, 0.0, 1.0);
}

@fragment
fn fs_composite(in: FsIn) -> @location(0) vec4f {
    let c = textureLoad(cov, vec2i(in.pos.xy), 0);
    let r = c.r;
    let mixf = clamp(c.g / max(r, 0.004), 0.0, 1.0); // guard R ~ 0
    let col = mix(P.color.rgb, P.emphasis.rgb, mixf);
    let a = r * P.color.a * P.params.x * P.params.y;
    return vec4f(col * a, a);
}
