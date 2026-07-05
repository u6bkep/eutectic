use super::*;
use crate::geom::Seg;

// `pin_role`/`pin_offset` now resolve a *stored identity* (pad number). These
// helpers verify the join by functional **name** — finding the PinDef directly —
// which is what these tests mean to check (the symbol's role landed on the named
// pin). User-facing name→pad resolution is exercised via `resolve_selector`.
fn role_of(part: &PartDef, name: &str) -> Option<PinRole> {
    part.pins.iter().find(|p| p.name == name).map(|p| p.role)
}
fn offset_of(part: &PartDef, name: &str) -> Option<Point> {
    part.pins.iter().find(|p| p.name == name).map(|p| p.offset)
}

/// A self-contained footprint modelled on a real JST-SH 1x03 vertical header
/// (`JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical`): three signal pads, two
/// shared `MP` mounting pads, plus an unnamed exposed pad — trimmed of
/// silkscreen/courtyard/3D noise but structurally faithful (nested parens,
/// quoted name, multi-line pads).
const JST_SH_1X03: &str = r#"
(footprint "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical"
    (version 20241229)
    (generator "pcbnew")
    (layer "F.Cu")
    (descr "JST SH series connector (with parens) http://example.com")
    (attr smd)
    (fp_line
        (start -2.61 -0.04)
        (end -2.61 1.11)
        (stroke (width 0.12) (type solid))
        (layer "F.SilkS")
    )
    (pad "1" smd roundrect
        (at -1 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
        (roundrect_rratio 0.25)
    )
    (pad "2" smd roundrect
        (at 0 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "3" smd roundrect
        (at 1 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "MP" smd roundrect
        (at -2.3 -1.2)
        (size 1.2 1.8)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "MP" smd roundrect
        (at 2.3 -1.2)
        (size 1.2 1.8)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "" smd roundrect
        (at 0 0)
        (size 0.3 0.3)
        (layers "F.Cu")
    )
    (model "${KICAD9_3DMODEL_DIR}/Connector_JST.3dshapes/x.step"
        (offset (xyz 0 0 0))
        (scale (xyz 1 1 1))
    )
)
"#;

#[test]
fn imports_jst_sh_name_and_pad_count() {
    let p = import_footprint(JST_SH_1X03).unwrap();
    assert_eq!(p.name, "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical");
    // 1,2,3 + one deduped MP = 4; the two `MP` collapse, the `""` pad is skipped.
    assert_eq!(p.pins.len(), 4);
    let names: Vec<&str> = p.pins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["1", "2", "3", "MP"]);
    // No footprint carries electrical roles or interfaces.
    assert!(p.pins.iter().all(|pin| pin.role == PinRole::Passive));
    assert!(p.interfaces.is_empty());
}

#[test]
fn imports_jst_sh_pad_offsets_in_nm() {
    let p = import_footprint(JST_SH_1X03).unwrap();
    // pad "1" at (-1, 1.325) mm
    assert_eq!(
        p.pin_offset("1"),
        Some(Point {
            x: -1_000_000,
            y: 1_325_000
        })
    );
    // pad "3" at (1, 1.325) mm
    assert_eq!(
        p.pin_offset("3"),
        Some(Point {
            x: 1_000_000,
            y: 1_325_000
        })
    );
    // first MP wins: (-2.3, -1.2) mm
    assert_eq!(
        p.pin_offset("MP"),
        Some(Point {
            x: -2_300_000,
            y: -1_200_000
        })
    );
}

#[test]
fn captures_pad_geometry() {
    let p = import_footprint(JST_SH_1X03).unwrap();
    // pad "1": roundrect 0.6 x 1.55 mm → a single Polygon copper region whose
    // bbox (radius included) is the full pad size, on the top layer.
    let pad1 = p
        .pins
        .iter()
        .find(|pin| pin.name == "1")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    assert_eq!(pad1.copper.len(), 1);
    assert!(matches!(pad1.copper[0].shape, Shape2D::Polygon { .. }));
    assert_eq!(pad1.copper[0].layers, PadLayers::Top);
    let (min, max) = pad1.copper[0].shape.bbox().unwrap();
    assert_eq!((max.x - min.x, max.y - min.y), (600_000, 1_550_000));
    // A rect pad (FP_4) captures a rectangle of its size.
    let r = import_footprint(FP_4).unwrap();
    let a1 = r
        .pins
        .iter()
        .find(|pin| pin.name == "1")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    let (min, max) = a1.copper[0].shape.bbox().unwrap();
    assert_eq!((max.x - min.x, max.y - min.y), (500_000, 500_000));
    // Geometry rides through the symbol/footprint join (footprint is the source).
    let joined = import_part(SYM_LIB, FP_4).unwrap();
    let vdd = joined.pins.iter().find(|pin| pin.name == "VDD").unwrap();
    let (min, max) = vdd.pad.clone().unwrap().copper[0].shape.bbox().unwrap();
    assert_eq!((max.x - min.x, max.y - min.y), (500_000, 500_000));
}

#[test]
fn imports_through_hole_drill_oval_and_rotation() {
    let src = r#"
(footprint "X"
  (pad "1" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8) (layers "*.Cu"))
  (pad "2" smd oval (at 3 0) (size 2 1) (layers "F.Cu"))
  (pad "3" smd rect (at 6 0 90) (size 2 1) (layers "B.Cu"))
)"#;
    let p = import_footprint(src).unwrap();

    // Through-hole round pad: copper spans all layers, a round drill, disc copper.
    let p1 = p
        .pins
        .iter()
        .find(|x| x.name == "1")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    assert_eq!(p1.copper[0].layers, PadLayers::Through);
    assert_eq!(p1.drill, Some(Drill::Round { d: 800_000 }));
    // A disc is a lone-point stroke: start, no segments.
    assert!(matches!(&p1.copper[0].shape, Shape2D::Stroke { path, .. } if path.segs.is_empty()));

    // Oval pad → a capsule (one-segment stroke) on the top layer.
    let p2 = p
        .pins
        .iter()
        .find(|x| x.name == "2")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    assert!(matches!(&p2.copper[0].shape, Shape2D::Stroke { path, .. } if path.segs.len() == 1));
    assert_eq!(p2.copper[0].layers, PadLayers::Top);
    assert_eq!(p2.drill, None);

    // Rect rotated 90°: a 2×1 pad's bbox becomes 1 wide × 2 tall; bottom layer.
    let p3 = p
        .pins
        .iter()
        .find(|x| x.name == "3")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    assert_eq!(p3.copper[0].layers, PadLayers::Bottom);
    let (min, max) = p3.copper[0].shape.bbox().unwrap();
    assert_eq!((max.x - min.x, max.y - min.y), (1_000_000, 2_000_000));
}

/// A custom pad is imported as the union of its anchor + `(primitives …)` — circle,
/// polygon, and a 3-point `gr_arc` (the modern KiCad encoding) — instead of the old
/// bounding-box collapse. Coordinates are pad-local (offset by the pad `(at)`).
#[test]
fn imports_custom_pad_primitives_as_compound_copper() {
    let src = r#"
(footprint "CUSTOM"
  (pad "1" smd custom (at 1 2) (size 0.3 0.3) (layers "F.Cu")
    (options (clearance outline) (anchor rect))
    (primitives
      (gr_circle (center 0 0.5) (end 0.2 0.5) (width 0) (fill yes))
      (gr_poly (pts (xy 0 0) (xy 0.4 0) (xy 0.4 0.4)) (width 0) (fill yes))
      (gr_arc (start 0 0) (mid 0.1 0.2) (end 0.2 0) (width 0.05))
    ))
)"#;
    let p = import_footprint(src).unwrap();
    let pad = p
        .pins
        .iter()
        .find(|x| x.name == "1")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    // Anchor rect + three primitives = four copper regions.
    assert_eq!(pad.copper.len(), 4, "anchor + 3 primitives");
    let shapes: Vec<&Shape2D> = pad.copper.iter().map(|c| &c.shape).collect();
    // The gr_circle → a disc (lone-point stroke) at (1, 2.5) mm, radius 0.2mm.
    assert!(
        shapes
            .iter()
            .any(|s| matches!(s, Shape2D::Stroke { path, radius }
                if path.segs.is_empty() && *radius == 200_000
                && path.start == Point { x: 1_000_000, y: 2_500_000 })),
        "gr_circle imported as a disc at the offset centre"
    );
    // Exactly one region carries a Seg::Arc (the gr_arc).
    let arcs: usize = shapes
        .iter()
        .map(|s| {
            s.path()
                .segs
                .iter()
                .filter(|seg| matches!(seg, Seg::Arc { .. }))
                .count()
        })
        .sum();
    assert_eq!(arcs, 1, "the gr_arc became a real arc edge");
    // The 3-point arc rides at the pad offset: start (1,2), mid (1.1,2.2), end (1.2,2).
    assert!(
        shapes
            .iter()
            .any(|s| s.path().segs.iter().any(|seg| matches!(seg,
            Seg::Arc { mid, end }
            if *mid == Point { x: 1_100_000, y: 2_200_000 }
            && *end == Point { x: 1_200_000, y: 2_000_000 })))
    );
}

/// The legacy `gr_arc` encoding — `(start = centre)(end = arc start)(angle)` — is
/// converted by rotating the arc-start point by the swept angle (end) and half it
/// (mid). This is the form real footprints (e.g. MCP_48QFN) use.
#[test]
fn imports_legacy_gr_arc_centre_angle_form() {
    let src = r#"
(footprint "LEGACY_ARC"
  (pad "1" smd custom (at 0 0) (size 0.2 0.2) (layers "F.Cu")
    (options (anchor rect))
    (primitives
      (gr_arc (start 0 0) (end 0.5 0) (angle 90) (width 0.1))
    ))
)"#;
    let p = import_footprint(src).unwrap();
    let pad = p
        .pins
        .iter()
        .find(|x| x.name == "1")
        .unwrap()
        .pad
        .clone()
        .unwrap();
    // anchor + one arc.
    assert_eq!(pad.copper.len(), 2);
    // Centre (0,0), arc-start (0.5mm,0) swept +90° ⇒ end ≈ (0, 0.5mm); the stroke
    // half-width is 0.1/2 = 0.05mm.
    let arc = pad
        .copper
        .iter()
        .find_map(|c| match &c.shape {
            Shape2D::Stroke { path, radius } => path
                .segs
                .iter()
                .find_map(|s| matches!(s, Seg::Arc { .. }).then_some((s.clone(), *radius))),
            _ => None,
        })
        .expect("a legacy gr_arc imported as an arc stroke");
    let (Seg::Arc { end, .. }, radius) = (&arc.0, arc.1) else {
        unreachable!()
    };
    assert_eq!(radius, 50_000, "stroke half-width = width/2");
    // 90° CCW of (0.5mm, 0) about origin = (0, 0.5mm), within nm rounding.
    assert!(
        (end.x).abs() < 10 && (end.y - 500_000).abs() < 10,
        "swept end ≈ (0, 0.5mm): {end:?}"
    );
}

/// Footprint graphics (issue 0016): silk `fp_line`s + an `fp_arc` land in
/// `graphics` with width baked into the shape radius; a courtyard `fp_poly` becomes
/// the authoritative `courtyard`; an `fp_line` on `F.Fab` is lifted too (Decision 15
/// — side-relative layer name kept; its role is resolved from the slab at lowering).
#[test]
fn imports_footprint_graphics_silk_and_courtyard() {
    let src = r#"
(footprint "GFX"
  (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
  (fp_line (start -1 -1) (end 1 -1) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
  (fp_line (start 1 -1) (end 1 1) (stroke (width 0.12) (type solid)) (layer "F.SilkS"))
  (fp_arc (start 0 0) (mid 0.1 0.2) (end 0.2 0) (stroke (width 0.15)) (layer "F.SilkS"))
  (fp_line (start 0 0) (end 1 0) (width 0.1) (layer "F.Fab"))
  (fp_poly (pts (xy -2 -2) (xy 2 -2) (xy 2 2) (xy -2 2)) (width 0.05) (layer "F.CrtYd"))
)"#;
    let p = import_footprint(src).unwrap();
    // Two silk lines + one silk arc + one fab line (the courtyard poly is not a
    // graphic — it becomes `courtyard`).
    assert_eq!(
        p.graphics.len(),
        4,
        "2 silk lines + 1 silk arc + 1 fab line"
    );
    assert_eq!(
        p.graphics.iter().filter(|g| g.layer == "F.SilkS").count(),
        3,
        "three silk graphics"
    );
    assert_eq!(
        p.graphics.iter().filter(|g| g.layer == "F.Fab").count(),
        1,
        "one fab graphic, layer name preserved (role resolved at lowering)"
    );
    // A 0.12mm line → capsule with radius width/2 = 60_000 nm.
    let line = p
        .graphics
        .iter()
        .find(|g| {
            matches!(&g.shape, Shape2D::Stroke { path, .. }
                if path.segs.iter().all(|s| matches!(s, Seg::Line { .. })))
        })
        .expect("a silk line");
    assert_eq!(line.shape.radius(), 60_000, "0.12mm width baked as radius");
    // The arc: a Stroke carrying a Seg::Arc, half-width 0.15/2 = 75_000 nm.
    let arc = p
        .graphics
        .iter()
        .find(|g| {
            g.shape
                .path()
                .segs
                .iter()
                .any(|s| matches!(s, Seg::Arc { .. }))
        })
        .expect("a silk arc");
    assert_eq!(arc.shape.radius(), 75_000);
    // The courtyard polygon overrides the pad-hull (Decision 10): the imported 4×4mm
    // square, not the ~1mm pad hull.
    let court = crate::part::courtyard_shape(&p).expect("imported courtyard");
    assert!(matches!(court, Shape2D::Polygon { .. }));
    let (lo, hi) = court.bbox().unwrap();
    assert_eq!(
        (lo.x, lo.y, hi.x, hi.y),
        (-2_000_000, -2_000_000, 2_000_000, 2_000_000),
        "courtyard is the imported 4×4mm outline, not the pad hull"
    );
}

/// Footprint text (Decision 14): classic `fp_text reference|value|user`. The
/// `reference`/`value` placeholder strings are discarded (the kind is an anchor, not
/// a frozen string); `user` keeps its literal. Height is the font-size *height*
/// component (thickness ignored); `hide` and the `(at … rot)` local orient are lifted.
#[test]
fn imports_footprint_text_reference_value_user_and_hide() {
    let src = r#"(footprint "R_0402"
  (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
  (fp_text reference "REF**" (at 0 1 90) (layer "F.SilkS") (effects (font (size 1 1) (thickness 0.15))))
  (fp_text value "R_0402" (at 0 -1) (layer "F.Fab") hide (effects (font (size 0.5 0.5))))
  (fp_text user "HELLO" (at 0 0) (layer "F.SilkS") (effects (font (size 0.8 0.8)))))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.texts.len(), 3);

    let refr = p
        .texts
        .iter()
        .find(|t| t.kind == FpTextKind::Reference)
        .expect("a reference anchor");
    assert_eq!(refr.layer, "F.SilkS");
    assert_eq!(refr.height, 1_000_000, "font size height → 1mm");
    assert!(!refr.hide);
    assert_eq!(
        refr.orient,
        Orient::from_deg(90).unwrap(),
        "the (at … 90) rotation is a local about-z orient"
    );

    let val = p
        .texts
        .iter()
        .find(|t| t.kind == FpTextKind::Label)
        .expect("a value → Label anchor");
    assert_eq!(val.layer, "F.Fab");
    assert_eq!(val.height, 500_000, "0.5mm height");
    assert!(val.hide, "the bare `hide` token is lifted");

    // `user` keeps its literal; reference/value placeholders never do (the kinds carry
    // no string), so "REF**"/"R_0402" are inherently discarded.
    assert!(
        p.texts
            .iter()
            .any(|t| t.kind == FpTextKind::Literal("HELLO".into()) && t.layer == "F.SilkS")
    );
}

/// A `fp_text user` whose *whole* string is a `${REFERENCE}`/`${VALUE}` KiCad text
/// variable resolves to the live Reference/Label anchor (the fixtures' F.Fab echoes);
/// mixed content stays a verbatim literal.
#[test]
fn imports_user_text_variables_but_leaves_mixed_content_literal() {
    let src = r#"(footprint "R"
  (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
  (fp_text user "${REFERENCE}" (at 0 0) (layer "F.Fab") (effects (font (size 1 1))))
  (fp_text user "${VALUE}" (at 0 1) (layer "F.Fab") (effects (font (size 1 1))))
  (fp_text user "R ${REFERENCE}" (at 0 2) (layer "F.SilkS") (effects (font (size 1 1)))))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.texts.len(), 3);
    assert!(
        p.texts.iter().any(|t| t.kind == FpTextKind::Reference),
        "whole-string ${{REFERENCE}} → Reference anchor"
    );
    assert!(
        p.texts.iter().any(|t| t.kind == FpTextKind::Label),
        "whole-string ${{VALUE}} → Label anchor"
    );
    assert!(
        p.texts
            .iter()
            .any(|t| t.kind == FpTextKind::Literal("R ${REFERENCE}".into())),
        "mixed content stays a verbatim literal"
    );
}

/// The v7 `(property "Reference"|"Value" …)` form maps like `fp_text reference|value`;
/// `(hide yes)` inside `(effects …)` counts as hidden; other property names
/// (Datasheet/Footprint/…) are footprint metadata, not silk, and are skipped.
#[test]
fn imports_footprint_text_property_form() {
    let src = r#"(footprint "R"
  (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
  (property "Reference" "REF**" (at 0 1) (layer "F.SilkS") (effects (font (size 1 1))))
  (property "Value" "10k" (at 0 -1) (layer "F.Fab") (effects (font (size 1 1)) (hide yes)))
  (property "Datasheet" "http://x" (at 0 0) (layer "F.Fab") (effects (hide yes))))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.texts.len(), 2, "Reference + Value; Datasheet skipped");
    assert!(
        p.texts
            .iter()
            .any(|t| t.kind == FpTextKind::Reference && !t.hide)
    );
    assert!(
        p.texts
            .iter()
            .any(|t| t.kind == FpTextKind::Label && t.hide),
        "(hide yes) in effects → hidden"
    );
}

