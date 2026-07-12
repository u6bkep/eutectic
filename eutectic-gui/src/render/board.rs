//! The board producer: `route::world_features` → [`Scene`] (renderer-spec
//! §2/§12 WP1).
//!
//! The GUI twin of the old `canvas.rs` VectorAsset projection (its
//! feature-parity reference) and of `export/svg.rs`: it walks the same
//! unified [`world_features`] stream and bins each physical feature onto the
//! visual plane it lives on by matching its z to a stackup slab. Where the
//! old canvas tessellated everything into paths, this producer lowers ~all
//! stroke-shaped copper to **analytic instances** (capsules / discs / arc
//! strokes) and keeps only genuine interiors (pours, glyph ink, rectangular
//! pads, mask solids) as [`PrimShape::Polygon`] rings for the CPU
//! tessellator.
//!
//! # Binning (parity with `canvas.rs`, deviations deliberate)
//!
//! - **Board outline** — dashed edge stroke ([`StyleClass::Dash`] pattern 0)
//!   on [`PlaneKey::Outline`]; the dash phase accumulates around each ring,
//!   closing edge included, exactly like the old geometry dashes — but here
//!   the dash is evaluated procedurally in the shader from `len0`.
//! - **Conductors** — discrete copper (traces / vias / pads) on
//!   [`PlaneKey::Copper`] (opaque); pour fills (`Shape2D::Area`) on
//!   [`PlaneKey::CopperPour`] beneath it (translucent via the style table —
//!   per-plane alpha replaces the old per-path `fill-opacity`).
//! - **Markings / fab datum** — strokes as round-capped capsule chains at a
//!   floored pen width, polygons/areas (TTF glyphs with counters) as filled
//!   interiors, on the slab's [`PlaneKey::Silk`] / [`PlaneKey::Fab`] plane.
//!   Copper text (a `Marking` at conductor z) lands on that copper plane,
//!   matching the old slab-name binning. *Deviation:* zero-thickness fab
//!   slabs now match exactly (see [`slab_of_z`]) — the old canvas's
//!   touching-fallback painted fab ink into the silk bucket.
//! - **Substrate** — rendered as a real plane (spec §4). The old canvas
//!   skipped the substrate fill and drew only the outline; here the style
//!   table owns its visibility, and the outline plane still exists.
//! - **Mask** — mask solids minus the pad mask-opening `Void`s, one boolean
//!   per mask slab at scene build (the honest fab semantics). *Deviation:*
//!   the old canvas binned mask-opening voids onto its drills layer and
//!   painted them hole-dark over the pads; openings are absence of mask, not
//!   holes, so here they punch the mask plane instead.
//! - **Drills** — every remaining `Void` (via/pad drills, authored NPTH,
//!   cutouts) on [`PlaneKey::Drills`], composited after copper with the
//!   background color (absence-through-everything, spec §4).
//! - **Keep-outs** — skipped, like the old canvas (a later ticket).
//!
//! # Semantic ids
//!
//! Provenance → [`SemanticKey`]: the net where the feature carries one, the
//! owning entity otherwise ([`FeatureOrigin`]), the chrome sentinel for
//! genuinely unattributable geometry. Ids intern in stream order, so equal
//! docs produce equal tables (the determinism contract).

use super::scene::{
    Plane, PlaneKey, Prim, PrimShape, Scene, SemanticInterner, SemanticKey, StyleClass,
};
use eutectic_core::coord::{MM, Nm, Point};
use eutectic_core::doc::{Doc, PinRef};
use eutectic_core::geom::kernel::{
    DEFAULT_CIRCLE_SEGS, Region, difference, shape_to_region, union_all,
};
use eutectic_core::geom::{
    DEFAULT_CHORD_TOL, Extent, FeatureOrigin, NetFeature, Path, Role, Seg, Shape2D, Slab, Stackup,
    ZRange, circumcenter,
};
use eutectic_core::id::NetId;
use eutectic_core::part::{PartLib, PinRole};
use eutectic_core::route::{DesignRules, world_features};
use std::collections::BTreeMap;

/// The 2 mm content margin `export/svg.rs` adds around the board bounds —
/// kept so the produced scene frames identically to the SVG oracle and the
/// old canvas viewBox.
const MARGIN: Nm = 2 * MM;

