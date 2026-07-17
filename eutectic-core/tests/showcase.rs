use eutectic_core::command::{Command, Transaction};
use eutectic_core::doc::Doc;
use eutectic_core::geom::{Role, Seg};
use eutectic_core::history::History;
use eutectic_core::id::NetId;
use eutectic_core::ir::GenDirective;
use eutectic_core::part::part_library;
use eutectic_core::query::{Engine, Key};
use eutectic_core::route::{DesignRules, Violation, pours, world_features};
use std::collections::BTreeMap;

const SHOWCASE: &str = include_str!("../../examples/showcase.eut");

fn load_showcase() -> Doc {
    let lib = part_library();
    let mut history = History::new(Doc::default());
    history
        .commit(
            Transaction::one(Command::LoadText(SHOWCASE.to_string())),
            &lib,
            "load showcase",
        )
        .expect("showcase parses and elaborates");
    history.doc().clone()
}

#[test]
fn showcase_inventory_and_deliberate_findings_are_exact() {
    let doc = load_showcase();
    let lib = part_library();
    let mut engine = Engine::new();
    let netlist = engine.query(&doc, &lib, Key::Netlist);
    let netlist = netlist.as_netlist();
    let stackup = eutectic_core::elaborate::stackup(&doc.source);
    let pours = pours(&doc, &lib, netlist, &DesignRules::default(), &stackup);
    let world = world_features(&doc, &lib, netlist, &DesignRules::default(), &stackup)
        .expect("showcase world geometry materializes");
    let silk_count = world
        .iter()
        .filter(|feature| feature.feature.role == Role::Marking)
        .count();
    let authored_silk_count = doc
        .source
        .iter()
        .filter(|directive| matches!(directive, GenDirective::Text { .. }))
        .count();
    let trace_layers = doc
        .traces
        .values()
        .fold(BTreeMap::new(), |mut counts, trace| {
            *counts.entry(trace.layer.as_str()).or_insert(0) += 1;
            counts
        });
    let mask_slabs = doc
        .source
        .iter()
        .filter(
            |directive| matches!(directive, GenDirective::Slab(slab) if slab.role == Role::Mask),
        )
        .count();
    let outline_arcs = doc
        .source
        .iter()
        .find_map(|directive| match directive {
            GenDirective::Board { outline } => Some(
                outline
                    .path()
                    .segs
                    .iter()
                    .filter(|seg| matches!(seg, Seg::Arc { .. }))
                    .count(),
            ),
            _ => None,
        })
        .expect("showcase has a board outline");
    let findings = engine.query(&doc, &lib, Key::Drc).as_drc().to_vec();

    assert_eq!(doc.components.len(), 7, "elaborated toy-library parts");
    assert_eq!(doc.traces.len(), 11, "persisted routed traces");
    assert_eq!(doc.vias.len(), 2, "persisted through-vias");
    assert!(
        doc.vias.values().all(|via| via.span.is_none()),
        "both vias are through-vias"
    );
    assert_eq!(trace_layers, BTreeMap::from([("B.Cu", 1), ("F.Cu", 10)]));
    assert_eq!(pours.len(), 2, "derived copper pours");
    assert_eq!(
        pours
            .iter()
            .map(|pour| (pour.net.clone(), pour.layer.clone()))
            .collect::<Vec<_>>(),
        vec![
            (NetId::new("GND"), "F.Cu".to_string()),
            (NetId::new("VCC"), "B.Cu".to_string()),
        ]
    );
    assert_eq!(authored_silk_count, 4, "authored silk text/graphics");
    assert_eq!(silk_count, 62, "realized silk glyph-stroke features");
    assert_eq!(mask_slabs, 2, "front and back solder-mask slabs");
    assert_eq!(
        outline_arcs, 4,
        "one rounded treatment at every board corner"
    );
    assert_eq!(
        doc.def_fragments.len(),
        4,
        "four instances reuse one def layout"
    );
    assert_eq!(
        doc.schematic
            .as_ref()
            .expect("authored schematic")
            .wires()
            .len(),
        3
    );
    assert!(doc.nets.contains_key(&NetId::new("U2.uart.tx")));
    assert!(doc.nets.contains_key(&NetId::new("U2.uart.rx")));
    assert_eq!(
        findings,
        vec![
            Violation::Clearance {
                a: NetId::new("VCC"),
                b: NetId::new("VIN"),
                layer: "F.Cu".into(),
            },
            Violation::Unrouted {
                net: NetId::new("U2.uart.rx"),
                islands: 2,
            },
        ],
        "only the ambassador board's deliberate clearance and ratsnest findings"
    );
    assert!(
        engine.query(&doc, &lib, Key::Erc).as_erc().is_empty(),
        "typed connectivity has no electrical-rule findings"
    );
    assert!(
        engine
            .query(&doc, &lib, Key::Floating)
            .as_floating()
            .is_empty(),
        "every physical pad is connected or explicitly accounted for"
    );
    assert!(doc.report.unplaced_components.is_empty());
    assert!(doc.report.schematic_wire_warnings.is_empty());
    assert!(doc.report.route_id_warnings.is_empty());
    assert!(doc.report.unmasked_copper.is_empty());
}

#[test]
fn showcase_text_is_a_canonical_round_trip_fixed_point() {
    let doc = load_showcase();
    let once = eutectic_core::text::serialize(&doc);
    assert_eq!(once, SHOWCASE, "the committed showcase is canonical text");

    let parsed = eutectic_core::text::parse(&once).expect("canonical text re-parses");
    assert!(
        parsed.warnings.is_empty(),
        "canonical route ids need no repair"
    );
    let reparsed = Doc {
        source: parsed.source,
        overrides: parsed.overrides,
        refdes_pins: parsed.refdes_pins,
        traces: parsed.traces,
        vias: parsed.vias,
        schematic: parsed.schematic,
        ..Doc::default()
    };
    assert_eq!(
        eutectic_core::text::serialize(&reparsed),
        once,
        "parse -> serialize -> parse -> serialize is stable"
    );
}
