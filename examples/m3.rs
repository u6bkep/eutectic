//! M3 demo: the least-change placement solver, themed as a tiny stand-in for the
//! eventual proof-of-concept (an RP2350-Zero carrier breaking GPIOs to JST-SH
//! headers as a multi-SWD probe). Run with `cargo run --example m3`.
//!
//! The board: the module is fixed at a mechanical datum; decouplers cluster near
//! it (Near + MinSep); the JST-SH headers sit in an aligned row along the top
//! edge (one header datum-fixed, the rest AlignY'd to it); everything is kept
//! inside the outline. Then we move the datum and watch the perturbation stay
//! local — the headers don't budge.

use ecad_core::command::{Command, Transaction};
use ecad_core::doc::{Doc, Point, Provenance, MM};
use ecad_core::elaborate::{board_rect, GenDirective as G, Source};
use ecad_core::history::History;
use ecad_core::part::part_library;

fn scene(mcu_datum: Point) -> Source {
    let mut s = vec![
        board_rect(Point::mm(0, 0), Point::mm(60, 40)),
        // The module, fixed at a mechanical datum.
        G::Instance { path: "mcu".into(), part: "MCU".into() },
        G::Fix { path: "mcu".into(), pos: mcu_datum },
    ];
    // Decouplers cluster near the module, spaced apart.
    for i in 0..3 {
        let d = format!("dec{i}");
        s.push(G::Instance { path: d.clone(), part: "Cap".into() });
        s.push(G::Near { a: d.clone(), b: "mcu".into(), within: 6 * MM });
    }
    s.push(G::MinSep { a: "dec0".into(), b: "dec1".into(), gap: 3 * MM });
    s.push(G::MinSep { a: "dec1".into(), b: "dec2".into(), gap: 3 * MM });
    s.push(G::MinSep { a: "dec0".into(), b: "dec2".into(), gap: 3 * MM });
    // JST-SH headers in a row along the top edge: one fixed datum, rest aligned.
    s.push(G::Instance { path: "h0".into(), part: "Cap".into() });
    s.push(G::Fix { path: "h0".into(), pos: Point::mm(10, 37) });
    let mut row = vec!["h0".to_string()];
    for (i, x) in [25, 40, 50].iter().enumerate() {
        let h = format!("h{}", i + 1);
        s.push(G::Instance { path: h.clone(), part: "Cap".into() });
        s.push(G::Place { path: h.clone(), pos: Point::mm(*x, 5) });
        row.push(h);
    }
    s.push(G::AlignY { nodes: row });
    s
}

fn show(d: &Doc) {
    for c in d.components.values() {
        let p = c.pos.value;
        let prov = match c.pos.prov {
            Provenance::Free => "free",
            Provenance::Hint => "hint",
            Provenance::Pinned => "pinned",
            Provenance::Fixed => "fixed",
        };
        println!(
            "  {:<5} {:<3} ({:>5.1},{:>5.1}) mm [{}]",
            c.id.as_str(),
            c.part,
            p.x as f64 / MM as f64,
            p.y as f64 / MM as f64,
            prov
        );
    }
}

fn main() {
    let lib = part_library();
    let mut h = History::new(Default::default());

    println!("Solved board (module datum at 30,20mm):");
    h.commit(Transaction::one(Command::SetSource(scene(Point::mm(30, 20)))), &lib, "v1").unwrap();
    show(h.doc());
    let headers_before: Vec<Point> =
        ["h0", "h1", "h2", "h3"].iter().map(|n| h.doc().components[&id(n)].pos.value).collect();

    println!("\nMove the module datum to (45,20mm) — decouplers follow, headers do not:");
    h.commit(Transaction::one(Command::SetSource(scene(Point::mm(45, 20)))), &lib, "v2").unwrap();
    show(h.doc());

    let headers_after: Vec<Point> =
        ["h0", "h1", "h2", "h3"].iter().map(|n| h.doc().components[&id(n)].pos.value).collect();
    let headers_unchanged = headers_before == headers_after;
    println!(
        "\n  headers unchanged by the datum move? {}  (least-change: perturbation stayed local)",
        headers_unchanged
    );
}

fn id(s: &str) -> ecad_core::id::EntityId {
    ecad_core::id::EntityId::new(s)
}
