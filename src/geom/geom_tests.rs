//! Tests for the geometry shape vocabulary + feature model (the [`super`] facade).
//! Extracted verbatim from the former `geom.rs` inline `#[cfg(test)] mod tests`.

use super::shape::orient;
use super::*;
use crate::coord::{Nm, Point};
const MM: Nm = 1_000_000;
fn pt(x: Nm, y: Nm) -> Point {
    Point { x, y }
}

#[test]
fn coord_ok_bound_is_inclusive() {
    assert!(coord_ok(MAX_COORD));
    assert!(coord_ok(-MAX_COORD));
    assert!(!coord_ok(MAX_COORD + 1));
    assert!(!coord_ok(-MAX_COORD - 1));
}

#[test]
fn kernel_safe_predicate_boundary_is_inclusive() {
    // (The ceiling ordering + i128-safety are compile-time-guarded at the const;
    // this checks the predicate's boundary behaviour.)
    assert!(coord_kernel_safe(KERNEL_SAFE_COORD));
    assert!(coord_kernel_safe(-KERNEL_SAFE_COORD));
    assert!(!coord_kernel_safe(KERNEL_SAFE_COORD + 1));
    assert!(!point_kernel_safe(pt(KERNEL_SAFE_COORD + 1, 0)));
}

/// Companion to the solver's F1 regression: the clearance kernel (`pt_seg_d2`) also
/// runs on composed world coords past `MAX_COORD` but within `KERNEL_SAFE_COORD`, so
/// a clearance check at that scale must not panic in debug (issue 0018, review F1).
#[test]
fn clearance_at_composed_coords_does_not_panic() {
    let c = MAX_COORD + 400_000; // past ingest bound, inside kernel-safe range
    let a = Shape2D::rect(pt(c, 0), MM, MM);
    let b = Shape2D::rect(pt(c + 3 * MM, 0), MM, MM);
    let _ = clearance_violated(&a, &b, 100_000); // exercises pt_seg_d2
}

/// In debug builds the hot kernel predicate asserts its inputs are within
/// `KERNEL_SAFE_COORD` — the loud backstop behind the release-time boundary
/// validation (issue 0018). Release builds trust the boundary and skip the check,
/// so this is debug-only. Note the trip point is `KERNEL_SAFE_COORD`, not the
/// tighter ingest `MAX_COORD` — legal composition between the two must NOT panic
/// (see `place_at_ingest_bound_with_courtyard_does_not_panic` in solve).
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "KERNEL_SAFE_COORD")]
fn circumcenter_debug_asserts_coordinate_bound() {
    let big = KERNEL_SAFE_COORD + 1;
    let _ = circumcenter(pt(big, 0), pt(0, big), pt(1, 1));
}

#[test]
fn disc_disc_clearance_is_center_distance_minus_radii() {
    let a = Shape2D::disc(pt(0, 0), MM); // r = 1mm
    let b = Shape2D::disc(pt(3 * MM, 0), MM); // centers 3mm apart, gap = 3-1-1 = 1mm
    assert!(clearance_violated(&a, &b, MM + 1), "gap 1mm < 1mm+ε");
    assert!(!clearance_violated(&a, &b, MM), "gap 1mm is not < 1mm");
    assert!(
        !clearance_violated(&a, &b, MM / 2),
        "1mm gap clears 0.5mm rule"
    );
}

#[test]
fn overlapping_shapes_violate_any_positive_clearance() {
    let a = Shape2D::disc(pt(0, 0), 2 * MM);
    let b = Shape2D::disc(pt(MM, 0), 2 * MM); // overlap (centers 1mm, radii 2+2)
    assert!(clearance_violated(&a, &b, 1));
    assert!(clearance_violated(&a, &b, 10 * MM));
}

#[test]
fn point_inside_filled_rect_is_overlap() {
    let rect = Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM); // covers [-2,2]^2
    let dot = Shape2D::disc(pt(0, 0), 1); // center inside the rect
    assert!(
        clearance_violated(&rect, &dot, 1),
        "a point inside the pad area is an overlap"
    );
}

