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
//! and an Excellon drill program ([`excellon_drill`]) — and a **fab-drawing SVG** pass
//! ([`svg_fab`] / [`fab_svg_set`], Decision 15: one SVG per authored `Role::Datum` slab,
//! the consumer that lets an authored fab slab actually render). Now that routing writes real
//! copper into the `Doc` (traces with width, vias with pad+drill) and footprint pads
//! carry render geometry, the fab artifacts describe genuine copper. All coordinates
//! flow from integer nanometres into each format by integer arithmetic (the Gerber
//! `%FSLAX46Y46*%` fixed-point format *is* nanometres — see [`gbr_coord`]), so the
//! determinism invariant holds end to end. See docs/architecture.md, "Prototype
//! status (Gerber/fab output)".

use crate::doc::{Doc, MM, Nm, Point};
use crate::geom::{
    DEFAULT_CHORD_TOL, Extent, Path, Role, Seg, Shape2D, Slab, Stackup, ZRange, circumcenter,
};
use crate::part::{PartDef, PartLib, pin_world};
use crate::region::Region;
use crate::route::{Layer, Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

/// Format a fixed-point nanometre coordinate as a millimetre decimal string with
/// exactly six fractional digits. Pure integer arithmetic — no float, so the
/// fixed-point determinism invariant is preserved end to end (e.g. `-2_000_000` ->
/// `"-2.000000"`, `1_325_000` -> `"1.325000"`).
pub(crate) fn fmt_mm(nm: Nm) -> String {
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
        let pins: Vec<String> = net
            .members
            .iter()
            .map(|p| format!("{}.{}", p.comp, p.pin))
            .collect();
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
    out.push_str("ref,part,x_mm,y_mm,rotation_deg,side\n");
    for c in doc.components.values() {
        // KiCad .pos convention: report the *authored* about-z angle with the side
        // marked separately, so a plain bottom flip is `0,B` (not `180,B`). Raw
        // `to_deg()` would couple the flip axis into the angle and misrotate parts on a
        // P&P line, so un-flip bottom parts first. `flipped()∘flipped() = −q` and
        // `to_deg` is negation-invariant, so this recovers the authored angle exactly.
        let base = if c.orient.is_bottom() {
            c.orient.flipped()
        } else {
            c.orient
        };
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            c.id,
            c.part,
            fmt_mm(c.pos.value.x),
            fmt_mm(c.pos.value.y),
            base.to_deg(),
            if c.orient.is_bottom() { "B" } else { "T" },
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
            // Issue 0029: skip a signal *bound* to a real pad (`iface.pads`). Its pad is
            // already enumerated by number above — the bound signal *is* that pad (the pin
            // identity `iface_infer` nets under). Enumerating `port.signal` too would draw
            // the same physical pin twice: once as real pad copper (by number) and once as
            // the 0.3 mm fallback dot (`port.signal` finds no `PinDef`), so the fallback
            // painted a spurious duplicate dot over the copper. An abstract (unbound)
            // signal keeps its `port.signal` identity — it has no colliding pad.
            if iface.pads.contains_key(sig) {
                continue;
            }
            ids.push(format!("{port}.{sig}"));
        }
    }
    ids
}

