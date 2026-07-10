//! Autoroute demo: place a small board, show DRC violations BEFORE (the nets are
//! unrouted), run the grid autorouter, apply its proposed transaction, and show DRC
//! AFTER (clean). A visible end-to-end pass through the real command + query layers.
//! Run with `cargo run --example autoroute`.

use eutectic_core::autoroute::autoroute;
use eutectic_core::command::{Command, Transaction};
use eutectic_core::doc::Point;
use eutectic_core::elaborate::{GenDirective as G, board_rect};
use eutectic_core::history::History;
use eutectic_core::part::part_library;
use eutectic_core::query::{Engine, Key};
use eutectic_core::route::DesignRules;

fn main() {
    let lib = part_library();
    let rules = DesignRules::default();

    // A regulator with two decouplers on a 24x20 mm board: VBUS (reg.VOUT + both
    // caps' p1) and GND (reg.GND + both caps' p2). A Pinned hand route walls the
    // direct VBUS path on Top, so the autorouter must detour (and use a via).
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

    let mut eng = Engine::new();
    println!("==== DRC before routing ====");
    print_drc(eng.query(h.doc(), &lib, Key::Drc).as_drc());

    let result = autoroute(h.doc(), &lib, &rules);
    let traces = result
        .commands
        .iter()
        .filter(|c| matches!(c, Command::AddTrace(..)))
        .count();
    let vias = result
        .commands
        .iter()
        .filter(|c| matches!(c, Command::AddVia(..)))
        .count();
    println!(
        "\n==== autoroute proposed {} commands ({traces} traces, {vias} vias) ====",
        result.commands.len()
    );
    println!("routed:   {:?}", result.routed);
    println!("unrouted: {:?}", result.unrouted);

    // Apply the proposed transaction through the ordinary atomic command path.
    h.commit(Transaction(result.commands), &lib, "autoroute")
        .unwrap();

    println!("\n==== DRC after routing ====");
    print_drc(eng.query(h.doc(), &lib, Key::Drc).as_drc());
}

fn print_drc(v: &[eutectic_core::route::Violation]) {
    if v.is_empty() {
        println!("  (clean — no violations)");
    } else {
        for x in v {
            println!("  {x:?}");
        }
    }
}
