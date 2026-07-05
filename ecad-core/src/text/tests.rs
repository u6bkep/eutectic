use super::*;
use crate::command::{Command, Transaction};
use crate::doc::Point;
use crate::elaborate::{elaborate, psu_module};
use crate::history::History;
use crate::part::part_library;

// ---- fixtures --------------------------------------------------------

fn uart_link() -> Source {
    vec![
        GenDirective::Instance {
            path: "mcu".into(),
            part: "MCU".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "sens".into(),
            part: "Sensor".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::ConnectInterface {
            a: ("mcu".into(), "uart".into()),
            b: ("sens".into(), "uart".into()),
        },
    ]
}

/// A scene exercising Board / Near / MinSep / AlignY / Fix.
fn placement_scene() -> Source {
    vec![
        GenDirective::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "c2".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Fix {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        board_rect(Point::mm(0, 0), Point::mm(50, 50)),
        GenDirective::Near {
            a: "c1".into(),
            b: "reg".into(),
            within: 3 * MM,
        },
        GenDirective::Near {
            a: "c2".into(),
            b: "reg".into(),
            within: 3 * MM,
        },
        GenDirective::MinSep {
            a: "c1".into(),
            b: "c2".into(),
            gap: 4 * MM,
        },
        GenDirective::AlignY {
            nodes: vec!["c1".into(), "c2".into()],
        },
    ]
}

/// A hand-built source touching *every* GenDirective variant.
fn all_variants() -> Source {
    vec![
        GenDirective::Instance {
            path: "psu.reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "psu.dec[0]".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "mcu".into(),
            part: "MCU".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "sens".into(),
            part: "Sensor".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Place {
            path: "psu.dec[0]".into(),
            pos: Point::mm(5, 5),
        },
        GenDirective::Fix {
            path: "psu.reg".into(),
            pos: Point {
                x: 1,
                y: -2_500_000,
            },
        },
        board_rect(Point::mm(0, 0), Point::mm(50, 50)),
        GenDirective::Cutout {
            shape: Shape2D::polygon(vec![
                Point::mm(20, 20),
                Point::mm(30, 20),
                Point::mm(25, 30),
            ]),
        },
        // An authored NPTH mounting hole (Decision 16b) — center + diameter round-trip.
        GenDirective::Hole {
            center: Point::mm(5, 45),
            dia: 2_700_000,
        },
        // A net-bound copper pour on the bottom layer, and a component keep-out.
        GenDirective::Region(RegionDecl {
            shape: Shape2D::polygon(vec![
                Point::mm(0, 0),
                Point::mm(50, 0),
                Point::mm(50, 50),
                Point::mm(0, 50),
            ]),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "B.Cu".into(),
        }),
        GenDirective::Region(RegionDecl {
            shape: Shape2D::polygon(vec![
                Point::mm(10, 10),
                Point::mm(15, 10),
                Point::mm(15, 15),
            ]),
            role: Role::Keepout(KeepoutKind::Component),
            net: None,
            layer: "F.Cu".into(),
        }),
        // An authored 3-slab stackup: conductor / substrate / conductor, exercising
        // the substrate role and both material-present and material-absent slabs.
        GenDirective::Slab(Slab {
            name: "B.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: Some(Material::named("copper")),
        }),
        GenDirective::Slab(Slab {
            name: "core".into(),
            z: ZRange::new(35_000, 1_565_000),
            role: Role::Substrate,
            material: None,
        }),
        GenDirective::Slab(Slab {
            name: "F.Cu".into(),
            z: ZRange::new(1_565_000, 1_600_000),
            role: Role::Conductor,
            material: Some(Material::named("copper")),
        }),
        // A zero-height fab datum slab: `datum` role authorable, `lo == hi` z
        // (Decision 15). Round-trips and flows through the stackup like any slab.
        GenDirective::Slab(Slab {
            name: "F.Fab".into(),
            z: ZRange::new(1_600_000, 1_600_000),
            role: Role::Datum,
            material: None,
        }),
        GenDirective::Near {
            a: "psu.dec[0]".into(),
            b: "psu.reg".into(),
            within: 2 * MM,
        },
        GenDirective::MinSep {
            a: "psu.dec[0]".into(),
            b: "mcu".into(),
            gap: MM,
        },
        GenDirective::AlignX {
            nodes: vec!["psu.reg".into(), "psu.dec[0]".into()],
        },
        GenDirective::AlignY {
            nodes: vec!["mcu".into(), "sens".into()],
        },
        GenDirective::Rotate {
            path: "psu.reg".into(),
            orient: Orient::from_deg(90).unwrap(),
        },
        GenDirective::NearPin {
            a: "psu.dec[0]".into(),
            b_comp: "psu.reg".into(),
            b_pin: "VOUT".into(),
            within: 2 * MM,
        },
        // Board text (silk): an identity-oriented label and a cardinally-rotated one.
        GenDirective::Text {
            string: "REF 1".into(),
            at: Point::mm(2, 40),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        },
        GenDirective::Text {
            string: "B1".into(),
            at: Point::mm(10, 40),
            height: 800_000,
            layer: "B.SilkS".into(),
            orient: Orient::from_deg(90).unwrap(),
        },
        GenDirective::ConnectInterface {
            a: ("mcu".into(), "uart".into()),
            b: ("sens".into(), "uart".into()),
        },
        GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("psu.reg".into(), "VOUT".into()),
                ("psu.dec[0]".into(), "p1".into()),
            ],
        },
        // GND is connected so the conductor pour above references a real net.
        GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("psu.dec[0]".into(), "p2".into())],
        },
        GenDirective::NoConnect {
            pins: vec![
                ("psu.reg".into(), "GND".into()),
                ("mcu".into(), "GPIO0".into()),
            ],
        },
    ]
}

fn doc_of(source: Source, overrides: BTreeMap<EntityId, Override>) -> Doc {
    Doc {
        source,
        overrides,
        ..Default::default()
    }
}

fn placed(src: Source) -> Doc {
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
        .unwrap();
    h.doc().clone()
}

// ---- routes state zone (Decision 18) --------------------------------

use crate::doc::Provenance;
use crate::id::NetId;

fn tr(net: &str, layer: &str, path: Vec<Point>, width: Nm, prov: Provenance) -> Trace {
    Trace {
        net: NetId::new(net),
        layer: layer.into(),
        path,
        width,
        prov,
    }
}

/// The `# routes` state zone round-trips: a doc carrying pinned/free/hint/fixed
/// traces and a full-span + a blind/buried via reparses to the same `traces`/`vias`
/// (ids re-minted in the same BTreeMap order, so the maps compare equal).
#[test]
fn routes_round_trip() {
    let mut doc = Doc::default();
    doc.traces.insert(
        TraceId(1),
        tr(
            "GND",
            "F.Cu",
            vec![Point::mm(1, 2), Point::mm(5, 2), Point::mm(5, 8)],
            150_000,
            Provenance::Pinned,
        ),
    );
    doc.traces.insert(
        TraceId(2),
        tr(
            "GND",
            "B.Cu",
            vec![Point::mm(2, 1), Point::mm(2, 9)],
            150_000,
            Provenance::Free,
        ),
    );
    doc.traces.insert(
        TraceId(3),
        tr(
            "VCC",
            "F.Cu",
            vec![Point::mm(0, 0), Point::mm(3, 0)],
            200_000,
            Provenance::Hint,
        ),
    );
    doc.traces.insert(
        TraceId(4),
        tr(
            "VCC",
            "F.Cu",
            vec![Point::mm(0, 5), Point::mm(3, 5)],
            200_000,
            Provenance::Fixed,
        ),
    );
    doc.vias.insert(
        ViaId(1),
        Via {
            net: NetId::new("GND"),
            at: Point::mm(5, 8),
            span: None,
            drill: 300_000,
            pad: 600_000,
            prov: Provenance::Pinned,
        },
    );
    doc.vias.insert(
        ViaId(2),
        Via {
            net: NetId::new("VCC"),
            at: Point::mm(3, 0),
            span: Some(("F.Cu".into(), "In1.Cu".into())),
            drill: 250_000,
            pad: 500_000,
            prov: Provenance::Free,
        },
    );

    let text = serialize(&doc);
    let parsed = parse(&text).expect("parse routes");
    assert_eq!(parsed.traces, doc.traces, "traces round-trip:\n{text}");
    assert_eq!(parsed.vias, doc.vias, "vias round-trip:\n{text}");
    // Idempotent: re-serialize the parsed routes byte-equals.
    let doc2 = Doc {
        traces: parsed.traces,
        vias: parsed.vias,
        ..Default::default()
    };
    assert_eq!(serialize(&doc2), text, "serialize is idempotent");
}

/// Provenance keywords (Decision 18): `pinned` is the default and prints nothing;
/// `free`/`hint`/`fixed` are explicit trailing keywords. Hand-authored (keyword-less)
/// lines parse as Pinned.
#[test]
fn route_provenance_keywords() {
    let mut doc = Doc::default();
    doc.traces.insert(
        TraceId(1),
        tr(
            "N",
            "F.Cu",
            vec![Point::mm(0, 0), Point::mm(1, 0)],
            150_000,
            Provenance::Pinned,
        ),
    );
    doc.traces.insert(
        TraceId(2),
        tr(
            "N",
            "F.Cu",
            vec![Point::mm(0, 1), Point::mm(1, 1)],
            150_000,
            Provenance::Free,
        ),
    );
    let text = serialize(&doc);
    // Pinned prints no keyword; Free prints ` free`.
    let route_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("route ")).collect();
    assert!(
        route_lines[0].ends_with(")") && !route_lines[0].contains("free"),
        "pinned prints no keyword: `{}`",
        route_lines[0]
    );
    assert!(route_lines[1].ends_with(" free"), "free prints the keyword");
    // A hand-authored keyword-less line parses as Pinned.
    let hand = "route N F.Cu w=0.15mm (0, 0) (1mm, 0)";
    let p = parse(hand).expect("parse hand route");
    assert_eq!(p.traces[&TraceId(1)].prov, Provenance::Pinned);
}

/// A blind/buried via's explicit `<from>..<to>` span parses (Decision 18 — parseable
/// today even though multilayer stackups are rare).
#[test]
fn via_blind_span_parses() {
    let p = parse("via SIG (2mm, 3mm) drill=0.25mm pad=0.5mm F.Cu..In1.Cu free")
        .expect("parse blind via");
    let v = &p.vias[&ViaId(1)];
    assert_eq!(v.span, Some(("F.Cu".into(), "In1.Cu".into())));
    assert_eq!(v.prov, Provenance::Free);
    assert_eq!(v.drill, 250_000);
}

/// A routeless doc serializes byte-identically to before this feature (no `# routes`
/// section), so existing files are undisturbed.
#[test]
fn no_routes_no_section() {
    let doc = placed(uart_link());
    assert!(
        !serialize(&doc).contains("# routes"),
        "a routeless doc emits no routes section"
    );
}

// ---- round-trip + idempotence ---------------------------------------

/// `parse(serialize(doc))` reproduces `(source, overrides, refdes_pins)` exactly,
/// for a source that touches every directive variant, both override strengths, and
/// refdes pins — including an entity (`mcu`) carrying both a pos pin and a refdes
/// pin, to exercise the interleaved override section.
#[test]
fn round_trip_all_variants() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        EntityId::new("psu.dec[0]"),
        Override {
            pos: Some(Point::mm(7, 3)),
            strength: Strength::Hint,
        },
    );
    overrides.insert(
        EntityId::new("mcu"),
        Override {
            pos: Some(Point {
                x: 12_345_678,
                y: -500_000,
            }),
            strength: Strength::Pin,
        },
    );
    let mut doc = doc_of(all_variants(), overrides);
    doc.refdes_pins
        .insert(EntityId::new("psu.dec[0]"), "C7".into());
    doc.refdes_pins.insert(EntityId::new("mcu"), "U3".into());

    let text = serialize(&doc);
    let Parsed {
        source: src,
        overrides: ovr,
        refdes_pins: rd,
        ..
    } = parse(&text).expect("parse");
    assert_eq!(src, doc.source, "source must round-trip");
    assert_eq!(ovr, doc.overrides, "overrides must round-trip");
    assert_eq!(rd, doc.refdes_pins, "refdes pins must round-trip");
}

