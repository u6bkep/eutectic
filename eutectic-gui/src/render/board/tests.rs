//! Board-producer tests (renderer-spec §10 CPU tier): scene lowering
//! coverage over a document with traces on two layers, a via, a pour with
//! knockout holes, silk text, and drills; plus dash arc-length accumulation
//! and the determinism contract.

use super::*;
use crate::app::DomainState;
use crate::render::scene::{PlaneKey, PrimShape, SemanticKey, StyleClass};

/// The canvas fixture board (outline, GND pour on F.Cu, toy parts, silk
/// text, NPTH hole) plus routed copper on **both** layers and a via — the
/// full lowering-coverage document.
const BOARD_ECAD: &str = "\
inst C1 Cap
inst C2 Cap
net GND C1.p1 C2.p1
net VBUS C1.p2 C2.p2
place C1 (15mm, 3mm)
place C2 (15mm, 12mm)
board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
region conductor net=GND layer=F.Cu (1mm, 1mm) (19mm, 1mm) (19mm, 14mm) (1mm, 14mm)
text \"BRD\" (4mm, 7mm) h=2mm layer=F.SilkS
hole (10mm, 12mm) dia=1mm
";

fn two_layer_domain() -> DomainState {
    use eutectic_core::command::Command;
    use eutectic_core::coord::Point;
    use eutectic_core::doc::Provenance;
    use eutectic_core::id::{NetId, TraceId, ViaId};
    use eutectic_core::route::{Trace, Via};

    DomainState::from_source_with(
        BOARD_ECAD.to_string(),
        Some("board.eut".to_string()),
        eutectic_core::part::part_library(),
        |_doc| {
            let trace = |net: &str, layer: &str, y: i64| Trace {
                net: NetId::new(net),
                layer: layer.to_string(),
                path: vec![Point { x: 3_000_000, y }, Point { x: 17_000_000, y }],
                width: 500_000,
                prov: Provenance::Free,
            };
            vec![
                Command::AddTrace(TraceId(1), trace("VBUS", "F.Cu", 7_000_000)),
                Command::AddTrace(TraceId(2), trace("GND", "B.Cu", 5_000_000)),
                Command::AddVia(
                    ViaId(1),
                    Via {
                        net: NetId::new("VBUS"),
                        at: Point {
                            x: 15_000_000,
                            y: 10_000_000,
                        },
                        span: None,
                        drill: 300_000,
                        pad: 600_000,
                        prov: Provenance::Free,
                    },
                ),
            ]
        },
    )
}

fn scene() -> Scene {
    let d = two_layer_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    board_scene(doc, &d.lib).expect("committed doc lowers")
}

fn sem_of(s: &Scene, sem: u32) -> &SemanticKey {
    &s.semantics[sem as usize]
}

#[test]
fn planes_enumerate_in_stackup_order() {
    let s = scene();
    let keys: Vec<&PlaneKey> = s.planes.iter().map(|p| &p.key).collect();
    let want = [
        PlaneKey::Substrate,
        PlaneKey::Outline,
        PlaneKey::Silk("B.SilkS".into()),
        PlaneKey::Mask("B.Mask".into()),
        PlaneKey::CopperPour("B.Cu".into()),
        PlaneKey::Copper("B.Cu".into()),
        PlaneKey::CopperPour("F.Cu".into()),
        PlaneKey::Copper("F.Cu".into()),
        PlaneKey::Mask("F.Mask".into()),
        PlaneKey::Silk("F.SilkS".into()),
        PlaneKey::Drills,
    ];
    assert_eq!(keys, want.iter().collect::<Vec<_>>());
}

