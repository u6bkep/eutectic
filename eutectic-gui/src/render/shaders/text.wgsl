// MSDF annotation-text coverage (renderer-spec §6, WP3): glyph quads render
// COLORLESS into the shared coverage target, exactly like the analytic
// primitives in cover.wgsl — R = coverage, G = state-flagged coverage,
// max-blended — so the composite pass gives text its plane's color / alpha /
// dim / emphasis mix for free.
//
// The distance-field sampling is damascene's `stock::text_msdf` recipe
// (crates/damascene-core/shaders/text_msdf.wgsl at the pinned rev): the
// atlas stores RGB = 3-channel MSDF and A = a true single-channel SDF; the
// median of RGB reconstructs the signed distance, and A wins wherever they
// disagree about inside/outside (the classic sharp-corner MSDF artifact).
// AA width derives from the screen-space UV gradient, so edges stay ~one
// screen pixel at every zoom — that is MSDF's point: one raster per glyph,
// crisp at all camera scales, no per-zoom rebuilds. The 2×2 rotated-grid
// supersample recovers small-size quality the same way damascene's shader
// does. The color/gamma remapping of the original is deliberately absent:
// this pass emits coverage, not color.

struct Frame {
    origin_px: vec2f,
    scale: f32,
    _p0: f32,
    viewport: vec2f,
    cursor_px: vec2f,
    grid: vec4f,
    grid_offset: vec2f,
    origin_marker: vec2f,
    flags: u32,
    _p1: u32,
    _p2: u32,
    _p3: u32,
    bg: vec4f,
    dot: vec4f,
    dot_major: vec4f,
    axis: vec4f,
    crosshair: vec4f,
    dash: array<vec4f, 4>,
}

@group(0) @binding(0) var<uniform> F: Frame;
@group(0) @binding(1) var<storage, read> state: array<u32>;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_smp: sampler;

// Anchor-relative nm (y up) -> pane px (y down) — cover.wgsl's transform.
fn nm_to_px(q: vec2f) -> vec2f {
    return F.origin_px + vec2f(q.x, -q.y) * F.scale;
}

fn ndc(px: vec2f) -> vec4f {
    let n = (px / F.viewport * 2.0 - vec2f(1.0)) * vec2f(1.0, -1.0);
    return vec4f(n, 0.0, 1.0);
}

fn state_flag(sem: u32) -> f32 {
    if sem < arrayLength(&state) && state[sem] != 0u {
        return 1.0;
    }
    return 0.0;
}

struct TextIn {
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4f, // x_left, y_top (anchor-relative nm, y-up), w, h
    @location(1) uv: vec4f,   // atlas-page uv rect
    @location(2) sem: u32,
    @location(3) spread: f32, // MSDF spread, atlas px
}

struct TextOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) @interpolate(flat) spread: f32,
    @location(2) @interpolate(flat) flag: f32,
}

@vertex
fn vs_text(in: TextIn) -> TextOut {
    let u = f32(in.vi & 1u);
    let v = f32(in.vi >> 1u);
    // y-up: the quad's top edge is rect.y; v grows downward (−y).
    let q = vec2f(in.rect.x + u * in.rect.z, in.rect.y - v * in.rect.w);
    var out: TextOut;
    out.pos = ndc(nm_to_px(q));
    out.uv = in.uv.xy + vec2f(u, v) * in.uv.zw;
    out.spread = in.spread;
    out.flag = state_flag(in.sem);
    return out;
}

fn median3(a: f32, b: f32, c: f32) -> f32 {
    return max(min(a, b), min(max(a, b), c));
}

fn coverage_at(uv: vec2f, screen_px_range: f32) -> f32 {
    let mtsd = textureSample(atlas_tex, atlas_smp, uv);
    let median_sd = median3(mtsd.r, mtsd.g, mtsd.b);
    let true_sd = mtsd.a;
    // MSDF lies near sharp corners; the true SDF wins on disagreement.
    let agree = (median_sd - 0.5) * (true_sd - 0.5) >= 0.0;
    let sd = select(true_sd, median_sd, agree) - 0.5;
    return clamp(sd * 2.0 * screen_px_range + 0.5, 0.0, 1.0);
}

@fragment
fn fs_text(in: TextOut) -> @location(0) vec4f {
    let spread = max(in.spread, 0.001);
    let atlas_size = vec2f(textureDimensions(atlas_tex));
    let unit_range = vec2f(spread, spread) / atlas_size;
    let dx = dpdx(in.uv);
    let dy = dpdy(in.uv);
    let screen_per_uv = vec2f(1.0) / fwidth(in.uv);
    let screen_px_range = max(0.5 * dot(unit_range, screen_per_uv), 1.0);

    // 2×2 rotated-grid supersample (see damascene's shader for rationale).
    let off1 = 0.125 * dx + 0.375 * dy;
    let off2 = 0.375 * dx - 0.125 * dy;
    let off3 = -0.375 * dx + 0.125 * dy;
    let off4 = -0.125 * dx - 0.375 * dy;
    let cov = 0.25 * (
        coverage_at(in.uv + off1, screen_px_range)
      + coverage_at(in.uv + off2, screen_px_range)
      + coverage_at(in.uv + off3, screen_px_range)
      + coverage_at(in.uv + off4, screen_px_range)
    );
    return vec4f(cov, cov * in.flag, 0.0, 0.0);
}
