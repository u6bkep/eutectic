use super::*;
use crate::doc::{Component, Dof, MM, Orient, Provenance};
use crate::id::EntityId;

fn comp(part: &str, pos: Point, orient: Orient) -> Component {
    Component {
        id: EntityId::new("u1"),
        part: part.into(),
        pos: Dof {
            value: pos,
            prov: Provenance::Free,
        },
        orient,
        params: std::collections::BTreeMap::new(),
        label: None,
    }
}

#[test]
fn pin_offset_resolves_discrete_and_interface_pins() {
    let lib = part_library();
    let ldo = &lib["LDO"];
    assert_eq!(ldo.pin_offset("VOUT"), Some(Point { x: 2 * MM, y: 0 }));
    assert_eq!(ldo.pin_offset("nope"), None);
    let mcu = &lib["MCU"];
    // Interface signals addressed as `port.signal`.
    assert_eq!(mcu.pin_offset("uart.tx"), Some(Point { x: 3 * MM, y: MM }));
    assert_eq!(mcu.pin_offset("uart.bogus"), None);
}

#[test]
fn resolve_selector_fans_out_by_name_and_falls_back_to_number() {
    use PinRole::*;
    let mk = |name: &str, number: &str, role| PinDef {
        name: name.into(),
        number: number.into(),
        role,
        offset: Point { x: 0, y: 0 },
        pad: None,
    };
    let part = PartDef {
        name: "P".into(),
        // Two pads share the name VDD (distinct numbers) — the duplicate-power
        // case; numbers are out of order to prove order follows declaration.
        pins: vec![
            mk("VDD", "1", PowerIn),
            mk("VDD", "8", PowerIn),
            mk("GND", "4", Passive),
        ],
        interfaces: BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: None,
        class: None,
    };
    // A functional name fans out to *every* matching pad number.
    assert_eq!(
        part.resolve_selector("VDD"),
        vec!["1".to_string(), "8".to_string()]
    );
    assert_eq!(part.resolve_selector("GND"), vec!["4".to_string()]);
    // No name matches -> fall back to a direct pad-number reference.
    assert_eq!(part.resolve_selector("8"), vec!["8".to_string()]);
    // Names nothing -> empty, so the caller raises a hard error (no silent dangle).
    assert!(part.resolve_selector("NOPE").is_empty());
    // Stored identity resolves by number, never by the colliding name.
    assert_eq!(part.pin_role("8"), Some(PowerIn));
    assert_eq!(part.pin_role("VDD"), None);
}

/// A pin's world position is exact under each of the four cardinal rotations.
#[test]
fn pin_world_exact_under_each_cardinal_rotation() {
    let lib = part_library();
    let ldo = &lib["LDO"];
    // VOUT local offset is (2mm, 0); component at (10mm, 5mm).
    let at = Point::mm(10, 5);
    let cases = [
        (
            Orient::from_deg(0).unwrap(),
            Point {
                x: 12 * MM,
                y: 5 * MM,
            },
        ), // (+2, 0)
        (
            Orient::from_deg(90).unwrap(),
            Point {
                x: 10 * MM,
                y: 7 * MM,
            },
        ), // (0, +2)
        (
            Orient::from_deg(180).unwrap(),
            Point {
                x: 8 * MM,
                y: 5 * MM,
            },
        ), // (-2, 0)
        (
            Orient::from_deg(270).unwrap(),
            Point {
                x: 10 * MM,
                y: 3 * MM,
            },
        ), // (0, -2)
    ];
    for (o, expected) in cases {
        let c = comp("LDO", at, o);
        assert_eq!(
            pin_world(&c, ldo, "VOUT"),
            Some(expected),
            "rotation {:?}",
            o
        );
    }
}

#[test]
fn rotate_is_exact_and_reversible() {
    let p = Point { x: 3 * MM, y: MM };
    assert_eq!(Orient::from_deg(0).unwrap().apply(p), p);
    // Two 180s (or four 90s) return to the original — exact, no drift.
    assert_eq!(
        Orient::from_deg(180)
            .unwrap()
            .apply(Orient::from_deg(180).unwrap().apply(p)),
        p
    );
    let q = Orient::from_deg(90).unwrap().apply(
        Orient::from_deg(90).unwrap().apply(
            Orient::from_deg(90)
                .unwrap()
                .apply(Orient::from_deg(90).unwrap().apply(p)),
        ),
    );
    assert_eq!(q, p);
}

#[test]
fn quaternion_cardinals_match_legacy_rotation_exactly() {
    let p = Point { x: 3 * MM, y: MM };
    assert_eq!(Orient::from_deg(0).unwrap().apply(p), p);
    assert_eq!(
        Orient::from_deg(90).unwrap().apply(p),
        Point { x: -p.y, y: p.x }
    );
    assert_eq!(
        Orient::from_deg(180).unwrap().apply(p),
        Point { x: -p.x, y: -p.y }
    );
    assert_eq!(
        Orient::from_deg(270).unwrap().apply(p),
        Point { x: p.y, y: -p.x }
    );
    // Default is identity, not the all-zero (invalid) quaternion.
    assert_eq!(Orient::default(), Orient::IDENTITY);
    assert_eq!(Orient::IDENTITY.apply(p), p);
}