/// Minimal XML text escaping for labels.
pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A board sketch as deterministic SVG: the board outline (the source `Board`
/// directive if present, else the bounding box of placed geometry), each component
/// drawn at its position with its pin pads (via [`pin_world`]) and an id label.
///
/// The model's y axis points up (ECAD convention); SVG's points down, so y is
/// flipped within the content bounds to keep the sketch upright. All coordinates
/// are six-decimal mm via [`fmt_mm`]; element order follows `EntityId` order. No
/// timestamps or other ambient state — byte-stable and diffable.
pub fn svg(doc: &Doc, lib: &PartLib) -> Result<String, String> {
    const MARGIN: Nm = 2 * MM;

    // The board outline carried by tier-1 source, if any (last `Board` wins, as in
    // elaboration). Routed copper (traces/vias) is drawn on top of this outline and
    // the placed pads.
    let board = source_board(doc);
    // The stackup, resolved once — the render uses it to class copper/silk by z
    // (Decision 13: the side/layer is a forward query over slab names, never stored).
    let su = crate::elaborate::stackup(&doc.source);

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
            // Footprint silk can extend past the pads — keep its extent in view.
            for g in &def.graphics {
                pts.extend(g.shape.map_points(|p| crate::part::to_world(c, p)).points());
            }
        }
    }
    if let Some((min, max)) = board.as_ref().and_then(Region::bbox) {
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
    // Silk markings (lowered board text) must be in view so labels aren't clipped.
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role == Role::Marking {
            let Extent::Prism { shape, .. } = &nf.feature.extent;
            pts.extend(shape.points());
        }
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

    // A pad's real copper as a *filled* shape (the footprint's honest extent),
    // mirroring `svg_outline`: a curve-aware `<path>` when the boundary bends
    // (rounded / oval pads), else the byte-identical `<polygon>`. No explicit fill
    // (defaults to black, like the legacy pad dot) and tagged `class="pad"` so the
    // existing structure/classes hold. Indented one level deeper than outlines since
    // pads live inside a component `<g>`.
    let svg_pad = |shape: &Shape2D| -> String {
        if has_curve(shape) {
            format!(
                "    <path class=\"pad\" d=\"{}\"/>\n",
                svg_path_d(shape, &flip)
            )
        } else {
            let pts: Vec<String> = shape
                .points()
                .iter()
                .map(|p| format!("{},{}", fmt_mm(p.x), fmt_mm(flip(p.y))))
                .collect();
            format!(
                "    <polygon class=\"pad\" points=\"{}\"/>\n",
                pts.join(" ")
            )
        }
    };
    // The board region (outline ∖ cutouts) as a single even-odd `<path>` — every ring
    // (outer + cutout holes) stroked, holes read as voids for fill (Decision 16a). The
    // rings are polygonized, so a curved board edge draws as a fine polyline (Decision
    // 16b). Falls back to the implicit bounding box when the source carries no board.
    match &board {
        Some(region) => {
            out.push_str(&format!(
                "  <path class=\"outline-board\" d=\"{}\" fill=\"none\" fill-rule=\"evenodd\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                region_svg_d(region, &flip)
            ));
        }
        None => {
            let (bx0, by0, bx1, by1) = (x0 + MARGIN, y0 + MARGIN, x1 - MARGIN, y1 - MARGIN);
            out.push_str(&format!(
                "  <rect class=\"outline-bbox\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                fmt_mm(bx0),
                fmt_mm(flip(by1)),
                fmt_mm(bx1 - bx0),
                fmt_mm(by1 - by0),
            ));
        }
    }

    // Through-cut voids (authored `hole` NPTH drills, Decision 16b): a source-level
    // `Role::Void` feature is a physical hole in the board a human reading the sketch
    // must see (the outline path above draws only outline ∖ cutouts, not standalone
    // voids). Each draws as an outlined circle at its center/radius. Pad/via drill
    // `Void`s live in `world_features`, not the source-only stream read here, so this is
    // exactly the authored holes — no double-draw of plated barrels.
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role != Role::Void {
            continue;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        let r = shape.radius();
        for c in shape.points() {
            if r > 0 {
                out.push_str(&format!(
                    "  <circle class=\"hole\" cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                    fmt_mm(c.x),
                    fmt_mm(flip(c.y)),
                    fmt_mm(r),
                ));
            }
        }
    }

    // Copper pour fills, under the components/traces: one translucent `<path>` per
    // pour (outer + hole subpaths, even-odd fill so knockouts read as voids), in the
    // pour's layer colour. Deterministic (pours iterate in source/net/layer order).
    for pf in pours_of(doc, lib) {
        let d = region_svg_d(&pf.fill, &flip);
        if !d.is_empty() {
            out.push_str(&format!(
                "  <path class=\"pour pour-{}\" data-net=\"{}\" d=\"{}\" fill=\"{}\" fill-opacity=\"0.25\" fill-rule=\"evenodd\" stroke=\"none\"/>\n",
                layer_class(&su, &pf.layer),
                xml_escape(&pf.net.0),
                d,
                layer_color(&su, &pf.layer),
            ));
        }
    }

    // (`su`, resolved above, resolves each pad's layer-relative copper to absolute z, so
    // pads fan out correctly — a through-hole pad becomes one conductor feature per
    // copper slab.)
    // Footprint auto-text (Decision 14): `refdes` is a whole-document query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);
    // Doc-wide outline font (Decision 17), resolved once; `None` ⇒ the stroke font.
    let font = crate::elaborate::resolve_font(&doc.source);

    // One group per component: pads, an origin marker, and an id label.
    for (id, c) in &doc.components {
        out.push_str(&format!(
            "  <g class=\"component\" data-id=\"{}\">\n",
            xml_escape(c.id.as_str())
        ));
        if let Some(def) = lib.get(&c.part) {
            for id in part_pin_ids(def) {
                // Real pad copper when the pin carries a footprint pad: each
                // `Role::Conductor` region's world `Shape2D`, drawn as filled copper.
                // A through pad fans out to one identical conductor per copper slab and
                // this top-down sketch shows them as a single outline, so we draw each
                // distinct shape once. Non-conductor features (the drill `Void`) are
                // skipped. Pins with no pad — toy-library pins and interface ports
                // (which are not `PinDef`s) — keep a small fallback dot at the pin's
                // world point so they stay visible.
                let mut shapes: Vec<Shape2D> = Vec::new();
                if let Some(pin) = def.pins.iter().find(|p| p.number == id) {
                    for f in pin.pad_features(c, &su) {
                        if f.role != Role::Conductor {
                            continue;
                        }
                        let Extent::Prism { shape, .. } = f.extent;
                        if !shapes.contains(&shape) {
                            shapes.push(shape);
                        }
                    }
                }
                if shapes.is_empty() {
                    if let Some(w) = pin_world(c, def, &id) {
                        out.push_str(&format!(
                            "    <circle class=\"pad\" cx=\"{}\" cy=\"{}\" r=\"0.3\"/>\n",
                            fmt_mm(w.x),
                            fmt_mm(flip(w.y)),
                        ));
                    }
                } else {
                    for shape in &shapes {
                        out.push_str(&svg_pad(shape));
                    }
                }
            }
        }
        // Footprint silkscreen: each derived `Role::Marking` graphic (side-swapped +
        // placed by `graphic_features`) drawn as a silk stroke, same look as board text.
        // Footprint auto-text (`text_features`) rides the same Marking-filtered path.
        if let Some(def) = lib.get(&c.part) {
            let rd = refdes.get(id).map(String::as_str).unwrap_or("");
            let lbl = crate::annotate::label(c, def, &reg);
            let graphics = crate::part::graphic_features(def, c, &su);
            let texts = crate::part::text_features(def, c, &su, rd, &lbl, font.as_ref());
            for f in graphics.into_iter().chain(texts) {
                if f.role != Role::Marking {
                    continue;
                }
                let Extent::Prism { shape, z } = &f.extent;
                out.push_str(&svg_silk(shape, &flip, is_bottom_side(&su, z)));
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
            layer_class(&su, &t.layer),
            tid,
            path.join(" "),
            layer_color(&su, &t.layer),
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

    // Silkscreen: lowered board text. Each derived stroke-font `Role::Marking` feature
    // (from the converged `features` view) is drawn as a thin stroked centreline
    // polyline, giving a silk-layer look. Source order ⇒ deterministic.
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role != Role::Marking {
            continue;
        }
        let Extent::Prism { shape, z } = &nf.feature.extent;
        out.push_str(&svg_silk(shape, &flip, is_bottom_side(&su, z)));
    }

    out.push_str("</svg>\n");
    Ok(out)
}

/// One silkscreen `Role::Marking` shape in the silk-layer look. Shared by lowered
/// board text (always strokes) and footprint graphics. A [`Shape2D::Stroke`]
/// (`fp_line`/`fp_arc`/text) draws as a thin stroked centreline polyline whose pen is
/// the shape's inflation diameter (`radius * 2`); a [`Shape2D::Polygon`]
/// (`fp_poly`/`fp_rect`) is a *filled* area, so it draws as a closed filled polygon —
/// rendering it as a centreline polyline would emit `stroke-width = radius*2 = 0` and
/// vanish.
///
/// `bottom` splits the two silk sides visually (following the copper `layer_class` /
/// `layer_color` convention): top silk keeps class `silk` / a mid grey, bottom silk gets
/// class `silk-bottom` / a lighter grey so a two-sided board reads apart in one top view.
fn svg_silk(shape: &Shape2D, flip: &impl Fn(Nm) -> Nm, bottom: bool) -> String {
    let (class, color) = if bottom {
        ("silk-bottom", "#c8c8c8")
    } else {
        ("silk", "#888888")
    };
    svg_surface(shape, flip, class, color)
}

/// One derived surface shape in the marking look, in the given `class`/`color`. Shared
/// by silk ([`svg_silk`]) and the fab drawing ([`svg_fab`]) — the shape-arm handling is
/// identical, only the class/colour differ. A [`Shape2D::Stroke`] (`fp_line`/`fp_arc`/
/// text) draws as a thin stroked centreline polyline whose pen is the shape's inflation
/// diameter (`radius * 2`); a [`Shape2D::Polygon`] (`fp_poly`/`fp_rect`) is a *filled*
/// area, so it draws as a closed filled polygon — rendering it as a centreline polyline
/// would emit `stroke-width = radius*2 = 0` and vanish; a [`Shape2D::Area`] (TTF outline
/// text) is an even-odd `<path>` so its counters read as voids.
fn svg_surface(shape: &Shape2D, flip: &impl Fn(Nm) -> Nm, class: &str, color: &str) -> String {
    let coords: Vec<String> = shape
        .points()
        .iter()
        .map(|p| format!("{},{}", fmt_mm(p.x), fmt_mm(flip(p.y))))
        .collect();
    match shape {
        Shape2D::Polygon { .. } => format!(
            "  <polygon class=\"{class}\" points=\"{}\" fill=\"{color}\" stroke=\"none\"/>\n",
            coords.join(" "),
        ),
        Shape2D::Stroke { .. } => format!(
            "  <polyline class=\"{class}\" points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"{}\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n",
            coords.join(" "),
            fmt_mm(shape.radius() * 2),
        ),
        Shape2D::Area { region } => format!(
            "  <path class=\"{class}\" d=\"{}\" fill=\"{color}\" fill-rule=\"evenodd\" stroke=\"none\"/>\n",
            region_svg_d(region, flip),
        ),
    }
}

/// Is a surface feature at z-range `z` on the **bottom** side? A slab outboard of (at or
/// below) the bottom copper is bottom-side (silk or fab); anything else is top. A forward
/// query against the stackup — the side is derived from z, never stored (Decision 13).
/// Falls back to top when there is no bottom copper to compare against.
fn is_bottom_side(su: &Stackup, z: &ZRange) -> bool {
    su.bottom_copper().is_some_and(|bot| z.hi <= bot.lo)
}

/// Which outer copper side a copper slab **name** sits on, for render-only classing:
/// top-most copper → `Top`, bottom-most → `Bottom`, anything between → `Inner`. A
/// forward query against the stackup (Decision 13); an unknown name falls back to `Top`.
fn copper_side(su: &Stackup, name: &str) -> Layer {
    let cu = su.copper_slabs();
    match cu.iter().position(|s| s.name == name) {
        Some(0) => Layer::Top,
        Some(i) if i + 1 == cu.len() => Layer::Bottom,
        Some(i) => Layer::Inner((i - 1) as u8),
        None => Layer::Top,
    }
}

/// SVG class suffix / stroke colour for a copper slab name (Top warm, Bottom cool,
/// inner green) — render-only, just enough to tell the layers apart by eye.
fn layer_class(su: &Stackup, name: &str) -> &'static str {
    match copper_side(su, name) {
        Layer::Top => "top",
        Layer::Bottom => "bottom",
        Layer::Inner(_) => "inner",
    }
}
fn layer_color(su: &Stackup, name: &str) -> &'static str {
    match copper_side(su, name) {
        Layer::Top => "#cc0000",
        Layer::Bottom => "#0066cc",
        Layer::Inner(_) => "#00aa00",
    }
}

