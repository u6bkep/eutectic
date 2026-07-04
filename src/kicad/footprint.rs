//! Import a `.kicad_mod` footprint into a [`PartDef`] — the pad→pin geometry plus
//! non-copper graphics, courtyard, and text anchors. See the crate module docs
//! ([`crate::kicad`]) for the full mapping contract.
//!
//! This module also owns the low-level graphic-primitive readers (`prim_xy`,
//! `gr_arc_points`, `rotate_point`, …) that the board-outline importer
//! ([`super::outline`]) reuses; those are `pub(crate)`.

use crate::doc::{Nm, Orient, Point};
use crate::geom::Shape2D;
use crate::part::{
    Drill, FpGraphic, FpText, FpTextKind, PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole,
};
use std::collections::BTreeMap;

use super::sexp::{Sexp, mm_to_nm, read, tokenize};

/// Parse a `.kicad_mod` S-expression and produce a [`PartDef`].
///
/// See the module docs for the pad→pin mapping rules (shared names deduped,
/// unnamed pads skipped, roles defaulted to [`PinRole::Passive`], no interfaces).
pub fn import_footprint(text: &str) -> Result<PartDef, String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let items = root.as_list().ok_or("top-level expression is not a list")?;

    // Header: `(footprint "name" ...)` or legacy `(module name ...)`.
    match items.first().and_then(Sexp::as_atom) {
        Some("footprint") | Some("module") => {}
        other => return Err(format!("expected 'footprint' or 'module', got {other:?}")),
    }
    let name = items
        .get(1)
        .and_then(Sexp::as_atom)
        .ok_or("footprint is missing its name")?
        .to_string();
    if name.is_empty() {
        return Err("footprint name is empty".into());
    }

    let mut pins: Vec<PinDef> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    for item in items {
        let Some(pad) = item.list_headed("pad") else {
            continue;
        };
        // (pad <name> <type> <shape> ... (at x y [angle]) ...)
        let pad_name = pad.get(1).and_then(Sexp::as_atom).unwrap_or("");
        if pad_name.is_empty() {
            continue; // unnamed: thermal/exposed/mechanical — no electrical identity
        }
        if seen.insert(pad_name.to_string(), ()).is_some() {
            continue; // shared pad name: keep first occurrence
        }
        let at = pad
            .iter()
            .find_map(|s| s.list_headed("at"))
            .ok_or_else(|| format!("pad {pad_name:?} has no (at ...)"))?;
        let x = at
            .get(1)
            .and_then(Sexp::as_atom)
            .ok_or_else(|| format!("pad {pad_name:?} (at ...) missing x"))?;
        let y = at
            .get(2)
            .and_then(Sexp::as_atom)
            .ok_or_else(|| format!("pad {pad_name:?} (at ...) missing y"))?;
        let offset = Point {
            x: mm_to_nm(x)?,
            y: mm_to_nm(y)?,
        };
        // Real pad copper + drill geometry, in component-local coords centred at the
        // pad's `(at)`. The shape/size/drill/layers/rotation are all lifted here.
        let pad = parse_pad_geometry(pad, offset)?;
        // A bare footprint has no functional naming: name == number == the pad id.
        pins.push(PinDef {
            name: pad_name.to_string(),
            number: pad_name.to_string(),
            role: PinRole::Passive,
            offset,
            pad,
        });
    }

    // Footprint graphics: silkscreen + fab → `graphics` (side-relative slab names; the
    // role is taken from the resolved slab at lowering, so fab graphics materialize only
    // if the stackup carries a fab slab — Decision 15), and a courtyard outline → the
    // authoritative `courtyard` (Decision 10). Still skipped: `fp_text`/auto-text (a
    // separate branch) and paste (Decision 15: derived at export) — see the module doc.
    let mut graphics: Vec<FpGraphic> = Vec::new();
    let mut courtyard: Option<Shape2D> = None;
    for item in items {
        let Some((shape, layer)) = parse_fp_graphic(item)? else {
            continue;
        };
        match layer.as_str() {
            "F.SilkS" | "B.SilkS" | "F.Fab" | "B.Fab" => graphics.push(FpGraphic { shape, layer }),
            // A courtyard is a single closed outline. We take a `fp_poly`/`fp_rect`
            // (a `Shape2D::Polygon`); loose `fp_line`/`fp_arc` courtyard segments are
            // not stitched into a loop yet, so they are ignored (noted). Last one wins.
            "F.CrtYd" | "B.CrtYd" if matches!(shape, Shape2D::Polygon { .. }) => {
                courtyard = Some(shape);
            }
            _ => {}
        }
    }

    // Footprint text → `texts` (Decision 14): `fp_text reference|value|user` and the v7
    // `property "Reference"|"Value"` form. The placeholder string ("REF**"/the value
    // placeholder) is discarded — a Reference/Label anchor re-derives its string at
    // lowering; only `user` text keeps its literal. `hide` anchors import as data (they
    // round-trip) but produce no features.
    let mut texts: Vec<FpText> = Vec::new();
    for item in items {
        if let Some(t) = parse_fp_text(item)? {
            texts.push(t);
        }
    }

    Ok(PartDef {
        name,
        pins,
        interfaces: BTreeMap::new(),
        graphics,
        texts,
        courtyard,
        // The importer does not infer class from a footprint (Decision 14, out of scope).
        class: None,
    })
}