#[test]
fn flip_to_bottom_is_a_rotation_not_a_mirror_flag() {
    // 180° about the y-axis = flip-to-bottom: a pure rotation, no bool needed.
    let flip = Orient {
        w: 0,
        x: 0,
        y: 1,
        z: 0,
    };
    assert!(flip.is_bottom(), "local +z now points down ⇒ bottom side");
    assert!(
        !Orient::from_deg(90).unwrap().is_bottom(),
        "an about-z turn stays top side"
    );
    // Applied to a planar point it flips x and stays in-plane (exact).
    assert_eq!(flip.apply(Point { x: 5, y: 3 }), Point { x: -5, y: 3 });
}

#[test]
fn flip_convention_is_the_y_axis_board_turn() {
    // The board-turn convention (KiCad/fab): flipping to the bottom negates x and
    // preserves y in-plane, so bottom silk reads upright. `flipped()` of the identity
    // is exactly `FLIP_y = (0,0,1,0)`, and its in-plane effect is that x-negation.
    let flip = Orient::default().flipped();
    assert_eq!(
        flip,
        Orient {
            w: 0,
            x: 0,
            y: 1,
            z: 0,
        }
    );
    assert!(flip.is_bottom());
    assert_eq!(flip.apply(Point { x: 7, y: 4 }), Point { x: -7, y: 4 });
}

#[test]
fn to_deg_projects_cardinals_exactly() {
    for d in [0, 90, 180, 270] {
        assert_eq!(Orient::from_deg(d).unwrap().to_deg(), d);
    }
}

#[test]
fn degenerate_quaternion_apply_is_a_safe_no_op() {
    // A zero quaternion isn't a rotation; `apply` must not divide by zero (defence
    // in depth — the parser also rejects it). It falls back to leaving the point put.
    let zero = Orient {
        w: 0,
        x: 0,
        y: 0,
        z: 0,
    };
    assert_eq!(zero.apply(Point { x: 5, y: 3 }), Point { x: 5, y: 3 });
}

#[test]
fn arbitrary_angle_rotates_correctly() {
    // 30° about z: apply to (1mm, 0) ≈ (cos30, sin30)·1mm = (866025, 500000) nm.
    let o = Orient::from_angle_deg(30.0);
    let r = o.apply(Point { x: MM, y: 0 });
    assert!(
        (r.x - 866_025).abs() < 50 && (r.y - 500_000).abs() < 50,
        "got {r:?}"
    );
    assert_eq!(o.to_deg(), 30);
}

#[test]
fn bottom_side_pad_swaps_to_the_bottom_copper_layer() {
    let su = Stackup::default_2layer();
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(surface_pad(Shape2D::disc(Point { x: 0, y: 0 }, MM))), // a Top pad
    };
    let top = comp("P", Point { x: 0, y: 0 }, Orient::default());
    let bot = comp("P", Point { x: 0, y: 0 }, Orient::default().flipped());
    assert!(bot.orient.is_bottom() && !top.orient.is_bottom());
    let tf = pin.pad_features(&top, &su);
    let bf = pin.pad_features(&bot, &su);
    let (_, z_top) = prism_shape_z(&tf[0]);
    let (_, z_bot) = prism_shape_z(&bf[0]);
    assert_eq!(
        z_top,
        su.top_copper().unwrap(),
        "top-side Top pad → top copper"
    );
    assert_eq!(
        z_bot,
        su.bottom_copper().unwrap(),
        "flipped Top pad → bottom copper (derived from orientation, no flag)"
    );
}

use crate::geom::{self, Extent, Role, Shape2D, Stackup};

/// A surface pad: one copper region on `Top`, no drill.
fn surface_pad(shape: Shape2D) -> PadGeo {
    PadGeo {
        copper: vec![PadCopper {
            shape,
            layers: PadLayers::Top,
        }],
        drill: None,
    }
}

fn prism_shape_z(f: &geom::Feature) -> (&Shape2D, geom::ZRange) {
    match &f.extent {
        Extent::Prism { shape, z } => (shape, *z),
    }
}

#[test]
fn pad_features_surface_pad_one_conductor_on_top() {
    let stackup = Stackup::default_2layer();
    // A 1mm square pad offset (1mm,0) in the footprint frame.
    let pad_shape = Shape2D::rect(Point { x: MM, y: 0 }, MM, MM);
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: MM, y: 0 },
        pad: Some(surface_pad(pad_shape.clone())),
    };
    let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
    let feats = pin.pad_features(&c, &stackup);
    let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
    assert_eq!(conductors.len(), 1, "one copper region, no drill");
    let (shape, z) = prism_shape_z(conductors[0]);
    assert_eq!(z, stackup.top_copper().unwrap(), "Top → top copper z");
    // At the origin with Deg0, the world shape == the local shape; bbox matches the
    // world-mapped copper bbox.
    let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
    assert_eq!(shape.bbox(), world.bbox());
    assert_eq!(shape.bbox(), pad_shape.bbox());
}

