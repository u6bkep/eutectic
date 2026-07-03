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
    for pf in pour_fills_of(doc, lib) {
        let d = region_svg_d(&pf.fill, &flip);
        if !d.is_empty() {
            out.push_str(&format!(
                "  <path class=\"pour pour-{}\" data-net=\"{}\" d=\"{}\" fill=\"{}\" fill-opacity=\"0.25\" fill-rule=\"evenodd\" stroke=\"none\"/>\n",
                layer_class(pf.layer),
                xml_escape(&pf.net.0),
                d,
                layer_color(pf.layer),
            ));
        }
    }

    // The stackup resolves each pad's layer-relative copper to absolute z, so pads
    // fan out correctly (a through-hole pad becomes one conductor feature per copper
    // slab). Today this is the default 2-layer stackup; the reader is the one place
    // that changes when authored stackups land.
    let su = crate::elaborate::stackup(&doc.source);
    // Footprint auto-text (Decision 14): `refdes` is a whole-document query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);

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
            let texts = crate::part::text_features(def, c, &su, rd, &lbl);
            for f in graphics.into_iter().chain(texts) {
                if f.role != Role::Marking {
                    continue;
                }
                let Extent::Prism { shape, z } = &f.extent;
                out.push_str(&svg_silk(shape, &flip, is_bottom_silk(&su, z)));
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

    // Silkscreen: lowered board text. Each derived stroke-font `Role::Marking` feature
    // (from the converged `features` view) is drawn as a thin stroked centreline
    // polyline, giving a silk-layer look. Source order ⇒ deterministic.
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role != Role::Marking {
            continue;
        }
        let Extent::Prism { shape, z } = &nf.feature.extent;
        out.push_str(&svg_silk(shape, &flip, is_bottom_silk(&su, z)));
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
        // A filled-area marking (e.g. TTF outline text) is an even-odd `<path>` so its
        // counters read as voids.
        Shape2D::Area { region } => format!(
            "  <path class=\"{class}\" d=\"{}\" fill=\"{color}\" fill-rule=\"evenodd\" stroke=\"none\"/>\n",
            region_svg_d(region, flip),
        ),
    }
}