// ---- 4. Gerber (RS-274X) + Excellon drill (fab output) ----

/// The board as a filled [`Region`] (outline ∖ cutouts) carried by tier-1 source (the
/// shared reader), if any. Rounded/concave outlines and cutouts alike — polygonized by
/// the region kernel (Decision 16b), so a curved board edge draws as a fine polyline.
fn source_board(doc: &Doc) -> Option<Region> {
    crate::elaborate::board_region(&doc.source)
}

/// Emit a filled [`Region`] as one RS-274X `G36`/`G37` region block: each ring is a
/// closed contour (`D02` move + `D01` draws, re-closing to the first point), so a hole
/// ring nested in an outer reads as a void under the region fill rule. Rings with < 3
/// points are skipped; the whole block is omitted if none qualify. All draws are
/// straight (a region is already polygonized). Shared by the copper-pour fills and the
/// [`Shape2D::Area`] arms — the one place ring-to-Gerber lives.
fn gerber_region_fill(region: &Region, out: &mut String) {
    if region.rings.iter().all(|r| r.len() < 3) {
        return;
    }
    out.push_str("G36*\n");
    // Region contours are straight; force linear interpolation so the block is correct
    // regardless of any preceding arc-mode state (self-contained — callers need not
    // reset). Idempotent when already G01.
    out.push_str("G01*\n");
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().chain(ring.first()).enumerate() {
            let op = if i == 0 { "D02" } else { "D01" };
            out.push_str(&format!("X{}Y{}{}*\n", gbr_coord(p.x), gbr_coord(p.y), op));
        }
    }
    out.push_str("G37*\n");
}

/// The SVG path `d` for a filled [`Region`]: every ring as an `M …L …Z` subpath. Paired
/// with `fill-rule="evenodd"` so hole rings read as voids. `flip` maps board-y into the
/// SVG (downward) frame.
fn region_svg_d(region: &Region, flip: &impl Fn(Nm) -> Nm) -> String {
    let mut d = String::new();
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().enumerate() {
            let cmd = if i == 0 { "M" } else { "L" };
            d.push_str(&format!("{cmd}{},{} ", fmt_mm(p.x), fmt_mm(flip(p.y))));
        }
        d.push_str("Z ");
    }
    d.trim_end().to_string()
}