/// A surface pad emits one mask-opening `Void` on its resolved side's mask slab:
/// F.Mask for a top-placed pad, B.Mask for a flipped (bottom) one, and the opening
/// is the pad copper inflated by [`geom::MASK_EXPANSION`] (Decision 13).
#[test]
fn pad_features_surface_pad_opens_its_side_mask() {
    let su = Stackup::default_2layer();
    let pad_shape = Shape2D::rect(Point { x: MM, y: 0 }, MM, MM);
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: MM, y: 0 },
        pad: Some(surface_pad(pad_shape)),
    };

    // Top-placed: opens F.Mask, at the F.Mask z, expanded by the margin.
    let top = comp("P", Point { x: 0, y: 0 }, Orient::default());
    let tf = pin.pad_features(&top, &su);
    let opens: Vec<_> = tf.iter().filter(|f| f.role == Role::Void).collect();
    assert_eq!(opens.len(), 1, "one mask opening for a surface pad");
    let (shape, z) = prism_shape_z(opens[0]);
    assert_eq!(z, su.slab_z("F.Mask").unwrap(), "top pad opens F.Mask");
    let world = pad_copper_world(&top, &pin.pad.as_ref().unwrap().copper[0]);
    assert_eq!(
        *shape,
        world.inflated(geom::MASK_EXPANSION),
        "opening is the copper expanded by the mask margin"
    );

    // Flipped (bottom): opens B.Mask instead (derived from orientation, no flag).
    let bot = comp("P", Point { x: 0, y: 0 }, Orient::default().flipped());
    let bf = pin.pad_features(&bot, &su);
    let opens: Vec<_> = bf.iter().filter(|f| f.role == Role::Void).collect();
    assert_eq!(opens.len(), 1, "one mask opening for a flipped surface pad");
    assert_eq!(
        prism_shape_z(opens[0]).1,
        su.slab_z("B.Mask").unwrap(),
        "flipped pad opens B.Mask"
    );
}

/// A custom stackup with no mask slab opens nothing (a `Void` is a no-op where no
/// mask exists — not an error). The copper still lowers as usual.
#[test]
fn pad_features_no_mask_slab_opens_nothing() {
    let su = Stackup {
        slabs: vec![geom::Slab {
            name: "F.Cu".into(),
            z: geom::ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: None,
        }],
    };
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(surface_pad(Shape2D::rect(Point { x: 0, y: 0 }, MM, MM))),
    };
    let c = comp("P", Point { x: 0, y: 0 }, Orient::default());
    let feats = pin.pad_features(&c, &su);
    assert!(
        !feats.iter().any(|f| f.role == Role::Void),
        "no mask slab ⇒ no opening"
    );
    assert_eq!(
        feats.iter().filter(|f| f.role == Role::Conductor).count(),
        1,
        "copper still lowers"
    );
}

/// The opening is resolved by role + z-position, not by a hardcoded slab name: a
/// custom stackup whose mask slab is named `TopMask` still gets a pad opening at
/// that slab's z. Guards the review's solid-by-role vs opening-by-name asymmetry —
/// `elaborate::features` masks this slab by role, so the opening must find it too.
#[test]
fn pad_features_opening_resolves_custom_named_mask_slab() {
    let su = Stackup {
        slabs: vec![
            geom::Slab {
                name: "F.Cu".into(),
                z: geom::ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            },
            geom::Slab {
                name: "TopMask".into(),
                z: geom::ZRange::new(35_000, 60_000),
                role: Role::Mask,
                material: Some(geom::Material::named("soldermask")),
            },
        ],
    };
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(surface_pad(Shape2D::rect(Point { x: 0, y: 0 }, MM, MM))),
    };
    let c = comp("P", Point { x: 0, y: 0 }, Orient::default());
    let feats = pin.pad_features(&c, &su);
    let opens: Vec<_> = feats.iter().filter(|f| f.role == Role::Void).collect();
    assert_eq!(opens.len(), 1, "the differently-named mask slab is opened");
    assert_eq!(
        prism_shape_z(opens[0]).1,
        su.slab_z("TopMask").unwrap(),
        "opening lands at the custom-named mask slab's z"
    );
}