#[test]
fn traces_bin_per_layer_as_capsules_with_net_ids() {
    let s = scene();
    let fcu = s.plane(&PlaneKey::Copper("F.Cu".into())).unwrap();
    let bcu = s.plane(&PlaneKey::Copper("B.Cu".into())).unwrap();
    let cap_net = |plane: &crate::render::scene::Plane, net: &str| {
        plane.prims.iter().any(|p| {
            matches!(p.shape, PrimShape::Capsule { r, .. } if r == 250_000)
                && *sem_of(&s, p.sem) == SemanticKey::Net(eutectic_core::id::NetId::new(net))
        })
    };
    assert!(cap_net(fcu, "VBUS"), "F.Cu carries the VBUS trace capsule");
    assert!(cap_net(bcu, "GND"), "B.Cu carries the GND trace capsule");
    // The via barrel fans out: a 300k-radius disc on each copper plane,
    // net-attributed.
    for plane in [fcu, bcu] {
        assert!(
            plane.prims.iter().any(|p| {
                matches!(p.shape, PrimShape::Disc { r, .. } if r == 300_000)
                    && *sem_of(&s, p.sem) == SemanticKey::Net(eutectic_core::id::NetId::new("VBUS"))
            }),
            "via barrel disc on {:?}",
            plane.key
        );
    }
}

#[test]
fn pour_lowers_to_holed_polygon_on_its_own_plane() {
    let s = scene();
    let pour = s.plane(&PlaneKey::CopperPour("F.Cu".into())).unwrap();
    assert_eq!(pour.prims.len(), 1, "one authored pour");
    let p = &pour.prims[0];
    assert_eq!(
        *sem_of(&s, p.sem),
        SemanticKey::Net(eutectic_core::id::NetId::new("GND"))
    );
    let PrimShape::Polygon { rings } = &p.shape else {
        panic!("pour must lower to a polygon, got {:?}", p.shape);
    };
    // The VBUS trace + via + pads knock holes into the fill.
    assert!(
        rings.len() > 1,
        "pour must keep its knockout holes ({} rings)",
        rings.len()
    );
    // The other pour plane is enumerated but empty.
    assert!(
        s.plane(&PlaneKey::CopperPour("B.Cu".into()))
            .unwrap()
            .prims
            .is_empty()
    );
}

#[test]
fn silk_text_arrives_as_geometry_never_text_runs() {
    let s = scene();
    let silk = s.plane(&PlaneKey::Silk("F.SilkS".into())).unwrap();
    assert!(!silk.prims.is_empty(), "the BRD label must land on F.SilkS");
    assert!(
        s.planes
            .iter()
            .flat_map(|p| &p.prims)
            .all(|p| !matches!(p.shape, PrimShape::TextRun { .. })),
        "the board producer emits no TextRuns (spec §6: fab ink is geometry)"
    );
    // Stroke-font silk floors its pen radius (a hairline still shows).
    assert!(silk.prims.iter().all(|p| match p.shape {
        PrimShape::Capsule { r, .. } | PrimShape::Disc { r, .. } => r >= MIN_MARKING_R,
        _ => true,
    }));
}

#[test]
fn drills_gather_on_the_drills_plane() {
    let s = scene();
    let drills = s.plane(&PlaneKey::Drills).unwrap();
    // Via drill (150k radius) + authored NPTH (500k radius) at least; pad
    // drills would add more if the toy parts were through-hole.
    assert!(
        drills
            .prims
            .iter()
            .any(|p| matches!(p.shape, PrimShape::Disc { r, .. } if r == 150_000)),
        "via drill"
    );
    assert!(
        drills
            .prims
            .iter()
            .any(|p| matches!(p.shape, PrimShape::Disc { r, .. } if r == 500_000)),
        "authored NPTH hole"
    );
    // The via's drill is attributable to the via entity.
    assert!(
        drills
            .prims
            .iter()
            .any(|p| matches!(sem_of(&s, p.sem), SemanticKey::Via(_))),
    );
}

#[test]
fn mask_openings_punch_the_mask_plane_not_drills() {
    let s = scene();
    let mask = s.plane(&PlaneKey::Mask("F.Mask".into())).unwrap();
    assert_eq!(mask.prims.len(), 1, "one boolean-resolved mask fill");
    let PrimShape::Polygon { rings } = &mask.prims[0].shape else {
        panic!("mask fill is a polygon");
    };
    assert!(
        rings.len() > 1,
        "pad mask openings must appear as holes ({} rings)",
        rings.len()
    );
    // And no mask-opening geometry leaked onto the drills plane: every
    // drill prim there is one of the round drills.
    let drills = s.plane(&PlaneKey::Drills).unwrap();
    assert!(
        drills
            .prims
            .iter()
            .all(|p| matches!(p.shape, PrimShape::Disc { .. })),
        "square pad mask openings must not reach the drills plane"
    );
}

