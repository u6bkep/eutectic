//! Deterministic output artifacts: netlist, pick-and-place, and an SVG sketch.
//!
//! Each exporter is a *pure function* of its inputs (a `Doc`, plus the `PartLib`
//! for geometry) — no wall-clock, no randomness, no iteration over `HashMap`. The
//! model is built on `BTreeMap`/`BTreeSet` precisely so this output is byte-stable
//! and diffable: calling an exporter twice on the same inputs yields identical
//! strings, and a one-thing change produces a one-line diff.
//!
//! Artifacts: the connectivity (`netlist`), placement (`placement_csv`) and sketch
//! (`svg`) exporters, plus **fab output** — RS-274X Gerber per copper layer + an
//! `Edge.Cuts` outline ([`gerber_layer`] / [`gerber_edge_cuts`] / [`gerber_set`])
//! and an Excellon drill program ([`excellon_drill`]). Now that routing writes real
//! copper into the `Doc` (traces with width, vias with pad+drill) and footprint pads
//! carry render geometry, the fab artifacts describe genuine copper. All coordinates
//! flow from integer nanometres into each format by integer arithmetic (the Gerber
//! `%FSLAX46Y46*%` fixed-point format *is* nanometres — see [`gbr_coord`]), so the
//! determinism invariant holds end to end. See docs/architecture.md, "Prototype
//! status (Gerber/fab output)".