#[test]
fn pad_features_through_pad_fans_out_with_drill_void() {
    let stackup = Stackup::default_2layer();
    let pad_shape = Shape2D::disc(Point { x: 0, y: 0 }, MM);
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(PadGeo {
            copper: vec![PadCopper {
                shape: pad_shape.clone(),
                layers: PadLayers::Through,
            }],
            drill: Some(Drill::Round { d: MM / 2 }),
        }),
    };
    let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
    let feats = pin.pad_features(&c, &stackup);
    let n_cu = stackup.copper_slabs().len();
    assert_eq!(n_cu, 2, "default 2-layer stackup has two copper slabs");
    let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
    let voids: Vec<_> = feats.iter().filter(|f| f.role == Role::Void).collect();
    assert_eq!(conductors.len(), n_cu, "one conductor per copper slab");
    // Voids: the drill (spanning the full stackup) + the two mask openings (a
    // through pad opens both F.Mask and B.Mask).
    let drill_void: Vec<_> = voids
        .iter()
        .filter(|f| prism_shape_z(f).1 == stackup.full_z().unwrap())
        .collect();
    assert_eq!(drill_void.len(), 1, "one drill void");
    assert_eq!(
        voids.len(),
        3,
        "drill void + two mask openings (both sides)"
    );
    // The two mask openings are on F.Mask and B.Mask (a through pad opens both).
    let mut mask_zs: Vec<_> = voids
        .iter()
        .map(|f| prism_shape_z(f).1)
        .filter(|z| *z != stackup.full_z().unwrap())
        .collect();
    mask_zs.sort_by_key(|z| z.lo);
    let mut want = vec![
        stackup.slab_z("F.Mask").unwrap(),
        stackup.slab_z("B.Mask").unwrap(),
    ];
    want.sort_by_key(|z| z.lo);
    assert_eq!(mask_zs, want, "through pad opens both F.Mask and B.Mask");
    // All conductor features share the same world shape, one per slab z.
    let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
    let mut zs: Vec<_> = conductors
        .iter()
        .map(|f| {
            let (shape, z) = prism_shape_z(f);
            assert_eq!(
                *shape, world,
                "every fan-out feature shares the world shape"
            );
            z
        })
        .collect();
    zs.sort_by_key(|z| z.lo);
    let slab_zs = {
        let mut v: Vec<_> = stackup.copper_slabs().iter().map(|s| s.z).collect();
        v.sort_by_key(|z| z.lo);
        v
    };
    assert_eq!(zs, slab_zs, "fan-out covers every copper slab z");
    // The drill void spans the full stackup (pierces mask + silk, not just the body).
    let (_, vz) = prism_shape_z(drill_void[0]);
    assert_eq!(
        vz,
        stackup.full_z().unwrap(),
        "drill void pierces the full stackup"
    );
}

#[test]
fn pad_features_slot_drill_is_a_world_mapped_capsule() {
    // Hardens the slot-drill frame the Phase-1 agent verified only by reasoning:
    // the slot endpoints are world-mapped through the *same* `to_world` as copper
    // (so a rotated/translated component moves them), and the void spans the board.
    let stackup = Stackup::default_2layer();
    let a = Point { x: -MM, y: 0 };
    let b = Point { x: MM, y: 0 };
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(PadGeo {
            copper: vec![PadCopper {
                shape: Shape2D::disc(Point { x: 0, y: 0 }, MM),
                layers: PadLayers::Through,
            }],
            drill: Some(Drill::Slot { a, b, d: MM / 2 }),
        }),
    };
    // Rotated + translated so a raw (un-mapped) slot would land in the wrong place.
    let c = comp(
        "P",
        Point { x: 5 * MM, y: 0 },
        Orient::from_deg(90).unwrap(),
    );
    let feats = pin.pad_features(&c, &stackup);
    // The drill void is the one spanning the full stackup; the others are mask
    // openings (a through pad opens both sides).
    let drill_void: Vec<_> = feats
        .iter()
        .filter(|f| f.role == Role::Void && prism_shape_z(f).1 == stackup.full_z().unwrap())
        .collect();
    assert_eq!(drill_void.len(), 1, "one drill void");
    let (shape, vz) = prism_shape_z(drill_void[0]);
    // Drill `d` is a diameter, so the capsule radius is `d / 2` (= MM / 4).
    let expected = Shape2D::capsule(a, b, MM / 4).map_points(|p| to_world(&c, p));
    assert_eq!(*shape, expected, "slot void is the world-mapped capsule");
    assert_eq!(
        vz,
        stackup.full_z().unwrap(),
        "slot void pierces the full stackup"
    );
}

#[test]
fn pad_features_rotated_component_rotates_world_shape() {
    let stackup = Stackup::default_2layer();
    // Pad at (2mm, 0) in the footprint frame; a Deg90 component rotates it to
    // (0, 2mm). Reusing pad_copper_world means the feature shape moves with it.
    let pad_shape = Shape2D::rect(Point { x: 2 * MM, y: 0 }, MM, MM);
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 2 * MM, y: 0 },
        pad: Some(surface_pad(pad_shape)),
    };
    let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(90).unwrap());
    let feats = pin.pad_features(&c, &stackup);
    let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
    assert_eq!(conductors.len(), 1);
    let (shape, _) = prism_shape_z(conductors[0]);
    let (lo, hi) = shape.bbox().unwrap();
    // The pad centre moved from (2mm,0) to (0,2mm); its bbox is now centred there.
    let cx = (lo.x + hi.x) / 2;
    let cy = (lo.y + hi.y) / 2;
    assert_eq!((cx, cy), (0, 2 * MM), "Deg90 rotates the world shape");
    // And it matches the world-mapped copper directly.
    let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
    assert_eq!(shape.bbox(), world.bbox());
}

