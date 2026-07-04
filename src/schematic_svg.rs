//! The schematic SVG renderer (Decision 20 — the second derived projection).
//!
//! [`schematic_svg`] renders the reflowed schematic ([`reflow`](crate::schematic::reflow))
//! as deterministic, byte-stable SVG: symbol boxes with pin stubs and names, a
//! refdes+name header per component, a **net tag at every connected pin** (§20c — tags
//! are the default connection rendering), the authored presentational **wires** drawn
//! *under* the symbols (§20d), and the derived **unplaced bin** set off by a labelled
//! divider so it reads as "unplaced", not layout.
//!
//! This is a *view* — it never mutates truth and reads only the netlist and the layout
//! tree. It lives beside the board renderer in [`crate::export`] rather than inside it
//! because it renders a *different coordinate space* (schematic space, y-up, board-
//! independent) with its own conventions (tags, stubs, the bin), sharing only the low-
//! level `fmt_mm`/`xml_escape` helpers (re-exported `pub(crate)`), not the board machinery.
//!
//! **Text** is drawn as SVG `<text>` (not stroked glyphs): the schematic sketch is a
//! human eyeball artifact, `<text>` is deterministic, and it matches the id-label idiom
//! already in `export::svg`. Coordinates are integer nm → six-decimal mm via
//! [`crate::export`]'s `fmt_mm`, the one project convention.

use crate::doc::{Doc, MM, Nm, Orient, Point};
use crate::export::{fmt_mm, xml_escape};
use crate::part::PartLib;
use crate::schematic::{LayoutNode, PinSide, Placement, pin_slots, symbol_extent};
use std::collections::BTreeMap;

/// Length of a pin stub drawn out from the box edge, and the gap before its name/tag.
const STUB_LEN: Nm = 1_270_000; // half a pin pitch.
/// Text height for pin names and the component header, in nm (→ SVG font-size in mm).
const PIN_TEXT_H: Nm = 1_000_000;
const HEADER_TEXT_H: Nm = 1_500_000;
const TAG_TEXT_H: Nm = 1_000_000;
/// Margin around the whole drawing, in nm.
const MARGIN: Nm = 2 * MM;