/// A refdes value is opaque (Decision 14), so it may hold whitespace or a `#`; both
/// must survive serialize→parse via the quote-aware machinery (`quote_value` wraps,
/// the quote-aware comment strip keeps a quoted `#` literal).
#[test]
fn refdes_value_with_whitespace_and_hash_round_trips() {
    let mut doc = doc_of(Vec::new(), BTreeMap::new());
    doc.refdes_pins
        .insert(EntityId::new("a"), "TEST POINT".into());
    doc.refdes_pins.insert(EntityId::new("b"), "X#1".into());
    let Parsed {
        refdes_pins: rd, ..
    } = parse(&serialize(&doc)).expect("parse");
    assert_eq!(rd, doc.refdes_pins);
}

/// A `slab` directive parses to the expected `Slab` (name, z's, role, optional
/// material) and round-trips through `serialize`. Covers material-present,
/// material-absent, and the `substrate` role (which `region` does not accept).
#[test]
fn slab_directive_parses_and_round_trips() {
    let text = "\
slab B.Cu 0mm 0.035mm conductor copper
slab core 0.035mm 1.565mm substrate
slab F.Cu 1.565mm 1.6mm conductor copper";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    assert_eq!(
        src,
        vec![
            GenDirective::Slab(Slab {
                name: "B.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: Some(Material::named("copper")),
            }),
            GenDirective::Slab(Slab {
                name: "core".into(),
                z: ZRange::new(35_000, 1_565_000),
                role: Role::Substrate,
                material: None,
            }),
            GenDirective::Slab(Slab {
                name: "F.Cu".into(),
                z: ZRange::new(1_565_000, 1_600_000),
                role: Role::Conductor,
                material: Some(Material::named("copper")),
            }),
        ]
    );
    // Canonical serialization re-parses to the same source.
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
}

/// An `inst` directive carrying a display label and identity params parses to the
/// expected `Instance` (params in `BTreeMap` order, values unquoted) and round-trips.
/// A quoted value with spaces and a `#` survives (the `#` is not a comment here).
#[test]
fn inst_with_params_and_label_round_trips() {
    let text = "inst r1 R_0402 label=\"{value:si:Ω}\" p:tol=5% p:value=4.7k";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    let mut params = BTreeMap::new();
    params.insert("tol".into(), "5%".into());
    params.insert("value".into(), "4.7k".into());
    assert_eq!(
        src,
        vec![GenDirective::Instance {
            path: "r1".into(),
            part: "R_0402".into(),
            params,
            label: Some("{value:si:Ω}".into()),
        }]
    );
    // Canonical serialization re-parses to the same source.
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

    // A quoted param value with a space and a `#` round-trips (not a comment).
    let text2 = "inst u1 MCU p:desc=\"dual # buck\"";
    let Parsed { source: src2, .. } = parse(text2).expect("parse2");
    let doc2 = doc_of(src2.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc2)).unwrap().source, src2);
    if let GenDirective::Instance { params, .. } = &src2[0] {
        assert_eq!(params["desc"], "dual # buck");
    } else {
        panic!("expected Instance");
    }

    // Bare `inst <path> <part>` still parses with empty/None defaults.
    let Parsed { source: bare, .. } = parse("inst q1 NPN").expect("bare");
    assert_eq!(
        bare,
        vec![GenDirective::Instance {
            path: "q1".into(),
            part: "NPN".into(),
            params: BTreeMap::new(),
            label: None,
        }]
    );
}

/// A `param` directive (Decision 21b) parses to `GenDirective::Param` and serializes
/// as authored (the expression text is emitted verbatim, never pre-evaluated).
#[test]
fn param_directive_parses_and_round_trips() {
    let text = "param n = 3\nparam gap = n + 1";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    assert_eq!(
        src,
        vec![
            GenDirective::Param {
                name: "n".into(),
                expr: "3".into(),
            },
            GenDirective::Param {
                name: "gap".into(),
                expr: "n + 1".into(),
            },
        ]
    );
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(serialize(&doc).trim(), "param n = 3\nparam gap = n + 1");
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
    // A malformed `param` (no `=`, empty name, non-identifier) is rejected.
    assert!(parse("param n 3").is_err());
    assert!(parse("param = 3").is_err());
    assert!(parse("param 1n = 3").is_err());
}

/// A generative `inst` — a `[lo..hi]` range, an `if=` conditional, and expression
/// `p:(...)` params — parses to `GenDirective::InstGenerative` and round-trips as
/// authored (evaluated results are elaboration-only, never serialized).
#[test]
fn generative_inst_parses_and_round_trips() {
    let text = "inst sense[0..n] R_0402 if=(i < 3) p:idx=(i + 1) p:tol=5%";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    let mut params = BTreeMap::new();
    params.insert("tol".into(), "5%".into());
    let mut param_exprs = BTreeMap::new();
    param_exprs.insert("idx".into(), "i + 1".into());
    assert_eq!(
        src,
        vec![GenDirective::InstGenerative {
            path: "sense".into(),
            part: "R_0402".into(),
            params,
            param_exprs,
            label: None,
            range: Some(("0".into(), "n".into())),
            if_expr: Some("i < 3".into()),
        }]
    );
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

    // An ordinary indexed path (`dec[0]`, no `..`) is NOT a range — it stays a plain
    // Instance, so existing docs are untouched.
    let Parsed { source: plain, .. } = parse("inst dec[0] Cap").expect("plain");
    assert_eq!(
        plain,
        vec![GenDirective::Instance {
            path: "dec[0]".into(),
            part: "Cap".into(),
            params: BTreeMap::new(),
            label: None,
        }]
    );
}

/// Documented limitation: a param value containing `" ` (a double quote followed by
/// whitespace) serializes WITHOUT escaping the inner quote — `p:x="a" b"` — so the
/// tokenizer closes the quoted run at the inner `"` and the trailing `b"` is an
/// orphan token: the output does not reparse. Pinned here (the same limitation as
/// `text`-label serialization) so a future escaping fix updates this test on purpose.
#[test]
fn embedded_double_quote_is_a_documented_serialize_limitation() {
    let mut params = BTreeMap::new();
    params.insert("x".to_string(), "a\" b".to_string());
    let doc = doc_of(
        vec![GenDirective::Instance {
            path: "u1".into(),
            part: "MCU".into(),
            params,
            label: None,
        }],
        BTreeMap::new(),
    );
    let text = serialize(&doc);
    assert!(
        text.contains("p:x=\"a\" b\""),
        "unescaped inner quote expected: {text}"
    );
    assert!(
        parse(&text).is_err(),
        "embedded `\" ` value is not round-trippable (documented limitation)"
    );
}

/// Elaboration copies an instance's `params`/`label` verbatim onto its `Component`.
#[test]
fn elaboration_copies_params_and_label_onto_component() {
    let Parsed { source: src, .. } =
        parse("inst c1 Cap label=\"{value}\" p:value=100n").expect("parse");
    let doc = placed(src);
    let c = &doc.components[&EntityId::new("c1")];
    assert_eq!(c.label.as_deref(), Some("{value}"));
    assert_eq!(c.params["value"], "100n");
}

/// Range instantiation (Decision 21b): `inst dec[0..n] Cap` with `param n = 3`
/// elaborates to concrete `dec[0]`, `dec[1]`, `dec[2]` components — hi exclusive.
#[test]
fn range_expands_to_indexed_instances() {
    let src = "param n = 3\ninst dec[0..n] Cap p:value=(100n)";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert!(doc.components.contains_key(&EntityId::new("dec[0]")));
    assert!(doc.components.contains_key(&EntityId::new("dec[1]")));
    assert!(doc.components.contains_key(&EntityId::new("dec[2]")));
    assert!(
        !doc.components.contains_key(&EntityId::new("dec[3]")),
        "hi exclusive"
    );
    // The expression param evaluated onto each instance's verbatim params.
    assert_eq!(
        doc.components[&EntityId::new("dec[0]")].params["value"],
        "100n"
    );
}

/// The loop variable `i` is bound in each range instance's expressions.
#[test]
fn loop_variable_binds_in_range_expressions() {
    let src = "inst r[0..3] Cap p:idx=(i + 1)";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert_eq!(doc.components[&EntityId::new("r[0]")].params["idx"], "1");
    assert_eq!(doc.components[&EntityId::new("r[1]")].params["idx"], "2");
    assert_eq!(doc.components[&EntityId::new("r[2]")].params["idx"], "3");
}

/// Changing a range bound preserves surviving instances' identities and decays the
/// removed one through the existing reconciliation machinery (the reconciliation-
/// safety requirement). An override pinned to `dec[1]` survives `n: 3→4`, and one
/// pinned to `dec[3]` orphans (surfaced, never silently dropped) when `n: 4→3`.
#[test]
fn range_bound_change_reconciles_by_path() {
    let lib = part_library();
    let mut h = History::new(Default::default());
    // Start at n=3 (dec[0..3]); pin dec[1].
    let Parsed { source: s3, .. } = parse("param n = 3\ninst dec[0..n] Cap").expect("parse");
    h.commit(Transaction::one(Command::SetSource(s3)), &lib, "n3")
        .unwrap();
    h.commit(
        Transaction::one(Command::Pin(EntityId::new("dec[1]"), Point::mm(7, 3))),
        &lib,
        "pin",
    )
    .unwrap();
    assert_eq!(
        h.doc().components[&EntityId::new("dec[1]")].pos.value,
        Point::mm(7, 3),
        "pin holds dec[1] at n=3"
    );
    // Grow to n=4: dec[1]'s identity (and its pin) survives; dec[3] now exists.
    let Parsed { source: s4, .. } = parse("param n = 4\ninst dec[0..n] Cap").expect("parse4");
    h.commit(Transaction::one(Command::SetSource(s4)), &lib, "n4")
        .unwrap();
    assert!(h.doc().components.contains_key(&EntityId::new("dec[3]")));
    assert_eq!(
        h.doc().components[&EntityId::new("dec[1]")].pos.value,
        Point::mm(7, 3),
        "the pin on dec[1] survives the bound change (identity by path)"
    );
    assert!(h.doc().report.orphaned.is_empty());
    // Shrink back to n=3: dec[3] is gone; a pin on it (add one first) would orphan.
    h.commit(
        Transaction::one(Command::Pin(EntityId::new("dec[3]"), Point::mm(9, 9))),
        &lib,
        "pin3",
    )
    .unwrap();
    let Parsed { source: s3b, .. } = parse("param n = 3\ninst dec[0..n] Cap").expect("parse3b");
    h.commit(Transaction::one(Command::SetSource(s3b)), &lib, "shrink")
        .unwrap();
    assert!(!h.doc().components.contains_key(&EntityId::new("dec[3]")));
    assert!(
        h.doc().report.orphaned.contains(&EntityId::new("dec[3]")),
        "the removed instance's override is surfaced as an orphan, not dropped"
    );
}

/// `if=` population conditional: a false condition depopulates the instance, and a
/// connection referencing the dropped part is skipped with a `W_DNP` warning (the
/// chosen dangling-connection semantics) rather than an `E_UNKNOWN_INSTANCE` error.
#[test]
fn if_conditional_depopulates_and_dangles_as_warning() {
    // if=true keeps it.
    let Parsed { source: on, .. } = parse("inst c1 Cap if=(true)").expect("on");
    assert!(placed(on).components.contains_key(&EntityId::new("c1")));

    // if=false drops it; a net referencing it warns (W_DNP), does not error.
    let src = "param populate = false\n\
                   inst c1 Cap\n\
                   inst c2 Cap if=populate\n\
                   net GND c1.p2 c2.p2";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert!(doc.components.contains_key(&EntityId::new("c1")));
    assert!(
        !doc.components.contains_key(&EntityId::new("c2")),
        "c2 is depopulated by if=false"
    );
    // The net referencing c2 is surfaced as a DNP dangle (a warning), and c1 still
    // joins GND (the surviving pin is unaffected).
    assert!(
        doc.report.dnp_dangling.iter().any(|(_, p)| p == "c2"),
        "dangling connection to c2 recorded: {:?}",
        doc.report.dnp_dangling
    );
    let gnd = &doc.nets[&crate::id::NetId::new("GND")];
    assert!(gnd.members.iter().any(|m| m.comp.as_str() == "c1"));
    assert!(!gnd.members.iter().any(|m| m.comp.as_str() == "c2"));
}