#[test]
fn courtyard_shape_covers_the_pads_plus_margin() {
    // Two 1mm square pads at (±2mm, 0). The hull of their corners spans
    // x∈[-2.5,2.5]mm, y∈[-0.5,0.5]mm; the courtyard is that polygon inflated by
    // COURTYARD_MARGIN.
    let mk = |cx: Nm| PinDef {
        name: "p".into(),
        number: "p".into(),
        role: PinRole::Passive,
        offset: Point { x: cx, y: 0 },
        pad: Some(surface_pad(Shape2D::rect(Point { x: cx, y: 0 }, MM, MM))),
    };
    let def = PartDef {
        name: "R".into(),
        pins: vec![mk(2 * MM), mk(-2 * MM)],
        interfaces: BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: None,
        class: None,
    };
    let court = courtyard_shape(&def).expect("a real pad part has a courtyard");
    assert!(
        matches!(court, Shape2D::Polygon { .. }),
        "courtyard is a polygon"
    );
    assert_eq!(
        court.radius(),
        COURTYARD_MARGIN,
        "radius carries the margin"
    );
    // The polygon skeleton is the pad hull; its bbox is the hull bbox + margin.
    let (lo, hi) = court.bbox().unwrap();
    assert_eq!(lo.x, -25 * MM / 10 - COURTYARD_MARGIN);
    assert_eq!(hi.x, 25 * MM / 10 + COURTYARD_MARGIN);
    assert_eq!(lo.y, -5 * MM / 10 - COURTYARD_MARGIN);
    assert_eq!(hi.y, 5 * MM / 10 + COURTYARD_MARGIN);
    // The hull encloses each pad centre.
    assert!(court.contains_point(Point { x: 2 * MM, y: 0 }));
    assert!(court.contains_point(Point { x: -2 * MM, y: 0 }));
    // A disc sitting just outside the hull but within the margin overlaps it.
    let probe = Shape2D::disc(
        Point {
            x: 26 * MM / 10,
            y: 0,
        },
        1,
    );
    assert!(
        geom::clearance_violated(&court, &probe, 0),
        "a point within the margin band is inside the courtyard keep-out"
    );
}

#[test]
fn courtyard_shape_is_none_without_a_footprint() {
    // Toy library parts carry no pads → no physical courtyard.
    let lib = part_library();
    assert!(courtyard_shape(&lib["LDO"]).is_none());
    // A single round pad has only one skeleton vertex: no 2-D hull → None.
    let one = PartDef {
        name: "dot".into(),
        pins: vec![PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(surface_pad(Shape2D::disc(Point { x: 0, y: 0 }, MM))),
        }],
        interfaces: BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: None,
        class: None,
    };
    assert!(courtyard_shape(&one).is_none());
}

#[test]
fn swap_side_flips_f_and_b_prefixes_only() {
    assert_eq!(swap_side("F.SilkS"), "B.SilkS");
    assert_eq!(swap_side("B.CrtYd"), "F.CrtYd");
    assert_eq!(swap_side("core"), "core"); // no side prefix ⇒ unchanged
    assert_eq!(swap_side("In1.Cu"), "In1.Cu");
}

/// Footprint silk lowers to a `Role::Marking` feature on the F.SilkS slab z when
/// placed top-side, and swaps to B.SilkS z when the component is flipped — the same
/// side derivation pad copper uses, verified end-to-end.
#[test]
fn graphic_features_place_silk_and_swap_side_on_flip() {
    let su = Stackup::default_2layer();
    let def = PartDef {
        name: "G".into(),
        pins: vec![],
        interfaces: BTreeMap::new(),
        graphics: vec![FpGraphic {
            shape: Shape2D::capsule(Point { x: -MM, y: 0 }, Point { x: MM, y: 0 }, 60_000),
            layer: "F.SilkS".into(),
        }],
        texts: vec![],
        courtyard: None,
        class: None,
    };
    let top = comp("G", Point { x: 0, y: 0 }, Orient::default());
    let bot = comp("G", Point { x: 0, y: 0 }, Orient::default().flipped());
    let tf = graphic_features(&def, &top, &su);
    let bf = graphic_features(&def, &bot, &su);
    assert_eq!(tf.len(), 1);
    assert_eq!(tf[0].role, Role::Marking);
    let (_, z_top) = prism_shape_z(&tf[0]);
    let (_, z_bot) = prism_shape_z(&bf[0]);
    assert_eq!(
        z_top,
        su.slab_z("F.SilkS").unwrap(),
        "top-side silk → F.SilkS z"
    );
    assert_eq!(
        z_bot,
        su.slab_z("B.SilkS").unwrap(),
        "flipped silk → B.SilkS z (side swap, no flag)"
    );
}