use crate::doc::{Doc, Nm, Point, MM};
use crate::elaborate::GenDirective;
use crate::part::{pin_world, Pad, PadShape, PartDef, PartLib};
use crate::route::{Layer, Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

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

/// Enumerate the stable pin identities of a part, deterministically: discrete pins
/// by pad **number** in declaration order, then `port.signal` for each interface
/// signal (both `BTreeMap`-ordered). These are exactly the identities [`pin_world`]
/// resolves — numbers, not names, since functional names can repeat across pads.
fn part_pin_ids(def: &PartDef) -> Vec<String> {
    let mut ids: Vec<String> = def.pins.iter().map(|p| p.number.clone()).collect();
    for (port, iface) in &def.interfaces {
        for sig in iface.signals.keys() {
            ids.push(format!("{port}.{sig}"));
        }
    }
    ids
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
    // elaboration). Routed copper (traces/vias) is drawn on top of this outline and
    // the placed pads.
    let board = source_board(doc);

    // Gather every point that must be in view: component origins, their pin pads,
    // and the board corners.
    let mut pts: Vec<Point> = Vec::new();
    for c in doc.components.values() {
        pts.push(c.pos.value);
        if let Some(def) = lib.get(&c.part) {
            for id in part_pin_ids(def) {
                if let Some(w) = pin_world(c, def, &id) {
                    pts.push(w);
                }
            }
        }
    }
    if let Some((min, max)) = board {
        pts.push(min);
        pts.push(max);
    }
    // Routed copper must be in view too.
    for t in doc.traces.values() {
        pts.extend(t.path.iter().copied());
    }
    for v in doc.vias.values() {
        pts.push(v.at);
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
            for id in part_pin_ids(def) {
                if let Some(w) = pin_world(c, def, &id) {
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

    // Routed copper, on top of the components: trace polylines (per-layer colour and
    // class, stroke width = the copper width) in `TraceId` order, then via pads as
    // circles in `ViaId` order. Deterministic, like everything above.
    for (tid, t) in &doc.traces {
        let path: Vec<String> = t
            .path
            .iter()
            .map(|p| format!("{},{}", fmt_mm(p.x), fmt_mm(flip(p.y))))
            .collect();
        out.push_str(&format!(
            "  <polyline class=\"trace trace-{}\" data-id=\"{}\" points=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n",
            layer_class(t.layer),
            tid,
            path.join(" "),
            layer_color(t.layer),
            fmt_mm(t.width),
        ));
    }
    for (vid, v) in &doc.vias {
        out.push_str(&format!(
            "  <circle class=\"via\" data-id=\"{}\" cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"#333\"/>\n",
            vid,
            fmt_mm(v.at.x),
            fmt_mm(flip(v.at.y)),
            fmt_mm(v.pad / 2),
        ));
    }

    out.push_str("</svg>\n");
    out
}

/// SVG class suffix / stroke colour for a copper layer (Top warm, Bottom cool,
/// inner green) — render-only, just enough to tell the layers apart by eye.
fn layer_class(l: Layer) -> &'static str {
    match l {
        Layer::Top => "top",
        Layer::Bottom => "bottom",
        Layer::Inner(_) => "inner",
    }
}
fn layer_color(l: Layer) -> &'static str {
    match l {
        Layer::Top => "#cc0000",
        Layer::Bottom => "#0066cc",
        Layer::Inner(_) => "#00aa00",
    }
}

// ---- 4. Gerber (RS-274X) + Excellon drill (fab output) ----

/// The board outline rectangle carried by tier-1 source (last `Board` wins, as in
/// elaboration), if any.
fn source_board(doc: &Doc) -> Option<(Point, Point)> {
    doc.source.iter().rev().find_map(|d| match d {
        GenDirective::Board { min, max } => Some((*min, *max)),
        _ => None,
    })
}

/// Bounding box of all placed/routed geometry (pad world points, trace vertices,
/// via centres) plus a 2 mm margin — the `Edge.Cuts` fallback when the source
/// carries no explicit `Board`. Falls back to a 10 mm box for an empty document.
fn placement_bbox(doc: &Doc, lib: &PartLib) -> (Point, Point) {
    const MARGIN: Nm = 2 * MM;
    let mut pts: Vec<Point> = Vec::new();
    for c in doc.components.values() {
        if let Some(def) = lib.get(&c.part) {
            for id in part_pin_ids(def) {
                if let Some(w) = pin_world(c, def, &id) {
                    pts.push(w);
                }
            }
        }
        pts.push(c.pos.value);
    }
    for t in doc.traces.values() {
        pts.extend(t.path.iter().copied());
    }
    for v in doc.vias.values() {
        pts.push(v.at);
    }
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
    (Point { x: x0 - MARGIN, y: y0 - MARGIN }, Point { x: x1 + MARGIN, y: y1 + MARGIN })
}

/// A Gerber aperture — the standard primitives this exporter needs. `Ord` so a
/// layer's aperture table gets codes assigned deterministically.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Aperture {
    /// Round: trace draws and round (via / circular) pads — one diameter.
    Circle(Nm),
    /// Rectangle — also the bounding-box stand-in for roundrect/custom pads.
    Rect(Nm, Nm),
    /// Obround / oval pad.
    Obround(Nm, Nm),
}

impl Aperture {
    /// The `%ADD%` template body, e.g. `C,0.150000` or `R,0.600000X1.550000`. Sizes
    /// are decimal millimetres (the standard aperture-definition unit).
    fn template(self) -> String {
        match self {
            Aperture::Circle(d) => format!("C,{}", fmt_mm(d)),
            Aperture::Rect(w, h) => format!("R,{}X{}", fmt_mm(w), fmt_mm(h)),
            Aperture::Obround(w, h) => format!("O,{}X{}", fmt_mm(w), fmt_mm(h)),
        }
    }
}

/// The aperture for a component pad. `RoundRect` and any unknown shape collapse to
/// their bounding `Rect` (Gerber's basic apertures have no rounded-rectangle, and
/// the corner radius is irrelevant to a copper flash at this fidelity).
fn pad_aperture(p: &Pad) -> Aperture {
    match p.shape {
        PadShape::Circle => Aperture::Circle(p.size.0),
        PadShape::Rect | PadShape::RoundRect => Aperture::Rect(p.size.0, p.size.1),
        PadShape::Oval => Aperture::Obround(p.size.0, p.size.1),
    }
}

/// A Gerber coordinate in the `%FSLAX46Y46*%` fixed-point format: 4 integer + 6
/// fractional digits of millimetre, leading zeros omitted. Because 1 mm =
/// 1_000_000 nm, the integer the file carries *is exactly the nanometre value* — so
/// this is just the integer, formatted with no float anywhere.
fn gbr_coord(nm: Nm) -> String {
    nm.to_string()
}

/// The KiCad-style layer token used in fab filenames: `F_Cu` / `B_Cu` / `In<n>_Cu`.
fn layer_file(l: Layer) -> String {
    match l {
        Layer::Top => "F_Cu".to_string(),
        Layer::Bottom => "B_Cu".to_string(),
        Layer::Inner(n) => format!("In{}_Cu", n as u16 + 1),
    }
}

/// The copper layers to emit: the outer copper (`Top`/`Bottom`, always present —
/// component pads occupy them under the all-layer pad model) plus any layer a trace
/// sits on or a via terminates on, in physical stack-up order.
fn copper_layers(doc: &Doc) -> Vec<Layer> {
    let mut set: BTreeSet<Layer> = BTreeSet::new();
    set.insert(Layer::Top);
    set.insert(Layer::Bottom);
    for t in doc.traces.values() {
        set.insert(t.layer);
    }
    for v in doc.vias.values() {
        set.insert(v.from);
        set.insert(v.to);
    }
    set.into_iter().collect()
}

/// Every component pad carrying copper geometry, as `(world position, aperture)`,
/// in `(EntityId, pin-declaration)` order. Pads are points on **all** layers in
/// this model (through-hole assumption), so the same set flashes on every copper
/// layer. Toy-library pins (no footprint, `pad: None`) contribute nothing.
fn component_pad_flashes(doc: &Doc, lib: &PartLib) -> Vec<(Point, Aperture)> {
    let mut out = Vec::new();
    for c in doc.components.values() {
        if let Some(def) = lib.get(&c.part) {
            for pin in &def.pins {
                if let Some(pad) = pin.pad
                    && let Some(w) = pin_world(c, def, &pin.number)
                {
                    out.push((w, pad_aperture(&pad)));
                }
            }
        }
    }
    out
}

/// One copper layer as RS-274X Gerber. Emits the format spec, mm units, the layer's
/// aperture table (codes 10.. in `Aperture` order), then objects: each trace's
/// centreline as a `D02` move + `D01` draws with its width aperture, and each via
/// pad / component pad as a `D03` flash with its shape aperture. Object order is
/// `TraceId`, then `ViaId`, then component pads — fully deterministic. Ends `M02*`.
pub fn gerber_layer(doc: &Doc, lib: &PartLib, layer: Layer) -> String {
    let traces: Vec<&Trace> = doc.traces.values().filter(|t| t.layer == layer).collect();
    let vias: Vec<&Via> = doc.vias.values().filter(|v| v.spans(layer)).collect();
    let pads = component_pad_flashes(doc, lib);

    // Aperture table: distinct apertures, codes from 10 in `Ord` order.
    let mut aps: BTreeSet<Aperture> = BTreeSet::new();
    for t in &traces {
        aps.insert(Aperture::Circle(t.width));
    }
    for v in &vias {
        aps.insert(Aperture::Circle(v.pad));
    }
    for (_, a) in &pads {
        aps.insert(*a);
    }
    let codes: BTreeMap<Aperture, u32> =
        aps.iter().enumerate().map(|(i, a)| (*a, 10 + i as u32)).collect();

    let mut out = String::new();
    out.push_str(&format!("G04 {} *\n", layer_file(layer)));
    out.push_str("%FSLAX46Y46*%\n");
    out.push_str("%MOMM*%\n");
    for (a, code) in &codes {
        out.push_str(&format!("%ADD{}{}*%\n", code, a.template()));
    }
    out.push_str("G01*\n"); // linear interpolation

    // Trace draws.
    for t in &traces {
        let code = codes[&Aperture::Circle(t.width)];
        out.push_str(&format!("D{code}*\n"));
        for (i, p) in t.path.iter().enumerate() {
            let op = if i == 0 { "D02" } else { "D01" };
            out.push_str(&format!("X{}Y{}{}*\n", gbr_coord(p.x), gbr_coord(p.y), op));
        }
    }
    // Via pad flashes (only on the layers the via spans).
    for v in &vias {
        let code = codes[&Aperture::Circle(v.pad)];
        out.push_str(&format!("D{code}*\n"));
        out.push_str(&format!("X{}Y{}D03*\n", gbr_coord(v.at.x), gbr_coord(v.at.y)));
    }
    // Component pad flashes (all-layer model).
    for (p, a) in &pads {
        let code = codes[a];
        out.push_str(&format!("D{code}*\n"));
        out.push_str(&format!("X{}Y{}D03*\n", gbr_coord(p.x), gbr_coord(p.y)));
    }

    out.push_str("M02*\n");
    out
}

/// The `Edge.Cuts` Gerber: the board outline as a closed rectangle drawn with a thin
/// (0.1 mm) round pen. Uses the source `Board` rect, else the placement bounding box.
pub fn gerber_edge_cuts(doc: &Doc, lib: &PartLib) -> String {
    let (min, max) = source_board(doc).unwrap_or_else(|| placement_bbox(doc, lib));
    let mut out = String::new();
    out.push_str("G04 Edge.Cuts *\n");
    out.push_str("%FSLAX46Y46*%\n");
    out.push_str("%MOMM*%\n");
    out.push_str("%ADD10C,0.100000*%\n");
    out.push_str("D10*\n");
    out.push_str("G01*\n");
    let corners = [
        (min.x, min.y),
        (max.x, min.y),
        (max.x, max.y),
        (min.x, max.y),
        (min.x, min.y),
    ];
    for (i, (x, y)) in corners.iter().enumerate() {
        let op = if i == 0 { "D02" } else { "D01" };
        out.push_str(&format!("X{}Y{}{}*\n", gbr_coord(*x), gbr_coord(*y), op));
    }
    out.push_str("M02*\n");
    out
}

/// The Excellon drill program for the board's plated holes (via drills today — the
/// model carries no other through-holes). Tools are the distinct drill diameters,
/// sorted and numbered `T1..`; under each tool, its hole coordinates in `ViaId`
/// order. Coordinates and tool sizes are decimal millimetres via [`fmt_mm`]
/// (explicit decimal points, so zero-suppression mode is moot). Deterministic.
pub fn excellon_drill(doc: &Doc) -> String {
    let mut dias: BTreeSet<Nm> = BTreeSet::new();
    for v in doc.vias.values() {
        dias.insert(v.drill);
    }
    let tools: BTreeMap<Nm, u32> =
        dias.iter().enumerate().map(|(i, d)| (*d, 1 + i as u32)).collect();

    let mut out = String::new();
    out.push_str("M48\n");
    out.push_str("; Excellon drill: plated through holes (via drills)\n");
    out.push_str("FMAT,2\n");
    out.push_str("METRIC,TZ\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}C{}\n", t, fmt_mm(*d)));
    }
    out.push_str("%\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}\n", t));
        for v in doc.vias.values() {
            if v.drill == *d {
                out.push_str(&format!("X{}Y{}\n", fmt_mm(v.at.x), fmt_mm(v.at.y)));
            }
        }
    }
    out.push_str("T0\n");
    out.push_str("M30\n");
    out
}

