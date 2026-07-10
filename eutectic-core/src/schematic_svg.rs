//! The schematic SVG renderer (Decision 20 — the second derived projection), rewired as
//! a **dumb serializer** of the realized-geometry stream (Decision 23).
//!
//! [`schematic_svg`] renders [`schematic_features`] — the one place the schematic drawing
//! is realized (symbol boxes, pin stubs and names, headers, net tags, nc marks, authored
//! wires, the unplaced-bin divider) — as deterministic, byte-stable SVG. All geometry,
//! positions, and content come from the stream; **no drawing convention lives here**.
//! What does live here is pure serialization: the viewBox from the stream's shared
//! [`Bounds`], the y-flip within those bounds (schematic space is y-up, SVG y-down), the
//! nm→mm number formatting ([`fmt_mm`]), XML escaping, the per-[`StyleClass`] class
//! names/colors/attribute layout, and the `<g class="symbol">` grouping derived from each
//! feature's [`Provenance`].
//!
//! This is a *view* — it never mutates truth. It lives beside the board renderer in
//! [`crate::export`] rather than inside it because it renders a *different coordinate
//! space* (schematic space, y-up, board-independent), sharing only the low-level
//! `fmt_mm`/`xml_escape` helpers (re-exported `pub(crate)`), not the board machinery.
//!
//! **Text** is drawn as SVG `<text>` (not stroked glyphs): the stream carries text as
//! *runs* (Decision 23 point 2), the schematic sketch is a human eyeball artifact,
//! `<text>` is deterministic, and it matches the id-label idiom in `export::svg`. Each
//! run's anchor is its baseline point, so serialization is `x`, `flip(y)`, `font-size`,
//! `text-anchor` — no vertical convention here.

use crate::doc::{Doc, Nm};
use crate::export::{fmt_mm, xml_escape};
use crate::id::EntityId;
use crate::part::PartLib;
use crate::schematic::{
    Bounds, Provenance, SchematicFeature, Shape, StyleClass, TextJustify, TextRun,
    schematic_features,
};

/// Render the schematic feature stream as deterministic SVG. Every component is drawn
/// (§20c totality) — placed ones in the flow, the rest in the unplaced bin below a
/// labelled divider. Output is byte-identical across runs (the stream is deterministic
/// and serialization is a pure fold over it).
pub fn schematic_svg(doc: &Doc, lib: &PartLib) -> String {
    let fs = schematic_features(doc, lib);
    let Bounds { x0, y0, x1, y1 } = fs.bounds;
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

    // The stream's order is the draw order (wires under symbols, each symbol's features
    // contiguous, chrome last). Component-owned features render inside a
    // `<g class="symbol" data-id="…">` group; the group boundary is where the owning
    // component of consecutive features changes.
    let mut open_group: Option<&EntityId> = None;
    for f in &fs.features {
        let owner = feature_owner(f);
        if owner != open_group {
            if open_group.is_some() {
                out.push_str("  </g>\n");
            }
            if let Some(id) = owner {
                out.push_str(&format!(
                    "  <g class=\"symbol\" data-id=\"{}\">\n",
                    xml_escape(id.as_str())
                ));
            }
            open_group = owner;
        }
        let indent = if open_group.is_some() { "    " } else { "  " };
        emit(&mut out, indent, f, &flip);
    }
    if open_group.is_some() {
        out.push_str("  </g>\n");
    }

    out.push_str("</svg>\n");
    out
}

/// The component a feature renders inside (its `<g>` group), from provenance. Wires and
/// chrome are top-level.
fn feature_owner(f: &SchematicFeature) -> Option<&EntityId> {
    match &f.provenance {
        Provenance::Component(id) => Some(id),
        Provenance::Pin { comp, .. } | Provenance::NetTag { comp, .. } => Some(comp),
        Provenance::Wire { .. } | Provenance::Chrome => None,
    }
}