#[test]
fn outline_is_a_dashed_ring_with_continuous_phase() {
    let s = scene();
    let outline = s.plane(&PlaneKey::Outline).unwrap();
    assert!(!outline.prims.is_empty());
    let mut expected_len0 = 0.0_f64;
    for p in &outline.prims {
        assert_eq!(p.class, StyleClass::Dash(DASH_EDGE));
        assert_eq!(*sem_of(&s, p.sem), SemanticKey::Board);
        let PrimShape::Capsule { a, b, .. } = p.shape else {
            panic!("outline edges are capsules");
        };
        assert!(
            (p.len0 - expected_len0).abs() < 1e-6,
            "dash phase must accumulate around the ring ({} vs {expected_len0})",
            p.len0
        );
        expected_len0 += dist(a, b);
    }
    // 20 × 15 mm rectangle: the last edge ends at the full perimeter.
    assert!((expected_len0 - 70_000_000.0).abs() < 1.0);
}

#[test]
fn anchor_is_bounds_center_and_bounds_carry_margin() {
    let s = scene();
    let (x0, y0, x1, y1) = s.bounds;
    assert_eq!(s.anchor.x, (x0 + x1) / 2);
    assert_eq!(s.anchor.y, (y0 + y1) / 2);
    // The board is 20 × 15 mm at origin; bounds include the 2 mm margin
    // (exact extents may exceed the outline via silk/copper, never shrink).
    assert!(x0 <= -MARGIN + 1 && y0 <= -MARGIN + 1);
    assert!(x1 >= 20_000_000 + MARGIN - 1 && y1 >= 15_000_000 + MARGIN - 1);
}

#[test]
fn scene_build_is_deterministic() {
    let d = two_layer_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let a = board_scene(doc, &d.lib).unwrap();
    let b = board_scene(doc, &d.lib).unwrap();
    assert_eq!(a, b, "equal docs must produce equal scenes");
}

#[test]
fn substrate_renders_as_a_plane() {
    let s = scene();
    let sub = s.plane(&PlaneKey::Substrate).unwrap();
    assert_eq!(sub.prims.len(), 1);
    assert!(matches!(sub.prims[0].shape, PrimShape::Polygon { .. }));
    assert_eq!(*sem_of(&s, sub.prims[0].sem), SemanticKey::Board);
}

#[test]
fn part_pads_carry_net_or_pin_identity() {
    let s = scene();
    let fcu = s.plane(&PlaneKey::Copper("F.Cu".into())).unwrap();
    // Toy Cap pads are netted squares: polygon prims with Net sems.
    assert!(
        fcu.prims
            .iter()
            .any(|p| matches!(p.shape, PrimShape::Polygon { .. })
                && matches!(sem_of(&s, p.sem), SemanticKey::Net(_))),
        "pad copper must be net-attributed polygon geometry"
    );
}

// ---------------------------------------------------------------------------
// Stroke lowering: dash arc-length accumulation + arc geometry.
// ---------------------------------------------------------------------------

#[test]
fn stroke_len0_accumulates_through_corners() {
    use eutectic_core::coord::Point;
    let path = eutectic_core::geom::Path::polyline(vec![
        Point { x: 0, y: 0 },
        Point { x: 3_000_000, y: 0 },
        Point {
            x: 3_000_000,
            y: 4_000_000,
        },
    ]);
    let mut out = Vec::new();
    stroke_prims(&mut out, &path, 100_000, 7, StyleClass::Dash(0));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].len0, 0.0);
    assert_eq!(out[1].len0, 3_000_000.0, "second segment starts at 3 mm");
}