/// Is a marking feature at z-range `z` on the **bottom** silk side? A marking slab
/// outboard of (at or below) the bottom copper is bottom silk; anything else is top.
/// A forward query against the stackup — the side is derived from z, never stored
/// (Decision 13). Falls back to top when there is no bottom copper to compare against.
fn is_bottom_silk(su: &Stackup, z: &ZRange) -> bool {
    su.bottom_copper().is_some_and(|bot| z.hi <= bot.lo)
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

/// The derived copper-pour fills, for export. Builds the membership netlist from the
/// materialized nets (roles are irrelevant to pours) and calls the shared
/// [`crate::route::pour_fills`]. Pure — same inputs, same fills.
fn pour_fills_of(doc: &Doc, lib: &PartLib) -> Vec<crate::route::PourFill> {
    use crate::part::PinRole;
    let netlist = doc
        .nets
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
        .collect();
    crate::route::pour_fills(doc, lib, &netlist, &crate::route::DesignRules::default())
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
fn copper_layers(doc: &Doc, lib: &PartLib) -> Vec<Layer> {
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
    for pf in pour_fills_of(doc, lib) {
        set.insert(pf.layer);
    }
    set.into_iter().collect()
}

/// Every component pad copper region that flashes on `layer`, as `(world centre,
/// aperture)`, in `(EntityId, pin-declaration, copper-region)` order. Each pad's
/// real geometry is transformed to world space and reduced to a flashable aperture; a
/// region flashes only on the layers it occupies. Toy-library pins (`pad: None`)
/// contribute nothing.
fn component_pad_flashes(doc: &Doc, lib: &PartLib, layer: Layer) -> Vec<(Point, Aperture)> {
    // Derive each pad's converged copper features and flash those whose slab is this
    // Gerber `layer`. `pad_features` already world-maps + assigns z; we match the slab
    // z of `layer`, so a Through pad flashes on every copper layer and an SMD pad only
    // on its own — exactly the old `pad_on_layer` selection, now off the Feature model.
    let su = crate::elaborate::stackup(&doc.source);
    let Some(target_z) = crate::route::layer_z(&su, layer) else {
        return Vec::new(); // this layer is not in the stackup
    };
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
pub fn gerber_layer(doc: &Doc, lib: &PartLib, layer: Layer) -> String {
    let traces: Vec<&Trace> = doc.traces.values().filter(|t| t.layer == layer).collect();
    let vias: Vec<&Via> = doc.vias.values().filter(|v| v.spans(layer)).collect();
    let pads = component_pad_flashes(doc, lib, layer);

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
    for pf in pour_fills_of(doc, lib).iter().filter(|p| p.layer == layer) {
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
    let tools: BTreeMap<Nm, u32> = dias
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, 1 + i as u32))
        .collect();

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

/// The solder-mask Gerber for one outer side (`Top`→`F.Mask`, `Bottom`→`B.Mask`),
/// derived **forward** from the model — never recomputed from a parallel rule set
/// (Decision 13). The mask slab for the side is resolved by z-position
/// ([`Stackup::top_mask`]/[`Stackup::bottom_mask`]); the file draws the **openings**
/// (the fab inverts to the mask coverage — a draw-the-openings convention that stays an
/// export-format detail):
///
/// - Pad openings: the [`Role::Void`] features [`PinDef::pad_features`] emits at the
///   mask slab's z (the pad copper already inflated by [`geom::MASK_EXPANSION`]) —
///   flashed as their aperture, so on the default stackup this is byte-for-byte the old
///   pad-opening output. A pad's **drill** `Void` is a through-cut at the *full* stackup
///   z, not the mask z, so it is not one of these — and it must not be: it sits inside
///   the pad opening (drawing it again would double the flash) and its home is the
///   Excellon file. Through-hole pads open both sides because `pad_features` places an
///   opening at each side's mask slab.
/// - Board cutouts: milled through the whole stack, so they remove mask over their whole
///   area — drawn as `G36`/`G37` region fills. This is new (the old parallel rule missed
///   cutouts entirely); it is strictly additive.
///
/// A side with no mask slab in the stackup opens nothing (an empty mask layer). Object
/// order is `(EntityId, pin, region)` then cutouts — fully deterministic. Fallible
/// because the cutout query runs the slab-name materialization gate (Decision 13).
pub fn gerber_mask(doc: &Doc, lib: &PartLib, side: Layer) -> Result<String, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let mask_slab = mask_slab_of(&su, side);
    let mask_z = mask_slab.map(|s| s.z);

    // Pad openings: `Void`s whose z lies within this mask slab (pad_features places the
    // inflated-copper opening there). A through-cut `Void` (a drill) extends past the
    // slab and is excluded — subsumed by the opening, and belongs to the drill file.
    let mut openings: Vec<(Point, Aperture)> = Vec::new();
    // Board cutouts remove mask over their whole area. A cutout is now a *hole* in the
    // board region (Decision 16b/c), not a `Void` feature, so the openings come from
    // `board_region().holes()` — the CW cutout rings — as region fills. A cutout is a
    // full-stack through-cut, so it always pierces a present mask slab; the `mask_z`
    // gate is just "does this side have a mask".
    let mut cutout_holes = Region::default();
    if let Some(mask_z) = mask_z {
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
        if let Some(region) = crate::elaborate::board_region(&doc.source) {
            cutout_holes = region.holes();
        }
    }

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
    out.push_str(&format!("G04 {} *\n", mask_name(&su, side)));
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

/// The resolved solder-mask [`Slab`] for a side — the `Role::Mask` slab immediately
/// outboard of the outer copper (by z-position; [`Stackup::top_mask`]/[`bottom_mask`]),
/// so a custom-named mask slab resolves like the default `F.Mask`/`B.Mask`. `None` if
/// that side has no mask.
fn mask_slab_of(su: &Stackup, side: Layer) -> Option<&Slab> {
    let z = match side {
        Layer::Bottom => su.bottom_mask(),
        _ => su.top_mask(),
    }?;
    su.slabs.iter().find(|s| s.role == Role::Mask && s.z == z)
}

/// The mask filename token for a side: the resolved mask slab's name (`.`→`_`, so a
/// custom `TopMask` keeps its name), else the conventional `F_Mask`/`B_Mask` fallback
/// when the side carries no mask slab. The default stackup's `F.Mask`/`B.Mask` yield the
/// unchanged `F_Mask`/`B_Mask`.
fn mask_name(su: &Stackup, side: Layer) -> String {
    match mask_slab_of(su, side) {
        Some(s) => slab_file(&s.name),
        None if side == Layer::Bottom => "B_Mask".to_string(),
        None => "F_Mask".to_string(),
    }
}

/// The KiCad-style filename token for a named slab: the slab name with `.`→`_`
/// (`F.SilkS`→`F_SilkS`), matching the `F_Cu` convention of [`layer_file`]. Names the
/// marking (silk) and solder-mask Gerbers from their resolved slab (see [`mask_name`]).
fn slab_file(name: &str) -> String {
    name.replace('.', "_")
}

/// Every world-frame [`Role::Marking`] feature of the board: lowered board text (from
/// the converged [`crate::elaborate::features`] view) plus each placed component's
/// footprint silk ([`crate::part::graphic_features`], side-swapped + placed). The single
/// forward source of silk geometry the mask/silk exporters and the SVG render share.
/// Fallible because the board-text lowering resolves slab names (an unknown one is a
/// hard error, per Decision 13).
fn marking_features(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
) -> Result<Vec<crate::geom::Feature>, String> {
    let mut out: Vec<crate::geom::Feature> = Vec::new();
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role == Role::Marking {
            out.push(nf.feature);
        }
    }
    // Footprint auto-text (Decision 14) rides the same Marking-filtered silk path as
    // graphics; `refdes` is a whole-document query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);
    for (id, c) in &doc.components {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for f in crate::part::graphic_features(def, c, su) {
            if f.role == Role::Marking {
                out.push(f);
            }
        }
        let rd = refdes.get(id).map(String::as_str).unwrap_or("");
        let lbl = crate::annotate::label(c, def, &reg);
        for f in crate::part::text_features(def, c, su, rd, &lbl) {
            if f.role == Role::Marking {
                out.push(f);
            }
        }
    }
    Ok(out)
}

/// One silkscreen Gerber for a marking [`Slab`], drawing the [`Role::Marking`] features
/// whose z intersects the slab (forward query per slab — Decision 13). A
/// [`Shape2D::Stroke`] (`fp_line`/`fp_arc`/text) draws as its centreline with a round
/// aperture of the stroke's pen diameter (`radius * 2`); a [`Shape2D::Polygon`]
/// (`fp_poly`/`fp_rect`) is a filled area, drawn as a `G36`/`G37` region. Aperture codes
/// run from 10 in `Ord` order; object order follows [`marking_features`] — deterministic.
pub fn gerber_silk(doc: &Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let feats: Vec<Shape2D> = marking_features(doc, lib, &su)?
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

/// The marking (silk) slabs of the stackup, ordered **top-down** (highest z first) so a
/// board's fileset lists `F.SilkS` before `B.SilkS`, mirroring `F_Cu`/`B_Cu` and
/// `F_Mask`/`B_Mask` ordering.
fn marking_slabs(su: &Stackup) -> Vec<Slab> {
    let mut m: Vec<Slab> = su
        .slabs
        .iter()
        .filter(|s| s.role == Role::Marking)
        .cloned()
        .collect();
    m.sort_by_key(|s| std::cmp::Reverse(s.z.hi));
    m
}

/// The full deterministic fab fileset: one Gerber per copper layer (`board-F_Cu.gbr`
/// …) in stack-up order, the two solder masks (`board-F_Mask.gbr` / `board-B_Mask.gbr`),
/// one silk Gerber per marking slab (`board-F_SilkS.gbr` / `board-B_SilkS.gbr`, top-down),
/// the `board-Edge_Cuts.gbr` outline, and the `board.drl` Excellon drill program.
/// `(filename, content)` pairs; no timestamps, stable order. Fallible because the silk
/// layers lower board text through the slab-name materialization gate (Decision 13).
pub fn gerber_set(doc: &Doc, lib: &PartLib) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for layer in copper_layers(doc, lib) {
        out.push((
            format!("board-{}.gbr", layer_file(layer)),
            gerber_layer(doc, lib, layer),
        ));
    }
    let su = crate::elaborate::stackup(&doc.source);
    for side in [Layer::Top, Layer::Bottom] {
        out.push((
            format!("board-{}.gbr", mask_name(&su, side)),
            gerber_mask(doc, lib, side)?,
        ));
    }
    for slab in marking_slabs(&su) {
        out.push((
            format!("board-{}.gbr", slab_file(&slab.name)),
            gerber_silk(doc, lib, &slab)?,
        ));
    }
    out.push((
        "board-Edge_Cuts.gbr".to_string(),
        gerber_edge_cuts(doc, lib),
    ));
    out.push(("board.drl".to_string(), excellon_drill(doc)));
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
                "board.drl",
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
        assert_eq!(
            gerber_layer(&doc, &lib, Layer::Top),
            gerber_layer(&doc, &lib, Layer::Top)
        );
        assert_eq!(excellon_drill(&doc), excellon_drill(&doc));
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
        let top = gerber_layer(doc, &lib, Layer::Top);
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
        let top = gerber_layer(&doc, &lib, Layer::Top);
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
        assert!(!gerber_layer(&doc, &lib, Layer::Bottom).contains("G36*"));
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
        let f = gerber_mask(&doc, &lib, Layer::Top).unwrap();
        assert!(f.contains("F_Mask"));
        assert!(
            f.contains("R,0.700000X1.300000*%"),
            "expanded rect opening:\n{f}"
        );
        assert!(f.contains("C,0.900000*%"), "expanded circle opening:\n{f}");
        assert_eq!(f.matches("D03*").count(), 2, "one opening per pad");
        // No bottom-side pads ⇒ no openings on B_Mask.
        assert_eq!(
            gerber_mask(&doc, &lib, Layer::Bottom)
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
            gerber_mask(doc, &lib, Layer::Top)
                .unwrap()
                .matches("D03*")
                .count(),
            1
        );
        assert_eq!(
            gerber_mask(doc, &lib, Layer::Bottom)
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
        let f = gerber_mask(h.doc(), &lib, Layer::Top).unwrap();
        assert!(f.contains("G36*"), "cutout opens a mask region:\n{f}");
        assert!(f.contains("G37*"), "region closes:\n{f}");
        // The cutout corner (12mm) is drawn in the region contour (nm coordinates).
        assert!(f.contains("X12000000Y12000000"), "cutout boundary:\n{f}");
        // Both faces lose mask over a through cutout.
        assert!(
            gerber_mask(h.doc(), &lib, Layer::Bottom)
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
}