/// A QUOTED `p:` value is always verbatim, even when it starts with `(` (M2 — the
/// escape hatch): `p:v="(5V)"` stores the literal `(5V)` and round-trips, while a
/// bare `p:v=(5)` is an expression.
#[test]
fn quoted_paren_value_is_verbatim_not_an_expression() {
    let Parsed { source, .. } = parse("inst c1 Cap p:v=\"(5V)\"").expect("parse");
    // Quoted ⇒ verbatim ⇒ stays a plain Instance with the literal value.
    assert_eq!(
        source,
        vec![GenDirective::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: {
                let mut m = BTreeMap::new();
                m.insert("v".into(), "(5V)".into());
                m
            },
            label: None,
        }]
    );
    let doc = doc_of(source.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, source);
    // A bare `(...)` IS an expression (routes to InstGenerative).
    let Parsed { source: ex, .. } = parse("inst c1 Cap p:v=(5)").expect("expr");
    assert!(matches!(ex[0], GenDirective::InstGenerative { .. }));
}

/// Unbalanced parentheses on the expression path are a PARSE-time error (m1), not a
/// deferred eval error — and `(1` no longer silently stays verbatim.
#[test]
fn unbalanced_parens_error_at_parse_time() {
    assert!(parse("inst c1 Cap p:v=(1").is_err()); // bare `(1` — was silently verbatim
    assert!(parse("inst c1 Cap if=(n > 0").is_err()); // unbalanced if=
    // A well-formed expression still parses.
    assert!(parse("inst c1 Cap p:v=(1)").is_ok());
    assert!(parse("inst c1 Cap if=(n > 0)").is_ok());
}

/// `if=(…)` re-serializes as the canonical paren form `if=(…)` (m2), not re-quoted.
#[test]
fn if_clause_serializes_as_canonical_parens() {
    let Parsed { source, .. } = parse("inst c1 Cap if=(n > 0)").expect("parse");
    let doc = doc_of(source.clone(), BTreeMap::new());
    let text = serialize(&doc);
    assert!(text.contains("if=(n > 0)"), "canonical paren form: {text}");
    assert!(!text.contains("if=\""), "not re-quoted: {text}");
    assert_eq!(parse(&text).unwrap().source, source);
}

/// A range's loop variable `i` shadows a doc-level `param i` (innermost wins) —
/// deterministic, per the documented rule.
#[test]
fn range_loop_variable_shadows_doc_level_param() {
    let src = "param i = 99\ninst r[0..2] Cap p:idx=(i)";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    // Inside the range, `i` is the loop index (0, 1), not the doc-level 99.
    assert_eq!(doc.components[&EntityId::new("r[0]")].params["idx"], "0");
    assert_eq!(doc.components[&EntityId::new("r[1]")].params["idx"], "1");
}

/// A PLACEMENT directive referencing a depopulated part is folded into the same
/// `W_DNP` dangling report as a connection (symmetric visibility), not silently
/// vanished.
#[test]
fn placement_ref_to_depopulated_part_warns() {
    let src = "inst anchor Cap\n\
                   inst c1 Cap if=(false)\n\
                   near c1 anchor 3mm";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert!(!doc.components.contains_key(&EntityId::new("c1")));
    assert!(
        doc.report.dnp_dangling.iter().any(|(_, p)| p == "c1"),
        "placement ref to c1 recorded as DNP dangle: {:?}",
        doc.report.dnp_dangling
    );
}

/// Every `E_EXPR` fault class aborts the commit (collect-all structural fault).
#[test]
fn expression_faults_abort_the_commit() {
    let lib = part_library();
    let commit = |src: &str| -> Result<(), Vec<crate::diagnostic::Diagnostic>> {
        let Parsed { source, .. } = parse(src).expect("parse");
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "x")
            .map(|_| ())
    };
    // unknown param
    assert!(commit("inst r[0..missing] Cap").is_err());
    // param cycle
    assert!(commit("param a = b + 1\nparam b = a + 1\ninst r[0..a] Cap").is_err());
    // type mismatch (bool as a range bound)
    assert!(commit("param f = true\ninst r[0..f] Cap").is_err());
    // inexact division in a param value
    assert!(commit("inst r1 Cap p:v=(1 / 3)").is_err());
    // negative bound
    assert!(commit("inst r[0..-1] Cap").is_err());
    // over the range cap
    assert!(commit("inst r[0..100000] Cap").is_err());
    // if= not a boolean
    assert!(commit("inst r1 Cap if=(1 + 1)").is_err());
}

/// A `class` directive parses to the expected `Class { name, ClassEntry }` (prefix,
/// template, and `p:`-namespaced defaults) and round-trips through `serialize`.
#[test]
fn class_directive_parses_and_round_trips() {
    let text = "class R prefix=RES template=\"{value:si:Ω}\" p:tol=5%";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    let mut defaults = BTreeMap::new();
    defaults.insert("tol".into(), "5%".into());
    assert_eq!(
        src,
        vec![GenDirective::Class {
            name: "R".into(),
            entry: ClassEntry {
                prefix: Some("RES".into()),
                template: Some("{value:si:Ω}".into()),
                defaults,
            },
        }]
    );
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

    // A bare `class <name>` (all fields defaulted) also round-trips.
    let Parsed { source: bare, .. } = parse("class LED").expect("bare");
    assert_eq!(
        bare,
        vec![GenDirective::Class {
            name: "LED".into(),
            entry: ClassEntry::default(),
        }]
    );
    let doc2 = doc_of(bare.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc2)).unwrap().source, bare);
}

/// A region directive parses to the expected `RegionDecl` (role, net, layer, and
/// points), and the inner-layer / keep-out-kind tokens round-trip.
#[test]
fn region_directive_parses_and_round_trips() {
    let text = "\
region conductor net=GND layer=B.Cu (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
region keepout-drill layer=In2.Cu (1mm, 1mm) (2mm, 1mm) (2mm, 2mm)";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    assert_eq!(
        src[0],
        GenDirective::Region(RegionDecl {
            shape: Shape2D::polygon(vec![
                Point::mm(0, 0),
                Point::mm(10, 0),
                Point::mm(10, 10),
                Point::mm(0, 10),
            ]),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "B.Cu".into(),
        })
    );
    assert_eq!(
        src[1],
        GenDirective::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(1, 1), Point::mm(2, 1), Point::mm(2, 2)]),
            role: Role::Keepout(KeepoutKind::Drill),
            net: None,
            layer: "In2.Cu".into(), // "In2.Cu" is 1-based ⇒ Inner(1).
        })
    );
    // Canonical serialization re-parses to the same source.
    let doc = doc_of(src.clone(), BTreeMap::new());
    assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
}

/// A `text` directive parses to the expected `GenDirective::Text` and round-trips,
/// with and without `rot=`. A quoted string containing a space survives intact.
#[test]
fn text_directive_parses_and_round_trips() {
    let text = "\
text \"R12\" (0mm, 0mm) h=1mm layer=F.SilkS
text \"VAL 3V3\" (2mm, 5mm) h=0.8mm layer=B.SilkS rot=90";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    assert_eq!(
        src[0],
        GenDirective::Text {
            string: "R12".into(),
            at: Point::mm(0, 0),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        }
    );
    assert_eq!(
        src[1],
        GenDirective::Text {
            string: "VAL 3V3".into(), // a quoted string with a space round-trips
            at: Point::mm(2, 5),
            height: 800_000,
            layer: "B.SilkS".into(),
            orient: Orient::from_deg(90).unwrap(),
        }
    );
    // Canonical serialization re-parses identically (silk tokens + rot survive).
    let doc = doc_of(src.clone(), BTreeMap::new());
    let canon = serialize(&doc);
    assert!(canon.contains("layer=F.SilkS"), "silk token:\n{canon}");
    assert!(canon.contains("rot=90"), "cardinal rot token:\n{canon}");
    assert_eq!(parse(&canon).unwrap().source, src);
}

#[test]
fn font_directive_parses_and_round_trips() {
    // `font "<path>"` — the doc-wide outline font (Decision 17); the path may contain
    // spaces (quoted).
    let text = "font \"/usr/share/fonts/My Font.ttf\"";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    assert_eq!(
        src[0],
        GenDirective::Font {
            path: "/usr/share/fonts/My Font.ttf".into(),
        }
    );
    let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
    assert!(canon.contains("font \""), "font token:\n{canon}");
    assert_eq!(parse(&canon).unwrap().source, src);
}

#[test]
fn text_string_may_contain_a_hash() {
    // A `#` inside a quoted text label is literal, not a comment (quote-aware strip),
    // so it round-trips. (`#` outside quotes still starts a comment.)
    let Parsed { source: src, .. } =
        parse("text \"P#1\" (0mm, 0mm) h=1mm layer=F.SilkS  # a real comment").expect("parse");
    let GenDirective::Text { string, .. } = &src[0] else {
        panic!("expected text, got {:?}", src[0]);
    };
    assert_eq!(
        string, "P#1",
        "the in-string # survived; the trailing # was stripped"
    );
    let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
    assert_eq!(
        parse(&canon).unwrap().source,
        src,
        "round-trips with the # intact"
    );
}

/// A region/text `layer=` accepts an **arbitrary slab-name token** (Decision 13),
/// stored verbatim and round-tripping exactly — including non-default names that no
/// longer map to a copper ordinal. Also exercises the text `layer=` default
/// (`F.SilkS`) and the region default (`F.Cu`).
#[test]
fn arbitrary_slab_names_round_trip() {
    let text = "\
region keepout layer=F.Fab (0mm, 0mm) (10mm, 0mm) (10mm, 10mm)
region conductor net=GND (0mm, 0mm) (5mm, 0mm) (5mm, 5mm)
text \"HELLO\" (1mm, 1mm) h=1mm layer=My.Custom.Layer
text \"WORLD\" (2mm, 2mm) h=1mm";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    // Verbatim storage of the authored names, and the two defaults.
    let GenDirective::Region(r0) = &src[0] else {
        panic!("region 0");
    };
    assert_eq!(
        r0.layer, "F.Fab",
        "arbitrary region slab name stored verbatim"
    );
    let GenDirective::Region(r1) = &src[1] else {
        panic!("region 1");
    };
    assert_eq!(r1.layer, "F.Cu", "region layer defaults to F.Cu");
    let GenDirective::Text { layer: l2, .. } = &src[2] else {
        panic!("text 2");
    };
    assert_eq!(
        l2, "My.Custom.Layer",
        "arbitrary text slab name stored verbatim"
    );
    let GenDirective::Text { layer: l3, .. } = &src[3] else {
        panic!("text 3");
    };
    assert_eq!(l3, "F.SilkS", "text layer defaults to F.SilkS");
    // Canonical serialization re-parses identically.
    let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
    assert!(
        canon.contains("layer=F.Fab"),
        "arbitrary name serialized:\n{canon}"
    );
    assert!(
        canon.contains("layer=My.Custom.Layer"),
        "verbatim:\n{canon}"
    );
    assert_eq!(
        parse(&canon).unwrap().source,
        src,
        "arbitrary slab names round-trip"
    );
}

/// `arc <mid> <end>` edges parse into `Seg::Arc`, mixed freely with straight edges,
/// and survive a canonical round-trip. A half-disc board (2 corners closed by an
/// arc) is accepted despite having < 3 corners.
#[test]
fn arc_edges_parse_and_round_trip() {
    let text = "\
board (-2mm, 0mm) arc (0mm, 2mm) (2mm, 0mm)
region conductor layer=F.Cu (0mm, 0mm) (4mm, 0mm) arc (5mm, 2mm) (4mm, 4mm) (0mm, 4mm)";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    // Board: a 2-corner arc polygon (half-disc).
    assert_eq!(
        src[0],
        GenDirective::Board {
            outline: Shape2D::polygon_path(
                Path {
                    start: Point::mm(-2, 0),
                    segs: vec![Seg::Arc {
                        mid: Point::mm(0, 2),
                        end: Point::mm(2, 0)
                    }],
                },
                0,
            )
        }
    );
    // Region: straight edges with one arc edge among them.
    match &src[1] {
        GenDirective::Region(r) => assert_eq!(
            r.shape.path().segs,
            vec![
                Seg::Line {
                    end: Point::mm(4, 0)
                },
                Seg::Arc {
                    mid: Point::mm(5, 2),
                    end: Point::mm(4, 4)
                },
                Seg::Line {
                    end: Point::mm(0, 4)
                },
            ],
        ),
        other => panic!("expected a region, got {other:?}"),
    }
    // Canonical serialization re-parses to the same source (arc markers survive).
    let doc = doc_of(src.clone(), BTreeMap::new());
    let canon = serialize(&doc);
    assert!(
        canon.contains("arc ("),
        "serialized form carries `arc` markers:\n{canon}"
    );
    assert_eq!(parse(&canon).unwrap().source, src);
}