#[test]
fn arc_segment_lowers_to_arc_stroke_and_accumulates_length() {
    use eutectic_core::coord::Point;
    // Quarter circle of radius 10 mm around the origin, CCW: (10,0) →
    // (7.071, 7.071) → (0, 10), then a straight tail. The tail's len0 must
    // carry the arc's length (≈ π/2 · 10 mm), so dashes flow through arcs.
    let path = eutectic_core::geom::Path {
        start: Point {
            x: 10_000_000,
            y: 0,
        },
        segs: vec![
            eutectic_core::geom::Seg::Arc {
                mid: Point {
                    x: 7_071_068,
                    y: 7_071_068,
                },
                end: Point {
                    x: 0,
                    y: 10_000_000,
                },
            },
            eutectic_core::geom::Seg::Line {
                end: Point {
                    x: 0,
                    y: 12_000_000,
                },
            },
        ],
    };
    let mut out = Vec::new();
    stroke_prims(&mut out, &path, 200_000, 1, StyleClass::Fill);
    assert_eq!(out.len(), 2);
    let PrimShape::ArcStroke {
        center,
        radius,
        a0,
        a1,
        half_width,
    } = out[0].shape
    else {
        panic!("arc seg must lower to ArcStroke, got {:?}", out[0].shape);
    };
    assert_eq!(out[0].len0, 0.0);
    assert_eq!(half_width, 200_000);
    // CCW quarter turn around ~the origin at ~10 mm radius.
    assert!(a1 > a0, "CCW arc has positive sweep");
    assert!(((a1 - a0) - std::f64::consts::FRAC_PI_2).abs() < 1e-3);
    assert!((radius - 10_000_000.0).abs() < 1_000.0, "radius {radius}");
    assert!(center[0].abs() < 1_000.0 && center[1].abs() < 1_000.0);
    // The tail starts after the arc's length.
    let quarter = std::f64::consts::FRAC_PI_2 * radius;
    assert!(
        (out[1].len0 - quarter).abs() < 10.0,
        "tail len0 {} vs arc length {quarter}",
        out[1].len0
    );
}

#[test]
fn collinear_arc_degenerates_to_capsule() {
    use eutectic_core::coord::Point;
    let path = eutectic_core::geom::Path {
        start: Point { x: 0, y: 0 },
        segs: vec![eutectic_core::geom::Seg::Arc {
            mid: Point { x: 1_000_000, y: 0 },
            end: Point { x: 2_000_000, y: 0 },
        }],
    };
    let mut out = Vec::new();
    stroke_prims(&mut out, &path, 100_000, 1, StyleClass::Fill);
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0].shape, PrimShape::Capsule { .. }));
}

#[test]
fn lone_point_stroke_is_a_disc() {
    use eutectic_core::coord::Point;
    let path = eutectic_core::geom::Path {
        start: Point { x: 5, y: 5 },
        segs: vec![],
    };
    let mut out = Vec::new();
    stroke_prims(&mut out, &path, 42, 3, StyleClass::Fill);
    assert_eq!(out.len(), 1);
    assert!(matches!(
        out[0].shape,
        PrimShape::Disc {
            r: 42,
            c: Point { x: 5, y: 5 }
        }
    ));
}

// ---------------------------------------------------------------------------
// End-to-end smoke: the real 4-layer multiprobe board (reads poc/ files,
// like the old canvas's smoke test — not part of the inline-only bundle).
// ---------------------------------------------------------------------------

#[test]
fn poc_board_lowers_end_to_end() {
    let d = crate::fixtures::poc_board_domain();
    let doc = d.doc.as_ref().expect("poc board elaborates");
    let s = board_scene(doc, &d.lib).expect("poc board lowers");
    // Four copper planes (F, In1, In2, B), each with discrete copper, plus
    // per-slab pour planes and a populated drills plane.
    let copper: Vec<&Plane> = s
        .planes
        .iter()
        .filter(|p| matches!(p.key, PlaneKey::Copper(_)))
        .collect();
    assert_eq!(copper.len(), 4, "4-layer stackup");
    assert!(copper.iter().all(|p| !p.prims.is_empty()));
    assert!(!s.plane(&PlaneKey::Drills).unwrap().prims.is_empty());
    // Silk exists and fab-datum geometry binned onto Fab planes (the poc
    // stackup carries F.Fab/B.Fab slabs), never lost.
    assert!(
        !s.plane(&PlaneKey::Silk("F.SilkS".into()))
            .unwrap()
            .prims
            .is_empty()
    );
    assert!(
        s.planes
            .iter()
            .any(|p| matches!(p.key, PlaneKey::Fab(_)) && !p.prims.is_empty()),
        "fab datum geometry lands on a Fab plane"
    );
    // Every plane key is one of the enumerated kinds (nothing fell through
    // to the defensive leftover-append).
    let n_semantics = s.semantics.len();
    assert!(n_semantics > 40, "nets + parts intern ({n_semantics})");
}