#[test]
fn trace_near_pad_edge_to_edge() {
    // Horizontal trace y=0, width 0.2mm (r=0.1mm); square pad centered (0,1mm) 0.4mm.
    let trace = Shape2D::trace(vec![pt(-5 * MM, 0), pt(5 * MM, 0)], MM / 5);
    let pad = Shape2D::rect(pt(0, MM), 4 * MM / 10, 4 * MM / 10); // [-0.2,0.2]x[0.8,1.2]
    // edge-to-edge gap: pad bottom at y=0.8mm, trace top at y=0.1mm → 0.7mm.
    let gap = 7 * MM / 10;
    assert!(clearance_violated(&trace, &pad, gap + 1));
    assert!(!clearance_violated(&trace, &pad, gap));
}

#[test]
fn round_rect_corner_is_rounded_not_sharp() {
    // A sharp rect's corner reaches farther than a rounded one; a probe disc just
    // off the corner clears the round_rect but not the sharp rect.
    let sharp = Shape2D::rect(pt(0, 0), 2 * MM, 2 * MM); // corner at (1,1)mm
    let round = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, MM / 2);
    // Probe just beyond the corner diagonally: sharp corner gap ≈ 0.14mm,
    // rounded corner gap ≈ 0.35mm. A 0.2mm rule separates them.
    let probe = Shape2D::disc(pt(11 * MM / 10, 11 * MM / 10), 1);
    assert!(
        clearance_violated(&sharp, &probe, MM / 5),
        "sharp corner within 0.2mm"
    );
    assert!(
        !clearance_violated(&round, &probe, MM / 5),
        "rounded corner beyond 0.2mm"
    );
}

#[test]
fn round_rect_radius_is_clamped_not_ballooned() {
    // r far larger than the box: clamps to min(w,h)/2 instead of growing. A
    // square 2mm box with r=10mm clamps to r=1mm → a disc of radius 1mm, so a
    // probe 1.5mm from center clears it (a ballooned shape would not).
    let rr = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, 10 * MM);
    let near = Shape2D::disc(pt(15 * MM / 10, 0), 1); // 1.5mm from center
    assert!(
        !clearance_violated(&rr, &near, MM / 10),
        "clamped shape stays ~1mm radius"
    );
    // A probe 0.9mm from center is inside the ~1mm clamped radius.
    let inside = Shape2D::disc(pt(9 * MM / 10, 0), 1);
    assert!(clearance_violated(&rr, &inside, 1));
}

#[test]
fn z_overlap_gates_clearance() {
    let su = Stackup::default_2layer();
    let top = su.slab_z("F.Cu").unwrap();
    let bot = su.slab_z("B.Cu").unwrap();
    assert!(
        !top.overlaps(&bot),
        "top and bottom copper z must not overlap"
    );
    // Two overlapping discs on opposite layers do NOT clash; same layer they do.
    let s = Shape2D::disc(pt(0, 0), MM);
    let a_top = Feature::prism(Role::Conductor, s.clone(), top);
    let b_top = Feature::prism(Role::Conductor, s.clone(), top);
    let b_bot = Feature::prism(Role::Conductor, s, bot);
    assert!(
        !a_top.clears(&b_top, MM),
        "coincident copper on the same layer clashes"
    );
    assert!(
        a_top.clears(&b_bot, MM),
        "different layers do not clash geometrically"
    );
}

#[test]
fn stackup_copper_accessors_are_ordered_top_down() {
    let su = Stackup::default_2layer();
    let cu = su.copper_slabs();
    assert_eq!(cu.len(), 2, "default 2-layer has two copper slabs");
    assert_eq!(cu[0].name, "F.Cu", "top-most copper is index 0");
    assert_eq!(cu[1].name, "B.Cu", "bottom copper is last");
    assert_eq!(su.top_copper(), su.slab_z("F.Cu"));
    assert_eq!(su.bottom_copper(), su.slab_z("B.Cu"));
    assert_eq!(su.nth_copper_from_top(0), su.slab_z("F.Cu"));
    assert_eq!(su.nth_copper_from_top(1), su.slab_z("B.Cu"));
    assert_eq!(su.nth_copper_from_top(2), None, "no third copper layer");
    let bz = su.board_z().unwrap();
    assert_eq!(
        (bz.lo, bz.hi),
        (0, BOARD_THICKNESS),
        "board_z spans the copper+substrate body, not the surface mask/silk"
    );
}

