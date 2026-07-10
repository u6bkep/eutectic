use super::*;
use crate::command::{Command, Transaction};
use crate::doc::{MM, Point};
use crate::elaborate::{GenDirective as G, RegionDecl, board_rect};
use crate::geom::{Material, Role, Shape2D, Slab, ZRange};
use crate::history::History;
use crate::part::part_library;

/// The router's ordinal↔name boundary (Decision 13 rule 2): `Top`/`Bottom` resolve
/// to the outer copper slab names and round-trip through `slab_layer`; `Inner(0)`
/// must NOT alias onto Bottom on a 2-layer stackup (there is no inner copper).
#[test]
fn router_layer_name_boundary_round_trips() {
    let su = crate::geom::Stackup::default_2layer();
    assert_eq!(layer_slab_name(&su, Layer::Top).as_deref(), Some("F.Cu"));
    assert_eq!(layer_slab_name(&su, Layer::Bottom).as_deref(), Some("B.Cu"));
    assert_eq!(
        layer_slab_name(&su, Layer::Inner(0)),
        None,
        "a 2-layer stackup has no inner copper layer"
    );
    // Names round-trip back to ordinals.
    assert_eq!(slab_layer(&su, "F.Cu"), Some(Layer::Top));
    assert_eq!(slab_layer(&su, "B.Cu"), Some(Layer::Bottom));
    // A non-copper / unknown name resolves to no ordinal.
    assert_eq!(slab_layer(&su, "F.SilkS"), None);
    assert_eq!(slab_layer(&su, "Nope"), None);
}

/// Netlist (membership only; roles irrelevant to pours) from a doc's nets.
fn netlist_of(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|pr| (pr.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}

/// One single-pad footprint on the given copper layer, so a placed instance's pad
/// copper sits exactly at the instance origin (1mm square).
fn one_pad(layer: &str) -> crate::part::PartDef {
    crate::kicad::import_footprint(&format!(
        r#"(footprint "P1" (pad "1" smd rect (at 0 0) (size 1 1) (layers "{layer}")))"#
    ))
    .unwrap()
}

fn board_pour_scene(sig_layer: &str) -> (Doc, PartLib) {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    lib.insert("PS".into(), one_pad(sig_layer));
    // A board-covering GND pour on F.Cu; a GND pad at (5,5), a foreign SIG pad at
    // (15,5).
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
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "s".into(),
            part: "PS".into(),
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
        .expect("elaborates");
    (h.doc().clone(), lib)
}

/// The `world_features` text seam (Decision 17): footprint labels lowered through the
/// unified producer honour the doc-wide `font`. A part with an `O` literal anchor,
/// under a `font` directive resolving to the test TTF, yields a `Role::Marking`
/// **filled `Area`** (outline glyph) in the world-feature stream — proving the font is
/// threaded to `world_features`' `part::text_features` call, not just the export one.
#[test]
fn world_features_footprint_text_honours_ttf_font() {
    // The test TTF, written to a temp file (fonts resolve by path).
    let mut path = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!(
        "eutectic-route-ttf-{}-{stamp}.ttf",
        std::process::id()
    ));
    std::fs::write(&path, crate::ttf::build_test_ttf()).unwrap();

    // A footprint carrying a single silk text anchor.
    let mut lib = part_library();
    lib.insert(
        "LBL".into(),
        crate::part::PartDef {
            name: "LBL".into(),
            pins: vec![],
            interfaces: std::collections::BTreeMap::new(),
            graphics: vec![],
            texts: vec![crate::part::FpText {
                kind: crate::part::FpTextKind::Literal("O".into()),
                at: Point { x: 0, y: 0 },
                height: MM,
                layer: "F.SilkS".into(),
                orient: crate::doc::Orient::default(),
                hide: false,
            }],
            courtyard: None,
            class: None,
        },
    );
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Font {
            path: path.to_string_lossy().into_owned(),
        },
        G::Instance {
            path: "u".into(),
            part: "LBL".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "ttf")
        .expect("elaborates");
    let doc = h.doc().clone();

    let su = stackup(&doc.source);
    let world =
        world_features(&doc, &lib, &netlist_of(&doc), &DesignRules::default(), &su).unwrap();
    let ttf_marks = world
            .iter()
            .filter(|nf| {
                nf.feature.role == Role::Marking
                    && matches!(&nf.feature.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. }))
            })
            .count();
    assert!(
        ttf_marks >= 1,
        "footprint text reached world_features as a filled Area (TTF), got {ttf_marks}"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn pour_knocks_out_foreign_keeps_same_net() {
    let (doc, lib) = board_pour_scene("F.Cu");
    let nl = netlist_of(&doc);
    let fills = pours(
        &doc,
        &lib,
        &nl,
        &DesignRules::default(),
        &stackup(&doc.source),
    );
    assert_eq!(fills.len(), 1, "one conductor pour");
    let f = &fills[0];
    assert_eq!(f.net, NetId::new("GND"));
    assert_eq!(f.layer, "F.Cu");
    // Same-net pad stays inside the pour (it connects to it).
    assert!(
        f.fill.contains_point(Point::mm(5, 5)),
        "GND pad inside the pour"
    );
    // Foreign pad is knocked out, with clearance: its centre and a point just
    // inside the clearance ring are not copper; a point beyond the ring is.
    assert!(
        !f.fill.contains_point(Point::mm(15, 5)),
        "SIG pad knocked out"
    );
    assert!(
        !f.fill.contains_point(Point {
            x: 14_400_000,
            y: 5 * MM
        }),
        "inside clearance ring"
    );
    assert!(
        f.fill.contains_point(Point::mm(14, 5)),
        "beyond the clearance ring is copper"
    );
    // Open board area is copper.
    assert!(f.fill.contains_point(Point::mm(10, 15)));
}

#[test]
fn pour_ignores_foreign_copper_on_other_layers() {
    // The SIG pad now lives on B.Cu; a Top pour must not knock it out.
    let (doc, lib) = board_pour_scene("B.Cu");
    let nl = netlist_of(&doc);
    let fills = pours(
        &doc,
        &lib,
        &nl,
        &DesignRules::default(),
        &stackup(&doc.source),
    );
    assert!(
        fills[0].fill.contains_point(Point::mm(15, 5)),
        "different-layer copper is not knocked out"
    );
}

#[test]
fn pour_on_unknown_net_is_rejected() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Instance {
            path: "g".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
            role: Role::Conductor,
            net: Some("GDN".into()), // typo
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    let err = h
        .commit(Transaction::one(Command::SetSource(src)), &lib, "bad")
        .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_UNKNOWN_NET"),
        "typo'd pour net is a hard fault: {err:?}"
    );
}

