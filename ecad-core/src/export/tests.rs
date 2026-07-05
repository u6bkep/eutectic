use super::*;
use crate::command::{Command, Transaction};
use crate::doc::Doc;
use crate::doc::Orient;
use crate::elaborate::{board_rect, psu_module};
use crate::history::History;
use crate::part::part_library;

/// Resolve a `Role::Mask` slab of `doc` by side — the top/bottom mask by z-position —
/// so the mask tests can name a side while `gerber_mask` takes the slab itself. Panics
/// if the side carries no mask (the tests all use the default stackup, which has both).
fn mask_of(doc: &Doc, side: Layer) -> Slab {
    let su = crate::elaborate::stackup(&doc.source);
    let z = match side {
        Layer::Bottom => su.bottom_mask(),
        _ => su.top_mask(),
    }
    .expect("side has a mask slab");
    su.slabs
        .iter()
        .find(|s| s.role == Role::Mask && s.z == z)
        .cloned()
        .expect("mask slab present")
}

/// A copper [`Slab`] of `doc`'s stackup by name — the test-side of the export copper
/// loop now taking a slab (Decision 13). Panics if the name is not a copper slab.
fn cu(doc: &Doc, name: &str) -> Slab {
    crate::elaborate::stackup(&doc.source)
        .copper_slabs()
        .into_iter()
        .find(|s| s.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("no copper slab `{name}`"))
}

fn doc_psu(n: usize) -> (Doc, PartLib) {
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(n))),
        &lib,
        "psu",
    )
    .unwrap();
    (h.doc().clone(), lib)
}

#[test]
fn fmt_mm_handles_sign_and_fraction() {
    assert_eq!(fmt_mm(0), "0.000000");
    assert_eq!(fmt_mm(2 * MM), "2.000000");
    assert_eq!(fmt_mm(-2 * MM), "-2.000000");
    assert_eq!(fmt_mm(1_325_000), "1.325000");
    assert_eq!(fmt_mm(-1), "-0.000001");
}

#[test]
fn netlist_lists_expected_nets_and_pins() {
    let (doc, _) = doc_psu(2);
    let nl = netlist(&doc);
    // psu_module(2): a regulator + two decouplers on VBUS/GND.
    let expected = "\
# netlist
GND: psu.dec[0].p2 psu.dec[1].p2 psu.reg.GND
VBUS: psu.dec[0].p1 psu.dec[1].p1 psu.reg.VOUT
";
    assert_eq!(nl, expected);
}

#[test]
fn placement_csv_has_header_and_rows() {
    let (doc, _) = doc_psu(2);
    let csv = placement_csv(&doc);
    let expected = "\
ref,part,x_mm,y_mm,rotation_deg,side
psu.dec[0],Cap,10.000000,0.000000,0,T
psu.dec[1],Cap,20.000000,0.000000,0,T
psu.reg,LDO,0.000000,0.000000,0,T
";
    assert_eq!(csv, expected);
    // Header + one row per component, nothing extra.
    assert_eq!(csv.lines().count(), 1 + doc.components.len());
}

#[test]
fn placement_csv_reflects_orientation() {
    // A rotated MCU shows up in the rotation column.
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            G::Instance {
                path: "u1".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
        ])),
        &lib,
        "rot",
    )
    .unwrap();
    let csv = placement_csv(h.doc());
    assert!(
        csv.contains("u1,MCU,0.000000,0.000000,90,T\n"),
        "got:\n{csv}"
    );
}

#[test]
fn placement_csv_marks_bottom_side() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            G::Instance {
                path: "u1".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(0).unwrap().flipped(),
            },
            G::Instance {
                path: "u2".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Rotate {
                path: "u2".into(),
                orient: Orient::from_deg(90).unwrap().flipped(),
            },
        ])),
        &lib,
        "flip",
    )
    .unwrap();
    let csv = placement_csv(h.doc());
    // KiCad .pos convention: rotation is the *authored* about-z angle, side marked
    // separately — a plain bottom flip is `0,B`, and an authored 90° bottom part is
    // `90,B` (the flip axis is not folded into the reported angle).
    assert!(
        csv.contains(",0,B\n"),
        "bottom-side component at 0° marked B:\n{csv}"
    );
    assert!(
        csv.contains(",90,B\n"),
        "authored 90° bottom part reports 90,B:\n{csv}"
    );
}

#[test]
fn svg_contains_outline_and_component_ids() {
    // A scene with an explicit board outline.
    let lib = part_library();
    let mut h = History::new(Default::default());
    let mut src = psu_module(2);
    src.insert(0, board_rect(Point::mm(0, 0), Point::mm(60, 40)));
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "board")
        .unwrap();
    let s = svg(h.doc(), &lib).unwrap();

    assert!(s.starts_with("<?xml"));
    assert!(s.contains("<svg "));
    assert!(s.contains("viewBox="));
    assert!(
        s.contains("class=\"outline-board\""),
        "explicit board outline expected"
    );
    assert!(s.contains("data-id=\"psu.reg\""));
    assert!(s.contains(">psu.dec[0]</text>"));
    assert!(s.contains("class=\"pad\""), "pin pads expected");
    assert!(s.trim_end().ends_with("</svg>"));
}

#[test]
fn svg_falls_back_to_bounding_box_without_board() {
    let (doc, lib) = doc_psu(2);
    let s = svg(&doc, &lib).unwrap();
    assert!(
        s.contains("class=\"outline-bbox\""),
        "implicit bbox outline expected"
    );
}

/// An authored `hole` (NPTH through-cut) draws as an outlined circle in the SVG, so a
/// human reading the sketch sees the mounting hole (the outline path draws only the
/// board region, not standalone voids).
#[test]
fn svg_draws_authored_holes() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Hole {
                center: Point::mm(3, 17),
                dia: 2_700_000,
            },
        ])),
        &lib,
        "hole",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(s.contains("class=\"hole\""), "hole circle expected:\n{s}");
    // Radius = dia/2 = 1.35mm, centered at cx=3mm.
    assert!(s.contains("cx=\"3.000000\""), "hole at x=3mm:\n{s}");
    assert!(s.contains("r=\"1.350000\""), "hole radius 1.35mm:\n{s}");
}

#[test]
fn svg_draws_real_pad_copper_not_a_dot() {
    use crate::elaborate::GenDirective as G;
    use crate::part::{PadCopper, PadGeo, PadLayers, PinDef, PinRole};

    // A part whose single pin carries real copper: a 1mm square pad on Top
    // (straight edges ⇒ a filled `<polygon>`, no curve).
    let mut lib = PartLib::new();
    lib.insert(
        "PAD".into(),
        PartDef {
            name: "PAD".into(),
            pins: vec![PinDef {
                name: "1".into(),
                number: "1".into(),
                role: PinRole::Passive,
                offset: Point { x: 0, y: 0 },
                pad: Some(PadGeo {
                    copper: vec![PadCopper {
                        shape: Shape2D::rect(Point { x: 0, y: 0 }, MM, MM),
                        layers: PadLayers::Top,
                    }],
                    drill: None,
                }),
            }],
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        },
    );
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "PAD".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "pad",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();

    // The footprint's real copper is drawn as a filled pad polygon...
    assert!(
        s.contains("<polygon class=\"pad\""),
        "real pad copper expected as a filled polygon:\n{s}"
    );
    // ...replacing the old fixed r=0.3 circle render-lie for a padded pin.
    assert!(
        !s.contains("<circle class=\"pad\""),
        "the r=0.3 pad-dot lie should be gone for a real pad:\n{s}"
    );
}