#[test]
fn bezier_edges_parse_and_round_trip() {
    // A region with one quadratic and one cubic edge, mixed with straight edges.
    let text = "\
region conductor layer=F.Cu (0mm, 0mm) quad (2mm, 3mm) (4mm, 0mm) cubic (5mm, 2mm) (7mm, 2mm) (8mm, 0mm) (0mm, 4mm)";
    let Parsed { source: src, .. } = parse(text).expect("parse");
    match &src[0] {
        GenDirective::Region(r) => assert_eq!(
            r.shape.path().segs,
            vec![
                Seg::Quadratic {
                    ctrl: Point::mm(2, 3),
                    end: Point::mm(4, 0),
                },
                Seg::Cubic {
                    c1: Point::mm(5, 2),
                    c2: Point::mm(7, 2),
                    end: Point::mm(8, 0),
                },
                Seg::Line {
                    end: Point::mm(0, 4),
                },
            ],
        ),
        other => panic!("expected a region, got {other:?}"),
    }
    // Canonical serialization re-parses identically (quad/cubic markers survive).
    let doc = doc_of(src.clone(), BTreeMap::new());
    let canon = serialize(&doc);
    assert!(
        canon.contains("quad (") && canon.contains("cubic ("),
        "markers:\n{canon}"
    );
    assert_eq!(parse(&canon).unwrap().source, src);
}

#[test]
fn bezier_path_parse_errors_are_reported() {
    assert!(
        parse("board (0mm,0mm) cubic (1mm,1mm) (2mm,2mm)").is_err(),
        "cubic needs two controls AND an endpoint"
    );
    assert!(
        parse("board (0mm,0mm) quad (1mm,1mm)").is_err(),
        "quad needs a control AND an endpoint"
    );
}

#[test]
fn arc_path_parse_errors_are_reported() {
    assert!(
        parse("board (0mm,0mm) arc (1mm,1mm)").is_err(),
        "arc needs mid AND end"
    );
    assert!(
        parse("board arc (0mm,0mm) (1mm,1mm)").is_err(),
        "path must start with a coord"
    );
    assert!(
        parse("board (0mm,0mm) bogus (1mm,1mm)").is_err(),
        "unknown path token"
    );
}

/// Regions are assembled by the shared reader and survive a real commit (they do
/// not disturb elaboration — no fill/connectivity yet, just storage).
#[test]
fn regions_assemble_through_commit() {
    let lib = part_library();
    let mut h = History::new(Default::default());
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        GenDirective::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        // GND must be a connected net for the conductor pour to validate.
        GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("c0".into(), "p2".into())],
        },
        GenDirective::Region(RegionDecl {
            shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(20, 0), Point::mm(20, 20)]),
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "B.Cu".into(),
        }),
    ];
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "r")
        .expect("elaborates");
    let regions = crate::elaborate::regions(&h.doc().source);
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].role, Role::Conductor);
    assert_eq!(regions[0].net.as_deref(), Some("GND"));
    assert_eq!(regions[0].layer, "B.Cu");
}

/// `serialize(parse(serialize(doc))) == serialize(doc)` — canonical form is a
/// fixed point.
#[test]
fn idempotent() {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        EntityId::new("psu.dec[0]"),
        Override {
            pos: Some(Point { x: 1, y: 999_999 }),
            strength: Strength::Pin,
        },
    );
    let doc = doc_of(all_variants(), overrides);

    let once = serialize(&doc);
    let Parsed {
        source: src,
        overrides: ovr,
        ..
    } = parse(&once).unwrap();
    let twice = serialize(&doc_of(src, ovr));
    assert_eq!(once, twice);
}

/// Human-authored forms (mm/nm/bare units, comments, extra whitespace) parse to
/// the canonical model.
#[test]
fn tolerant_input_canonicalizes() {
    let text = "
            # a power rail
            inst   psu.reg   LDO        # the regulator
            place psu.reg (30mm, 20mm)
            fix   psu.reg (30000000nm, 20000000)   # mm, nm and bare all equal 30/20 mm
            near psu.reg psu.reg 0.5mm
        ";
    let Parsed { source: src, .. } = parse(text).unwrap();
    assert_eq!(
        src[1],
        GenDirective::Place {
            path: "psu.reg".into(),
            pos: Point::mm(30, 20)
        }
    );
    assert_eq!(
        src[2],
        GenDirective::Fix {
            path: "psu.reg".into(),
            pos: Point::mm(30, 20)
        }
    );
    assert_eq!(
        src[3],
        GenDirective::Near {
            a: "psu.reg".into(),
            b: "psu.reg".into(),
            within: 500_000
        }
    );
}

#[test]
fn canonical_length_forms() {
    assert_eq!(fmt_len(30 * MM), "30mm");
    assert_eq!(fmt_len(0), "0mm");
    assert_eq!(fmt_len(500_000), "0.5mm");
    assert_eq!(fmt_len(-5_500_000), "-5.5mm");
    assert_eq!(fmt_len(1), "0.000001mm");
    // every canonical form parses back to itself
    for v in [30 * MM, 0, 500_000, -5_500_000, 1, 12_345_678] {
        assert_eq!(parse_len(&fmt_len(v)).unwrap(), v, "round-trip {v}nm");
    }
}

// ---- elaboration equivalence ----------------------------------------

/// Re-elaborating the parsed `(source, overrides)` reproduces the same
/// materialized `components`, `nets`, and reconciliation `report`.
fn assert_elaboration_equiv(doc: &Doc) {
    let lib = part_library();
    let Parsed {
        source: src,
        overrides: ovr,
        refdes_pins: rp,
        ..
    } = parse(&serialize(doc)).expect("parse");
    let elab = elaborate(&src, &ovr, &rp, &lib).expect("elaborate");
    assert_eq!(elab.components, doc.components, "components diverged");
    assert_eq!(elab.nets, doc.nets, "nets diverged");
    assert_eq!(elab.report, doc.report, "report diverged");
}

#[test]
fn equiv_psu_module() {
    assert_elaboration_equiv(&placed(psu_module(3)));
}

#[test]
fn equiv_psu_module_with_overrides() {
    // An *effective* nudge + pin: kept, report stays clean, so it round-trips.
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(3))),
        &lib,
        "psu",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Nudge(
            EntityId::new("psu.dec[1]"),
            Point::mm(42, 7),
        )),
        &lib,
        "nudge",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Pin(EntityId::new("psu.dec[2]"), Point::mm(3, 30))),
        &lib,
        "pin",
    )
    .unwrap();
    let d = h.doc();
    assert!(
        d.report.decayed.is_empty(),
        "fixture should not have decayed hints"
    );
    assert_elaboration_equiv(d);
}

#[test]
fn equiv_uart_link() {
    assert_elaboration_equiv(&placed(uart_link()));
}

#[test]
fn equiv_placement_scene() {
    assert_elaboration_equiv(&placed(placement_scene()));
}

/// A scene using the physical-parts directives (Rotate + NearPin) round-trips
/// through text and re-elaborates identically.
#[test]
fn equiv_physical_scene() {
    let scene = vec![
        GenDirective::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        GenDirective::Fix {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        GenDirective::Rotate {
            path: "reg".into(),
            orient: Orient::from_deg(90).unwrap(),
        },
        GenDirective::NearPin {
            a: "dec".into(),
            b_comp: "reg".into(),
            b_pin: "VOUT".into(),
            within: 0,
        },
    ];
    assert_elaboration_equiv(&placed(scene));
}

/// `rotate` / `nearpin` parse from human-authored text (negative/over-360
/// degrees normalise; mm length on the pin proximity).
#[test]
fn parse_rotate_and_nearpin() {
    let Parsed { source: src, .. } = parse("rotate u1 -90\nnearpin c1 u1.VOUT 1.5mm").unwrap();
    assert_eq!(
        src[0],
        GenDirective::Rotate {
            path: "u1".into(),
            orient: Orient::from_deg(-90).unwrap(),
        }
    );
    assert_eq!(
        src[1],
        GenDirective::NearPin {
            a: "c1".into(),
            b_comp: "u1".into(),
            b_pin: "VOUT".into(),
            within: 1_500_000,
        }
    );
    // Off-axis angles are valid now (Stage 2) — lowered to a quaternion, not rejected.
    assert!(parse("rotate u1 45").is_ok());
    assert!(parse("rotate u1 30.5").is_ok());
    assert!(parse("rotate u1 notnum").is_err());
}

#[test]
fn arbitrary_angle_round_trips_as_a_quaternion() {
    // A non-cardinal angle lowers to a quaternion and serialises as `quat=(…)`
    // (the angle isn't exactly representable; the quaternion is the canonical form).
    let Parsed { source: src, .. } = parse("rotate u1 30").unwrap();
    let GenDirective::Rotate { orient, .. } = &src[0] else {
        panic!("expected a rotate, got {:?}", src[0]);
    };
    assert_eq!(*orient, Orient::from_angle_deg(30.0));
    assert_eq!(orient.to_deg(), 30, "≈ 30° about z");
    // Canonical form is the exact quaternion, and re-parses identically.
    let canon = render_directive(&src[0]);
    assert!(
        canon.starts_with("rotate u1 quat=("),
        "non-cardinal serialises as quat: {canon}"
    );
    assert_eq!(parse(&canon).unwrap().source, src);
    // A cardinal still serialises readably (and `bottom` survives).
    assert_eq!(
        render_directive(&parse("rotate u1 90 bottom").unwrap().source[0]),
        "rotate u1 90 bottom"
    );
}

#[test]
fn rotate_rejects_non_finite_and_tolerates_quat_whitespace() {
    // Non-finite angles must be a clean error, never a degenerate (0,0,0,0) orient.
    assert!(parse("rotate u1 nan").is_err());
    assert!(parse("rotate u1 inf").is_err());
    assert!(parse("rotate u1 -inf").is_err());
    assert!(parse("rotate u1 1e309").is_err()); // overflows f64 to +inf
    // `quat=` tolerates whitespace after commas (same as the no-space canonical form).
    let spaced = parse("rotate u1 quat=(1, 0, 0, 1)").unwrap().source;
    let tight = parse("rotate u1 quat=(1,0,0,1)").unwrap().source;
    assert_eq!(spaced, tight);
    assert!(parse("rotate u1 quat=(0,0,0,0)").is_err());
}

#[test]
fn rotate_bottom_authoring_round_trips() {
    let Parsed { source: src, .. } = parse("rotate u1 90 bottom").unwrap();
    assert_eq!(
        src[0],
        GenDirective::Rotate {
            path: "u1".into(),
            orient: Orient::from_deg(90).unwrap().flipped(),
        }
    );
    // Canonical serialization carries the `bottom` flag and re-parses identically.
    assert_eq!(render_directive(&src[0]), "rotate u1 90 bottom");
    assert_eq!(parse("rotate u1 90").unwrap().source[0], {
        GenDirective::Rotate {
            path: "u1".into(),
            orient: Orient::from_deg(90).unwrap(),
        }
    });
    // A stray third token that isn't `bottom` is an error.
    assert!(parse("rotate u1 90 sideways").is_err());
}

// ---- LoadText command (text -> tier-1 in one atomic transaction) -----

#[test]
fn load_text_replaces_state_and_matches_set_source() {
    let lib = part_library();

    // Reference: build the scene via the data API.
    let reference = placed(placement_scene());

    // Same scene authored as text, loaded atomically.
    let text = serialize(&reference);
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::LoadText(text)), &lib, "load")
        .unwrap();
    let loaded = h.doc();

    assert_eq!(loaded.source, reference.source);
    assert_eq!(loaded.components, reference.components);
    assert_eq!(loaded.nets, reference.nets);
}

#[test]
fn load_text_is_atomic_on_parse_error() {
    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu",
    )
    .unwrap();
    let before = crate::project::render(h.doc());
    // Garbage text must fail and leave head untouched.
    let r = h.commit(
        Transaction::one(Command::LoadText("inst onlyonetoken".into())),
        &lib,
        "bad",
    );
    assert!(r.is_err());
    assert_eq!(before, crate::project::render(h.doc()));
}

// ---- parse errors ----------------------------------------------------

