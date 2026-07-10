//! Schematic-render demo (Decision 20): author a few-part document with a layout tree and
//! a couple of drawn wires, elaborate it, and write the derived schematic SVG so a human
//! can eyeball it. Run with `cargo run --example schematic` — it writes `schematic.svg` to
//! the current directory and prints the path.
//!
//! The document is a small power-supply-ish fragment: a regulator, an MCU, and two
//! decoupling caps. The `schematic { … }` block lays the parts out (a power column beside
//! the MCU, the caps in a row) and draws two presentational wires (§20d) — one straight,
//! one routed through a waypoint. The wires are pure drawing: the *connections* are the
//! `net` directives, and every connected pin renders its net name as a tag (§20c).

use eutectic_core::command::{Command, Transaction};
use eutectic_core::history::History;
use eutectic_core::part::part_library;
use eutectic_core::schematic_svg::schematic_svg;

fn main() {
    let lib = part_library();

    // The whole document as authored text: instances, nets (the connection truth), and the
    // schematic layout tree with two drawn wires. `U1` (MCU) is deliberately left out of
    // the layout so it lands in the derived "unplaced" bin — the view stays total (§20c).
    let src = "\
inst reg LDO
inst U1 MCU
inst C1 Cap
inst C2 Cap

net VBUS reg.VOUT C1.p1 C2.p1
net GND  reg.GND  C1.p2 C2.p2 U1.GND

schematic {
  row gap=10mm align=center {
    column gap=5mm {
      sym reg
    }
    row gap=5mm {
      sym C1
      sym C2
    }
  }

  # a straight presentational wire, and one routed through a waypoint (§20d)
  wire reg.VOUT C1.p1
  wire C1.p2 C2.p2 via (0mm, -12mm)
}
";

    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::LoadText(src.into())),
        &lib,
        "demo",
    )
    .expect("the document elaborates cleanly");

    // Surface any non-blocking findings (e.g. an unplaced component warning for U1).
    use eutectic_core::diagnostic::Diagnose;
    for d in h.doc().report.diagnostics() {
        println!("[{}] {}", d.code, d.message);
    }

    let svg = schematic_svg(h.doc(), &lib);
    let path = "schematic.svg";
    std::fs::write(path, &svg).expect("write the SVG");
    println!(
        "\nwrote {} ({} bytes) — open it in a browser to eyeball it",
        path,
        svg.len()
    );
}