#[test]
fn bound_interface_signal_draws_no_spurious_dot() {
    // Issue 0029: an interface signal *bound* to a real pad (`iface.pads`) must not be
    // enumerated as a separate `port.signal` pin — its pad is already drawn as copper
    // by number, so the `port.signal` fallback dot painted a duplicate on the pad.
    use crate::elaborate::GenDirective as G;
    use crate::part::{Dir, InterfaceDef, PadCopper, PadGeo, PadLayers, PinDef, PinRole};

    let mut iface = InterfaceDef {
        type_name: "uart".into(),
        signals: BTreeMap::from([("tx".into(), Dir::Out)]),
        offsets: BTreeMap::from([("tx".into(), Point { x: 0, y: 0 })]),
        mate: Vec::new(),
        pads: BTreeMap::new(),
    };
    // Bind the `tx` signal to pad number `1` (the imported-part case).
    iface.pads.insert("tx".into(), "1".into());

    let def = PartDef {
        name: "BND".into(),
        pins: vec![PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: Shape2D::rect(Point { x: 0, y: 0 }, MM, MM),
                    layers: PadLayers::Top,
                }],
                drill: None,
            }),
        }],
        interfaces: BTreeMap::from([("port".to_string(), iface)]),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: None,
        class: None,
    };

    // The bound signal is dropped from the enumeration; only the pad number remains.
    assert_eq!(part_pin_ids(&def), vec!["1".to_string()]);

    // And the rendered SVG draws the real pad copper, with no `<circle class="pad">`
    // fallback dot on top of it.
    let mut lib = PartLib::new();
    lib.insert("BND".into(), def);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "BND".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "bnd",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("<polygon class=\"pad\""),
        "real pad copper expected:\n{s}"
    );
    assert!(
        !s.contains("<circle class=\"pad\""),
        "no spurious fallback dot for the bound interface signal:\n{s}"
    );
}

#[test]
fn svg_renders_board_text_as_silk_strokes() {
    use crate::doc::Orient;
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Text {
                string: "R12".into(),
                at: Point::mm(2, 10),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            },
        ])),
        &lib,
        "text",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("class=\"silk\""),
        "lowered board text should render as silk strokes:\n{s}"
    );
    // Several glyph strokes ⇒ more than one silk polyline.
    assert!(s.matches("class=\"silk\"").count() >= 3, "got:\n{s}");
}

/// Imported footprint silk renders through the `Role::Marking` silk path (issue
/// 0016): a placed component's `fp_line`s appear as `class="silk"` polylines.
#[test]
fn svg_renders_footprint_silk_as_silk_strokes() {
    use crate::elaborate::GenDirective as G;
    let mut lib = PartLib::new();
    let part = crate::kicad::import_footprint(
        r#"(footprint "GFX"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_line (start -1 -1) (end 1 -1) (stroke (width 0.12)) (layer "F.SilkS"))
                (fp_line (start 1 -1) (end 1 1) (stroke (width 0.12)) (layer "F.SilkS")))"#,
    )
    .unwrap();
    lib.insert("GFX".into(), part);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "GFX".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "gfx",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("class=\"silk\""),
        "footprint silk should render as silk strokes:\n{s}"
    );
    assert!(
        s.matches("class=\"silk\"").count() >= 2,
        "two silk lines expected:\n{s}"
    );
}

/// A silk `fp_poly` is a *filled* area (radius 0): it must render as a closed
/// filled `<polygon class="silk">`, not a `stroke-width="0"` (invisible) polyline.
#[test]
fn svg_renders_silk_polygon_as_filled_polygon() {
    use crate::elaborate::GenDirective as G;
    let mut lib = PartLib::new();
    let part = crate::kicad::import_footprint(
        r#"(footprint "TRI"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.SilkS")))"#,
    )
    .unwrap();
    lib.insert("TRI".into(), part);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "TRI".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "tri",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("<polygon class=\"silk\""),
        "silk fp_poly should render as a filled polygon:\n{s}"
    );
    assert!(
        s.contains("<polygon class=\"silk\" points=\"") && s.contains("fill=\"#888888\""),
        "silk polygon should be filled silk-colour:\n{s}"
    );
    // It must NOT be emitted as an invisible zero-width silk polyline.
    assert!(
        !s.contains("class=\"silk\" points=\"") || !s.contains("stroke-width=\"0\""),
        "silk polygon must not be a stroke-width=0 polyline:\n{s}"
    );
}

#[test]
fn exporters_are_deterministic() {
    let (doc, lib) = doc_psu(3);
    assert_eq!(netlist(&doc), netlist(&doc));
    assert_eq!(placement_csv(&doc), placement_csv(&doc));
    assert_eq!(svg(&doc, &lib), svg(&doc, &lib));
}

// --- fab output (Gerber / Excellon) ------------------------------------

use crate::doc::Provenance;
use crate::elaborate::GenDirective as G;
use crate::id::{NetId, TraceId, ViaId};
use crate::route::{Trace, Via};

/// Two caps on a 20x10 board joined by net `N`, hand-routed with a known
/// top trace, a bottom trace, and a via joining them at (10,5) — exact, so
/// the fab output is fully predictable (no autorouter nondeterminism).
fn hand_routed_board() -> (Doc, PartLib) {
    let lib = part_library();
    let mut h = History::new(Default::default());
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "c0".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "c1".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "N".into(),
            pins: vec![("c0".into(), "p1".into()), ("c1".into(), "p1".into())],
        },
    ];
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "place")
        .unwrap();
    let net = NetId::new("N");
    let t0 = Trace {
        net: net.clone(),
        layer: "F.Cu".into(),
        path: vec![Point::mm(6, 5), Point::mm(10, 5)],
        width: 200_000,
        prov: Provenance::Pinned,
    };
    let t1 = Trace {
        net: net.clone(),
        layer: "B.Cu".into(),
        path: vec![Point::mm(10, 5), Point::mm(14, 5)],
        width: 200_000,
        prov: Provenance::Pinned,
    };
    let v = Via {
        net,
        at: Point::mm(10, 5),
        span: None,
        drill: 300_000,
        pad: 600_000,
        prov: Provenance::Pinned,
    };
    h.commit(
        Transaction(vec![
            Command::AddTrace(TraceId(0), t0),
            Command::AddTrace(TraceId(1), t1),
            Command::AddVia(ViaId(0), v),
        ]),
        &lib,
        "route",
    )
    .unwrap();
    (h.doc().clone(), lib)
}

#[test]
fn gerber_layer_has_format_apertures_draws_and_flashes() {
    let (doc, lib) = hand_routed_board();
    let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
    // Format spec + mm units + end.
    assert!(top.contains("%FSLAX46Y46*%"));
    assert!(top.contains("%MOMM*%"));
    assert!(top.trim_end().ends_with("M02*"));
    // Aperture defs: 0.2mm trace pen and 0.6mm via pad.
    assert!(top.contains("%ADD10C,0.200000*%"), "got:\n{top}");
    assert!(top.contains("%ADD11C,0.600000*%"), "got:\n{top}");
    // The Top trace: a move to (6,5) then a draw to (10,5) — nm == 4.6 integer.
    assert!(top.contains("X6000000Y5000000D02*"));
    assert!(top.contains("X10000000Y5000000D01*"));
    // The via flashes on Top (it spans Top..Bottom).
    assert!(top.contains("X10000000Y5000000D03*"));
    // The toy Cap pads (0.8 mm squares, top copper) flash with a rect aperture.
    assert!(top.contains("%ADD12R,0.800000X0.800000*%"), "got:\n{top}");
    // Exactly one draw (one 2-pt trace) on Top; five flashes — the via plus the
    // two Caps' four top-side toy pads.
    assert_eq!(top.matches("D01*").count(), 1);
    assert_eq!(top.matches("D03*").count(), 5);
    // The Bottom layer carries the other trace and the same via flash; the toy
    // pads are Top-only, so no pad flashes land there.
    let bot = gerber_layer(&doc, &lib, &cu(&doc, "B.Cu"));
    assert_eq!(bot.matches("D01*").count(), 1);
    assert_eq!(bot.matches("D03*").count(), 1);
}