#[test]
fn unmasked_outer_copper_lint() {
    // Fully-masked default board: nothing unmasked.
    assert!(Stackup::default_2layer().unmasked_outer_copper().is_empty());

    // F.Mask only + both copper: the bottom copper side is unmasked, top is fine.
    let cu_top = |name: &str| Slab {
        name: name.into(),
        z: ZRange::new(1_500_000, 1_535_000),
        role: Role::Conductor,
        material: None,
    };
    let cu_bot = |name: &str| Slab {
        name: name.into(),
        z: ZRange::new(0, 35_000),
        role: Role::Conductor,
        material: None,
    };
    let f_mask = Slab {
        name: "F.Mask".into(),
        z: ZRange::new(1_535_000, 1_545_000),
        role: Role::Mask,
        material: None,
    };
    let su = Stackup {
        slabs: vec![cu_top("F.Cu"), cu_bot("B.Cu"), f_mask.clone()],
    };
    assert_eq!(su.unmasked_outer_copper(), vec!["B.Cu".to_string()]);

    // Zero mask slabs anywhere: deliberately maskless, silent.
    let bare = Stackup {
        slabs: vec![cu_top("F.Cu"), cu_bot("B.Cu")],
    };
    assert!(bare.unmasked_outer_copper().is_empty());

    // Mask present but neither side covered: both outer coppers named, top first.
    let stray_mask = Slab {
        name: "mid.Mask".into(),
        z: ZRange::new(700_000, 710_000),
        role: Role::Mask,
        material: None,
    };
    let su2 = Stackup {
        slabs: vec![cu_top("F.Cu"), cu_bot("B.Cu"), stray_mask],
    };
    assert_eq!(
        su2.unmasked_outer_copper(),
        vec!["F.Cu".to_string(), "B.Cu".to_string()]
    );

    // Single copper slab with a mask below it: the bottom face is masked, the top
    // face is bare, so the sole copper is named once via the top branch. (This does
    // not reach the dedupe guard — `bottom_mask()` is Some here — it exercises the
    // ordinary top-unmasked/bottom-masked path on a one-copper board.)
    let one = Stackup {
        slabs: vec![cu_bot("F.Cu"), stray_mask_below()],
    };
    assert_eq!(one.unmasked_outer_copper(), vec!["F.Cu".to_string()]);
}

// A mask slab below the sole copper (outboard of its bottom face): `bottom_mask`
// resolves it, `top_mask` finds nothing above — so the copper's top face is unmasked.
fn stray_mask_below() -> Slab {
    Slab {
        name: "far.Mask".into(),
        z: ZRange::new(-100, -50),
        role: Role::Mask,
        material: None,
    }
}