/// Floor on a silk / fab stroke's **radius** (half the pen width), so a
/// hairline authored stroke still shows. Mirrors the old canvas's
/// `MIN_STROKE_MM = 0.05` (a 0.05 mm pen = 25 µm radius).
const MIN_MARKING_R: Nm = 25_000;

/// The dash pattern id of the board-edge outline stroke (see
/// [`StyleTables::board_defaults`](super::style::StyleTables::board_defaults):
/// 0.8 mm on / 0.5 mm off, the old canvas's `EDGE_DASH_MM`/`EDGE_GAP_MM`).
pub const DASH_EDGE: u8 = 0;

/// Lower an elaborated document to a [`Scene`] over the unified
/// [`world_features`] stream.
///
/// `Err` only if feature lowering fails (an unknown slab name), which a
/// committed `Doc` never hits (the commit-time slab gate); the caller
/// surfaces it as a load error. Deterministic: equal documents produce equal
/// scenes (planes, primitive order, semantic tables).
pub fn board_scene(doc: &Doc, lib: &PartLib) -> Result<Scene, String> {
    let su = eutectic_core::elaborate::stackup(&doc.source);
    let netlist = doc_netlist(doc);
    let features = world_features(doc, lib, &netlist, &DesignRules::default(), &su)?;
    let bounds = content_bounds(doc, &features);
    let anchor = Point {
        x: (bounds.0 + bounds.2) / 2,
        y: (bounds.1 + bounds.3) / 2,
    };

    let mut sems = SemanticInterner::new();
    let mut buckets: BTreeMap<PlaneKey, Vec<Prim>> = BTreeMap::new();

    // Board-edge outline: each region ring as a dashed capsule chain, the
    // dash phase continuous around the ring (closing edge included).
    if let Some(region) = eutectic_core::elaborate::board_region(&doc.source) {
        let sem = sems.intern(SemanticKey::Board);
        let out = buckets.entry(PlaneKey::Outline).or_default();
        for ring in &region.rings {
            if ring.len() < 3 {
                continue;
            }
            let mut len = 0.0_f64;
            let n = ring.len();
            for k in 0..n {
                let (a, b) = (ring[k], ring[(k + 1) % n]);
                if a == b {
                    continue;
                }
                out.push(Prim {
                    sem,
                    class: StyleClass::Dash(DASH_EDGE),
                    len0: len,
                    shape: PrimShape::Capsule {
                        a,
                        b,
                        r: EDGE_STROKE_R,
                    },
                });
                len += dist(a, b);
            }
        }
    }

    // Mask solids and mask-opening voids, gathered per mask slab for the one
    // boolean below (openings punch the solids; Decision 13 — an opening is a
    // `Void` at mask z, not a negative layer, and here it *acts* like one).
    let mut mask_solids: BTreeMap<String, Vec<Region>> = BTreeMap::new();
    let mut mask_openings: BTreeMap<String, Vec<Region>> = BTreeMap::new();

    for nf in &features {
        let Extent::Prism { shape, z } = &nf.feature.extent;
        let Some(slab) = slab_of_z(&su, z) else {
            continue; // no slab spans this z (cannot happen for lowered features)
        };
        let sem = sems.intern(sem_key(nf));
        match nf.feature.role {
            Role::Conductor => {
                if matches!(shape, Shape2D::Area { .. }) {
                    // Pour fill: its own plane so the composite can run it
                    // translucent while discrete copper stays opaque.
                    buckets
                        .entry(PlaneKey::CopperPour(slab.name.clone()))
                        .or_default()
                        .push(Prim::fill(sem, polygon_shape(shape)));
                } else {
                    fill_prims(
                        buckets
                            .entry(PlaneKey::Copper(slab.name.clone()))
                            .or_default(),
                        shape,
                        sem,
                        0,
                    );
                }
            }
            Role::Marking | Role::Datum => {
                let key = match slab.role {
                    Role::Datum => PlaneKey::Fab(slab.name.clone()),
                    Role::Conductor => PlaneKey::Copper(slab.name.clone()),
                    _ => PlaneKey::Silk(slab.name.clone()),
                };
                fill_prims(buckets.entry(key).or_default(), shape, sem, MIN_MARKING_R);
            }
            Role::Substrate => {
                buckets
                    .entry(PlaneKey::Substrate)
                    .or_default()
                    .push(Prim::fill(sem, polygon_shape(shape)));
            }
            Role::Mask => {
                mask_solids
                    .entry(slab.name.clone())
                    .or_default()
                    .push(to_region(shape));
            }
            Role::Void => {
                if slab.role == Role::Mask {
                    // A pad's mask opening: punches the mask plane, is not a
                    // drill (deviation from the old canvas — see module docs).
                    mask_openings
                        .entry(slab.name.clone())
                        .or_default()
                        .push(to_region(shape));
                } else {
                    fill_prims(buckets.entry(PlaneKey::Drills).or_default(), shape, sem, 0);
                }
            }
            Role::Keepout(_) => {}
        }
    }

    // Mask planes: solids ∖ openings, one boolean per slab.
    let board_sem = sems.intern(SemanticKey::Board);
    for (name, solids) in mask_solids {
        let solid = union_all(solids);
        let fill = match mask_openings.remove(&name) {
            Some(openings) => difference(&solid, &union_all(openings)),
            None => solid,
        };
        let rings: Vec<Vec<Point>> = fill.rings.into_iter().filter(|r| r.len() >= 3).collect();
        if !rings.is_empty() {
            buckets
                .entry(PlaneKey::Mask(name))
                .or_default()
                .push(Prim::fill(board_sem, PrimShape::Polygon { rings }));
        }
    }

    // Assemble planes in back-to-front composite order: substrate, outline,
    // every stackup slab ascending z (higher layers paint over lower —
    // matching the old canvas's layer order), drills last. Empty planes are
    // still enumerated (stable indices for style tables / the layer panel);
    // any bucket that somehow missed the enumeration is appended in key
    // order so geometry is never dropped silently.
    let mut planes: Vec<Plane> = Vec::new();
    let mut push = |key: PlaneKey, buckets: &mut BTreeMap<PlaneKey, Vec<Prim>>| {
        let prims = buckets.remove(&key).unwrap_or_default();
        planes.push(Plane { key, prims });
    };
    push(PlaneKey::Substrate, &mut buckets);
    push(PlaneKey::Outline, &mut buckets);
    let mut slabs: Vec<&Slab> = su.slabs.iter().collect();
    slabs.sort_by_key(|s| s.z.lo);
    for s in slabs {
        match s.role {
            Role::Conductor => {
                push(PlaneKey::CopperPour(s.name.clone()), &mut buckets);
                push(PlaneKey::Copper(s.name.clone()), &mut buckets);
            }
            Role::Mask => push(PlaneKey::Mask(s.name.clone()), &mut buckets),
            Role::Marking => push(PlaneKey::Silk(s.name.clone()), &mut buckets),
            Role::Datum => push(PlaneKey::Fab(s.name.clone()), &mut buckets),
            // The substrate slab's features went to PlaneKey::Substrate.
            Role::Substrate | Role::Void | Role::Keepout(_) => {}
        }
    }
    push(PlaneKey::Drills, &mut buckets);
    for (key, prims) in buckets {
        planes.push(Plane { key, prims });
    }

    Ok(Scene {
        anchor,
        bounds,
        planes,
        semantics: sems.into_table(),
    })
}