/// A graphic whose (resolved) slab is absent from the stackup is skipped — matching
/// how `pad_features` drops a pad on a missing copper slab.
#[test]
fn graphic_features_skips_a_missing_slab() {
    let su = Stackup::default_2layer();
    let def = PartDef {
        name: "G".into(),
        pins: vec![],
        interfaces: BTreeMap::new(),
        graphics: vec![FpGraphic {
            shape: Shape2D::capsule(Point { x: 0, y: 0 }, Point { x: MM, y: 0 }, 1),
            layer: "F.Fab".into(), // not a slab in the default stackup
        }],
        texts: vec![],
        courtyard: None,
        class: None,
    };
    let c = comp("G", Point { x: 0, y: 0 }, Orient::default());
    assert!(graphic_features(&def, &c, &su).is_empty());
}

/// A graphic's `Role` comes from its resolved slab, not a hardcoded `Marking`
/// (Decision 15): a graphic on a `Role::Datum` fab slab lowers to a `Role::Datum`
/// feature, while silk stays `Role::Marking`. Same lowering, role forward-queried.
#[test]
fn graphic_features_take_role_from_slab() {
    // Default stackup plus a zero-height F.Fab datum slab at the F.Cu top face.
    let mut slabs = Stackup::default_2layer().slabs;
    let top = slabs.iter().find(|s| s.name == "F.Cu").unwrap().z.hi;
    slabs.push(geom::Slab {
        name: "F.Fab".into(),
        z: geom::ZRange::new(top, top),
        role: Role::Datum,
        material: None,
    });
    let su = Stackup { slabs };
    let line = || Shape2D::capsule(Point { x: 0, y: 0 }, Point { x: MM, y: 0 }, 60_000);
    let def = PartDef {
        name: "G".into(),
        pins: vec![],
        interfaces: BTreeMap::new(),
        graphics: vec![
            FpGraphic {
                shape: line(),
                layer: "F.Fab".into(),
            },
            FpGraphic {
                shape: line(),
                layer: "F.SilkS".into(),
            },
        ],
        texts: vec![],
        courtyard: None,
        class: None,
    };
    let c = comp("G", Point { x: 0, y: 0 }, Orient::default());
    let feats = graphic_features(&def, &c, &su);
    assert_eq!(feats[0].role, Role::Datum, "fab slab → Datum feature");
    assert_eq!(prism_shape_z(&feats[0]).1, su.slab_z("F.Fab").unwrap());
    assert_eq!(
        feats[1].role,
        Role::Marking,
        "silk slab → Marking, unchanged"
    );
}

/// A KiCad-imported F.Fab graphic materializes into a feature only when the stackup
/// carries a fab slab (Decision 15): under the default stackup — which has none — it
/// produces nothing, exactly as the manually-built case above.
#[test]
fn imported_fab_graphic_is_inert_without_a_fab_slab() {
    let def = crate::kicad::import_footprint(
        r#"(footprint "F"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_line (start 0 0) (end 1 0) (width 0.1) (layer "F.Fab")))"#,
    )
    .unwrap();
    assert!(
        def.graphics.iter().any(|g| g.layer == "F.Fab"),
        "the fab graphic imported"
    );
    let c = comp("F", Point { x: 0, y: 0 }, Orient::default());
    assert!(
        graphic_features(&def, &c, &Stackup::default_2layer()).is_empty(),
        "no fab slab in the default stackup ⇒ the fab graphic emits no feature"
    );
}

/// An imported courtyard overrides both the polygon `courtyard_shape` and the AABB
/// `courtyard_half_extents` proxy the solver pushes (Decision 10).
#[test]
fn imported_courtyard_overrides_derived_hull() {
    let def = PartDef {
        name: "C".into(),
        // A lone tiny pad — its derived hull would be None / near-zero extents.
        pins: vec![PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(surface_pad(Shape2D::disc(Point { x: 0, y: 0 }, MM))),
        }],
        interfaces: BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: Some(Shape2D::rect(Point { x: 0, y: 0 }, 8 * MM, 4 * MM)),
        class: None,
    };
    // Half-extents come from the imported outline (4mm × 2mm), not the pad hull.
    assert_eq!(courtyard_half_extents(&def), (4 * MM, 2 * MM));
    let court = courtyard_shape(&def).expect("imported courtyard");
    assert_eq!(
        court.bbox().unwrap().1,
        Point {
            x: 4 * MM,
            y: 2 * MM
        }
    );
}

#[test]
fn pad_features_no_pad_is_empty() {
    let stackup = Stackup::default_2layer();
    let pin = pin("VIN", PinRole::PowerIn, Point { x: 0, y: 0 });
    let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
    assert!(pin.pad_features(&c, &stackup).is_empty());
}

#[test]
fn orient_from_deg_normalises_and_rejects_off_axis() {
    assert_eq!(Orient::from_deg(-90), Some(Orient::from_deg(270).unwrap()));
    assert_eq!(Orient::from_deg(450), Some(Orient::from_deg(90).unwrap()));
    assert_eq!(Orient::from_deg(360), Some(Orient::from_deg(0).unwrap()));
    assert_eq!(Orient::from_deg(45), None);
}

// ---- footprint auto-text lowering (Decision 14) --------------------------------