/// A zero-height slab (`lo == hi`, permitted by `ZRange::new`) flows through the
/// stackup like any other: `slab_z`/`slab` resolve it, its range is degenerate but
/// well-formed, and it z-*touches* an adjacent slab (closed `overlaps`) — the
/// property a fab `Role::Datum` slab relies on (Decision 15).
#[test]
fn zero_height_slab_resolves_through_stackup() {
    let su = Stackup {
        slabs: vec![
            Slab {
                name: "F.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            },
            Slab {
                name: "F.Fab".into(),
                z: ZRange::new(35_000, 35_000),
                role: Role::Datum,
                material: None,
            },
        ],
    };
    let fab = su.slab("F.Fab").expect("datum slab resolves");
    assert_eq!(fab.role, Role::Datum);
    assert_eq!(fab.z.lo, fab.z.hi, "zero-height: lo == hi");
    assert_eq!(su.slab_z("F.Fab"), Some(fab.z));
    assert!(
        fab.z.overlaps(&su.slab_z("F.Cu").unwrap()),
        "zero-height datum z-touches the copper it sits on"
    );
}

#[test]
fn stackup_mask_accessors_resolve_by_role_and_position() {
    // Default stackup: role+position resolution agrees with the named mask slabs.
    let su = Stackup::default_2layer();
    assert_eq!(su.top_mask(), su.slab_z("F.Mask"), "top mask is F.Mask");
    assert_eq!(
        su.bottom_mask(),
        su.slab_z("B.Mask"),
        "bottom mask is B.Mask"
    );

    // A custom stackup whose mask is named `TopMask` still resolves by role + z.
    let custom = Stackup {
        slabs: vec![
            Slab {
                name: "F.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            },
            Slab {
                name: "TopMask".into(),
                z: ZRange::new(35_000, 60_000),
                role: Role::Mask,
                material: None,
            },
        ],
    };
    assert_eq!(
        custom.top_mask(),
        custom.slab_z("TopMask"),
        "the mask above top copper is found by role, whatever its name"
    );
    assert_eq!(custom.bottom_mask(), None, "no mask below the only copper");
}

#[test]
fn netfeature_same_net_is_identity_not_presence() {
    use crate::id::NetId;
    let s = Shape2D::disc(pt(0, 0), MM);
    let z = Stackup::default_2layer().top_copper().unwrap();
    let f = Feature::prism(Role::Conductor, s, z);
    let gnd1 = NetFeature::new(Some(NetId::new("GND")), f.clone());
    let gnd2 = NetFeature::new(Some(NetId::new("GND")), f.clone());
    let vcc = NetFeature::new(Some(NetId::new("VCC")), f.clone());
    let floating = NetFeature::netless(f);
    assert!(gnd1.same_net(&gnd2), "equal net ids are the same net");
    assert!(!gnd1.same_net(&vcc), "different net ids are not");
    assert!(
        !floating.same_net(&floating),
        "an unnetted piece shares net identity with nothing"
    );
}

#[test]
fn capsule_distance_is_to_the_segment() {
    // Pill from (0,0) to (4mm,0), r=0.5mm; probe disc at (2mm, 2mm) r=0.5mm.
    let pill = Shape2D::capsule(pt(0, 0), pt(4 * MM, 0), MM / 2);
    let probe = Shape2D::disc(pt(2 * MM, 2 * MM), MM / 2);
    // gap = 2mm − 0.5 − 0.5 = 1mm.
    assert!(clearance_violated(&pill, &probe, MM + 1));
    assert!(!clearance_violated(&pill, &probe, MM));
}

#[test]
fn circumcenter_is_exact() {
    // Three points on the circle centred (1mm, 0), radius 1mm.
    let (ux, uy, den) = circumcenter(pt(0, 0), pt(MM, MM), pt(2 * MM, 0));
    assert_ne!(den, 0);
    assert_eq!(ux / den, MM as i128, "cx = 1mm");
    assert_eq!(uy / den, 0, "cy = 0");
    // Collinear ⇒ no finite centre.
    let (_, _, d0) = circumcenter(pt(0, 0), pt(MM, 0), pt(2 * MM, 0));
    assert_eq!(d0, 0, "collinear triple has den == 0");
}

#[test]
fn arc_tessellates_onto_its_circle() {
    // Semicircle: (-10mm,0) → (0,10mm) → (10mm,0), centre (0,0), R = 10mm.
    let r = 10 * MM;
    let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), 0);
    let pts = arc.path().flatten(DEFAULT_CHORD_TOL);
    assert!(
        pts.len() > 16,
        "a 10mm semicircle subdivides into many chords"
    );
    assert_eq!(
        *pts.first().unwrap(),
        pt(-r, 0),
        "endpoints stay exact lattice pts"
    );
    assert_eq!(*pts.last().unwrap(), pt(r, 0));
    assert!(
        pts.contains(&pt(0, r)),
        "the defining midpoint is on the polyline"
    );
    // Every vertex lies on the circle to within rounding (a few nm).
    for p in &pts {
        let d = (((p.x as f64).powi(2) + (p.y as f64).powi(2)).sqrt() - r as f64).abs();
        assert!(d < 4.0, "tessellation point off-circle by {d} nm");
    }
}