/// The membership netlist from the materialized nets (roles are irrelevant to the
/// geometry producer). The bridge every exporter uses to feed the unified
/// [`crate::route::world_features`] / [`crate::route::pours`] queries.
fn doc_netlist(
    doc: &Doc,
) -> BTreeMap<crate::id::NetId, Vec<(crate::doc::PinRef, crate::part::PinRole)>> {
    use crate::part::PinRole;
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|pr| (pr.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}

/// The derived copper-pour fills, for export — the [`crate::route::pours`] view over the
/// unified feature stream (the same `Shape2D::Area` conductor features DRC sees). Pure —
/// same inputs, same fills.
fn pours_of(doc: &Doc, lib: &PartLib) -> Vec<crate::route::Pour> {
    let su = crate::elaborate::stackup(&doc.source);
    crate::route::pours(
        doc,
        lib,
        &doc_netlist(doc),
        &crate::route::DesignRules::default(),
        &su,
    )
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
    (
        Point {
            x: x0 - MARGIN,
            y: y0 - MARGIN,
        },
        Point {
            x: x1 + MARGIN,
            y: y1 + MARGIN,
        },
    )
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

/// A flashable aperture for a world-frame pad copper [`Shape2D`], with its centre: a
/// disc → `Circle`, a capsule → `Obround`, a polygon → its bounding `Rect`. Gerber's
/// basic apertures have no rounded-rect or rotated/custom shape, so those collapse to
/// the bounding box — a conservative copper flash at this (render-only) fidelity; the
/// exact geometry lives in the model for DRC. `None` for an empty shape.
fn shape_flash(s: &Shape2D) -> Option<(Point, Aperture)> {
    let (min, max) = s.bbox()?;
    let center = Point {
        x: (min.x + max.x) / 2,
        y: (min.y + max.y) / 2,
    };
    let (w, h) = (max.x - min.x, max.y - min.y);
    let ap = match s {
        Shape2D::Stroke { path, radius } if path.segs.is_empty() => Aperture::Circle(2 * radius),
        Shape2D::Stroke { .. } => Aperture::Obround(w, h),
        Shape2D::Polygon { .. } => Aperture::Rect(w, h),
        // A pad's copper is never an `Area` — pads are discs/capsules/polygons. An `Area`
        // (board/pour/glyph) is a filled region drawn via `gerber_region_fill`, not flashed.
        Shape2D::Area { .. } => unreachable!("Shape2D::Area is not a flashable pad aperture"),
    };
    Some((center, ap))
}

/// A Gerber coordinate in the `%FSLAX46Y46*%` fixed-point format: 4 integer + 6
/// fractional digits of millimetre, leading zeros omitted. Because 1 mm =
/// 1_000_000 nm, the integer the file carries *is exactly the nanometre value* — so
/// this is just the integer, formatted with no float anywhere.
fn gbr_coord(nm: Nm) -> String {
    nm.to_string()
}

/// Round `num/den` to the nearest integer (half away from zero), for either sign of
/// `den`. Exact i128 ⇒ byte-stable across platforms (no float).
fn rdiv(num: i128, den: i128) -> i128 {
    let (n, d) = if den < 0 { (-num, -den) } else { (num, den) };
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

/// Does this shape's skeleton contain any curved edge (arc or Bézier)? Straight shapes
/// keep their exact legacy export (polygon / G01 lines); only curve-bearing shapes take
/// the curve-aware `<path>` / contour route.
fn has_curve(s: &Shape2D) -> bool {
    s.path().segs.iter().any(|seg| {
        matches!(
            seg,
            Seg::Arc { .. } | Seg::Quadratic { .. } | Seg::Cubic { .. }
        )
    })
}

/// `mid` and `end` re-expressed relative to `start` (so `start` becomes the origin).
/// All arc predicates work in this frame: translation-invariant, but the degree-4 side
/// test then scales with the board *extent* (the arc's own span, ~cm) rather than the
/// absolute coordinate magnitude, keeping the i128 arithmetic far from overflow even
/// for a board referenced far from the origin.
fn rel_to_start(start: Point, mid: Point, end: Point) -> (Point, Point) {
    (
        Point {
            x: mid.x - start.x,
            y: mid.y - start.y,
        },
        Point {
            x: end.x - start.x,
            y: end.y - start.y,
        },
    )
}

/// The Gerber arc I/J `(centre − start)` (rounded to nm) and turn (`+1` CCW / `−1` CW)
/// of the 3-point arc `start`→`mid`→`end`. Since the arc's start *is* the current point,
/// the start-relative centre is exactly the I/J offset Gerber wants. `None` if collinear
/// (caller draws a straight line). Exact-rational [`circumcenter`], [`rdiv`]-rounded —
/// byte-stable.
fn arc_ij_turn(start: Point, mid: Point, end: Point) -> Option<(Point, i32)> {
    let (b, c) = rel_to_start(start, mid, end);
    let (ux, uy, den) = circumcenter(Point { x: 0, y: 0 }, b, c);
    if den == 0 {
        return None;
    }
    let ij = Point {
        x: rdiv(ux, den) as Nm,
        y: rdiv(uy, den) as Nm,
    };
    Some((ij, den.signum() as i32))
}

/// SVG elliptical-arc parameters `(radius, large_arc_flag, sweep_flag)` for the arc
/// `start`→`mid`→`end`. The flags are computed **exactly** (integer predicates, in the
/// start-relative frame so they can't overflow at board scale), so the SVG is
/// byte-stable; only the radius uses correctly-rounded `sqrt`.
///
/// - `sweep`: SVG's y axis points *down* (we emit flipped y), which reverses turn
///   handedness, so a model-CCW arc (`turn > 0`) is a screen-CW arc ⇒ `sweep = 0`, and
///   model-CW ⇒ `sweep = 1`.
/// - `large_arc`: 1 iff the sweep exceeds 180°, i.e. the centre and `mid` lie on the
///   **same** side of the chord `start`→`end` (for a minor arc they are on opposite
///   sides; a semicircle puts the centre on the chord ⇒ 0).
fn svg_arc_params(start: Point, mid: Point, end: Point) -> Option<(Nm, u8, u8)> {
    let (b, c) = rel_to_start(start, mid, end); // origin, b=mid, c=end
    let (ux, uy, den) = circumcenter(Point { x: 0, y: 0 }, b, c);
    if den == 0 {
        return None;
    }
    // Centre is start-relative, so radius = |centre − start| = |(cx, cy)|.
    let (cx, cy) = (ux as f64 / den as f64, uy as f64 / den as f64);
    let radius = (cx * cx + cy * cy).sqrt().round() as Nm;
    let sweep: u8 = if den < 0 { 1 } else { 0 };
    // Side of the chord (origin→c) that `mid` (= b) and the centre fall on.
    let side_mid = c.x as i128 * b.y as i128 - c.y as i128 * b.x as i128;
    let num = c.x as i128 * uy - c.y as i128 * ux;
    let side_c = num.signum() * den.signum();
    let large: u8 = if side_mid.signum() == side_c && side_mid != 0 {
        1
    } else {
        0
    };
    Some((radius, large, sweep))
}

/// Build an SVG path `d` for a closed `shape`, walking its skeleton so arc edges become
/// `A` commands (and straight edges `L`). `flip` lifts model-y (up) to SVG-y (down).
fn svg_path_d(shape: &Shape2D, flip: &impl Fn(Nm) -> Nm) -> String {
    let path = shape.path();
    let mut d = format!("M {},{}", fmt_mm(path.start.x), fmt_mm(flip(path.start.y)));
    let mut cur = path.start;
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => {
                d.push_str(&format!(" L {},{}", fmt_mm(end.x), fmt_mm(flip(end.y))));
            }
            Seg::Arc { mid, end } => match svg_arc_params(cur, *mid, *end) {
                Some((r, large, sweep)) => d.push_str(&format!(
                    " A {} {} 0 {} {} {},{}",
                    fmt_mm(r),
                    fmt_mm(r),
                    large,
                    sweep,
                    fmt_mm(end.x),
                    fmt_mm(flip(end.y)),
                )),
                None => d.push_str(&format!(" L {},{}", fmt_mm(end.x), fmt_mm(flip(end.y)))),
            },
            // Béziers export directly — SVG carries them losslessly. Control points are
            // y-flipped alongside the endpoints.
            Seg::Quadratic { ctrl, end } => d.push_str(&format!(
                " Q {},{} {},{}",
                fmt_mm(ctrl.x),
                fmt_mm(flip(ctrl.y)),
                fmt_mm(end.x),
                fmt_mm(flip(end.y)),
            )),
            Seg::Cubic { c1, c2, end } => d.push_str(&format!(
                " C {},{} {},{} {},{}",
                fmt_mm(c1.x),
                fmt_mm(flip(c1.y)),
                fmt_mm(c2.x),
                fmt_mm(flip(c2.y)),
                fmt_mm(end.x),
                fmt_mm(flip(end.y)),
            )),
        }
        cur = seg.end();
    }
    d.push_str(" Z");
    d
}

/// Walk `path`'s skeleton emitting a `D02` move-to-start then a draw per segment
/// (`G01` line, `G02`/`G03` multi-quadrant arc, or a flattened Bézier run) into `out`.
/// `mode` tracks the current interpolation code and `g75` whether multi-quadrant has
/// been enabled, so a straight-only path emits no spurious mode lines. When `close` and
/// the path does not end where it started, a straight edge back to `start` closes it.
/// Shared by the closed-contour emitter ([`gerber_contour`], `close = true`: edge cuts,
/// region fills) and the open-stroke emitter ([`gerber_stroke`], `close = false`: silk).
fn gerber_walk(path: &Path, out: &mut String, mode: &mut &str, g75: &mut bool, close: bool) {
    let start = path.start;
    out.push_str(&format!(
        "X{}Y{}D02*\n",
        gbr_coord(start.x),
        gbr_coord(start.y)
    ));
    let mut cur = start;
    let line_to = |p: Point, out: &mut String, mode: &mut &str| {
        if *mode != "G01" {
            out.push_str("G01*\n");
            *mode = "G01";
        }
        out.push_str(&format!("X{}Y{}D01*\n", gbr_coord(p.x), gbr_coord(p.y)));
    };
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => line_to(*end, out, mode),
            Seg::Arc { mid, end } => match arc_ij_turn(cur, *mid, *end) {
                Some((ij, turn)) => {
                    if !*g75 {
                        out.push_str("G75*\n");
                        *g75 = true;
                    }
                    let dir = if turn > 0 { "G03" } else { "G02" };
                    if *mode != dir {
                        out.push_str(&format!("{dir}*\n"));
                        *mode = dir;
                    }
                    // I/J is the centre relative to the arc start (= cur), which is
                    // exactly what `arc_ij_turn` returns.
                    out.push_str(&format!(
                        "X{}Y{}I{}J{}D01*\n",
                        gbr_coord(end.x),
                        gbr_coord(end.y),
                        gbr_coord(ij.x),
                        gbr_coord(ij.y),
                    ));
                }
                None => line_to(*end, out, mode),
            },
            // Gerber has no Béziers — flatten this edge to chord-tolerance G01 segments
            // (the start is the current point, already emitted; skip it).
            Seg::Quadratic { .. } | Seg::Cubic { .. } => {
                let flat = Path {
                    start: cur,
                    segs: vec![seg.clone()],
                }
                .flatten(DEFAULT_CHORD_TOL);
                for p in flat.into_iter().skip(1) {
                    line_to(p, out, mode);
                }
            }
        }
        cur = seg.end();
    }
    if close && cur != start {
        line_to(start, out, mode); // implicit straight closing edge
    }
}

/// Emit one **closed** contour of `shape` as Gerber draws — the boundary walk plus a
/// straight closing edge. Used for the `Edge.Cuts` outline and for `G36`/`G37` region
/// fills (a filled area's boundary is a closed contour).
fn gerber_contour(shape: &Shape2D, out: &mut String, mode: &mut &str, g75: &mut bool) {
    gerber_walk(shape.path(), out, mode, g75, true);
}

/// Emit an **open** stroke centreline of `shape` as Gerber draws (no closing edge). The
/// caller selects the round aperture (the stroke's pen diameter) beforehand; this only
/// walks the centreline, so silk `fp_line`/`fp_arc`/text strokes come out as real
/// draws with true arcs.
fn gerber_stroke(shape: &Shape2D, out: &mut String, mode: &mut &str, g75: &mut bool) {
    gerber_walk(shape.path(), out, mode, g75, false);
}