#[test]
fn parse_error_unknown_directive() {
    let e = crate::diagnostic::render(&parse("frobnicate a b").unwrap_err());
    assert!(e.contains("unknown directive"), "got: {e}");
    assert!(
        e.contains("frobnicate"),
        "error should name the offending line: {e}"
    );
}

#[test]
fn parse_error_bad_coordinate() {
    let e = crate::diagnostic::render(&parse("place foo (3mm)").unwrap_err());
    assert!(
        e.contains("1:1"),
        "error should carry the line location: {e}"
    );
}

/// A `hole` parses with a positive diameter and round-trips; a zero or negative
/// diameter is rejected at parse (a degenerate/negative drill tool must not slip
/// silently into the Excellon output).
#[test]
fn hole_requires_positive_diameter() {
    let ok = parse("hole (4mm, 4mm) dia=2.7mm").unwrap();
    assert_eq!(ok.source.len(), 1, "one hole directive");
    assert!(
        parse("hole (4mm, 4mm) dia=0mm").is_err(),
        "zero diameter rejected"
    );
    let e = crate::diagnostic::render(&parse("hole (4mm, 4mm) dia=-1mm").unwrap_err());
    assert!(e.contains("must be positive"), "negative rejected: {e}");
}

#[test]
fn parse_error_bad_pin_ref() {
    let e = crate::diagnostic::render(&parse("net VBUS nodotpin").unwrap_err());
    assert!(e.contains("<comp>"), "got: {e}");
}

/// Collect-all: several malformed lines are all reported in one parse, each
/// located by line number — not just the first.
#[test]
fn parse_collects_all_line_errors() {
    let diags = parse("frobnicate x\ninst u1 LDO\nplace foo (3mm)").unwrap_err();
    assert_eq!(diags.len(), 2, "both bad lines reported: {diags:?}");
    let text = crate::diagnostic::render(&diags);
    assert!(
        text.contains("1:1") && text.contains("3:1"),
        "located by line: {text}"
    );
}

// ---- coordinate-range ceiling (issue 0018) ---------------------------

/// A point beyond ±MAX_COORD (1 m) is a hard `E_COORD_RANGE` error at the text
/// boundary — never a silent i128 wrap in the geometry kernel downstream.
#[test]
fn parse_rejects_out_of_range_point() {
    let diags = parse("place foo (2000mm, 0)").unwrap_err();
    assert!(
        diags.iter().any(|d| d.code == "E_COORD_RANGE"),
        "expected E_COORD_RANGE: {diags:?}"
    );
}

/// An oversized length (a text height here) is caught too — the walker bounds
/// every nm a directive contributes, not only point coordinates.
#[test]
fn parse_rejects_out_of_range_height() {
    let diags = parse(r#"text "A" (0mm, 0mm) h=2000mm layer=F.SilkS"#).unwrap_err();
    assert!(
        diags.iter().any(|d| d.code == "E_COORD_RANGE"),
        "expected E_COORD_RANGE: {diags:?}"
    );
}

/// A coordinate exactly at the bound (1 m = MAX_COORD) is accepted; the ceiling
/// is inclusive, so real board-scale geometry is never rejected.
#[test]
fn parse_accepts_coordinate_at_the_bound() {
    assert!(
        parse("place foo (1000mm, 0)").is_ok(),
        "1 m = MAX_COORD must be accepted"
    );
}

/// The command surface enforces the same ceiling as the text parser: an
/// out-of-range `Pin` position is rejected with `E_COORD_RANGE`, so the geometry
/// kernel never sees a coordinate that could overflow i128 (issue 0018).
#[test]
fn command_ingress_rejects_out_of_range_pin() {
    let lib = part_library();
    let doc = Doc::default();
    let err = crate::command::apply(
        &doc,
        &Transaction::one(Command::Pin(EntityId::new("x"), Point::mm(2000, 0))),
        &lib,
        1,
    )
    .unwrap_err();
    assert!(
        err.iter().any(|d| d.code == "E_COORD_RANGE"),
        "expected E_COORD_RANGE: {err:?}"
    );
}

// ---- nested block grammar (Phase 0 infrastructure) -------------------

/// A leaf block, for building expected trees compactly in assertions.
fn leaf(header: &str, line: u32) -> Block {
    let (keyword, tokens, rest) = split_header(header);
    Block {
        keyword,
        tokens,
        rest,
        opened_block: false,
        children: Vec::new(),
        line,
    }
}

/// The nested block within a body node, for terse child assertions.
fn as_block(n: &Node) -> &Block {
    match n {
        Node::Block(b) => b,
        other => panic!("expected a Node::Block, got {other:?}"),
    }
}

/// Nesting to 3+ levels builds the expected tree, with header tokens pre-split and
/// children in source order. (No keyword accepts a block yet, so this exercises the
/// generic representation via `parse_blocks`, not `parse`.)
#[test]
fn blocks_nest_to_arbitrary_depth() {
    let text = "\
row main gap=2mm {
  column left {
    def inner {
      inst r1 R
    }
  }
  inst c1 Cap
}";
    let forest = parse_blocks(text).expect("parse_blocks");
    // Top level: one `row` opener.
    assert_eq!(forest.len(), 1);
    let row = &forest[0];
    assert_eq!(row.keyword, "row");
    assert_eq!(row.tokens, vec!["row", "main", "gap=2mm"]);
    assert_eq!(row.rest, "main gap=2mm");
    assert!(row.opened_block);
    // `row` has two children: `column` (a block) then `inst c1` (a leaf), in order.
    assert_eq!(row.children.len(), 2);
    let col = as_block(&row.children[0]);
    assert_eq!(col.keyword, "column");
    assert!(col.opened_block);
    assert_eq!(row.children[1], Node::Block(leaf("inst c1 Cap", 7)));
    // `column` -> `def` (block) -> `inst r1` (leaf), 3 levels below the row.
    let def = as_block(&col.children[0]);
    assert_eq!(def.keyword, "def");
    assert!(def.opened_block);
    assert_eq!(def.children, vec![Node::Block(leaf("inst r1 R", 4))]);
}

/// An empty block still round-trips as a block (opened_block true, no children) —
/// the distinction the flat path relies on to reject an empty block on a keyword
/// that does not take one.
#[test]
fn empty_block_is_still_a_block() {
    let forest = parse_blocks("def empty {\n}").expect("parse");
    assert_eq!(forest.len(), 1);
    assert!(forest[0].opened_block);
    assert!(forest[0].children.is_empty());
}

/// Comments and blank lines *inside* a block are preserved as trivia nodes, in
/// order, and round-trip byte-faithfully through serialize -> parse -> serialize
/// (Decision 21 mixed authorship). Top-level trivia stays dropped (the flat path's
/// pre-existing behavior).
#[test]
fn block_interior_trivia_round_trips() {
    let text = "\
# top-level comment (dropped, as always)
def amp {
  # bias network
  inst r1 R

  # decoupling
  inst c1 Cap
}
";
    let forest = parse_blocks(text).expect("parse");
    let def = &forest[0];
    // The body preserves the two comments, the blank, and two directives in order.
    assert_eq!(
        def.children,
        vec![
            Node::Comment("bias network".into()),
            Node::Block(leaf("inst r1 R", 4)),
            Node::Blank,
            Node::Comment("decoupling".into()),
            Node::Block(leaf("inst c1 Cap", 7)),
        ]
    );
    // Round-trip: the canonical form (top-level comment stripped) is a fixed point,
    // and the interior trivia survives byte-for-byte.
    let canon = serialize_blocks(&forest);
    let expected = "\
def amp {
  # bias network
  inst r1 R

  # decoupling
  inst c1 Cap
}
";
    assert_eq!(
        canon, expected,
        "interior trivia round-trips byte-faithfully"
    );
    // Structural fixpoint: re-parsing the canonical form and re-serializing is a
    // fixed point. (The tree carries source line numbers, which legitimately differ
    // between the original — with its dropped top-level comment on line 1 — and the
    // canonical form; the byte-identity above is the faithful-round-trip guarantee.)
    let reforest = parse_blocks(&canon).unwrap();
    assert_eq!(
        serialize_blocks(&reforest),
        canon,
        "canonical form is a fixpoint"
    );
}

/// A `def` carrying a Decision-20 `schematic { … }` layout fragment (over its internal
/// paths) parses into `GenDirective::Def { layout: Some(..) }` and round-trips
/// byte-identically: the fragment re-emits as an indented block after the body/ports.
#[test]
fn def_with_layout_fragment_round_trips() {
    let text = "\
def rc {
  inst R1 R
  inst C1 Cap
  net mid R1.p2 C1.p1
  port out = C1.p2
  schematic {
    row {
      sym R1
      sym C1
    }
  }
}
";
    let parsed = parse(text).expect("parse");
    // The def carries the parsed layout fragment (last-block-wins; here the only one).
    let layout = match &parsed.source[0] {
        GenDirective::Def { layout, .. } => layout.clone(),
        other => panic!("expected a def, got {other:?}"),
    };
    let frag = layout.expect("def carries a schematic layout fragment");
    // Two internal syms (`R1`, `C1`), def-relative — NOT yet instance-prefixed.
    assert_eq!(frag.symbol_paths(), vec!["R1", "C1"]);

    // Byte-lossless round-trip: serialize the doc and re-parse to the canonical form.
    let doc = doc_of(parsed.source.clone(), BTreeMap::new());
    let canon = serialize(&doc);
    assert_eq!(canon, text, "def-with-layout serializes byte-identically");
    assert_eq!(
        parse(&canon).unwrap().source,
        parsed.source,
        "re-parse of the canonical form reproduces the source"
    );
}

/// An unbalanced `{` (a block never closed) is an `E_BLOCK` error located at the
/// opener's line.
#[test]
fn unbalanced_open_is_an_error() {
    let err = parse_blocks("row a {\n  inst r1 R\n").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
    let rendered = crate::diagnostic::render(&err);
    assert!(rendered.contains("1:1"), "located at opener: {rendered}");
    assert!(
        rendered.contains("never closed"),
        "names the failure: {rendered}"
    );
}

/// A `}` with no open block is an `E_BLOCK` error located at the stray close.
#[test]
fn stray_close_is_an_error() {
    let err = parse_blocks("inst r1 R\n}").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
    let rendered = crate::diagnostic::render(&err);
    assert!(
        rendered.contains("2:1"),
        "located at the stray `}}`: {rendered}"
    );
    assert!(
        rendered.contains("no open block"),
        "names the failure: {rendered}"
    );
}

/// Braces inside a quoted value are literal: a trailing `{` inside quotes does not
/// open a block, and `{`/`}` within a quoted run do not confuse balancing.
#[test]
fn braces_inside_quotes_are_literal() {
    // Trailing `{` inside a quoted value: NOT a block opener.
    let forest = parse_blocks("inst r1 R label=\"a { b\"").expect("parse");
    assert_eq!(forest.len(), 1);
    assert!(
        !forest[0].opened_block,
        "a `{{` inside quotes must not open a block"
    );
    // A lone-looking `}` that is actually inside a quoted value is not a close: the
    // whole thing is one directive, and the quoted braces are preserved verbatim.
    let forest = parse_blocks("text \"x{y}z\" (0,0) h=1mm").expect("parse");
    assert_eq!(forest.len(), 1);
    assert!(!forest[0].opened_block);
    assert!(forest[0].rest.contains("x{y}z"), "quoted braces preserved");
}

/// Comment stripping is quote-aware and runs *before* brace detection: a `{` after
/// a `#` comment is stripped away and never opens a block; a `{` before a comment
/// still opens one.
#[test]
fn brace_after_comment_does_not_open_a_block() {
    // `{` lives in the comment: stripped, so no block opens.
    let forest = parse_blocks("inst r1 R  # note { not a block").expect("parse");
    assert_eq!(forest.len(), 1);
    assert!(!forest[0].opened_block, "commented `{{` is not an opener");
    // `{` before the comment DOES open a block (comment stripped off the tail first).
    let forest = parse_blocks("row a {  # opens here\n}").expect("parse");
    assert_eq!(forest.len(), 1);
    assert!(forest[0].opened_block, "pre-comment `{{` opens a block");
}