/// Render the reflowed schematic as deterministic SVG (Decision 20). Reads the layout
/// tree ([`Doc::schematic`]) and the elaborated netlist ([`Doc::nets`]); every component
/// is drawn (§20c totality) — placed ones in the flow, the rest in the unplaced bin below
/// a labelled divider. Output is byte-identical across runs (all iteration is over
/// `BTreeMap`s / the pre-order tree walk, all arithmetic integer nm).
pub fn schematic_svg(doc: &Doc, lib: &PartLib) -> String {
    let placements = doc.reflow_schematic(lib);

    // Authored schematic rotation per component path (the `Symbol.rot` leaf, §20b), so pin
    // stubs rotate with the box. Absent ⇒ identity (unplaced parts, or no `schematic`).
    let rots = symbol_rotations(doc);
    // The placed set: components the tree names *and* that are populated. Everything else
    // (never-placed, or a part missing from the lib) is a bin cell. Used for the divider.
    let placed = placed_paths(doc, lib);

    // Pin identity -> net name, from the materialized netlist (the tag source). A pin absent
    // here joins no net and gets no tag (§20c: unconnected pins get nothing).
    let pin_net: BTreeMap<(String, String), String> = doc
        .nets
        .values()
        .flat_map(|net| {
            net.members
                .iter()
                .map(move |m| ((m.comp.to_string(), m.pin.clone()), net.name.clone()))
        })
        .collect();
    // No-connect marks (§20c): a pin the source declared `nc` gets a small ✕ instead of a
    // tag. Keyed the same way.
    let nc: std::collections::BTreeSet<(String, String)> = doc
        .no_connects
        .iter()
        .map(|p| (p.comp.to_string(), p.pin.clone()))
        .collect();

    // ---- content bounds -------------------------------------------------------------
    // Gather every drawn point: box corners (+ stub reach for names/tags), wire endpoints
    // and waypoints. The name/tag text extends past the stub; a generous horizontal pad
    // keeps labels in view without measuring glyphs.
    const LABEL_PAD: Nm = 20 * MM;
    let mut xs: Vec<Nm> = Vec::new();
    let mut ys: Vec<Nm> = Vec::new();
    let mut note = |x: Nm, y: Nm| {
        xs.push(x);
        ys.push(y);
    };
    for pl in placements.values() {
        let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);
        note(pl.center.x - hw - STUB_LEN - LABEL_PAD, pl.center.y - hh);
        note(
            pl.center.x + hw + STUB_LEN + LABEL_PAD,
            pl.center.y + hh + HEADER_TEXT_H,
        );
    }
    for w in wire_polylines(doc, &placements, lib, &rots) {
        for p in w {
            note(p.x, p.y);
        }
    }
    let (mut x0, mut y0, mut x1, mut y1) = if xs.is_empty() {
        (0, 0, 10 * MM, 10 * MM)
    } else {
        (
            *xs.iter().min().unwrap(),
            *ys.iter().min().unwrap(),
            *xs.iter().max().unwrap(),
            *ys.iter().max().unwrap(),
        )
    };
    x0 -= MARGIN;
    y0 -= MARGIN;
    x1 += MARGIN;
    y1 += MARGIN;
    // Schematic space is y-up; SVG is y-down. Flip within the bounds so it reads upright.
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

    // ---- wires, under the symbols (§20d) --------------------------------------------
    for poly in wire_polylines(doc, &placements, lib, &rots) {
        if poly.len() < 2 {
            continue;
        }
        let pts: Vec<String> = poly
            .iter()
            .map(|p| format!("{},{}", fmt_mm(p.x), fmt_mm(flip(p.y))))
            .collect();
        out.push_str(&format!(
            "  <polyline class=\"wire\" points=\"{}\" fill=\"none\" stroke=\"#0a0\" stroke-width=\"0.15\"/>\n",
            pts.join(" ")
        ));
    }

    // ---- symbols (BTreeMap order ⇒ deterministic) -----------------------------------
    for (id, pl) in &placements {
        let comp = &doc.components[id];
        let def = lib.get(&comp.part);
        let rot = rots.get(id.as_str()).copied().unwrap_or(Orient::IDENTITY);
        out.push_str(&format!(
            "  <g class=\"symbol\" data-id=\"{}\">\n",
            xml_escape(id.as_str())
        ));

        // Box.
        let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);
        out.push_str(&format!(
            "    <rect class=\"body\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
            fmt_mm(pl.center.x - hw),
            fmt_mm(flip(pl.center.y + hh)),
            fmt_mm(pl.extent.w),
            fmt_mm(pl.extent.h),
        ));

        // Header: refdes/name above the box top-left.
        let header = comp.part.clone();
        out.push_str(&format!(
            "    <text class=\"header\" x=\"{}\" y=\"{}\" font-size=\"{}\">{}</text>\n",
            fmt_mm(pl.center.x - hw),
            fmt_mm(flip(pl.center.y + hh) - fmt_gap()),
            fmt_mm(HEADER_TEXT_H),
            xml_escape(&format!("{id} ({header})")),
        ));

        // Pin stubs + names + net tags, per the sizing convention (only when the part is
        // known; a missing part draws the min box with no stubs — the view stays total).
        if let Some(def) = def {
            // Stubs are built in the part's UNROTATED frame (`pin_slots` places `dy` against
            // the unrotated box height), so the x-offset must be the unrotated half-width —
            // NOT `pl.extent`, whose w/h reflow already swapped for a 90/270 rot. Using the
            // swapped width here and *then* rotating would double-count the swap and float
            // the stubs off the box. `stub_geometry` applies `rot` to the finished point.
            let unrot_hw = symbol_extent(def).w / 2;
            for slot in pin_slots(def) {
                // Stub base on the box edge (unrotated frame), then rotated by the authored
                // schematic rot so the stub sticks out the correct side of the rotated box.
                let (base, tip, tag_anchor, name_x, anchor) =
                    stub_geometry(slot.side, unrot_hw, slot.dy, rot);
                let (bx, by) = (pl.center.x + base.x, pl.center.y + base.y);
                let (tx, ty) = (pl.center.x + tip.x, pl.center.y + tip.y);
                out.push_str(&format!(
                    "    <line class=\"stub\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                    fmt_mm(bx),
                    fmt_mm(flip(by)),
                    fmt_mm(tx),
                    fmt_mm(flip(ty)),
                ));
                // Pin name, just inside the box edge, anchored toward the interior.
                let (nx, ny) = (pl.center.x + name_x.x, pl.center.y + name_x.y);
                out.push_str(&format!(
                    "    <text class=\"pin\" x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"{}\">{}</text>\n",
                    fmt_mm(nx),
                    fmt_mm(flip(ny) + PIN_TEXT_H / 3),
                    fmt_mm(PIN_TEXT_H),
                    anchor.inner,
                    xml_escape(&slot.name),
                ));
                // Net tag (§20c) at the stub tip, or a no-connect ✕, or nothing.
                let key = (id.to_string(), slot.id.clone());
                let (gx, gy) = (pl.center.x + tag_anchor.x, pl.center.y + tag_anchor.y);
                if let Some(net) = pin_net.get(&key) {
                    out.push_str(&format!(
                        "    <text class=\"tag\" x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"{}\">{}</text>\n",
                        fmt_mm(gx),
                        fmt_mm(flip(gy) + TAG_TEXT_H / 3),
                        fmt_mm(TAG_TEXT_H),
                        anchor.outer,
                        xml_escape(net),
                    ));
                } else if nc.contains(&key) {
                    out.push_str(&format!(
                        "    <text class=\"nc\" x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"{}\">✕</text>\n",
                        fmt_mm(gx),
                        fmt_mm(flip(gy) + TAG_TEXT_H / 3),
                        fmt_mm(TAG_TEXT_H),
                        anchor.outer,
                    ));
                }
            }
        }
        out.push_str("  </g>\n");
    }

    // ---- unplaced-bin divider + label -----------------------------------------------
    // The bin sits below the placed content. Draw a divider between the lowest placed box
    // bottom and the highest bin box top, labelled so the bin reads as "unplaced".
    if let Some(div_y) = bin_divider_y(&placements, &placed) {
        out.push_str(&format!(
            "  <line class=\"bin-divider\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#888\" stroke-width=\"0.1\" stroke-dasharray=\"1,1\"/>\n",
            fmt_mm(x0 + MARGIN / 2),
            fmt_mm(flip(div_y)),
            fmt_mm(x1 - MARGIN / 2),
            fmt_mm(flip(div_y)),
        ));
        out.push_str(&format!(
            "  <text class=\"bin-label\" x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"#888\">unplaced</text>\n",
            fmt_mm(x0 + MARGIN / 2),
            fmt_mm(flip(div_y) + HEADER_TEXT_H),
            fmt_mm(HEADER_TEXT_H),
        ));
    }

    out.push_str("</svg>\n");
    out
}