#[test]
fn conductor_pour_without_net_is_rejected() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
            role: Role::Conductor,
            net: None,
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    let err = h
        .commit(Transaction::one(Command::SetSource(src)), &lib, "nonet")
        .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_POUR_NO_NET"),
        "netless conductor pour rejected: {err:?}"
    );
}

#[test]
fn conductor_pour_on_non_copper_slab_is_rejected() {
    // A net-bound copper pour targeting the silk slab is nonsense (Decision 13): a
    // hard commit fault, and `pour_fills` never sees it.
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.SilkS".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    // (The unconnected net also faults; collect-all surfaces both — we assert the
    // slab fault is present.)
    let err = h
        .commit(Transaction::one(Command::SetSource(src)), &lib, "silkpour")
        .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_POUR_NON_COPPER"),
        "pour on silk rejected: {err:?}"
    );
}

#[test]
fn region_on_unknown_slab_is_rejected() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
            role: Role::Keepout(crate::geom::KeepoutKind::Copper),
            net: None,
            layer: "Z.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    let err = h
        .commit(Transaction::one(Command::SetSource(src)), &lib, "badslab")
        .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
        "unknown slab rejected: {err:?}"
    );
}

#[test]
fn pours_are_deterministic() {
    let (doc, lib) = board_pour_scene("F.Cu");
    let nl = netlist_of(&doc);
    let rules = DesignRules::default();
    assert_eq!(
        pours(&doc, &lib, &nl, &rules, &stackup(&doc.source)),
        pours(&doc, &lib, &nl, &rules, &stackup(&doc.source))
    );
}

fn drc(doc: &Doc, lib: &PartLib) -> Vec<Violation> {
    check_drc(doc, lib, &netlist_of(doc), &DesignRules::default())
}

/// Two GND pads with no traces are unrouted — until a GND pour covers them, which
/// collapses the ratsnest (the headline pour win).
#[test]
fn pour_connects_same_net_pads() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 20),
        Point::mm(0, 20),
    ]);
    let base = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "g1".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(15, 15),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
    ];
    // Without a pour and without traces: GND's two pads are disconnected.
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(base.clone())),
        &lib,
        "no-pour",
    )
    .unwrap();
    assert!(
        drc(h.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "GND is unrouted without a pour"
    );
    // Add the GND pour: the two pads now share its island ⇒ no longer unrouted.
    let mut with_pour = base;
    with_pour.push(G::Region(RegionDecl {
        shape: outline,
        role: Role::Conductor,
        net: Some("GND".into()),
        layer: "F.Cu".into(),
    }));
    let mut h2 = History::new(Default::default());
    h2.commit(
        Transaction::one(Command::SetSource(with_pour)),
        &lib,
        "pour",
    )
    .unwrap();
    assert!(
        !drc(h2.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "the pour connects both GND pads: {:?}",
        drc(h2.doc(), &lib)
    );
}

/// A foreign trace cutting fully across the pour splits it into two islands; GND
/// pads on opposite sides stay disconnected — honest fragmentation reporting.
#[test]
fn fragmented_pour_leaves_pads_unrouted() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 20),
        Point::mm(0, 20),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "g1".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "s".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        }, // below the cut
        G::Place {
            path: "g2".into(),
            pos: Point::mm(5, 15),
        }, // above the cut
        G::Place {
            path: "s".into(),
            pos: Point::mm(10, 10),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
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
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "frag")
        .unwrap();
    // A full-width SIG trace at y=10 cuts the GND pour into top/bottom islands.
    let cut = Trace {
        net: NetId::new("SIG"),
        layer: "F.Cu".into(),
        path: vec![Point::mm(0, 10), Point::mm(20, 10)],
        width: 150_000,
        prov: crate::doc::Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), cut)),
        &lib,
        "cut",
    )
    .unwrap();
    assert!(
        drc(h.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Unrouted { net, islands } if *net == NetId::new("GND") && *islands == 2
        )),
        "the split pour leaves GND in two islands: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Review regression (BUG 1): a same-net trace on a *different* layer that passes