/// No existing keyword accepts a block, so a block opened on a current keyword is a
/// hard parse error through the full `parse` surface — existing documents are
/// unchanged, and a stray block cannot silently become an empty directive.
#[test]
fn block_on_existing_keyword_is_rejected() {
    let err = parse("inst r1 R {\n}").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
    let rendered = crate::diagnostic::render(&err);
    assert!(
        rendered.contains("does not take a block"),
        "clear message: {rendered}"
    );
    // The children of the rejected block are not lowered as directives.
    assert!(
        !rendered.contains("unknown directive"),
        "children not descended into: {rendered}"
    );
}

/// serialize -> parse -> serialize is a fixed point for a block tree, with canonical
/// two-space-per-depth indentation.
#[test]
fn block_serialize_is_a_fixpoint() {
    let text = "\
row main gap=2mm {
  column left {
    inst r1 R
  }
  inst c1 Cap
}
inst top MCU
";
    let forest = parse_blocks(text).expect("parse");
    let once = serialize_blocks(&forest);
    // Canonical indentation is exactly what we authored above.
    assert_eq!(once, text, "canonical two-space indent per depth");
    let reforest = parse_blocks(&once).expect("re-parse");
    let twice = serialize_blocks(&reforest);
    assert_eq!(once, twice, "serialize is a fixed point");
    assert_eq!(forest, reforest, "the tree round-trips structurally");
}

/// The flat (blockless) document path is byte-for-byte unchanged: a full-coverage
/// source serialized by the `Doc` serializer parses back through the new
/// block-aware `parse` identically to before.
#[test]
fn flat_documents_are_unchanged_through_blocks() {
    let doc = doc_of(all_variants(), BTreeMap::new());
    let text = serialize(&doc);
    // parse -> the same source, and the flat forest has no openers.
    assert_eq!(parse(&text).unwrap().source, doc.source);
    let forest = parse_blocks(&text).expect("parse_blocks");
    assert!(
        forest.iter().all(|b| !b.opened_block),
        "a canonical Doc serialization contains no blocks"
    );
}

/// A block opener with no directive before `{` (e.g. a lone `{`) is rejected by
/// `parse_blocks` itself — the public API guardrail (finding 4), so a malformed
/// opener never reaches a consumer nor serializes to a leading-space line.
#[test]
fn empty_keyword_block_is_rejected_by_parse_blocks() {
    let err = parse_blocks("{\n}").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
    let rendered = crate::diagnostic::render(&err);
    assert!(
        rendered.contains("no directive before"),
        "clear message: {rendered}"
    );
}

/// Collect-all through a rejected block: an unaccepted block's *children* are still
/// line-parsed, so their own syntax errors surface in the same pass as the
/// `E_BLOCK` rejection (finding 5) — the author fixes both at once, not in two
/// rounds.
#[test]
fn rejected_block_still_reports_child_errors() {
    // `inst` does not take a block; its child is itself a bad line.
    let err = parse("inst u1 MCU {\n  frobnicate x\n}").unwrap_err();
    let rendered = crate::diagnostic::render(&err);
    assert!(
        err.iter().any(|d| d.code == "E_BLOCK"),
        "the block rejection: {rendered}"
    );
    assert!(
        rendered.contains("unknown directive") && rendered.contains("frobnicate"),
        "the child's own error surfaces too: {rendered}"
    );
    // The child error is located on its own line (2), not the opener's (1).
    assert!(
        rendered.contains("2:1"),
        "child located by line: {rendered}"
    );
}

/// The `parse_forest` descent path is exercised end-to-end by a `cfg(test)`
/// block-accepting keyword (finding 3): a block on `testblock` is *not* rejected,
/// its children are descended into and lowered as ordinary directives into
/// `parsed.source`, and the block tree serializes to a fixed point. This gives
/// Phase 1 a tested recursion path rather than a latent one.
#[test]
fn accepted_block_descends_into_children() {
    assert!(
        keyword_takes_block(TEST_BLOCK_KEYWORD),
        "the sentinel keyword opts into blocks"
    );
    let text = "\
testblock amp {
  inst r1 R
  inst c1 Cap
}
inst top MCU
";
    let parsed = parse(text).expect("accepted block parses without E_BLOCK");
    // The descent lowered both children plus the trailing top-level directive; the
    // `testblock` header itself contributes no directive (a real consumer owns it).
    assert_eq!(
        parsed.source,
        vec![
            GenDirective::Instance {
                path: "r1".into(),
                part: "R".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "top".into(),
                part: "MCU".into(),
                params: BTreeMap::new(),
                label: None,
            },
        ]
    );
    // A syntax error *inside* an accepted block is reported at its own line.
    let err = parse("testblock a {\n  place foo (3mm)\n}").unwrap_err();
    assert!(
        crate::diagnostic::render(&err).contains("2:1"),
        "child error located by line: {err:?}"
    );
    // The block tree round-trips (serialize -> parse -> serialize fixpoint).
    let forest = parse_blocks(text).unwrap();
    let once = serialize_blocks(&forest);
    assert_eq!(parse_blocks(&once).unwrap(), forest);
}

#[test]
fn parse_never_panics_on_junk() {
    // A pile of malformed lines: each must yield an Err, none may panic.
    for junk in [
        "(((",
        "near a b",
        "near a b notanumber",
        "place x (1mm, )",
        "place x (1mm, 2mm, 3mm)",
        "fix x (1.1234567mm, 0)",
        "connect a.b",
        "inst",
    ] {
        assert!(parse(junk).is_err(), "expected Err for `{junk}`");
    }
}

// ---- Decision-20 schematic layout grammar ---------------------------

use crate::schematic::{Align, Direction, LayoutNode, SchematicLayout};

/// Parse a schematic block, asserting success, and return its layout.
fn parse_layout(text: &str) -> SchematicLayout {
    parse(text)
        .unwrap_or_else(|e| panic!("parse failed: {e:?}"))
        .schematic
        .expect("a schematic block")
}

#[test]
fn schematic_block_parses_containers_and_syms() {
    let layout = parse_layout(
        "schematic {\n  row power gap=2mm align=center {\n    sym C1\n    sym U1 rot=90 dx=1mm dy=-2mm\n  }\n  column {\n    sym C2\n  }\n}\n",
    );
    assert_eq!(layout.roots.len(), 2);
    let LayoutNode::Container(power) = &layout.roots[0] else {
        panic!("expected a container");
    };
    assert_eq!(power.dir, Direction::Row);
    assert_eq!(power.name.as_deref(), Some("power"));
    assert_eq!(power.gap, 2_000_000);
    assert_eq!(power.align, Align::Center);
    // The second child of the row is a rotated, pinned symbol.
    let LayoutNode::Symbol(u1) = &power.children[1] else {
        panic!("expected a symbol");
    };
    assert_eq!(u1.path, "U1");
    assert_eq!(u1.rot, Orient::from_deg(90).unwrap());
    assert_eq!(u1.dx, 1_000_000);
    assert_eq!(u1.dy, -2_000_000);
}

#[test]
fn schematic_round_trips_byte_identical() {
    // Canonical text -> parse -> serialize reproduces the input exactly. Note the
    // canonical omissions (align=start, rot=0, dx/dy=0, gap=0 are all elided).
    let canonical = "inst C1 Cap\ninst U1 MCU\nschematic {\n  row power gap=2mm {\n    sym C1\n    sym U1 rot=90\n  }\n  column align=end {\n    sym C1 dx=1mm\n  }\n}\n";
    let parsed = parse(canonical).unwrap();
    let doc = Doc {
        source: parsed.source,
        schematic: parsed.schematic,
        ..Default::default()
    };
    assert_eq!(serialize(&doc), canonical);
}

#[test]
fn schematic_preserves_trivia_round_trip() {
    // Comments and blank lines inside the block survive a round-trip (Decision 20/21).
    let canonical = "schematic {\n  # power section\n  row {\n    sym C1\n\n    sym C2\n  }\n}\n";
    let parsed = parse(canonical).unwrap();
    let doc = Doc {
        schematic: parsed.schematic,
        ..Default::default()
    };
    assert_eq!(serialize(&doc), canonical);
}

#[test]
fn schematic_serialize_parse_fixpoint() {
    // A second round is a fixpoint even from a non-canonical (extra-spaced) authoring.
    let authored = "schematic {\n   row   power   gap=2mm   {\n      sym C1\n   }\n}\n";
    let doc1 = Doc {
        schematic: parse(authored).unwrap().schematic,
        ..Default::default()
    };
    let once = serialize(&doc1);
    let doc2 = Doc {
        schematic: parse(&once).unwrap().schematic,
        ..Default::default()
    };
    assert_eq!(serialize(&doc2), once);
}

#[test]
fn nesting_is_arbitrary() {
    let layout = parse_layout(
        "schematic {\n  row {\n    column {\n      row {\n        sym C1\n      }\n    }\n  }\n}\n",
    );
    // Walk three levels down to the symbol.
    let mut node = &layout.roots[0];
    for _ in 0..3 {
        let LayoutNode::Container(c) = node else {
            panic!("expected container");
        };
        node = &c.children[0];
    }
    assert!(matches!(node, LayoutNode::Symbol(s) if s.path == "C1"));
}

#[test]
fn wire_parses_straight_and_via() {
    use crate::schematic::Wire;
    let layout = parse_layout(
        "schematic {\n  row {\n    wire C1.p1 C2.p2\n    wire U1.tx U2.rx via (1mm, 2mm) (3mm, -4mm)\n  }\n}\n",
    );
    let LayoutNode::Container(row) = &layout.roots[0] else {
        panic!("expected a container");
    };
    // A straight wire (no waypoints), endpoints split at the last dot.
    let LayoutNode::Wire(Wire { a, b, waypoints }) = &row.children[0] else {
        panic!("expected a wire");
    };
    assert_eq!((a.comp.as_str(), a.pin.as_str()), ("C1", "p1"));
    assert_eq!((b.comp.as_str(), b.pin.as_str()), ("C2", "p2"));
    assert!(waypoints.is_empty());
    // A routed wire with two waypoints.
    let LayoutNode::Wire(Wire { waypoints, .. }) = &row.children[1] else {
        panic!("expected a wire");
    };
    assert_eq!(
        waypoints,
        &vec![
            Point {
                x: 1_000_000,
                y: 2_000_000
            },
            Point {
                x: 3_000_000,
                y: -4_000_000
            },
        ]
    );
}

#[test]
fn wire_hierarchical_path_splits_at_last_dot() {
    use crate::schematic::Wire;
    // A hierarchical comp path (with dots and an index) survives — only the *last* dot
    // separates the pin, matching the `nearpin` idiom.
    let layout = parse_layout("schematic {\n  wire psu.dec[0].p1 mcu.rst\n}\n");
    let LayoutNode::Wire(Wire { a, b, .. }) = &layout.roots[0] else {
        panic!("expected a wire");
    };
    assert_eq!((a.comp.as_str(), a.pin.as_str()), ("psu.dec[0]", "p1"));
    assert_eq!((b.comp.as_str(), b.pin.as_str()), ("mcu", "rst"));
}

#[test]
fn wire_round_trips_byte_identical() {
    let canonical =
        "schematic {\n  row {\n    wire C1.p1 C2.p2\n    wire U1.tx U2.rx via (1mm, 2mm)\n  }\n}\n";
    let doc = Doc {
        schematic: parse(canonical).unwrap().schematic,
        ..Default::default()
    };
    assert_eq!(serialize(&doc), canonical);
}

#[test]
fn wire_errors_are_e_schematic() {
    // A one-endpoint wire, a non-`comp.pin` endpoint, a `via` with no waypoint, and a
    // block on a `wire` leaf are all hard `E_SCHEMATIC`.
    for bad in [
        "schematic {\n  wire C1.p1\n}\n",
        "schematic {\n  wire C1 C2.p2\n}\n",
        "schematic {\n  wire C1.p1 C2.p2 via\n}\n",
        "schematic {\n  wire C1.p1 C2.p2 {\n  }\n}\n",
    ] {
        let err = parse(bad).unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_SCHEMATIC"),
            "expected E_SCHEMATIC for `{bad}`, got {err:?}"
        );
    }
}

#[test]
fn wire_waypoints_are_range_checked() {
    // A waypoint past ±1 m is E_COORD_RANGE, same discipline as `gap`/`dx` (issue 0018).
    let err = parse("schematic {\n  wire C1.p1 C2.p2 via (2000mm, 0mm)\n}\n").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_COORD_RANGE"), "{err:?}");
}