#[test]
fn pad_without_size_yields_no_geometry() {
    let src = r#"(footprint "X" (pad "1" smd circle (at 0 0) (layers "F.Cu")))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.pins[0].pad, None);
}

#[test]
fn skips_unnamed_pad() {
    let p = import_footprint(JST_SH_1X03).unwrap();
    assert!(p.pins.iter().all(|pin| !pin.name.is_empty()));
}

#[test]
fn accepts_legacy_module_header_and_bare_pad_names() {
    // Legacy single-line `(module ...)` form with unquoted name and bare pad
    // numbers, and a pad with a rotation angle in `(at x y angle)`.
    let src = r#"(module RP2040-QFN-56 (layer F.Cu) (tedit 5EF32B43)
            (descr "QFN")
            (pad 56 smd roundrect (at -2.6 -3.4375) (size 0.2 0.875) (layers F.Cu F.Mask))
            (pad 1 smd roundrect (at -1.2 -3.4375 90) (size 0.2 0.875) (layers F.Cu F.Mask)))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.name, "RP2040-QFN-56");
    assert_eq!(p.pins.len(), 2);
    assert_eq!(
        p.pin_offset("56"),
        Some(Point {
            x: -2_600_000,
            y: -3_437_500
        })
    );
    // angle is ignored; only x/y become the offset.
    assert_eq!(
        p.pin_offset("1"),
        Some(Point {
            x: -1_200_000,
            y: -3_437_500
        })
    );
}