#[test]
fn excellon_lists_via_drills() {
    let (doc, lib) = hand_routed_board();
    let files = excellon_drill(&doc, &lib);
    // The via is a plated through-hole, so it lands in the PTH file; the Cap pads are
    // footprint-less (no drill), so there is no NPTH file.
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["board-PTH.drl"], "PTH only, no NPTH");
    let drl = &files[0].1;
    assert!(drl.starts_with("M48"));
    assert!(drl.contains("METRIC"));
    // One tool at the via's 0.3mm drill, with the via's coordinate.
    assert!(drl.contains("T1C0.300000"), "got:\n{drl}");
    assert!(drl.contains("X10.000000Y5.000000"), "got:\n{drl}");
    assert!(drl.trim_end().ends_with("M30"));
}

/// Issue 0022: the drill file is a forward query over through-cut `Void` features, so
/// a plated through-hole **pad**'s drill now reaches the PTH file — not only vias. A
/// board with a drilled pad *and* a via yields both, with correct diameters at the
/// right coordinates, and there is no NPTH file (both holes are plated).
#[test]
fn excellon_includes_pad_and_via_drills() {
    let mut lib = part_library();
    let fp = crate::kicad::import_footprint(
            r#"(footprint "TH" (pad "1" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8) (layers "*.Cu")))"#,
        )
        .unwrap();
    lib.insert("TH".into(), fp);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "j".into(),
                part: "TH".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "j".into(),
                pos: Point::mm(5, 5),
            },
            // Establishes net N so the via may be added.
            G::ConnectPins {
                net: "N".into(),
                pins: vec![("j".into(), "1".into())],
            },
        ])),
        &lib,
        "th",
    )
    .unwrap();
    // A via, so the file carries both a pad drill and a via drill.
    let v = Via {
        net: NetId::new("N"),
        at: Point::mm(12, 8),
        span: None,
        drill: 300_000,
        pad: 600_000,
        prov: Provenance::Pinned,
    };
    h.commit(Transaction::one(Command::AddVia(ViaId(0), v)), &lib, "via")
        .unwrap();
    let doc = h.doc();

    let files = excellon_drill(doc, &lib);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["board-PTH.drl"],
        "both holes are plated ⇒ one PTH file, no NPTH: {names:?}"
    );
    let drl = &files[0].1;
    // Both tools present: the pad drill (0.8mm) and the via drill (0.3mm).
    assert!(drl.contains("C0.800000"), "pad drill tool 0.8mm:\n{drl}");
    assert!(drl.contains("C0.300000"), "via drill tool 0.3mm:\n{drl}");
    // Hit coordinates: the pad at (5,5), the via at (12,8).
    assert!(drl.contains("X5.000000Y5.000000"), "pad drill hit:\n{drl}");
    assert!(drl.contains("X12.000000Y8.000000"), "via drill hit:\n{drl}");
}

/// An authored `hole` directive (Decision 16b NPTH) reaches `board-NPTH.drl`: the
/// full-stackup material-less `Role::Void` it lowers to is classified non-plated by
/// `drill_hits` and lands in the NPTH file at its exact center + diameter.
#[test]
fn authored_hole_reaches_npth_drill() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Hole {
                center: Point::mm(3, 17),
                dia: 2_700_000, // M2.5 clearance
            },
        ])),
        &lib,
        "hole",
    )
    .unwrap();
    let files = excellon_drill(h.doc(), &lib);
    let npth = files
        .iter()
        .find(|(n, _)| n == "board-NPTH.drl")
        .map(|(_, c)| c.as_str())
        .unwrap_or_else(|| panic!("expected board-NPTH.drl, got {:?}", files));
    assert!(npth.contains("C2.700000"), "2.7mm NPTH tool:\n{npth}");
    assert!(
        npth.contains("X3.000000Y17.000000"),
        "hole at (3,17):\n{npth}"
    );
    // And it is NOT in a PTH file (no plated barrel).
    assert!(
        !files.iter().any(|(n, _)| n == "board-PTH.drl"),
        "a lone NPTH hole ships no PTH file"
    );
}

/// The plating split: a hit list with both a plated and a non-plated hole yields two
/// files, each carrying only its own class. (Exercised on a synthesized hit list so
/// the split logic is unit-testable without a full authored board — the end-to-end
/// authoring path is `authored_hole_reaches_npth_drill`.)
#[test]
fn excellon_splits_pth_and_npth() {
    let hits = vec![
        (true, 800_000, DrillKind::Round(Point::mm(5, 5))), // plated pad, 0.8mm
        (false, 900_000, DrillKind::Round(Point::mm(9, 9))), // NPTH mounting, 0.9mm
    ];
    let files = excellon_files(hits);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["board-PTH.drl", "board-NPTH.drl"]);
    let pth = &files[0].1;
    let npth = &files[1].1;
    assert!(
        pth.contains("C0.800000") && !pth.contains("C0.900000"),
        "PTH:\n{pth}"
    );
    assert!(
        npth.contains("C0.900000") && !npth.contains("C0.800000"),
        "NPTH:\n{npth}"
    );
    assert!(pth.contains("X5.000000Y5.000000") && npth.contains("X9.000000Y9.000000"));
}

/// A slot (capsule) drill emits a `G85` routed hole between its endpoints.
#[test]
fn excellon_slot_emits_g85() {
    let prog = excellon_program(
        &[(600_000, DrillKind::Slot(Point::mm(2, 3), Point::mm(6, 3)))],
        "slots",
    );
    assert!(
        prog.contains("X2.000000Y3.000000G85X6.000000Y3.000000"),
        "slot as G85:\n{prog}"
    );
}

#[test]
fn edge_cuts_traces_the_outline() {
    let (doc, lib) = hand_routed_board();
    let e = gerber_edge_cuts(&doc, &lib);
    assert!(e.contains("Edge.Cuts"));
    // Closed 0,0 -> 20,0 -> 20,10 -> 0,10 -> 0,0 rectangle (nm coordinates).
    assert!(e.contains("X0Y0D02*"));
    assert!(e.contains("X20000000Y0D01*"));
    assert!(e.contains("X20000000Y10000000D01*"));
    assert!(e.contains("X0Y10000000D01*"));
}

// --- stage 3: arc-aware export helpers --------------------------------------
// (Until import/text can author arcs, no arc board reaches export end-to-end, so
//  the helpers are exercised directly here on constructed arc shapes.)

const TMM: Nm = 1_000_000;
fn tp(x: Nm, y: Nm) -> Point {
    Point { x, y }
}
/// A filled half-disc (D-shape): an arc over the top closed by the flat diameter.
fn half_disc(r: Nm) -> Shape2D {
    Shape2D::polygon_path(
        crate::geom::Path {
            start: tp(-r, 0),
            segs: vec![Seg::Arc {
                mid: tp(0, r),
                end: tp(r, 0),
            }],
        },
        0,
    )
}

#[test]
fn svg_arc_params_match_hand_computed_flags() {
    let r = 10 * TMM;
    // Upper semicircle (-R,0)→(0,R)→(R,0): model-CW after y-flip ⇒ sweep 1; the
    // 180° span puts the centre on the chord ⇒ large 0.
    let (rad, large, sweep) = svg_arc_params(tp(-r, 0), tp(0, r), tp(r, 0)).unwrap();
    assert_eq!((large, sweep), (0, 1));
    assert!((rad - r).abs() < 10, "radius ~ R, got {rad}");
    // Minor CCW quarter (R,0)→45°→(0,R): turn > 0 ⇒ sweep 0; < 180° ⇒ large 0.
    let m = (r as f64 * std::f64::consts::FRAC_1_SQRT_2).round() as Nm;
    let (rad2, large2, sweep2) = svg_arc_params(tp(r, 0), tp(m, m), tp(0, r)).unwrap();
    assert_eq!((large2, sweep2), (0, 0));
    assert!((rad2 - r).abs() < 10);
    // Collinear ⇒ None (caller draws a straight line).
    assert!(svg_arc_params(tp(0, 0), tp(TMM, 0), tp(2 * TMM, 0)).is_none());
}

