//! Tests for the region boolean/offset kernel ([`super`]). Extracted verbatim from
//! the former `region.rs` inline `#[cfg(test)] mod tests`.

use super::*;
const MM: Nm = 1_000_000;
fn pt(x: Nm, y: Nm) -> Point {
    Point { x, y }
}
fn square(cx: Nm, cy: Nm, half: Nm) -> Ring {
    vec![
        pt(cx - half, cy - half),
        pt(cx + half, cy - half),
        pt(cx + half, cy + half),
        pt(cx - half, cy + half),
    ]
}
fn area_abs(r: &Region) -> i128 {
    r.area2().abs() / 2
}

#[test]
fn signed_area_orientation() {
    let ccw = square(0, 0, MM);
    assert!(signed_area2(&ccw) > 0);
    let mut cw = ccw.clone();
    cw.reverse();
    assert!(signed_area2(&cw) < 0);
    assert_eq!(
        area_abs(&Region::from_ring(ccw)),
        4 * MM as i128 * MM as i128
    );
}

#[test]
fn contains_with_hole() {
    // Outer 4mm square, CW hole 2mm square ⇒ a frame; center is in the hole.
    let outer = square(0, 0, 2 * MM);
    let mut hole = square(0, 0, MM);
    hole.reverse(); // CW hole
    let frame = Region::new(vec![outer, hole]);
    assert!(!frame.contains_point(pt(0, 0)), "center is in the hole");
    assert!(frame.contains_point(pt(3 * MM / 2, 0)), "in the frame band");
    assert!(!frame.contains_point(pt(3 * MM, 0)), "outside entirely");
}

#[test]
fn union_overlapping_squares_area() {
    // Two unit squares overlapping by a 1x2 strip.
    let a = Region::from_ring(square(0, 0, MM)); // [-1,1]^2, area 4
    let b = Region::from_ring(square(MM, 0, MM)); // [0,2]x[-1,1], area 4
    let u = union(&a, &b);
    // Union spans [-1,2]x[-1,1] = 3x2 = 6 mm^2.
    assert_eq!(area_abs(&u), 6 * MM as i128 * MM as i128, "union area");
    assert!(u.contains_point(pt(15 * MM / 10, 0)));
}

#[test]
fn intersection_overlapping_squares_area() {
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(MM, 0, MM));
    let i = intersection(&a, &b);
    // Overlap [0,1]x[-1,1] = 1x2 = 2 mm^2.
    assert_eq!(
        area_abs(&i),
        2 * MM as i128 * MM as i128,
        "intersection area"
    );
}

#[test]
fn difference_makes_a_hole() {
    // 4mm square minus a centered 2mm square ⇒ frame, area 16-4 = 12 mm^2.
    let a = Region::from_ring(square(0, 0, 2 * MM));
    let b = Region::from_ring(square(0, 0, MM));
    let d = difference(&a, &b);
    assert_eq!(area_abs(&d), 12 * MM as i128 * MM as i128, "frame area");
    assert!(!d.contains_point(pt(0, 0)), "hole punched at center");
    assert!(d.contains_point(pt(15 * MM / 10, 0)), "band remains");
}

#[test]
fn difference_partial_overlap() {
    // a=[-1,1]^2 minus b=[0,2]x[-1,1] ⇒ left half [-1,0]x[-1,1] = 2 mm^2.
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(MM, 0, MM));
    let d = difference(&a, &b);
    assert_eq!(area_abs(&d), 2 * MM as i128 * MM as i128);
    assert!(d.contains_point(pt(-MM / 2, 0)));
    assert!(!d.contains_point(pt(MM / 2, 0)));
}

#[test]
fn difference_identical_is_empty() {
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(0, 0, MM));
    let d = difference(&a, &b);
    assert_eq!(area_abs(&d), 0, "A−A is empty");
}

#[test]
fn union_identical_is_same_area() {
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(0, 0, MM));
    let u = union(&a, &b);
    assert_eq!(area_abs(&u), 4 * MM as i128 * MM as i128, "A∪A = A");
}

#[test]
fn disjoint_union_keeps_both() {
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(10 * MM, 0, MM));
    let u = union(&a, &b);
    assert_eq!(area_abs(&u), 8 * MM as i128 * MM as i128);
    assert!(u.contains_point(pt(0, 0)) && u.contains_point(pt(10 * MM, 0)));
}