/// under a pour must NOT be joined through it — cross-layer copper connects only
/// via a via. Here a B.Cu GND trace runs under an F.Cu GND pour with no via, so
/// the two GND pads stay disconnected.
#[test]
fn cross_layer_trace_not_joined_through_pour() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let left_pour = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(15, 0),
        Point::mm(15, 10),
        Point::mm(0, 10),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(30, 10)),
        G::Instance {
            path: "g1".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        }, // under the F.Cu pour
        G::Place {
            path: "g2".into(),
            pos: Point::mm(25, 5),
        }, // outside the pour
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: left_pour,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "xlayer")
        .unwrap();
    // A B.Cu GND trace from g2 running left *under* the F.Cu pour (x=10 is inside
    // the pour), but on the bottom layer with no via.
    let t = Trace {
        net: NetId::new("GND"),
        layer: "B.Cu".into(),
        path: vec![Point::mm(25, 5), Point::mm(10, 5)],
        width: 150_000,
        prov: crate::doc::Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), t)),
        &lib,
        "btrace",
    )
    .unwrap();
    assert!(
        drc(h.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Unrouted { net, .. } if *net == NetId::new("GND")
        )),
        "B.Cu trace must not connect through the F.Cu pour without a via: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Review regression (BUG 2): two overlapping same-net pours on one layer are one
/// blob of copper — they must be unioned before islanding, so pads split between
/// them are connected (not falsely reported as two islands).
#[test]
fn overlapping_same_net_pours_merge() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let a = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(18, 0),
        Point::mm(18, 10),
        Point::mm(0, 10),
    ]);
    let b = Shape2D::polygon(vec![
        Point::mm(12, 0),
        Point::mm(30, 0),
        Point::mm(30, 10),
        Point::mm(12, 10),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(30, 10)),
        G::Instance {
            path: "g1".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        }, // pour A only
        G::Place {
            path: "g2".into(),
            pos: Point::mm(25, 5),
        }, // pour B only
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: a,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
        G::Region(RegionDecl {
            shape: b,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "twopours")
        .unwrap();
    assert!(
        !drc(h.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Unrouted { net, .. } if *net == NetId::new("GND")
        )),
        "overlapping same-net pours are one island connecting both pads: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Mask generation must not perturb DRC. The mask-opening `Void`s that
/// `pad_features` now emits (and the mask solids `elaborate::features` emits) are
/// non-conductor geometry; the DRC copper producer (`net_features`) filters to
/// `Role::Conductor`, so none of it reaches clearance or connectivity, and the
/// violation set is exactly the copper-only result. This guards that invariant.
#[test]
fn mask_generation_does_not_perturb_drc() {
    let (doc, lib) = board_pour_scene("B.Cu");
    let nl = netlist_of(&doc);
    let su = stackup(&doc.source);

    // Sanity: the scene's pads DO generate mask-opening `Void`s, so the exclusion
    // below is a real guard rather than vacuous.
    let produces_openings = doc.components.values().any(|c| {
        lib.get(&c.part).is_some_and(|def| {
            def.pins
                .iter()
                .flat_map(|p| p.pad_features(c, &su))
                .any(|f| f.role == crate::geom::Role::Void)
        })
    });
    assert!(produces_openings, "scene pads produce mask-opening Voids");

    // The DRC copper producer is copper-only: no mask/void feature reaches it, so
    // the violation set is unchanged by the presence of mask geometry.
    let feats = net_features(&doc, &lib, &nl, &su);
    assert!(
        feats
            .iter()
            .all(|(_, nf)| nf.feature.role == crate::geom::Role::Conductor),
        "net_features carries only copper — mask/void never enters DRC"
    );
    assert!(
        !feats.is_empty(),
        "the scene has copper features (the check is non-trivial)"
    );
}

/// A fab graphic on a zero-height `Role::Datum` slab (Decision 15) must never
/// register a physical clash, even where it lies directly over foreign copper and
/// z-*touches* it (`ZRange::overlaps` is closed). This is the Datum analogue of
/// `mask_generation_does_not_perturb_drc`: DRC's copper producer (`net_features`)
/// filters to `Role::Conductor`, and footprint graphics never enter DRC at all, so
/// a Datum graphic sitting on foreign copper is not a short.
#[test]
fn datum_graphic_over_copper_is_not_a_clash() {
    let mut lib = part_library();
    // A plain SIG pad at the origin, and a GND part whose F.Fab graphic runs from
    // its own pad back across the origin — so the fab line lands on the SIG copper.
    lib.insert("SIG".into(), one_pad("F.Cu"));
    lib.insert(
        "GDFAB".into(),
        crate::kicad::import_footprint(
            r#"(footprint "GDFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end -10 0) (layer "F.Fab") (stroke (width 0.5))))"#,
        )
        .unwrap(),
    );
    // An authored stackup whose zero-height F.Fab datum slab sits at the F.Cu top
    // face, so a fab graphic z-*touches* copper (`lo == hi == 1_600_000`).
    let c = 35_000;
    let t = 1_600_000;
    let stack = |name: &str, lo: Nm, hi: Nm, role: Role, mat: Option<&str>| {
        G::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role,
            material: mat.map(Material::named),
        })
    };
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        stack("B.Cu", 0, c, Role::Conductor, Some("copper")),
        stack("core", c, t - c, Role::Substrate, Some("FR4")),
        stack("F.Cu", t - c, t, Role::Conductor, Some("copper")),
        stack("F.Fab", t, t, Role::Datum, None),
        G::Instance {
            path: "sig".into(),
            part: "SIG".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "gd".into(),
            part: "GDFAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "sig".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "gd".into(),
            pos: Point::mm(10, 0),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("sig".into(), "1".into())],
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("gd".into(), "1".into())],
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "datum")
        .expect("elaborates");
    let doc = h.doc();
    let su = stackup(&doc.source);

    // Sanity (non-vacuous): the fab graphic really does lower to a single
    // `Role::Datum` feature that z-touches AND x/y-overlaps the SIG copper — so if
    // Datum were treated as copper this pair *would* clash geometrically.
    let gd = doc.components.values().find(|c| c.part == "GDFAB").unwrap();
    let gd_def = lib.get(&gd.part).unwrap();
    let datum: Vec<_> = crate::part::graphic_features(gd_def, gd, &su);
    assert_eq!(datum.len(), 1, "one fab graphic → one feature");
    assert_eq!(datum[0].role, Role::Datum, "role comes from the F.Fab slab");
    let sig = doc.components.values().find(|c| c.part == "SIG").unwrap();
    let sig_cu = lib.get(&sig.part).unwrap().pins[0]
        .pad_features(sig, &su)
        .into_iter()
        .find(|f| f.role == Role::Conductor)
        .unwrap();
    assert!(
        !datum[0].clears(&sig_cu, DesignRules::default().min_clearance),
        "the datum graphic geometrically clashes the SIG copper (touch in z, \
             overlap in x/y) — the exclusion below is a real guard"
    );

    // The guard: no SIG/GND clearance violation, because the Datum graphic is
    // netless non-copper and never enters the clearance check.
    assert!(
        !drc(doc, &lib).iter().any(|v| matches!(
            v,
            Violation::Clearance { a, b, .. }
                if [a, b].contains(&&NetId::new("SIG"))
                    && [a, b].contains(&&NetId::new("GND"))
        )),
        "datum graphic over foreign copper is not a clash: {:?}",
        drc(doc, &lib)
    );
}