#[test]
fn svg_arc_params_major_arc_sets_large_flag() {
    let r = 10 * TMM;
    let f = |deg: f64| {
        let a = deg.to_radians();
        tp(
            (r as f64 * a.cos()).round() as Nm,
            (r as f64 * a.sin()).round() as Nm,
        )
    };
    // 0°→200°→210°: a 210° CCW major arc.
    let (_, large, sweep) = svg_arc_params(f(0.0), f(200.0), f(210.0)).unwrap();
    assert_eq!(large, 1, "sweep > 180° sets large-arc");
    assert_eq!(sweep, 0, "CCW in model ⇒ sweep 0");
}

#[test]
fn arc_ij_turn_is_exact_and_oriented() {
    let r = 10 * TMM;
    // Upper semicircle: centre origin, start (−R,0) ⇒ I/J = centre − start = (R,0);
    // CW ⇒ turn −1.
    let (ij, turn) = arc_ij_turn(tp(-r, 0), tp(0, r), tp(r, 0)).unwrap();
    assert_eq!(ij, tp(r, 0));
    assert_eq!(turn, -1);
    assert!(arc_ij_turn(tp(0, 0), tp(TMM, 0), tp(2 * TMM, 0)).is_none());
    // Far-from-origin placement: the same arc shifted by (1e9, 1e9) nm must give the
    // identical I/J (the start-relative computation is overflow-safe and invariant).
    let s = 1_000_000_000;
    let (ij2, turn2) = arc_ij_turn(tp(s - r, s), tp(s, s + r), tp(s + r, s)).unwrap();
    assert_eq!(
        (ij2, turn2),
        (tp(r, 0), -1),
        "translation-invariant, no overflow"
    );
}

#[test]
fn svg_path_d_emits_an_arc_command() {
    let d = svg_path_d(&half_disc(10 * TMM), &(|y: Nm| -y));
    assert!(d.starts_with("M "), "{d}");
    assert!(d.contains(" A "), "carries an SVG arc command: {d}");
    assert!(d.ends_with(" Z"), "closed: {d}");
}

#[test]
fn gerber_contour_emits_g02_arc_with_ij() {
    let mut out = String::new();
    let (mut mode, mut g75) = ("G01", false);
    gerber_contour(&half_disc(10 * TMM), &mut out, &mut mode, &mut g75);
    assert!(out.contains("X-10000000Y0D02*"), "move to start:\n{out}");
    assert!(
        out.contains("G75*"),
        "multi-quadrant enabled before the arc:\n{out}"
    );
    assert!(
        out.contains("G02*"),
        "the upper semicircle is CW (G02):\n{out}"
    );
    // Arc to end (R,0) with I/J = centre(0,0) − start(−R,0) = (R, 0).
    assert!(
        out.contains("X10000000Y0I10000000J0D01*"),
        "arc draw with I/J:\n{out}"
    );
    // The flat diameter closes the contour with a straight line back to start.
    assert!(
        out.contains("G01*\nX-10000000Y0D01*"),
        "straight closing edge:\n{out}"
    );
}

/// A filled blob whose top edge is a cubic Bézier, closed by the flat diameter.
fn cubic_blob(r: Nm) -> Shape2D {
    Shape2D::polygon_path(
        crate::geom::Path {
            start: tp(-r, 0),
            segs: vec![Seg::Cubic {
                c1: tp(-r, 2 * r),
                c2: tp(r, 2 * r),
                end: tp(r, 0),
            }],
        },
        0,
    )
}

#[test]
fn svg_path_d_emits_a_cubic_command() {
    let d = svg_path_d(&cubic_blob(10 * TMM), &(|y: Nm| -y));
    assert!(d.starts_with("M "), "{d}");
    assert!(d.contains(" C "), "carries an SVG cubic command: {d}");
    assert!(d.ends_with(" Z"), "closed: {d}");
}

#[test]
fn gerber_contour_flattens_a_bezier_to_g01_lines() {
    // Gerber has no Béziers: the curve must come out as a run of G01 draws, with
    // no arc codes and no SVG-isms.
    let mut out = String::new();
    let (mut mode, mut g75) = ("G01", false);
    gerber_contour(&cubic_blob(10 * TMM), &mut out, &mut mode, &mut g75);
    assert!(
        !out.contains("G02*") && !out.contains("G03*"),
        "a Bézier emits no arc codes:\n{out}"
    );
    let draws = out.matches("D01*").count();
    assert!(
        draws > 2,
        "the Bézier flattens to several G01 draws ({draws}):\n{out}"
    );
    assert!(
        out.contains("X10000000Y0"),
        "reaches the curve endpoint:\n{out}"
    );
}

#[test]
fn arc_board_flattens_to_polyline_in_edge_cuts_and_svg() {
    // A half-disc board authored in the text front-end. Under Decision 16b/c the
    // substrate is a `Shape2D::Area` (a polygonized region), so the curved edge
    // exports as a fine straight-segment polyline (G01 / SVG `L`), not a G02/G03 /
    // SVG `A` arc — the arc is gone once the outline becomes a region. The authored
    // arc still lives in the `Board` directive; only this derived export is flat.
    let lib = part_library();
    let crate::text::Parsed { source: src, .. } =
        crate::text::parse("board (-2mm, 0mm) arc (0mm, 2mm) (2mm, 0mm)").unwrap();
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "arc board")
        .unwrap();
    let doc = h.doc().clone();
    let g = gerber_edge_cuts(&doc, &lib);
    assert!(
        !g.contains("G02*") && !g.contains("G03*"),
        "the arc is flattened — no G02/G03:\n{g}"
    );
    assert!(
        g.matches("D01*").count() > 8,
        "the curved edge draws as many straight G01 segments:\n{g}"
    );
    // The arc endpoints (−2,0) and (2,0) mm are exact ring vertices.
    assert!(
        g.contains("X-2000000Y0") && g.contains("X2000000Y0"),
        "reaches endpoints:\n{g}"
    );
    let s = svg(&doc, &lib).unwrap();
    assert!(
        s.contains("<path class=\"outline-board\""),
        "outline is a path:\n{s}"
    );
    assert!(
        !s.contains(" A "),
        "the polygonized region carries no SVG arc command:\n{s}"
    );
}

#[test]
fn gerber_set_names_and_layers() {
    let (doc, lib) = hand_routed_board();
    let set = gerber_set(&doc, &lib).unwrap();
    let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "board-F_Cu.gbr",
            "board-B_Cu.gbr",
            "board-F_Mask.gbr",
            "board-B_Mask.gbr",
            "board-F_SilkS.gbr",
            "board-B_SilkS.gbr",
            "board-Edge_Cuts.gbr",
            "board-PTH.drl",
        ]
    );
}

#[test]
fn svg_draws_traces_and_vias() {
    let (doc, lib) = hand_routed_board();
    let s = svg(&doc, &lib).unwrap();
    assert!(s.contains("class=\"trace trace-top\""), "got:\n{s}");
    assert!(s.contains("class=\"trace trace-bottom\""));
    assert!(s.contains("class=\"via\""));
    // The polyline carries the trace's mm-formatted vertices.
    assert!(s.contains("6.000000,"));
    assert!(s.trim_end().ends_with("</svg>"));
}

/// A part with real pad geometry flashes as copper (rect + circle apertures).
fn padded_board() -> (Doc, PartLib) {
    let mut lib = part_library();
    let fp = crate::kicad::import_footprint(
        r#"(footprint "PADX"
                (pad "1" smd rect (at -1 0) (size 0.6 1.2) (layers "F.Cu"))
                (pad "2" smd circle (at 1 0) (size 0.8 0.8) (layers "F.Cu")))"#,
    )
    .unwrap();
    lib.insert("PADX".into(), fp);
    let mut h = History::new(Default::default());
    let src = vec![
        G::Instance {
            path: "u1".into(),
            part: "PADX".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "u1".into(),
            pos: Point::mm(5, 5),
        },
    ];
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "p")
        .unwrap();
    (h.doc().clone(), lib)
}

