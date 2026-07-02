//! Fab-output demo: place a small board, autoroute it, then dump the deterministic
//! fab fileset — a Gerber per copper layer, the Edge.Cuts outline, the Excellon
//! drill program — plus the SVG sketch (now with traces + vias). Run with
//! `cargo run --example gerber`.
//!
//! Every artifact is a pure function of the routed document, so this output is
//! byte-stable across runs.

use ecad_core::autoroute::autoroute;
use ecad_core::command::{Command, Transaction};
use ecad_core::doc::Point;
use ecad_core::elaborate::{GenDirective as G, board_rect};
use ecad_core::export::{gerber_set, svg};
use ecad_core::history::History;
use ecad_core::part::part_library;
use ecad_core::route::DesignRules;

fn main() {
    let lib = part_library();
    let rules = DesignRules::default();

    // A regulator + two decouplers on a 24x20 mm board (the autoroute demo board).
    let src = vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
        },
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
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

    // Autoroute and apply through the ordinary atomic command path.
    let result = autoroute(h.doc(), &lib, &rules);
    println!(
        "routed {:?}, unrouted {:?} ({} commands)",
        result.routed,
        result.unrouted,
        result.commands.len()
    );
    h.commit(Transaction(result.commands), &lib, "autoroute")
        .unwrap();
    let doc = h.doc();

    // Dump the whole fab fileset (filename + content), then the SVG.
    for (name, content) in gerber_set(doc, &lib).unwrap() {
        println!("\n==== {name} ====");
        print!("{content}");
    }

    println!("\n==== board.svg ====");
    print!("{}", svg(doc, &lib).unwrap());
}
