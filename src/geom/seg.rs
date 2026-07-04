//! The exact-integer point/segment primitives shared by the shape vocabulary
//! ([`shape`](super::shape)) and the boolean/offset kernel ([`kernel`](super::kernel)).
//!
//! Wave-2 kept value-identical copies of these in both modules (they differed only in
//! `debug_assert` flavour and in whether the cross product was spelled via `orient` or
//! `cross`). Both spellings compute the same `i128` quantities on the same integer
//! domain with the same boundary/inclusivity semantics, so this module holds the single
//! canonical copy; the two consumers import it and keep only the helpers they use
//! elsewhere (convex hull, point-in-polygon, winding order).

use super::limits::point_kernel_safe;
use crate::coord::Point;

/// `(a→b) × (a→p)`: twice the signed area of triangle (a, b, p). Sign = orientation
/// (+ CCW, − CW, 0 collinear). Exact `i128`.
fn cross(a: Point, b: Point, p: Point) -> i128 {
    (b.x - a.x) as i128 * (p.y - a.y) as i128 - (b.y - a.y) as i128 * (p.x - a.x) as i128
}

/// Is collinear point `p` within segment a–b's bounding box (⇒ on the segment)?
fn on_seg_bbox(a: Point, b: Point, p: Point) -> bool {
    p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
}

/// Exact squared distance from point `p` to segment `a`–`b`, as `(num, den)` with
/// `dist² = num/den` and `den > 0`. A degenerate segment (`a == b`) yields the
/// point-to-point distance.
pub(super) fn pt_seg_d2(p: Point, a: Point, b: Point) -> (i128, i128) {
    // The worst i128 chain in the kernel (`|w|²·den ≤ 64·C⁴`); [`KERNEL_SAFE_COORD`] is
    // its true ceiling. Assert against that (not the tighter ingest [`MAX_COORD`]) so
    // legal composition — a placement + courtyard within the headroom — never panics.
    debug_assert!(
        point_kernel_safe(p) && point_kernel_safe(a) && point_kernel_safe(b),
        "pt_seg_d2 coordinate exceeds KERNEL_SAFE_COORD; the i128 product may overflow (issue 0018)"
    );
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy;
    if den == 0 {
        return (wx * wx + wy * wy, 1); // degenerate segment = point a
    }
    let t = wx * vx + wy * vy;
    if t <= 0 {
        (wx * wx + wy * wy, 1)
    } else if t >= den {
        let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
        (ux * ux + uy * uy, 1)
    } else {
        // Perpendicular: |w|² − t²/den = (|w|²·den − t²)/den.
        let ww = wx * wx + wy * wy;
        (ww * den - t * t, den)
    }
}

/// Do segments `a1a2` and `b1b2` intersect (touching / collinear-overlap counts)?
pub(super) fn segs_intersect(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = cross(b1, b2, a1).signum();
    let d2 = cross(b1, b2, a2).signum();
    let d3 = cross(a1, a2, b1).signum();
    let d4 = cross(a1, a2, b2).signum();
    if d1 != d2 && d3 != d4 {
        return true;
    }
    (d1 == 0 && on_seg_bbox(b1, b2, a1))
        || (d2 == 0 && on_seg_bbox(b1, b2, a2))
        || (d3 == 0 && on_seg_bbox(a1, a2, b1))
        || (d4 == 0 && on_seg_bbox(a1, a2, b2))
}
