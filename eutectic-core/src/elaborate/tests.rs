use super::*;
// The elaborate facade no longer imports these into its own scope (they migrated to the
// `query`/`support` submodules with the code that uses them), so `super::*` no longer
// re-exports them; the tests name them directly.
use crate::geom::{Extent, NetFeature, Slab, Stackup, ZRange};
use crate::part::PartDef;

fn pt(x: Nm, y: Nm) -> Point {
    Point { x, y }
}

#[test]
fn ring_places_instances_around_a_circle_facing_outward() {
    // 12 side-firing LEDs on a 10 mm-radius ring — the arbitrary-angle case.
    let s = ring("led", "LED", pt(0, 0), 10_000_000, 12);
    assert_eq!(s.len(), 36, "12 × (Instance, Place, Rotate)");
    // Pull the (Place, Rotate) for a given index.
    let place_of = |i: usize| {
        s.iter().find_map(|d| match d {
            GenDirective::Place { path, pos } if path == &format!("led[{i}]") => Some(*pos),
            _ => None,
        })
    };
    let rot_of = |i: usize| {
        s.iter().find_map(|d| match d {
            GenDirective::Rotate { path, orient } if path == &format!("led[{i}]") => Some(*orient),
            _ => None,
        })
    };
    // led[0] at angle 0 → east point, 0°. led[3] at 90° → north, ≈90°. led[6] →
    // west, ≈180°. All exactly on the ring (positions rounded to nm).
    assert_eq!(place_of(0).unwrap(), pt(10_000_000, 0));
    assert_eq!(rot_of(0).unwrap().to_deg(), 0);
    assert_eq!(place_of(3).unwrap(), pt(0, 10_000_000));
    assert_eq!(rot_of(3).unwrap().to_deg(), 90);
    assert_eq!(rot_of(6).unwrap().to_deg(), 180);
    // 30° (= 360/12) is off-axis: led[1] is a real quaternion, not a cardinal.
    assert_eq!(rot_of(1).unwrap().to_deg(), 30);
    assert!(!rot_of(1).unwrap().is_bottom());
}