/// Parse one footprint text node into an [`FpText`] anchor, or `Ok(None)` if it isn't
/// footprint text (or lacks a `(layer …)`). Two forms:
///
/// - classic `(fp_text reference|value|user "STR" (at x y [rot]) (layer L) [hide]
///   (effects (font (size H W) (thickness T))))`, and
/// - v7 `(property "Reference"|"Value" "STR" (at …) (layer L) [(hide yes)] (effects …))`.
///
/// Mapping (Decision 14): `reference`/`Reference` → [`FpTextKind::Reference`] (placeholder
/// discarded), `value`/`Value` → [`FpTextKind::Label`] (placeholder discarded), `user` →
/// [`FpTextKind::Literal`] keeping the string — except a `user` string that is *exactly*
/// the `${REFERENCE}`/`${VALUE}` KiCad text variable resolves to the live Reference/Label
/// anchor (fab layers commonly echo the refdes this way); mixed content stays literal
/// (see [`text_kind_from_user`]). Height is the font `(size H …)` height component
/// (default 1 mm if absent); the stroke `(thickness …)` is **ignored** — the pen is the
/// `height / 8` rule (Decision 14). `(at … rot)` becomes a local about-z [`Orient`] (exact
/// for cardinals). The layer name is kept as imported (side-relative). Other `property`
/// names (Footprint/Datasheet/…) are footprint metadata, not silk, and return `Ok(None)`.
fn parse_fp_text(item: &Sexp) -> Result<Option<FpText>, String> {
    let Some(list) = item.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let kind = match head {
        "fp_text" => match list.get(1).and_then(Sexp::as_atom).unwrap_or("") {
            "reference" => FpTextKind::Reference,
            "value" => FpTextKind::Label,
            "user" => text_kind_from_user(list.get(2).and_then(Sexp::as_atom).unwrap_or("")),
            _ => return Ok(None),
        },
        "property" => match list.get(1).and_then(Sexp::as_atom).unwrap_or("") {
            "Reference" => FpTextKind::Reference,
            "Value" => FpTextKind::Label,
            _ => return Ok(None), // metadata property, not silk text
        },
        _ => return Ok(None),
    };
    let Some(layer) = layer_name(list) else {
        return Ok(None);
    };
    let at = prim_xy(list, "at")?.unwrap_or(Point { x: 0, y: 0 });
    let rot = list
        .iter()
        .find_map(|s| s.list_headed("at"))
        .and_then(|a| a.get(3))
        .and_then(Sexp::as_atom)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    // Cardinal rotations get the tiny exact quaternion; off-axis angles are approximated.
    let orient = Orient::from_deg(rot as i32).unwrap_or_else(|| Orient::from_angle_deg(rot));
    let height = text_font_height(list).unwrap_or(1_000_000); // KiCad default text size ≈ 1 mm
    Ok(Some(FpText {
        kind,
        at,
        height,
        layer,
        orient,
        hide: text_hidden(list),
    }))
}