/// The KiCad-style layer token used in fab filenames, derived from the copper slab
/// **name** (Decision 13): `F.Cu` → `F_Cu`, `B.Cu` → `B_Cu`, `In1.Cu` → `In1_Cu`.
fn layer_file(slab: &Slab) -> String {
    slab_file(&slab.name)
}

/// The copper slabs to emit, in physical stack-up order (top-down) — every conductor
/// slab in the stackup. Component pads occupy the outer copper under the all-layer pad
/// model, and a forward per-slab query attributes each trace/via/pour by name, so the
/// full copper set is exactly the stackup's copper slabs (Decision 13 rule 3).
fn copper_layers(doc: &Doc) -> Vec<Slab> {
    let su = crate::elaborate::stackup(&doc.source);
    su.copper_slabs().into_iter().cloned().collect()
}

/// Every component pad copper region that flashes on the copper slab with z-range
/// `target_z`, as `(world centre, aperture)`, in `(EntityId, pin-declaration,
/// copper-region)` order. Each pad's real geometry is transformed to world space and
/// reduced to a flashable aperture; a region flashes only on the slabs it occupies.
/// Toy-library pins (`pad: None`) contribute nothing.
fn component_pad_flashes(doc: &Doc, lib: &PartLib, target_z: ZRange) -> Vec<(Point, Aperture)> {
    // Derive each pad's converged copper features and flash those whose slab z is this
    // Gerber slab's z. `pad_features` already world-maps + assigns z, so a Through pad
    // flashes on every copper slab and an SMD pad only on its own — a forward per-slab
    // query off the Feature model.
    let su = crate::elaborate::stackup(&doc.source);
    let mut out = Vec::new();
    for c in doc.components.values() {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for pin in &def.pins {
            for f in pin.pad_features(c, &su) {
                if f.role != Role::Conductor {
                    continue; // the Void drill does not flash on a copper layer
                }
                let Extent::Prism { shape, z } = &f.extent;
                if *z != target_z {
                    continue;
                }
                if let Some((center, ap)) = shape_flash(shape) {
                    out.push((center, ap));
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
pub fn gerber_layer(doc: &Doc, lib: &PartLib, slab: &Slab) -> String {
    let su = crate::elaborate::stackup(&doc.source);
    let cu = su.copper_slabs();
    let traces: Vec<&Trace> = doc
        .traces
        .values()
        .filter(|t| t.layer == slab.name)
        .collect();
    let vias: Vec<&Via> = doc
        .vias
        .values()
        .filter(|v| v.spans_z(&cu, &slab.z))
        .collect();
    let pads = component_pad_flashes(doc, lib, slab.z);

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
    let codes: BTreeMap<Aperture, u32> = aps
        .iter()
        .enumerate()
        .map(|(i, a)| (*a, 10 + i as u32))
        .collect();

    let mut out = String::new();
    out.push_str(&format!("G04 {} *\n", layer_file(slab)));
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
        out.push_str(&format!(
            "X{}Y{}D03*\n",
            gbr_coord(v.at.x),
            gbr_coord(v.at.y)
        ));
    }
    // Component pad flashes (all-layer model).
    for (p, a) in &pads {
        let code = codes[a];
        out.push_str(&format!("D{code}*\n"));
        out.push_str(&format!("X{}Y{}D03*\n", gbr_coord(p.x), gbr_coord(p.y)));
    }

    // Copper pour fills on this layer as RS-274X region fills. A fill's outer rings
    // and hole rings are emitted as contours inside one `G36`/`G37` block; the region
    // fill rule treats a contour nested in another as a hole, so the knockouts come
    // out as voids. (A pour fill is already a tessellated polygon, so no arcs needed.)
    for pf in pours_of(doc, lib).iter().filter(|p| p.layer == slab.name) {
        gerber_region_fill(&pf.fill, &mut out);
    }

    out.push_str("M02*\n");
    out
}

/// The `Edge.Cuts` Gerber: the board outline as a closed rectangle drawn with a thin
/// (0.1 mm) round pen. Uses the source `Board` rect, else the placement bounding box.
pub fn gerber_edge_cuts(doc: &Doc, lib: &PartLib) -> String {
    // The board region (outline ∖ cutouts); fall back to a rectangle around all geometry.
    let region = source_board(doc).unwrap_or_else(|| {
        let (min, max) = placement_bbox(doc, lib);
        crate::region::shape_to_region(
            &Shape2D::rect(
                Point {
                    x: (min.x + max.x) / 2,
                    y: (min.y + max.y) / 2,
                },
                max.x - min.x,
                max.y - min.y,
            ),
            crate::region::DEFAULT_CIRCLE_SEGS,
        )
    });
    let mut out = String::new();
    out.push_str("G04 Edge.Cuts *\n");
    out.push_str("%FSLAX46Y46*%\n");
    out.push_str("%MOMM*%\n");
    out.push_str("%ADD10C,0.100000*%\n");
    out.push_str("D10*\n");
    out.push_str("G01*\n");
    // Each ring (outer boundary, then every cutout hole) draws as a closed contour of
    // straight G01 lines. The region is polygonized, so a curved board edge or round
    // cutout comes out as a fine polyline rather than a G02/G03 arc (Decision 16b — the
    // arc is gone once the outline is a region).
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().chain(ring.first()).enumerate() {
            let op = if i == 0 { "D02" } else { "D01" };
            out.push_str(&format!("X{}Y{}{}*\n", gbr_coord(p.x), gbr_coord(p.y), op));
        }
    }
    out.push_str("M02*\n");
    out
}

/// One drilled hole gathered from the unified feature stream: a round hole at a point,
/// or a slot between two points (a routed `G85` hole). `Ord` so a tool's hits emit in a
/// canonical order (byte-stable, diffable output).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DrillKind {
    Round(Point),
    Slot(Point, Point),
}

/// The board's drilled holes, read **forward** from the unified feature stream
/// ([`crate::route::world_features`]): every full-stackup through-cut `Role::Void`, as
/// `(plated, diameter, kind)`. Three producers reach here — a pad drill, a via drill, and
/// an authored `hole` NPTH (Decision 16b, full-z by construction). This is the fix for
/// issue 0022 — the drill file is a query over the same `Void` features the solder-mask
/// export sees, so pad drills (previously omitted), via drills, and mounting holes appear.
///
/// A mask opening is a *partial-z* `Void` (at the mask slab), and a `region void` is
/// single-slab (at its slab's z) — neither is a through-cut, so both are excluded by the
/// full-z gate. An authored `hole`, by contrast, IS full-z and admitted. A void's
/// **plating** is carried by its material (Decision 16b): pad/via drills are plated (a
/// copper barrel), a material-less void (the `hole`) is NPTH. A disc void is a `Round`
/// hit; a capsule (slot) void a `Slot`. Any other drill-void shape is an un-handled seam.
fn drill_hits(doc: &Doc, lib: &PartLib) -> Vec<(bool, Nm, DrillKind)> {
    let su = crate::elaborate::stackup(&doc.source);
    let full = su.full_z();
    // `world_features` cannot fail on a committed doc (the commit-time slab gate — see
    // `route::check_drc`); an `Err` is a broken invariant, made loud rather than emitting
    // an empty (⇒ no-holes) drill program for a board that never materialised.
    let world = crate::route::world_features(
        doc,
        lib,
        &doc_netlist(doc),
        &crate::route::DesignRules::default(),
        &su,
    )
    .expect("world_features on a committed doc (slab gate enforced at commit)");
    let mut hits = Vec::new();
    for nf in world {
        if nf.feature.role != Role::Void {
            continue;
        }
        let Extent::Prism { shape, z } = &nf.feature.extent;
        if Some(*z) != full {
            continue; // not a through-cut (mask opening / single-slab authored void)
        }
        // Plated iff the drill Void carries the copper-barrel material (Decision 16b): a
        // pad/via plated through-hole. Gated on the material *name*, not merely
        // `is_some()`, so a future void with some other material (e.g. a resin-filled or
        // capped via) is not silently classified PTH — authored voids default NPTH.
        let plated = nf
            .feature
            .material
            .as_ref()
            .is_some_and(|m| m.name == "copper");
        let dia = shape.radius() * 2;
        let pts = shape.points();
        let kind = match pts.as_slice() {
            [c] => DrillKind::Round(*c),
            [a, b] => DrillKind::Slot(*a, *b),
            // A drill Void is always a disc or capsule stroke; anything else is a shape
            // no drill-lowering produces today. Leave a loud seam rather than dead code.
            _ => unimplemented!("drill Void with a non-disc/capsule shape ({pts:?})"),
        };
        hits.push((plated, dia, kind));
    }
    hits
}

/// One Excellon drill program for a set of `hits` (all one plating class). Tools are the
/// distinct diameters, sorted and numbered `T1..`; under each tool its hits emit in
/// canonical order — round holes as a coordinate, slots as a `G85` routed hole. `label`
/// names the file's plating in the header comment. Coordinates and tool sizes are
/// decimal millimetres via [`fmt_mm`]. Deterministic.
fn excellon_program(hits: &[(Nm, DrillKind)], label: &str) -> String {
    let dias: BTreeSet<Nm> = hits.iter().map(|(d, _)| *d).collect();
    let tools: BTreeMap<Nm, u32> = dias
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, 1 + i as u32))
        .collect();

    let mut out = String::new();
    out.push_str("M48\n");
    out.push_str(&format!("; Excellon drill: {label}\n"));
    out.push_str("FMAT,2\n");
    out.push_str("METRIC,TZ\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}C{}\n", t, fmt_mm(*d)));
    }
    out.push_str("%\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}\n", t));
        let mut kinds: Vec<DrillKind> = hits
            .iter()
            .filter(|(hd, _)| hd == d)
            .map(|(_, k)| *k)
            .collect();
        kinds.sort();
        for k in kinds {
            match k {
                DrillKind::Round(c) => {
                    out.push_str(&format!("X{}Y{}\n", fmt_mm(c.x), fmt_mm(c.y)));
                }
                // A slot is a routed hole: position at one end, then `G85` to the other.
                DrillKind::Slot(a, b) => {
                    out.push_str(&format!(
                        "X{}Y{}G85X{}Y{}\n",
                        fmt_mm(a.x),
                        fmt_mm(a.y),
                        fmt_mm(b.x),
                        fmt_mm(b.y)
                    ));
                }
            }
        }
    }
    out.push_str("T0\n");
    out.push_str("M30\n");
    out
}