/// Serialize one feature. The match is on style class — the class decides the SVG
/// element, its `class` attribute, and its presentation attributes (colors and the
/// historical attribute layout live here, and only here; geometry is the stream's).
fn emit(out: &mut String, indent: &str, f: &SchematicFeature, flip: &impl Fn(Nm) -> Nm) {
    match (&f.class, &f.shape) {
        (StyleClass::Wire, Shape::Polyline { pts, width }) => {
            if pts.len() < 2 {
                return;
            }
            let pts: Vec<String> = pts
                .iter()
                .map(|p| format!("{},{}", fmt_mm(p.x), fmt_mm(flip(p.y))))
                .collect();
            out.push_str(&format!(
                "{indent}<polyline class=\"wire\" points=\"{}\" fill=\"none\" stroke=\"#0a0\" stroke-width=\"{}\"/>\n",
                pts.join(" "),
                fmt_mm_trim(*width),
            ));
        }
        (StyleClass::SymbolOutline, Shape::Polygon { pts, width }) => {
            // The derived box-with-pins body is an axis-aligned rectangle; serialize its
            // bounding box as the historical `<rect>`. (Authored artwork later earns a
            // `<path>` arm — the stream contract does not change.)
            let (Some(min_x), Some(max_x)) =
                (pts.iter().map(|p| p.x).min(), pts.iter().map(|p| p.x).max())
            else {
                return;
            };
            let (Some(min_y), Some(max_y)) =
                (pts.iter().map(|p| p.y).min(), pts.iter().map(|p| p.y).max())
            else {
                return;
            };
            out.push_str(&format!(
                "{indent}<rect class=\"body\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"{}\"/>\n",
                fmt_mm(min_x),
                fmt_mm(flip(max_y)),
                fmt_mm(max_x - min_x),
                fmt_mm(max_y - min_y),
                fmt_mm_trim(*width),
            ));
        }
        (StyleClass::PinStub, Shape::Polyline { pts, width }) if pts.len() == 2 => {
            out.push_str(&format!(
                "{indent}<line class=\"stub\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"black\" stroke-width=\"{}\"/>\n",
                fmt_mm(pts[0].x),
                fmt_mm(flip(pts[0].y)),
                fmt_mm(pts[1].x),
                fmt_mm(flip(pts[1].y)),
                fmt_mm_trim(*width),
            ));
        }
        (StyleClass::BinDivider, Shape::Polyline { pts, width }) if pts.len() == 2 => {
            out.push_str(&format!(
                "{indent}<line class=\"bin-divider\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#888\" stroke-width=\"{}\" stroke-dasharray=\"1,1\"/>\n",
                fmt_mm(pts[0].x),
                fmt_mm(flip(pts[0].y)),
                fmt_mm(pts[1].x),
                fmt_mm(flip(pts[1].y)),
                fmt_mm_trim(*width),
            ));
        }
        (StyleClass::Header, Shape::Text(run)) => {
            // The header historically carries no text-anchor (its justify is Start).
            out.push_str(&format!(
                "{indent}<text class=\"header\" x=\"{}\" y=\"{}\" font-size=\"{}\">{}</text>\n",
                fmt_mm(run.at.x),
                fmt_mm(flip(run.at.y)),
                fmt_mm(run.height),
                xml_escape(&run.text),
            ));
        }
        (StyleClass::BinLabel, Shape::Text(run)) => {
            out.push_str(&format!(
                "{indent}<text class=\"bin-label\" x=\"{}\" y=\"{}\" font-size=\"{}\" fill=\"#888\">{}</text>\n",
                fmt_mm(run.at.x),
                fmt_mm(flip(run.at.y)),
                fmt_mm(run.height),
                xml_escape(&run.text),
            ));
        }
        (StyleClass::PinName, Shape::Text(run)) => {
            emit_anchored_text(out, indent, "pin", run, flip)
        }
        (StyleClass::NetTag, Shape::Text(run)) => emit_anchored_text(out, indent, "tag", run, flip),
        (StyleClass::NcMark, Shape::Text(run)) => emit_anchored_text(out, indent, "nc", run, flip),
        // No other (class, shape) pairing is produced today (discs arrive with junction
        // dots, gw-26); an unknown pairing serializes nothing rather than guessing.
        _ => {}
    }
}

/// A `<text>` with an explicit `text-anchor` (pin names, tags, nc marks).
fn emit_anchored_text(
    out: &mut String,
    indent: &str,
    class: &str,
    run: &TextRun,
    flip: &impl Fn(Nm) -> Nm,
) {
    let anchor = match run.justify {
        TextJustify::Start => "start",
        TextJustify::End => "end",
    };
    out.push_str(&format!(
        "{indent}<text class=\"{class}\" x=\"{}\" y=\"{}\" font-size=\"{}\" text-anchor=\"{anchor}\">{}</text>\n",
        fmt_mm(run.at.x),
        fmt_mm(flip(run.at.y)),
        fmt_mm(run.height),
        xml_escape(&run.text),
    ));
}