#[test]
fn arc_bbox_covers_the_bulge() {
    // The same semicircle bows up to y = R between its two endpoints (both at
    // y = 0). The bbox must reach the bulge, not just the corner vertices.
    let r = 10 * MM;
    let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), 0);
    let (min, max) = arc.bbox().unwrap();
    assert_eq!(min.y, 0, "endpoints sit on y = 0");
    assert_eq!(max.y, r, "bbox reaches the arc's top (the bulge)");
    assert_eq!((min.x, max.x), (-r, r));
}

#[test]
fn collinear_arc_degenerates_to_a_chord() {
    // An arc whose three points are collinear is just the straight chord.
    let arc = Shape2D::arc(pt(0, 0), pt(MM, 0), pt(2 * MM, 0), 0);
    let pts = arc.path().flatten(DEFAULT_CHORD_TOL);
    assert_eq!(
        pts,
        vec![pt(0, 0), pt(2 * MM, 0)],
        "collinear ⇒ a single chord"
    );
}

// Perpendicular distance from `(px,py)` to segment `a`–`b`, in f64 nm (test only).
fn pt_seg_dist_f64(px: f64, py: f64, a: Point, b: Point) -> f64 {
    let (ax, ay) = (a.x as f64, a.y as f64);
    let (bx, by) = (b.x as f64, b.y as f64);
    let (ex, ey) = (bx - ax, by - ay);
    let len2 = ex * ex + ey * ey;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((px - ax) * ex + (py - ay) * ey) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (ax + t * ex, ay + t * ey);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

#[test]
fn straight_cubic_flattens_to_a_chord() {
    // Controls lying on the segment ⇒ the curve is the chord ⇒ no subdivision.
    let poly = Path {
        start: pt(0, 0),
        segs: vec![Seg::Cubic {
            c1: pt(3 * MM, 0),
            c2: pt(6 * MM, 0),
            end: pt(9 * MM, 0),
        }],
    }
    .flatten(DEFAULT_CHORD_TOL);
    assert_eq!(poly, vec![pt(0, 0), pt(9 * MM, 0)]);
}

#[test]
fn cubic_flatten_approximates_the_curve_within_tolerance() {
    let (p0, c1, c2, p3) = (
        pt(0, 0),
        pt(0, 4 * MM),
        pt(10 * MM, -4 * MM),
        pt(10 * MM, 0),
    );
    let poly = Path {
        start: p0,
        segs: vec![Seg::Cubic { c1, c2, end: p3 }],
    }
    .flatten(DEFAULT_CHORD_TOL);
    assert_eq!(poly.first().copied(), Some(p0));
    assert_eq!(poly.last().copied(), Some(p3));
    assert!(poly.len() > 2, "a curved cubic must subdivide");
    // Every analytic curve sample lies within tol (+ rounding slack) of the polyline.
    let eval = |t: f64| {
        let u = 1.0 - t;
        let bx = u * u * u * p0.x as f64
            + 3.0 * u * u * t * c1.x as f64
            + 3.0 * u * t * t * c2.x as f64
            + t * t * t * p3.x as f64;
        let by = u * u * u * p0.y as f64
            + 3.0 * u * u * t * c1.y as f64
            + 3.0 * u * t * t * c2.y as f64
            + t * t * t * p3.y as f64;
        (bx, by)
    };
    for i in 0..=200 {
        let (x, y) = eval(i as f64 / 200.0);
        let best = poly
            .windows(2)
            .map(|w| pt_seg_dist_f64(x, y, w[0], w[1]))
            .fold(f64::MAX, f64::min);
        assert!(
            best <= DEFAULT_CHORD_TOL as f64 + 50.0,
            "curve sample {i} is {best} nm off the flattened polyline"
        );
    }
}

#[test]
fn quadratic_flatten_captures_the_apex() {
    // Apex at t=½ is (p0 + 2c + p2)/4 = (5mm, 3mm).
    let (p0, c, p2) = (pt(0, 0), pt(5 * MM, 6 * MM), pt(10 * MM, 0));
    let poly = Path {
        start: p0,
        segs: vec![Seg::Quadratic { ctrl: c, end: p2 }],
    }
    .flatten(DEFAULT_CHORD_TOL);
    assert_eq!(poly.first().copied(), Some(p0));
    assert_eq!(poly.last().copied(), Some(p2));
    assert!(
        poly.iter().any(|p| (p.y - 3 * MM).abs() < 200_000),
        "a flattened vertex tracks the apex bulge near y=3mm"
    );
}

#[test]
fn cubic_bbox_covers_the_bulge() {
    // A cubic bowing up to ~3.75mm; the bbox (via flatten) must reach the bulge.
    let s = Shape2D::cubic(
        pt(0, 0),
        pt(0, 5 * MM),
        pt(10 * MM, 5 * MM),
        pt(10 * MM, 0),
        MM / 10,
    );
    let (_min, max) = s.bbox().unwrap();
    assert!(max.y > 2 * MM, "bbox reaches the upward bulge: {max:?}");
}

#[test]
fn major_arc_with_skewed_midpoint_stays_on_the_intended_side() {
    // A major arc (sweep > 180°) whose `mid` is skewed so one half-arc exceeds 180°.
    // A chord-midpoint projection would flip that half onto the complementary
    // (minor) side; the turn-aware bisection must keep every point on the true arc.
    // Circle centre origin, R = 10mm. start 0°, mid 200°, end 210° (CCW through mid).
    let r = 10 * MM;
    let f = |deg: f64| {
        let a = deg.to_radians();
        pt(
            (r as f64 * a.cos()).round() as Nm,
            (r as f64 * a.sin()).round() as Nm,
        )
    };
    let (start, mid, end) = (f(0.0), f(200.0), f(210.0));
    let pts = Shape2D::arc(start, mid, end, 0)
        .path()
        .flatten(DEFAULT_CHORD_TOL);
    // All points lie on the circle…
    for p in &pts {
        let d = (((p.x as f64).powi(2) + (p.y as f64).powi(2)).sqrt() - r as f64).abs();
        assert!(d < 4.0, "off-circle by {d} nm");
    }
    // …the 0°→200° half must sweep through ~100° (its interior)…
    let saw_100 = pts.iter().any(|p| {
        let ang = (p.y as f64)
            .atan2(p.x as f64)
            .to_degrees()
            .rem_euclid(360.0);
        (90.0..110.0).contains(&ang)
    });
    assert!(
        saw_100,
        "the 0°→200° half must pass through ~100°, not the minor side"
    );
    // …and no point may stray onto the complementary arc (215°..355°).
    let on_minor = pts.iter().any(|p| {
        let ang = (p.y as f64)
            .atan2(p.x as f64)
            .to_degrees()
            .rem_euclid(360.0);
        (215.0..355.0).contains(&ang)
    });
    assert!(!on_minor, "no point may stray onto the complementary arc");
}

#[test]
fn arc_trace_clearance_is_measured_to_the_curve() {
    // A semicircular trace (-10mm,0)→(0,10mm)→(10mm,0), width 0.2mm (r=0.1mm),
    // bulging up to (0,10mm). A probe disc (r=0.1mm) sits 1mm above the bulge at
    // (0,11mm). Edge-to-edge gap = 1mm − 0.1 − 0.1 = 0.8mm — measured to the ARC's
    // top. (The straight chord between the endpoints lies on y=0, ~11mm away; if the
    // kernel mis-measured to the chord this rule would clear.)
    let r = 10 * MM;
    let trace = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), MM / 5);
    let probe = Shape2D::disc(pt(0, 11 * MM), MM / 10);
    let gap = 8 * MM / 10;
    assert!(
        clearance_violated(&trace, &probe, gap + 1),
        "0.8mm gap to the bulge"
    );
    assert!(
        !clearance_violated(&trace, &probe, gap),
        "0.8mm is not < 0.8mm"
    );
    // Sanity: a far probe well outside the arc clears a small rule.
    let far = Shape2D::disc(pt(0, 13 * MM), MM / 10);
    assert!(!clearance_violated(&trace, &far, MM / 2));
}