/// Board + cutout + a Top conductor region: `features()` (the source-only geometry
/// query) lowers exactly one Substrate (an `Area` whose cutout is a *hole*, not a
/// separate Void — Decision 16b/c) and two mask solids. A **Conductor** region is a
/// copper pour: its filled `Area` needs the placed copper to knock out, so it is
/// lowered by [`crate::route::world_features`], not here — `features()` still
/// *validates* the pour's slab (this call succeeds) but emits no conductor.
#[test]
fn features_lowers_board_cutout_and_region() {
    let su = Stackup::default_2layer();
    let src = vec![
        board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
        GenDirective::Cutout {
            shape: Shape2D::rect(pt(5 * MM, 5 * MM), MM, MM),
        },
        GenDirective::Region(RegionDecl {
            shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
    ];

    let feats = features(&src).unwrap();
    // one substrate + two mask solids (F/B.Mask in the default stackup). The cutout is
    // a hole in the substrate Area — no separate Void; the pour is lowered elsewhere.
    assert_eq!(
        feats.len(),
        3,
        "substrate + 2 masks (cutout is a hole; the pour lowers in world_features)"
    );

    let subs: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Substrate)
        .collect();
    assert_eq!(subs.len(), 1, "exactly one substrate feature");
    assert!(subs[0].net.is_none(), "substrate is netless");
    let Extent::Prism { shape, z } = &subs[0].feature.extent;
    assert_eq!(*z, su.board_z().unwrap(), "substrate spans the board body");
    // The substrate is an `Area` (outline ∖ cutout): its region has the outer ring
    // plus one hole (the cutout), and the cutout centre is outside the filled area.
    let region = shape.region().expect("substrate is a Shape2D::Area");
    assert_eq!(region.rings.len(), 2, "outer boundary + one cutout hole");
    assert!(region.contains_point(pt(MM, MM)), "board body is filled");
    assert!(
        !region.contains_point(pt(5 * MM, 5 * MM)),
        "the cutout is a hole, not filled"
    );

    assert!(
        !feats.iter().any(|f| f.feature.role == Role::Void),
        "a board cutout is a hole in the substrate Area, not a Void feature"
    );
    assert!(
        !feats.iter().any(|f| f.feature.role == Role::Conductor),
        "a Conductor pour is lowered by world_features, not the source-only features()"
    );
}

/// Every `Role::Mask` slab in the stackup yields exactly one solid mask `Feature`
/// with the board-outline shape at that slab's z, carrying the slab's material
/// (Decision 13 — mask is a generated positive solid, not a negative layer). The
/// default stackup has two mask slabs (F/B.Mask), so a board generates two solids;
/// a boardless source generates none (no board area to cover).
#[test]
fn features_generates_one_mask_solid_per_mask_slab() {
    let su = Stackup::default_2layer();
    let outline = Shape2D::rect(pt(0, 0), 8 * MM, 6 * MM);
    let src = vec![GenDirective::Board {
        outline: outline.clone(),
    }];

    let feats = features(&src).unwrap();
    let masks: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Mask)
        .collect();

    let mask_slabs: Vec<&Slab> = su.slabs.iter().filter(|s| s.role == Role::Mask).collect();
    assert_eq!(mask_slabs.len(), 2, "default stackup has F.Mask + B.Mask");
    assert_eq!(masks.len(), 2, "one mask solid per mask slab");
    assert!(masks.iter().all(|f| f.net.is_none()), "mask is netless");

    // Each solid is the board region (as an `Area`) at its slab's z, with the slab's
    // material. No cutouts here, so the region is just the outline.
    let expected = board_region(&src).unwrap();
    for slab in &mask_slabs {
        let m = masks
            .iter()
            .find(|f| matches!(f.feature.extent, Extent::Prism { z, .. } if z == slab.z))
            .unwrap_or_else(|| panic!("a mask solid at {:?}", slab.z));
        let Extent::Prism { shape, .. } = &m.feature.extent;
        assert_eq!(
            shape.region(),
            Some(&expected),
            "mask solid is the board region"
        );
        assert_eq!(
            m.feature.material, slab.material,
            "carries the slab material"
        );
    }

    // No `Board` ⇒ no board area ⇒ no mask solids.
    let boardless = features(&vec![]).unwrap();
    assert!(
        !boardless.iter().any(|f| f.feature.role == Role::Mask),
        "a boardless source generates no mask"
    );
}

/// A custom stackup with no `Role::Mask` slab generates no mask solids (no special
/// cases — the generator simply finds nothing to emit).
#[test]
fn features_no_mask_slab_generates_no_mask() {
    // A minimal 1-copper-slab stackup: no mask, no silk.
    let src: Source = vec![GenDirective::Slab(Slab {
        name: "F.Cu".into(),
        z: ZRange::new(0, 35_000),
        role: Role::Conductor,
        material: Some(crate::geom::Material::named("copper")),
    })]
    .into_iter()
    .chain(std::iter::once(GenDirective::Board {
        outline: Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
    }))
    .collect();
    let feats = features(&src).unwrap();
    assert!(
        !feats.iter().any(|f| f.feature.role == Role::Mask),
        "no mask slab ⇒ no mask solid"
    );
}

/// Two `Board` directives: only the last outline becomes the substrate feature
/// (mirrors `board_region`'s "last `Board` wins").
#[test]
fn features_last_board_wins() {
    let first = Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM);
    let last = Shape2D::rect(pt(0, 0), 8 * MM, 8 * MM);
    let src = vec![
        GenDirective::Board {
            outline: first.clone(),
        },
        GenDirective::Board {
            outline: last.clone(),
        },
    ];

    let feats = features(&src).unwrap();
    let subs: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Substrate)
        .collect();
    assert_eq!(subs.len(), 1, "only one substrate emitted");
    let Extent::Prism { shape, .. } = &subs[0].feature.extent;
    // The substrate Area is the LAST board's region: it fills out to ±4 mm (the 8 mm
    // board) but the earlier 4 mm board's corner at (±2, ±2) is interior to it, and a
    // point past the 4 mm board (e.g. (3 mm, 0)) is still on the board — proving the
    // larger last outline won.
    let region = shape.region().expect("substrate is a Shape2D::Area");
    assert_eq!(*region, board_region(&src).unwrap());
    assert!(
        region.contains_point(pt(3 * MM, 0)),
        "the LAST (8 mm) board won"
    );
}