#[test]
fn component_pads_flash_by_shape() {
    let (doc, lib) = padded_board();
    let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
    // Rect pad 0.6x1.2 and circle pad 0.8 become R / C apertures.
    assert!(top.contains("R,0.600000X1.200000*%"), "got:\n{top}");
    assert!(top.contains("C,0.800000*%"), "got:\n{top}");
    // Two flashes at the pads' world positions: u1 at (5,5), pads at -1 / +1 mm.
    assert!(top.contains("X4000000Y5000000D03*"));
    assert!(top.contains("X6000000Y5000000D03*"));
    assert_eq!(top.matches("D03*").count(), 2);
}

#[test]
fn fab_exporters_are_deterministic() {
    let (doc, lib) = hand_routed_board();
    assert_eq!(gerber_set(&doc, &lib), gerber_set(&doc, &lib));
    assert_eq!(
        gerber_layer(&doc, &lib, &cu(&doc, "F.Cu")),
        gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"))
    );
    assert_eq!(excellon_drill(&doc, &lib), excellon_drill(&doc, &lib));
    assert_eq!(gerber_edge_cuts(&doc, &lib), gerber_edge_cuts(&doc, &lib));
}

#[test]
fn gerber_set_on_autorouted_board_is_deterministic() {
    use crate::autoroute::autoroute;
    use crate::route::DesignRules;
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "c0".into(),
            pos: Point::mm(12, 5),
        },
        G::Place {
            path: "c1".into(),
            pos: Point::mm(12, -5),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("reg".into(), "VOUT".into()),
                ("c0".into(), "p1".into()),
                ("c1".into(), "p1".into()),
            ],
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![
                ("reg".into(), "GND".into()),
                ("c0".into(), "p2".into()),
                ("c1".into(), "p2".into()),
            ],
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "place")
        .unwrap();
    let result = autoroute(h.doc(), &lib, &DesignRules::default());
    h.commit(Transaction(result.commands), &lib, "route")
        .unwrap();
    let doc = h.doc();
    // The autorouter laid real copper, so the F_Cu Gerber has trace draws.
    assert!(!doc.traces.is_empty());
    let top = gerber_layer(doc, &lib, &cu(doc, "F.Cu"));
    assert!(top.matches("D01*").count() > 0);
    assert_eq!(gerber_set(doc, &lib), gerber_set(doc, &lib));
}

// --- copper pour export (0004 stage 5) --------------------------------

/// A 20x20 board with a GND pour on F.Cu and a foreign SIG pad (knocked out).
fn poured_board() -> (Doc, PartLib) {
    use crate::elaborate::RegionDecl;
    use crate::geom::Role;
    let mut lib = part_library();
    let pad = crate::kicad::import_footprint(
        r#"(footprint "P1" (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
    )
    .unwrap();
    lib.insert("P1".into(), pad);
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 20),
        Point::mm(0, 20),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "g".into(),
            part: "P1".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "s".into(),
            part: "P1".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "s".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("s".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "pour")
        .unwrap();
    (h.doc().clone(), lib)
}

#[test]
fn gerber_emits_pour_region_fill() {
    let (doc, lib) = poured_board();
    let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
    assert!(top.contains("G36*"), "pour region opens:\n{top}");
    assert!(top.contains("G37*"), "pour region closes");
    // Outer board contour + a knockout hole around the SIG pad ⇒ ≥2 contours
    // (≥2 D02 moves) inside the single G36/G37 block.
    let block = top
        .split("G36*")
        .nth(1)
        .unwrap()
        .split("G37*")
        .next()
        .unwrap();
    assert!(
        block.matches("D02*").count() >= 2,
        "outer + hole contours:\n{block}"
    );
    // The bottom layer carries no pour.
    assert!(!gerber_layer(&doc, &lib, &cu(&doc, "B.Cu")).contains("G36*"));
}

#[test]
fn svg_draws_pour_with_holes() {
    let (doc, lib) = poured_board();
    let s = svg(&doc, &lib).unwrap();
    assert!(
        s.contains("class=\"pour pour-top\""),
        "pour path present:\n{s}"
    );
    assert!(s.contains("fill-rule=\"evenodd\""), "holes via even-odd");
    assert!(s.contains("data-net=\"GND\""));
}

#[test]
fn fab_with_pour_is_deterministic() {
    let (doc, lib) = poured_board();
    assert_eq!(gerber_set(&doc, &lib), gerber_set(&doc, &lib));
    assert_eq!(svg(&doc, &lib), svg(&doc, &lib));
}

// --- solder mask (0004 stage 6) ---------------------------------------

#[test]
fn solder_mask_opens_over_pads_with_expansion() {
    // padded_board has an F.Cu rect pad 0.6x1.2 and a circle pad 0.8. The mask
    // opening inflates each by 0.05mm per side: rect → 0.7x1.3, circle → 0.9.
    let (doc, lib) = padded_board();
    let f = gerber_mask(&doc, &lib, &mask_of(&doc, Layer::Top)).unwrap();
    assert!(f.contains("F_Mask"));
    assert!(
        f.contains("R,0.700000X1.300000*%"),
        "expanded rect opening:\n{f}"
    );
    assert!(f.contains("C,0.900000*%"), "expanded circle opening:\n{f}");
    assert_eq!(f.matches("D03*").count(), 2, "one opening per pad");
    // No bottom-side pads ⇒ no openings on B_Mask.
    assert_eq!(
        gerber_mask(&doc, &lib, &mask_of(&doc, Layer::Bottom))
            .unwrap()
            .matches("D03*")
            .count(),
        0
    );
}

#[test]
fn through_hole_pad_opens_both_masks() {
    let mut lib = part_library();
    let fp = crate::kicad::import_footprint(
            r#"(footprint "TH" (pad "1" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8) (layers "*.Cu")))"#,
        )
        .unwrap();
    lib.insert("TH".into(), fp);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            G::Instance {
                path: "j".into(),
                part: "TH".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "j".into(),
                pos: Point::mm(5, 5),
            },
        ])),
        &lib,
        "th",
    )
    .unwrap();
    let doc = h.doc();
    // A through-hole pad is exposed on both faces, so it opens on both masks. Its
    // drill `Void` is a through-cut (full-stack z), not a mask-slab opening, so it is
    // NOT an extra flash — the count stays one opening per side.
    assert_eq!(
        gerber_mask(doc, &lib, &mask_of(doc, Layer::Top))
            .unwrap()
            .matches("D03*")
            .count(),
        1
    );
    assert_eq!(
        gerber_mask(doc, &lib, &mask_of(doc, Layer::Bottom))
            .unwrap()
            .matches("D03*")
            .count(),
        1
    );
}

/// New capability (Decision 13): a board cutout removes solder mask over its whole
/// area, so it appears on the mask as a `G36`/`G37` region fill. The old parallel
/// rule (pad-copper + expansion only) missed cutouts entirely.
#[test]
fn mask_gerber_includes_board_cutout() {
    let lib = part_library();
    let cutout = Shape2D::polygon(vec![
        Point::mm(8, 8),
        Point::mm(12, 8),
        Point::mm(12, 12),
        Point::mm(8, 12),
    ]);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Cutout { shape: cutout },
        ])),
        &lib,
        "cut",
    )
    .unwrap();
    let f = gerber_mask(h.doc(), &lib, &mask_of(h.doc(), Layer::Top)).unwrap();
    assert!(f.contains("G36*"), "cutout opens a mask region:\n{f}");
    assert!(f.contains("G37*"), "region closes:\n{f}");
    // The cutout corner (12mm) is drawn in the region contour (nm coordinates).
    assert!(f.contains("X12000000Y12000000"), "cutout boundary:\n{f}");
    // Both faces lose mask over a through cutout.
    assert!(
        gerber_mask(h.doc(), &lib, &mask_of(h.doc(), Layer::Bottom))
            .unwrap()
            .contains("G36*")
    );
}