/// Two different-net pours overlapping on the same layer is a short.
#[test]
fn overlapping_pours_short() {
    let mut lib = part_library();
    lib.insert("PT".into(), one_pad("F.Cu"));
    let left = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(12, 0),
        Point::mm(12, 12),
        Point::mm(0, 12),
    ]);
    let right = Shape2D::polygon(vec![
        Point::mm(8, 8),
        Point::mm(20, 8),
        Point::mm(20, 20),
        Point::mm(8, 20),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "a".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "b".into(),
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "a".into(),
            pos: Point::mm(2, 2),
        },
        G::Place {
            path: "b".into(),
            pos: Point::mm(18, 18),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("a".into(), "1".into())],
        },
        G::ConnectPins {
            net: "PWR".into(),
            pins: vec![("b".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: left,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
        G::Region(RegionDecl {
            shape: right,
            role: Role::Conductor,
            net: Some("PWR".into()),
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "shorts")
        .unwrap();
    assert!(
        drc(h.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Clearance { a, b, .. }
                if *a == NetId::new("GND") && *b == NetId::new("PWR")
        )),
        "overlapping GND/PWR pours short: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Issue 0023: an authored **copper keep-out** now excludes copper — DRC gates the
/// unified stream's copper against `Role::Keepout` features. A trace crossing a F.Cu
/// copper keep-out flags `Violation::Keepout`; a keep-out on the *other* layer does
/// not (z-overlap gates it to its slab).
#[test]
fn copper_keepout_is_enforced() {
    use crate::geom::KeepoutKind;
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));
    let mk = |layer: &str| {
        // A SIG pad in a safe corner establishes the net (so its trace may be added);
        // the trace then runs through the keep-out square.
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(3, 3),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                role: Role::Keepout(KeepoutKind::Copper),
                net: None,
                layer: layer.into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
            .unwrap();
        // A Top trace running straight through the keep-out square's centre.
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(6, 10), Point::mm(14, 10)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        h
    };
    // Keep-out on the trace's own layer (F.Cu): the trace intrudes it.
    let same = mk("F.Cu");
    assert!(
        drc(same.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Keepout { net, kind }
                if *net == NetId::new("SIG") && *kind == KeepoutKind::Copper
        )),
        "a Top trace crossing a F.Cu copper keep-out must flag: {:?}",
        drc(same.doc(), &lib)
    );
    // Keep-out on B.Cu: a Top trace does not overlap it in z, so no keep-out fault.
    let other = mk("B.Cu");
    assert!(
        !drc(other.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Keepout { .. })),
        "a B.Cu keep-out must not gate a Top trace: {:?}",
        drc(other.doc(), &lib)
    );
}