#[test]
fn quoted_name_with_spaces_is_preserved() {
    let src = r#"(footprint "Name With Spaces (rev 2)"
            (layer "F.Cu")
            (pad "A1" smd rect (at 0.5 -0.5) (size 1 1) (layers "F.Cu")))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.name, "Name With Spaces (rev 2)");
    assert_eq!(p.pins.len(), 1);
    assert_eq!(
        p.pin_offset("A1"),
        Some(Point {
            x: 500_000,
            y: -500_000
        })
    );
}

#[test]
fn rounds_sub_nm_fractional_mm() {
    // 7+ fractional digits: rounds half-away-from-zero at the nm.
    let src = r#"(footprint "R" (pad "1" smd rect (at 0.0000005 -0.0000004) (size 1 1)))"#;
    let p = import_footprint(src).unwrap();
    assert_eq!(p.pin_offset("1"), Some(Point { x: 1, y: 0 }));
}

/// A pad coordinate beyond ±MAX_COORD (1 m = 1e9 nm) is a clean import error, not
/// a silent i128 wrap in the geometry kernel (issue 0018). 2 m ⇒ 2e9 nm.
#[test]
fn import_rejects_out_of_range_coordinate() {
    let src = r#"(footprint "R" (pad "1" smd rect (at 2000 0) (size 1 1)))"#;
    let e = import_footprint(src).unwrap_err();
    assert!(e.contains("range"), "expected a range error, got: {e}");
}