/// A board with a cutout: the substrate is one `Area` (outline ∖ cutout), so
/// `Edge.Cuts` draws both the outer boundary and the cutout hole ring, and the SVG
/// `outline-board` path carries the cutout ring too (Decision 16b/c).
#[test]
fn edge_cuts_and_svg_include_board_cutout() {
    let lib = part_library();
    let cutout = Shape2D::polygon(vec![
        Point::mm(8, 8),
        Point::mm(12, 8),
        Point::mm(12, 12),
        Point::mm(8, 12),
    ]);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Cutout { shape: cutout },
        ])),
        &lib,
        "cut",
    )
    .unwrap();
    let doc = h.doc();

    let e = gerber_edge_cuts(doc, &lib);
    assert!(
        e.contains("X20000000Y20000000"),
        "outer boundary corner:\n{e}"
    );
    assert!(e.contains("X12000000Y12000000"), "cutout ring corner:\n{e}");
    assert!(
        e.matches("D02*").count() >= 2,
        "outer + cutout are two closed contours:\n{e}"
    );

    let s = svg(doc, &lib).unwrap();
    assert!(
        s.contains("class=\"outline-board\""),
        "board outline path:\n{s}"
    );
    // The cutout's 8/12 mm coordinates appear only in the cutout ring (the outer
    // square is 0/20 mm), and the path has a second subpath (the hole).
    assert!(
        s.contains("12.000000,12.000000") && s.contains("8.000000,8.000000"),
        "cutout ring in the svg path:\n{s}"
    );
    assert!(
        s.matches(" M").count() + s.matches("\"M").count() >= 2,
        "outline path has an outer subpath and a cutout subpath:\n{s}"
    );
}

// --- silk Gerbers (Decision 13, stage 2b) -----------------------------

/// The default fileset carries an F and B silk Gerber, and board text on F.SilkS
/// comes out on the F silk layer as centreline draws with a round pen aperture.
#[test]
fn silk_gerber_draws_text_strokes_with_aperture() {
    use crate::doc::Orient;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Text {
                string: "R1".into(),
                at: Point::mm(2, 10),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            },
        ])),
        &lib,
        "silk-text",
    )
    .unwrap();
    let doc = h.doc();
    // The fileset exposes both silk layers.
    let set = gerber_set(doc, &lib).unwrap();
    let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"board-F_SilkS.gbr"),
        "F silk file: {names:?}"
    );
    assert!(
        names.contains(&"board-B_SilkS.gbr"),
        "B silk file: {names:?}"
    );

    let su = crate::elaborate::stackup(&doc.source);
    let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
    let g = gerber_silk(doc, &lib, fsilk).unwrap();
    // A round pen aperture (the text stroke width = height/8 = 0.125mm) and real draws.
    assert!(g.contains("C,0.125000*%"), "round silk pen aperture:\n{g}");
    assert!(g.matches("D01*").count() > 2, "text strokes draw:\n{g}");
    // The empty B silk layer carries no draws.
    let bsilk = su.slabs.iter().find(|s| s.name == "B.SilkS").unwrap();
    assert_eq!(
        gerber_silk(doc, &lib, bsilk)
            .unwrap()
            .matches("D01*")
            .count(),
        0
    );
}

/// A footprint `fp_poly` on silk is a filled area, so it comes out as a `G36`/`G37`
/// region (not a zero-width stroke).
#[test]
fn silk_gerber_fp_poly_is_a_region() {
    let mut lib = PartLib::new();
    let part = crate::kicad::import_footprint(
        r#"(footprint "TRI"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.SilkS")))"#,
    )
    .unwrap();
    lib.insert("TRI".into(), part);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "TRI".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "tri",
    )
    .unwrap();
    let doc = h.doc();
    let su = crate::elaborate::stackup(&doc.source);
    let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
    let g = gerber_silk(doc, &lib, fsilk).unwrap();
    assert!(
        g.contains("G36*") && g.contains("G37*"),
        "fp_poly is a region:\n{g}"
    );
}

/// Regression: a straight silk stroke following an arc-bearing one must switch the
/// interpolation mode back to `G01` before its line draw. Aperture (D-code) selection
/// does not reset the modal G01/G02/G03 state, so without the transition the line
/// would be emitted while still in arc mode (a malformed draw).
#[test]
fn silk_gerber_line_after_arc_returns_to_g01() {
    let mut lib = PartLib::new();
    // An fp_arc (emits G02/G03) declared before an fp_line (a straight draw), same
    // pen width so they share one aperture — exactly the order that tripped the bug.
    let part = crate::kicad::import_footprint(
        r#"(footprint "ARCLINE"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_arc (start -2 0) (mid 0 2) (end 2 0) (stroke (width 0.2)) (layer "F.SilkS"))
                (fp_line (start 3 0) (end 5 0) (stroke (width 0.2)) (layer "F.SilkS")))"#,
    )
    .unwrap();
    lib.insert("ARCLINE".into(), part);
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![G::Instance {
            path: "u1".into(),
            part: "ARCLINE".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }])),
        &lib,
        "arcline",
    )
    .unwrap();
    let doc = h.doc();
    let su = crate::elaborate::stackup(&doc.source);
    let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
    let g = gerber_silk(doc, &lib, fsilk).unwrap();

    // An arc really was emitted...
    let arc_pos = g
        .find("G03*")
        .or_else(|| g.find("G02*"))
        .expect("fp_arc emits a G02/G03 draw");
    // ...and a G01* returns before the fp_line is drawn.
    assert!(
        g[arc_pos..].contains("G01*"),
        "a straight stroke after an arc must switch back to G01:\n{g}"
    );
    // The fp_line reaches its endpoint (5mm) as a plain line draw, never a degenerate
    // arc (an arc draw carries I/J offsets; a stuck-in-arc-mode line would not).
    assert!(
        g.contains("X5000000Y0D01*"),
        "fp_line drawn as a straight D01:\n{g}"
    );
}

/// SVG splits silk by side: a bottom-side marking gets `class="silk-bottom"`, while
/// top-side silk keeps `class="silk"` (existing single-side fixtures unchanged).
#[test]
fn svg_bottom_silk_gets_bottom_class() {
    use crate::doc::Orient;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Text {
                string: "B1".into(),
                at: Point::mm(2, 10),
                height: MM,
                layer: "B.SilkS".into(),
                orient: Orient::IDENTITY,
            },
        ])),
        &lib,
        "b-silk",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("class=\"silk-bottom\""),
        "bottom silk gets its own class:\n{s}"
    );
    assert!(
        !s.contains("class=\"silk\" "),
        "no top-silk class for a bottom-only board:\n{s}"
    );
}

// --- fab drawing (Decision 15 consumer) -------------------------------

/// The default 2-layer stackup with an added zero-height `F.Fab` datum slab at the
/// F.Cu top face — the way a user authors a fab slab (Decision 15). Returned as `Slab`
/// directives so `elaborate::stackup` picks them up.
fn stackup_with_fab() -> Vec<crate::elaborate::GenDirective> {
    use crate::elaborate::GenDirective as G;
    let mut slabs = Stackup::default_2layer().slabs;
    let top = slabs.iter().find(|s| s.name == "F.Cu").unwrap().z.hi;
    slabs.push(Slab {
        name: "F.Fab".into(),
        z: ZRange::new(top, top),
        role: Role::Datum,
        material: None,
    });
    slabs.into_iter().map(G::Slab).collect()
}

/// A footprint carrying an SMD pad, an `F.Fab` graphic line, and an `F.Fab` `user`
/// text anchor (imported as a `Literal`) — the three fab-layer inputs the drawing pass
/// must render.
fn fab_footprint() -> PartDef {
    crate::kicad::import_footprint(
        r#"(footprint "FAB"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "F.Fab"))
                (fp_text user "FAB1" (at 0 1) (layer "F.Fab") (effects (font (size 1 1)))))"#,
    )
    .unwrap()
}