#[test]
fn disc_region_area_approximates_pi_r2() {
    // A 64-gon disc of r=1mm: area ≈ π mm^2 = 3.1415e12 nm^2. 64-gon is slightly
    // under: (1/2) n sin(2π/n) r^2 ≈ 0.99949·π.
    let region = shape_to_region(&Shape2D::disc(pt(0, 0), MM), DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64;
    let pi_r2 = std::f64::consts::PI * (MM as f64) * (MM as f64);
    let ratio = a / pi_r2;
    assert!(ratio > 0.995 && ratio <= 1.0, "disc area ratio {ratio}");
}

#[test]
fn rounded_rect_area_matches_geometry() {
    // round_rect(2mm,2mm,r=0.5) is a 2x2 rounded rect = a 1x1 core ⊕ 0.5mm. Its
    // area = core 1 + perimeter 4*0.5 + π*0.5^2 = 1 + 2 + π/4 ≈ 3.785 mm^2 (the
    // four rounded corners replace the four corner squares with quarter-discs).
    let rr = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, MM / 2);
    let region = shape_to_region(&rr, DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
    assert!(
        (a - 3.785).abs() < 0.05,
        "rounded-rect area {a} mm^2 ~ 3.785"
    );
}

#[test]
fn capsule_region_is_stadium() {
    // Pill from (0,0) to (4mm,0) r=1mm: area = rect(4x2) + circle(r1) = 8 + π.
    let cap = Shape2D::capsule(pt(0, 0), pt(4 * MM, 0), MM);
    let region = shape_to_region(&cap, DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
    let expect = 8.0 + std::f64::consts::PI;
    assert!((a - expect).abs() < 0.05, "stadium area {a} ~ {expect}");
    assert!(region.contains_point(pt(2 * MM, 0)));
}

#[test]
fn dilate_grows_islands_and_shrinks_holes() {
    // A 10mm square with a 4mm square hole (walls at ±2mm). Dilating by 1mm grows the
    // outer boundary outward and moves the hole walls inward by 1mm (to ±1mm).
    let outer = shape_to_region(
        &Shape2D::rect(pt(0, 0), 10 * MM, 10 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let hole = shape_to_region(
        &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let holed = difference(&outer, &hole);
    assert_eq!(holed.holes().rings.len(), 1, "one cutout hole");

    let big = dilate(&holed, MM, DEFAULT_CIRCLE_SEGS);
    // Outer boundary grew: a point 0.5mm outside the old edge is now filled.
    assert!(
        big.contains_point(pt(55 * MM / 10, 0)),
        "outer edge grew outward"
    );
    // Hole shrank: a point 1.5mm from centre (was inside the ±2mm hole) is now filled.
    assert!(
        !holed.contains_point(pt(0, 15 * MM / 10)),
        "was in the hole"
    );
    assert!(
        big.contains_point(pt(0, 15 * MM / 10)),
        "hole wall moved inward"
    );
    // d == 0 is identity.
    assert_eq!(dilate(&holed, 0, DEFAULT_CIRCLE_SEGS), holed);
}

#[test]
#[should_panic(expected = "erosion")]
fn dilate_negative_offset_panics() {
    let sq = shape_to_region(
        &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let _ = dilate(&sq, -MM, DEFAULT_CIRCLE_SEGS);
}

#[test]
fn offset_via_radius_bump_inflates() {
    // The pour use case: inflate a pad by clearance = build it with radius+clr.
    // A 1mm disc inflated by 0.5mm clearance ⇒ a 1.5mm disc.
    let inflated = shape_to_region(&Shape2D::disc(pt(0, 0), MM + MM / 2), DEFAULT_CIRCLE_SEGS);
    assert!(inflated.contains_point(pt(14 * MM / 10, 0)), "within 1.5mm");
    assert!(
        !inflated.contains_point(pt(16 * MM / 10, 0)),
        "beyond 1.5mm"
    );
}

#[test]
fn pour_knockout_end_to_end() {
    // The headline: a board-area pour minus the clearance around one pad. Board
    // 10x10 (area 100), pad a 1mm disc at center inflated by 0.5mm clearance
    // (area ≈ π·1.5² ≈ 7.07) ⇒ pour with a hole, area ≈ 92.9 mm^2.
    let board = Region::from_ring(square(0, 0, 5 * MM)); // 10x10
    let knock = shape_to_region(&Shape2D::disc(pt(0, 0), MM + MM / 2), DEFAULT_CIRCLE_SEGS);
    let pour = difference(&board, &knock);
    let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
    assert!((a - 92.93).abs() < 0.1, "pour area {a} ~ 92.93");
    assert!(!pour.contains_point(pt(0, 0)), "pad knocked out");
    assert!(
        pour.contains_point(pt(4 * MM, 4 * MM)),
        "copper in the corner"
    );
}

#[test]
fn union_shared_edge_merges() {
    // Two squares sharing the exact edge x=1mm (A right edge ↑, B left edge ↓ ⇒
    // opposite-direction coincident ⇒ interior, dropped). Merge ⇒ 4x2 = 8 mm^2.
    let a = Region::from_ring(square(0, 0, MM)); // [-1,1]^2
    let b = Region::from_ring(square(2 * MM, 0, MM)); // [1,3]x[-1,1]
    let u = union(&a, &b);
    assert_eq!(
        area_abs(&u),
        8 * MM as i128 * MM as i128,
        "shared-edge union area"
    );
    assert_eq!(u.rings.len(), 1, "merges into one ring, no internal edge");
    assert!(
        u.contains_point(pt(MM, 0)),
        "former shared edge is now interior"
    );
}

#[test]
fn union_corner_touch_keeps_both() {
    // Two squares touching only at the corner (1,1): no shared edge, both kept.
    let a = Region::from_ring(square(0, 0, MM)); // corner at (1,1)
    let b = Region::from_ring(square(2 * MM, 2 * MM, MM)); // corner at (1,1)
    let u = union(&a, &b);
    assert_eq!(area_abs(&u), 8 * MM as i128 * MM as i128);
}

#[test]
fn difference_b_outside_is_a() {
    let a = Region::from_ring(square(0, 0, MM));
    let b = Region::from_ring(square(10 * MM, 0, MM));
    let d = difference(&a, &b);
    assert_eq!(
        area_abs(&d),
        4 * MM as i128 * MM as i128,
        "A−(disjoint B) = A"
    );
}

#[test]
fn difference_b_contains_a_is_empty() {
    let a = Region::from_ring(square(0, 0, MM)); // small
    let b = Region::from_ring(square(0, 0, 3 * MM)); // engulfs A
    let d = difference(&a, &b);
    assert_eq!(area_abs(&d), 0, "A−(B⊇A) is empty");
}

#[test]
fn concave_l_shape_dilation_is_consistent() {
    // An L (concave) polygon, area 3 mm^2, dilated by 0.25mm (radius on the
    // Polygon) via the union decomposition — the reflex vertex must not break the
    // union. Dilation area = core 3 + perimeter*r + π r^2; the L's perimeter
    // (2x2 minus a 1x1 notch) = 8mm.
    let l = Shape2D::polygon_path(
        crate::geom::Path::polyline(vec![
            pt(0, 0),
            pt(2 * MM, 0),
            pt(2 * MM, MM),
            pt(MM, MM),
            pt(MM, 2 * MM),
            pt(0, 2 * MM),
        ]),
        MM / 4,
    );
    let region = shape_to_region(&l, DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
    let expect = 3.0 + 8.0 * 0.25 + std::f64::consts::PI * 0.25 * 0.25;
    assert!((a - expect).abs() < 0.05, "L dilation area {a} ~ {expect}");
    // The reflex notch interior (just outside the core, within r) is now filled.
    assert!(region.contains_point(pt(11 * MM / 10, 11 * MM / 10)));
}

#[test]
fn pour_with_two_knockouts() {
    // Closer to the real pipeline: board minus the union of two inflated pads.
    // Board 10x10 = 100 mm^2; two 1mm discs (at ±2,0) each inflated to 1.5mm
    // (area π·1.5² ≈ 7.07 each, disjoint) ⇒ pour ≈ 100 − 14.14 = 85.86 mm^2.
    let board = Region::from_ring(square(0, 0, 5 * MM));
    let pads = union_all(vec![
        shape_to_region(
            &Shape2D::disc(pt(-2 * MM, 0), MM + MM / 2),
            DEFAULT_CIRCLE_SEGS,
        ),
        shape_to_region(
            &Shape2D::disc(pt(2 * MM, 0), MM + MM / 2),
            DEFAULT_CIRCLE_SEGS,
        ),
    ]);
    let pour = difference(&board, &pads);
    let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
    assert!(
        (a - 85.86).abs() < 0.2,
        "two-knockout pour area {a} ~ 85.86"
    );
    assert!(!pour.contains_point(pt(-2 * MM, 0)) && !pour.contains_point(pt(2 * MM, 0)));
    assert!(
        pour.contains_point(pt(0, 0)),
        "copper survives between the two pads"
    );
}

#[test]
fn arc_stroke_dilates_to_a_curved_tube() {
    // A semicircular trace (skeleton arc, R=2mm) of width 1mm (r=0.5mm). Its copper
    // is the r-tube around the arc: a half annulus from 1.5 to 2.5mm plus the two
    // end-cap half-discs. Area = ½π(2.5²−1.5²) + π·0.5² = 2π + 0.25π ≈ 7.07 mm².
    // This exercises the boolean closing a fan of fat segments + discs along a curve
    // (no cracks) — the integration test for arc input to the kernel. (R is kept
    // small: at 1µm tol a large arc flattens to hundreds of edges and the O(N²)
    // union dominates — a documented perf characteristic, not needed to test here.)
    let r = 2 * MM;
    let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), MM); // width 1mm
    let region = shape_to_region(&arc, DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
    let expect = 2.0 * std::f64::consts::PI + 0.25 * std::f64::consts::PI;
    assert!((a - expect).abs() < 0.1, "arc tube area {a} ~ {expect}");
    // A point on the arc centreline (the apex) is inside the copper; the hollow
    // centre of the semicircle (origin) is not.
    assert!(
        region.contains_point(pt(0, r)),
        "copper covers the arc apex"
    );
    assert!(
        !region.contains_point(pt(0, 0)),
        "the tube is hollow at the centre"
    );
}

#[test]
fn arc_edged_polygon_is_a_filled_half_disc() {
    // A filled half-disc (D-shape): start (-2mm,0), an Arc through the apex (0,2mm)
    // to (2mm,0), closed by the implicit straight diameter. Area = ½πR² = 2π
    // ≈ 6.28 mm². Confirms a Polygon's arc edge tessellates into the core ring and
    // the implicit closing Line seals it.
    let r = 2 * MM;
    let half = Shape2D::polygon_path(
        crate::geom::Path {
            start: pt(-r, 0),
            segs: vec![crate::geom::Seg::Arc {
                mid: pt(0, r),
                end: pt(r, 0),
            }],
        },
        0,
    );
    let region = shape_to_region(&half, DEFAULT_CIRCLE_SEGS);
    let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
    let expect = 2.0 * std::f64::consts::PI;
    assert!((a - expect).abs() < 0.1, "half-disc area {a} ~ {expect}");
    assert!(region.contains_point(pt(0, MM)), "interior point is filled");
    assert!(
        region.contains_point(pt(0, 18 * MM / 10)),
        "point under the apex is filled"
    );
    assert!(
        !region.contains_point(pt(0, -MM / 2)),
        "below the diameter is outside"
    );
    assert!(
        !region.contains_point(pt(18 * MM / 10, 18 * MM / 10)),
        "outside the circle"
    );
}

#[test]
fn nonlattice_diagonal_crossings() {
    // Regression for the snapped-crossing bug: a rect and a triangle whose slanted
    // edges cross the rect's top edge at NON-integer-nm points (x = 1.6̄ mm,
    // 2.3̄ mm), so the crossing rounds to a lattice point on neither integer line.
    // A = 4x2 rect (area 8). B = triangle (1,0),(3,0),(2,3) (area 3); B's tip above
    // y=2 is clipped, so A∩B = 3 − tip(1/3) = 8/3 ≈ 2.667 mm^2; A∪B = 25/3 ≈ 8.333.
    let a = Region::from_ring(vec![
        pt(0, 0),
        pt(4 * MM, 0),
        pt(4 * MM, 2 * MM),
        pt(0, 2 * MM),
    ]);
    let b = Region::from_ring(vec![pt(MM, 0), pt(3 * MM, 0), pt(2 * MM, 3 * MM)]);
    let inter = intersection(&a, &b);
    let uni = union(&a, &b);
    let mm2 = MM as f64 * MM as f64;
    assert!(
        (area_abs(&inter) as f64 / mm2 - 8.0 / 3.0).abs() < 0.001,
        "A∩B = 8/3"
    );
    assert!(
        (area_abs(&uni) as f64 / mm2 - 25.0 / 3.0).abs() < 0.001,
        "A∪B = 25/3"
    );
    // The crossing must split BOTH edges identically (no crack): re-running is
    // byte-identical, and a point just inside the clipped overlap is contained.
    assert_eq!(
        inter,
        intersection(&a, &b),
        "deterministic under non-lattice crossings"
    );
    assert!(inter.contains_point(pt(2 * MM, MM)), "overlap interior");
}

#[test]
fn shared_diagonal_edge_merges() {
    // Regression for diagonal coincident-edge handling: two triangles sharing the
    // diagonal (0,0)-(2,2) in opposite directions merge into the 2x2 square.
    let t1 = Region::from_ring(vec![pt(0, 0), pt(2 * MM, 0), pt(2 * MM, 2 * MM)]);
    let t2 = Region::from_ring(vec![pt(0, 0), pt(2 * MM, 2 * MM), pt(0, 2 * MM)]);
    let u = union(&t1, &t2);
    assert_eq!(
        area_abs(&u),
        4 * MM as i128 * MM as i128,
        "two triangles → square"
    );
    assert_eq!(
        u.rings.len(),
        1,
        "shared diagonal becomes interior, one ring"
    );
    // Intersection of the two triangles (share only the diagonal) has no area.
    assert_eq!(
        area_abs(&intersection(&t1, &t2)),
        0,
        "triangles meet only on the diagonal"
    );
}

#[test]
fn rotated_pad_pour_knockout() {
    // The real-world Bug-1 trigger: a diamond (45°-rotated square) knockout whose
    // edges cross the board boundary at non-lattice points. Board 10x10 (area 100);
    // a diamond of "radius" 3mm centered exactly on the board corner (5,5). Only the
    // one quadrant with x≤5 ∧ y≤5 lies inside the board — a triangle of area
    // (1/2)*3*3 = 4.5 mm^2 (the diamond's full area 2*3^2=18 split four ways). So
    // the pour ≈ 100 − 4.5 = 95.5 mm^2.
    let board = Region::from_ring(square(0, 0, 5 * MM)); // [-5,5]^2
    let d = 3 * MM;
    let diamond = Region::from_ring(vec![
        pt(5 * MM + d, 5 * MM),
        pt(5 * MM, 5 * MM + d),
        pt(5 * MM - d, 5 * MM),
        pt(5 * MM, 5 * MM - d),
    ]);
    let pour = difference(&board, &diamond);
    let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
    assert!(
        (a - 95.5).abs() < 0.01,
        "rotated-diamond pour area {a} ~ 95.5"
    );
    assert!(
        !pour.contains_point(pt(4 * MM, 5 * MM)),
        "copper knocked out near the corner"
    );
}

#[test]
fn boolean_is_deterministic() {
    let a = Region::from_ring(square(0, 0, 2 * MM));
    let b = shape_to_region(&Shape2D::disc(pt(MM, MM), MM), DEFAULT_CIRCLE_SEGS);
    let d1 = difference(&a, &b);
    let d2 = difference(&a, &b);
    assert_eq!(d1, d2, "same inputs ⇒ identical region");
}

#[test]
fn point_within_inside_is_zero_distance() {
    // A 10mm square centred at origin: any interior/boundary point is within any thr,
    // including thr = 0 (distance 0).
    let sq = Region::from_ring(square(0, 0, 5 * MM));
    assert!(sq.point_within(pt(0, 0), 0), "centre is inside");
    assert!(
        sq.point_within(pt(5 * MM, 0), 0),
        "boundary counts as inside"
    );
    assert!(sq.point_within(pt(-2 * MM, 3 * MM), 0), "interior point");
}

#[test]
fn point_within_outside_uses_edge_distance() {
    // Square [-5,5]^2 (mm). A point 2mm to the right of the right edge (at x=7, y=0) is
    // exactly 2mm from the region; within 2mm (≤, boundary of the tolerance disc) and
    // within 3mm, but not within 1mm.
    let sq = Region::from_ring(square(0, 0, 5 * MM));
    let p = pt(7 * MM, 0);
    assert!(!sq.point_within(p, MM), "1mm < 2mm gap → miss");
    assert!(sq.point_within(p, 2 * MM), "exactly 2mm → hit (≤)");
    assert!(sq.point_within(p, 3 * MM), "3mm > 2mm gap → hit");
}

#[test]
fn point_within_corner_is_round_join() {
    // Distance to a convex corner is the Euclidean distance to the corner point (round
    // join), NOT the axis-aligned box distance. Corner at (5,5); a point at (8,9) is
    // 5mm away (3-4-5). Within 5mm, not within 4mm — a miter/box test would disagree.
    let sq = Region::from_ring(square(0, 0, 5 * MM));
    let p = pt(8 * MM, 9 * MM);
    assert!(
        !sq.point_within(p, 4 * MM),
        "4mm < 5mm corner distance → miss"
    );
    assert!(
        sq.point_within(p, 5 * MM),
        "exactly 5mm to the corner → hit"
    );
}

#[test]
fn point_within_negative_thr_floors_to_containment() {
    let sq = Region::from_ring(square(0, 0, 5 * MM));
    assert!(
        sq.point_within(pt(0, 0), -1),
        "inside regardless of negative thr"
    );
    assert!(
        !sq.point_within(pt(7 * MM, 0), -1),
        "outside with negative thr never hits"
    );
}