/// The full deterministic fab fileset: one Gerber per copper layer (`board-F_Cu.gbr`
/// …) in stack-up order, the `board-Edge_Cuts.gbr` outline, and the `board.drl`
/// Excellon drill program. `(filename, content)` pairs; no timestamps, stable order.
pub fn gerber_set(doc: &Doc, lib: &PartLib) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for layer in copper_layers(doc) {
        out.push((format!("board-{}.gbr", layer_file(layer)), gerber_layer(doc, lib, layer)));
    }
    out.push(("board-Edge_Cuts.gbr".to_string(), gerber_edge_cuts(doc, lib)));
    out.push(("board.drl".to_string(), excellon_drill(doc)));
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

    // --- fab output (Gerber / Excellon) ------------------------------------

    use crate::doc::Provenance;
    use crate::elaborate::GenDirective as G;
    use crate::id::{NetId, TraceId, ViaId};
    use crate::route::{Trace, Via};

    /// Two caps on a 20x10 board joined by net `N`, hand-routed with a known
    /// top trace, a bottom trace, and a via joining them at (10,5) — exact, so
    /// the fab output is fully predictable (no autorouter nondeterminism).
    fn hand_routed_board() -> (Doc, PartLib) {
        let lib = part_library();
        let mut h = History::new(Default::default());
        let src = vec![
            G::Board { min: Point::mm(0, 0), max: Point::mm(20, 10) },
            G::Instance { path: "c0".into(), part: "Cap".into() },
            G::Instance { path: "c1".into(), part: "Cap".into() },
            G::Place { path: "c0".into(), pos: Point::mm(5, 5) },
            G::Place { path: "c1".into(), pos: Point::mm(15, 5) },
            G::ConnectPins {
                net: "N".into(),
                pins: vec![("c0".into(), "p1".into()), ("c1".into(), "p1".into())],
            },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "place").unwrap();
        let net = NetId::new("N");
        let t0 = Trace {
            net: net.clone(),
            layer: Layer::Top,
            path: vec![Point::mm(6, 5), Point::mm(10, 5)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        let t1 = Trace {
            net: net.clone(),
            layer: Layer::Bottom,
            path: vec![Point::mm(10, 5), Point::mm(14, 5)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        let v = Via {
            net,
            at: Point::mm(10, 5),
            from: Layer::Top,
            to: Layer::Bottom,
            drill: 300_000,
            pad: 600_000,
            prov: Provenance::Pinned,
        };
        h.commit(
            Transaction(vec![
                Command::AddTrace(TraceId(0), t0),
                Command::AddTrace(TraceId(1), t1),
                Command::AddVia(ViaId(0), v),
            ]),
            &lib,
            "route",
        )
        .unwrap();
        (h.doc().clone(), lib)
    }

    #[test]
    fn gerber_layer_has_format_apertures_draws_and_flashes() {
        let (doc, lib) = hand_routed_board();
        let top = gerber_layer(&doc, &lib, Layer::Top);
        // Format spec + mm units + end.
        assert!(top.contains("%FSLAX46Y46*%"));
        assert!(top.contains("%MOMM*%"));
        assert!(top.trim_end().ends_with("M02*"));
        // Aperture defs: 0.2mm trace pen and 0.6mm via pad.
        assert!(top.contains("%ADD10C,0.200000*%"), "got:\n{top}");
        assert!(top.contains("%ADD11C,0.600000*%"), "got:\n{top}");
        // The Top trace: a move to (6,5) then a draw to (10,5) — nm == 4.6 integer.
        assert!(top.contains("X6000000Y5000000D02*"));
        assert!(top.contains("X10000000Y5000000D01*"));
        // The via flashes on Top (it spans Top..Bottom).
        assert!(top.contains("X10000000Y5000000D03*"));
        // Exactly one draw (one 2-pt trace) and one flash (the via) on Top.
        assert_eq!(top.matches("D01*").count(), 1);
        assert_eq!(top.matches("D03*").count(), 1);
        // The Bottom layer carries the other trace and the same via flash.
        let bot = gerber_layer(&doc, &lib, Layer::Bottom);
        assert_eq!(bot.matches("D01*").count(), 1);
        assert_eq!(bot.matches("D03*").count(), 1);
    }

    #[test]
    fn excellon_lists_via_drills() {
        let (doc, _lib) = hand_routed_board();
        let drl = excellon_drill(&doc);
        assert!(drl.starts_with("M48"));
        assert!(drl.contains("METRIC"));
        // One tool at the via's 0.3mm drill, with the via's coordinate.
        assert!(drl.contains("T1C0.300000"), "got:\n{drl}");
        assert!(drl.contains("X10.000000Y5.000000"), "got:\n{drl}");
        assert!(drl.trim_end().ends_with("M30"));
    }

    #[test]
    fn edge_cuts_traces_the_outline() {
        let (doc, lib) = hand_routed_board();
        let e = gerber_edge_cuts(&doc, &lib);
        assert!(e.contains("Edge.Cuts"));
        // Closed 0,0 -> 20,0 -> 20,10 -> 0,10 -> 0,0 rectangle (nm coordinates).
        assert!(e.contains("X0Y0D02*"));
        assert!(e.contains("X20000000Y0D01*"));
        assert!(e.contains("X20000000Y10000000D01*"));
        assert!(e.contains("X0Y10000000D01*"));
    }

    #[test]
    fn gerber_set_names_and_layers() {
        let (doc, lib) = hand_routed_board();
        let set = gerber_set(&doc, &lib);
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["board-F_Cu.gbr", "board-B_Cu.gbr", "board-Edge_Cuts.gbr", "board.drl"]
        );
    }

    #[test]
    fn svg_draws_traces_and_vias() {
        let (doc, lib) = hand_routed_board();
        let s = svg(&doc, &lib);
        assert!(s.contains("class=\"trace trace-top\""), "got:\n{s}");
        assert!(s.contains("class=\"trace trace-bottom\""));
        assert!(s.contains("class=\"via\""));
        // The polyline carries the trace's mm-formatted vertices.
        assert!(s.contains("6.000000,"));
        assert!(s.trim_end().ends_with("</svg>"));
    }

    /// A part with real pad geometry flashes as copper (rect + circle apertures).
    fn padded_board() -> (Doc, PartLib) {
        let mut lib = part_library();
        let fp = crate::kicad::import_footprint(
            r#"(footprint "PADX"
                (pad "1" smd rect (at -1 0) (size 0.6 1.2) (layers "F.Cu"))
                (pad "2" smd circle (at 1 0) (size 0.8 0.8) (layers "F.Cu")))"#,
        )
        .unwrap();
        lib.insert("PADX".into(), fp);
        let mut h = History::new(Default::default());
        let src = vec![
            G::Instance { path: "u1".into(), part: "PADX".into() },
            G::Place { path: "u1".into(), pos: Point::mm(5, 5) },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "p").unwrap();
        (h.doc().clone(), lib)
    }

    #[test]
    fn component_pads_flash_by_shape() {
        let (doc, lib) = padded_board();
        let top = gerber_layer(&doc, &lib, Layer::Top);
        // Rect pad 0.6x1.2 and circle pad 0.8 become R / C apertures.
        assert!(top.contains("R,0.600000X1.200000*%"), "got:\n{top}");
        assert!(top.contains("C,0.800000*%"), "got:\n{top}");
        // Two flashes at the pads' world positions: u1 at (5,5), pads at -1 / +1 mm.
        assert!(top.contains("X4000000Y5000000D03*"));
        assert!(top.contains("X6000000Y5000000D03*"));
        assert_eq!(top.matches("D03*").count(), 2);
    }

    #[test]
    fn fab_exporters_are_deterministic() {
        let (doc, lib) = hand_routed_board();
        assert_eq!(gerber_set(&doc, &lib), gerber_set(&doc, &lib));
        assert_eq!(gerber_layer(&doc, &lib, Layer::Top), gerber_layer(&doc, &lib, Layer::Top));
        assert_eq!(excellon_drill(&doc), excellon_drill(&doc));
        assert_eq!(gerber_edge_cuts(&doc, &lib), gerber_edge_cuts(&doc, &lib));
    }

    #[test]
    fn gerber_set_on_autorouted_board_is_deterministic() {
        use crate::autoroute::autoroute;
        use crate::route::DesignRules;
        let lib = part_library();
        let src = vec![
            G::Board { min: Point::mm(-6, -10), max: Point::mm(18, 10) },
            G::Instance { path: "reg".into(), part: "LDO".into() },
            G::Instance { path: "c0".into(), part: "Cap".into() },
            G::Instance { path: "c1".into(), part: "Cap".into() },
            G::Place { path: "reg".into(), pos: Point::mm(0, 0) },
            G::Place { path: "c0".into(), pos: Point::mm(12, 5) },
            G::Place { path: "c1".into(), pos: Point::mm(12, -5) },
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
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "place").unwrap();
        let result = autoroute(h.doc(), &lib, &DesignRules::default());
        h.commit(Transaction(result.commands), &lib, "route").unwrap();
        let doc = h.doc();
        // The autorouter laid real copper, so the F_Cu Gerber has trace draws.
        assert!(!doc.traces.is_empty());
        let top = gerber_layer(doc, &lib, Layer::Top);
        assert!(top.matches("D01*").count() > 0);
        assert_eq!(gerber_set(doc, &lib), gerber_set(doc, &lib));
    }
}