#[test]
fn doc_without_schematic_block_is_byte_identical() {
    // The poc guard: a blockless doc serializes exactly as before this feature.
    let src = "inst C1 Cap\ninst C2 Cap\nnet N1 C1.p1 C2.p1\n";
    let doc = Doc {
        source: parse(src).unwrap().source,
        ..Default::default()
    };
    assert_eq!(serialize(&doc), src);
    assert!(doc.schematic.is_none());
}

#[test]
fn last_schematic_block_wins() {
    let layout = parse_layout("schematic {\n  sym C1\n}\nschematic {\n  sym C2\n}\n");
    // The second block replaces the first.
    assert_eq!(layout.roots.len(), 1);
    assert!(matches!(&layout.roots[0], LayoutNode::Symbol(s) if s.path == "C2"));
}

#[test]
fn bad_child_keyword_is_e_schematic() {
    let err = parse("schematic {\n  inst C1 Cap\n}\n").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
}

#[test]
fn row_outside_schematic_is_e_schematic() {
    let err = parse("row {\n  sym C1\n}\n").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
}

#[test]
fn sym_with_block_is_e_schematic() {
    let err = parse("schematic {\n  sym C1 {\n  }\n}\n").unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
}

#[test]
fn bad_align_and_rot_are_errors() {
    assert!(parse("schematic {\n  row align=middle {\n    sym C1\n  }\n}\n").is_err());
    assert!(parse("schematic {\n  row {\n    sym C1 rot=45\n  }\n}\n").is_err());
    assert!(parse("schematic {\n  row {\n    sym C1 bogus=1\n  }\n}\n").is_err());
}

#[test]
fn schematic_takes_no_args() {
    assert!(parse("schematic foo {\n  sym C1\n}\n").is_err());
}

#[test]
fn names_and_paths_with_structural_chars_round_trip() {
    // An `inst` path is unrestricted, so a comp path (and a container name) may hold
    // `=`, `#`, or spaces. Such tokens must serialize quoted and re-parse identically
    // (regression: unquoted, `=` split the token into a bogus attribute and `#` was
    // silently truncated by the comment stripper).
    for (name, path) in [
        ("a=b", "u=1"),
        ("has space", "sens[0].fb"),
        ("with#hash", "n#2"),
    ] {
        let layout = SchematicLayout {
            roots: vec![LayoutNode::Container(crate::schematic::Container {
                dir: Direction::Row,
                name: Some(name.into()),
                gap: 0,
                align: Align::Start,
                children: vec![LayoutNode::Symbol(crate::schematic::Symbol {
                    path: path.into(),
                    rot: Orient::IDENTITY,
                    dx: 0,
                    dy: 0,
                })],
            })],
        };
        let doc = Doc {
            schematic: Some(layout.clone()),
            ..Default::default()
        };
        let text = serialize(&doc);
        let reparsed = parse(&text).unwrap().schematic.unwrap();
        assert_eq!(
            reparsed, layout,
            "round-trip failed for name={name:?} path={path:?} via `{text}`"
        );
    }
}

#[test]
fn schematic_lengths_are_range_checked() {
    // Authored lengths obey the issue-0018 ingress bound (MAX_COORD), like every other
    // coordinate — an over-bound `gap`/`dx` is E_COORD_RANGE at parse, not an
    // add-overflow panic in reflow.
    let over = crate::geom::MAX_COORD + 1;
    let gap_err = parse(&format!(
        "schematic {{\n  row gap={over}nm {{\n    sym C1\n  }}\n}}\n"
    ))
    .unwrap_err();
    assert!(gap_err.iter().any(|d| d.code == "E_COORD_RANGE"));
    let dx_err = parse(&format!(
        "schematic {{\n  row {{\n    sym C1 dx={over}nm\n  }}\n}}\n"
    ))
    .unwrap_err();
    assert!(dx_err.iter().any(|d| d.code == "E_COORD_RANGE"));
}

#[test]
fn max_coord_scale_lengths_reflow_without_panic() {
    // A gap/dx at the MAX_COORD ceiling parses and reflows cleanly (no overflow).
    let big = crate::geom::MAX_COORD;
    let doc = Doc {
            schematic: parse(&format!(
                "schematic {{\n  row gap={big}nm {{\n    sym C1 dx={big}nm dy=-{big}nm\n    sym C2\n  }}\n}}\n"
            ))
            .unwrap()
            .schematic,
            ..Default::default()
        };
    let lib = part_library();
    let parts = BTreeMap::from([
        (EntityId::new("C1"), "Cap".to_string()),
        (EntityId::new("C2"), "Cap".to_string()),
    ]);
    // Must not panic (debug add-overflow) — the whole point of the range check.
    let placed = crate::schematic::reflow(&doc.schematic.unwrap(), &parts, &lib, &BTreeMap::new());
    assert_eq!(placed.len(), 2);
}

#[test]
fn canonical_defaults_are_elided_first_pass() {
    // Explicitly-authored defaults (align=start, rot=0, gap=0, dx=0, dy=0) all elide on
    // the FIRST serialization — guards against a regression that starts emitting them
    // (which the already-canonical fixpoint tests would not catch).
    let authored =
        "schematic {\n  row power gap=0mm align=start {\n    sym C1 rot=0 dx=0mm dy=0mm\n  }\n}\n";
    let expected = "schematic {\n  row power {\n    sym C1\n  }\n}\n";
    let doc = Doc {
        schematic: parse(authored).unwrap().schematic,
        ..Default::default()
    };
    assert_eq!(serialize(&doc), expected);
}

#[test]
fn load_text_carries_and_validates_schematic() {
    // End-to-end: LoadText parses the block, the post-elaborate gate validates paths,
    // and an unplaced component surfaces as a non-blocking W_SCHEMATIC_UNPLACED.
    let lib = part_library();
    let text = "inst C1 Cap\ninst C2 Cap\nnet N1 C1.p1 C2.p1\nnet N2 C1.p2 C2.p2\nschematic {\n  row {\n    sym C1\n  }\n}\n";
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::LoadText(text.into())),
        &lib,
        "load",
    )
    .unwrap();
    let doc = h.doc();
    assert!(doc.schematic.is_some());
    // C2 is not placed -> reported, but the commit still succeeded (view is total).
    assert_eq!(doc.report.unplaced_components, vec![EntityId::new("C2")]);
    assert!(doc.report.is_clean()); // unplaced is a warning, not a dirtying finding.
}

#[test]
fn load_text_rejects_unknown_sym_path() {
    let lib = part_library();
    // `sym NOPE` names no instance -> E_SCHEMATIC aborts the transaction (atomic).
    let text = "inst C1 Cap\nschematic {\n  sym NOPE\n}\n";
    let mut h = History::new(Default::default());
    let err = h
        .commit(
            Transaction::one(Command::LoadText(text.into())),
            &lib,
            "load",
        )
        .unwrap_err();
    assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
}

#[test]
fn load_text_dnp_placed_symbol_degrades_not_aborts() {
    // End-to-end (Decision 20c × 21b): a `sym` placing a component that a false `if=`
    // depopulates must COMMIT (not hard-abort a variant toggle) — the symbol is absent
    // from reflow and the part surfaces as W_SCHEMATIC_UNPLACED.
    let lib = part_library();
    let text = "param populate = false\n\
                    inst C1 Cap\n\
                    inst C2 Cap if=populate\n\
                    net N1 C1.p1 C1.p2\n\
                    schematic {\n  row {\n    sym C1\n    sym C2\n  }\n}\n";
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::LoadText(text.into())),
        &lib,
        "load",
    )
    .expect("a DNP-dropped placed symbol must not abort the commit");
    let doc = h.doc();
    // C2 is depopulated -> not a real component, surfaced as unplaced, and warns.
    assert!(!doc.components.contains_key(&EntityId::new("C2")));
    assert_eq!(doc.report.unplaced_components, vec![EntityId::new("C2")]);
    assert!(doc.report.is_clean()); // W_SCHEMATIC_UNPLACED is a non-dirtying warning.
    // Reflow places only the populated C1; C2 is absent from the output entirely.
    let placed = doc.reflow_schematic(&lib);
    assert!(placed.contains_key(&EntityId::new("C1")));
    assert!(
        !placed.contains_key(&EntityId::new("C2")),
        "a depopulated part must not appear in reflow output"
    );
}

// ---- Decision-21a `def` construct ------------------------------------

/// Elaborate `source` against the toy library and return the diagnostics, panicking if
/// it unexpectedly succeeded. (`Elaborated` isn't `Debug`, so `expect_err` can't be
/// used directly.)
fn elab_err(source: &Source) -> Vec<Diagnostic> {
    let lib = part_library();
    match elaborate(source, &Default::default(), &Default::default(), &lib) {
        Ok(_) => panic!("expected elaboration to fail"),
        Err(e) => e,
    }
}

/// Return the elaborated net whose name is `name`, panicking if absent.
fn net_named<'a>(doc: &'a Doc, name: &str) -> &'a crate::doc::Net {
    doc.nets
        .values()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("net `{name}` not found in {:?}", doc.nets.keys()))
}

/// A `def` stamps its body per instantiation with path prefixing: `sense[0].R1`-style
/// component paths and path-prefixed internal nets, so two instances never collide.
#[test]
fn def_stamps_body_with_path_prefix() {
    let src = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  net fb R1.p2 C1.p1\n}\n\
                   inst a rc\ninst b rc";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    for p in ["a.R1", "a.C1", "b.R1", "b.C1"] {
        assert!(
            doc.components.contains_key(&EntityId::new(p)),
            "stamped component `{p}` missing"
        );
    }
    // Internal net `fb` is path-prefixed per instance — distinct nets, no collision.
    let a_fb = net_named(&doc, "a.fb");
    let b_fb = net_named(&doc, "b.fb");
    assert!(a_fb.members.iter().any(|m| m.comp.as_str() == "a.R1"));
    assert!(b_fb.members.iter().any(|m| m.comp.as_str() == "b.R1"));
    assert!(!a_fb.members.iter().any(|m| m.comp.as_str() == "b.R1"));
}

/// A connection to a def instance's port resolves through to the bound internal pin's
/// pad identity (no new namespace) — an outer `net VOUT amp.out` lands on `amp.R1`'s
/// pad, not a phantom port pin.
#[test]
fn def_port_resolves_to_bound_internal_pin() {
    let src = "def divider {\n  inst R1 Cap\n  inst R2 Cap\n  net mid R1.p2 R2.p1\n  \
                   port out = R1.p2\n}\n\
                   inst d divider\nnet VOUT d.out";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    let vout = net_named(&doc, "VOUT");
    // The outer net reaches the internal R1 pad p2 — the port's binding.
    assert!(
        vout.members
            .iter()
            .any(|m| m.comp.as_str() == "d.R1" && m.pin.as_str() == "p2"),
        "VOUT should reach d.R1.p2 via the port, got {:?}",
        vout.members
    );
}

/// Def params: a default is used when the instantiation omits it; a `p:` override
/// replaces it; the value flows into body expressions (evaluated in the def scope).
#[test]
fn def_params_default_and_override() {
    let src = "def rc param val=100n {\n  inst C1 Cap p:value=(val)\n}\n\
                   inst a rc\ninst b rc p:val=220n";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert_eq!(
        doc.components[&EntityId::new("a.C1")].params["value"],
        "100n",
        "default param flows into a body expression"
    );
    assert_eq!(
        doc.components[&EntityId::new("b.C1")].params["value"],
        "220n",
        "p: override replaces the default"
    );
}

/// A def param shadows an outer doc param of the same name (innermost wins — the same
/// rule as the range loop variable `i`). The body reads the def param's value.
#[test]
fn def_param_shadows_outer_doc_param() {
    let src = "param val = 1n\n\
                   def rc param val=999n {\n  inst C1 Cap p:value=(val)\n}\n\
                   inst a rc";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert_eq!(
        doc.components[&EntityId::new("a.C1")].params["value"],
        "999n",
        "the def param shadows the outer doc param"
    );
}

/// An outer doc param is visible inside a def body when not shadowed.
#[test]
fn def_body_sees_outer_param() {
    let src = "param gain = 5\n\
                   def amp {\n  inst C1 Cap p:g=(gain)\n}\n\
                   inst a amp";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert_eq!(doc.components[&EntityId::new("a.C1")].params["g"], "5");
}