/// The board's Excellon drill program(s), split by plating (issue 0022 / Decision 16b):
/// `board-PTH.drl` for plated through-holes (pad + via drills) and `board-NPTH.drl` for
/// non-plated holes. Each file is emitted only when it has holes, so a board with no
/// NPTH holes ships only the PTH file. `(filename, content)` pairs; deterministic.
pub fn excellon_drill(doc: &Doc, lib: &PartLib) -> Vec<(String, String)> {
    excellon_files(drill_hits(doc, lib))
}

/// Split a `(plated, diameter, kind)` hit list into the PTH / NPTH drill files, emitting
/// each only when it has holes. Factored out of [`excellon_drill`] so the split is unit-
/// testable on a synthesized hit list; the end-to-end authoring path for an NPTH hole is
/// the `hole` directive → a full-stackup material-less [`Role::Void`] (Decision 16b), and
/// the through-cut query above classifies it non-plated into `board-NPTH.drl`.
fn excellon_files(hits: Vec<(bool, Nm, DrillKind)>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (plated, label, filename) in [
        (true, "plated through-holes (PTH)", "board-PTH.drl"),
        (false, "non-plated holes (NPTH)", "board-NPTH.drl"),
    ] {
        let group: Vec<(Nm, DrillKind)> = hits
            .iter()
            .filter(|(p, _, _)| *p == plated)
            .map(|(_, d, k)| (*d, *k))
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push((filename.to_string(), excellon_program(&group, label)));
    }
    out
}

/// The solder-mask Gerber for one [`Role::Mask`] `slab`, derived **forward** from the
/// model — never recomputed from a parallel rule set, and entered by the slab's *name*,
/// not a copper-layer enum (Decision 13 / 16 stage 4). The file draws the **openings**
/// (the fab inverts to the mask coverage — a draw-the-openings convention that stays an
/// export-format detail):
///
/// - Pad openings: the [`Role::Void`] features [`PinDef::pad_features`] emits at the
///   mask slab's z (the pad copper already inflated by [`geom::MASK_EXPANSION`]) —
///   flashed as their aperture, so on the default stackup this is byte-for-byte the old
///   pad-opening output. A pad's **drill** `Void` is a through-cut at the *full* stackup
///   z, not the mask z, so it is not one of these — and it must not be: it sits inside
///   the pad opening (drawing it again would double the flash) and its home is the
///   Excellon file. Through-hole pads open every mask slab their z spans because
///   `pad_features` places an opening at each side's mask slab.
/// - Board cutouts: milled through the whole stack, so they remove mask over their whole
///   area — drawn as `G36`/`G37` region fills.
///
/// Object order is `(EntityId, pin, region)` then cutouts — fully deterministic. Fallible
/// because the cutout query runs the slab-name materialization gate (Decision 13).
pub fn gerber_mask(doc: &Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let mask_z = slab.z;

    // Pad openings: `Void`s whose z lies within this mask slab (pad_features places the
    // inflated-copper opening there). A through-cut `Void` (a drill) extends past the
    // slab and is excluded — subsumed by the opening, and belongs to the drill file.
    let mut openings: Vec<(Point, Aperture)> = Vec::new();
    for c in doc.components.values() {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for pin in &def.pins {
            for f in pin.pad_features(c, &su) {
                if f.role != Role::Void {
                    continue;
                }
                let Extent::Prism { shape, z } = &f.extent;
                // A pad opening sits within the mask slab; a through-cut (drill) does
                // not and is skipped (it is subsumed by the opening).
                if z.lo < mask_z.lo || z.hi > mask_z.hi {
                    continue;
                }
                if let Some(fa) = shape_flash(shape) {
                    openings.push(fa);
                }
            }
        }
    }
    // Board cutouts remove mask over their whole area. A cutout is now a *hole* in the
    // board region (Decision 16b/c), not a `Void` feature, so the openings come from
    // `board_region().holes()` — the CW cutout rings — as region fills. A cutout is a
    // full-stack through-cut, so it always pierces a present mask slab.
    let cutout_holes = crate::elaborate::board_region(&doc.source)
        .map(|region| region.holes())
        .unwrap_or_default();

    let mut aps: BTreeSet<Aperture> = BTreeSet::new();
    for (_, a) in &openings {
        aps.insert(*a);
    }
    let codes: BTreeMap<Aperture, u32> = aps
        .iter()
        .enumerate()
        .map(|(i, a)| (*a, 10 + i as u32))
        .collect();

    let mut out = String::new();
    out.push_str(&format!("G04 {} *\n", slab_file(&slab.name)));
    out.push_str("%FSLAX46Y46*%\n");
    out.push_str("%MOMM*%\n");
    for (a, code) in &codes {
        out.push_str(&format!("%ADD{}{}*%\n", code, a.template()));
    }
    out.push_str("G01*\n");
    for (p, a) in &openings {
        let code = codes[a];
        out.push_str(&format!("D{code}*\n"));
        out.push_str(&format!("X{}Y{}D03*\n", gbr_coord(p.x), gbr_coord(p.y)));
    }
    // Cutout openings as region fills (one G36/G37 block per cutout hole ring).
    gerber_region_fill(&cutout_holes, &mut out);
    out.push_str("M02*\n");
    Ok(out)
}

