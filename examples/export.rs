//! Export demo: elaborate a tiny board, then print the three deterministic
//! output artifacts — netlist, pick-and-place CSV, and an SVG sketch.
//! Run with `cargo run --example export`.
//!
//! The board is a small power-supply module (a regulator + decouplers) placed on
//! an explicit board outline, so the SVG draws a real outline rather than the
//! bounding-box fallback. Every artifact is a pure function of the elaborated
//! document, so this output is byte-stable across runs.

use ecad_core::command::{Command, Transaction};
use ecad_core::doc::{Point, MM};
use ecad_core::elaborate::{psu_module, GenDirective as G};
use ecad_core::export::{netlist, placement_csv, svg};
use ecad_core::history::History;
use ecad_core::part::part_library;

fn main() {
    let lib = part_library();
    let mut h = History::new(Default::default());

    // psu_module(2) on a 60x40 mm board outline; cluster the decouplers near the
    // regulator so the placement is not just a default row.
    let mut src = vec![G::Board { min: Point::mm(0, 0), max: Point::mm(60, 40) }];
    src.extend(psu_module(2));
    src.push(G::Fix { path: "psu.reg".into(), pos: Point::mm(30, 20) });
    src.push(G::Near { a: "psu.dec[0]".into(), b: "psu.reg".into(), within: 6 * MM });
    src.push(G::Near { a: "psu.dec[1]".into(), b: "psu.reg".into(), within: 6 * MM });
    src.push(G::MinSep { a: "psu.dec[0]".into(), b: "psu.dec[1]".into(), gap: 3 * MM });
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "demo").unwrap();
    let doc = h.doc();

    println!("==== netlist ====");
    print!("{}", netlist(doc));

    println!("\n==== pick-and-place (CSV) ====");
    print!("{}", placement_csv(doc));

    println!("\n==== SVG sketch ====");
    print!("{}", svg(doc, &lib));
}