/// Def instantiation composes with a range: `inst sense[0..n] SenseDef` stamps the
/// body under each `sense[i]` prefix, and the loop variable is usable in `p:`.
#[test]
fn def_instantiation_with_range() {
    let src = "param n = 2\n\
                   def sensor {\n  inst U Cap\n}\n\
                   inst sense[0..n] sensor";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    assert!(doc.components.contains_key(&EntityId::new("sense[0].U")));
    assert!(doc.components.contains_key(&EntityId::new("sense[1].U")));
    assert!(!doc.components.contains_key(&EntityId::new("sense[2].U")));
}

/// Nested def instantiation composes paths, and a re-exported port (a def's port bound
/// to a nested def's port) resolves transitively to the deepest real pin.
#[test]
fn nested_def_composes_and_reexports_port() {
    let src = "def leaf {\n  inst R Cap\n  port o = R.p2\n}\n\
                   def mid {\n  inst inner leaf\n  port o = inner.o\n}\n\
                   inst top mid\nnet OUT top.o";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    // Path composition: top → mid.inner → leaf.R
    assert!(
        doc.components.contains_key(&EntityId::new("top.inner.R")),
        "nested path did not compose: {:?}",
        doc.components.keys().collect::<Vec<_>>()
    );
    // Transitive port resolution: OUT reaches top.inner.R.p2.
    let out = net_named(&doc, "OUT");
    assert!(
        out.members
            .iter()
            .any(|m| m.comp.as_str() == "top.inner.R" && m.pin.as_str() == "p2"),
        "OUT should reach top.inner.R.p2, got {:?}",
        out.members
    );
}

/// A def reaching itself through any instantiation chain is an `E_DEF_CYCLE` error
/// naming the cycle — not an infinite loop.
#[test]
fn def_cycle_is_an_error() {
    let src = "def a {\n  inst x b\n}\n\
                   def b {\n  inst y a\n}\n\
                   inst top a";
    let Parsed { source, .. } = parse(src).expect("parse");
    let err = elab_err(&source);
    assert!(
        err.iter().any(|d| d.code == "E_DEF_CYCLE"),
        "expected E_DEF_CYCLE, got {:?}",
        err.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// A def whose name also names a library part is rejected at elaboration
/// (`E_DEF_PART_AMBIGUOUS`) rather than silently shadowing.
#[test]
fn def_name_colliding_with_part_is_ambiguous() {
    let src = "def Cap {\n  inst X Cap\n}\ninst a Cap";
    let Parsed { source, .. } = parse(src).expect("parse");
    let err = elab_err(&source);
    assert!(
        err.iter().any(|d| d.code == "E_DEF_PART_AMBIGUOUS"),
        "expected E_DEF_PART_AMBIGUOUS, got {:?}",
        err.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// `if=false` on a def instance drops the whole stamped subtree; an external net
/// referencing a dropped port dangles as `W_DNP`, never an unknown-instance error.
#[test]
fn def_instance_if_false_drops_subtree() {
    let src = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  port o = R1.p2\n}\n\
                   inst a rc if=(false)\nnet OUT a.o";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    // The whole subtree is gone.
    assert!(!doc.components.contains_key(&EntityId::new("a.R1")));
    assert!(!doc.components.contains_key(&EntityId::new("a.C1")));
    // The external reference to the dropped instance dangles as a warning.
    assert!(
        doc.report.dnp_dangling.iter().any(|(_, p)| p == "a"),
        "dangling connection to dropped def instance recorded: {:?}",
        doc.report.dnp_dangling
    );
}

/// Refdes stays board-global flat across hierarchical def paths (industry
/// convention): two `Cap` instances stamped from two def instances get R1/R2 (or the
/// class prefix), numbered over all instances regardless of their hierarchical path.
#[test]
fn def_refdes_stays_board_global_flat() {
    let src = "def rc {\n  inst K Cap\n}\n\
                   inst a rc\ninst b rc";
    let Parsed { source, .. } = parse(src).expect("parse");
    let doc = placed(source);
    let lib = part_library();
    let rd = crate::annotate::refdes(&doc, &lib, &crate::annotate::registry(&[]));
    let designators: BTreeSet<String> = [
        rd[&EntityId::new("a.K")].clone(),
        rd[&EntityId::new("b.K")].clone(),
    ]
    .into_iter()
    .collect();
    // Two distinct board-global designators over the two hierarchical instances.
    assert_eq!(
        designators.len(),
        2,
        "two stamped Caps must get two distinct board-global refdes, got {designators:?}"
    );
}

/// A `def` document round-trips byte-identically through parse → serialize → parse →
/// serialize (canonical fixpoint), preserving body directives, params, ports, and
/// interior trivia.
#[test]
fn def_serialize_parse_fixpoint() {
    let authored = "def rc param val=100n {\n  # the resistor\n  inst R1 Cap p:value=(val)\n\n  \
                        inst C1 Cap\n  port out = R1.p2\n}\ninst a rc p:val=220n\n";
    let once = serialize(&Doc {
        source: parse(authored).unwrap().source,
        ..Default::default()
    });
    let twice = serialize(&Doc {
        source: parse(&once).unwrap().source,
        ..Default::default()
    });
    assert_eq!(once, twice, "def serialization must reach a fixpoint");
    // The fixpoint form preserves the def structure.
    assert!(once.contains("def rc param val=100n {"));
    assert!(once.contains("  # the resistor"));
    assert!(once.contains("  inst R1 Cap p:value=(val)"));
    assert!(once.contains("  port out = R1.p2"));
}

/// The poc guard: a document with no `def` serializes byte-identically to before this
/// feature — the def machinery adds nothing to a blockless program's text.
#[test]
fn defless_doc_is_byte_identical() {
    let src = "inst U1 MCU\ninst c1 Cap\nnet GND U1.GND c1.p2\n";
    let doc = Doc {
        source: parse(src).unwrap().source,
        ..Default::default()
    };
    assert_eq!(
        serialize(&doc),
        src,
        "a def-free doc must be byte-identical"
    );
}

/// A `p:` override naming a param the def does not declare is a hard error (a typo,
/// never silently ignored).
#[test]
fn def_unknown_param_override_is_an_error() {
    let src = "def rc param val=1n {\n  inst C1 Cap\n}\ninst a rc p:nope=2n";
    let Parsed { source, .. } = parse(src).expect("parse");
    let err = elab_err(&source);
    assert!(
        err.iter().any(|d| d.code == "E_DEF"),
        "expected E_DEF, got {:?}",
        err.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// A nested def *definition* (a `def` inside a `def` body) is rejected — definitions
/// stay top-level in v1.
#[test]
fn nested_def_definition_is_rejected() {
    let src = "def outer {\n  def inner {\n    inst X Cap\n  }\n}\ninst a outer";
    let errs = parse(src).expect_err("a nested def definition must fail parsing");
    assert!(
        errs.iter().any(|d| d.code == "E_DEF"),
        "expected E_DEF, got {:?}",
        errs.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// A ref to a *leaf pin* of a def instance dropped by `if=false` degrades to `W_DNP`,
/// exactly like a ref to the instance itself — not a hard `E_UNKNOWN_INSTANCE` (the
/// prefix rule: a path beneath a dropped subtree is intentionally-absent). With
/// `if=true` the same connection resolves normally; a genuinely unknown deep path (no
/// such def instance ever) still hard-errors.
#[test]
fn deep_ref_into_dropped_def_degrades_to_warning() {
    let def = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n}\n";
    // if=false: deep pin ref into the never-stamped subtree degrades.
    let src = format!("{def}inst a rc if=(false)\nnet OUT a.R1.p2");
    let Parsed { source, .. } = parse(&src).expect("parse");
    let doc = placed(source);
    assert!(!doc.components.contains_key(&EntityId::new("a.R1")));
    assert!(
        doc.report.dnp_dangling.iter().any(|(_, p)| p == "a.R1"),
        "deep ref into dropped subtree should be W_DNP, got {:?}",
        doc.report.dnp_dangling
    );

    // if=true: the same deep ref connects normally.
    let src_on = format!("{def}inst a rc if=(true)\nnet OUT a.R1.p2");
    let Parsed { source: on, .. } = parse(&src_on).expect("parse on");
    let doc_on = placed(on);
    let out = net_named(&doc_on, "OUT");
    assert!(
        out.members
            .iter()
            .any(|m| m.comp.as_str() == "a.R1" && m.pin.as_str() == "p2"),
        "with if=true the deep pin connects, got {:?}",
        out.members
    );

    // A genuinely unknown deep path (no def instance `zzz` ever) still hard-errors.
    let bad = format!("{def}inst a rc\nnet OUT zzz.R1.p2");
    let Parsed { source: b, .. } = parse(&bad).expect("parse bad");
    let err = elab_err(&b);
    assert!(
        err.iter().any(|d| d.code == "E_UNKNOWN_INSTANCE"),
        "an unknown deep path must still hard-error, got {:?}",
        err.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// An authored top-level net whose name equals a stamped def-internal net is a hard
/// `E_DEF_NET_COLLISION` (not a silent merge), naming both sides. Tested in both
/// authoring orders (authored-before-def-inst and after), since a silent merge would
/// be order-independent too.
#[test]
fn authored_net_colliding_with_internal_net_is_an_error() {
    let def = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  net fb R1.p2 C1.p1\n}\n";
    // The stamped internal net is `a.fb`; author a top-level `net a.fb …` that collides.
    for order in [
        format!("{def}inst a rc\nnet a.fb R1.p1"),
        format!("{def}net a.fb R1.p1\ninst a rc"),
    ] {
        let Parsed { source, .. } = parse(&order).expect("parse");
        let err = elab_err(&source);
        assert!(
            err.iter().any(|d| d.code == "E_DEF_NET_COLLISION"),
            "expected E_DEF_NET_COLLISION for `{order}`, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }
}

/// The range loop variable `i` is NOT visible inside a def body (the body is a pure
/// function of its declared params). A body expression referencing `i` is an `E_EXPR`
/// unknown variable — the index must be passed explicitly via a `p:`.
#[test]
fn range_index_not_visible_inside_def_body() {
    let src = "param n = 2\n\
                   def s {\n  inst U Cap p:idx=(i)\n}\n\
                   inst sense[0..n] s";
    let Parsed { source, .. } = parse(src).expect("parse");
    let err = elab_err(&source);
    assert!(
        err.iter().any(|d| d.code == "E_EXPR"),
        "referencing `i` inside a def body must be E_EXPR, got {:?}",
        err.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    // The explicit-forward form works: pass the index as a param.
    let ok = "param n = 2\n\
                  def s param idx=0 {\n  inst U Cap p:tag=(idx)\n}\n\
                  inst sense[0..n] s p:idx=(i)";
    let Parsed { source: oks, .. } = parse(ok).expect("parse ok");
    let doc = placed(oks);
    assert_eq!(
        doc.components[&EntityId::new("sense[0].U")].params["tag"],
        "0"
    );
    assert_eq!(
        doc.components[&EntityId::new("sense[1].U")].params["tag"],
        "1"
    );
}

/// An override pinned to a stamped def-instance path survives a def param change and
/// orphans (surfaced, not dropped) when the instance disappears — reconciliation flows
/// through stamped paths exactly as for hand-written ones.
#[test]
fn def_override_survives_and_decays_by_stamped_path() {
    let lib = part_library();
    let mut h = History::new(Default::default());
    let base = "param n = 2\ndef s {\n  inst U Cap\n}\ninst sense[0..n] s";
    let Parsed { source: s2, .. } = parse(base).expect("parse");
    h.commit(Transaction::one(Command::SetSource(s2)), &lib, "n2")
        .unwrap();
    // Pin a stamped path.
    h.commit(
        Transaction::one(Command::Pin(EntityId::new("sense[1].U"), Point::mm(5, 5))),
        &lib,
        "pin",
    )
    .unwrap();
    assert_eq!(
        h.doc().components[&EntityId::new("sense[1].U")].pos.value,
        Point::mm(5, 5),
        "pin holds the stamped path"
    );
    // Shrink the range: sense[1] disappears; the pin orphans, is not silently dropped.
    let Parsed { source: s1, .. } =
        parse("param n = 1\ndef s {\n  inst U Cap\n}\ninst sense[0..n] s").expect("parse1");
    h.commit(Transaction::one(Command::SetSource(s1)), &lib, "n1")
        .unwrap();
    assert!(
        !h.doc()
            .components
            .contains_key(&EntityId::new("sense[1].U"))
    );
    assert!(
        h.doc()
            .report
            .orphaned
            .contains(&EntityId::new("sense[1].U")),
        "the removed stamped instance's override is surfaced as an orphan"
    );
}