/// The board-edge outline stroke radius (half the old canvas's
/// `EDGE_STROKE_MM = 0.12` pen).
const EDGE_STROKE_R: Nm = 60_000;

/// Provenance → semantic key (renderer-spec §2): net where present, owning
/// entity otherwise, chrome sentinel for the genuinely unattributable.
fn sem_key(nf: &NetFeature) -> SemanticKey {
    if let Some(n) = &nf.net {
        return SemanticKey::Net(n.clone());
    }
    match &nf.origin {
        FeatureOrigin::Trace(t) => SemanticKey::Trace(*t),
        FeatureOrigin::Via(v) => SemanticKey::Via(*v),
        FeatureOrigin::Pad { comp, pad } => SemanticKey::Pin {
            comp: comp.clone(),
            pad: pad.clone(),
        },
        FeatureOrigin::ComponentMarking(e) => SemanticKey::Part(e.clone()),
        FeatureOrigin::Region { net: Some(n), .. } => SemanticKey::Net(n.clone()),
        FeatureOrigin::Region { net: None, .. } | FeatureOrigin::Board => SemanticKey::Board,
        FeatureOrigin::BoardText => SemanticKey::BoardText,
        FeatureOrigin::Unattributed => SemanticKey::Chrome,
    }
}

/// Lower one shape's **filled extent** into primitives. A `Stroke` becomes
/// analytic capsules / discs / arc strokes (their union *is* the honest
/// inflated region — same-plane max-blend saturates the joints); a `Polygon`
/// or `Area` becomes tessellatable rings. `min_r` floors a stroke's radius
/// (silk hairlines); `0` keeps the authored radius. `pub(crate)`: WP2's
/// overlay lowering reuses it for drag-ghost shapes.
pub(crate) fn fill_prims(out: &mut Vec<Prim>, shape: &Shape2D, sem: u32, min_r: Nm) {
    match shape {
        Shape2D::Stroke { path, radius } => {
            stroke_prims(out, path, (*radius).max(min_r), sem, StyleClass::Fill)
        }
        Shape2D::Polygon { .. } | Shape2D::Area { .. } => {
            out.push(Prim::fill(sem, polygon_shape(shape)))
        }
    }
}

