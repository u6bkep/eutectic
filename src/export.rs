//! Deterministic output artifacts: netlist, pick-and-place, and an SVG sketch.
//!
//! Each exporter is a *pure function* of its inputs (a `Doc`, plus the `PartLib`
//! for geometry) — no wall-clock, no randomness, no iteration over `HashMap`. The
//! model is built on `BTreeMap`/`BTreeSet` precisely so this output is byte-stable
//! and diffable: calling an exporter twice on the same inputs yields identical
//! strings, and a one-thing change produces a one-line diff.
//!
//! Scope is deliberately limited to what the model carries *today*: placement
//! (component positions + cardinal orientation) and connectivity (the net
//! hypergraph). Real **Gerber/drill output is deferred**: those describe copper
//! geometry — trace polygons, pad stacks, drill hits — and there is **no router
//! yet**, so the model has no copper traces to emit. Gerber becomes meaningful
//! once a routing layer writes trace geometry into the document (see
//! docs/architecture.md, "Prototype status (export)").

use crate::doc::{Doc, Nm, Point, MM};
use crate::elaborate::GenDirective;
use crate::part::{pin_world, PartDef, PartLib};

/// Format a fixed-point nanometre coordinate as a millimetre decimal string with
/// exactly six fractional digits. Pure integer arithmetic — no float, so the
/// fixed-point determinism invariant is preserved end to end (e.g. `-2_000_000` ->
/// `"-2.000000"`, `1_325_000` -> `"1.325000"`).
fn fmt_mm(nm: Nm) -> String {
    let neg = nm < 0;
    let a = nm.unsigned_abs();
    let int = a / MM as u64;
    let frac = a % MM as u64;
    let body = format!("{int}.{frac:06}");
    if neg && a != 0 {
        format!("-{body}")
    } else {
        body
    }
}

// ---- 1. Netlist (connectivity artifact) ----

/// The connectivity artifact: every net and the pins it joins, in canonical form.
///
/// One net per line, `name: comp.pin comp.pin ...`. Nets iterate in `NetId` order
/// and pins in `PinRef` order (both `BTree*`), so the output is fully deterministic
/// and is the thing you check a fabricated/assembled board against.
pub fn netlist(doc: &Doc) -> String {
    let mut out = String::new();
    out.push_str("# netlist\n");
    for net in doc.nets.values() {
        let pins: Vec<String> =
            net.members.iter().map(|p| format!("{}.{}", p.comp, p.pin)).collect();
        out.push_str(&format!("{}: {}\n", net.name, pins.join(" ")));
    }
    out
}

// ---- 2. Pick-and-place (placement artifact) ----

/// A pick-and-place CSV: one row per component, `ref,part,x_mm,y_mm,rotation_deg`.
///
/// Rows iterate in `EntityId` order. Coordinates use [`fmt_mm`] (six-decimal mm);
/// rotation is the component's cardinal orientation in degrees. Refs and part
/// names are hierarchical paths / library keys that contain no commas in the
/// current model, so they are emitted unquoted (a quoting/escaping pass is future
/// work if names ever gain commas).
pub fn placement_csv(doc: &Doc) -> String {
    let mut out = String::new();
    out.push_str("ref,part,x_mm,y_mm,rotation_deg\n");
    for c in doc.components.values() {
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            c.id,
            c.part,
            fmt_mm(c.pos.value.x),
            fmt_mm(c.pos.value.y),
            c.orient.to_deg(),
        ));
    }
    out
}

// ---- 3. SVG sketch (visual sanity-check artifact) ----

/// Enumerate the pin reference names of a part, deterministically: discrete pins
/// in declaration order, then `port.signal` for each interface signal (both
/// `BTreeMap`-ordered). These are exactly the names [`pin_world`] resolves.
fn part_pin_names(def: &PartDef) -> Vec<String> {
    let mut names: Vec<String> = def.pins.iter().map(|p| p.name.clone()).collect();
    for (port, iface) in &def.interfaces {
        for sig in iface.signals.keys() {
            names.push(format!("{port}.{sig}"));
        }
    }
    names
}

