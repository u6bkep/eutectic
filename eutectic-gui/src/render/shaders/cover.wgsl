// Coverage pass (renderer-spec §3/§4): geometry renders COLORLESS into the
// shared coverage target. R = base coverage, G = state-flagged coverage
// (the vertex stage fetches the primitive's state word from the semantic
// buffer; G ≤ R by construction). Blending is max() so overlapping
// same-plane primitives (trace end over pad, capsule joints) saturate
// instead of double-blending — what makes translucent planes composite
// correctly.
//
// Analytic instances: one bounding quad per capsule / disc / arc-stroke;
// the fragment shader evaluates signed distance in *pixel* space and writes
// exact coverage (resolution-independent AA). Polygon meshes write flat
// coverage 1; their edge AA comes from the target's MSAA.

struct Frame {
    origin_px: vec2f,     // px position of the scene anchor
    scale: f32,           // px per nm
    _p0: f32,
    viewport: vec2f,
    cursor_px: vec2f,
    grid: vec4f,          // pitch_px, dot_r_minor_px, dot_r_major_px, unused
    grid_offset: vec2f,   // phase of the 10x-pitch lattice, px
    origin_marker: vec2f, // px of the board origin
    flags: u32,           // bit0 cursor valid, bit1 x-axis, bit2 y-axis
    _p1: u32,
    _p2: u32,
    _p3: u32,
    bg: vec4f,
    dot: vec4f,
    dot_major: vec4f,
    axis: vec4f,
    crosshair: vec4f,
    dash: array<vec4f, 4>, // per pattern: on_px, period_px, 0, 0
}

@group(0) @binding(0) var<uniform> F: Frame;
@group(0) @binding(1) var<storage, read> state: array<u32>;

const AA_MARGIN: f32 = 1.5;
const TAU: f32 = 6.28318530717958647692;

// Anchor-relative nm (y up) -> pane px (y down).
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

// ---------------------------------------------------------------------------
// Analytic instances.
// ---------------------------------------------------------------------------

struct InstIn {
    @builtin(vertex_index) vi: u32,
    @location(0) a: vec2f,      // anchor-relative nm
    @location(1) b: vec2f,      // nm, or (a0, a1) radians for arcs
    @location(2) params: vec4f, // r_nm, hw_nm, len0_nm, unused
    @location(3) ks: u32,       // bits 0..7 kind, 8..15 style (0=fill, 1+n=dash n)
    @location(4) sem: u32,
}

struct InstOut {
    @builtin(position) pos: vec4f,
    @location(0) px: vec2f,
    @location(1) @interpolate(flat) pa: vec2f,
    @location(2) @interpolate(flat) pb: vec2f,   // px, or (a0, a1) for arcs
    @location(3) @interpolate(flat) pr: vec4f,   // r_px, hw_px, len0_px, unused
    @location(4) @interpolate(flat) ks: u32,
    @location(5) @interpolate(flat) flag: f32,
}

@vertex
fn vs_inst(in: InstIn) -> InstOut {
    let kind = in.ks & 0xffu;
    let pa = nm_to_px(in.a);
    var pb = pa;
    var lo: vec2f;
    var hi: vec2f;
    if kind == 2u { // arc-stroke: bound by center ± (R + hw)
        let ext = (in.params.x + in.params.y) * F.scale + AA_MARGIN;
        lo = pa - vec2f(ext);
        hi = pa + vec2f(ext);
        pb = in.b; // angles pass through untransformed
    } else { // capsule / disc: bound by endpoints ± r
        pb = nm_to_px(in.b);
        let ext = in.params.x * F.scale + AA_MARGIN;
        lo = min(pa, pb) - vec2f(ext);
        hi = max(pa, pb) + vec2f(ext);
    }
    let corner = vec2f(
        select(lo.x, hi.x, (in.vi & 1u) == 1u),
        select(lo.y, hi.y, in.vi >= 2u),
    );
    var out: InstOut;
    out.pos = ndc(corner);
    out.px = corner;
    out.pa = pa;
    out.pb = pb;
    out.pr = vec4f(
        in.params.x * F.scale,
        in.params.y * F.scale,
        in.params.z * F.scale,
        0.0,
    );
    out.ks = in.ks;
    out.flag = state_flag(in.sem);
    return out;
}