/// A part carrying a single text anchor (`height = 1mm`, identity orient), for the
/// lowering tests. Layer is side-relative like `graphics`.
fn text_part(name: &str, kind: FpTextKind, at: Point, layer: &str) -> PartDef {
    PartDef {
        name: name.into(),
        pins: vec![],
        interfaces: BTreeMap::new(),
        graphics: vec![],
        texts: vec![FpText {
            kind,
            at,
            height: MM,
            layer: layer.into(),
            orient: Orient::default(),
            hide: false,
        }],
        courtyard: None,
        class: None,
    }
}

/// The bbox over every feature's shape (features must be non-empty).
fn text_bbox(feats: &[geom::Feature]) -> (Point, Point) {
    let mut lo = Point {
        x: Nm::MAX,
        y: Nm::MAX,
    };
    let mut hi = Point {
        x: Nm::MIN,
        y: Nm::MIN,
    };
    for f in feats {
        let (s, _) = prism_shape_z(f);
        let (a, b) = s.bbox().expect("a bounded shape");
        lo.x = lo.x.min(a.x);
        lo.y = lo.y.min(a.y);
        hi.x = hi.x.max(b.x);
        hi.y = hi.y.max(b.y);
    }
    (lo, hi)
}

/// A `Reference` anchor renders the *annotated* refdes wired through
/// [`crate::annotate::refdes`] — proven by matching the geometry of a `Literal("R1")`
/// anchor at the same placement (geometry, not string, since strokes are geometry).
#[test]
fn text_features_reference_renders_annotated_refdes() {
    let su = Stackup::default_2layer();
    let def = text_part(
        "R_0402",
        FpTextKind::Reference,
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    let c = comp("R_0402", Point { x: 0, y: 0 }, Orient::default());

    let mut doc = crate::doc::Doc::default();
    doc.components.insert(c.id.clone(), c.clone());
    let mut lib = PartLib::new();
    lib.insert("R_0402".into(), def.clone());
    let reg = crate::annotate::registry(&[]);
    let refdes = crate::annotate::refdes(&doc, &lib, &reg)[&c.id].clone();
    assert_eq!(refdes, "R1");

    let got = text_features(&def, &c, &su, &refdes, "", None);
    let lit = text_part(
        "R_0402",
        FpTextKind::Literal("R1".into()),
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    assert!(!got.is_empty());
    assert_eq!(got, text_features(&lit, &c, &su, "", "", None));
}

/// A `Label` anchor renders through the class registry template: `value = 4.7k` under
/// a `{value:iec}` template → `4k7`, matching a `Literal("4k7")` anchor's geometry.
#[test]
fn text_features_label_renders_registry_template() {
    use crate::elaborate::GenDirective;
    let su = Stackup::default_2layer();
    let def = text_part("R_0402", FpTextKind::Label, Point { x: 0, y: 0 }, "F.SilkS");
    let mut c = comp("R_0402", Point { x: 0, y: 0 }, Orient::default());
    c.params.insert("value".into(), "4.7k".into());
    let reg = crate::annotate::registry(&[GenDirective::Class {
        name: "R".into(),
        entry: crate::annotate::ClassEntry {
            prefix: None,
            template: Some("{value:iec}".into()),
            defaults: BTreeMap::new(),
        },
    }]);
    let lbl = crate::annotate::label(&c, &def, &reg);
    assert_eq!(lbl, "4k7");

    let got = text_features(&def, &c, &su, "", &lbl, None);
    let lit = text_part(
        "R_0402",
        FpTextKind::Literal("4k7".into()),
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    assert!(!got.is_empty());
    assert_eq!(got, text_features(&lit, &c, &su, "", "", None));
}

/// The lowered geometry is **live**: a params edit re-renders the label to different
/// strokes (nothing is baked at import).
#[test]
fn text_features_are_live_to_params() {
    let su = Stackup::default_2layer();
    let def = text_part("R_0402", FpTextKind::Label, Point { x: 0, y: 0 }, "F.SilkS");
    let reg = crate::annotate::registry(&[]); // built-in R seed: `{value}` verbatim
    let mut a = comp("R_0402", Point { x: 0, y: 0 }, Orient::default());
    a.params.insert("value".into(), "100".into());
    let mut b = comp("R_0402", Point { x: 0, y: 0 }, Orient::default());
    b.params.insert("value".into(), "220".into());
    let ga = text_features(
        &def,
        &a,
        &su,
        "",
        &crate::annotate::label(&a, &def, &reg),
        None,
    );
    let gb = text_features(
        &def,
        &b,
        &su,
        "",
        &crate::annotate::label(&b, &def, &reg),
        None,
    );
    assert!(!ga.is_empty());
    assert_ne!(ga, gb, "different params → different lowered strokes");
}

/// A bottom-side component's text is mirrored and lands on the `B.*` slab — both
/// falling out of the component quaternion (no special-case code). NOTE: this crate's
/// flip ([`Orient::flipped`]) is a 180° rotation about the in-plane y-axis, so the
/// in-plane mirror is an **x-negation** (y unchanged), matching the KiCad/fab
/// board-turn convention. Anchored at `+2mm` in x, the top text sits right of the axis
/// and the bottom text left — an observable reflection.
#[test]
fn text_features_bottom_side_mirrors_and_swaps_slab() {
    let su = Stackup::default_2layer();
    let def = text_part(
        "R",
        FpTextKind::Literal("LR".into()),
        Point { x: 2 * MM, y: 0 },
        "F.SilkS",
    );
    let top = comp("R", Point { x: 0, y: 0 }, Orient::default());
    let bot = comp("R", Point { x: 0, y: 0 }, Orient::default().flipped());
    let tf = text_features(&def, &top, &su, "", "", None);
    let bf = text_features(&def, &bot, &su, "", "", None);
    assert!(!tf.is_empty());
    assert_eq!(tf.len(), bf.len());

    let (tlo, thi) = text_bbox(&tf);
    let (blo, bhi) = text_bbox(&bf);
    assert_eq!(
        (blo.y, bhi.y),
        (tlo.y, thi.y),
        "y unchanged by the y-axis flip"
    );
    assert_eq!(
        (blo.x, bhi.x),
        (-thi.x, -tlo.x),
        "x mirrored across the y-axis"
    );
    assert!(tlo.x > 0, "top text is right of the axis (anchor +2mm)");

    assert_eq!(prism_shape_z(&tf[0]).1, su.slab_z("F.SilkS").unwrap());
    assert_eq!(
        prism_shape_z(&bf[0]).1,
        su.slab_z("B.SilkS").unwrap(),
        "flipped text → B.SilkS"
    );
}

/// End-to-end outline-font footprint text on the **bottom** side: an `O` (a glyph with
/// a counter) lowers to a filled `Area` marking that rides the same `to_world`
/// reflection as copper — and its winding survives. The outer ring stays CCW
/// (positive), the counter stays CW (negative): the hole reads through as a hole (an
/// island stays an island) rather than flipping to a solid blob under the flip.
#[test]
fn text_features_ttf_bottom_side_keeps_counter_a_hole() {
    let su = Stackup::default_2layer();
    let font = crate::font::TtfFont::from_bytes(crate::ttf::build_test_ttf()).unwrap();
    let def = text_part(
        "R",
        FpTextKind::Literal("O".into()),
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    let bot = comp("R", Point { x: 0, y: 0 }, Orient::default().flipped());
    let feats = text_features(&def, &bot, &su, "", "", Some(&font));
    assert_eq!(feats.len(), 1, "one Area glyph");
    let (shape, z) = prism_shape_z(&feats[0]);
    assert_eq!(z, su.slab_z("B.SilkS").unwrap(), "flipped → B.SilkS");
    let Shape2D::Area { region } = shape else {
        panic!("outline text lowers to a filled Area, got {shape:?}");
    };
    assert_eq!(region.rings.len(), 2, "outer + counter");
    assert!(
        crate::region::signed_area2(&region.rings[0]) > 0,
        "outer stays CCW after the bottom-side reflection"
    );
    assert!(
        crate::region::signed_area2(&region.rings[1]) < 0,
        "counter stays CW (a hole) after the reflection"
    );
}

/// Footprint text is centre-anchored (unlike left-origin board text): a 2-char run
/// centres its **ink extent** on the anchor origin — the bbox centre lands on the
/// anchor to within integer rounding (well under one stroke width) on both axes, not
/// biased left by the advance box's trailing inter-glyph gap.
#[test]
fn text_features_center_justification_centers_on_anchor() {
    let su = Stackup::default_2layer();
    let def = text_part(
        "R",
        FpTextKind::Literal("II".into()),
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    let c = comp("R", Point { x: 0, y: 0 }, Orient::default());
    let (lo, hi) = text_bbox(&text_features(&def, &c, &su, "", "", None));
    let cx = (lo.x + hi.x) / 2;
    let cy = (lo.y + hi.y) / 2;
    let pen = MM / 8; // one stroke width
    assert!(
        cx.abs() < pen,
        "horizontally centred on the anchor: cx={cx}"
    );
    assert!(cy.abs() < pen, "vertically centred on the anchor: cy={cy}");
}

/// A hidden anchor emits nothing; text on a slab absent from the stackup is skipped
/// (no panic) — both mirroring `graphic_features`' skips.
#[test]
fn text_features_hide_and_missing_slab_emit_nothing() {
    let su = Stackup::default_2layer();
    let c = comp("R", Point { x: 0, y: 0 }, Orient::default());
    let mut hidden = text_part(
        "R",
        FpTextKind::Literal("X".into()),
        Point { x: 0, y: 0 },
        "F.SilkS",
    );
    hidden.texts[0].hide = true;
    assert!(text_features(&hidden, &c, &su, "", "", None).is_empty());

    let no_slab = text_part(
        "R",
        FpTextKind::Literal("X".into()),
        Point { x: 0, y: 0 },
        "F.Fab", // not in the default stackup
    );
    assert!(text_features(&no_slab, &c, &su, "", "", None).is_empty());
}