/// The KiCad-style filename token for a named slab: the slab name with `.`→`_`
/// (`F.SilkS`→`F_SilkS`), matching the `F_Cu` convention of [`layer_file`]. Names the
/// marking (silk), solder-mask, and fab Gerbers/SVGs from their resolved slab.
fn slab_file(name: &str) -> String {
    name.replace('.', "_")
}

/// Every world-frame feature of the board carrying `role`: board-level graphics/text
/// (from the converged [`crate::elaborate::features`] view) plus each placed component's
/// footprint graphics ([`crate::part::graphic_features`], side-swapped + placed) and
/// auto-text ([`crate::part::text_features`]). The single forward source of derived
/// surface geometry the mask/silk exporters, the fab SVG pass, and the SVG render share:
/// silk queries [`Role::Marking`], the fab drawing [`Role::Datum`] (Decision 15 — the
/// role is resolved from the slab, so both flow through the same producer). Fallible
/// because the board-level lowering resolves slab names (an unknown one is a hard error,
/// per Decision 13).
fn role_features(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
    role: Role,
) -> Result<Vec<crate::geom::Feature>, String> {
    let mut out: Vec<crate::geom::Feature> = Vec::new();
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role == role {
            out.push(nf.feature);
        }
    }
    // Footprint auto-text (Decision 14) rides the same role-filtered path as graphics;
    // `refdes` is a whole-document query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);
    let font = crate::elaborate::resolve_font(&doc.source);
    for (id, c) in &doc.components {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for f in crate::part::graphic_features(def, c, su) {
            if f.role == role {
                out.push(f);
            }
        }
        let rd = refdes.get(id).map(String::as_str).unwrap_or("");
        let lbl = crate::annotate::label(c, def, &reg);
        for f in crate::part::text_features(def, c, su, rd, &lbl, font.as_ref()) {
            if f.role == role {
                out.push(f);
            }
        }
    }
    Ok(out)
}

/// One derived-surface Gerber for a `role`'s [`Slab`], drawing the features of that role
/// whose z intersects the slab (forward query per slab — Decision 13). A
/// [`Shape2D::Stroke`] (`fp_line`/`fp_arc`/text) draws as its centreline with a round
/// aperture of the stroke's pen diameter (`radius * 2`); a [`Shape2D::Polygon`]
/// (`fp_poly`/`fp_rect`) is a filled area, drawn as a `G36`/`G37` region; a
/// [`Shape2D::Area`] (TTF outline text) is a `G36`/`G37` region fill. Aperture codes run
/// from 10 in `Ord` order; object order follows [`role_features`] — deterministic. Shared
/// by [`gerber_silk`] (silk markings) and [`gerber_fab`] (fab drawing) — only the queried
/// role differs, exactly as the SVG side shares [`svg_surface`]. Coordinates are
/// board-frame with no side mirroring (a bottom slab is not flipped — the fab viewer
/// flips it), matching the copper/mask/silk Gerber convention.
fn gerber_role_surface(
    doc: &Doc,
    lib: &PartLib,
    slab: &Slab,
    role: Role,
) -> Result<String, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let feats: Vec<Shape2D> = role_features(doc, lib, &su, role)?
        .into_iter()
        .filter(|f| {
            let Extent::Prism { z, .. } = &f.extent;
            z.overlaps(&slab.z)
        })
        .map(|f| {
            let Extent::Prism { shape, .. } = f.extent;
            shape
        })
        .collect();

    // Aperture table: one round aperture per distinct stroke pen diameter.
    let mut aps: BTreeSet<Aperture> = BTreeSet::new();
    for s in &feats {
        if matches!(s, Shape2D::Stroke { .. }) {
            aps.insert(Aperture::Circle(s.radius() * 2));
        }
    }
    let codes: BTreeMap<Aperture, u32> = aps
        .iter()
        .enumerate()
        .map(|(i, a)| (*a, 10 + i as u32))
        .collect();

    let mut out = String::new();
    out.push_str(&format!("G04 {} *\n", slab_file(&slab.name)));
    out.push_str("%FSLAX46Y46*%\n");
    out.push_str("%MOMM*%\n");
    for (a, code) in &codes {
        out.push_str(&format!("%ADD{}{}*%\n", code, a.template()));
    }
    out.push_str("G01*\n");
    let mut mode = "G01";
    let mut g75 = false;
    for s in &feats {
        match s {
            Shape2D::Stroke { .. } => {
                let code = codes[&Aperture::Circle(s.radius() * 2)];
                out.push_str(&format!("D{code}*\n"));
                // No modal reset here: aperture (D-code) selection does not change the
                // G01/G02/G03 interpolation mode. `gerber_walk`'s own line/arc transitions
                // emit the needed mode line, so a straight stroke after an arc still gets
                // its `G01*` (a manual reset would suppress it, drawing the line in arc
                // mode as a degenerate I0J0 arc).
                gerber_stroke(s, &mut out, &mut mode, &mut g75);
            }
            Shape2D::Polygon { .. } => {
                out.push_str("G36*\n");
                gerber_contour(s, &mut out, &mut mode, &mut g75);
                out.push_str("G37*\n");
            }
            // A filled-area marking (TTF outline text): its rings as a region fill. The
            // helper emits its own G01, leaving interpolation linear afterwards.
            Shape2D::Area { region } => {
                gerber_region_fill(region, &mut out);
                mode = "G01";
            }
        }
    }
    out.push_str("M02*\n");
    Ok(out)
}

/// One silkscreen Gerber for a marking [`Slab`]: the [`Role::Marking`] surface features
/// intersecting the slab. See [`gerber_role_surface`].
pub fn gerber_silk(doc: &Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    gerber_role_surface(doc, lib, slab, Role::Marking)
}

/// One fab-drawing Gerber for a [`Role::Datum`] `slab` (Decision 15): the fab surface
/// features intersecting the slab, emitted board-frame with no side mirroring (a `B.Fab`
/// Gerber is a document layer the viewer flips, matching bottom silk). The Gerber sibling
/// of [`svg_fab`] — same [`datum_slabs`] iteration, RS-274X instead of SVG. Empty unless a
/// fab slab is authored, so the default stackup ships no fab Gerber (Decision 15
/// contract). See [`gerber_role_surface`].
pub fn gerber_fab(doc: &Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    gerber_role_surface(doc, lib, slab, Role::Datum)
}

/// The stackup's slabs of a given `role`, ordered **top-down** (highest z first) so a
/// board's fileset lists the front side before the back (`F.SilkS` before `B.SilkS`,
/// `F.Fab` before `B.Fab`), mirroring `F_Cu`/`B_Cu` and `F_Mask`/`B_Mask` ordering.
fn role_slabs(su: &Stackup, role: Role) -> Vec<Slab> {
    let mut m: Vec<Slab> = su
        .slabs
        .iter()
        .filter(|s| s.role == role)
        .cloned()
        .collect();
    m.sort_by_key(|s| std::cmp::Reverse(s.z.hi));
    m
}

/// The marking (silk) slabs, top-down. See [`role_slabs`].
fn marking_slabs(su: &Stackup) -> Vec<Slab> {
    role_slabs(su, Role::Marking)
}

/// The fab-drawing ([`Role::Datum`]) slabs, top-down. See [`role_slabs`]. Empty unless
/// the stackup authors a fab slab (`F.Fab`/`B.Fab`) — the default stackup has none, so
/// the fab fileset is empty by default (Decision 15).
fn datum_slabs(su: &Stackup) -> Vec<Slab> {
    role_slabs(su, Role::Datum)
}