/// Issue 0023: copper too close to the board edge flags `EdgeClearance`. A trace
/// hugging the left edge (0.1 mm in, under the 0.2 mm rule) violates; a trace routed
/// through the board interior does not.
#[test]
fn copper_near_board_edge_flags_edge_clearance() {
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));
    let scene = |x_mm: i64| {
        // A centred SIG pad establishes the net; the trace under test runs vertically
        // at `x_mm`/10 mm from the left edge.
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(10, 10),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "board")
            .unwrap();
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![
                Point {
                    x: x_mm * MM / 10,
                    y: 2 * MM,
                },
                Point {
                    x: x_mm * MM / 10,
                    y: 18 * MM,
                },
            ],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        h
    };
    // Centreline 0.1 mm from the x=0 edge (x_mm/10 = 1 → 0.1 mm): within the rule.
    let near = scene(1);
    assert!(
        drc(near.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::EdgeClearance { net } if *net == NetId::new("SIG"))),
        "copper 0.1mm from the edge must flag: {:?}",
        drc(near.doc(), &lib)
    );
    // Centreline 10 mm in: comfortably clear.
    let mid = scene(100);
    assert!(
        !drc(mid.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::EdgeClearance { .. })),
        "interior copper must not flag edge clearance: {:?}",
        drc(mid.doc(), &lib)
    );
}

/// A `Route` keep-out is enforced like a `Copper` one; a `Component` keep-out (a
/// courtyard — a placement concern) is NOT a DRC copper fault (guards against
/// double-reporting vs the placement courtyard verify).
#[test]
fn route_keepout_enforced_component_keepout_ignored() {
    use crate::geom::KeepoutKind;
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));
    let mk = |kind: KeepoutKind| {
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(3, 3),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                role: Role::Keepout(kind),
                net: None,
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
            .unwrap();
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(6, 10), Point::mm(14, 10)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        h
    };
    let route = mk(KeepoutKind::Route);
    assert!(
        drc(route.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Keepout { kind, .. } if *kind == KeepoutKind::Route
        )),
        "a Route keep-out gates copper: {:?}",
        drc(route.doc(), &lib)
    );
    let comp = mk(KeepoutKind::Component);
    assert!(
        !drc(comp.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Keepout { .. })),
        "a Component keep-out (courtyard) is not a DRC copper fault: {:?}",
        drc(comp.doc(), &lib)
    );
}

/// Boundary at clearance 0: copper whose edge is *exactly tangent* to a keep-out
/// (zero gap) does not violate — the clearance test is strict `<`. Only overlap does.
#[test]
fn keepout_tangent_does_not_violate() {
    use crate::geom::KeepoutKind;
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "p".into(),
            part: "P".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "p".into(),
            pos: Point::mm(3, 3),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("p".into(), "1".into())],
        },
        // Keep-out square spans x ∈ [8mm, 12mm].
        G::Region(RegionDecl {
            shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
            role: Role::Keepout(KeepoutKind::Copper),
            net: None,
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
        .unwrap();
    // A vertical trace (width 0.15mm ⇒ r = 0.075mm) whose centreline is 0.075mm left
    // of the keep-out edge, so its right copper edge lands exactly on x = 8mm.
    let t = Trace {
        net: NetId::new("SIG"),
        layer: "F.Cu".into(),
        path: vec![
            Point {
                x: 8 * MM - 75_000,
                y: 6 * MM,
            },
            Point {
                x: 8 * MM - 75_000,
                y: 14 * MM,
            },
        ],
        width: 150_000,
        prov: crate::doc::Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), t)),
        &lib,
        "t",
    )
    .unwrap();
    assert!(
        !drc(h.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Keepout { .. })),
        "copper tangent to the keep-out edge (gap 0) must not violate: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Edge clearance: copper fully outside the board flags, copper inside a cutout hole
/// flags, and a copper pour reaching the board edge is exempt (pull-back is a fill
/// concern, not a DRC fault).
#[test]
fn edge_clearance_outside_cutout_and_pour_exempt() {
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));

    // (a) A trace entirely outside the 10×10 board (at x = 12mm).
    let outside = {
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(5, 5),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "o")
            .unwrap();
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(12, 2), Point::mm(12, 8)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        h
    };
    assert!(
        drc(outside.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::EdgeClearance { .. })),
        "copper outside the board must flag edge clearance: {:?}",
        drc(outside.doc(), &lib)
    );

    // (b) A trace inside a cutout hole (the cutout wall is a board edge).
    let in_cutout = {
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Cutout {
                shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
            },
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(3, 3),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "c")
            .unwrap();
        // A short trace inside the [8,12]² cutout.
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(9, 10), Point::mm(11, 10)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        h
    };
    assert!(
        drc(in_cutout.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::EdgeClearance { .. })),
        "copper inside a cutout must flag edge clearance: {:?}",
        drc(in_cutout.doc(), &lib)
    );

    // (c) A board-covering pour reaches the edge but is EXEMPT from edge clearance.
    let pour = {
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(10, 10),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("p".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: Shape2D::polygon(vec![
                    Point::mm(0, 0),
                    Point::mm(20, 0),
                    Point::mm(20, 20),
                    Point::mm(0, 20),
                ]),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "p")
            .unwrap();
        h
    };
    assert!(
        !drc(pour.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::EdgeClearance { .. })),
        "a pour at the board edge is exempt from edge clearance: {:?}",
        drc(pour.doc(), &lib)
    );
}