/// A shape's filled interior as [`PrimShape::Polygon`] rings, realised
/// through the same region kernel the SVG backend uses (fixed flatten
/// tolerance, corner radii honoured, `Area` holes preserved).
fn polygon_shape(shape: &Shape2D) -> PrimShape {
    let rings = to_region(shape)
        .rings
        .into_iter()
        .filter(|r| r.len() >= 3)
        .collect();
    PrimShape::Polygon { rings }
}

/// A shape as its filled [`Region`] (an `Area` is already one).
fn to_region(shape: &Shape2D) -> Region {
    match shape {
        Shape2D::Area { region } => region.clone(),
        _ => shape_to_region(shape, DEFAULT_CIRCLE_SEGS),
    }
}

/// Walk a stroke skeleton into analytic primitives with accumulated path
/// length: `Line` → capsule, `Arc` → one [`PrimShape::ArcStroke`] (or a
/// capsule when collinear), Béziers → flattened capsule chains (the kernel's
/// fixed chord tolerance). A lone point is a disc. Consecutive primitives
/// share endpoints, so round joins come free from coverage max-blend.
fn stroke_prims(out: &mut Vec<Prim>, path: &Path, radius: Nm, sem: u32, class: StyleClass) {
    if path.segs.is_empty() {
        out.push(Prim {
            sem,
            class,
            len0: 0.0,
            shape: PrimShape::Disc {
                c: path.start,
                r: radius,
            },
        });
        return;
    }
    let mut cur = path.start;
    let mut len = 0.0_f64;
    let capsule = |out: &mut Vec<Prim>, a: Point, b: Point, len: &mut f64| {
        if a == b {
            return;
        }
        out.push(Prim {
            sem,
            class,
            len0: *len,
            shape: PrimShape::Capsule { a, b, r: radius },
        });
        *len += dist(a, b);
    };
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => capsule(out, cur, *end, &mut len),
            Seg::Arc { mid, end } => match arc_geom(cur, *mid, *end) {
                Some((center, r, a0, a1)) => {
                    out.push(Prim {
                        sem,
                        class,
                        len0: len,
                        shape: PrimShape::ArcStroke {
                            center,
                            radius: r,
                            a0,
                            a1,
                            half_width: radius,
                        },
                    });
                    len += r * (a1 - a0).abs();
                }
                None => capsule(out, cur, *end, &mut len),
            },
            Seg::Quadratic { .. } | Seg::Cubic { .. } => {
                let flat = Path {
                    start: cur,
                    segs: vec![seg.clone()],
                }
                .flatten(DEFAULT_CHORD_TOL);
                for w in flat.windows(2) {
                    capsule(out, w[0], w[1], &mut len);
                }
            }
        }
        cur = seg.end();
    }
}