/// One fab-drawing SVG for a [`Role::Datum`] `slab` (Decision 15): the board outline for
/// context plus every [`Role::Datum`] feature whose z intersects the slab, drawn in the
/// marking look (mirroring the silk SVG pass). This is the consumer that closes the
/// documented "an authored fab slab renders nowhere" gap — footprint fab graphics/text
/// import as `F.Fab`/`B.Fab` and lower with the slab's role, then materialize here.
///
/// Unlike [`svg`] (a single top-view composite of the whole board), this is one file per
/// fab slab, so a two-sided design gets a distinct `F.Fab` and `B.Fab` drawing. Bottom
/// fab is mirrored about board-y (as a bottom drawing is normally viewed through the
/// board), matching how [`svg_silk`] visually distinguishes the two silk sides — here we
/// mirror the *geometry* since a per-side sheet is read head-on, not composited.
///
/// Fallible because the underlying feature lowering resolves slab names (Decision 13).
///
/// [`gerber_fab`] is the RS-274X sibling of this pass — same [`datum_slabs`] iteration,
/// Gerber instead of SVG (board-frame, so it does not mirror a bottom sheet the way this
/// one does).
pub fn svg_fab(doc: &Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    const MARGIN: Nm = 2 * MM;
    let su = crate::elaborate::stackup(&doc.source);
    let board = source_board(doc);
    let bottom = is_bottom_side(&su, &slab.z);

    // This slab's fab features: `Role::Datum` shapes whose z intersects the slab (the
    // same forward-per-slab query the silk Gerber uses).
    let shapes: Vec<Shape2D> = role_features(doc, lib, &su, Role::Datum)?
        .into_iter()
        .filter(|f| {
            let Extent::Prism { z, .. } = &f.extent;
            z.overlaps(&slab.z)
        })
        .map(|f| {
            let Extent::Prism { shape, .. } = f.extent;
            shape
        })
        .collect();

    // Content bounds: the board corners and every fab-feature point (so nothing clips),
    // + margin. Falls back to a 10 mm box for an empty sheet so the viewBox is never
    // degenerate — matching [`svg`].
    let mut pts: Vec<Point> = Vec::new();
    if let Some((min, max)) = board.as_ref().and_then(Region::bbox) {
        pts.push(min);
        pts.push(max);
    }
    for s in &shapes {
        pts.extend(s.points());
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
    x0 -= MARGIN;
    y0 -= MARGIN;
    x1 += MARGIN;
    y1 += MARGIN;

    // Flip board-y into the SVG (downward) frame; for a bottom sheet, also mirror x about
    // the content centre so the drawing reads as viewed from the back.
    let flip = |y: Nm| -> Nm { y0 + y1 - y };
    let mirror = |x: Nm| -> Nm { if bottom { x0 + x1 - x } else { x } };

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{} {} {} {}\">\n",
        fmt_mm(x0),
        fmt_mm(y0),
        fmt_mm(x1 - x0),
        fmt_mm(y1 - y0),
    ));

    // Board outline for context (like [`svg`]): the board region as an even-odd path, else
    // the implicit bounding box.
    match &board {
        Some(region) => {
            // Mirror each ring's x for a bottom sheet (no-op on top). Winding flips under
            // mirroring, but the fill is even-odd so crossing count — not orientation —
            // decides voids; the outline reads correctly either way.
            let region = Region::new(
                region
                    .rings
                    .iter()
                    .map(|ring| {
                        ring.iter()
                            .map(|p| Point {
                                x: mirror(p.x),
                                y: p.y,
                            })
                            .collect()
                    })
                    .collect(),
            );
            out.push_str(&format!(
                "  <path class=\"outline-board\" d=\"{}\" fill=\"none\" fill-rule=\"evenodd\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                region_svg_d(&region, &flip)
            ));
        }
        None => {
            let (bx0, by0, bx1, by1) = (x0 + MARGIN, y0 + MARGIN, x1 - MARGIN, y1 - MARGIN);
            out.push_str(&format!(
                "  <rect class=\"outline-bbox\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"0.1\"/>\n",
                fmt_mm(bx0),
                fmt_mm(flip(by1)),
                fmt_mm(bx1 - bx0),
                fmt_mm(by1 - by0),
            ));
        }
    }

    // The fab features themselves, in the marking look (front/back class + colour).
    let (class, color) = if bottom {
        ("fab-bottom", "#996633")
    } else {
        ("fab", "#663300")
    };
    for shape in &shapes {
        let shape = shape.map_points(|p| Point {
            x: mirror(p.x),
            y: p.y,
        });
        out.push_str(&svg_surface(&shape, &flip, class, color));
    }

    out.push_str("</svg>\n");
    Ok(out)
}

/// The deterministic fab-drawing SVG fileset: one `board-F_Fab.svg` / `board-B_Fab.svg`
/// per authored [`Role::Datum`] slab (top-down; see [`datum_slabs`]), named from the slab
/// with the [`slab_file`] convention (`.`→`_`) that the Gerbers use. Empty for the default
/// stackup (no fab slab). `(filename, content)` pairs, stable order; fallible because the
/// per-slab render resolves slab names (Decision 13).
pub fn fab_svg_set(doc: &Doc, lib: &PartLib) -> Result<Vec<(String, String)>, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let mut out = Vec::new();
    for slab in datum_slabs(&su) {
        out.push((
            format!("board-{}.svg", slab_file(&slab.name)),
            svg_fab(doc, lib, &slab)?,
        ));
    }
    Ok(out)
}

/// The full deterministic fab fileset: one Gerber per copper layer (`board-F_Cu.gbr`
/// …) in stack-up order, the two solder masks (`board-F_Mask.gbr` / `board-B_Mask.gbr`),
/// one silk Gerber per marking slab (`board-F_SilkS.gbr` / `board-B_SilkS.gbr`, top-down),
/// one fab Gerber per authored [`Role::Datum`] slab (`board-F_Fab.gbr` / `board-B_Fab.gbr`,
/// top-down — none on the default stackup, Decision 15), the `board-Edge_Cuts.gbr` outline,
/// and the Excellon drill program(s), split by plating into `board-PTH.drl` /
/// `board-NPTH.drl` (only the non-empty file(s), 0022). `(filename, content)` pairs; no
/// timestamps, stable order. Fallible because the silk/fab layers lower board text through
/// the slab-name materialization gate (Decision 13).
pub fn gerber_set(doc: &Doc, lib: &PartLib) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for slab in copper_layers(doc) {
        out.push((
            format!("board-{}.gbr", layer_file(&slab)),
            gerber_layer(doc, lib, &slab),
        ));
    }
    let su = crate::elaborate::stackup(&doc.source);
    // One solder-mask Gerber per `Role::Mask` slab, iterated by name (top-down; F.Mask
    // before B.Mask on the default stackup) exactly as silk iterates its marking slabs —
    // no copper-layer enum (Decision 16 stage 4).
    for slab in role_slabs(&su, Role::Mask) {
        out.push((
            format!("board-{}.gbr", slab_file(&slab.name)),
            gerber_mask(doc, lib, &slab)?,
        ));
    }
    for slab in marking_slabs(&su) {
        out.push((
            format!("board-{}.gbr", slab_file(&slab.name)),
            gerber_silk(doc, lib, &slab)?,
        ));
    }
    // One fab Gerber per authored fab slab (top-down; F.Fab before B.Fab), exactly as the
    // silk loop above iterates its marking slabs. Empty on the default stackup (no fab
    // slab), so a default board's fileset is byte-identical to before (Decision 15).
    for slab in datum_slabs(&su) {
        out.push((
            format!("board-{}.gbr", slab_file(&slab.name)),
            gerber_fab(doc, lib, &slab)?,
        ));
    }
    out.push((
        "board-Edge_Cuts.gbr".to_string(),
        gerber_edge_cuts(doc, lib),
    ));
    // Drill program(s), split PTH / NPTH (issue 0022); only non-empty files are emitted.
    out.extend(excellon_drill(doc, lib));
    Ok(out)
}

#[cfg(test)]
mod tests;