@fragment
fn fs_inst(in: InstOut) -> @location(0) vec4f {
    let kind = in.ks & 0xffu;
    var d: f32;
    var along = -1.0; // < 0: primitive has no dash axis
    if kind == 0u { // capsule: distance to segment, minus radius
        let ba = in.pb - in.pa;
        let l2 = dot(ba, ba);
        var t = 0.0;
        if l2 > 0.0 {
            t = clamp(dot(in.px - in.pa, ba) / l2, 0.0, 1.0);
        }
        d = length(in.px - in.pa - ba * t) - in.pr.x;
        along = in.pr.z + t * sqrt(l2);
    } else if kind == 1u { // disc
        d = length(in.px - in.pa) - in.pr.x;
    } else { // arc-stroke, evaluated in the y-up frame
        let r_mid = in.pr.x;
        let hw = in.pr.y;
        let p = (in.px - in.pa) * vec2f(1.0, -1.0);
        let a0 = in.pb.x;
        let sweep = in.pb.y - in.pb.x; // signed; CCW positive
        let ang = atan2(p.y, p.x);
        var u = (ang - a0) / TAU;
        u = (u - floor(u)) * TAU; // wrap into [0, TAU)
        var inside = false;
        var t = 0.0; // arc-length parameter from the start, in angle units
        if sweep >= 0.0 {
            inside = u <= sweep;
            t = u;
        } else {
            let v = u - TAU; // wrap into (-TAU, 0]
            inside = v >= sweep;
            t = -v;
        }
        if inside {
            d = abs(length(p) - r_mid) - hw;
            along = in.pr.z + t * r_mid;
        } else { // round caps at the endpoints
            let a1 = in.pb.y;
            let e0 = vec2f(cos(a0), sin(a0)) * r_mid;
            let e1 = vec2f(cos(a1), sin(a1)) * r_mid;
            d = min(length(p - e0), length(p - e1)) - hw;
            along = in.pr.z;
        }
    }
    var cov = clamp(0.5 - d, 0.0, 1.0);
    // Procedural dash from the accumulated along-axis parameter: the
    // pattern flows continuously through corners because len0 carries the
    // path length at the primitive's start.
    let style = (in.ks >> 8u) & 0xffu;
    if style > 0u && along >= 0.0 {
        let pat = F.dash[min(style - 1u, 3u)];
        let on_px = pat.x;
        let period = pat.y;
        if period > 0.0 {
            var ph = along / period;
            ph = (ph - floor(ph)) * period;
            var dd: f32;
            if ph < on_px {
                dd = min(ph, on_px - ph);
            } else {
                dd = -min(ph - on_px, period - ph);
            }
            cov = cov * clamp(dd + 0.5, 0.0, 1.0);
        }
    }
    return vec4f(cov, cov * in.flag, 0.0, 0.0);
}

// ---------------------------------------------------------------------------
// Polygon meshes (tessellated interiors).
// ---------------------------------------------------------------------------

struct MeshIn {
    @location(0) pos: vec2f, // anchor-relative nm
    @location(1) sem: u32,
}

struct MeshOut {
    @builtin(position) pos: vec4f,
    @location(0) @interpolate(flat) flag: f32,
}

@vertex
fn vs_mesh(in: MeshIn) -> MeshOut {
    var out: MeshOut;
    out.pos = ndc(nm_to_px(in.pos));
    out.flag = state_flag(in.sem);
    return out;
}

@fragment
fn fs_mesh(in: MeshOut) -> @location(0) vec4f {
    return vec4f(1.0, in.flag, 0.0, 0.0);
}