/// A `text` directive lowers to several `Role::Marking` stroke features sitting on
/// the named silk slab's **honest z** (not copper z — Decision 13), advancing in +x
/// across the string (Decision 9).
#[test]
fn features_lowers_text_to_marking_strokes() {
    let su = Stackup::default_2layer();
    let src = vec![GenDirective::Text {
        string: "R12".into(),
        at: pt(0, 0),
        height: MM,
        layer: "F.SilkS".into(),
        orient: Orient::IDENTITY,
    }];

    let feats = features(&src).unwrap();
    let marks: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Marking)
        .collect();
    // "R12": R(2) + 1(2) + 2(1) = 5 strokes; in any case several, all netless.
    assert!(
        marks.len() >= 3,
        "expected several marking strokes, got {}",
        marks.len()
    );
    assert!(marks.iter().all(|f| f.net.is_none()), "silk is netless");

    // All markings sit on the F.SilkS slab's honest z — above the top copper, not
    // aliased onto it (the pre-Decision-13 stopgap).
    let silk_z = su.slab_z("F.SilkS").unwrap();
    assert_ne!(
        silk_z,
        su.top_copper().unwrap(),
        "silk z is distinct from copper z"
    );
    for m in &marks {
        let Extent::Prism { z, .. } = m.feature.extent;
        assert_eq!(z, silk_z, "marking on the F.SilkS z");
    }

    // The text advances in +x: the rightmost stroke point of the 3-char string
    // lies well to the right of the origin (the '1' and '2' are advanced glyphs).
    let max_x = marks
        .iter()
        .flat_map(|m| {
            let Extent::Prism { shape, .. } = &m.feature.extent;
            shape.points().into_iter().map(|p| p.x)
        })
        .max()
        .unwrap();
    assert!(max_x > MM, "string advances past the first glyph in +x");
}

/// Write the test TTF fixture to a unique temp path (removed by the caller). Board and
/// footprint lowering resolve fonts by *path*, so an end-to-end test needs a file.
fn write_fixture_font() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("eutectic-test-{}-{stamp}.ttf", std::process::id()));
    std::fs::write(&p, crate::ttf::build_test_ttf()).unwrap();
    p
}

/// With a `font` directive resolving to a real file, board text lowers to filled
/// `Area` markings (outline glyphs) instead of stroke traces — and the font loads
/// cleanly (no diagnostic).
#[test]
fn features_ttf_font_lowers_text_to_area_markings() {
    let path = write_fixture_font();
    let src = vec![
        GenDirective::Font {
            path: path.to_string_lossy().into_owned(),
        },
        GenDirective::Text {
            string: "HOo".into(),
            at: pt(0, 0),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        },
    ];
    let feats = features(&src).unwrap();
    let marks: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Marking)
        .collect();
    assert_eq!(marks.len(), 3, "one Area per inked glyph (H, O, o)");
    for m in &marks {
        let Extent::Prism { shape, .. } = &m.feature.extent;
        assert!(
            matches!(shape, Shape2D::Area { .. }),
            "outline text is a filled Area, got {shape:?}"
        );
    }
    assert_eq!(
        font_load_failure(&src),
        None,
        "a loadable font records no failure"
    );
    std::fs::remove_file(&path).ok();
}