/// Map a `fp_text user` string to a kind: the KiCad text variables `${REFERENCE}` and
/// `${VALUE}`, matched as the **whole** string, become the live Reference/Label anchors;
/// anything else (including mixed content like `X ${REFERENCE}`) stays a verbatim literal.
fn text_kind_from_user(s: &str) -> FpTextKind {
    match s {
        "${REFERENCE}" => FpTextKind::Reference,
        "${VALUE}" => FpTextKind::Label,
        _ => FpTextKind::Literal(s.to_string()),
    }
}

/// A footprint text's font **height** in nm: the first component of
/// `(effects (font (size H W) …))` (KiCad lists height then width). `None` if absent.
fn text_font_height(list: &[Sexp]) -> Option<Nm> {
    list.iter()
        .find_map(|s| s.list_headed("effects"))
        .and_then(|eff| eff.iter().find_map(|s| s.list_headed("font")))
        .and_then(|font| font.iter().find_map(|s| s.list_headed("size")))
        .and_then(|size| size.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
}

/// Is a footprint text hidden? Both the classic bare `hide` atom (at the text level) and
/// the v7 `(hide yes)` list — at the text level or nested in `(effects …)` — count.
/// `(hide no)` is explicitly not hidden.
fn text_hidden(list: &[Sexp]) -> bool {
    let hidden_in = |l: &[Sexp]| {
        l.iter().any(|s| s.as_atom() == Some("hide"))
            || l.iter()
                .find_map(|s| s.list_headed("hide"))
                .is_some_and(|h| h.get(1).and_then(Sexp::as_atom) != Some("no"))
    };
    hidden_in(list)
        || list
            .iter()
            .find_map(|s| s.list_headed("effects"))
            .is_some_and(hidden_in)
}

/// Parse one footprint graphic (`fp_line`/`fp_arc`/`fp_circle`/`fp_poly`/`fp_rect`)
/// into its component-local [`Shape2D`] + slab layer name. Coordinates are already in
/// the footprint frame (no pad-centre offset), so this reuses the `gr_*` point readers
/// with a zero centre. Stroke width comes from `(stroke (width w))` (modern) or a bare
/// `(width w)` (legacy) and, per this crate's convention, is baked into the shape's
/// Minkowski radius — `fp_line`→capsule, `fp_arc`→arc stroke (both `width/2`); a
/// zero-width stroke carries no ink ⇒ `Ok(None)`. `fp_rect`/`fp_poly` build the filled
/// polygon; `fp_circle` builds a filled disc (an outline-only circle is approximated as
/// filled — the same simplification the custom-pad `gr_circle` path makes). `Ok(None)`
/// for any other head or a graphic with no `(layer …)`.
fn parse_fp_graphic(item: &Sexp) -> Result<Option<(Shape2D, String)>, String> {
    let Some(list) = item.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let origin = Point { x: 0, y: 0 };
    let width = graphic_width(list);
    let shape = match head {
        "fp_line" => {
            let s = prim_xy(list, "start")?.ok_or("fp_line missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_line missing (end …)")?;
            (width > 0).then(|| Shape2D::capsule(s, e, width / 2))
        }
        "fp_arc" => {
            if width <= 0 {
                None
            } else {
                let (start, mid, end) = gr_arc_points(list, origin)?;
                Some(Shape2D::arc(start, mid, end, width))
            }
        }
        "fp_circle" => {
            let c = prim_xy(list, "center")?.ok_or("fp_circle missing (center …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_circle missing (end …)")?;
            let r = dist_nm(c, e);
            (r > 0).then(|| Shape2D::disc(c, r))
        }
        "fp_rect" => {
            let s = prim_xy(list, "start")?.ok_or("fp_rect missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_rect missing (end …)")?;
            Some(Shape2D::polygon(vec![
                s,
                Point { x: e.x, y: s.y },
                e,
                Point { x: s.x, y: e.y },
            ]))
        }
        "fp_poly" => {
            let pts = prim_pts(list)?;
            (pts.len() >= 3).then(|| Shape2D::polygon(pts))
        }
        _ => return Ok(None),
    };
    let (Some(shape), Some(layer)) = (shape, layer_name(list)) else {
        return Ok(None);
    };
    Ok(Some((shape, layer)))
}

/// A footprint graphic's stroke width in nm: modern `(stroke (width w) …)` or the
/// legacy bare `(width w)`. `0` (⇒ a filled, unstroked shape) if neither is present.
fn graphic_width(list: &[Sexp]) -> Nm {
    if let Some(w) = list
        .iter()
        .find_map(|s| s.list_headed("stroke"))
        .and_then(|st| st.iter().find_map(|s| s.list_headed("width")))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
    {
        return w;
    }
    prim_width(list)
}

/// A graphic item's `(layer "X")` name (quoted or bare), if present.
fn layer_name(list: &[Sexp]) -> Option<String> {
    list.iter()
        .find_map(|s| s.list_headed("layer"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .map(str::to_string)
}

/// Lift a pad's real copper + drill geometry out of a
/// `(pad <name> <type> <shape> (at x y [angle]) (size w h) (layers …) (drill …) …)`
/// node, in component-local coordinates centred at `center` (the pad's `(at)`).
///
/// `circle`/`rect`/`roundrect`/`oval` build exact [`Shape2D`]s; `trapezoid`/`custom`/
/// `chamfered_rect` and any other token fall back to the bounding rectangle — a
/// conservative copper extent. (Full custom `(primitives …)` import is a follow-up;
/// the [`PadGeo`] representation already supports compound pads as a union.) The pad
/// `(at)` angle is baked into the geometry — exact for cardinal rotations, off-axis
/// angles float-rotated and rounded to nm *at import* (like mm→nm). A pad with no
/// `(size …)` and no `(drill …)` yields `None`.
fn parse_pad_geometry(pad: &[Sexp], center: Point) -> Result<Option<PadGeo>, String> {
    let pad_type = pad.get(2).and_then(Sexp::as_atom).unwrap_or("");
    let shape_tok = pad.get(3).and_then(Sexp::as_atom).unwrap_or("");
    let angle = pad
        .iter()
        .find_map(|s| s.list_headed("at"))
        .and_then(|at| at.get(3))
        .and_then(Sexp::as_atom)
        .and_then(|a| a.parse::<f64>().ok())
        .unwrap_or(0.0);

    let drill = parse_drill(pad, center, angle)?;
    let layers = pad_layers(pad, pad_type);

    let copper = if let Some(size) = pad.iter().find_map(|s| s.list_headed("size")) {
        let w = mm_to_nm(
            size.get(1)
                .and_then(Sexp::as_atom)
                .ok_or("pad (size …) missing width")?,
        )?;
        let h = mm_to_nm(
            size.get(2)
                .and_then(Sexp::as_atom)
                .ok_or("pad (size …) missing height")?,
        )?;
        let shapes: Vec<Shape2D> = match shape_tok {
            "circle" => vec![Shape2D::disc(center, w / 2)],
            "roundrect" => {
                let rratio = pad
                    .iter()
                    .find_map(|s| s.list_headed("roundrect_rratio"))
                    .and_then(|l| l.get(1))
                    .and_then(Sexp::as_atom)
                    .and_then(|a| a.parse::<f64>().ok())
                    .unwrap_or(0.25);
                let r = ((w.min(h) as f64) * rratio).round() as Nm;
                vec![Shape2D::round_rect(center, w, h, r)]
            }
            "oval" => vec![oval_shape(center, w, h)],
            // A custom pad is the union of its anchor + `(primitives …)` — including
            // `gr_arc` edges, now that `Shape2D` carries arcs.
            "custom" => parse_custom_copper(pad, center, w, h)?,
            // trapezoid / chamfered_rect / …: bounding rectangle (a documented
            // conservative fallback; only `custom` gets exact compound geometry).
            _ => vec![Shape2D::rect(center, w, h)],
        };
        // The pad `(at)` angle rotates the whole compound shape.
        shapes
            .into_iter()
            .map(|s| PadCopper {
                shape: rotate_shape(s, center, angle),
                layers,
            })
            .collect()
    } else {
        Vec::new()
    };

    if copper.is_empty() && drill.is_none() {
        return Ok(None);
    }
    Ok(Some(PadGeo { copper, drill }))
}

/// An oval/pill pad of size `w`×`h` centred at `c`: a capsule along the longer axis
/// (a circle when `w == h`).
fn oval_shape(c: Point, w: Nm, h: Nm) -> Shape2D {
    if w == h {
        Shape2D::disc(c, w / 2)
    } else if w > h {
        let dx = (w - h) / 2;
        Shape2D::capsule(
            Point {
                x: c.x - dx,
                y: c.y,
            },
            Point {
                x: c.x + dx,
                y: c.y,
            },
            h / 2,
        )
    } else {
        let dy = (h - w) / 2;
        Shape2D::capsule(
            Point {
                x: c.x,
                y: c.y - dy,
            },
            Point {
                x: c.x,
                y: c.y + dy,
            },
            w / 2,
        )
    }
}

/// The copper of a `custom` pad: its anchor shape (the `(size …)` rectangle, or a disc
/// for `(anchor circle)`) **unioned** with every `(primitives …)` element, in
/// pre-rotation world coords (centred at the pad `(at)`). KiCad renders a custom pad as
/// exactly this union; [`PadGeo::copper`] is already a `Vec` for it. Unknown primitive
/// kinds (e.g. `gr_text`) are skipped. The pad `(at)` rotation is applied by the caller.
fn parse_custom_copper(pad: &[Sexp], center: Point, w: Nm, h: Nm) -> Result<Vec<Shape2D>, String> {
    let anchor = pad
        .iter()
        .find_map(|s| s.list_headed("options"))
        .and_then(|o| o.iter().find_map(|s| s.list_headed("anchor")))
        .and_then(|a| a.get(1))
        .and_then(Sexp::as_atom)
        .unwrap_or("rect");
    let mut shapes = vec![match anchor {
        "circle" => Shape2D::disc(center, w.min(h) / 2),
        _ => Shape2D::rect(center, w, h),
    }];
    if let Some(prims) = pad.iter().find_map(|s| s.list_headed("primitives")) {
        for prim in &prims[1..] {
            if let Some(shape) = parse_primitive(prim, center)? {
                shapes.push(shape);
            }
        }
    }
    Ok(shapes)
}

/// One custom-pad primitive → a [`Shape2D`] in pre-rotation world coords (`center` +
/// the primitive's pad-local coordinates). Handles `gr_circle` / `gr_line` / `gr_rect`
/// / `gr_poly` / `gr_arc`; other kinds (text, etc.) return `None`. Filled primitives
/// become filled shapes; stroked ones (`width > 0`) become the stroke ⊕ width/2.
fn parse_primitive(prim: &Sexp, center: Point) -> Result<Option<Shape2D>, String> {
    let Some(list) = prim.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let off = |p: Point| Point {
        x: center.x + p.x,
        y: center.y + p.y,
    };
    Ok(match head {
        "gr_circle" => {
            let c = prim_xy(list, "center")?.ok_or("gr_circle missing (center …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_circle missing (end …)")?;
            let r = dist_nm(c, e);
            (r > 0).then(|| Shape2D::disc(off(c), r))
        }
        "gr_line" => {
            let s = prim_xy(list, "start")?.ok_or("gr_line missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_line missing (end …)")?;
            let width = prim_width(list);
            (width > 0).then(|| Shape2D::capsule(off(s), off(e), width / 2))
        }
        "gr_rect" => {
            let s = prim_xy(list, "start")?.ok_or("gr_rect missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_rect missing (end …)")?;
            Some(Shape2D::polygon(vec![
                off(s),
                off(Point { x: e.x, y: s.y }),
                off(e),
                off(Point { x: s.x, y: e.y }),
            ]))
        }
        "gr_poly" => {
            let pts = prim_pts(list)?;
            (pts.len() >= 3).then(|| Shape2D::polygon(pts.into_iter().map(off).collect()))
        }
        "gr_arc" => parse_gr_arc(list, center)?,
        _ => None,
    })
}

/// A `gr_arc` primitive → an arc-stroke [`Shape2D`]. Two KiCad encodings:
///   - **3-point** `(start)(mid)(end)`: used directly (matches our [`Seg::Arc`](crate::geom::Seg)).
///   - **legacy** `(start = centre)(end = arc start point)(angle = swept °)`: the end
///     and mid are the arc-start rotated by `angle` and `angle/2` about the centre.
///     Using the *same* `angle` for both guarantees the mid lands on the swept arc
///     whatever the sign convention. Zero-width arcs carry no copper ⇒ `None`.
fn parse_gr_arc(list: &[Sexp], center: Point) -> Result<Option<Shape2D>, String> {
    let width = prim_width(list);
    if width <= 0 {
        return Ok(None);
    }
    let (start, mid, end) = gr_arc_points(list, center)?;
    Ok(Some(Shape2D::arc(start, mid, end, width)))
}

/// The three lattice points `(start, mid, end)` of a `gr_arc`, in `center`-offset
/// coords, normalising both KiCad encodings (the shared core of [`parse_gr_arc`] and
/// the board-outline importer, neither of which cares about stroke width):
///   - **3-point** `(start)(mid)(end)`: used directly (matches our [`Seg::Arc`](crate::geom::Seg)).
///   - **legacy** `(start = centre)(end = arc start)(angle = swept °)`: the arc runs
///     from the arc-start point, with `end`/`mid` its rotation by `angle`/`angle/2`
///     about the centre (the same `angle` for both keeps the mid on the swept side
///     whatever the sign convention).
pub(crate) fn gr_arc_points(list: &[Sexp], center: Point) -> Result<(Point, Point, Point), String> {
    let off = |p: Point| Point {
        x: center.x + p.x,
        y: center.y + p.y,
    };
    let start = prim_xy(list, "start")?.ok_or("gr_arc missing (start …)")?;
    let end = prim_xy(list, "end")?.ok_or("gr_arc missing (end …)")?;
    if let Some(mid) = prim_xy(list, "mid")? {
        Ok((off(start), off(mid), off(end)))
    } else if let Some(angle) = prim_angle(list) {
        let (c, p0) = (off(start), off(end));
        Ok((
            p0,
            rotate_point(p0, c, angle / 2.0),
            rotate_point(p0, c, angle),
        ))
    } else {
        Err("gr_arc needs either (mid …) or (angle …)".into())
    }
}

/// A `(<head> x y)` child of `list`, mm→nm. `Ok(None)` if absent, `Err` if malformed.
pub(crate) fn prim_xy(list: &[Sexp], head: &str) -> Result<Option<Point>, String> {
    let Some(l) = list.iter().find_map(|s| s.list_headed(head)) else {
        return Ok(None);
    };
    let x = mm_to_nm(
        l.get(1)
            .and_then(Sexp::as_atom)
            .ok_or(format!("{head} missing x"))?,
    )?;
    let y = mm_to_nm(
        l.get(2)
            .and_then(Sexp::as_atom)
            .ok_or(format!("{head} missing y"))?,
    )?;
    Ok(Some(Point { x, y }))
}

/// A primitive's `(width w)` in nm (0 if absent ⇒ a filled, not stroked, primitive).
fn prim_width(list: &[Sexp]) -> Nm {
    list.iter()
        .find_map(|s| s.list_headed("width"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
        .unwrap_or(0)
}

/// A primitive's `(angle a)` in degrees (legacy `gr_arc` sweep), if present.
fn prim_angle(list: &[Sexp]) -> Option<f64> {
    list.iter()
        .find_map(|s| s.list_headed("angle"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| a.parse::<f64>().ok())
}

/// A `gr_poly`'s `(pts (xy x y) …)` as points (mm→nm).
fn prim_pts(list: &[Sexp]) -> Result<Vec<Point>, String> {
    let Some(pts) = list.iter().find_map(|s| s.list_headed("pts")) else {
        return Ok(vec![]);
    };
    let mut out = Vec::new();
    for xy in &pts[1..] {
        if let Some(l) = xy.list_headed("xy") {
            let x = mm_to_nm(l.get(1).and_then(Sexp::as_atom).ok_or("xy missing x")?)?;
            let y = mm_to_nm(l.get(2).and_then(Sexp::as_atom).ok_or("xy missing y")?)?;
            out.push(Point { x, y });
        }
    }
    Ok(out)
}

/// Distance between two points, nm, rounded (import-time float — like mm→nm rounding).
pub(crate) fn dist_nm(a: Point, b: Point) -> Nm {
    let (dx, dy) = ((a.x - b.x) as f64, (a.y - b.y) as f64);
    (dx * dx + dy * dy).sqrt().round() as Nm
}

/// Rotate a point about `center` by `deg` (KiCad CCW degrees). Exact for the four
/// cardinal angles; off-axis angles use float trig rounded to nm (import-time only).
fn rotate_point(p: Point, center: Point, deg: f64) -> Point {
    let d = ((deg % 360.0) + 360.0) % 360.0;
    if d == 0.0 {
        return p;
    }
    let (dx, dy) = (p.x - center.x, p.y - center.y);
    let (rx, ry) = if d == 90.0 {
        (-dy, dx)
    } else if d == 180.0 {
        (-dx, -dy)
    } else if d == 270.0 {
        (dy, -dx)
    } else {
        let r = d.to_radians();
        let (sin, cos) = (r.sin(), r.cos());
        (
            ((dx as f64) * cos - (dy as f64) * sin).round() as Nm,
            ((dx as f64) * sin + (dy as f64) * cos).round() as Nm,
        )
    };
    Point {
        x: center.x + rx,
        y: center.y + ry,
    }
}

/// Rotate a shape's vertices about `center` by `deg` (see [`rotate_point`]).
fn rotate_shape(s: Shape2D, center: Point, deg: f64) -> Shape2D {
    s.map_points(|p| rotate_point(p, center, deg))
}

/// Parse a pad's `(drill <d>)` (round) or `(drill oval <w> <h>)` (slot, along the
/// longer axis), centred at `center` and rotated by the pad `(at)` angle so the
/// drill agrees with the copper. `None` if the pad has no drill. (A drill `(offset
/// …)` is not yet applied — the hole sits at the pad centre; rare, noted.)
fn parse_drill(pad: &[Sexp], center: Point, angle: f64) -> Result<Option<Drill>, String> {
    let Some(d) = pad.iter().find_map(|s| s.list_headed("drill")) else {
        return Ok(None);
    };
    match d.get(1).and_then(Sexp::as_atom) {
        Some("oval") => {
            let w = mm_to_nm(
                d.get(2)
                    .and_then(Sexp::as_atom)
                    .ok_or("drill oval missing w")?,
            )?;
            let h = mm_to_nm(
                d.get(3)
                    .and_then(Sexp::as_atom)
                    .ok_or("drill oval missing h")?,
            )?;
            let (a, b, dia) = if w >= h {
                let dx = (w - h) / 2;
                (
                    Point {
                        x: center.x - dx,
                        y: center.y,
                    },
                    Point {
                        x: center.x + dx,
                        y: center.y,
                    },
                    h,
                )
            } else {
                let dy = (h - w) / 2;
                (
                    Point {
                        x: center.x,
                        y: center.y - dy,
                    },
                    Point {
                        x: center.x,
                        y: center.y + dy,
                    },
                    w,
                )
            };
            Ok(Some(Drill::Slot {
                a: rotate_point(a, center, angle),
                b: rotate_point(b, center, angle),
                d: dia,
            }))
        }
        Some(tok) => Ok(Some(Drill::Round { d: mm_to_nm(tok)? })),
        None => Ok(None),
    }
}

/// Which copper layer(s) a pad occupies: through-hole types span the board; otherwise
/// read `(layers …)` — `*.` or both outer layers ⇒ through, a lone `B.Cu` ⇒ bottom,
/// else top.
fn pad_layers(pad: &[Sexp], pad_type: &str) -> PadLayers {
    if pad_type == "thru_hole" || pad_type == "np_thru_hole" {
        return PadLayers::Through;
    }
    if let Some(l) = pad.iter().find_map(|s| s.list_headed("layers")) {
        let toks: Vec<&str> = l.iter().skip(1).filter_map(Sexp::as_atom).collect();
        let (has_f, has_b) = (toks.contains(&"F.Cu"), toks.contains(&"B.Cu"));
        if toks.iter().any(|t| t.starts_with("*.")) || (has_f && has_b) {
            return PadLayers::Through;
        }
        if has_b {
            return PadLayers::Bottom;
        }
    }
    PadLayers::Top
}

/// Convenience wrapper: read a `.kicad_mod` file from disk and import it.
pub fn import_footprint_file(path: &str) -> Result<PartDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_footprint(&text)
}