/// The commit gate that makes `world_features`' fail-loud sound: a `SetSource` naming
/// a typo'd slab is REJECTED at commit (via `elaborate`), so no doc with an
/// unresolvable slab ever reaches DRC. (Companion to `region_on_unknown_slab_is_rejected`,
/// pinning the Conductor-pour variant the reviewer flagged.)
#[test]
fn setsource_conductor_on_bad_slab_is_rejected_at_commit() {
    let mut lib = part_library();
    lib.insert("P".into(), one_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Instance {
            path: "p".into(),
            part: "P".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("p".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: Shape2D::rect(Point::mm(5, 5), MM, MM),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cuu".into(), // typo
        }),
    ];
    let mut h = History::new(Default::default());
    let err = h
        .commit(Transaction::one(Command::SetSource(src)), &lib, "typo")
        .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
        "a Conductor pour on a typo'd slab is rejected at commit: {err:?}"
    );
}

/// Fail-loud, not fail-silent (the reviewer's finding 1): if a doc that bypassed the
/// commit gate (so its slab does not resolve) somehow reaches DRC, `check_drc` must
/// PANIC — never return an empty (⇒ "clean") bill for a board that never
/// materialised. Here we hand-build such a `Doc` directly, without committing.
#[test]
#[should_panic(expected = "committed doc")]
fn drc_on_unmaterialized_bad_slab_doc_panics() {
    let doc = Doc {
        source: vec![G::Region(RegionDecl {
            shape: Shape2D::rect(Point::mm(1, 1), MM, MM),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cuu".into(), // never resolves
        })],
        ..Default::default()
    };
    // Must panic (world_features errors on the unresolvable slab), not return empty.
    let _ = check_drc(
        &doc,
        &part_library(),
        &BTreeMap::new(),
        &DesignRules::default(),
    );
}

// ------------------------------------------------------------------------
// Decision 19c — layer-honest pad incidence (closes PoC finding F1).
// ------------------------------------------------------------------------

/// A 4-copper stackup (F.Cu / In1.Cu / In2.Cu / B.Cu) for the layer-honesty tests,
/// z descending F→B, with the two outer masks so a board is fully materialised.
fn four_copper_slabs() -> Vec<G> {
    let cu = |name: &str, lo: Nm, hi: Nm| {
        G::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role: Role::Conductor,
            material: Some(Material::named("copper")),
        })
    };
    let other = |name: &str, lo: Nm, hi: Nm, role: Role| {
        G::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role,
            material: None,
        })
    };
    vec![
        other("B.Mask", -25_000, 0, Role::Mask),
        cu("B.Cu", 0, 35_000),
        other("core3", 35_000, 500_000, Role::Substrate),
        cu("In2.Cu", 500_000, 535_000),
        other("core2", 535_000, 1_000_000, Role::Substrate),
        cu("In1.Cu", 1_000_000, 1_035_000),
        other("core1", 1_035_000, 1_565_000, Role::Substrate),
        cu("F.Cu", 1_565_000, 1_600_000),
        other("F.Mask", 1_600_000, 1_625_000, Role::Mask),
    ]
}

/// A through-hole (drilled) one-pad footprint: its barrel copper fans out to every
/// copper slab, so its pad exists on all layers (incl. an inner plane's slab).
fn one_pad_thru() -> crate::part::PartDef {
    crate::kicad::import_footprint(
            r#"(footprint "PTH" (pad "1" thru_hole circle (at 0 0) (size 1 1) (drill 0.5) (layers "*.Cu")))"#,
        )
        .unwrap()
}

/// The F1 case, distilled: an SMD pad on F.Cu is **not** joined to a same-net plane
/// poured on an inner slab (In1.Cu) that it merely overlaps in XY — with no stitching
/// via there is no copper path, and the model must not claim connectivity. Two such
/// F.Cu pads over an In1.Cu GND plane stay two islands.
#[test]
fn smd_pad_not_joined_to_foreign_slab_plane() {
    let mut lib = part_library();
    lib.insert("SP".into(), one_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 10),
        Point::mm(0, 10),
    ]);
    let mut src = four_copper_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        // A full-board GND plane on the INNER layer In1.Cu.
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "f1")
        .unwrap();
    assert!(
        drc(h.doc(), &lib).iter().any(|v| matches!(
            v,
            Violation::Unrouted { net, islands } if *net == NetId::new("GND") && *islands == 2
        )),
        "F.Cu SMD pads must NOT join an In1.Cu plane without stitching vias: {:?}",
        drc(h.doc(), &lib)
    );
}