/// A `font` directive pointing at a missing file **degrades** to the stroke font
/// (board text still lowers, as `Stroke` traces — the doc does not fail), and the
/// failure surfaces on the [`ReconReport`] as a non-blocking `W_FONT_LOAD` warning
/// that leaves the doc `is_clean`.
#[test]
fn missing_font_degrades_to_stroke_with_warning() {
    use crate::diagnostic::Diagnose;
    let src = vec![
        GenDirective::Font {
            path: "/no/such/font/file.ttf".into(),
        },
        GenDirective::Text {
            string: "R12".into(),
            at: pt(0, 0),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        },
    ];
    // Rendering must not fail; it falls back to the stroke font (traced polylines).
    let feats = features(&src).unwrap();
    let marks: Vec<&NetFeature> = feats
        .iter()
        .filter(|f| f.feature.role == Role::Marking)
        .collect();
    assert!(marks.len() >= 3, "stroke fallback still lowers the text");
    for m in &marks {
        let Extent::Prism { shape, .. } = &m.feature.extent;
        assert!(
            matches!(shape, Shape2D::Stroke { .. }),
            "degraded to stroke traces, got {shape:?}"
        );
    }
    // Elaboration succeeds; the failure rides on the report as a warning that does
    // NOT dirty the doc (Decision 17 degrade-never-fail).
    let el = elaborate(&src, &BTreeMap::new(), &BTreeMap::new(), &PartLib::new())
        .expect("elaborates despite the bad font");
    assert!(el.report.font_load_failure.is_some());
    assert!(el.report.is_clean(), "a font degrade keeps the doc clean");
    let diags = el.report.diagnostics();
    let w = diags
        .iter()
        .find(|d| d.code == "W_FONT_LOAD")
        .expect("a W_FONT_LOAD diagnostic");
    assert!(!w.is_error(), "font load failure is a warning");
}

/// Issue 0024: an outer copper side with no mask slab — while the stackup carries a
/// mask elsewhere — surfaces as a non-blocking `W_COPPER_NO_MASK` warning that leaves
/// the doc `is_clean`. A fully-masked board is silent; a deliberately maskless board
/// (zero mask slabs) is silent too.
#[test]
fn unmasked_copper_warns_but_stays_clean() {
    use crate::diagnostic::Diagnose;
    let cu = |name: &str, lo, hi| {
        GenDirective::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role: Role::Conductor,
            material: None,
        })
    };
    let mask = |name: &str, lo, hi| {
        GenDirective::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role: Role::Mask,
            material: None,
        })
    };
    let ov = BTreeMap::new();
    let rp = BTreeMap::new();

    // Default stackup (both masks) → no warning.
    let src: Source = vec![];
    let el = elaborate(&src, &ov, &rp, &PartLib::new()).unwrap();
    assert!(
        el.report.unmasked_copper.is_empty(),
        "default board fully masked"
    );

    // F.Mask only, both copper → the bottom copper side is unmasked.
    let f_mask_only: Source = vec![
        cu("F.Cu", 1_965_000, 2_000_000),
        cu("B.Cu", 0, 35_000),
        mask("F.Mask", 2_000_000, 2_010_000),
    ];
    let el = elaborate(&f_mask_only, &ov, &rp, &PartLib::new()).unwrap();
    assert_eq!(el.report.unmasked_copper, vec!["B.Cu".to_string()]);
    assert!(
        el.report.is_clean(),
        "an unmasked-copper warning keeps the doc clean"
    );
    let diags = el.report.diagnostics();
    let w = diags
        .iter()
        .find(|d| d.code == "W_COPPER_NO_MASK")
        .expect("a W_COPPER_NO_MASK diagnostic");
    assert!(!w.is_error(), "unmasked copper is a warning");
    assert!(
        w.message.contains("B.Cu"),
        "the message names the slab: {}",
        w.message
    );

    // Zero mask slabs anywhere → deliberately maskless, silent.
    let bare: Source = vec![cu("F.Cu", 1_965_000, 2_000_000), cu("B.Cu", 0, 35_000)];
    let el = elaborate(&bare, &ov, &rp, &PartLib::new()).unwrap();
    assert!(
        el.report.unmasked_copper.is_empty(),
        "bare-copper board is silent"
    );
    assert!(
        !el.report
            .diagnostics()
            .iter()
            .any(|d| d.code == "W_COPPER_NO_MASK"),
        "no warning for a deliberately maskless board"
    );
}