#[test]
fn convex_hull_of_a_square_is_its_four_corners() {
    // The four corners plus an interior point and edge-midpoints; the hull keeps
    // only the corners (interior + collinear points dropped). CCW order.
    let pts = vec![
        pt(0, 0),
        pt(2 * MM, 0),
        pt(2 * MM, 2 * MM),
        pt(0, 2 * MM),
        pt(MM, MM),     // interior
        pt(MM, 0),      // collinear on bottom edge
        pt(2 * MM, MM), // collinear on right edge
        pt(MM, 2 * MM), // collinear on top edge
        pt(0, MM),      // collinear on left edge
    ];
    let hull = convex_hull(&pts);
    assert_eq!(hull.len(), 4, "only the four corners survive");
    // It is a CCW ring (every consecutive turn is a left turn).
    let n = hull.len();
    for i in 0..n {
        let a = hull[i];
        let b = hull[(i + 1) % n];
        let c = hull[(i + 2) % n];
        assert_eq!(
            orient(a, b, c),
            1,
            "hull winds CCW with no collinear corners"
        );
    }
    // The corner set matches.
    let mut got = hull.clone();
    got.sort_by(|a, b| a.x.cmp(&b.x).then(a.y.cmp(&b.y)));
    assert_eq!(
        got,
        vec![pt(0, 0), pt(0, 2 * MM), pt(2 * MM, 0), pt(2 * MM, 2 * MM)]
    );
}