/// Circular-arc geometry from the 3-point form: `(center, radius, a0, a1)`
/// with `a1 - a0` the **signed** sweep (CCW positive, y-up frame), such that
/// sweeping from `a0` to `a1` passes through `mid`. `None` for a collinear
/// triple (a straight chord). f64 from the exact-rational circumcenter, so
/// rebuilding the same scene reproduces identical values.
fn arc_geom(start: Point, mid: Point, end: Point) -> Option<([f64; 2], f64, f64, f64)> {
    let (ux, uy, den) = circumcenter(start, mid, end);
    if den == 0 {
        return None;
    }
    let (cx, cy) = (ux as f64 / den as f64, uy as f64 / den as f64);
    let r = ((start.x as f64 - cx).powi(2) + (start.y as f64 - cy).powi(2)).sqrt();
    let ang = |p: Point| (p.y as f64 - cy).atan2(p.x as f64 - cx);
    let (a0, a1) = (ang(start), ang(end));
    let tau = std::f64::consts::TAU;
    // `den` is 2·cross(start, mid, end): positive ⇒ the arc turns CCW.
    let sweep = if den > 0 {
        let s = (a1 - a0).rem_euclid(tau);
        if s == 0.0 { tau } else { s }
    } else {
        let s = (a0 - a1).rem_euclid(tau);
        -(if s == 0.0 { tau } else { s })
    };
    Some(([cx, cy], r, a0, a0 + sweep))
}

/// Euclidean distance in nm (f64; IEEE sqrt is correctly rounded, so this is
/// deterministic across rebuilds).
fn dist(a: Point, b: Point) -> f64 {
    ((b.x - a.x) as f64).hypot((b.y - a.y) as f64)
}

/// The stackup slab whose z-range contains the **midpoint** of a feature's z
/// (a forward query — the z was assigned from a slab at lowering). Midpoint
/// disambiguates the shared faces of contiguous slabs; a zero-thickness
/// feature prefers an **exact zero-thickness slab** at its plane, then falls
/// back to a touching slab. Local twin of the pick module's helper (the
/// renderer stays app-module-free; no imports from it) — with one
/// deliberate fix: the old fallback bound zero-thickness fab-datum slabs
/// (`F.Fab 1.635..1.635 mm`) to the *silk* slab whose face they touch, so
/// fab ink rendered in the silk bucket; the exact-match step here puts fab
/// geometry on its own Fab plane.
fn slab_of_z<'a>(su: &'a Stackup, z: &ZRange) -> Option<&'a Slab> {
    let mid = z.lo + (z.hi - z.lo) / 2;
    if z.lo == z.hi
        && let Some(s) = su.slabs.iter().find(|s| s.z.lo == z.lo && s.z.hi == z.hi)
    {
        return Some(s);
    }
    su.slabs
        .iter()
        .find(|s| s.z.lo <= mid && mid < s.z.hi)
        .or_else(|| su.slabs.iter().find(|s| s.z.lo <= mid && mid <= s.z.hi))
}

/// The membership netlist [`world_features`] needs, rebuilt from `doc.nets`
/// (the crate-internal `route::doc_netlist` is not public; roles are
/// irrelevant to the geometry producer).
fn doc_netlist(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
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

/// Content bounds in nm `(x0, y0, x1, y1)` **with the 2 mm margin** — the
/// same box `export/svg.rs` and the old canvas derive: board-region corners
/// plus the extents of exactly the `Conductor` + `Marking` features (never
/// `Datum` fab geometry, substrate, mask, voids, or keep-outs — the fab text
/// runs far outside the board and would mis-frame it). Falls back to a 10 mm
/// box for an empty document.
fn content_bounds(doc: &Doc, features: &[NetFeature]) -> (Nm, Nm, Nm, Nm) {
    let mut pts: Vec<Point> = Vec::new();
    if let Some(region) = eutectic_core::elaborate::board_region(&doc.source)
        && let Some((min, max)) = region.bbox()
    {
        pts.push(min);
        pts.push(max);
    }
    for nf in features {
        if !matches!(nf.feature.role, Role::Conductor | Role::Marking) {
            continue;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        pts.extend(shape.points());
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
    (x0 - MARGIN, y0 - MARGIN, x1 + MARGIN, y1 + MARGIN)
}

#[cfg(test)]
mod tests;
