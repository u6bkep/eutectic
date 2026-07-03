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
            ids.push(format!("{port}.{sig}"));
        }
    }
    ids
}

/// Minimal XML text escaping for labels.
fn xml_escape(s: &str) -> String {
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
/// ([`crate::route::world_features`]): every full-stackup through-cut `Role::Void` (a
/// pad drill or a via drill), as `(plated, diameter, kind)`. This is the fix for issue
/// 0022 — the drill file is now a query over the same `Void` features the solder-mask
/// export sees, so pad drills (previously omitted) and via drills both appear.
///
/// A mask opening is a *partial-z* `Void` (at the mask slab) and a board-authored void
/// is single-slab, so neither is a through-cut — both are excluded by the full-z gate. A
/// void's **plating** is carried by its material (Decision 16b): pad/via drills are
/// plated (a copper barrel), a material-less void is NPTH. A disc void is a `Round` hit;
/// a capsule (slot) void a `Slot`. Any other drill-void shape is an un-handled seam.
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
/// testable without an authoring path for NPTH holes (which the model cannot produce yet
/// — a mounting-hole `Void` is future work; see 0022 / Decision 16b).
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
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::doc::Doc;
    use crate::doc::Orient;
    use crate::elaborate::{board_rect, psu_module};
    use crate::history::History;
    use crate::part::part_library;

    /// Resolve a `Role::Mask` slab of `doc` by side — the top/bottom mask by z-position —
    /// so the mask tests can name a side while `gerber_mask` takes the slab itself. Panics
    /// if the side carries no mask (the tests all use the default stackup, which has both).
    fn mask_of(doc: &Doc, side: Layer) -> Slab {
        let su = crate::elaborate::stackup(&doc.source);
        let z = match side {
            Layer::Bottom => su.bottom_mask(),
            _ => su.top_mask(),
        }
        .expect("side has a mask slab");
        su.slabs
            .iter()
            .find(|s| s.role == Role::Mask && s.z == z)
            .cloned()
            .expect("mask slab present")
    }

    /// A copper [`Slab`] of `doc`'s stackup by name — the test-side of the export copper
    /// loop now taking a slab (Decision 13). Panics if the name is not a copper slab.
    fn cu(doc: &Doc, name: &str) -> Slab {
        crate::elaborate::stackup(&doc.source)
            .copper_slabs()
            .into_iter()
            .find(|s| s.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("no copper slab `{name}`"))
    }

    fn doc_psu(n: usize) -> (Doc, PartLib) {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(n))),
            &lib,
            "psu",
        )
        .unwrap();
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
ref,part,x_mm,y_mm,rotation_deg,side
psu.dec[0],Cap,10.000000,0.000000,0,T
psu.dec[1],Cap,20.000000,0.000000,0,T
psu.reg,LDO,0.000000,0.000000,0,T
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
                G::Instance {
                    path: "u1".into(),
                    part: "MCU".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Rotate {
                    path: "u1".into(),
                    orient: Orient::from_deg(90).unwrap(),
                },
            ])),
            &lib,
            "rot",
        )
        .unwrap();
        let csv = placement_csv(h.doc());
        assert!(
            csv.contains("u1,MCU,0.000000,0.000000,90,T\n"),
            "got:\n{csv}"
        );
    }

    #[test]
    fn placement_csv_marks_bottom_side() {
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                G::Instance {
                    path: "u1".into(),
                    part: "MCU".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Rotate {
                    path: "u1".into(),
                    orient: Orient::from_deg(0).unwrap().flipped(),
                },
                G::Instance {
                    path: "u2".into(),
                    part: "MCU".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Rotate {
                    path: "u2".into(),
                    orient: Orient::from_deg(90).unwrap().flipped(),
                },
            ])),
            &lib,
            "flip",
        )
        .unwrap();
        let csv = placement_csv(h.doc());
        // KiCad .pos convention: rotation is the *authored* about-z angle, side marked
        // separately — a plain bottom flip is `0,B`, and an authored 90° bottom part is
        // `90,B` (the flip axis is not folded into the reported angle).
        assert!(
            csv.contains(",0,B\n"),
            "bottom-side component at 0° marked B:\n{csv}"
        );
        assert!(
            csv.contains(",90,B\n"),
            "authored 90° bottom part reports 90,B:\n{csv}"
        );
    }

    #[test]
    fn svg_contains_outline_and_component_ids() {
        // A scene with an explicit board outline.
        let lib = part_library();
        let mut h = History::new(Default::default());
        let mut src = psu_module(2);
        src.insert(0, board_rect(Point::mm(0, 0), Point::mm(60, 40)));
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "board")
            .unwrap();
        let s = svg(h.doc(), &lib).unwrap();

        assert!(s.starts_with("<?xml"));
        assert!(s.contains("<svg "));
        assert!(s.contains("viewBox="));
        assert!(
            s.contains("class=\"outline-board\""),
            "explicit board outline expected"
        );
        assert!(s.contains("data-id=\"psu.reg\""));
        assert!(s.contains(">psu.dec[0]</text>"));
        assert!(s.contains("class=\"pad\""), "pin pads expected");
        assert!(s.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn svg_falls_back_to_bounding_box_without_board() {
        let (doc, lib) = doc_psu(2);
        let s = svg(&doc, &lib).unwrap();
        assert!(
            s.contains("class=\"outline-bbox\""),
            "implicit bbox outline expected"
        );
    }

    #[test]
    fn svg_draws_real_pad_copper_not_a_dot() {
        use crate::elaborate::GenDirective as G;
        use crate::part::{PadCopper, PadGeo, PadLayers, PinDef, PinRole};

        // A part whose single pin carries real copper: a 1mm square pad on Top
        // (straight edges ⇒ a filled `<polygon>`, no curve).
        let mut lib = PartLib::new();
        lib.insert(
            "PAD".into(),
            PartDef {
                name: "PAD".into(),
                pins: vec![PinDef {
                    name: "1".into(),
                    number: "1".into(),
                    role: PinRole::Passive,
                    offset: Point { x: 0, y: 0 },
                    pad: Some(PadGeo {
                        copper: vec![PadCopper {
                            shape: Shape2D::rect(Point { x: 0, y: 0 }, MM, MM),
                            layers: PadLayers::Top,
                        }],
                        drill: None,
                    }),
                }],
                interfaces: BTreeMap::new(),
                graphics: Vec::new(),
                texts: Vec::new(),
                courtyard: None,
                class: None,
            },
        );
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![G::Instance {
                path: "u1".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            }])),
            &lib,
            "pad",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();

        // The footprint's real copper is drawn as a filled pad polygon...
        assert!(
            s.contains("<polygon class=\"pad\""),
            "real pad copper expected as a filled polygon:\n{s}"
        );
        // ...replacing the old fixed r=0.3 circle render-lie for a padded pin.
        assert!(
            !s.contains("<circle class=\"pad\""),
            "the r=0.3 pad-dot lie should be gone for a real pad:\n{s}"
        );
    }

    #[test]
    fn svg_renders_board_text_as_silk_strokes() {
        use crate::doc::Orient;
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Text {
                    string: "R12".into(),
                    at: Point::mm(2, 10),
                    height: MM,
                    layer: "F.SilkS".into(),
                    orient: Orient::IDENTITY,
                },
            ])),
            &lib,
            "text",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();
        assert!(
            s.contains("class=\"silk\""),
            "lowered board text should render as silk strokes:\n{s}"
        );
        // Several glyph strokes ⇒ more than one silk polyline.
        assert!(s.matches("class=\"silk\"").count() >= 3, "got:\n{s}");
    }

    /// Imported footprint silk renders through the `Role::Marking` silk path (issue
    /// 0016): a placed component's `fp_line`s appear as `class="silk"` polylines.
    #[test]
    fn svg_renders_footprint_silk_as_silk_strokes() {
        use crate::elaborate::GenDirective as G;
        let mut lib = PartLib::new();
        let part = crate::kicad::import_footprint(
            r#"(footprint "GFX"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_line (start -1 -1) (end 1 -1) (stroke (width 0.12)) (layer "F.SilkS"))
                (fp_line (start 1 -1) (end 1 1) (stroke (width 0.12)) (layer "F.SilkS")))"#,
        )
        .unwrap();
        lib.insert("GFX".into(), part);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![G::Instance {
                path: "u1".into(),
                part: "GFX".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            }])),
            &lib,
            "gfx",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();
        assert!(
            s.contains("class=\"silk\""),
            "footprint silk should render as silk strokes:\n{s}"
        );
        assert!(
            s.matches("class=\"silk\"").count() >= 2,
            "two silk lines expected:\n{s}"
        );
    }

    /// A silk `fp_poly` is a *filled* area (radius 0): it must render as a closed
    /// filled `<polygon class="silk">`, not a `stroke-width="0"` (invisible) polyline.
    #[test]
    fn svg_renders_silk_polygon_as_filled_polygon() {
        use crate::elaborate::GenDirective as G;
        let mut lib = PartLib::new();
        let part = crate::kicad::import_footprint(
            r#"(footprint "TRI"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.SilkS")))"#,
        )
        .unwrap();
        lib.insert("TRI".into(), part);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![G::Instance {
                path: "u1".into(),
                part: "TRI".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            }])),
            &lib,
            "tri",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();
        assert!(
            s.contains("<polygon class=\"silk\""),
            "silk fp_poly should render as a filled polygon:\n{s}"
        );
        assert!(
            s.contains("<polygon class=\"silk\" points=\"") && s.contains("fill=\"#888888\""),
            "silk polygon should be filled silk-colour:\n{s}"
        );
        // It must NOT be emitted as an invisible zero-width silk polyline.
        assert!(
            !s.contains("class=\"silk\" points=\"") || !s.contains("stroke-width=\"0\""),
            "silk polygon must not be a stroke-width=0 polyline:\n{s}"
        );
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
            board_rect(Point::mm(0, 0), Point::mm(20, 10)),
            G::Instance {
                path: "c0".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "c0".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "c1".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "N".into(),
                pins: vec![("c0".into(), "p1".into()), ("c1".into(), "p1".into())],
            },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "place")
            .unwrap();
        let net = NetId::new("N");
        let t0 = Trace {
            net: net.clone(),
            layer: "F.Cu".into(),
            path: vec![Point::mm(6, 5), Point::mm(10, 5)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        let t1 = Trace {
            net: net.clone(),
            layer: "B.Cu".into(),
            path: vec![Point::mm(10, 5), Point::mm(14, 5)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        let v = Via {
            net,
            at: Point::mm(10, 5),
            span: None,
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
        let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
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
        let bot = gerber_layer(&doc, &lib, &cu(&doc, "B.Cu"));
        assert_eq!(bot.matches("D01*").count(), 1);
        assert_eq!(bot.matches("D03*").count(), 1);
    }

    #[test]
    fn excellon_lists_via_drills() {
        let (doc, lib) = hand_routed_board();
        let files = excellon_drill(&doc, &lib);
        // The via is a plated through-hole, so it lands in the PTH file; the Cap pads are
        // footprint-less (no drill), so there is no NPTH file.
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["board-PTH.drl"], "PTH only, no NPTH");
        let drl = &files[0].1;
        assert!(drl.starts_with("M48"));
        assert!(drl.contains("METRIC"));
        // One tool at the via's 0.3mm drill, with the via's coordinate.
        assert!(drl.contains("T1C0.300000"), "got:\n{drl}");
        assert!(drl.contains("X10.000000Y5.000000"), "got:\n{drl}");
        assert!(drl.trim_end().ends_with("M30"));
    }

    /// Issue 0022: the drill file is a forward query over through-cut `Void` features, so
    /// a plated through-hole **pad**'s drill now reaches the PTH file — not only vias. A
    /// board with a drilled pad *and* a via yields both, with correct diameters at the
    /// right coordinates, and there is no NPTH file (both holes are plated).
    #[test]
    fn excellon_includes_pad_and_via_drills() {
        let mut lib = part_library();
        let fp = crate::kicad::import_footprint(
            r#"(footprint "TH" (pad "1" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8) (layers "*.Cu")))"#,
        )
        .unwrap();
        lib.insert("TH".into(), fp);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "j".into(),
                    part: "TH".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "j".into(),
                    pos: Point::mm(5, 5),
                },
                // Establishes net N so the via may be added.
                G::ConnectPins {
                    net: "N".into(),
                    pins: vec![("j".into(), "1".into())],
                },
            ])),
            &lib,
            "th",
        )
        .unwrap();
        // A via, so the file carries both a pad drill and a via drill.
        let v = Via {
            net: NetId::new("N"),
            at: Point::mm(12, 8),
            span: None,
            drill: 300_000,
            pad: 600_000,
            prov: Provenance::Pinned,
        };
        h.commit(Transaction::one(Command::AddVia(ViaId(0), v)), &lib, "via")
            .unwrap();
        let doc = h.doc();

        let files = excellon_drill(doc, &lib);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["board-PTH.drl"],
            "both holes are plated ⇒ one PTH file, no NPTH: {names:?}"
        );
        let drl = &files[0].1;
        // Both tools present: the pad drill (0.8mm) and the via drill (0.3mm).
        assert!(drl.contains("C0.800000"), "pad drill tool 0.8mm:\n{drl}");
        assert!(drl.contains("C0.300000"), "via drill tool 0.3mm:\n{drl}");
        // Hit coordinates: the pad at (5,5), the via at (12,8).
        assert!(drl.contains("X5.000000Y5.000000"), "pad drill hit:\n{drl}");
        assert!(drl.contains("X12.000000Y8.000000"), "via drill hit:\n{drl}");
    }

    /// An authored `hole` directive (Decision 16b NPTH) reaches `board-NPTH.drl`: the
    /// full-stackup material-less `Role::Void` it lowers to is classified non-plated by
    /// `drill_hits` and lands in the NPTH file at its exact center + diameter.
    #[test]
    fn authored_hole_reaches_npth_drill() {
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Hole {
                    center: Point::mm(3, 17),
                    dia: 2_700_000, // M2.5 clearance
                },
            ])),
            &lib,
            "hole",
        )
        .unwrap();
        let files = excellon_drill(h.doc(), &lib);
        let npth = files
            .iter()
            .find(|(n, _)| n == "board-NPTH.drl")
            .map(|(_, c)| c.as_str())
            .unwrap_or_else(|| panic!("expected board-NPTH.drl, got {:?}", files));
        assert!(npth.contains("C2.700000"), "2.7mm NPTH tool:\n{npth}");
        assert!(
            npth.contains("X3.000000Y17.000000"),
            "hole at (3,17):\n{npth}"
        );
        // And it is NOT in a PTH file (no plated barrel).
        assert!(
            !files.iter().any(|(n, _)| n == "board-PTH.drl"),
            "a lone NPTH hole ships no PTH file"
        );
    }

    /// The plating split: a hit list with both a plated and a non-plated hole yields two
    /// files, each carrying only its own class. (Exercised on a synthesized hit list so
    /// the split logic is unit-testable without a full authored board — the end-to-end
    /// authoring path is `authored_hole_reaches_npth_drill`.)
    #[test]
    fn excellon_splits_pth_and_npth() {
        let hits = vec![
            (true, 800_000, DrillKind::Round(Point::mm(5, 5))), // plated pad, 0.8mm
            (false, 900_000, DrillKind::Round(Point::mm(9, 9))), // NPTH mounting, 0.9mm
        ];
        let files = excellon_files(hits);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["board-PTH.drl", "board-NPTH.drl"]);
        let pth = &files[0].1;
        let npth = &files[1].1;
        assert!(
            pth.contains("C0.800000") && !pth.contains("C0.900000"),
            "PTH:\n{pth}"
        );
        assert!(
            npth.contains("C0.900000") && !npth.contains("C0.800000"),
            "NPTH:\n{npth}"
        );
        assert!(pth.contains("X5.000000Y5.000000") && npth.contains("X9.000000Y9.000000"));
    }

    /// A slot (capsule) drill emits a `G85` routed hole between its endpoints.
    #[test]
    fn excellon_slot_emits_g85() {
        let prog = excellon_program(
            &[(600_000, DrillKind::Slot(Point::mm(2, 3), Point::mm(6, 3)))],
            "slots",
        );
        assert!(
            prog.contains("X2.000000Y3.000000G85X6.000000Y3.000000"),
            "slot as G85:\n{prog}"
        );
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

    // --- stage 3: arc-aware export helpers --------------------------------------
    // (Until import/text can author arcs, no arc board reaches export end-to-end, so
    //  the helpers are exercised directly here on constructed arc shapes.)

    const TMM: Nm = 1_000_000;
    fn tp(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }
    /// A filled half-disc (D-shape): an arc over the top closed by the flat diameter.
    fn half_disc(r: Nm) -> Shape2D {
        Shape2D::polygon_path(
            crate::geom::Path {
                start: tp(-r, 0),
                segs: vec![Seg::Arc {
                    mid: tp(0, r),
                    end: tp(r, 0),
                }],
            },
            0,
        )
    }

    #[test]
    fn svg_arc_params_match_hand_computed_flags() {
        let r = 10 * TMM;
        // Upper semicircle (-R,0)→(0,R)→(R,0): model-CW after y-flip ⇒ sweep 1; the
        // 180° span puts the centre on the chord ⇒ large 0.
        let (rad, large, sweep) = svg_arc_params(tp(-r, 0), tp(0, r), tp(r, 0)).unwrap();
        assert_eq!((large, sweep), (0, 1));
        assert!((rad - r).abs() < 10, "radius ~ R, got {rad}");
        // Minor CCW quarter (R,0)→45°→(0,R): turn > 0 ⇒ sweep 0; < 180° ⇒ large 0.
        let m = (r as f64 * std::f64::consts::FRAC_1_SQRT_2).round() as Nm;
        let (rad2, large2, sweep2) = svg_arc_params(tp(r, 0), tp(m, m), tp(0, r)).unwrap();
        assert_eq!((large2, sweep2), (0, 0));
        assert!((rad2 - r).abs() < 10);
        // Collinear ⇒ None (caller draws a straight line).
        assert!(svg_arc_params(tp(0, 0), tp(TMM, 0), tp(2 * TMM, 0)).is_none());
    }

    #[test]
    fn svg_arc_params_major_arc_sets_large_flag() {
        let r = 10 * TMM;
        let f = |deg: f64| {
            let a = deg.to_radians();
            tp(
                (r as f64 * a.cos()).round() as Nm,
                (r as f64 * a.sin()).round() as Nm,
            )
        };
        // 0°→200°→210°: a 210° CCW major arc.
        let (_, large, sweep) = svg_arc_params(f(0.0), f(200.0), f(210.0)).unwrap();
        assert_eq!(large, 1, "sweep > 180° sets large-arc");
        assert_eq!(sweep, 0, "CCW in model ⇒ sweep 0");
    }

    #[test]
    fn arc_ij_turn_is_exact_and_oriented() {
        let r = 10 * TMM;
        // Upper semicircle: centre origin, start (−R,0) ⇒ I/J = centre − start = (R,0);
        // CW ⇒ turn −1.
        let (ij, turn) = arc_ij_turn(tp(-r, 0), tp(0, r), tp(r, 0)).unwrap();
        assert_eq!(ij, tp(r, 0));
        assert_eq!(turn, -1);
        assert!(arc_ij_turn(tp(0, 0), tp(TMM, 0), tp(2 * TMM, 0)).is_none());
        // Far-from-origin placement: the same arc shifted by (1e9, 1e9) nm must give the
        // identical I/J (the start-relative computation is overflow-safe and invariant).
        let s = 1_000_000_000;
        let (ij2, turn2) = arc_ij_turn(tp(s - r, s), tp(s, s + r), tp(s + r, s)).unwrap();
        assert_eq!(
            (ij2, turn2),
            (tp(r, 0), -1),
            "translation-invariant, no overflow"
        );
    }

    #[test]
    fn svg_path_d_emits_an_arc_command() {
        let d = svg_path_d(&half_disc(10 * TMM), &(|y: Nm| -y));
        assert!(d.starts_with("M "), "{d}");
        assert!(d.contains(" A "), "carries an SVG arc command: {d}");
        assert!(d.ends_with(" Z"), "closed: {d}");
    }

    #[test]
    fn gerber_contour_emits_g02_arc_with_ij() {
        let mut out = String::new();
        let (mut mode, mut g75) = ("G01", false);
        gerber_contour(&half_disc(10 * TMM), &mut out, &mut mode, &mut g75);
        assert!(out.contains("X-10000000Y0D02*"), "move to start:\n{out}");
        assert!(
            out.contains("G75*"),
            "multi-quadrant enabled before the arc:\n{out}"
        );
        assert!(
            out.contains("G02*"),
            "the upper semicircle is CW (G02):\n{out}"
        );
        // Arc to end (R,0) with I/J = centre(0,0) − start(−R,0) = (R, 0).
        assert!(
            out.contains("X10000000Y0I10000000J0D01*"),
            "arc draw with I/J:\n{out}"
        );
        // The flat diameter closes the contour with a straight line back to start.
        assert!(
            out.contains("G01*\nX-10000000Y0D01*"),
            "straight closing edge:\n{out}"
        );
    }

    /// A filled blob whose top edge is a cubic Bézier, closed by the flat diameter.
    fn cubic_blob(r: Nm) -> Shape2D {
        Shape2D::polygon_path(
            crate::geom::Path {
                start: tp(-r, 0),
                segs: vec![Seg::Cubic {
                    c1: tp(-r, 2 * r),
                    c2: tp(r, 2 * r),
                    end: tp(r, 0),
                }],
            },
            0,
        )
    }

    #[test]
    fn svg_path_d_emits_a_cubic_command() {
        let d = svg_path_d(&cubic_blob(10 * TMM), &(|y: Nm| -y));
        assert!(d.starts_with("M "), "{d}");
        assert!(d.contains(" C "), "carries an SVG cubic command: {d}");
        assert!(d.ends_with(" Z"), "closed: {d}");
    }

    #[test]
    fn gerber_contour_flattens_a_bezier_to_g01_lines() {
        // Gerber has no Béziers: the curve must come out as a run of G01 draws, with
        // no arc codes and no SVG-isms.
        let mut out = String::new();
        let (mut mode, mut g75) = ("G01", false);
        gerber_contour(&cubic_blob(10 * TMM), &mut out, &mut mode, &mut g75);
        assert!(
            !out.contains("G02*") && !out.contains("G03*"),
            "a Bézier emits no arc codes:\n{out}"
        );
        let draws = out.matches("D01*").count();
        assert!(
            draws > 2,
            "the Bézier flattens to several G01 draws ({draws}):\n{out}"
        );
        assert!(
            out.contains("X10000000Y0"),
            "reaches the curve endpoint:\n{out}"
        );
    }

    #[test]
    fn arc_board_flattens_to_polyline_in_edge_cuts_and_svg() {
        // A half-disc board authored in the text front-end. Under Decision 16b/c the
        // substrate is a `Shape2D::Area` (a polygonized region), so the curved edge
        // exports as a fine straight-segment polyline (G01 / SVG `L`), not a G02/G03 /
        // SVG `A` arc — the arc is gone once the outline becomes a region. The authored
        // arc still lives in the `Board` directive; only this derived export is flat.
        let lib = part_library();
        let crate::text::Parsed { source: src, .. } =
            crate::text::parse("board (-2mm, 0mm) arc (0mm, 2mm) (2mm, 0mm)").unwrap();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "arc board")
            .unwrap();
        let doc = h.doc().clone();
        let g = gerber_edge_cuts(&doc, &lib);
        assert!(
            !g.contains("G02*") && !g.contains("G03*"),
            "the arc is flattened — no G02/G03:\n{g}"
        );
        assert!(
            g.matches("D01*").count() > 8,
            "the curved edge draws as many straight G01 segments:\n{g}"
        );
        // The arc endpoints (−2,0) and (2,0) mm are exact ring vertices.
        assert!(
            g.contains("X-2000000Y0") && g.contains("X2000000Y0"),
            "reaches endpoints:\n{g}"
        );
        let s = svg(&doc, &lib).unwrap();
        assert!(
            s.contains("<path class=\"outline-board\""),
            "outline is a path:\n{s}"
        );
        assert!(
            !s.contains(" A "),
            "the polygonized region carries no SVG arc command:\n{s}"
        );
    }

    #[test]
    fn gerber_set_names_and_layers() {
        let (doc, lib) = hand_routed_board();
        let set = gerber_set(&doc, &lib).unwrap();
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "board-F_Cu.gbr",
                "board-B_Cu.gbr",
                "board-F_Mask.gbr",
                "board-B_Mask.gbr",
                "board-F_SilkS.gbr",
                "board-B_SilkS.gbr",
                "board-Edge_Cuts.gbr",
                "board-PTH.drl",
            ]
        );
    }

    #[test]
    fn svg_draws_traces_and_vias() {
        let (doc, lib) = hand_routed_board();
        let s = svg(&doc, &lib).unwrap();
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
            G::Instance {
                path: "u1".into(),
                part: "PADX".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "u1".into(),
                pos: Point::mm(5, 5),
            },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "p")
            .unwrap();
        (h.doc().clone(), lib)
    }

    #[test]
    fn component_pads_flash_by_shape() {
        let (doc, lib) = padded_board();
        let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
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
        assert_eq!(
            gerber_layer(&doc, &lib, &cu(&doc, "F.Cu")),
            gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"))
        );
        assert_eq!(excellon_drill(&doc, &lib), excellon_drill(&doc, &lib));
        assert_eq!(gerber_edge_cuts(&doc, &lib), gerber_edge_cuts(&doc, &lib));
    }

    #[test]
    fn gerber_set_on_autorouted_board_is_deterministic() {
        use crate::autoroute::autoroute;
        use crate::route::DesignRules;
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c0".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
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
        let result = autoroute(h.doc(), &lib, &DesignRules::default());
        h.commit(Transaction(result.commands), &lib, "route")
            .unwrap();
        let doc = h.doc();
        // The autorouter laid real copper, so the F_Cu Gerber has trace draws.
        assert!(!doc.traces.is_empty());
        let top = gerber_layer(doc, &lib, &cu(doc, "F.Cu"));
        assert!(top.matches("D01*").count() > 0);
        assert_eq!(gerber_set(doc, &lib), gerber_set(doc, &lib));
    }

    // --- copper pour export (0004 stage 5) --------------------------------

    /// A 20x20 board with a GND pour on F.Cu and a foreign SIG pad (knocked out).
    fn poured_board() -> (Doc, PartLib) {
        use crate::elaborate::RegionDecl;
        use crate::geom::Role;
        let mut lib = part_library();
        let pad = crate::kicad::import_footprint(
            r#"(footprint "P1" (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap();
        lib.insert("P1".into(), pad);
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 20),
            Point::mm(0, 20),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "g".into(),
                part: "P1".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "s".into(),
                part: "P1".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "s".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("s".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: outline,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "pour")
            .unwrap();
        (h.doc().clone(), lib)
    }

    #[test]
    fn gerber_emits_pour_region_fill() {
        let (doc, lib) = poured_board();
        let top = gerber_layer(&doc, &lib, &cu(&doc, "F.Cu"));
        assert!(top.contains("G36*"), "pour region opens:\n{top}");
        assert!(top.contains("G37*"), "pour region closes");
        // Outer board contour + a knockout hole around the SIG pad ⇒ ≥2 contours
        // (≥2 D02 moves) inside the single G36/G37 block.
        let block = top
            .split("G36*")
            .nth(1)
            .unwrap()
            .split("G37*")
            .next()
            .unwrap();
        assert!(
            block.matches("D02*").count() >= 2,
            "outer + hole contours:\n{block}"
        );
        // The bottom layer carries no pour.
        assert!(!gerber_layer(&doc, &lib, &cu(&doc, "B.Cu")).contains("G36*"));
    }

    #[test]
    fn svg_draws_pour_with_holes() {
        let (doc, lib) = poured_board();
        let s = svg(&doc, &lib).unwrap();
        assert!(
            s.contains("class=\"pour pour-top\""),
            "pour path present:\n{s}"
        );
        assert!(s.contains("fill-rule=\"evenodd\""), "holes via even-odd");
        assert!(s.contains("data-net=\"GND\""));
    }

    #[test]
    fn fab_with_pour_is_deterministic() {
        let (doc, lib) = poured_board();
        assert_eq!(gerber_set(&doc, &lib), gerber_set(&doc, &lib));
        assert_eq!(svg(&doc, &lib), svg(&doc, &lib));
    }

    // --- solder mask (0004 stage 6) ---------------------------------------

    #[test]
    fn solder_mask_opens_over_pads_with_expansion() {
        // padded_board has an F.Cu rect pad 0.6x1.2 and a circle pad 0.8. The mask
        // opening inflates each by 0.05mm per side: rect → 0.7x1.3, circle → 0.9.
        let (doc, lib) = padded_board();
        let f = gerber_mask(&doc, &lib, &mask_of(&doc, Layer::Top)).unwrap();
        assert!(f.contains("F_Mask"));
        assert!(
            f.contains("R,0.700000X1.300000*%"),
            "expanded rect opening:\n{f}"
        );
        assert!(f.contains("C,0.900000*%"), "expanded circle opening:\n{f}");
        assert_eq!(f.matches("D03*").count(), 2, "one opening per pad");
        // No bottom-side pads ⇒ no openings on B_Mask.
        assert_eq!(
            gerber_mask(&doc, &lib, &mask_of(&doc, Layer::Bottom))
                .unwrap()
                .matches("D03*")
                .count(),
            0
        );
    }

    #[test]
    fn through_hole_pad_opens_both_masks() {
        let mut lib = part_library();
        let fp = crate::kicad::import_footprint(
            r#"(footprint "TH" (pad "1" thru_hole circle (at 0 0) (size 1.5 1.5) (drill 0.8) (layers "*.Cu")))"#,
        )
        .unwrap();
        lib.insert("TH".into(), fp);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                G::Instance {
                    path: "j".into(),
                    part: "TH".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "j".into(),
                    pos: Point::mm(5, 5),
                },
            ])),
            &lib,
            "th",
        )
        .unwrap();
        let doc = h.doc();
        // A through-hole pad is exposed on both faces, so it opens on both masks. Its
        // drill `Void` is a through-cut (full-stack z), not a mask-slab opening, so it is
        // NOT an extra flash — the count stays one opening per side.
        assert_eq!(
            gerber_mask(doc, &lib, &mask_of(doc, Layer::Top))
                .unwrap()
                .matches("D03*")
                .count(),
            1
        );
        assert_eq!(
            gerber_mask(doc, &lib, &mask_of(doc, Layer::Bottom))
                .unwrap()
                .matches("D03*")
                .count(),
            1
        );
    }

    /// New capability (Decision 13): a board cutout removes solder mask over its whole
    /// area, so it appears on the mask as a `G36`/`G37` region fill. The old parallel
    /// rule (pad-copper + expansion only) missed cutouts entirely.
    #[test]
    fn mask_gerber_includes_board_cutout() {
        let lib = part_library();
        let cutout = Shape2D::polygon(vec![
            Point::mm(8, 8),
            Point::mm(12, 8),
            Point::mm(12, 12),
            Point::mm(8, 12),
        ]);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Cutout { shape: cutout },
            ])),
            &lib,
            "cut",
        )
        .unwrap();
        let f = gerber_mask(h.doc(), &lib, &mask_of(h.doc(), Layer::Top)).unwrap();
        assert!(f.contains("G36*"), "cutout opens a mask region:\n{f}");
        assert!(f.contains("G37*"), "region closes:\n{f}");
        // The cutout corner (12mm) is drawn in the region contour (nm coordinates).
        assert!(f.contains("X12000000Y12000000"), "cutout boundary:\n{f}");
        // Both faces lose mask over a through cutout.
        assert!(
            gerber_mask(h.doc(), &lib, &mask_of(h.doc(), Layer::Bottom))
                .unwrap()
                .contains("G36*")
        );
    }

    /// A board with a cutout: the substrate is one `Area` (outline ∖ cutout), so
    /// `Edge.Cuts` draws both the outer boundary and the cutout hole ring, and the SVG
    /// `outline-board` path carries the cutout ring too (Decision 16b/c).
    #[test]
    fn edge_cuts_and_svg_include_board_cutout() {
        let lib = part_library();
        let cutout = Shape2D::polygon(vec![
            Point::mm(8, 8),
            Point::mm(12, 8),
            Point::mm(12, 12),
            Point::mm(8, 12),
        ]);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Cutout { shape: cutout },
            ])),
            &lib,
            "cut",
        )
        .unwrap();
        let doc = h.doc();

        let e = gerber_edge_cuts(doc, &lib);
        assert!(
            e.contains("X20000000Y20000000"),
            "outer boundary corner:\n{e}"
        );
        assert!(e.contains("X12000000Y12000000"), "cutout ring corner:\n{e}");
        assert!(
            e.matches("D02*").count() >= 2,
            "outer + cutout are two closed contours:\n{e}"
        );

        let s = svg(doc, &lib).unwrap();
        assert!(
            s.contains("class=\"outline-board\""),
            "board outline path:\n{s}"
        );
        // The cutout's 8/12 mm coordinates appear only in the cutout ring (the outer
        // square is 0/20 mm), and the path has a second subpath (the hole).
        assert!(
            s.contains("12.000000,12.000000") && s.contains("8.000000,8.000000"),
            "cutout ring in the svg path:\n{s}"
        );
        assert!(
            s.matches(" M").count() + s.matches("\"M").count() >= 2,
            "outline path has an outer subpath and a cutout subpath:\n{s}"
        );
    }

    // --- silk Gerbers (Decision 13, stage 2b) -----------------------------

    /// The default fileset carries an F and B silk Gerber, and board text on F.SilkS
    /// comes out on the F silk layer as centreline draws with a round pen aperture.
    #[test]
    fn silk_gerber_draws_text_strokes_with_aperture() {
        use crate::doc::Orient;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Text {
                    string: "R1".into(),
                    at: Point::mm(2, 10),
                    height: MM,
                    layer: "F.SilkS".into(),
                    orient: Orient::IDENTITY,
                },
            ])),
            &lib,
            "silk-text",
        )
        .unwrap();
        let doc = h.doc();
        // The fileset exposes both silk layers.
        let set = gerber_set(doc, &lib).unwrap();
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"board-F_SilkS.gbr"),
            "F silk file: {names:?}"
        );
        assert!(
            names.contains(&"board-B_SilkS.gbr"),
            "B silk file: {names:?}"
        );

        let su = crate::elaborate::stackup(&doc.source);
        let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
        let g = gerber_silk(doc, &lib, fsilk).unwrap();
        // A round pen aperture (the text stroke width = height/8 = 0.125mm) and real draws.
        assert!(g.contains("C,0.125000*%"), "round silk pen aperture:\n{g}");
        assert!(g.matches("D01*").count() > 2, "text strokes draw:\n{g}");
        // The empty B silk layer carries no draws.
        let bsilk = su.slabs.iter().find(|s| s.name == "B.SilkS").unwrap();
        assert_eq!(
            gerber_silk(doc, &lib, bsilk)
                .unwrap()
                .matches("D01*")
                .count(),
            0
        );
    }

    /// A footprint `fp_poly` on silk is a filled area, so it comes out as a `G36`/`G37`
    /// region (not a zero-width stroke).
    #[test]
    fn silk_gerber_fp_poly_is_a_region() {
        let mut lib = PartLib::new();
        let part = crate::kicad::import_footprint(
            r#"(footprint "TRI"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.SilkS")))"#,
        )
        .unwrap();
        lib.insert("TRI".into(), part);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![G::Instance {
                path: "u1".into(),
                part: "TRI".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            }])),
            &lib,
            "tri",
        )
        .unwrap();
        let doc = h.doc();
        let su = crate::elaborate::stackup(&doc.source);
        let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
        let g = gerber_silk(doc, &lib, fsilk).unwrap();
        assert!(
            g.contains("G36*") && g.contains("G37*"),
            "fp_poly is a region:\n{g}"
        );
    }

    /// Regression: a straight silk stroke following an arc-bearing one must switch the
    /// interpolation mode back to `G01` before its line draw. Aperture (D-code) selection
    /// does not reset the modal G01/G02/G03 state, so without the transition the line
    /// would be emitted while still in arc mode (a malformed draw).
    #[test]
    fn silk_gerber_line_after_arc_returns_to_g01() {
        let mut lib = PartLib::new();
        // An fp_arc (emits G02/G03) declared before an fp_line (a straight draw), same
        // pen width so they share one aperture — exactly the order that tripped the bug.
        let part = crate::kicad::import_footprint(
            r#"(footprint "ARCLINE"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_arc (start -2 0) (mid 0 2) (end 2 0) (stroke (width 0.2)) (layer "F.SilkS"))
                (fp_line (start 3 0) (end 5 0) (stroke (width 0.2)) (layer "F.SilkS")))"#,
        )
        .unwrap();
        lib.insert("ARCLINE".into(), part);
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![G::Instance {
                path: "u1".into(),
                part: "ARCLINE".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            }])),
            &lib,
            "arcline",
        )
        .unwrap();
        let doc = h.doc();
        let su = crate::elaborate::stackup(&doc.source);
        let fsilk = su.slabs.iter().find(|s| s.name == "F.SilkS").unwrap();
        let g = gerber_silk(doc, &lib, fsilk).unwrap();

        // An arc really was emitted...
        let arc_pos = g
            .find("G03*")
            .or_else(|| g.find("G02*"))
            .expect("fp_arc emits a G02/G03 draw");
        // ...and a G01* returns before the fp_line is drawn.
        assert!(
            g[arc_pos..].contains("G01*"),
            "a straight stroke after an arc must switch back to G01:\n{g}"
        );
        // The fp_line reaches its endpoint (5mm) as a plain line draw, never a degenerate
        // arc (an arc draw carries I/J offsets; a stuck-in-arc-mode line would not).
        assert!(
            g.contains("X5000000Y0D01*"),
            "fp_line drawn as a straight D01:\n{g}"
        );
    }

    /// SVG splits silk by side: a bottom-side marking gets `class="silk-bottom"`, while
    /// top-side silk keeps `class="silk"` (existing single-side fixtures unchanged).
    #[test]
    fn svg_bottom_silk_gets_bottom_class() {
        use crate::doc::Orient;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Text {
                    string: "B1".into(),
                    at: Point::mm(2, 10),
                    height: MM,
                    layer: "B.SilkS".into(),
                    orient: Orient::IDENTITY,
                },
            ])),
            &lib,
            "b-silk",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();
        assert!(
            s.contains("class=\"silk-bottom\""),
            "bottom silk gets its own class:\n{s}"
        );
        assert!(
            !s.contains("class=\"silk\" "),
            "no top-silk class for a bottom-only board:\n{s}"
        );
    }

    // --- fab drawing (Decision 15 consumer) -------------------------------

    /// The default 2-layer stackup with an added zero-height `F.Fab` datum slab at the
    /// F.Cu top face — the way a user authors a fab slab (Decision 15). Returned as `Slab`
    /// directives so `elaborate::stackup` picks them up.
    fn stackup_with_fab() -> Vec<crate::elaborate::GenDirective> {
        use crate::elaborate::GenDirective as G;
        let mut slabs = Stackup::default_2layer().slabs;
        let top = slabs.iter().find(|s| s.name == "F.Cu").unwrap().z.hi;
        slabs.push(Slab {
            name: "F.Fab".into(),
            z: ZRange::new(top, top),
            role: Role::Datum,
            material: None,
        });
        slabs.into_iter().map(G::Slab).collect()
    }

    /// A footprint carrying an SMD pad, an `F.Fab` graphic line, and an `F.Fab` `user`
    /// text anchor (imported as a `Literal`) — the three fab-layer inputs the drawing pass
    /// must render.
    fn fab_footprint() -> PartDef {
        crate::kicad::import_footprint(
            r#"(footprint "FAB"
                (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "F.Fab"))
                (fp_text user "FAB1" (at 0 1) (layer "F.Fab") (effects (font (size 1 1)))))"#,
        )
        .unwrap()
    }

    /// An authored `F.Fab` slab plus a footprint with fab graphics and a fab text anchor
    /// emits a fab SVG that carries both the graphic stroke and the text strokes — the
    /// consumer that closes the "authored fab slab renders nowhere" gap (Decision 15).
    #[test]
    fn fab_svg_emitted_with_graphics_and_text() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert("FAB".into(), fab_footprint());
        let mut source = stackup_with_fab();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Instance {
            path: "u".into(),
            part: "FAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "fab")
            .unwrap();
        let doc = h.doc();

        let set = fab_svg_set(doc, &lib).unwrap();
        assert_eq!(set.len(), 1, "one fab slab ⇒ one fab SVG");
        let (name, svg) = &set[0];
        assert_eq!(name, "board-F_Fab.svg");
        // Board outline for context.
        assert!(svg.contains("class=\"outline-board\""), "outline:\n{svg}");
        // The fab graphic line draws as a fab-class stroke.
        assert!(
            svg.contains("class=\"fab\""),
            "fab graphic + text render as fab strokes:\n{svg}"
        );
        // Text lowers to several glyph strokes ⇒ more than the single graphic line.
        assert!(
            svg.matches("class=\"fab\"").count() >= 3,
            "graphic line + multiple text strokes expected:\n{svg}"
        );
        assert_eq!(
            fab_svg_set(doc, &lib),
            fab_svg_set(doc, &lib),
            "deterministic"
        );
    }

    /// With **no** fab slab authored (the default stackup), the fab fileset is empty and
    /// fab-layer footprint graphics stay invisible in every other output — the Decision 15
    /// contract (a fab graphic materializes only when a fab slab exists).
    #[test]
    fn no_fab_slab_means_no_fab_output_and_invisible_graphics() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert("FAB".into(), fab_footprint());
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "u".into(),
                    part: "FAB".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "u".into(),
                    pos: Point::mm(5, 5),
                },
            ])),
            &lib,
            "no-fab",
        )
        .unwrap();
        let doc = h.doc();

        // No fab SVG.
        assert!(
            fab_svg_set(doc, &lib).unwrap().is_empty(),
            "no fab slab ⇒ no fab file"
        );
        // The fab graphic is inert everywhere else: not in the SVG, not in the Gerber set.
        let s = svg(doc, &lib).unwrap();
        assert!(
            !s.contains("class=\"fab\""),
            "fab graphic must not leak into the SVG:\n{s}"
        );
        let gset = gerber_set(doc, &lib).unwrap();
        assert!(
            gset.iter().all(|(n, _)| !n.contains("Fab")),
            "no fab Gerber in the fileset: {:?}",
            gset.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }

    /// A bottom fab slab (`B.Fab`) renders with the bottom-side class, mirroring the silk
    /// side split. Driven by a footprint carrying a `B.Fab` graphic (placed top-side, so
    /// `swap_side` leaves it on `B.Fab`) — the footprint path is role-driven off the slab,
    /// so a `Role::Datum` `B.Fab` slab produces a bottom-side fab feature.
    #[test]
    fn bottom_fab_gets_bottom_class() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert(
            "BFAB".into(),
            crate::kicad::import_footprint(
                r#"(footprint "BFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "B.Fab")))"#,
            )
            .unwrap(),
        );
        let mut slabs = Stackup::default_2layer().slabs;
        let bot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
        slabs.push(Slab {
            name: "B.Fab".into(),
            z: ZRange::new(bot, bot),
            role: Role::Datum,
            material: None,
        });
        let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Instance {
            path: "u".into(),
            part: "BFAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "bfab")
            .unwrap();
        let set = fab_svg_set(h.doc(), &lib).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].0, "board-B_Fab.svg");
        assert!(
            set[0].1.contains("class=\"fab-bottom\""),
            "bottom fab gets its own class:\n{}",
            set[0].1
        );
    }

    // --- fab Gerber (Decision 15/16) --------------------------------------

    /// An authored `F.Fab` slab plus a footprint with a fab graphic line and a fab text
    /// anchor emits a fab Gerber carrying real stroke draws (the graphic + glyph strokes),
    /// and the fileset lists `board-F_Fab.gbr` — the Gerber sibling of the fab SVG.
    #[test]
    fn fab_gerber_emitted_with_strokes() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert("FAB".into(), fab_footprint());
        let mut source = stackup_with_fab();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Instance {
            path: "u".into(),
            part: "FAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "fab")
            .unwrap();
        let doc = h.doc();

        // The fileset exposes the fab Gerber.
        let set = gerber_set(doc, &lib).unwrap();
        let names: Vec<&str> = set.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"board-F_Fab.gbr"),
            "fab Gerber in the fileset: {names:?}"
        );

        let su = crate::elaborate::stackup(&doc.source);
        let fab = su.slab("F.Fab").unwrap();
        let g = gerber_fab(doc, &lib, fab).unwrap();
        // A round pen aperture (the text stroke width = height/8 = 0.125mm) and real draws
        // (the graphic line + the glyph strokes).
        assert!(g.contains("C,0.125000*%"), "round fab pen aperture:\n{g}");
        assert!(g.matches("D01*").count() > 2, "fab strokes draw:\n{g}");
        assert_eq!(g, gerber_fab(doc, &lib, fab).unwrap(), "deterministic");
    }

    /// A fab `fp_poly` is a filled area, so it comes out as a `G36`/`G37` region fill on the
    /// fab Gerber (the same area path silk uses) — exercising the region-fill arm.
    #[test]
    fn fab_gerber_fp_poly_is_a_region() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert(
            "FABTRI".into(),
            crate::kicad::import_footprint(
                r#"(footprint "FABTRI"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_poly (pts (xy -1 -1) (xy 1 -1) (xy 0 1)) (width 0) (layer "F.Fab")))"#,
            )
            .unwrap(),
        );
        let mut source = stackup_with_fab();
        source.push(G::Instance {
            path: "u".into(),
            part: "FABTRI".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "fabtri")
            .unwrap();
        let doc = h.doc();
        let su = crate::elaborate::stackup(&doc.source);
        let fab = su.slab("F.Fab").unwrap();
        let g = gerber_fab(doc, &lib, fab).unwrap();
        assert!(
            g.contains("G36*") && g.contains("G37*"),
            "fab fp_poly is a region:\n{g}"
        );
    }

    /// A bottom fab Gerber is **not** mirrored: coordinates are board-frame (the viewer
    /// flips a `B.Fab` document layer), matching the bottom-silk Gerber convention — unlike
    /// the per-side fab *SVG*, which mirrors x. Drive it with a `B.Fab` graphic whose end
    /// point (world x) must appear verbatim in the Gerber.
    #[test]
    fn bottom_fab_gerber_is_not_mirrored() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert(
            "BFAB".into(),
            crate::kicad::import_footprint(
                r#"(footprint "BFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end 1 0) (width 0.12) (layer "B.Fab")))"#,
            )
            .unwrap(),
        );
        let mut slabs = Stackup::default_2layer().slabs;
        let bot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
        slabs.push(Slab {
            name: "B.Fab".into(),
            z: ZRange::new(bot, bot),
            role: Role::Datum,
            material: None,
        });
        let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Instance {
            path: "u".into(),
            part: "BFAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(5, 5),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "bfab")
            .unwrap();
        let doc = h.doc();
        let su = crate::elaborate::stackup(&doc.source);
        let fab = su.slab("B.Fab").unwrap();
        let g = gerber_fab(doc, &lib, fab).unwrap();
        // The line runs from x=5mm to x=6mm (place at 5, end offset +1mm), both in the raw
        // board frame — a mirrored export would place them elsewhere. `%FSLAX46Y46*%` mm =
        // nm, so 6mm is the integer 6000000.
        assert!(g.contains("X6000000"), "unmirrored world x for B.Fab:\n{g}");
        // The fileset names it board-B_Fab.gbr.
        let set = gerber_set(doc, &lib).unwrap();
        assert!(
            set.iter().any(|(n, _)| n == "board-B_Fab.gbr"),
            "bottom fab Gerber named board-B_Fab.gbr"
        );
    }

    /// No fab slab authored (default stackup) ⇒ no fab Gerber in the fileset, and a
    /// fab-layer footprint graphic stays inert — the Decision 15 contract on the Gerber
    /// side (the SVG side is covered by `no_fab_slab_means_no_fab_output_and_invisible_graphics`).
    #[test]
    fn no_fab_slab_means_no_fab_gerber() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert("FAB".into(), fab_footprint());
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "u".into(),
                    part: "FAB".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "u".into(),
                    pos: Point::mm(5, 5),
                },
            ])),
            &lib,
            "no-fab",
        )
        .unwrap();
        let gset = gerber_set(h.doc(), &lib).unwrap();
        assert!(
            gset.iter().all(|(n, _)| !n.contains("Fab")),
            "no fab Gerber in the fileset: {:?}",
            gset.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }

    // --- mask export enters by role (Decision 16 stage 4) -----------------

    /// A custom stackup with a single `Role::Mask` slab exports exactly one mask Gerber,
    /// named from that slab — the mask loop iterates mask slabs by name, not a fixed
    /// `[Top, Bottom]` copper-layer pair.
    #[test]
    fn single_mask_slab_exports_one_mask_gerber() {
        use crate::elaborate::GenDirective as G;
        // A 1-layer stackup: one copper slab and one mask slab above it.
        let slabs = vec![
            Slab {
                name: "F.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            },
            Slab {
                name: "F.Mask".into(),
                z: ZRange::new(35_000, 45_000),
                role: Role::Mask,
                material: None,
            },
        ];
        let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
        source.push(board_rect(Point::mm(0, 0), Point::mm(10, 10)));
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "1mask")
            .unwrap();
        let gset = gerber_set(h.doc(), &lib).unwrap();
        let masks: Vec<&String> = gset
            .iter()
            .map(|(n, _)| n)
            .filter(|n| n.contains("Mask"))
            .collect();
        assert_eq!(masks, vec!["board-F_Mask.gbr"], "exactly one mask Gerber");
    }

    /// Board-level `text` on a fab slab renders on the fab SVG and is **absent** from silk
    /// (F1): the text lowering forward-queries the resolved slab's role rather than
    /// hardcoding `Role::Marking`, so `layer=F.Fab` (a `Role::Datum` slab) lands on fab,
    /// not silk. Before the fix this text shipped visibly on `F_SilkS`.
    #[test]
    fn board_text_on_fab_slab_renders_fab_not_silk() {
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut source = stackup_with_fab();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Text {
            string: "FAB".into(),
            at: Point::mm(4, 10),
            height: MM,
            layer: "F.Fab".into(),
            orient: crate::doc::Orient::IDENTITY,
        });
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(source)),
            &lib,
            "fabtext",
        )
        .unwrap();
        let doc = h.doc();

        // Fab SVG carries the text strokes.
        let set = fab_svg_set(doc, &lib).unwrap();
        assert_eq!(set.len(), 1);
        assert!(
            set[0].1.matches("class=\"fab\"").count() >= 3,
            "fab-slab board text renders as fab strokes:\n{}",
            set[0].1
        );
        // The composite SVG and silk Gerbers must NOT show it as silk.
        let s = svg(doc, &lib).unwrap();
        assert!(
            !s.contains("class=\"silk\""),
            "fab-slab board text must not leak onto silk:\n{s}"
        );
        // The F.SilkS silk Gerber is empty of drawing ops (no D-code selection / strokes).
        let su = crate::elaborate::stackup(&doc.source);
        let silk = su.slab("F.SilkS").unwrap();
        let g = gerber_silk(doc, &lib, silk).unwrap();
        assert!(
            !g.contains("D10*"),
            "no strokes on the silk Gerber for fab-slab text:\n{g}"
        );
    }

    /// Board-level `text` on a silk slab is unchanged by the F1 fix — it still lowers to a
    /// `Role::Marking` silk stroke (silk byte-identity for the default stackup).
    #[test]
    fn board_text_on_silk_slab_unchanged() {
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Text {
                    string: "S".into(),
                    at: Point::mm(4, 10),
                    height: MM,
                    layer: "F.SilkS".into(),
                    orient: crate::doc::Orient::IDENTITY,
                },
            ])),
            &lib,
            "silktext",
        )
        .unwrap();
        let s = svg(h.doc(), &lib).unwrap();
        assert!(
            s.contains("class=\"silk\""),
            "silk-slab board text still renders as silk strokes:\n{s}"
        );
    }

    /// F3: a footprint `F.Fab` graphic on a **flipped** component swaps to `B.Fab`
    /// (`swap_side`) and lands on the bottom fab sheet — the same side derivation copper
    /// uses. With both fab slabs authored, the graphic appears only on `board-B_Fab.svg`.
    #[test]
    fn flipped_component_fab_graphic_lands_on_bottom_sheet() {
        use crate::elaborate::GenDirective as G;
        let mut lib = part_library();
        lib.insert("FAB".into(), fab_footprint()); // authors an F.Fab graphic + text
        // Default stackup + both F.Fab and B.Fab datum slabs.
        let mut slabs = Stackup::default_2layer().slabs;
        let ftop = slabs.iter().find(|s| s.name == "F.Cu").unwrap().z.hi;
        let bbot = slabs.iter().find(|s| s.name == "B.Cu").unwrap().z.lo;
        slabs.push(Slab {
            name: "F.Fab".into(),
            z: ZRange::new(ftop, ftop),
            role: Role::Datum,
            material: None,
        });
        slabs.push(Slab {
            name: "B.Fab".into(),
            z: ZRange::new(bbot, bbot),
            role: Role::Datum,
            material: None,
        });
        let mut source: Vec<G> = slabs.into_iter().map(G::Slab).collect();
        source.push(board_rect(Point::mm(0, 0), Point::mm(20, 20)));
        source.push(G::Instance {
            path: "u".into(),
            part: "FAB".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        source.push(G::Place {
            path: "u".into(),
            pos: Point::mm(10, 10),
        });
        source.push(G::Rotate {
            path: "u".into(),
            orient: crate::doc::Orient::default().flipped(),
        });
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(source)), &lib, "flip")
            .unwrap();
        let doc = h.doc();

        let set = fab_svg_set(doc, &lib).unwrap();
        // Two fab slabs ⇒ two SVGs; the flipped graphic draws on B.Fab, not F.Fab.
        let by_name = |name: &str| set.iter().find(|(n, _)| n == name).map(|(_, c)| c.as_str());
        let f = by_name("board-F_Fab.svg").expect("F.Fab sheet present");
        let b = by_name("board-B_Fab.svg").expect("B.Fab sheet present");
        assert!(
            !f.contains("class=\"fab\""),
            "flipped graphic is NOT on the front sheet:\n{f}"
        );
        assert!(
            b.contains("class=\"fab-bottom\""),
            "flipped graphic swaps to the bottom sheet:\n{b}"
        );
    }

    /// An authored-but-empty fab slab (no fab geometry, no board outline) emits a valid SVG
    /// via the fallback 10mm viewBox — the degenerate path must not panic or produce an
    /// empty viewBox.
    #[test]
    fn empty_fab_slab_emits_valid_svg() {
        use crate::elaborate::GenDirective as G;
        let lib = part_library();
        // A stackup with one copper slab and one fab slab, and NO board / geometry.
        let source = vec![
            G::Slab(Slab {
                name: "F.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            }),
            G::Slab(Slab {
                name: "F.Fab".into(),
                z: ZRange::new(35_000, 35_000),
                role: Role::Datum,
                material: None,
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(source)),
            &lib,
            "emptyfab",
        )
        .unwrap();
        let set = fab_svg_set(h.doc(), &lib).unwrap();
        assert_eq!(set.len(), 1);
        let (name, s) = &set[0];
        assert_eq!(name, "board-F_Fab.svg");
        // Fallback bbox path: a 10mm box + margin ⇒ a 14mm-wide non-degenerate viewBox.
        assert!(
            s.contains("viewBox=\"-2.000000 -2.000000 14.000000 14.000000\""),
            "fallback viewBox:\n{s}"
        );
        assert!(
            s.contains("class=\"outline-bbox\""),
            "fallback outline rect:\n{s}"
        );
        assert!(s.ends_with("</svg>\n"));
    }
}
