//! The RS-274X Gerber backend: copper layers, the `Edge.Cuts` outline, solder-mask
//! openings, and the derived-surface (silk / fab) layers, plus the [`gerber_set`] that
//! bundles the whole fileset. All coordinates flow through [`gbr_coord`] — the
//! `%FSLAX46Y46*%` fixed-point integer *is* the nanometre value, so the determinism
//! invariant holds with no float anywhere. Arc edges emit true G02/G03 draws via exact
//! integer circumcentre arithmetic ([`arc_ij_turn`]); region fills reuse the shared
//! ring-to-Gerber emitter [`gerber_region_fill`].

use crate::doc::{MM, Nm, Point};
use crate::geom::kernel::Region;
use crate::geom::{DEFAULT_CHORD_TOL, Extent, Path, Role, Seg, Shape2D, Slab, Stackup, ZRange};
use crate::part::{PartLib, pin_world};
use crate::route::{Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

use super::features::{pours_of, role_features};
use super::placement::part_pin_ids;
use super::svg::source_board;
use super::svg_writer::{fmt_mm, rel_to_start};

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

/// Bounding box of all placed/routed geometry (pad world points, trace vertices,
/// via centres) plus a 2 mm margin — the `Edge.Cuts` fallback when the source
/// carries no explicit `Board`. Falls back to a 10 mm box for an empty document.
fn placement_bbox(doc: &crate::doc::Doc, lib: &PartLib) -> (Point, Point) {
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
pub(crate) enum Aperture {
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

/// The Gerber arc I/J `(centre − start)` (rounded to nm) and turn (`+1` CCW / `−1` CW)
/// of the 3-point arc `start`→`mid`→`end`. Since the arc's start *is* the current point,
/// the start-relative centre is exactly the I/J offset Gerber wants. `None` if collinear
/// (caller draws a straight line). Exact-rational [`crate::geom::circumcenter`],
/// [`rdiv`]-rounded — byte-stable.
pub(crate) fn arc_ij_turn(start: Point, mid: Point, end: Point) -> Option<(Point, i32)> {
    let (b, c) = rel_to_start(start, mid, end);
    let (ux, uy, den) = crate::geom::circumcenter(Point { x: 0, y: 0 }, b, c);
    if den == 0 {
        return None;
    }
    let ij = Point {
        x: rdiv(ux, den) as Nm,
        y: rdiv(uy, den) as Nm,
    };
    Some((ij, den.signum() as i32))
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
pub(crate) fn gerber_contour(shape: &Shape2D, out: &mut String, mode: &mut &str, g75: &mut bool) {
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
fn copper_layers(doc: &crate::doc::Doc) -> Vec<Slab> {
    let su = crate::elaborate::stackup(&doc.source);
    su.copper_slabs().into_iter().cloned().collect()
}

/// Every component pad copper region that flashes on the copper slab with z-range
/// `target_z`, as `(world centre, aperture)`, in `(EntityId, pin-declaration,
/// copper-region)` order. Each pad's real geometry is transformed to world space and
/// reduced to a flashable aperture; a region flashes only on the slabs it occupies.
/// Toy-library pins (`pad: None`) contribute nothing.
fn component_pad_flashes(
    doc: &crate::doc::Doc,
    lib: &PartLib,
    target_z: ZRange,
) -> Vec<(Point, Aperture)> {
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
pub fn gerber_layer(doc: &crate::doc::Doc, lib: &PartLib, slab: &Slab) -> String {
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
pub fn gerber_edge_cuts(doc: &crate::doc::Doc, lib: &PartLib) -> String {
    // The board region (outline ∖ cutouts); fall back to a rectangle around all geometry.
    let region = source_board(doc).unwrap_or_else(|| {
        let (min, max) = placement_bbox(doc, lib);
        crate::geom::kernel::shape_to_region(
            &Shape2D::rect(
                Point {
                    x: (min.x + max.x) / 2,
                    y: (min.y + max.y) / 2,
                },
                max.x - min.x,
                max.y - min.y,
            ),
            crate::geom::kernel::DEFAULT_CIRCLE_SEGS,
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

/// The solder-mask Gerber for one [`Role::Mask`] `slab`, derived **forward** from the
/// model — never recomputed from a parallel rule set, and entered by the slab's *name*,
/// not a copper-layer enum (Decision 13 / 16 stage 4). The file draws the **openings**
/// (the fab inverts to the mask coverage — a draw-the-openings convention that stays an
/// export-format detail):
///
/// - Pad openings: the [`Role::Void`] features [`crate::part::PinDef::pad_features`]
///   emits at the mask slab's z (the pad copper already inflated by
///   [`crate::geom::MASK_EXPANSION`]) — flashed as their aperture, so on the default
///   stackup this is byte-for-byte the old pad-opening output. A pad's **drill** `Void`
///   is a through-cut at the *full* stackup z, not the mask z, so it is not one of these
///   — and it must not be: it sits inside the pad opening (drawing it again would double
///   the flash) and its home is the Excellon file. Through-hole pads open every mask slab
///   their z spans because `pad_features` places an opening at each side's mask slab.
/// - Board cutouts: milled through the whole stack, so they remove mask over their whole
///   area — drawn as `G36`/`G37` region fills.
///
/// Object order is `(EntityId, pin, region)` then cutouts — fully deterministic. Fallible
/// because the cutout query runs the slab-name materialization gate (Decision 13).
pub fn gerber_mask(doc: &crate::doc::Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
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
pub(crate) fn slab_file(name: &str) -> String {
    name.replace('.', "_")
}

/// One derived-surface Gerber for a `role`'s [`Slab`], drawing the features of that role
/// whose z intersects the slab (forward query per slab — Decision 13). A
/// [`Shape2D::Stroke`] (`fp_line`/`fp_arc`/text) draws as its centreline with a round
/// aperture of the stroke's pen diameter (`radius * 2`); a [`Shape2D::Polygon`]
/// (`fp_poly`/`fp_rect`) is a filled area, drawn as a `G36`/`G37` region; a
/// [`Shape2D::Area`] (TTF outline text) is a `G36`/`G37` region fill. Aperture codes run
/// from 10 in `Ord` order; object order follows [`role_features`] — deterministic. Shared
/// by [`gerber_silk`] (silk markings) and [`gerber_fab`] (fab drawing) — only the queried
/// role differs, exactly as the SVG side shares [`super::svg::svg_fab`]. Coordinates are
/// board-frame with no side mirroring (a bottom slab is not flipped — the fab viewer
/// flips it), matching the copper/mask/silk Gerber convention.
fn gerber_role_surface(
    doc: &crate::doc::Doc,
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
pub fn gerber_silk(doc: &crate::doc::Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
    gerber_role_surface(doc, lib, slab, Role::Marking)
}

/// One fab-drawing Gerber for a [`Role::Datum`] `slab` (Decision 15): the fab surface
/// features intersecting the slab, emitted board-frame with no side mirroring (a `B.Fab`
/// Gerber is a document layer the viewer flips, matching bottom silk). The Gerber sibling
/// of [`super::svg::svg_fab`] — same [`datum_slabs`] iteration, RS-274X instead of SVG.
/// Empty unless a fab slab is authored, so the default stackup ships no fab Gerber
/// (Decision 15 contract). See [`gerber_role_surface`].
pub fn gerber_fab(doc: &crate::doc::Doc, lib: &PartLib, slab: &Slab) -> Result<String, String> {
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
pub(crate) fn datum_slabs(su: &Stackup) -> Vec<Slab> {
    role_slabs(su, Role::Datum)
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
pub fn gerber_set(doc: &crate::doc::Doc, lib: &PartLib) -> Result<Vec<(String, String)>, String> {
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
    out.extend(super::excellon::excellon_drill(doc, lib));
    Ok(out)
}
