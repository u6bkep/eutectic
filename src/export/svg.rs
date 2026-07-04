//! The board SVG backends: the top-view sketch ([`svg`]) and the per-side fab drawing
//! ([`svg_fab`] / [`fab_svg_set`], Decision 15). Both render derived surface geometry in
//! the marking look via the shared [`svg_surface`] arm, and both class/colour copper by a
//! forward stackup query ([`copper_side`] and friends — Decision 13, the side is derived
//! from z, never stored). Coordinates flow through [`fmt_mm`] and the curve-aware path
//! builders in [`super::svg_writer`], keeping the output byte-stable and diffable.

use crate::doc::{MM, Nm, Point};
use crate::geom::{Extent, Role, Shape2D, Slab, Stackup, ZRange};
use crate::part::{PartLib, pin_world};
use crate::region::Region;
use crate::route::Layer;

use super::features::{pours_of, role_features};
use super::placement::part_pin_ids;
use super::svg_writer::{fmt_mm, has_curve, region_svg_d, svg_path_d, xml_escape};

/// The board as a filled [`Region`] (outline ∖ cutouts) carried by tier-1 source (the
/// shared reader), if any. Rounded/concave outlines and cutouts alike — polygonized by
/// the region kernel (Decision 16b), so a curved board edge draws as a fine polyline.
pub(crate) fn source_board(doc: &crate::doc::Doc) -> Option<Region> {
    crate::elaborate::board_region(&doc.source)
}

/// A board sketch as deterministic SVG: the board outline (the source `Board`
/// directive if present, else the bounding box of placed geometry), each component
/// drawn at its position with its pin pads (via [`pin_world`]) and an id label.
///
/// The model's y axis points up (ECAD convention); SVG's points down, so y is
/// flipped within the content bounds to keep the sketch upright. All coordinates
/// are six-decimal mm via [`fmt_mm`]; element order follows `EntityId` order. No
/// timestamps or other ambient state — byte-stable and diffable.
pub fn svg(doc: &crate::doc::Doc, lib: &PartLib) -> Result<String, String> {
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
/// [`super::gerber::gerber_fab`] is the RS-274X sibling of this pass — same
/// [`super::gerber::datum_slabs`] iteration, Gerber instead of SVG (board-frame, so it
/// does not mirror a bottom sheet the way this one does).
pub fn svg_fab(doc: &crate::doc::Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
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
/// per authored [`Role::Datum`] slab (top-down; see [`super::gerber::datum_slabs`]), named
/// from the slab with the [`super::gerber::slab_file`] convention (`.`→`_`) that the
/// Gerbers use. Empty for the default stackup (no fab slab). `(filename, content)` pairs,
/// stable order; fallible because the per-slab render resolves slab names (Decision 13).
pub fn fab_svg_set(doc: &crate::doc::Doc, lib: &PartLib) -> Result<Vec<(String, String)>, String> {
    let su = crate::elaborate::stackup(&doc.source);
    let mut out = Vec::new();
    for slab in super::gerber::datum_slabs(&su) {
        out.push((
            format!("board-{}.svg", super::gerber::slab_file(&slab.name)),
            svg_fab(doc, lib, &slab)?,
        ));
    }
    Ok(out)
}