/// A coordinate exactly at the bound (1 m) imports fine — the ceiling is inclusive.
#[test]
fn import_accepts_coordinate_at_the_bound() {
    let src = r#"(footprint "R" (pad "1" smd rect (at 1000 0) (size 1 1)))"#;
    assert!(import_footprint(src).is_ok());
}

#[test]
fn malformed_inputs_return_err_not_panic() {
    assert!(import_footprint("(footprint").is_err()); // unterminated list
    assert!(import_footprint("").is_err()); // no expression
    assert!(import_footprint("(symbol \"foo\")").is_err()); // wrong head
    assert!(import_footprint("(footprint)").is_err()); // missing name
    assert!(import_footprint(r#"(footprint "x" (pad "1" smd (at)))"#).is_err()); // at missing x/y
    assert!(import_footprint(r#"(footprint "x" (pad "1" smd (at a b)))"#).is_err()); // non-numeric
    assert!(import_footprint(r#"(footprint "x" "unterminated)"#).is_err()); // bad quote
}

/// Optional smoke test over a real on-disk footprint. Guarded on existence so
/// it is a no-op when the KiCad repo isn't present.
#[test]
fn real_file_smoke_test_if_present() {
    let path = "/home/ben/Documents/kalogon/git/Orbiter-Ultra-Hardware-multi_probe/Orbiter_Ultra.pretty/JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical.kicad_mod";
    if !std::path::Path::new(path).exists() {
        return;
    }
    let p = import_footprint_file(path).unwrap();
    assert_eq!(p.name, "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical");
    // 1,2,3 + deduped MP.
    assert_eq!(p.pins.len(), 4);
    assert_eq!(
        p.pin_offset("1"),
        Some(Point {
            x: -1_000_000,
            y: 1_325_000
        })
    );
}

// --- symbol / role layer ------------------------------------------------

/// A self-contained symbol modelled on a real `.kicad_sym`: a `kicad_symbol_lib`
/// holding one multi-unit `(symbol ...)`. Pins are split across two child unit
/// symbols (unit 0 = the power pin, unit 1 = the signal pins), each `(pin ...)`
/// carrying an electrical type, a functional `(name ...)` and a pad `(number
/// ...)` — and nested `(effects ...)` noise, like the real files.
const SYM_LIB: &str = r#"
(kicad_symbol_lib
    (version 20241209)
    (generator "kicad_symbol_editor")
    (symbol "ACME1234"
        (pin_names (offset 0.254))
        (in_bom yes)
        (property "Reference" "U" (at 0 5 0))
        (property "Value" "ACME1234" (at 0 -5 0))
        (property "Footprint" "Acme:ACME-SOT-4" (at 0 -10 0) (effects (hide yes)))
        (symbol "ACME1234_0_1"
            (pin power_in line
                (at -7.62 2.54 0) (length 2.54)
                (name "VDD" (effects (font (size 1.27 1.27))))
                (number "1" (effects (font (size 1.27 1.27))))
            )
        )
        (symbol "ACME1234_1_1"
            (pin output line
                (at 7.62 2.54 180) (length 2.54)
                (name "GPIO0" (effects (font (size 1.27 1.27))))
                (number "2" (effects (font (size 1.27 1.27))))
            )
            (pin bidirectional line
                (at 7.62 0 180) (length 2.54)
                (name "SWDIO" (effects (font (size 1.27 1.27))))
                (number "3" (effects (font (size 1.27 1.27))))
            )
            (pin passive line
                (at 7.62 -2.54 180) (length 2.54)
                (name "GND" (effects (font (size 1.27 1.27))))
                (number "4" (effects (font (size 1.27 1.27))))
            )
        )
    )
)
"#;

/// Footprint with four pads matching the symbol's numbers 1..4, at distinct
/// positions so the join's offsets are checkable.
const FP_4: &str = r#"
(footprint "ACME-SOT-4"
    (layer "F.Cu")
    (pad "1" smd rect (at -1 1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "2" smd rect (at 1 1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "3" smd rect (at 1 -1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "4" smd rect (at -1 -1) (size 0.5 0.5) (layers "F.Cu"))
)
"#;

#[test]
fn parses_symbol_pins_across_units() {
    let s = import_symbol(SYM_LIB).unwrap();
    assert_eq!(s.name, "ACME1234");
    assert_eq!(s.footprint.as_deref(), Some("Acme:ACME-SOT-4"));
    // 1 pin in unit 0 + 3 pins in unit 1 = 4, gathered across the nesting.
    assert_eq!(s.pins.len(), 4);
    let by_num: std::collections::BTreeMap<&str, &SymbolPin> =
        s.pins.iter().map(|p| (p.number.as_str(), p)).collect();
    assert_eq!(by_num["1"].name, "VDD");
    assert_eq!(by_num["1"].etype, ElecType::PowerIn);
    assert_eq!(by_num["2"].name, "GPIO0");
    assert_eq!(by_num["3"].etype, ElecType::Bidirectional);
}

#[test]
fn elec_type_to_role_mapping_table() {
    use PinRole::*;
    let cases = [
        ("power_in", PowerIn),
        ("power_out", PowerOut),
        ("output", Output),
        ("input", Input),
        ("bidirectional", Bidir),
        // Everything below collapses to Passive (documented conservative default).
        ("passive", Passive),
        ("free", Passive),
        ("unspecified", Passive),
        ("no_connect", Passive),
        ("tri_state", Passive),
        ("open_collector", Passive),
        ("open_emitter", Passive),
    ];
    for (tok, want) in cases {
        assert_eq!(ElecType::parse(tok).unwrap().role(), want, "type {tok}");
    }
    // Unknown type is an error, not a silent Passive.
    assert!(ElecType::parse("quantum").is_err());
}

#[test]
fn apply_role_map_overlays_names_and_roles_by_pad_number() {
    // A bare footprint imports role-less (name == number, Passive).
    let bare = import_footprint(FP_4).unwrap();
    assert_eq!(role_of(&bare, "VIN"), None);
    let roled = apply_role_map(
        bare,
        &[
            ("1", "VIN", PinRole::PowerIn),
            ("4", "VOUT", PinRole::PowerOut),
        ],
    )
    .unwrap();
    assert_eq!(role_of(&roled, "VIN"), Some(PinRole::PowerIn));
    assert_eq!(role_of(&roled, "VOUT"), Some(PinRole::PowerOut));
    // The overlaid name now resolves to its pad as a connection selector.
    assert_eq!(roled.resolve_selector("VIN"), vec!["1".to_string()]);
    // A map entry for a pad the footprint lacks is a hard error, not a no-op.
    let err = apply_role_map(
        import_footprint(FP_4).unwrap(),
        &[("99", "X", PinRole::PowerIn)],
    )
    .unwrap_err();
    assert!(err.contains("99"), "got {err}");
}

#[test]
fn join_pairs_names_roles_numbers_and_offsets() {
    let part = import_part(SYM_LIB, FP_4).unwrap();
    assert_eq!(part.name, "ACME-SOT-4");
    assert_eq!(part.pins.len(), 4);

    // Functional name resolves to symbol role; offset comes from the footprint.
    assert_eq!(role_of(&part, "VDD"), Some(PinRole::PowerIn));
    assert_eq!(
        offset_of(&part, "VDD"),
        Some(Point {
            x: -1_000_000,
            y: 1_000_000
        })
    );
    assert_eq!(role_of(&part, "GPIO0"), Some(PinRole::Output));
    assert_eq!(role_of(&part, "SWDIO"), Some(PinRole::Bidir));
    assert_eq!(role_of(&part, "GND"), Some(PinRole::Passive));
    assert_eq!(
        offset_of(&part, "GND"),
        Some(Point {
            x: -1_000_000,
            y: -1_000_000
        })
    );

    // Stored identity is the pad number, and the name selector resolves to it.
    assert_eq!(part.resolve_selector("VDD"), vec!["1".to_string()]);
    assert_eq!(part.pin_role("1"), Some(PinRole::PowerIn));

    // Pad numbers preserved as the manufacturing/join key, distinct from names.
    let vdd = part.pins.iter().find(|p| p.name == "VDD").unwrap();
    assert_eq!(vdd.number, "1");
    let gpio = part.pins.iter().find(|p| p.name == "GPIO0").unwrap();
    assert_eq!(gpio.number, "2");
}

#[test]
fn join_reports_mismatches_without_dropping_pins() {
    // Symbol has a power pin "5" with no pad; footprint has a pad "6" with no
    // symbol pin. Neither must be silently dropped.
    let sym = r#"
(symbol "X"
    (pin power_in line (at 0 0 0) (length 1) (name "VBUS") (number "5"))
    (pin input line (at 0 0 0) (length 1) (name "IN") (number "1"))
)"#;
    let fp = r#"
(footprint "X-FP"
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
    (pad "6" smd rect (at 2 0) (size 1 1) (layers "F.Cu"))
)"#;
    let symbol = import_symbol(sym).unwrap();
    let footprint = import_footprint(fp).unwrap();
    let report = join_symbol_footprint(&symbol, &footprint);

    // The matched pin carries name + role; the unmatched pad stays Passive.
    assert_eq!(role_of(&report.part, "IN"), Some(PinRole::Input));
    // The orphan power pin is surfaced (number, name, role), not dropped.
    assert_eq!(
        report.symbol_only,
        vec![("5".to_string(), "VBUS".to_string(), PinRole::PowerIn)]
    );
    // The orphan pad is surfaced and kept Passive with name = number.
    assert_eq!(report.footprint_only, vec!["6".to_string()]);
    let pad6 = report.part.pins.iter().find(|p| p.number == "6").unwrap();
    assert_eq!(pad6.role, PinRole::Passive);
    assert_eq!(pad6.name, "6");

    // The strict convenience wrapper turns any mismatch into an Err.
    assert!(import_part(sym, fp).is_err());
}

/// Real-data join: pair a real `.kicad_sym` symbol with the `.kicad_mod` its own
/// `Footprint` property names. Guarded on existence (no-op without the repo).
#[test]
fn real_symbol_footprint_join_if_present() {
    let sym_path =
        "/home/ben/Documents/kalogon/git/Kalogon-KiCad-Repository/Power_Management_TI.kicad_sym";
    let fp_path = "/home/ben/Documents/kalogon/git/Kalogon-KiCad-Repository/footprints/eFuse_TI.pretty/Texas_RPW9919A_VQFN-HR-10.kicad_mod";
    if !std::path::Path::new(sym_path).exists() || !std::path::Path::new(fp_path).exists() {
        return;
    }
    let sym_text = std::fs::read_to_string(sym_path).unwrap();
    let symbol = import_symbol_named(&sym_text, "TPS25981x").unwrap();
    assert_eq!(
        symbol.footprint.as_deref(),
        Some("eFuse_TI:Texas_RPW9919A_VQFN-HR-10")
    );
    let footprint = import_footprint_file(fp_path).unwrap();
    let report = join_symbol_footprint(&symbol, &footprint);

    // Every footprint pad became a pin; a real power pin carries its role.
    assert!(!report.part.pins.is_empty());
    // IN is the eFuse input rail (power_in -> PowerIn).
    assert_eq!(role_of(&report.part, "IN"), Some(PinRole::PowerIn));
    // OUT is the switched output rail (power_out -> PowerOut).
    assert_eq!(role_of(&report.part, "OUT"), Some(PinRole::PowerOut));
    // PG is open_collector -> Passive (conservative default).
    assert_eq!(role_of(&report.part, "PG"), Some(PinRole::Passive));
    // Exact 10/10 join: no orphan pins on either side.
    assert!(report.symbol_only.is_empty() && report.footprint_only.is_empty());
}

/// PoC Stage-1 gate: the authoritative RP2350A QFN-60 symbol + footprint
/// (KiCad official library, vendored under poc/parts/) join cleanly into a
/// 61-pin part with real RP2350 functions and roles. Guarded on the vendored
/// files existing, so it is a no-op in a checkout without them.
#[test]
fn rp2350a_qfn60_join_if_present() {
    let sym_path = "poc/parts/MCU_RaspberryPi.kicad_sym";
    let fp_path = "poc/parts/RP2350A_QFN-60.kicad_mod";
    if !std::path::Path::new(sym_path).exists() || !std::path::Path::new(fp_path).exists() {
        return;
    }
    let sym = import_symbol_named(&std::fs::read_to_string(sym_path).unwrap(), "RP2350A").unwrap();
    let footprint = import_footprint_file(fp_path).unwrap();
    let report = join_symbol_footprint(&sym, &footprint);
    // 60 signal/power pads + the exposed pad = 61 pins, clean both ways.
    assert_eq!(report.part.pins.len(), 61);
    assert!(report.symbol_only.is_empty() && report.footprint_only.is_empty());
    // Real RP2350 functional names + roles survive the join.
    assert_eq!(role_of(&report.part, "GPIO0"), Some(PinRole::Bidir));
    assert_eq!(role_of(&report.part, "IOVDD"), Some(PinRole::PowerIn));
    assert_eq!(role_of(&report.part, "VREG_LX"), Some(PinRole::PowerOut));
    assert!(report.part.pins.iter().any(|p| p.name == "USB_DP"));
    assert!(report.part.pins.iter().any(|p| p.name == "QSPI_SCLK"));
    // 6 IOVDD + 3 DVDD pads share a functional name. The fix: a name selector
    // fans out to ALL of them (distinct pad numbers), so connecting "IOVDD"
    // nets every pad — no uniquify workaround, no silently-floating power pads.
    assert_eq!(
        report
            .part
            .pins
            .iter()
            .filter(|p| p.name == "IOVDD")
            .count(),
        6
    );
    assert_eq!(
        report.part.pins.iter().filter(|p| p.name == "DVDD").count(),
        3
    );
    let iovdd_pads = report.part.resolve_selector("IOVDD");
    assert_eq!(iovdd_pads.len(), 6);
    assert_eq!(report.part.resolve_selector("DVDD").len(), 3);
    // Each resolved identity is a real, distinct pad that resolves to a role.
    // (KiCad marks only one pin of a stacked power rail `power_in` and the rest
    // `passive`, so the roles legitimately vary — what matters is all 6 are
    // present and connectable, which is the floating-power-pad fix.)
    assert!(iovdd_pads.iter().all(|n| report.part.pin_role(n).is_some()));
    assert!(
        iovdd_pads
            .iter()
            .any(|n| report.part.pin_role(n) == Some(PinRole::PowerIn))
    );
}

// --- board outline (.kicad_pcb Edge.Cuts) -------------------------------

/// Count the arc segments across a shape's skeleton path.
fn arc_segs(s: &Shape2D) -> usize {
    s.path()
        .segs
        .iter()
        .filter(|seg| matches!(seg, Seg::Arc { .. }))
        .count()
}

/// Board membership for an imported `(outline, cutouts)`: inside the outline and
/// outside every cutout (the former `BoardShape::contains`).
fn on_board(b: &(Shape2D, Vec<Shape2D>), p: Point) -> bool {
    b.0.contains_point(p) && !b.1.iter().any(|c| c.contains_point(p))
}

/// A rectangular outline authored as four `gr_line`s on `Edge.Cuts` (with the
/// real `.kicad_pcb` nesting: a header, a `(stroke …)`, layer last). The lines
/// are intentionally out of order to exercise the endpoint stitching.
#[test]
fn imports_rectangular_outline_from_gr_lines() {
    let src = r#"
(kicad_pcb
  (version 20240108)
  (generator "pcbnew")
  (general (thickness 1.6))
  (gr_line (start 0 0) (end 10 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
  (gr_line (start 10 20) (end 0 20) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
  (gr_line (start 0 20) (end 0 0) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
  (gr_line (start 10 0) (end 10 20) (stroke (width 0.1) (type default)) (layer "Edge.Cuts"))
  (gr_line (start -5 -5) (end -5 5) (stroke (width 0.1)) (layer "F.SilkS"))
)"#;
    let b = import_board_outline(src).unwrap();
    assert!(b.1.is_empty());
    // 0..10 mm × 0..20 mm rectangle: a midpoint is inside, an outside point is not.
    assert!(on_board(
        &b,
        Point {
            x: 5_000_000,
            y: 10_000_000
        }
    ));
    assert!(!on_board(
        &b,
        Point {
            x: 15_000_000,
            y: 10_000_000
        }
    ));
    // The non-Edge.Cuts silkscreen line is ignored: a pure 4-line rectangle.
    assert_eq!(b.0.points().len(), 4);
    assert_eq!(arc_segs(&b.0), 0);
}

/// An outline with one curved edge: three `gr_line`s + one 3-point `gr_arc` close a
/// loop, and the arc lands in the path as a `Seg::Arc`.
#[test]
fn imports_outline_with_arc_edge() {
    // A "D"-ish loop: bottom, right, top straight edges, then an arc bowing left
    // from the top-left back down to the start.
    let src = r#"
(kicad_pcb
  (gr_line (start 0 0) (end 10 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 0) (end 10 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 10) (end 0 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_arc (start 0 10) (mid -2 5) (end 0 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
)"#;
    let b = import_board_outline(src).unwrap();
    assert!(b.1.is_empty());
    assert_eq!(arc_segs(&b.0), 1, "the gr_arc became a Seg::Arc edge");
    // The arc's mid (-2,5)mm is the stored on-curve point.
    assert!(b.0.path().segs.iter().any(|seg| matches!(seg,
            Seg::Arc { mid, .. } if *mid == Point { x: -2_000_000, y: 5_000_000 })));
    // A point well inside the rectangular body is on the board; one past the arc
    // bulge to the left is off it.
    assert!(on_board(
        &b,
        Point {
            x: 5_000_000,
            y: 5_000_000
        }
    ));
    assert!(!on_board(
        &b,
        Point {
            x: -3_000_000,
            y: 5_000_000
        }
    ));
}

/// A rectangular outline with an inner rectangular cutout: two disjoint closed
/// loops → the larger is the outline, the smaller a cutout.
#[test]
fn imports_outline_with_inner_cutout() {
    let src = r#"
(kicad_pcb
  (gr_line (start 0 0) (end 30 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 30 0) (end 30 30) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 30 30) (end 0 30) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 0 30) (end 0 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 10) (end 20 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 20 10) (end 20 20) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 20 20) (end 10 20) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 20) (end 10 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
)"#;
    let b = import_board_outline(src).unwrap();
    assert_eq!(b.1.len(), 1, "inner loop classified as a cutout");
    // Inside the outer rect but outside the inner cutout: on the board.
    assert!(on_board(
        &b,
        Point {
            x: 5_000_000,
            y: 5_000_000
        }
    ));
    // Centre of the inner cutout: inside the outline, but carved out ⇒ off-board.
    assert!(b.0.contains_point(Point {
        x: 15_000_000,
        y: 15_000_000
    }));
    assert!(!on_board(
        &b,
        Point {
            x: 15_000_000,
            y: 15_000_000
        }
    ));
}

/// A circular board: one `gr_circle` becomes a closed two-arc outline.
#[test]
fn imports_circular_outline_from_gr_circle() {
    let src = r#"
(kicad_pcb
  (gr_circle (center 0 0) (end 10 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
)"#;
    let b = import_board_outline(src).unwrap();
    assert!(b.1.is_empty());
    assert_eq!(arc_segs(&b.0), 2, "circle modelled as two semicircle arcs");
    // Radius 10mm about the origin: centre is inside, a point past the radius is not.
    assert!(on_board(&b, Point { x: 0, y: 0 }));
    assert!(on_board(&b, Point { x: 9_000_000, y: 0 }));
    assert!(!on_board(
        &b,
        Point {
            x: 11_000_000,
            y: 0
        }
    ));
}

#[test]
fn board_outline_errors_are_not_panics() {
    // Wrong top-level head.
    assert!(import_board_outline(r#"(footprint "x")"#).is_err());
    // No Edge.Cuts geometry at all.
    assert!(import_board_outline(r#"(kicad_pcb (version 1))"#).is_err());
    // An open contour (3 sides of a rect, never closed).
    let open = r#"
(kicad_pcb
  (gr_line (start 0 0) (end 10 0) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 0) (end 10 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
  (gr_line (start 10 10) (end 0 10) (stroke (width 0.1)) (layer "Edge.Cuts"))
)"#;
    assert!(import_board_outline(open).is_err());
}