/// A small vertical gap (nm) used to lift the header off the box top.
fn fmt_gap() -> Nm {
    500_000
}

/// The `text-anchor` values for a pin's name (drawn *inside* the box) and its net tag
/// (drawn *outside*, past the stub tip), so labels read outward from the box.
struct Anchors {
    inner: &'static str,
    outer: &'static str,
}

/// Geometry of one pin stub in the box frame (offsets from the box center): the stub base
/// on the box edge, its tip, the tag anchor (just past the tip), and the pin-name anchor
/// (just inside the edge), all rotated by the authored schematic `rot`. Cardinal-only, so
/// the rotation is exact. `hw` is the box half-width; `dy` the stub's vertical offset.
fn stub_geometry(
    side: PinSide,
    hw: Nm,
    dy: Nm,
    rot: Orient,
) -> (Point, Point, Point, Point, Anchors) {
    // Unrotated: left stubs point out −x, right stubs out +x.
    let sign = match side {
        PinSide::Left => -1,
        PinSide::Right => 1,
    };
    let edge = Point {
        x: sign * hw,
        y: dy,
    };
    let tip = Point {
        x: sign * (hw + STUB_LEN),
        y: dy,
    };
    let tag = Point {
        x: sign * (hw + STUB_LEN + STUB_LEN / 2),
        y: dy,
    };
    let name = Point {
        x: sign * (hw - STUB_LEN / 4),
        y: dy,
    };
    // Rotate each offset by the cardinal `rot` (exact for cardinals).
    let r = |p: Point| rot.apply(p);
    // Text anchors: inner names hug the interior, outer tags read away from the box.
    let anchors = match side {
        PinSide::Left => Anchors {
            inner: "start",
            outer: "end",
        },
        PinSide::Right => Anchors {
            inner: "end",
            outer: "start",
        },
    };
    (r(edge), r(tip), r(tag), r(name), anchors)
}