/// An authored `F.Fab` slab plus a footprint with fab graphics and a fab text anchor
/// emits a fab SVG that carries both the graphic stroke and the text strokes — the
/// consumer that closes the "authored fab slab renders nowhere" gap (Decision 15).
#[test]
fn fab_svg_emitted_with_graphics_and_text() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert("FAB".into(), fab_footprint());
    let mut source = stackup_with_fab();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Instance {
        path: "u".into(),
        part: "FAB".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(5, 5),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "fab")
        .unwrap();
    let doc = h.doc();

    let set = fab_svg_set(doc, &lib).unwrap();
    assert_eq!(set.len(), 1, "one fab slab ⇒ one fab SVG");
    let (name, svg) = &set[0];
    assert_eq!(name, "board-F_Fab.svg");
    // Board outline for context.
    assert!(svg.contains("class=\"outline-board\""), "outline:\n{svg}");
    // The fab graphic line draws as a fab-class stroke.
    assert!(
        svg.contains("class=\"fab\""),
        "fab graphic + text render as fab strokes:\n{svg}"
    );
    // Text lowers to several glyph strokes ⇒ more than the single graphic line.
    assert!(
        svg.matches("class=\"fab\"").count() >= 3,
        "graphic line + multiple text strokes expected:\n{svg}"
    );
    assert_eq!(
        fab_svg_set(doc, &lib),
        fab_svg_set(doc, &lib),
        "deterministic"
    );
}

/// With **no** fab slab authored (the default stackup), the fab fileset is empty and
/// fab-layer footprint graphics stay invisible in every other output — the Decision 15
/// contract (a fab graphic materializes only when a fab slab exists).
#[test]
fn no_fab_slab_means_no_fab_output_and_invisible_graphics() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert("FAB".into(), fab_footprint());
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "u".into(),
                part: "FAB".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "u".into(),
                pos: Point::mm(5, 5),
            },
        ])),
        &lib,
        "no-fab",
    )
    .unwrap();
    let doc = h.doc();

    // No fab SVG.
    assert!(
        fab_svg_set(doc, &lib).unwrap().is_empty(),
        "no fab slab ⇒ no fab file"
    );
    // The fab graphic is inert everywhere else: not in the SVG, not in the Gerber set.
    let s = svg(doc, &lib).unwrap();
    assert!(
        !s.contains("class=\"fab\""),
        "fab graphic must not leak into the SVG:\n{s}"
    );
    let gset = gerber_set(doc, &lib).unwrap();
    assert!(
        gset.iter().all(|(n, _)| !n.contains("Fab")),
        "no fab Gerber in the fileset: {:?}",
        gset.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

/// A bottom fab slab (`B.Fab`) renders with the bottom-side class, mirroring the silk
/// side split. Driven by a footprint carrying a `B.Fab` graphic (placed top-side, so
/// `swap_side` leaves it on `B.Fab`) — the footprint path is role-driven off the slab,
/// so a `Role::Datum` `B.Fab` slab produces a bottom-side fab feature.
#[test]
fn bottom_fab_gets_bottom_class() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert(
        "BFAB".into(),
        crate::kicad::import_footprint(
            r#"(footprint "BFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "B.Fab")))"#,
        )
        .unwrap(),
    );
    let mut slabs = Stackup::default_2layer().slabs;
    let bot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
    slabs.push(Slab {
        name: "B.Fab".into(),
        z: ZRange::new(bot, bot),
        role: Role::Datum,
        material: None,
    });
    let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Instance {
        path: "u".into(),
        part: "BFAB".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(5, 5),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "bfab")
        .unwrap();
    let set = fab_svg_set(h.doc(), &lib).unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].0, "board-B_Fab.svg");
    assert!(
        set[0].1.contains("class=\"fab-bottom\""),
        "bottom fab gets its own class:\n{}",
        set[0].1
    );
}

// --- fab Gerber (Decision 15/16) --------------------------------------

/// An authored `F.Fab` slab plus a footprint with a fab graphic line and a fab text
/// anchor emits a fab Gerber carrying real stroke draws (the graphic + glyph strokes),
/// and the fileset lists `board-F_Fab.gbr` — the Gerber sibling of the fab SVG.
#[test]
fn fab_gerber_emitted_with_strokes() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert("FAB".into(), fab_footprint());
    let mut source = stackup_with_fab();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Instance {
        path: "u".into(),
        part: "FAB".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(5, 5),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "fab")
        .unwrap();
    let doc = h.doc();

    // The fileset exposes the fab Gerber.
    let set = gerber_set(doc, &lib).unwrap();
    let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"board-F_Fab.gbr"),
        "fab Gerber in the fileset: {names:?}"
    );

    let su = crate::elaborate::stackup(&doc.source);
    let fab = su.slab("F.Fab").unwrap();
    let g = gerber_fab(doc, &lib, fab).unwrap();
    // A round pen aperture (the text stroke width = height/8 = 0.125mm) and real draws
    // (the graphic line + the glyph strokes).
    assert!(g.contains("C,0.125000*%"), "round fab pen aperture:\n{g}");
    assert!(g.matches("D01*").count() > 2, "fab strokes draw:\n{g}");
    assert_eq!(g, gerber_fab(doc, &lib, fab).unwrap(), "deterministic");
}

/// A fab `fp_poly` is a filled area, so it comes out as a `G36`/`G37` region fill on the
/// fab Gerber (the same area path silk uses) — exercising the region-fill arm.
#[test]
fn fab_gerber_fp_poly_is_a_region() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert(
        "FABTRI".into(),
        crate::kicad::import_footprint(
            r#"(footprint "FABTRI"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.Fab")))"#,
        )
        .unwrap(),
    );
    let mut source = stackup_with_fab();
    source.push(G::Instance {
        path: "u".into(),
        part: "FABTRI".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(5, 5),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "fabtri")
        .unwrap();
    let doc = h.doc();
    let su = crate::elaborate::stackup(&doc.source);
    let fab = su.slab("F.Fab").unwrap();
    let g = gerber_fab(doc, &lib, fab).unwrap();
    assert!(
        g.contains("G36*") && g.contains("G37*"),
        "fab fp_poly is a region:\n{g}"
    );
}

/// A bottom fab Gerber is **not** mirrored: coordinates are board-frame (the viewer
/// flips a `B.Fab` document layer), matching the bottom-silk Gerber convention — unlike
/// the per-side fab *SVG*, which mirrors x. Drive it with a `B.Fab` graphic whose end
/// point (world x) must appear verbatim in the Gerber.
#[test]
fn bottom_fab_gerber_is_not_mirrored() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert(
        "BFAB".into(),
        crate::kicad::import_footprint(
            r#"(footprint "BFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "B.Fab")))"#,
        )
        .unwrap(),
    );
    let mut slabs = Stackup::default_2layer().slabs;
    let bot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
    slabs.push(Slab {
        name: "B.Fab".into(),
        z: ZRange::new(bot, bot),
        role: Role::Datum,
        material: None,
    });
    let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Instance {
        path: "u".into(),
        part: "BFAB".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(5, 5),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "bfab")
        .unwrap();
    let doc = h.doc();
    let su = crate::elaborate::stackup(&doc.source);
    let fab = su.slab("B.Fab").unwrap();
    let g = gerber_fab(doc, &lib, fab).unwrap();
    // The line runs from x=5mm to x=6mm (place at 5, end offset +1mm), both in the raw
    // board frame — a mirrored export would place them elsewhere. `%FSLAX46Y46*%` mm =
    // nm, so 6mm is the integer 6000000.
    assert!(g.contains("X6000000"), "unmirrored world x for B.Fab:\n{g}");
    // The fileset names it board-B_Fab.gbr.
    let set = gerber_set(doc, &lib).unwrap();
    assert!(
        set.iter().any(|(n, _)| n == "board-B_Fab.gbr"),
        "bottom fab Gerber named board-B_Fab.gbr"
    );
}

