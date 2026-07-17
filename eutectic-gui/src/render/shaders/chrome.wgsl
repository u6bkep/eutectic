// Canvas furniture (renderer-spec §4): the procedural dot grid and the
// crosshair cursor, evaluated from camera uniforms alone — zero CPU, zero
// geometry. All math is in screen px; the CPU reduced the grid phase mod
// the 10x lattice in f64, so no board-scale magnitude ever reaches f32.
//
// `fs_background` runs first (over the cleared background color): dot grid
// (1-2-5 pitch ladder keyed to zoom, one emphasis tier at 10x, origin
// marker + axes). `fs_crosshair` runs last, over the composited planes.
// Both output premultiplied alpha.

struct Frame {
    origin_px: vec2f,
    scale: f32,
    _p0: f32,
    viewport: vec2f,
    cursor_px: vec2f,
    grid: vec4f,          // pitch_px, dot_r_minor_px, dot_r_major_px, unused
    grid_offset: vec2f,   // phase of the 10x-pitch lattice, px
    origin_marker: vec2f, // px of the board origin
    flags: u32,           // bit0 cursor, bit1 x-axis, bit2 y-axis, bit3 grid lines
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

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4f {
    let x = f32(i32(vi & 1u) * 4 - 1);
    let y = f32(i32(vi >> 1u) * 4 - 1);
    return vec4f(x, y, 0.0, 1.0);
}

fn premul(c: vec4f) -> vec4f {
    return vec4f(c.rgb * c.a, c.a);
}

// src OVER dst, both premultiplied.
fn over(src: vec4f, dst: vec4f) -> vec4f {
    return src + dst * (1.0 - src.a);
}

@fragment
fn fs_background(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    let p = pos.xy;
    var acc = vec4f(0.0);
    let pitch = F.grid.x;
    if pitch >= 2.0 {
        let d = p - F.grid_offset;
        // Distance to the nearest minor / major (10x) lattice point/line.
        let c1 = (fract(d / pitch + vec2f(0.5)) - vec2f(0.5)) * pitch;
        let p10 = pitch * 10.0;
        let c10 = (fract(d / p10 + vec2f(0.5)) - vec2f(0.5)) * p10;
        var major: f32;
        var minor: f32;
        if (F.flags & 8u) != 0u {
            major = clamp(0.75 - min(abs(c10.x), abs(c10.y)), 0.0, 1.0);
            minor = clamp(0.6 - min(abs(c1.x), abs(c1.y)), 0.0, 1.0) * (1.0 - major);
        } else {
            major = clamp(F.grid.z + 0.5 - length(c10), 0.0, 1.0);
            minor = clamp(F.grid.y + 0.5 - length(c1), 0.0, 1.0) * (1.0 - major);
        }
        acc = premul(F.dot_major) * major + premul(F.dot) * minor;
    }
    // Origin axes (hairlines through board (0,0)) + a small origin ring.
    if (F.flags & 2u) != 0u {
        let v = clamp(0.75 - abs(p.x - F.origin_marker.x), 0.0, 1.0);
        acc = over(premul(F.axis) * v, acc);
    }
    if (F.flags & 4u) != 0u {
        let v = clamp(0.75 - abs(p.y - F.origin_marker.y), 0.0, 1.0);
        acc = over(premul(F.axis) * v, acc);
    }
    if (F.flags & 6u) == 6u {
        let dr = abs(length(p - F.origin_marker) - 6.0) - 1.0;
        acc = over(premul(F.axis) * clamp(0.5 - dr, 0.0, 1.0), acc);
    }
    return acc;
}

@fragment
fn fs_crosshair(@builtin(position) pos: vec4f) -> @location(0) vec4f {
    if (F.flags & 1u) == 0u {
        return vec4f(0.0);
    }
    let p = pos.xy;
    let vx = clamp(0.75 - abs(p.x - F.cursor_px.x), 0.0, 1.0);
    let vy = clamp(0.75 - abs(p.y - F.cursor_px.y), 0.0, 1.0);
    return premul(F.crosshair) * max(vx, vy);
}