/// Authored schematic rotation ([`Symbol.rot`], §20b) per component path, from the layout
/// tree. Deterministic pre-order walk; last placement of a path wins (validation forbids
/// duplicates, so this is unambiguous in a valid doc).
fn symbol_rotations(doc: &Doc) -> BTreeMap<String, Orient> {
    let mut out = BTreeMap::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    fn walk(nodes: &[LayoutNode], out: &mut BTreeMap<String, Orient>) {
        for n in nodes {
            match n {
                LayoutNode::Symbol(s) => {
                    out.insert(s.path.clone(), s.rot);
                }
                LayoutNode::Container(c) => walk(&c.children, out),
                _ => {}
            }
        }
    }
    walk(&layout.roots, &mut out);
    out
}

/// Component paths that are *placed* by the tree (named by a `sym` **and** populated). The
/// complement (within the placement set) is the unplaced bin — used to site the divider.
fn placed_paths(doc: &Doc, lib: &PartLib) -> std::collections::BTreeSet<String> {
    let _ = lib;
    let mut out = std::collections::BTreeSet::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    for path in layout.symbol_paths() {
        if doc.components.contains_key(&crate::id::EntityId::new(path)) {
            out.insert(path.to_string());
        }
    }
    out
}

/// The y (schematic space, nm) of the unplaced-bin divider: midway between the lowest
/// placed box bottom and the highest bin box top. `None` when there is nothing placed *or*
/// nothing in the bin (no divider needed — the drawing is all one or all the other).
fn bin_divider_y(
    placements: &BTreeMap<crate::id::EntityId, Placement>,
    placed: &std::collections::BTreeSet<String>,
) -> Option<Nm> {
    let mut lowest_placed: Option<Nm> = None;
    let mut highest_bin: Option<Nm> = None;
    for (id, pl) in placements {
        let bottom = pl.center.y - pl.extent.h / 2;
        let top = pl.center.y + pl.extent.h / 2;
        if placed.contains(id.as_str()) {
            lowest_placed = Some(lowest_placed.map_or(bottom, |v: Nm| v.min(bottom)));
        } else {
            highest_bin = Some(highest_bin.map_or(top, |v: Nm| v.max(top)));
        }
    }
    match (lowest_placed, highest_bin) {
        (Some(lo), Some(hi)) => Some((lo + hi) / 2),
        _ => None,
    }
}