/// No fab slab authored (default stackup) ⇒ no fab Gerber in the fileset, and a
/// fab-layer footprint graphic stays inert — the Decision 15 contract on the Gerber
/// side (the SVG side is covered by `no_fab_slab_means_no_fab_output_and_invisible_graphics`).
#[test]
fn no_fab_slab_means_no_fab_gerber() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert("FAB".into(), fab_footprint());
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "u".into(),
                part: "FAB".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "u".into(),
                pos: Point::mm(5, 5),
            },
        ])),
        &lib,
        "no-fab",
    )
    .unwrap();
    let gset = gerber_set(h.doc(), &lib).unwrap();
    assert!(
        gset.iter().all(|(n, _)| !n.contains("Fab")),
        "no fab Gerber in the fileset: {:?}",
        gset.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

// --- mask export enters by role (Decision 16 stage 4) -----------------

/// A custom stackup with a single `Role::Mask` slab exports exactly one mask Gerber,
/// named from that slab — the mask loop iterates mask slabs by name, not a fixed
/// `[Top, Bottom]` copper-layer pair.
#[test]
fn single_mask_slab_exports_one_mask_gerber() {
    use crate::elaborate::GenDirective as G;
    // A 1-layer stackup: one copper slab and one mask slab above it.
    let slabs = vec![
        Slab {
            name: "F.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: None,
        },
        Slab {
            name: "F.Mask".into(),
            z: ZRange::new(35_000, 45_000),
            role: Role::Mask,
            material: None,
        },
    ];
    let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
    source.push(board_rect(Point::mm(0, 0), Point::mm(10, 10)));
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "1mask")
        .unwrap();
    let gset = gerber_set(h.doc(), &lib).unwrap();
    let masks: Vec<&String> = gset
        .iter()
        .map(|(n, _)| n)
        .filter(|n| n.contains("Mask"))
        .collect();
    assert_eq!(masks, vec!["board-F_Mask.gbr"], "exactly one mask Gerber");
}

/// Board-level `text` on a fab slab renders on the fab SVG and is **absent** from silk
/// (F1): the text lowering forward-queries the resolved slab's role rather than
/// hardcoding `Role::Marking`, so `layer=F.Fab` (a `Role::Datum` slab) lands on fab,
/// not silk. Before the fix this text shipped visibly on `F_SilkS`.
#[test]
fn board_text_on_fab_slab_renders_fab_not_silk() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut source = stackup_with_fab();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Text {
        string: "FAB".into(),
        at: Point::mm(4, 10),
        height: MM,
        layer: "F.Fab".into(),
        orient: crate::doc::Orient::IDENTITY,
    });
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(source)),
        &lib,
        "fabtext",
    )
    .unwrap();
    let doc = h.doc();

    // Fab SVG carries the text strokes.
    let set = fab_svg_set(doc, &lib).unwrap();
    assert_eq!(set.len(), 1);
    assert!(
        set[0].1.matches("class=\"fab\"").count() >= 3,
        "fab-slab board text renders as fab strokes:\n{}",
        set[0].1
    );
    // The composite SVG and silk Gerbers must NOT show it as silk.
    let s = svg(doc, &lib).unwrap();
    assert!(
        !s.contains("class=\"silk\""),
        "fab-slab board text must not leak onto silk:\n{s}"
    );
    // The F.SilkS silk Gerber is empty of drawing ops (no D-code selection / strokes).
    let su = crate::elaborate::stackup(&doc.source);
    let silk = su.slab("F.SilkS").unwrap();
    let g = gerber_silk(doc, &lib, silk).unwrap();
    assert!(
        !g.contains("D10*"),
        "no strokes on the silk Gerber for fab-slab text:\n{g}"
    );
}

/// Board-level `text` on a silk slab is unchanged by the F1 fix — it still lowers to a
/// `Role::Marking` silk stroke (silk byte-identity for the default stackup).
#[test]
fn board_text_on_silk_slab_unchanged() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Text {
                string: "S".into(),
                at: Point::mm(4, 10),
                height: MM,
                layer: "F.SilkS".into(),
                orient: crate::doc::Orient::IDENTITY,
            },
        ])),
        &lib,
        "silktext",
    )
    .unwrap();
    let s = svg(h.doc(), &lib).unwrap();
    assert!(
        s.contains("class=\"silk\""),
        "silk-slab board text still renders as silk strokes:\n{s}"
    );
}

/// F3: a footprint `F.Fab` graphic on a **flipped** component swaps to `B.Fab`
/// (`swap_side`) and lands on the bottom fab sheet — the same side derivation copper
/// uses. With both fab slabs authored, the graphic appears only on `board-B_Fab.svg`.
#[test]
fn flipped_component_fab_graphic_lands_on_bottom_sheet() {
    use crate::elaborate::GenDirective as G;
    let mut lib = part_library();
    lib.insert("FAB".into(), fab_footprint()); // authors an F.Fab graphic + text
    // Default stackup + both F.Fab and B.Fab datum slabs.
    let mut slabs = Stackup::default_2layer().slabs;
    let ftop = slabs.iter().find(|s| s.name == "F.Cu").unwrap().z.hi;
    let bbot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
    slabs.push(Slab {
        name: "F.Fab".into(),
        z: ZRange::new(ftop, ftop),
        role: Role::Datum,
        material: None,
    });
    slabs.push(Slab {
        name: "B.Fab".into(),
        z: ZRange::new(bbot, bbot),
        role: Role::Datum,
        material: None,
    });
    let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
    source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
    source.push(G::Instance {
        path: "u".into(),
        part: "FAB".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    });
    source.push(G::Place {
        path: "u".into(),
        pos: Point::mm(10, 10),
    });
    source.push(G::Rotate {
        path: "u".into(),
        orient: crate::doc::Orient::default().flipped(),
    });
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(source)), &lib, "flip")
        .unwrap();
    let doc = h.doc();

    let set = fab_svg_set(doc, &lib).unwrap();
    // Two fab slabs ⇒ two SVGs; the flipped graphic draws on B.Fab, not F.Fab.
    let by_name = |name: &str| set.iter().find(|(n, _)| n == name).map(|(_, c)| c.as_str());
    let f = by_name("board-F_Fab.svg").expect("F.Fab sheet present");
    let b = by_name("board-B_Fab.svg").expect("B.Fab sheet present");
    assert!(
        !f.contains("class=\"fab\""),
        "flipped graphic is NOT on the front sheet:\n{f}"
    );
    assert!(
        b.contains("class=\"fab-bottom\""),
        "flipped graphic swaps to the bottom sheet:\n{b}"
    );
}

/// An authored-but-empty fab slab (no fab geometry, no board outline) emits a valid SVG
/// via the fallback 10mm viewBox — the degenerate path must not panic or produce an
/// empty viewBox.
#[test]
fn empty_fab_slab_emits_valid_svg() {
    use crate::elaborate::GenDirective as G;
    let lib = part_library();
    // A stackup with one copper slab and one fab slab, and NO board / geometry.
    let source = vec![
        G::Slab(Slab {
            name: "F.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: None,
        }),
        G::Slab(Slab {
            name: "F.Fab".into(),
            z: ZRange::new(35_000, 35_000),
            role: Role::Datum,
            material: None,
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(source)),
        &lib,
        "emptyfab",
    )
    .unwrap();
    let set = fab_svg_set(h.doc(), &lib).unwrap();
    assert_eq!(set.len(), 1);
    let (name, s) = &set[0];
    assert_eq!(name, "board-F_Fab.svg");
    // Fallback bbox path: a 10mm box + margin ⇒ a 14mm-wide non-degenerate viewBox.
    assert!(
        s.contains("viewBox=\"-2.000000 -2.000000 14.000000 14.000000\""),
        "fallback viewBox:\n{s}"
    );
    assert!(
        s.contains("class=\"outline-bbox\""),
        "fallback outline rect:\n{s}"
    );
    assert!(s.ends_with("</svg>\n"));
}
