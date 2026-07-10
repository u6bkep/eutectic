//! SVG board-outline import demo: take an inline SVG whose `<path>` carries an outer
//! board boundary plus an interior cutout, run [`svg_import::import_board_outline`], and
//! feed the resulting `(outline, cutouts)` into a [`Source`] as `Board`/`Cutout`
//! directives. A couple of library parts are placed on the board, the document is
//! elaborated through [`History`], and the derived board SVG is written out so a human
//! can eyeball it. Run with `cargo run --example svg_outline`.
//!
//! This is the first (and only) consumer wiring [`svg_import`] into a runnable path — an
//! alternative front end to the KiCad `Edge.Cuts` importer for authoring a board shape
//! from vector art. The import is asserted (cutout count) so the example fails loudly if
//! the SVG path parser ever regresses. Deterministic: the SVG string and placements are
//! fixed, so the output is byte-stable across runs.

use eutectic_core::command::{Command, Transaction};
use eutectic_core::doc::{MM, Point};
use eutectic_core::elaborate::GenDirective as G;
use eutectic_core::export::svg;
use eutectic_core::history::History;
use eutectic_core::part::part_library;
use eutectic_core::svg_import;
use std::collections::BTreeMap;

/// A 40 × 30 mm board with a 10 × 10 mm interior cutout (a window / connector clearance).
/// SVG y is down; the importer flips it to model y-up. Two subpaths in one `d`: the outer
/// boundary and the inner hole (the smaller-area loop classifies as the cutout).
const BOARD_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg">
  <path d="M 0 0 L 40 0 L 40 30 L 0 30 Z
           M 15 10 L 25 10 L 25 20 L 15 20 Z"/>
</svg>"#;

fn main() {
    let (outline, cutouts) =
        svg_import::import_board_outline(BOARD_SVG).expect("import the SVG board outline + cutout");

    // The example's whole point is exercising the import path — fail loudly if the SVG
    // parser stops recognising the interior subpath as a cutout.
    assert_eq!(
        cutouts.len(),
        1,
        "expected exactly one interior cutout, got {}",
        cutouts.len()
    );

    // Build a Source: the imported board geometry as Board/Cutout directives, then a
    // regulator + decoupling cap placed on it (Instance declares them; Fix/Place position
    // them well clear of the central cutout).
    let mut src = vec![G::Board { outline }];
    src.extend(cutouts.into_iter().map(|shape| G::Cutout { shape }));
    src.push(G::Instance {
        path: "reg".into(),
        part: "LDO".into(),
        params: BTreeMap::new(),
        label: None,
    });
    src.push(G::Instance {
        path: "dec".into(),
        part: "Cap".into(),
        params: BTreeMap::new(),
        label: None,
    });
    src.push(G::Fix {
        path: "reg".into(),
        pos: Point::mm(8, 15),
    });
    src.push(G::Near {
        a: "dec".into(),
        b: "reg".into(),
        within: 5 * MM,
    });

    let lib = part_library();
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(src)),
        &lib,
        "svg-outline demo",
    )
    .expect("commit the imported board + placements");
    let doc = h.doc();

    let rendered = svg(doc, &lib).expect("render the board SVG");
    let path = "target/svg_outline.svg";
    std::fs::write(path, &rendered).expect("write the SVG");

    println!(
        "imported SVG board (40x30 mm, 1 cutout), placed 2 parts, wrote {} ({} bytes)",
        path,
        rendered.len()
    );
}