/// Each drawn wire (§20d) as a schematic-space polyline: pin-A world point, the authored
/// waypoints in order, pin-B world point. An *unplaced* component is still in `placements`
/// (in the bin, §20c totality), so a wire to it draws to the bin — that is intentional, not
/// a drop. A wire is dropped only when an endpoint is genuinely absent from `placements` (a
/// DNP-dropped component — the source declared it but a false `if=` removed it) or its part
/// is missing from the lib / the pin selector resolves to no stub; those cases earn a
/// warning at commit and simply are not drawn. Pre-order wire walk, so the order is
/// deterministic.
fn wire_polylines(
    doc: &Doc,
    placements: &BTreeMap<crate::id::EntityId, Placement>,
    lib: &PartLib,
    rots: &BTreeMap<String, Orient>,
) -> Vec<Vec<Point>> {
    let mut out = Vec::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    for w in layout.wires() {
        let (Some(a), Some(b)) = (
            wire_end_point(doc, placements, lib, rots, &w.a.comp, &w.a.pin),
            wire_end_point(doc, placements, lib, rots, &w.b.comp, &w.b.pin),
        ) else {
            continue;
        };
        let mut poly = vec![a];
        poly.extend(w.waypoints.iter().copied());
        poly.push(b);
        out.push(poly);
    }
    out
}