/// nm → mm with trailing zeros trimmed (`100000` → `"0.1"`), for stroke widths — the
/// historical short form (`stroke-width="0.1"`, not `"0.100000"`).
fn fmt_mm_trim(nm: Nm) -> String {
    let s = fmt_mm(nm);
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::history::History;
    use crate::part::part_library;
    use crate::schematic::HEADER_TEXT_H;

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
        // The header carries the *annotated* refdes, not the raw instance path: the test
        // lib's `Cap` class prefixes with its own name, so `C1`/`C2` annotate to
        // `Cap1`/`Cap2` (the `data-id` group still keys on the path).
        assert!(
            s.contains(">Cap1 (Cap)<"),
            "refdes+name header expected:\n{s}"
        );
        // The net tag is drawn at the connected pin (§20c: tags are the default rendering).
        assert!(s.contains("class=\"tag\""), "net tag expected:\n{s}");
        assert!(s.contains(">VCC<"), "net name tag expected:\n{s}");
    }

    /// The rendered horizontal bounds `[left, left + header_width(text)]` of each
    /// `class="header"` `<text>`, in mm, in document order — the geometry a reader sees for
    /// the `refdes (Part)` labels.
    fn header_bounds(svg: &str) -> Vec<(f64, f64)> {
        let mm_per_nm = crate::doc::MM as f64;
        svg.match_indices("class=\"header\"")
            .map(|(i, _)| {
                let tail = &svg[i..];
                let x_key = "x=\"";
                let xs = tail.find(x_key).unwrap() + x_key.len();
                let xe = tail[xs..].find('"').unwrap() + xs;
                let left: f64 = tail[xs..xe].parse().unwrap();
                let ts = tail.find('>').unwrap() + 1;
                let te = tail[ts..].find("</text>").unwrap() + ts;
                let text = &tail[ts..te];
                let width_mm = crate::schematic::header_width(text) as f64 / mm_per_nm;
                (left, left + width_mm)
            })
            .collect()
    }

    /// Regression (header-overlap fix): two adjacent single-column parts packed with **no
    /// gap** must not have overlapping rendered headers. Before the fix the box was sized to
    /// a floor of the part name alone, so the far wider `Cap1 (Cap)` header spilled onto the
    /// neighbour; now the flow reserves `header_width` per symbol, so the headers are at
    /// worst edge-to-edge.
    #[test]
    fn adjacent_headers_do_not_intersect() {
        let s = render(
            "inst C1 Cap\ninst C2 Cap\nschematic {\n  row {\n    sym C1\n    sym C2\n  }\n}\n",
        );
        let mut bounds = header_bounds(&s);
        assert_eq!(bounds.len(), 2, "two headers expected:\n{s}");
        bounds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (_, right0) = bounds[0];
        let (left1, _) = bounds[1];
        assert!(
            right0 <= left1,
            "adjacent headers overlap: {:?} then {:?} (right {right0} > left {left1})\n{s}",
            bounds[0],
            bounds[1],
        );
    }

    /// The layout-time header nominal ([`header_width`]) must be an **upper bound** on the
    /// stroke font's real per-glyph advance at [`HEADER_TEXT_H`] (`GLYPH_ADVANCE / CELL_HEIGHT`
    /// of the text height). Otherwise the reserved slot would understate a header the GUI
    /// stroke-renders and let two neighbours collide — the bug this slice fixes. Pins the two
    /// constants together so a change to either can't silently reintroduce the overlap.
    #[test]
    fn header_width_upper_bounds_the_stroke_advance() {
        use crate::font::{CELL_HEIGHT, GLYPH_ADVANCE};
        let stroke_advance_per_char = GLYPH_ADVANCE as i64 * HEADER_TEXT_H / CELL_HEIGHT as i64;
        // header_width of a one-char string is exactly the per-char nominal.
        let nominal_per_char = crate::schematic::header_width("M");
        assert!(
            nominal_per_char >= stroke_advance_per_char,
            "header nominal {nominal_per_char} < stroke advance {stroke_advance_per_char} at \
             HEADER_TEXT_H={HEADER_TEXT_H}: reserved header slots would understate the drawn text",
        );
    }

    #[test]
    fn header_shows_annotated_refdes_not_the_path() {
        // Two caps under non-refdes instance paths (`CA`, `CB`): the annotator assigns
        // `Cap1`/`Cap2` (the test lib's `Cap` class prefixes with its own name), but a
        // `refdes` pin overrides the second to `C7`. The header must read the annotated
        // designator (`C7`), proving it wires the annotate query rather than echoing the
        // instance path.
        let s = render(
            "inst CA Cap\ninst CB Cap\nrefdes CB C7\nschematic {\n  row {\n    sym CA\n    sym CB\n  }\n}\n",
        );
        assert!(
            s.contains(">Cap1 (Cap)<"),
            "auto refdes header expected:\n{s}"
        );
        assert!(
            s.contains(">C7 (Cap)<"),
            "pinned refdes header expected (not the path `CB`):\n{s}"
        );
        assert!(
            !s.contains(">CB (Cap)<"),
            "header must not echo the instance path when a refdes exists:\n{s}"
        );
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

    /// The byte oracle for the Decision-23 rewire: the `examples/schematic.rs` document
    /// (same source text) must render **byte-identically** to the golden fixture captured
    /// from the pre-rewire renderer at `f6fda2f`. Guards the whole serialization surface —
    /// viewBox math, grouping, attribute layout, number formatting, text baselines. (The
    /// tracked `poc/out/schematic.svg`, regenerated by `poc_multiprobe`, is the second,
    /// larger golden.) If a deliberate drawing-convention change ever lands, regenerate
    /// the fixture in the same commit and say so.
    #[test]
    fn example_matches_pre_rewire_golden() {
        // Kept verbatim from examples/schematic.rs (the demo doc: LDO + MCU + two caps,
        // one straight wire, one waypoint wire, U1 deliberately unplaced).
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
        let golden = include_str!("schematic_svg/golden_example.svg");
        assert_eq!(
            render(src),
            golden,
            "schematic SVG must be byte-identical to the pre-rewire golden"
        );
    }
}