/// An unknown slab name is a hard elaboration error (no silent board-z fallback,
/// Decision 13); the message names the unknown slab and the available names.
#[test]
fn features_unknown_slab_name_is_hard_error() {
    let src = vec![
        board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
        GenDirective::Region(RegionDecl {
            shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
            role: Role::Keepout(crate::geom::KeepoutKind::Copper),
            net: None,
            layer: "Q.Cu".into(),
        }),
    ];
    let err = features(&src).unwrap_err();
    assert!(err.contains("Q.Cu"), "names the unknown slab: {err}");
    assert!(err.contains("F.Cu"), "lists available slabs: {err}");

    // A text label on an unknown slab is likewise a hard error.
    let src = vec![GenDirective::Text {
        string: "X".into(),
        at: pt(0, 0),
        height: MM,
        layer: "Nope".into(),
        orient: Orient::IDENTITY,
    }];
    assert!(features(&src).unwrap_err().contains("Nope"));
}

/// A net-bound `Conductor` region on a non-copper slab (silk) is nonsense and is
/// rejected by the materialization gate (Decision 13).
#[test]
fn features_conductor_pour_on_non_copper_slab_errors() {
    let src = vec![
        board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
        GenDirective::Region(RegionDecl {
            shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.SilkS".into(),
        }),
    ];
    let err = features(&src).unwrap_err();
    assert!(
        err.contains("F.SilkS") && err.contains("non-copper"),
        "rejects a pour on silk: {err}"
    );
}

/// A source with `Slab` directives makes `stackup()` return *those* slabs, in
/// declaration order — not the 2-layer default.
#[test]
fn stackup_reads_authored_slabs() {
    // A non-default 2 mm board (distinct z's from `default_2layer`), with the middle
    // dielectric left material-less to also exercise the optional-material path.
    let authored = vec![
        Slab {
            name: "B.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: Some(crate::geom::Material::named("copper")),
        },
        Slab {
            name: "core".into(),
            z: ZRange::new(35_000, 1_965_000),
            role: Role::Substrate,
            material: None,
        },
        Slab {
            name: "F.Cu".into(),
            z: ZRange::new(1_965_000, 2_000_000),
            role: Role::Conductor,
            material: Some(crate::geom::Material::named("copper")),
        },
    ];
    let src: Source = authored.iter().cloned().map(GenDirective::Slab).collect();
    let su = stackup(&src);
    assert_eq!(
        su.slabs, authored,
        "stackup() returns the authored slabs verbatim"
    );
    assert_ne!(
        su,
        Stackup::default_2layer(),
        "authored slabs are not the default (distinct z's)"
    );
}

/// With no `Slab` directives, `stackup()` falls back to the unchanged 2-layer
/// default — even when the source has other (non-slab) directives.
#[test]
fn stackup_defaults_when_no_slabs() {
    assert_eq!(stackup(&vec![]), Stackup::default_2layer());
    let src = vec![board_rect(pt(0, 0), pt(10 * MM, 10 * MM))];
    assert_eq!(
        stackup(&src),
        Stackup::default_2layer(),
        "non-slab directives don't disturb the default"
    );
}

// ---- refdes-pin reconciliation ----

fn part_lib(name: &str) -> PartLib {
    let mut lib = PartLib::new();
    lib.insert(
        name.to_string(),
        PartDef {
            name: name.to_string(),
            pins: vec![],
            interfaces: BTreeMap::new(),
            graphics: vec![],
            texts: vec![],
            courtyard: None,
            class: None,
        },
    );
    lib
}

fn inst(path: &str, part: &str) -> GenDirective {
    GenDirective::Instance {
        path: path.to_string(),
        part: part.to_string(),
        params: BTreeMap::new(),
        label: None,
    }
}