/// The complement: a **through-hole** pad, whose barrel spans every copper slab,
/// DOES join an inner-layer plane it sits over — its copper genuinely exists on that
/// slab. Two through-hole GND pads over an In1.Cu plane collapse to one island.
#[test]
fn thru_pad_joins_foreign_slab_plane() {
    let mut lib = part_library();
    lib.insert("TP".into(), one_pad_thru());
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 10),
        Point::mm(0, 10),
    ]);
    let mut src = four_copper_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "g1".into(),
            part: "TP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "TP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "thru")
        .unwrap();
    assert!(
        !drc(h.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "through-hole pads span In1.Cu, so the plane connects them: {:?}",
        drc(h.doc(), &lib)
    );
}

/// A stitching via ties an F.Cu SMD pad to an inner GND plane: pad + via + plane form
/// one island. Same scene as `smd_pad_not_joined_to_foreign_slab_plane` but with a
/// through via at each pad dropping to In1.Cu — now GND is one island (connected).
#[test]
fn stitching_via_connects_smd_pad_to_inner_plane() {
    let mut lib = part_library();
    lib.insert("SP".into(), one_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 10),
        Point::mm(0, 10),
    ]);
    let mut src = four_copper_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "stitch")
        .unwrap();
    // A through via at each pad: it spans F.Cu (touching the SMD pad) down through
    // In1.Cu (landing on the plane), so each pad is now genuinely on the plane.
    for (id, at) in [(1u64, Point::mm(5, 5)), (2, Point::mm(15, 5))] {
        let v = Via {
            net: NetId::new("GND"),
            at,
            span: None,
            drill: 300_000,
            pad: 600_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddVia(crate::id::ViaId(id), v)),
            &lib,
            "via",
        )
        .unwrap();
    }
    assert!(
        !drc(h.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "pad + stitching via + plane must be one island: {:?}",
        drc(h.doc(), &lib)
    );
}

/// Padless-compatibility (Decision 19c): a bare terminal (a pin whose footprint
/// carries NO pad copper) keeps all-layer incidence, so it joins a same-net plane
/// on any slab it sits over. The toy library's pins now carry real pad copper, so
/// the test builds its own genuinely padless `LDO`/`Cap` stand-ins.
#[test]
fn bare_pin_keeps_all_layer_plane_incidence() {
    // Padless twins of the toy LDO/Cap: same pin names/offsets, `pad: None`.
    let bare_pin = |name: &str, offset: Point| crate::part::PinDef {
        name: name.into(),
        number: name.into(),
        role: crate::part::PinRole::Passive,
        offset,
        pad: None,
    };
    let bare_part = |name: &str, pins: Vec<crate::part::PinDef>| crate::part::PartDef {
        name: name.into(),
        pins,
        interfaces: std::collections::BTreeMap::new(),
        graphics: Vec::new(),
        texts: Vec::new(),
        courtyard: None,
        class: None,
    };
    let mut lib = PartLib::new();
    lib.insert(
        "LDO".into(),
        bare_part(
            "LDO",
            vec![
                bare_pin("VIN", Point { x: -2 * MM, y: 0 }),
                bare_pin("VOUT", Point { x: 2 * MM, y: 0 }),
                bare_pin("GND", Point { x: 0, y: -2 * MM }),
            ],
        ),
    );
    lib.insert(
        "Cap".into(),
        bare_part(
            "Cap",
            vec![
                bare_pin("p1", Point { x: -MM, y: 0 }),
                bare_pin("p2", Point { x: MM, y: 0 }),
            ],
        ),
    );
    let outline = Shape2D::polygon(vec![
        Point::mm(-6, -10),
        Point::mm(18, -10),
        Point::mm(18, 10),
        Point::mm(-6, 10),
    ]);
    let mut src = four_copper_slabs();
    src.extend(vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("reg".into(), "GND".into()), ("dec".into(), "p2".into())],
        },
        // A GND plane on an inner slab. A bare (padless) pin has no copper on any
        // specific slab, so under all-layer compatibility it still joins here.
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "bare")
        .unwrap();
    assert!(
        !drc(h.doc(), &lib)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "a padless (bare) pin keeps all-layer plane incidence: {:?}",
        drc(h.doc(), &lib)
    );
}

// ----------------------------------------------------------------------------
// Feature provenance (issue 0031): the derived `world_features` stream carries a
// `FeatureOrigin` naming the source entity every feature was lowered from.
// ----------------------------------------------------------------------------