#[test]
fn convex_hull_handles_degenerate_inputs() {
    assert_eq!(convex_hull(&[]), vec![]);
    assert_eq!(convex_hull(&[pt(MM, MM)]), vec![pt(MM, MM)]);
    // Duplicates collapse; two distinct points have no polygon hull.
    assert_eq!(
        convex_hull(&[pt(0, 0), pt(0, 0), pt(MM, 0), pt(MM, 0)]),
        vec![pt(0, 0), pt(MM, 0)]
    );
    // Three collinear points: no area, reduces to the two extremes.
    assert_eq!(
        convex_hull(&[pt(0, 0), pt(MM, 0), pt(2 * MM, 0)]),
        vec![pt(0, 0), pt(2 * MM, 0)]
    );
}

#[test]
fn determinism_same_inputs_same_answer() {
    let a = Shape2D::round_rect(pt(0, 0), 3 * MM, 2 * MM, MM / 4);
    let b = Shape2D::trace(vec![pt(0, 5 * MM), pt(10 * MM, 5 * MM)], MM / 5);
    let r1 = clearance_violated(&a, &b, 2 * MM);
    let r2 = clearance_violated(&a, &b, 2 * MM);
    assert_eq!(r1, r2);
}

/// A 10 mm square with a 2 mm square hole (walls at ±1 mm), as a `Shape2D::Area`:
/// `radius`/`bbox`/`contains_point`/`points`/`inflated`/`closest_boundary_point` all
/// respect the hole.
#[test]
fn area_shape_geometry_ops() {
    use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region};
    let outer = shape_to_region(
        &Shape2D::rect(pt(0, 0), 10 * MM, 10 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let hole = shape_to_region(
        &Shape2D::rect(pt(0, 0), 2 * MM, 2 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let area = Shape2D::Area {
        region: difference(&outer, &hole),
    };

    assert_eq!(area.radius(), 0, "an Area has no inflation radius");
    assert_eq!(
        area.bbox().unwrap(),
        (pt(-5 * MM, -5 * MM), pt(5 * MM, 5 * MM)),
        "bbox is the outer boundary"
    );
    assert!(area.contains_point(pt(4 * MM, 0)), "inside the filled ring");
    assert!(!area.contains_point(pt(0, 0)), "the hole is not filled");
    assert!(!area.contains_point(pt(9 * MM, 0)), "outside the board");
    assert!(area.points().len() >= 8, "outer + hole ring vertices");

    // A point deep in the hole is not filled; after a 1 mm dilation (walls at ±1 mm
    // move inward, closing the hole) that same point is filled.
    assert!(!area.contains_point(pt(0, 9 * MM / 10)));
    assert!(
        area.inflated(MM).contains_point(pt(0, 9 * MM / 10)),
        "dilation shrinks/closes the hole"
    );
    // Dilation also grows the outer boundary outward.
    let (lo, hi) = area.inflated(MM).bbox().unwrap();
    assert!(
        lo.x < -5 * MM && hi.x > 5 * MM,
        "outer boundary grew by ~1 mm"
    );

    // The nearest boundary point to the hole centre lands on a hole wall (~1 mm away).
    let q = area.closest_boundary_point(pt(0, 0));
    let d2 = (q.x as i128).pow(2) + (q.y as i128).pow(2);
    let mm = MM as i128;
    assert!(
        d2 >= (9 * mm / 10).pow(2) && d2 <= (11 * mm / 10).pow(2),
        "closest boundary is the hole wall at ~1 mm: {q:?}"
    );
}

/// `clears()` for an `Area` substrate: a shape inside the filled island violates (an
/// overlap), a shape deep in a hole clears, and a shape near a hole wall violates.
#[test]
fn area_clears_island_versus_hole() {
    use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region};
    // 20 mm square, 4 mm square hole (walls at ±2 mm).
    let outer = shape_to_region(
        &Shape2D::rect(pt(0, 0), 20 * MM, 20 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let hole = shape_to_region(
        &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let z = ZRange::new(0, 1000);
    let area = Feature::prism(
        Role::Substrate,
        Shape2D::Area {
            region: difference(&outer, &hole),
        },
        z,
    );

    // Inside the filled island: overlaps ⇒ violates even at zero clearance.
    let inside = Feature::prism(Role::Conductor, Shape2D::disc(pt(5 * MM, 0), MM / 10), z);
    assert!(
        !area.clears(&inside, 0),
        "a shape inside the filled area is a violation"
    );

    // Deep in the hole (walls ≥ ~1.9 mm away): clears a 1 mm rule.
    let in_hole = Feature::prism(Role::Conductor, Shape2D::disc(pt(0, 0), MM / 10), z);
    assert!(
        area.clears(&in_hole, MM),
        "a shape deep inside a hole clears the hole walls"
    );

    // Near a hole wall (edge gap ~0 mm): violates a 0.5 mm rule.
    let near_wall = Feature::prism(
        Role::Conductor,
        Shape2D::disc(pt(19 * MM / 10, 0), MM / 10),
        z,
    );
    assert!(
        !area.clears(&near_wall, MM / 2),
        "a shape hard against a hole wall violates"
    );
}

/// A reflecting `map_points` (a bottom-side flip, `(x,y)→(−x,y)`, negative
/// determinant) reverses every ring's signed area. `map_points` must re-sign the
/// rings so islands stay islands and holes stay holes — otherwise `holes()`/
/// `islands()` invert (a wave-2 bottom-silk glyph would render as its own negative).
#[test]
fn area_map_points_preserves_orientation_under_reflection() {
    use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region, signed_area2};
    // 10 mm square centred at +6 mm x (so a reflection across x=0 relocates it), with
    // a 4 mm hole. Filled ring x∈[1,11]∖[4,8]; hole centre at (6,0).
    let outer = shape_to_region(
        &Shape2D::rect(pt(6 * MM, 0), 10 * MM, 10 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let hole = shape_to_region(
        &Shape2D::rect(pt(6 * MM, 0), 4 * MM, 4 * MM),
        DEFAULT_CIRCLE_SEGS,
    );
    let area = Shape2D::Area {
        region: difference(&outer, &hole),
    };

    let flipped = area.map_points(|p| Point { x: -p.x, y: p.y });
    let region = flipped.region().unwrap();

    // Fill semantics survive: a filled-ring point (was (9,0) → (−9,0)) is on the
    // board; the hole centre (was (6,0) → (−6,0)) is still excluded.
    assert!(
        flipped.contains_point(pt(-9 * MM, 0)),
        "island still filled"
    );
    assert!(
        !flipped.contains_point(pt(-6 * MM, 0)),
        "hole still excluded"
    );

    // Orientation survives: exactly one CCW island ring, and holes()/islands() still
    // classify correctly (the actual regression surface).
    assert_eq!(
        region.rings.iter().filter(|r| signed_area2(r) > 0).count(),
        1,
        "one CCW island ring"
    );
    assert_eq!(region.holes().rings.len(), 1, "the hole is still a hole");
    let islands = region.islands();
    assert_eq!(islands.len(), 1, "one island");
    assert_eq!(
        islands[0].rings.len(),
        2,
        "island keeps its hole (outer + hole)"
    );
}