/// Two entities pinned to one identical string surface as an `E_REFDES_PIN_DUP`
/// finding on an otherwise-valid elaboration (non-blocking, like pos findings).
#[test]
fn duplicate_refdes_pin_is_surfaced() {
    let src = vec![inst("c0", "C"), inst("c1", "C")];
    let mut pins = BTreeMap::new();
    pins.insert(EntityId::new("c0"), "C7".to_string());
    pins.insert(EntityId::new("c1"), "C7".to_string());
    let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
    assert_eq!(
        elab.report.refdes_pin_dups,
        vec![(
            "C7".to_string(),
            vec![EntityId::new("c0"), EntityId::new("c1")]
        )]
    );
    assert!(!elab.report.is_clean());
    // Distinct pins do not collide.
    let mut ok = BTreeMap::new();
    ok.insert(EntityId::new("c0"), "C7".to_string());
    ok.insert(EntityId::new("c1"), "C8".to_string());
    let clean = elaborate(&src, &BTreeMap::new(), &ok, &part_lib("C")).expect("elaborates");
    assert!(clean.report.refdes_pin_dups.is_empty());
}

/// A refdes pin on an entity that does not exist after elaboration is orphaned —
/// the same channel and behavior as a stale position override.
#[test]
fn refdes_pin_on_unknown_id_is_orphaned() {
    let src = vec![inst("c0", "C")];
    let mut pins = BTreeMap::new();
    pins.insert(EntityId::new("ghost"), "C9".to_string());
    let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
    assert!(elab.report.orphaned.contains(&EntityId::new("ghost")));
}

/// An entity carrying BOTH a pos override and a refdes pin, orphaned, is flagged
/// exactly once (the refdes-orphan loop dedups against the pos-orphan loop).
#[test]
fn orphan_with_both_pos_override_and_refdes_pin_is_flagged_once() {
    let src = vec![inst("c0", "C")];
    let ghost = EntityId::new("ghost");
    let mut overrides = BTreeMap::new();
    overrides.insert(
        ghost.clone(),
        Override {
            pos: Some(Point { x: 1, y: 2 }),
            strength: Strength::Pin,
        },
    );
    let mut pins = BTreeMap::new();
    pins.insert(ghost.clone(), "C9".to_string());
    let elab = elaborate(&src, &overrides, &pins, &part_lib("C")).expect("elaborates");
    assert_eq!(
        elab.report
            .orphaned
            .iter()
            .filter(|&id| *id == ghost)
            .count(),
        1,
        "orphan reported once despite two override kinds"
    );
}

/// Issue 0019 (review): an imported courtyard with an outward-bowing arc edge must
/// be covered by the convex hull the solver reserves. The arc apex is not a corner,
/// so a corners-only lowering ([`Shape2D::points`]) would drop the bulge and
/// under-reserve; `component_courtyard` flattens the arc to chords and hulls that, so
/// the bulge lands inside the reserved polygon.
#[test]
fn component_courtyard_covers_an_arc_bulge() {
    use crate::geom::{Path, Seg, Shape2D};
    // Bottom edge (−1,0)→(1,0), then an arc bowing up through (0, 2 mm) back to the
    // start. (0, 2 mm) is the arc mid, not a corner: corners give max-y 0.
    let path = Path {
        start: pt(-1_000_000, 0),
        segs: vec![
            Seg::Line {
                end: pt(1_000_000, 0),
            },
            Seg::Arc {
                mid: pt(0, 2_000_000),
                end: pt(-1_000_000, 0),
            },
        ],
    };
    let def = PartDef {
        name: "ARC".into(),
        pins: Vec::new(),
        interfaces: BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: Some(Shape2D::polygon_path(path, 0)),
        class: None,
    };
    let (verts, _r) =
        component_courtyard(&def, Orient::IDENTITY).expect("arc courtyard has a hull");
    let max_y = verts.iter().map(|p| p.y).max().unwrap();
    assert!(
        max_y > 1_500_000,
        "the arc bulge (~2 mm) must be inside the reserved hull, got max-y {max_y}"
    );
}