/// Minimal XML text escaping for labels.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A board sketch as deterministic SVG: the board outline (the source `Board`
/// directive if present, else the bounding box of placed geometry), each component
/// drawn at its position with its pin pads (via [`pin_world`]) and an id label.
///
/// The model's y axis points up (ECAD convention); SVG's points down, so y is
/// flipped within the content bounds to keep the sketch upright. All coordinates
/// are six-decimal mm via [`fmt_mm`]; element order follows `EntityId` order. No
/// timestamps or other ambient state — byte-stable and diffable.
pub fn svg(doc: &Doc, lib: &PartLib) -> String {
    const MARGIN: Nm = 2 * MM;

    // The board outline carried by tier-1 source, if any (last `Board` wins, as in
    // elaboration). There are no copper layers in the model, so this outline plus
    // the placed pads is the entire drawable geometry.
    let board = doc.source.iter().rev().find_map(|d| match d {
        GenDirective::Board { min, max } => Some((*min, *max)),
        _ => None,
    });

    // Gather every point that must be in view: component origins, their pin pads,
    // and the board corners.
    let mut pts: Vec<Point> = Vec::new();
    for c in doc.components.values() {
        pts.push(c.pos.value);
        if let Some(def) = lib.get(&c.part) {
            for name in part_pin_names(def) {
                if let Some(w) = pin_world(c, def, &name) {
                    pts.push(w);
                }
            }
        }
    }
    if let Some((min, max)) = board {
        pts.push(min);
        pts.push(max);
    }

    // Content bounds (+ margin). Fall back to a 10mm box for an empty document so
    // the viewBox is never degenerate.
    let (mut x0, mut y0, mut x1, mut y1) = match pts.first() {
        Some(p) => (p.x, p.y, p.x, p.y),
        None => (0, 0, 10 * MM, 10 * MM),
    };
    for p in &pts {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    x0 -= MARGIN;
    y0 -= MARGIN;
    x1 += MARGIN;
    y1 += MARGIN;

    // Flip y into the SVG (downward) frame, staying inside the same bounds so the
    // sketch reads upright.
    let flip = |y: Nm| -> Nm { y0 + y1 - y };

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\">\n",
        fmt_mm(x0),
        fmt_mm(y0),
        fmt_mm(x1 - x0),
        fmt_mm(y1 - y0),
    ));

    // Board outline (or the implicit bounding box when the source carries none).
    let (bx0, by0, bx1, by1) = match board {
        Some((min, max)) => (min.x, min.y, max.x, max.y),
        None => (x0 + MARGIN, y0 + MARGIN, x1 - MARGIN, y1 - MARGIN),
    };
    let outline_kind = if board.is_some() { "board" } else { "bbox" };
    // Rect origin is the top-left in SVG space: min x, flipped max y.
    out.push_str(&format!(
        "  <rect class=\"outline-{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
        outline_kind,
        fmt_mm(bx0),
        fmt_mm(flip(by1)),
        fmt_mm(bx1 - bx0),
        fmt_mm(by1 - by0),
    ));

    // One group per component: pads, an origin marker, and an id label.
    for c in doc.components.values() {
        out.push_str(&format!("  <g class=\"component\" data-id=\"{}\">\n", xml_escape(c.id.as_str())));
        if let Some(def) = lib.get(&c.part) {
            for name in part_pin_names(def) {
                if let Some(w) = pin_world(c, def, &name) {
                    out.push_str(&format!(
                        "    <circle class=\"pad\" cx=\"{}\" cy=\"{}\" r=\"0.3\"/>\n",
                        fmt_mm(w.x),
                        fmt_mm(flip(w.y)),
                    ));
                }
            }
        }
        let o = c.pos.value;
        out.push_str(&format!(
            "    <circle class=\"origin\" cx=\"{}\" cy=\"{}\" r=\"0.5\" fill=\"red\"/>\n",
            fmt_mm(o.x),
            fmt_mm(flip(o.y)),
        ));
        out.push_str(&format!(
            "    <text x=\"{}\" y=\"{}\" font-size=\"1.5\">{}</text>\n",
            fmt_mm(o.x),
            fmt_mm(flip(o.y)),
            xml_escape(c.id.as_str()),
        ));
        out.push_str("  </g>\n");
    }

    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::doc::Doc;
    use crate::elaborate::psu_module;
    use crate::history::History;
    use crate::part::part_library;

    fn doc_psu(n: usize) -> (Doc, PartLib) {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(n))), &lib, "psu").unwrap();
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
ref,part,x_mm,y_mm,rotation_deg
psu.dec[0],Cap,10.000000,0.000000,0
psu.dec[1],Cap,20.000000,0.000000,0
psu.reg,LDO,0.000000,0.000000,0
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
                G::Instance { path: "u1".into(), part: "MCU".into() },
                G::Rotate { path: "u1".into(), deg: 90 },
            ])),
            &lib,
            "rot",
        )
        .unwrap();
        let csv = placement_csv(h.doc());
        assert!(csv.contains("u1,MCU,0.000000,0.000000,90\n"), "got:\n{csv}");
    }

    #[test]
    fn svg_contains_outline_and_component_ids() {
        // A scene with an explicit board outline.
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut h = History::new(Default::default());
        let mut src = psu_module(2);
        src.insert(0, G::Board { min: Point::mm(0, 0), max: Point::mm(60, 40) });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "board").unwrap();
        let s = svg(h.doc(), &lib);

        assert!(s.starts_with("<?xml"));
        assert!(s.contains("<svg "));
        assert!(s.contains("viewBox="));
        assert!(s.contains("class=\"outline-board\""), "explicit board outline expected");
        assert!(s.contains("data-id=\"psu.reg\""));
        assert!(s.contains(">psu.dec[0]</text>"));
        assert!(s.contains("class=\"pad\""), "pin pads expected");
        assert!(s.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn svg_falls_back_to_bounding_box_without_board() {
        let (doc, lib) = doc_psu(2);
        let s = svg(&doc, &lib);
        assert!(s.contains("class=\"outline-bbox\""), "implicit bbox outline expected");
    }

    #[test]
    fn exporters_are_deterministic() {
        let (doc, lib) = doc_psu(3);
        assert_eq!(netlist(&doc), netlist(&doc));
        assert_eq!(placement_csv(&doc), placement_csv(&doc));
        assert_eq!(svg(&doc, &lib), svg(&doc, &lib));
    }
}