/// The schematic-space point of a wire endpoint: the stub *tip* of the named pin on the
/// placed symbol (so wires meet the drawn stubs, not the box edge). `None` if the
/// component is not placed, the part is unknown, or the pin selector resolves to no stub.
fn wire_end_point(
    doc: &Doc,
    placements: &BTreeMap<crate::id::EntityId, Placement>,
    lib: &PartLib,
    rots: &BTreeMap<String, Orient>,
    comp: &str,
    pin: &str,
) -> Option<Point> {
    let cid = crate::id::EntityId::new(comp);
    let pl = placements.get(&cid)?;
    let def = lib.get(&doc.components.get(&cid)?.part)?;
    // Resolve the authored selector to a stored identity, then find that pin's slot.
    let ids = def.resolve_selector(pin);
    let want = ids.first().map(String::as_str).unwrap_or(pin);
    let slot = pin_slots(def).into_iter().find(|s| s.id == want)?;
    let rot = rots.get(comp).copied().unwrap_or(Orient::IDENTITY);
    // Unrotated half-width (see the symbol loop): `stub_geometry` builds in the part's own
    // frame and rotates the point, so passing `pl.extent` (reflow-swapped) would misplace
    // the tip on a rotated symbol and drag the wire endpoint with it.
    let unrot_hw = symbol_extent(def).w / 2;
    let (_base, tip, _tag, _name, _anchors) = stub_geometry(slot.side, unrot_hw, slot.dy, rot);
    Some(Point {
        x: pl.center.x + tip.x,
        y: pl.center.y + tip.y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::history::History;
    use crate::part::part_library;

    /// Elaborate a document from source text and render its schematic.
    fn render(src: &str) -> String {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::LoadText(src.into())), &lib, "t")
            .unwrap_or_else(|e| panic!("commit failed: {e:?}"));
        schematic_svg(h.doc(), &lib)
    }

    #[test]
    fn renders_boxes_headers_and_net_tags() {
        // Two caps on one net, both placed; a symbol box + header + a net tag at each pin.
        let s = render(
            "inst C1 Cap\ninst C2 Cap\nnet VCC C1.p1 C2.p1\nschematic {\n  row {\n    sym C1\n    sym C2\n  }\n}\n",
        );
        assert!(s.contains("class=\"symbol\" data-id=\"C1\""), "{s}");
        assert!(s.contains("class=\"body\""), "symbol box expected:\n{s}");
        assert!(
            s.contains(">C1 (Cap)<"),
            "refdes+name header expected:\n{s}"
        );
        // The net tag is drawn at the connected pin (§20c: tags are the default rendering).
        assert!(s.contains("class=\"tag\""), "net tag expected:\n{s}");
        assert!(s.contains(">VCC<"), "net name tag expected:\n{s}");
    }

    #[test]
    fn unplaced_component_gets_a_bin_divider() {
        // C1 placed, C2 not — C2 falls to the bin below a labelled divider.
        let s = render("inst C1 Cap\ninst C2 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
        assert!(
            s.contains("class=\"bin-divider\""),
            "divider expected:\n{s}"
        );
        assert!(s.contains(">unplaced<"), "bin label expected:\n{s}");
        // Both components are drawn (§20c totality).
        assert!(
            s.contains("data-id=\"C1\"") && s.contains("data-id=\"C2\""),
            "{s}"
        );
    }

    #[test]
    fn no_bin_divider_when_all_placed() {
        let s = render("inst C1 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
        assert!(
            !s.contains("bin-divider"),
            "no divider when all placed:\n{s}"
        );
    }

    #[test]
    fn draws_authored_wire() {
        // A straight wire between two placed pins renders as a polyline under the symbols.
        let s = render(
            "inst C1 Cap\ninst C2 Cap\nschematic {\n  row gap=10mm {\n    sym C1\n    sym C2\n    wire C1.p2 C2.p1\n  }\n}\n",
        );
        assert!(s.contains("class=\"wire\""), "wire polyline expected:\n{s}");
        // The wire is emitted before the first symbol group (drawn under them, §20d).
        let wire_at = s.find("class=\"wire\"").unwrap();
        let sym_at = s.find("class=\"symbol\"").unwrap();
        assert!(wire_at < sym_at, "wire must render under symbols:\n{s}");
    }

    #[test]
    fn stubs_sit_on_the_box_perimeter_for_every_rotation() {
        // Regression for the rot=90/270 stub-detach bug: stub geometry is built in the
        // part's UNROTATED frame and rotated, so a stub base must land exactly on the
        // (post-rotation) box perimeter and its tip must fall strictly outside — for all
        // four cardinals, not just identity. The box after `rot` has the reflow-swapped
        // extent, centered on the origin; check each stub against that box.
        use crate::part::part_library;
        let lib = part_library();
        let def = &lib["MCU"]; // 4 edge pins, non-square box (exercises the swap).
        let base_ext = symbol_extent(def);
        let unrot_hw = base_ext.w / 2;

        for deg in [0, 90, 180, 270] {
            let rot = Orient::from_deg(deg).unwrap();
            // Post-rotation box half-extents (reflow swaps w/h for 90/270).
            let (hw, hh) = if deg == 90 || deg == 270 {
                (base_ext.h / 2, base_ext.w / 2)
            } else {
                (base_ext.w / 2, base_ext.h / 2)
            };
            for slot in pin_slots(def) {
                // Production always feeds the UNROTATED half-width; `stub_geometry` rotates.
                let (base, tip, _tag, _name, _anchor) =
                    stub_geometry(slot.side, unrot_hw, slot.dy, rot);
                // Base on the perimeter: it touches one edge (|coord| == that half-extent)
                // and stays within the other (± a 1 nm rounding slack from the halving).
                let on_x = (base.x.abs() - hw).abs() <= 1 && base.y.abs() <= hh + 1;
                let on_y = (base.y.abs() - hh).abs() <= 1 && base.x.abs() <= hw + 1;
                assert!(
                    on_x || on_y,
                    "rot={deg} stub base {base:?} off the {hw}×{hh} box perimeter"
                );
                // Tip strictly outside the box (the stub points outward).
                assert!(
                    tip.x.abs() > hw || tip.y.abs() > hh,
                    "rot={deg} stub tip {tip:?} not outside the {hw}×{hh} box"
                );
            }
        }
    }

    #[test]
    fn output_is_deterministic() {
        let src = "inst C1 Cap\ninst U1 MCU\nnet N C1.p1 U1.VDD\nschematic {\n  row {\n    sym C1\n    sym U1\n  }\n}\n";
        assert_eq!(render(src), render(src), "byte-identical across runs");
    }

    #[test]
    fn totality_with_no_schematic_block() {
        // No `schematic` block ⇒ every component still renders (all in the bin).
        let s = render("inst C1 Cap\ninst C2 Cap\n");
        assert!(
            s.contains("data-id=\"C1\"") && s.contains("data-id=\"C2\""),
            "{s}"
        );
    }
}