/// The full origin contract: on a small board carrying a netted pad, a copper pour,
/// a routed trace, and a through via, `world_features` tags every feature with its
/// owning source entity — a trace's copper its `TraceId`, a via's barrel + drill its
/// `ViaId`, a pad's copper `(component, pad number)`, a pour its `Region { net,
/// layer }` identity, the substrate/mask the `Board`, and footprint silk its
/// component. The set of `Unattributed` features is asserted to be *exactly* the
/// expected kinds (here: none — the scene has no NPTH `hole`) so any future feature
/// kind that ships without a considered origin surfaces as a regression.
#[test]
fn world_features_carry_source_provenance() {
    use crate::geom::FeatureOrigin;
    use crate::id::{TraceId, ViaId};
    use crate::route::model::{Trace, Via};

    let mut lib = part_library();
    // A real KiCad footprint whose pad *name* differs from its *number* would be
    // ideal, but `one_pad` uses number == name "1"; the pad-number contract is
    // exercised end-to-end by the GUI's `picked_pin_projects_its_net`. Here we just
    // need a netted through-hole pad so a Pad origin appears.
    lib.insert("PT".into(), one_pad("F.Cu"));
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
            part: "PT".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g".into(),
            pos: Point::mm(5, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }),
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "prov")
        .expect("elaborates");
    // Add a routed trace and a through via on GND.
    h.commit(
        Transaction::one(Command::AddTrace(
            TraceId(7),
            Trace {
                net: NetId::new("GND"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(5, 5), Point::mm(15, 5)],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            },
        )),
        &lib,
        "trace",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::AddVia(
            ViaId(9),
            Via {
                net: NetId::new("GND"),
                at: Point::mm(10, 10),
                span: None,
                drill: 300_000,
                pad: 600_000,
                prov: crate::doc::Provenance::Pinned,
            },
        )),
        &lib,
        "via",
    )
    .unwrap();

    let doc = h.doc().clone();
    let su = stackup(&doc.source);
    let world =
        world_features(&doc, &lib, &netlist_of(&doc), &DesignRules::default(), &su).unwrap();

    let has = |o: FeatureOrigin| world.iter().any(|nf| nf.origin == o);

    // A trace's copper carries its TraceId.
    assert!(
        has(FeatureOrigin::Trace(TraceId(7))),
        "a trace's copper must carry its TraceId"
    );
    // A via's barrel (conductor prism, fanned per copper slab) carries its ViaId; so
    // does its plated drill Void. Assert both a Conductor and a Void feature name v9.
    assert!(
        world
            .iter()
            .any(|nf| nf.origin == FeatureOrigin::Via(ViaId(9))
                && nf.feature.role == Role::Conductor),
        "a via's copper barrel must carry its ViaId"
    );
    assert!(
        world
            .iter()
            .any(|nf| nf.origin == FeatureOrigin::Via(ViaId(9)) && nf.feature.role == Role::Void),
        "a via's plated drill Void must carry its ViaId"
    );
    // A pad's copper carries (component, pad number). The `one_pad` component id is
    // its instance path "g"; pad number is "1".
    assert!(
        world.iter().any(|nf| {
            nf.origin
                == FeatureOrigin::Pad {
                    comp: crate::id::EntityId::new("g"),
                    pad: "1".to_string(),
                }
                && nf.feature.role == Role::Conductor
        }),
        "a pad's copper must carry (component, pad number)"
    );
    // The pour carries Region { net = GND, layer = F.Cu }.
    assert!(
        has(FeatureOrigin::Region {
            net: Some(NetId::new("GND")),
            layer: "F.Cu".to_string(),
        }),
        "a pour must carry its Region net+layer identity"
    );
    // The board substrate + mask solids carry Board.
    assert!(
        world
            .iter()
            .any(|nf| nf.origin == FeatureOrigin::Board && nf.feature.role == Role::Substrate),
        "the substrate must carry Board provenance"
    );
    assert!(
        world
            .iter()
            .any(|nf| nf.origin == FeatureOrigin::Board && nf.feature.role == Role::Mask),
        "mask solids must carry Board provenance"
    );

    // The Unattributed set: assert exactly which feature kinds (role) are
    // unattributed. This scene authors no NPTH `hole`, and every derived feature has
    // a considered origin, so the set must be EMPTY. A future feature kind shipped
    // without an origin lands here and fails this assertion.
    let unattributed_roles: std::collections::BTreeSet<String> = world
        .iter()
        .filter(|nf| nf.origin == FeatureOrigin::Unattributed)
        .map(|nf| format!("{:?}", nf.feature.role))
        .collect();
    assert!(
        unattributed_roles.is_empty(),
        "no feature in this scene should be Unattributed; got roles: {unattributed_roles:?}"
    );
}

/// The one genuinely-unattributable kind: an authored NPTH `hole` lowers to a
/// pierce-everything `Role::Void` with no owning selectable entity → `Unattributed`.
/// Asserted separately so the "Unattributed set is exactly {NPTH hole void}" contract
/// is explicit and a regression (a hole silently gaining/losing attribution) surfaces.
#[test]
fn npth_hole_is_the_only_unattributed_kind() {
    use crate::geom::FeatureOrigin;

    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Hole {
            center: Point::mm(10, 10),
            dia: 3 * MM,
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "hole")
        .expect("elaborates");
    let doc = h.doc().clone();
    let su = stackup(&doc.source);
    let world =
        world_features(&doc, &lib, &netlist_of(&doc), &DesignRules::default(), &su).unwrap();

    let unattributed: Vec<&crate::geom::NetFeature> = world
        .iter()
        .filter(|nf| nf.origin == FeatureOrigin::Unattributed)
        .collect();
    assert_eq!(
        unattributed.len(),
        1,
        "exactly one Unattributed feature (the NPTH hole void)"
    );
    assert_eq!(
        unattributed[0].feature.role,
        Role::Void,
        "the only Unattributed feature is the NPTH hole's Void"
    );
}
