//! Purposed regions: the physical-geometry foundation (see docs/architecture.md §8).
//!
//! Everything physical — copper, the board body, holes, keep-outs — is a
//! [`Feature`]: a `(role, material?, extent)`. This module is the **2.5D core**:
//! the shape vocabulary, the z-stackup, and an exact-integer clearance kernel. As of
//! the geometry-model convergence (docs/geometry-model-convergence.md, Phases 0–2)
//! this **is** the live clearance model: DRC, pours, Gerber, and the autorouter all
//! reduce copper to [`Feature`]s and gate on [`Feature::clears`]; the former
//! `route::Layer`-based copper-piece model has been retired. `route::Layer` survives
//! only as the routing/trace/via tier and the violation-report granularity.
//!
//! ## One shape: a skeleton inflated by a radius
//!
//! [`Shape2D`] is a skeleton (a polyline, or a filled polygon) **⊕ a radius** — the
//! Minkowski sum with a disc. This single type subsumes every pad primitive *and*
//! traces *and* via annuli:
//!   - point ⊕ r  = a round pad / via
//!   - segment ⊕ r = an oval/pill pad
//!   - open polyline ⊕ (width/2) = a trace
//!   - rectangle polygon ⊕ r = a rounded rect (r = 0 ⇒ sharp); arbitrary polygon = a
//!     trapezoid / custom pad; a *union* of shapes = a compound pad (e.g. BMP581).
//!
//! Clearance is then uniform and exact: the edge-to-edge gap is
//! `skeleton_distance(a, b) − rₐ − r_b`, and a violation is that gap `< min_clearance`.
//! All distance math is `i128` squared-distance comparison — no float, deterministic.
//!
//! ## z is real; a "layer" is a named z-slab
//!
//! An [`Extent::Prism`] carries a [`ZRange`]. Two features can clash only if their
//! z-ranges overlap; with the discrete slabs of a [`Stackup`] that collapses to
//! "same layer", recovering ordinary 2.5D behaviour — but nothing is *limited* to
//! discrete layers, so below-surface bodies (negative/arbitrary z) are expressible,
//! and `Extent::Solid` is reserved for true 3D. Net-aware *policy* (which feature
//! pairs to check) lives in DRC; this module is the pure geometry + data model.

use crate::doc::{Nm, Point};
use crate::id::NetId;

/// Default board thickness: 1.6 mm, in nm.
pub const BOARD_THICKNESS: Nm = 1_600_000;
/// Default finished copper thickness: ~1 oz (35 µm), in nm.
pub const COPPER_THICKNESS: Nm = 35_000;
/// Default solder-mask thickness: 25 µm, in nm.
pub const MASK_THICKNESS: Nm = 25_000;
/// Default silkscreen (ink) thickness: 10 µm, in nm.
pub const SILK_THICKNESS: Nm = 10_000;
/// Solder-mask expansion: how much larger a mask opening is than the pad copper, per
/// side (the pad copper is inflated by this to get the opening). The **single source
/// of truth** for that margin — the model's mask-opening `Void`s
/// ([`crate::part::PinDef::pad_features`]), the design-rule default
/// ([`crate::route::DesignRules::default`]), and the Gerber mask path all read it, so
/// there is one value to change. A generic process figure; production reads it from
/// the stack-up/process.
pub const MASK_EXPANSION: Nm = 50_000;
/// Default arc chord tolerance for tessellation: max sagitta (arc-to-chord deviation),
/// in nm. 1 µm — finer than the 64-gon disc approximation at pad scale, coarse enough
/// to keep segment counts modest for large-radius board-outline arcs.
///
/// The flattening is **inscribed** (vertices sit *on* the arc, chords cut inside), so
/// for DRC the tessellated copper is at most one sagitta smaller than the true arc and
/// a clearance check is *optimistic* by at most that amount. At 1 µm against ≥ 100 µm
/// clearances this is < 1 %; keep it well under the fab margin. (A conservative DRC
/// would circumscribe instead — deferred; not worth the complexity at this tolerance.)
pub const DEFAULT_CHORD_TOL: Nm = 1_000;

/// The enforced ceiling on any coordinate magnitude, in nm: **1 m** (`±1e9 nm`).
///
/// The exact-integer kernel keeps its squared-distance math in `i128`, and the
/// worst chain is the perpendicular case of [`pt_seg_d2`]: `|w|²·den` where each of
/// `|w|²` and `den` is a sum of two squared coordinate *differences*. A difference of
/// two coordinates each in `[−C, C]` has magnitude ≤ `2C`, so `|w|², den ≤ 2·(2C)² =
/// 8C²` and the product is ≤ `64·C⁴`. Requiring `64·C⁴ ≤ i128::MAX ≈ 1.70e38` gives
/// `C ≤ (2^127 / 64)^(1/4) ≈ 1.28e9` nm. We round that **down** to a memorable
/// `1e9 nm = 1 m`, which leaves `64·(1e9)⁴ ≈ 6.4e37` — a ~2.7× margin under the
/// `i128` ceiling. Every other integer predicate is lower-order in `C` (the
/// [`circumcenter`]/[`region::crossings`] numerators are ~`C³`, [`orient`] ~`C²`), so
/// this quartic bound is the binding one and protects them all.
///
/// This is the crate-wide operating range — far beyond any real board (a 1 m panel).
/// It is *enforced* at every ingest boundary (text parse, KiCad/SVG import, command
/// ingress) as a hard `E_COORD_RANGE` diagnostic, and *asserted* in the hot kernel
/// predicates in debug builds; release builds trust the boundary guarantee and stay
/// unchecked. This resolves issue 0018 (the former silent-wrap-above-~1.28e9 hazard).
pub const MAX_COORD: Nm = 1_000_000_000;

/// Is a single coordinate within the enforced [`MAX_COORD`] ingest range?
pub fn coord_ok(n: Nm) -> bool {
    n.unsigned_abs() <= MAX_COORD as u64
}

/// Are both components of a point within the [`MAX_COORD`] ingest range? The
/// ingest-boundary validation predicate (text/import/command).
pub fn point_ok(p: Point) -> bool {
    coord_ok(p.x) && coord_ok(p.y)
}

/// The **true** `i128`-safe coordinate ceiling — the largest magnitude for which the
/// worst kernel product `64·C⁴` still fits in `i128` (`64·C⁴ ≤ i128::MAX` ⟹
/// `C ≤ (2^127/64)^(1/4) = 1_276_901_416`). Rounded **down** to `1_276_000_000` for a
/// small safety margin (`64·C⁴ ≈ 1.697e38 < 1.701e38`).
///
/// This is distinct from — and larger than — [`MAX_COORD`] on purpose. Ingest bounds
/// *authored/imported* coordinates at `MAX_COORD` (1 m); the kernel then *composes*
/// them (a placement offset + a footprint-local courtyard extent, an inflation by a
/// clearance), and a composed world coordinate can legitimately exceed `MAX_COORD`
/// while staying correct. The `~0.28e9` gap between the two constants is exactly that
/// composition headroom. The kernel debug_asserts fire at `KERNEL_SAFE_COORD` (the
/// real overflow risk), **not** at `MAX_COORD` — otherwise a part legally placed at the
/// 1 m ingest bound would panic a debug build the instant its courtyard is measured.
pub const KERNEL_SAFE_COORD: Nm = 1_276_000_000;

// Compile-time guards on the two ceilings (issue 0018): the kernel ceiling must sit
// above the ingest ceiling (that gap is the composition headroom), and the worst
// kernel product `64·C⁴` at the kernel ceiling must still fit in `i128`.
const _: () = assert!(KERNEL_SAFE_COORD > MAX_COORD);
const _: () = {
    let c = KERNEL_SAFE_COORD as i128;
    let c2 = c * c;
    assert!(c2.checked_mul(c2).is_some(), "C⁴ overflows i128");
    assert!((c2 * c2).checked_mul(64).is_some(), "64·C⁴ overflows i128");
};

/// Is a single coordinate within the [`KERNEL_SAFE_COORD`] i128-safe range? The
/// debug-assert predicate for the hot integer kernels (composition-frame, not ingest).
pub fn coord_kernel_safe(n: Nm) -> bool {
    n.unsigned_abs() <= KERNEL_SAFE_COORD as u64
}

/// Are both components of a point within [`KERNEL_SAFE_COORD`]? The kernel debug-assert
/// predicate.
pub fn point_kernel_safe(p: Point) -> bool {
    coord_kernel_safe(p.x) && coord_kernel_safe(p.y)
}

// ----------------------------------------------------------------------------
// Shape2D — a skeleton ⊕ radius.
// ----------------------------------------------------------------------------

/// One edge of a skeleton [`Path`], implicitly starting at the previous point.
///
/// Curved edges (`Arc`, `Quadratic`, `Cubic`) follow "strategy A": they are
/// authoritative, and the only places they are special are (a) one [`Path::flatten`]
/// arm each and (b) export arms. The clearance/boolean kernel and every other
/// path-walking consumer see *only* the tessellated polyline `flatten` produces, so
/// adding a curve kind never touches them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    /// A straight edge to `end`.
    Line { end: Point },
    /// A circular arc from the path's current point through `mid` to `end` — the
    /// **3-point** form. All three are lattice points (no over-determination, no
    /// degenerate-consistency failure mode), and the centre/radius derive as exact
    /// rationals ([`circumcenter`]) only when export needs `G02`/`G03` I/J or an SVG
    /// `A` arc. A collinear triple degenerates to a straight chord.
    Arc { mid: Point, end: Point },
    /// A quadratic Bézier from the current point, control point `ctrl`, to `end` —
    /// the form TrueType `glyf` outlines use. Control points are lattice points;
    /// flattens by pure-integer de Casteljau (no float, no `sqrt`). Stored natively
    /// rather than elevated to a cubic so `glyf` round-trips without off-lattice loss.
    Quadratic { ctrl: Point, end: Point },
    /// A cubic Bézier from the current point, controls `c1`,`c2`, to `end` — the form
    /// OpenType CFF charstrings and SVG paths use. Same integer de Casteljau flatten.
    Cubic { c1: Point, c2: Point, end: Point },
}

impl Seg {
    /// Where this edge ends (the next path point).
    pub fn end(&self) -> Point {
        match self {
            Seg::Line { end }
            | Seg::Arc { end, .. }
            | Seg::Quadratic { end, .. }
            | Seg::Cubic { end, .. } => *end,
        }
    }
    fn map(&self, f: &impl Fn(Point) -> Point) -> Seg {
        match self {
            Seg::Line { end } => Seg::Line { end: f(*end) },
            Seg::Arc { mid, end } => Seg::Arc {
                mid: f(*mid),
                end: f(*end),
            },
            Seg::Quadratic { ctrl, end } => Seg::Quadratic {
                ctrl: f(*ctrl),
                end: f(*end),
            },
            Seg::Cubic { c1, c2, end } => Seg::Cubic {
                c1: f(*c1),
                c2: f(*c2),
                end: f(*end),
            },
        }
    }
}

/// A skeleton path: a `start` point followed by edges ([`Seg`]). For a [`Shape2D::Stroke`]
/// it is an **open** polyline; for a [`Shape2D::Polygon`] it is a **closed** ring whose
/// final edge back to `start` is an implicit straight [`Seg::Line`] (skipped when the
/// last segment already ends at `start`, so an explicit arc *can* close the ring).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub start: Point,
    pub segs: Vec<Seg>,
}

impl Path {
    /// A straight polyline through `points` (the all-`Line` path the legacy
    /// constructors build). `points` must be non-empty.
    pub fn polyline(points: Vec<Point>) -> Path {
        let mut it = points.into_iter();
        let start = it.next().expect("Path::polyline needs ≥1 point");
        Path {
            start,
            segs: it.map(|end| Seg::Line { end }).collect(),
        }
    }

    /// Corner vertices in order: `start`, then each segment's `end`. Arc `mid`s are
    /// **not** corners (they are interior to the curve). For drawing/serialising the
    /// straight skeleton; geometry that must respect arc curvature uses [`flatten`].
    fn corners(&self) -> Vec<Point> {
        let mut v = Vec::with_capacity(self.segs.len() + 1);
        v.push(self.start);
        v.extend(self.segs.iter().map(Seg::end));
        v
    }

    /// Every lattice point this path is *defined by*: `start`, then each segment's
    /// control/mid point(s) and end. Unlike [`corners`], this includes arc mids and
    /// Bézier control points — the off-corner lattice points the kernel also consumes
    /// (a wild control point overflows the flatten's `i128` product even when the
    /// corners are in range), so coordinate-range validation walks these.
    fn defining_points(&self) -> Vec<Point> {
        let mut v = vec![self.start];
        for s in &self.segs {
            match s {
                Seg::Line { end } => v.push(*end),
                Seg::Arc { mid, end } => v.extend([*mid, *end]),
                Seg::Quadratic { ctrl, end } => v.extend([*ctrl, *end]),
                Seg::Cubic { c1, c2, end } => v.extend([*c1, *c2, *end]),
            }
        }
        v
    }

    /// Flatten to a polyline (`start`, then each edge flattened — arcs subdivided to
    /// chord tolerance `tol`). Does **not** append a closing edge; callers that need a
    /// closed ring wrap the result. This is the single seam through which arcs reach
    /// the exact-integer kernel: everything downstream sees straight segments.
    pub fn flatten(&self, tol: Nm) -> Vec<Point> {
        let mut out = vec![self.start];
        let mut cur = self.start;
        for s in &self.segs {
            match s {
                Seg::Line { end } => out.push(*end),
                Seg::Arc { mid, end } => flatten_arc(cur, *mid, *end, tol, &mut out),
                Seg::Quadratic { ctrl, end } => flatten_quad(cur, *ctrl, *end, tol, &mut out),
                Seg::Cubic { c1, c2, end } => flatten_cubic(cur, *c1, *c2, *end, tol, &mut out),
            }
            cur = s.end();
        }
        out
    }

    fn map(&self, f: &impl Fn(Point) -> Point) -> Path {
        Path {
            start: f(self.start),
            segs: self.segs.iter().map(|s| s.map(f)).collect(),
        }
    }
}

/// A 2D region: a skeleton inflated by `radius` (Minkowski ⊕ a disc of that radius).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape2D {
    /// An open path (≥ 1 point) inflated by `radius`. A lone point ⇒ a disc; one
    /// segment ⇒ a capsule/oval; many ⇒ a trace of width `2*radius` (now arc-capable).
    Stroke { path: Path, radius: Nm },
    /// A filled simple polygon (closed [`Path`]; ≥ 3 corners), with corners rounded by
    /// `radius` (`0` ⇒ sharp; a rectangle with `radius` ⇒ a rounded rect). Edges may be
    /// arcs (e.g. a D-shaped or slotted board outline).
    Polygon { path: Path, radius: Nm },
    /// A filled area with holes: a [`Region`](crate::region::Region) of oriented rings
    /// (CCW islands minus CW holes) carried as a first-class shape (Decision 16a). Unlike
    /// `Stroke`/`Polygon` it has no skeleton+radius — the rings *are* the boundary, already
    /// polygonized (a board outline ∖ cutouts, a pour fill, a glyph with counters). Its
    /// `radius()` is `0`; [`inflated`](Shape2D::inflated) uses the region kernel's exact
    /// dilation ([`region::dilate`](crate::region::dilate)).
    Area { region: crate::region::Region },
}

impl Shape2D {
    /// A round pad / via annulus: a disc of `radius` centred at `c`.
    pub fn disc(c: Point, radius: Nm) -> Shape2D {
        Shape2D::Stroke {
            path: Path {
                start: c,
                segs: vec![],
            },
            radius,
        }
    }
    /// A pill/oval: the `radius`-inflation of segment `a`–`b`.
    pub fn capsule(a: Point, b: Point, radius: Nm) -> Shape2D {
        Shape2D::Stroke {
            path: Path::polyline(vec![a, b]),
            radius,
        }
    }
    /// A trace: a polyline of copper `width` wide (inflation `width/2`).
    pub fn trace(points: Vec<Point>, width: Nm) -> Shape2D {
        Shape2D::Stroke {
            path: Path::polyline(points),
            radius: width / 2,
        }
    }
    /// An open arc stroke: `width`-wide copper following the circular arc through
    /// `start`→`mid`→`end` (e.g. a curved trace). Sugar over the [`Path`]/[`Seg::Arc`] form.
    pub fn arc(start: Point, mid: Point, end: Point, width: Nm) -> Shape2D {
        Shape2D::Stroke {
            path: Path {
                start,
                segs: vec![Seg::Arc { mid, end }],
            },
            radius: width / 2,
        }
    }
    /// An open cubic-Bézier stroke: `width`-wide copper following the cubic
    /// `start`→(`c1`,`c2`)→`end`. Sugar over the [`Path`]/[`Seg::Cubic`] form.
    pub fn cubic(start: Point, c1: Point, c2: Point, end: Point, width: Nm) -> Shape2D {
        Shape2D::Stroke {
            path: Path {
                start,
                segs: vec![Seg::Cubic { c1, c2, end }],
            },
            radius: width / 2,
        }
    }
    /// An axis-aligned rectangle (sharp corners) of size `w`×`h` centred at `c`.
    pub fn rect(c: Point, w: Nm, h: Nm) -> Shape2D {
        let (hw, hh) = (w / 2, h / 2);
        Shape2D::Polygon {
            path: Path::polyline(vec![
                Point {
                    x: c.x - hw,
                    y: c.y - hh,
                },
                Point {
                    x: c.x + hw,
                    y: c.y - hh,
                },
                Point {
                    x: c.x + hw,
                    y: c.y + hh,
                },
                Point {
                    x: c.x - hw,
                    y: c.y + hh,
                },
            ]),
            radius: 0,
        }
    }
    /// An axis-aligned rounded rectangle: the core rect inset by `r`, ⊕ `r`. The
    /// radius is clamped to `[0, min(w, h)/2]` (as KiCad does), so an over-large `r`
    /// degenerates to a capsule/disc rather than ballooning the shape.
    pub fn round_rect(c: Point, w: Nm, h: Nm, r: Nm) -> Shape2D {
        let r = r.clamp(0, w.min(h) / 2);
        let (hw, hh) = (w / 2 - r, h / 2 - r);
        Shape2D::Polygon {
            path: Path::polyline(vec![
                Point {
                    x: c.x - hw,
                    y: c.y - hh,
                },
                Point {
                    x: c.x + hw,
                    y: c.y - hh,
                },
                Point {
                    x: c.x + hw,
                    y: c.y + hh,
                },
                Point {
                    x: c.x - hw,
                    y: c.y + hh,
                },
            ]),
            radius: r,
        }
    }
    /// A filled polygon from explicit points (e.g. a rotated or custom pad).
    pub fn polygon(points: Vec<Point>) -> Shape2D {
        Shape2D::Polygon {
            path: Path::polyline(points),
            radius: 0,
        }
    }
    /// A filled polygon from an explicit [`Path`] (edges may be arcs) and corner
    /// `radius`. The general constructor behind the sugar above.
    pub fn polygon_path(path: Path, radius: Nm) -> Shape2D {
        Shape2D::Polygon { path, radius }
    }

    /// This shape's skeleton path. **Panics for [`Shape2D::Area`]**, which has no
    /// skeleton — its boundary is the region's rings, reached via [`points`](Shape2D::points)
    /// / [`region`](Shape2D::region) instead. Callers that may see an `Area` (exporters,
    /// curve tests) branch on the variant first; the skeleton-only paths (pads, silk
    /// strokes, authored outlines) never carry an `Area`.
    pub fn path(&self) -> &Path {
        match self {
            Shape2D::Stroke { path, .. } | Shape2D::Polygon { path, .. } => path,
            Shape2D::Area { .. } => {
                unreachable!("Shape2D::Area has no skeleton path; use points()/region()")
            }
        }
    }

    /// The region of a [`Shape2D::Area`], else `None`. The one accessor that reaches an
    /// `Area`'s rings without going through the (panicking) skeleton [`path`](Shape2D::path).
    pub fn region(&self) -> Option<&crate::region::Region> {
        match self {
            Shape2D::Area { region } => Some(region),
            _ => None,
        }
    }

    /// The inflation radius (the Minkowski disc). Public so the offset kernel can
    /// realise `skeleton ⊕ disc(radius)` as a filled region. An [`Area`](Shape2D::Area) is
    /// already the filled set, so its radius is `0`.
    pub fn radius(&self) -> Nm {
        match self {
            Shape2D::Stroke { radius, .. } | Shape2D::Polygon { radius, .. } => *radius,
            Shape2D::Area { .. } => 0,
        }
    }

    /// This shape inflated (Minkowski ⊕ a disc) by `d`: the skeleton is unchanged and
    /// the inflation radius grows by `d`. Offsetting copper by a clearance is *exactly*
    /// this — disc Minkowski sums add radii — which is why a pour knockout never needs
    /// a bespoke polygon-offset. `d` may be negative (deflate) for a `Stroke`/`Polygon`
    /// (the radius floors at 0); for an [`Area`](Shape2D::Area) a negative `d` (erosion)
    /// is unimplemented and **panics**.
    pub fn inflated(&self, d: Nm) -> Shape2D {
        match self {
            Shape2D::Stroke { path, radius } => Shape2D::Stroke {
                path: path.clone(),
                radius: (radius + d).max(0),
            },
            Shape2D::Polygon { path, radius } => Shape2D::Polygon {
                path: path.clone(),
                radius: (radius + d).max(0),
            },
            // An `Area` is already realised rings, so inflation is the region kernel's
            // exact Minkowski dilation by a disc of `d` (same decomposition
            // `shape_to_region` uses). `d == 0` is identity; **negative `d` (erosion) is
            // not implemented and panics** in [`region::dilate`](crate::region::dilate) —
            // no consumer needs it (clearance offsets are always positive) and a silent
            // wrong answer is worse than a loud one.
            Shape2D::Area { region } => Shape2D::Area {
                region: if d == 0 {
                    region.clone()
                } else {
                    crate::region::dilate(region, d, crate::region::DEFAULT_CIRCLE_SEGS)
                },
            },
        }
    }

    /// Apply a point map (e.g. a placement transform: cardinal rotation + offset) to
    /// every path point, preserving the inflation radius. Used to lift a footprint-local
    /// pad shape into world coordinates.
    pub fn map_points(&self, f: impl Fn(Point) -> Point) -> Shape2D {
        match self {
            Shape2D::Stroke { path, radius } => Shape2D::Stroke {
                path: path.map(&f),
                radius: *radius,
            },
            Shape2D::Polygon { path, radius } => Shape2D::Polygon {
                path: path.map(&f),
                radius: *radius,
            },
            Shape2D::Area { region } => Shape2D::Area {
                region: crate::region::Region {
                    rings: region
                        .rings
                        .iter()
                        .map(|ring| {
                            let before = crate::region::signed_area2(ring);
                            let mut mapped: Vec<Point> = ring.iter().map(|&p| f(p)).collect();
                            // A reflecting transform (negative determinant — e.g. a
                            // bottom-side flip (x,y)→(−x,y)) reverses every ring's signed
                            // area, so CCW islands would read as CW holes and vice versa.
                            // Restore each ring's original winding sign so the region's
                            // fill semantics survive the map (containment / holes()).
                            let after = crate::region::signed_area2(&mapped);
                            if after != 0 && (before > 0) != (after > 0) {
                                mapped.reverse();
                            }
                            mapped
                        })
                        .collect(),
                },
            },
        }
    }

    /// Axis-aligned bounding box `(min, max)`, inflated by the radius. The box covers
    /// arc *bulge* (it is taken over the flattened skeleton, not just the corners), so
    /// an arc bowing past its endpoints is enclosed. Empty shapes return `None`.
    pub fn bbox(&self) -> Option<(Point, Point)> {
        let pts = self.skeleton_points();
        let first = *pts.first()?;
        let (mut min, mut max) = (first, first);
        for p in &pts {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        let r = self.radius();
        Some((
            Point {
                x: min.x - r,
                y: min.y - r,
            },
            Point {
                x: max.x + r,
                y: max.y + r,
            },
        ))
    }

    /// This shape's corner vertices in order (`start` + each segment's `end`). For
    /// drawing/serialising the straight skeleton; **arc curvature is not reflected**
    /// here — geometry that must respect arcs uses [`bbox`]/[`segments`], and arc-aware
    /// export walks [`path`] directly.
    pub fn points(&self) -> Vec<Point> {
        match self {
            Shape2D::Area { region } => region.rings.iter().flatten().copied().collect(),
            _ => self.path().corners(),
        }
    }

    /// Every lattice point this shape is *defined by* (path defining points including
    /// arc mids / Bézier controls, or an [`Area`](Shape2D::Area)'s ring vertices) — the
    /// exhaustive set for coordinate-range validation at ingest boundaries (issue 0018,
    /// [`MAX_COORD`]). Distinct from [`points`](Shape2D::points), which returns only the
    /// straight-skeleton corners.
    pub fn coords(&self) -> Vec<Point> {
        match self {
            Shape2D::Area { region } => region.rings.iter().flatten().copied().collect(),
            _ => self.path().defining_points(),
        }
    }

    /// Is `p` inside this shape's filled area? `Polygon` uses point-in-polygon
    /// (boundary counts as inside); a `Stroke` has no area, so `false`. Used for
    /// board containment (the outline is a polygon).
    pub fn contains_point(&self, p: Point) -> bool {
        self.area_contains(p)
    }

    /// The point on this shape's boundary closest to `p` (exact-ish: the projection
    /// parameter is computed in f64 and rounded to nm). Used to pull an out-of-bounds
    /// component back to the board edge. Empty shapes return `p` unchanged.
    pub fn closest_boundary_point(&self, p: Point) -> Point {
        let mut best = p;
        let mut best_d2 = i128::MAX;
        for (a, b) in self.segments() {
            let q = closest_on_segment(p, a, b);
            let d2 = (q.x - p.x) as i128 * (q.x - p.x) as i128
                + (q.y - p.y) as i128 * (q.y - p.y) as i128;
            if d2 < best_d2 {
                best_d2 = d2;
                best = q;
            }
        }
        best
    }

    /// The skeleton's segments as straight edges, **flattening any arc to chord
    /// tolerance** ([`DEFAULT_CHORD_TOL`]) — the single seam through which arcs reach
    /// the exact-integer clearance/boolean kernel (strategy A). A polygon's boundary
    /// closes; a lone-point stroke yields one degenerate segment `(p, p)`.
    fn segments(&self) -> Vec<(Point, Point)> {
        let pts = self.skeleton_points();
        match self {
            Shape2D::Stroke { .. } => {
                if pts.len() == 1 {
                    vec![(pts[0], pts[0])]
                } else {
                    pts.windows(2).map(|w| (w[0], w[1])).collect()
                }
            }
            Shape2D::Polygon { .. } => {
                let n = pts.len();
                (0..n).map(|i| (pts[i], pts[(i + 1) % n])).collect()
            }
            // Every ring closes on itself (last→first); rings are not joined to each other.
            Shape2D::Area { region } => region
                .rings
                .iter()
                .filter(|r| r.len() >= 2)
                .flat_map(|r| (0..r.len()).map(move |i| (r[i], r[(i + 1) % r.len()])))
                .collect(),
        }
    }

    /// The flattened skeleton polyline (arcs subdivided). For a polygon this is the
    /// boundary ring's points, not wrapped; for an [`Area`](Shape2D::Area) it is every
    /// ring's points concatenated (used only for bbox and the vertex-in-area overlap test,
    /// where ring membership is irrelevant).
    fn skeleton_points(&self) -> Vec<Point> {
        match self {
            Shape2D::Area { region } => region.rings.iter().flatten().copied().collect(),
            _ => self.path().flatten(DEFAULT_CHORD_TOL),
        }
    }

    /// Does this shape's *filled area* contain point `p`? Strokes have no area
    /// (their copper extent comes entirely from the radius), so always `false`.
    fn area_contains(&self, p: Point) -> bool {
        match self {
            Shape2D::Stroke { .. } => false,
            Shape2D::Polygon { .. } => point_in_polygon(p, &self.skeleton_points()),
            Shape2D::Area { region } => region.contains_point(p),
        }
    }
}

// ----------------------------------------------------------------------------
// Exact clearance kernel.
// ----------------------------------------------------------------------------

/// Is the edge-to-edge gap between two shapes a clearance violation, for `min_clr ≥
/// 0`? Two cases:
///   - **Overlapping or exactly touching** regions (gap `≤ 0`) always violate. For
///     DRC this is the right call: copper of two different nets that touches is a
///     short, regardless of the rule value (so an exact zero-gap touch under a
///     `min_clr == 0` rule is reported, even though `0 < 0` is false).
///   - **Strictly disjoint** regions violate iff the gap `< min_clr`, where gap =
///     `skeleton_distance(a, b) − radius(a) − radius(b)` — i.e. `skeleton_distance <
///     min_clr + radius(a) + radius(b)`.
///
/// All exact i128, deterministic.
pub fn clearance_violated(a: &Shape2D, b: &Shape2D, min_clr: Nm) -> bool {
    // Overlapping/touching skeletons ⇒ the inflated regions overlap or touch (gap ≤ 0).
    if skeletons_overlap(a, b) {
        return true;
    }
    let thr = min_clr + a.radius() + b.radius();
    if thr <= 0 {
        return false; // disjoint skeletons can't be within a non-positive distance
    }
    let thr2 = (thr as i128) * (thr as i128);
    for &(a1, a2) in &a.segments() {
        for &(b1, b2) in &b.segments() {
            if segs_within(a1, a2, b1, b2, thr2) {
                return true;
            }
        }
    }
    false
}

/// Do two skeletons touch — a boundary/segment intersection, or one skeleton's
/// vertex inside the other's filled area? (⇒ the regions overlap, gap ≤ 0.)
fn skeletons_overlap(a: &Shape2D, b: &Shape2D) -> bool {
    // Flattened skeleton points (arcs included) — so a curved edge bowing fully inside
    // the other shape, with no edge crossing, is still caught.
    if a.skeleton_points().iter().any(|&p| b.area_contains(p))
        || b.skeleton_points().iter().any(|&p| a.area_contains(p))
    {
        return true;
    }
    for &(a1, a2) in &a.segments() {
        for &(b1, b2) in &b.segments() {
            if segs_intersect(a1, a2, b1, b2) {
                return true;
            }
        }
    }
    false
}

/// Exact squared distance from point `p` to segment `a`–`b`, as `(num, den)` with
/// `dist² = num/den` and `den > 0`.
fn pt_seg_d2(p: Point, a: Point, b: Point) -> (i128, i128) {
    // The worst i128 chain in the kernel (`|w|²·den ≤ 64·C⁴`); [`KERNEL_SAFE_COORD`] is
    // its true ceiling. Assert against that (not the tighter ingest [`MAX_COORD`]) so
    // legal composition — a placement + courtyard within the headroom — never panics.
    debug_assert!(
        point_kernel_safe(p) && point_kernel_safe(a) && point_kernel_safe(b),
        "pt_seg_d2 coordinate exceeds KERNEL_SAFE_COORD; the i128 product may overflow (issue 0018)"
    );
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy;
    if den == 0 {
        return (wx * wx + wy * wy, 1); // degenerate segment = point a
    }
    let t = wx * vx + wy * vy;
    if t <= 0 {
        (wx * wx + wy * wy, 1)
    } else if t >= den {
        let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
        (ux * ux + uy * uy, 1)
    } else {
        // Perpendicular: |w|² − t²/den = (|w|²·den − t²)/den.
        let ww = wx * wx + wy * wy;
        (ww * den - t * t, den)
    }
}

/// The point on segment `a`–`b` closest to `p`: project, clamp the parameter to
/// `[0, 1]`, round to nm. f64 intermediate (the parameter is rational) — fine for the
/// approximate board-containment pull-back; integer endpoints stay exact at t∈{0,1}.
fn closest_on_segment(p: Point, a: Point, b: Point) -> Point {
    let (vx, vy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let len2 = vx * vx + vy * vy;
    if len2 == 0.0 {
        return a;
    }
    let t = (((p.x - a.x) as f64 * vx + (p.y - a.y) as f64 * vy) / len2).clamp(0.0, 1.0);
    Point {
        x: (a.x as f64 + t * vx).round() as Nm,
        y: (a.y as f64 + t * vy).round() as Nm,
    }
}

// ----------------------------------------------------------------------------
// Arc geometry: exact-rational centre (for export) + trig-free tessellation.
// ----------------------------------------------------------------------------

/// The circumcentre of three points as `(x_num, y_num, den)` with `cx = x_num/den`,
/// `cy = y_num/den` — **exact** in `i128` (no rounding). `den == 0` iff the points are
/// collinear (no finite centre). This is what `G02`/`G03` I/J and an SVG `A` radius
/// derive from at export; stored arcs keep their three lattice points, so the centre is
/// computed, never stored. (`den == 2·cross(a,b,c)`, so its sign also gives the turn
/// direction of the arc.)
pub fn circumcenter(a: Point, b: Point, c: Point) -> (i128, i128, i128) {
    // Numerators are ~`C³` (e.g. `a2·(by−cy)`), lower-order than [`pt_seg_d2`]'s
    // quartic, so [`KERNEL_SAFE_COORD`] keeps them well within i128. Assert in debug.
    debug_assert!(
        point_kernel_safe(a) && point_kernel_safe(b) && point_kernel_safe(c),
        "circumcenter coordinate exceeds KERNEL_SAFE_COORD (issue 0018)"
    );
    let (ax, ay) = (a.x as i128, a.y as i128);
    let (bx, by) = (b.x as i128, b.y as i128);
    let (cx, cy) = (c.x as i128, c.y as i128);
    let den = 2 * (ax * (by - cy) + bx * (cy - ay) + cx * (ay - by));
    let a2 = ax * ax + ay * ay;
    let b2 = bx * bx + by * by;
    let c2 = cx * cx + cy * cy;
    let ux = a2 * (by - cy) + b2 * (cy - ay) + c2 * (ay - by);
    let uy = a2 * (cx - bx) + b2 * (ax - cx) + c2 * (bx - ax);
    (ux, uy, den)
}

/// Append the flattening of the circular arc `start`→`mid`→`end` to `out` (excluding
/// `start`, which the caller already pushed; including `end`). Subdivides recursively
/// until each chord's sagitta ≤ `tol`. A collinear triple degenerates to the straight
/// chord. Uses only IEEE `sqrt`/division (correctly-rounded ⇒ deterministic) — **no
/// `sin`/`cos`, no `hypot`** (the latter isn't IEEE-mandated correctly-rounded) —
/// mirroring the `closest_on_segment` / `region::segment_rect` precedent.
fn flatten_arc(start: Point, mid: Point, end: Point, tol: Nm, out: &mut Vec<Point>) {
    let (ux, uy, den) = circumcenter(start, mid, end);
    if den == 0 {
        out.push(end); // collinear: a straight chord
        return;
    }
    let (cx, cy) = (ux as f64 / den as f64, uy as f64 / den as f64);
    let (rx, ry) = (start.x as f64 - cx, start.y as f64 - cy);
    let r = (rx * rx + ry * ry).sqrt().max(1.0);
    let tol = tol.max(1) as f64;
    // Turn direction of the arc (CCW > 0 / CW < 0). `den == 2·cross(start, mid, end)`,
    // so its sign **is** that turn — and both half-arcs share it. The recursion uses it
    // to pick the correct side of every chord, so a half-arc spanning ≥ 180° (a skewed
    // `mid` on a major arc) still tessellates the intended side, not its complement.
    let turn = den.signum() as i32;
    subdivide_arc(start, mid, cx, cy, r, tol, turn, out);
    subdivide_arc(mid, end, cx, cy, r, tol, turn, out);
}

/// Recursively flatten the sub-arc between `p` and `q` on the circle (centre `cx,cy`,
/// radius `r`), on the side matching the parent arc's `turn`. Pushes intermediate points
/// and `q`. Robust for sub-arcs of any span: the apex is the perpendicular-bisector ∩
/// circle point whose turn matches, and the stop test is the true apex-to-chord sagitta.
#[allow(clippy::too_many_arguments)]
fn subdivide_arc(
    p: Point,
    q: Point,
    cx: f64,
    cy: f64,
    r: f64,
    tol: f64,
    turn: i32,
    out: &mut Vec<Point>,
) {
    let (ex, ey) = (q.x as f64 - p.x as f64, q.y as f64 - p.y as f64);
    let elen = (ex * ex + ey * ey).sqrt();
    if elen == 0.0 {
        out.push(q); // degenerate chord
        return;
    }
    // Unit normal to the chord; the two circle points on the perpendicular bisector are
    // centre ± r·n. Pick the side whose orientation (p → m → q) matches the arc's turn.
    let (nx, ny) = (-ey / elen, ex / elen);
    let o1 = (cx + r * nx - p.x as f64) * ey - (cy + r * ny - p.y as f64) * ex;
    let s = if (o1 > 0.0) == (turn > 0) { 1.0 } else { -1.0 };
    let (mfx, mfy) = (cx + s * r * nx, cy + s * r * ny);
    // True sagitta = perpendicular distance from the apex to the chord.
    let sag = ((mfx - p.x as f64) * ey - (mfy - p.y as f64) * ex).abs() / elen;
    if sag <= tol {
        out.push(q);
        return;
    }
    let m = Point {
        x: mfx.round() as Nm,
        y: mfy.round() as Nm,
    };
    if m == p || m == q {
        out.push(q); // apex rounds onto an endpoint: at grid resolution
        return;
    }
    subdivide_arc(p, m, cx, cy, r, tol, turn, out);
    subdivide_arc(m, q, cx, cy, r, tol, turn, out);
}

/// Max recursion depth for Bézier flattening — a backstop against pathological
/// non-convergence at integer resolution. Cubic control-point deviation shrinks ~4×
/// per subdivision, so even a board-spanning curve reaches µm flatness well within it.
const MAX_BEZIER_DEPTH: u32 = 24;

/// Integer midpoint of two lattice points — de Casteljau's only operation. By the
/// convex-hull property every generated point stays within the input control hull, so
/// the i64 sum `a.x + b.x` is never the binding limit. The real ceiling for the whole
/// flatten is the flatness test's i128 product in [`pt_seg_d2`] (≈`64·C⁴`), which is
/// safe because every coordinate is bounded by [`MAX_COORD`] (`±1e9` nm, ~2.7× margin)
/// — the crate-wide coordinate-range ceiling, not specific to Béziers.
fn midpoint(a: Point, b: Point) -> Point {
    Point {
        x: (a.x + b.x) / 2,
        y: (a.y + b.y) / 2,
    }
}

/// Append the flattening of the quadratic Bézier `p0`→(`ctrl`)→`p2` to `out`
/// (excluding `p0`, including `p2`). Pure-integer de Casteljau: subdivide at the
/// midpoint until the control point is within `tol` of the chord. No float, no `sqrt`.
/// Uses *segment* distance (not line distance) so a collinear control point that
/// overshoots an endpoint still forces subdivision — the curve's overshoot is kept.
fn flatten_quad(p0: Point, ctrl: Point, p2: Point, tol: Nm, out: &mut Vec<Point>) {
    let t = tol.max(1) as i128;
    subdivide_quad(p0, ctrl, p2, t * t, 0, out);
}

fn subdivide_quad(p0: Point, c: Point, p2: Point, tol2: i128, depth: u32, out: &mut Vec<Point>) {
    if depth >= MAX_BEZIER_DEPTH || pt_seg_within2(c, p0, p2, tol2) {
        out.push(p2);
        return;
    }
    let p01 = midpoint(p0, c);
    let p12 = midpoint(c, p2);
    let m = midpoint(p01, p12);
    subdivide_quad(p0, p01, m, tol2, depth + 1, out);
    subdivide_quad(m, p12, p2, tol2, depth + 1, out);
}

/// Append the flattening of the cubic Bézier `p0`→(`c1`,`c2`)→`p3` to `out` (excluding
/// `p0`, including `p3`). Pure-integer de Casteljau: subdivide at the midpoint until
/// **both** control points are within `tol` of the chord. No float, no `sqrt`.
fn flatten_cubic(p0: Point, c1: Point, c2: Point, p3: Point, tol: Nm, out: &mut Vec<Point>) {
    let t = tol.max(1) as i128;
    subdivide_cubic(p0, c1, c2, p3, t * t, 0, out);
}

#[allow(clippy::too_many_arguments)]
fn subdivide_cubic(
    p0: Point,
    c1: Point,
    c2: Point,
    p3: Point,
    tol2: i128,
    depth: u32,
    out: &mut Vec<Point>,
) {
    if depth >= MAX_BEZIER_DEPTH
        || (pt_seg_within2(c1, p0, p3, tol2) && pt_seg_within2(c2, p0, p3, tol2))
    {
        out.push(p3);
        return;
    }
    let p01 = midpoint(p0, c1);
    let p12 = midpoint(c1, c2);
    let p23 = midpoint(c2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let m = midpoint(p012, p123);
    subdivide_cubic(p0, p01, p012, m, tol2, depth + 1, out);
    subdivide_cubic(m, p123, p23, p3, tol2, depth + 1, out);
}

/// Is the squared distance from point `p` to segment `a`–`b` strictly `< thr2`?
/// Tested as `num < thr2·den` (no fraction min, so no cross-multiplying two large
/// numerators — that would overflow i128 at board scale).
fn pt_seg_within2(p: Point, a: Point, b: Point, thr2: i128) -> bool {
    let (num, den) = pt_seg_d2(p, a, b);
    num < thr2 * den
}

/// For two **disjoint** segments (callers handle intersection separately), is their
/// minimum squared distance `< thr2`? The min is attained at one of the four
/// endpoint-to-opposite-segment distances.
fn segs_within(a1: Point, a2: Point, b1: Point, b2: Point, thr2: i128) -> bool {
    pt_seg_within2(a1, b1, b2, thr2)
        || pt_seg_within2(a2, b1, b2, thr2)
        || pt_seg_within2(b1, a1, a2, thr2)
        || pt_seg_within2(b2, a1, a2, thr2)
}

/// 2D orientation sign of (a, b, c): +1 CCW, −1 CW, 0 collinear (exact i128).
fn orient(a: Point, b: Point, c: Point) -> i32 {
    let v = (b.x - a.x) as i128 * (c.y - a.y) as i128 - (b.y - a.y) as i128 * (c.x - a.x) as i128;
    v.signum() as i32
}

fn on_seg(a: Point, b: Point, p: Point) -> bool {
    p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
}

/// Do segments `a1a2` and `b1b2` intersect (including touching/collinear overlap)?
fn segs_intersect(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = orient(b1, b2, a1);
    let d2 = orient(b1, b2, a2);
    let d3 = orient(a1, a2, b1);
    let d4 = orient(a1, a2, b2);
    if d1 != d2 && d3 != d4 {
        return true;
    }
    (d1 == 0 && on_seg(b1, b2, a1))
        || (d2 == 0 && on_seg(b1, b2, a2))
        || (d3 == 0 && on_seg(a1, a2, b1))
        || (d4 == 0 && on_seg(a1, a2, b2))
}

/// Point-in-polygon (crossing number), exact integer; boundary counts as inside.
fn point_in_polygon(p: Point, poly: &[Point]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    // On-boundary ⇒ inside. This pre-check must run BEFORE the crossing loop: the
    // crossing test below uses `(lhs < rhs) == (dy > 0)`, whose `>=`-vs-`>` edge only
    // arises for a point exactly on a downward edge — already handled here, so the
    // crossing loop never sees it. Do not reorder.
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + 1) % n]);
        if orient(a, b, p) == 0 && on_seg(a, b, p) {
            return true;
        }
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (pi, pj) = (poly[i], poly[j]);
        // Ray to +x: does edge (pj,pi) cross the horizontal line y = p.y to the right?
        if (pi.y > p.y) != (pj.y > p.y) {
            // x of the intersection > p.x ? Compare without division (exact).
            // x_int = pi.x + (p.y - pi.y)*(pj.x - pi.x)/(pj.y - pi.y)
            let dy = (pj.y - pi.y) as i128;
            let lhs = (p.x - pi.x) as i128 * dy;
            let rhs = (p.y - pi.y) as i128 * (pj.x - pi.x) as i128;
            // We need p.x < x_int  ⇔  (p.x-pi.x)*dy < (p.y-pi.y)*(pj.x-pi.x), sign-adjusted.
            if (lhs < rhs) == (dy > 0) {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// The convex hull of `points` as a CCW ring of its extreme vertices (Andrew's
/// monotone-chain algorithm). Built on the exact-integer [`orient`] predicate, so it
/// is deterministic and free of floating-point error; collinear points lying on a
/// hull edge are dropped and exact duplicate points are ignored. Fewer than three
/// *distinct* points cannot form a polygon — the deduplicated input is returned
/// unchanged (0, 1, or 2 points).
pub fn convex_hull(points: &[Point]) -> Vec<Point> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.x.cmp(&b.x).then(a.y.cmp(&b.y)));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let mut hull: Vec<Point> = Vec::with_capacity(2 * pts.len());
    // Lower hull (left → right), then upper hull (right → left). Each pops while the
    // last turn is not a strict left turn (`orient <= 0`), which removes both right
    // turns and collinear points, yielding a minimal CCW ring.
    for &p in &pts {
        while hull.len() >= 2 && orient(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0 {
            hull.pop();
        }
        hull.push(p);
    }
    let lower = hull.len() + 1;
    for &p in pts.iter().rev().skip(1) {
        while hull.len() >= lower && orient(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop(); // the closing point duplicates the start
    hull
}

// ----------------------------------------------------------------------------
// z-stackup, roles, materials, features.
// ----------------------------------------------------------------------------

/// A vertical extent in nm, `lo ≤ hi`. z increases upward; the board bottom face is
/// `0` and the top face is the board thickness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZRange {
    pub lo: Nm,
    pub hi: Nm,
}

impl ZRange {
    pub fn new(lo: Nm, hi: Nm) -> ZRange {
        ZRange {
            lo: lo.min(hi),
            hi: lo.max(hi),
        }
    }
    /// Do two z-ranges overlap (touching counts)? This is the 2.5D "same/adjacent
    /// layer" test once z comes from discrete stackup slabs.
    pub fn overlaps(&self, o: &ZRange) -> bool {
        self.lo <= o.hi && o.lo <= self.hi
    }
}

/// What a region *is* — kept small and physical. Named PCB features (fiducials,
/// mouse-bites, thermal relief) are compositions over these, not new roles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Electrically active copper (a pad, trace, via annulus, pour).
    Conductor,
    /// Board body / dielectric. Its outline boundary *is* the board edge.
    Substrate,
    /// Absence of material: a drill, board cutout, milled pocket.
    Void,
    /// Reserved space nothing may intrude into, by kind.
    Keepout(KeepoutKind),
    /// Surface marking (silkscreen).
    Marking,
    /// Solder-mask material (positive). Openings are `Void` deletion volumes at mask
    /// z, not a negative layer (Decision 13 — no negative layers).
    Mask,
    /// A mechanical/reference datum (e.g. an MCAD fit point).
    Datum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeepoutKind {
    Copper,
    Component,
    Drill,
    Route,
}

/// A physical material. Carries a name now; physical properties (resistivity,
/// permittivity, thermal) attach here later so simulation reads the same model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Material {
    pub name: String,
}

impl Material {
    pub fn named(name: &str) -> Material {
        Material { name: name.into() }
    }
}

/// Where a feature is in space. `Prism` is the 2.5D case (a 2D shape over a z-range);
/// `Solid` is reserved for arbitrary 3D (not built — keeps 3D representable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Extent {
    Prism { shape: Shape2D, z: ZRange },
}

/// A purposed region of space: the physical-geometry unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    pub role: Role,
    pub material: Option<Material>,
    pub extent: Extent,
}

impl Feature {
    pub fn prism(role: Role, shape: Shape2D, z: ZRange) -> Feature {
        Feature {
            role,
            material: None,
            extent: Extent::Prism { shape, z },
        }
    }
    pub fn with_material(mut self, m: Material) -> Feature {
        self.material = Some(m);
        self
    }
    fn prism_parts(&self) -> (&Shape2D, &ZRange) {
        match &self.extent {
            Extent::Prism { shape, z } => (shape, z),
        }
    }
    /// Pure-geometry clash: z-ranges overlap **and** the 2D shapes are within
    /// `min_clr` edge-to-edge. *Role/net policy is the caller's* (DRC decides which
    /// feature pairs warrant a check — e.g. different-net conductors).
    pub fn clears(&self, other: &Feature, min_clr: Nm) -> bool {
        let (sa, za) = self.prism_parts();
        let (sb, zb) = other.prism_parts();
        !(za.overlaps(zb) && clearance_violated(sa, sb, min_clr))
    }
}

/// A physical [`Feature`] paired with the electrical **net** it carries, if any.
/// This is the converged copper-clearance currency (it replaced the former ad-hoc
/// copper-piece type): the net is an
/// *annotation alongside* the geometry, **never a field on [`Feature`]** —
/// connectivity is authoritative and lives separately (see
/// docs/geometry-model-convergence.md, Decision 12). `net == None` means no
/// electrical identity: board substrate, a silk marking, a void, or a floating pad.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetFeature {
    pub net: Option<NetId>,
    pub feature: Feature,
}

impl NetFeature {
    pub fn new(net: Option<NetId>, feature: Feature) -> NetFeature {
        NetFeature { net, feature }
    }
    /// A feature with no electrical identity (substrate, silk, void).
    pub fn netless(feature: Feature) -> NetFeature {
        NetFeature { net: None, feature }
    }
    /// Do two features belong to the **same** net? Two unnetted pieces are *not* the
    /// same net — an unnetted piece shares identity with nothing. (The different-net
    /// clearance *policy* stays in the caller; this is just net identity.)
    pub fn same_net(&self, other: &NetFeature) -> bool {
        matches!((&self.net, &other.net), (Some(a), Some(b)) if a == b)
    }
}

/// A copper/dielectric/etc. slab: a named z-range with a default role + material.
/// A "layer" in the familiar sense is one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slab {
    pub name: String,
    pub z: ZRange,
    pub role: Role,
    pub material: Option<Material>,
}

/// The board stackup: the ordered set of slabs that gives a "layer" its real z. The
/// 2.5D view is a projection of this; defaults let a project ignore z entirely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stackup {
    pub slabs: Vec<Slab>,
}

impl Stackup {
    /// The familiar default: 1.6 mm board, 1 oz copper top and bottom, with solder
    /// mask and silkscreen at honest z on each side. Bottom copper at `[0, C]`, top
    /// copper at `[T−C, T]`, core dielectric between; the mask/silk slabs extend
    /// contiguously outward from the outer copper (Decision 13 — silk/mask are named
    /// z-intervals, resolved away at elaboration).
    pub fn default_2layer() -> Stackup {
        let t = BOARD_THICKNESS;
        let c = COPPER_THICKNESS;
        let mask = MASK_THICKNESS;
        let silk = SILK_THICKNESS;
        Stackup {
            slabs: vec![
                Slab {
                    name: "B.SilkS".into(),
                    z: ZRange::new(-mask - silk, -mask),
                    role: Role::Marking,
                    material: Some(Material::named("ink")),
                },
                Slab {
                    name: "B.Mask".into(),
                    z: ZRange::new(-mask, 0),
                    role: Role::Mask,
                    material: Some(Material::named("soldermask")),
                },
                Slab {
                    name: "B.Cu".into(),
                    z: ZRange::new(0, c),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                },
                Slab {
                    name: "core".into(),
                    z: ZRange::new(c, t - c),
                    role: Role::Substrate,
                    material: Some(Material::named("FR4")),
                },
                Slab {
                    name: "F.Cu".into(),
                    z: ZRange::new(t - c, t),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                },
                Slab {
                    name: "F.Mask".into(),
                    z: ZRange::new(t, t + mask),
                    role: Role::Mask,
                    material: Some(Material::named("soldermask")),
                },
                Slab {
                    name: "F.SilkS".into(),
                    z: ZRange::new(t + mask, t + mask + silk),
                    role: Role::Marking,
                    material: Some(Material::named("ink")),
                },
            ],
        }
    }

    /// The z-range of a named slab (the bridge a 2.5D "place this on F.Cu" uses).
    pub fn slab_z(&self, name: &str) -> Option<ZRange> {
        self.slab(name).map(|s| s.z)
    }

    /// The named slab itself (z **and** role/material). A graphic's role comes from
    /// its slab — silk slabs are `Role::Marking`, a fab slab is `Role::Datum`
    /// (Decision 15) — so lowering forward-queries the slab rather than hardcoding.
    pub fn slab(&self, name: &str) -> Option<&Slab> {
        self.slabs.iter().find(|s| s.name == name)
    }

    /// The conductor slabs, ordered **top-most first** (descending z). This is the
    /// bridge from an abstract copper layer to its real z: the top outer copper is
    /// index `0`, the bottom outer copper is the last entry, and inner copper layers
    /// fall in between in physical stack order — matching
    /// [`route::Layer::depth`](crate::route::Layer::depth) (`Top` = 0, `Inner(n)` =
    /// `1+n`, `Bottom` = last).
    pub fn copper_slabs(&self) -> Vec<&Slab> {
        let mut cu: Vec<&Slab> = self
            .slabs
            .iter()
            .filter(|s| s.role == Role::Conductor)
            .collect();
        cu.sort_by_key(|s| std::cmp::Reverse(s.z.hi));
        cu
    }

    /// The z-range of the `i`-th copper layer counting from the top (0 = top outer).
    /// `None` if the stackup has fewer than `i+1` copper layers.
    pub fn nth_copper_from_top(&self, i: usize) -> Option<ZRange> {
        self.copper_slabs().get(i).map(|s| s.z)
    }

    /// The top outer copper z-range (highest-z conductor slab).
    pub fn top_copper(&self) -> Option<ZRange> {
        self.copper_slabs().first().map(|s| s.z)
    }

    /// The bottom outer copper z-range (lowest-z conductor slab).
    pub fn bottom_copper(&self) -> Option<ZRange> {
        self.copper_slabs().last().map(|s| s.z)
    }

    /// The solder-mask slab immediately **outboard** of the top outer copper — the
    /// nearest `Role::Mask` slab sitting above (higher z than) the top copper, i.e. the
    /// mask a top-side pad opens. Resolved by **role + z-position**, not by a hardcoded
    /// slab name (Decision 13 — names are the authored-reference vocabulary, but a
    /// derived lookup queries the stackup): a custom stackup whose mask slab is named
    /// `TopMask` resolves just the same. `None` if there is no top copper or no mask
    /// slab above it (that side simply has no mask to open).
    pub fn top_mask(&self) -> Option<ZRange> {
        let top = self.top_copper()?;
        self.slabs
            .iter()
            .filter(|s| s.role == Role::Mask && s.z.lo >= top.hi)
            .min_by_key(|s| s.z.lo)
            .map(|s| s.z)
    }

    /// The solder-mask slab immediately **outboard** of the bottom outer copper — the
    /// nearest `Role::Mask` slab below (lower z than) the bottom copper. The mirror of
    /// [`top_mask`](Self::top_mask); same role + z-position query.
    pub fn bottom_mask(&self) -> Option<ZRange> {
        let bot = self.bottom_copper()?;
        self.slabs
            .iter()
            .filter(|s| s.role == Role::Mask && s.z.hi <= bot.lo)
            .max_by_key(|s| s.z.hi)
            .map(|s| s.z)
    }

    /// The physical **board body** vertical extent — the span of the conductor and
    /// substrate slabs (copper + dielectric), lowest face to highest. This is the z a
    /// board substrate prism or a through-hole/plated barrel spans; it deliberately
    /// **excludes** the surface mask and silk slabs, which sit outside the board body
    /// (a drill through the body is what matters, not the ink on top). Falls back to
    /// the full slab span if the stackup has no conductor/substrate slabs at all.
    pub fn board_z(&self) -> Option<ZRange> {
        let body: Vec<&Slab> = self
            .slabs
            .iter()
            .filter(|s| matches!(s.role, Role::Conductor | Role::Substrate))
            .collect();
        // Fall back to all slabs only if there is no board body at all.
        let slabs: Vec<&Slab> = if body.is_empty() {
            self.slabs.iter().collect()
        } else {
            body
        };
        let lo = slabs.iter().map(|s| s.z.lo).min()?;
        let hi = slabs.iter().map(|s| s.z.hi).max()?;
        Some(ZRange::new(lo, hi))
    }

    /// The **full** stackup vertical extent — the span of *every* slab, mask and silk
    /// included. This is the z a through-cut spans: a milled board cutout or a drill
    /// physically pierces the mask and silk as well as the board body, unlike
    /// [`board_z`](Self::board_z) (the body-only extent a substrate prism or a plated
    /// barrel occupies). `None` only for an empty stackup.
    pub fn full_z(&self) -> Option<ZRange> {
        let lo = self.slabs.iter().map(|s| s.z.lo).min()?;
        let hi = self.slabs.iter().map(|s| s.z.hi).max()?;
        Some(ZRange::new(lo, hi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MM: Nm = 1_000_000;
    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    #[test]
    fn coord_ok_bound_is_inclusive() {
        assert!(coord_ok(MAX_COORD));
        assert!(coord_ok(-MAX_COORD));
        assert!(!coord_ok(MAX_COORD + 1));
        assert!(!coord_ok(-MAX_COORD - 1));
    }

    #[test]
    fn kernel_safe_predicate_boundary_is_inclusive() {
        // (The ceiling ordering + i128-safety are compile-time-guarded at the const;
        // this checks the predicate's boundary behaviour.)
        assert!(coord_kernel_safe(KERNEL_SAFE_COORD));
        assert!(coord_kernel_safe(-KERNEL_SAFE_COORD));
        assert!(!coord_kernel_safe(KERNEL_SAFE_COORD + 1));
        assert!(!point_kernel_safe(pt(KERNEL_SAFE_COORD + 1, 0)));
    }

    /// Companion to the solver's F1 regression: the clearance kernel (`pt_seg_d2`) also
    /// runs on composed world coords past `MAX_COORD` but within `KERNEL_SAFE_COORD`, so
    /// a clearance check at that scale must not panic in debug (issue 0018, review F1).
    #[test]
    fn clearance_at_composed_coords_does_not_panic() {
        let c = MAX_COORD + 400_000; // past ingest bound, inside kernel-safe range
        let a = Shape2D::rect(pt(c, 0), MM, MM);
        let b = Shape2D::rect(pt(c + 3 * MM, 0), MM, MM);
        let _ = clearance_violated(&a, &b, 100_000); // exercises pt_seg_d2
    }

    /// In debug builds the hot kernel predicate asserts its inputs are within
    /// `KERNEL_SAFE_COORD` — the loud backstop behind the release-time boundary
    /// validation (issue 0018). Release builds trust the boundary and skip the check,
    /// so this is debug-only. Note the trip point is `KERNEL_SAFE_COORD`, not the
    /// tighter ingest `MAX_COORD` — legal composition between the two must NOT panic
    /// (see `place_at_ingest_bound_with_courtyard_does_not_panic` in solve).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "KERNEL_SAFE_COORD")]
    fn circumcenter_debug_asserts_coordinate_bound() {
        let big = KERNEL_SAFE_COORD + 1;
        let _ = circumcenter(pt(big, 0), pt(0, big), pt(1, 1));
    }

    #[test]
    fn disc_disc_clearance_is_center_distance_minus_radii() {
        let a = Shape2D::disc(pt(0, 0), MM); // r = 1mm
        let b = Shape2D::disc(pt(3 * MM, 0), MM); // centers 3mm apart, gap = 3-1-1 = 1mm
        assert!(clearance_violated(&a, &b, MM + 1), "gap 1mm < 1mm+ε");
        assert!(!clearance_violated(&a, &b, MM), "gap 1mm is not < 1mm");
        assert!(
            !clearance_violated(&a, &b, MM / 2),
            "1mm gap clears 0.5mm rule"
        );
    }

    #[test]
    fn overlapping_shapes_violate_any_positive_clearance() {
        let a = Shape2D::disc(pt(0, 0), 2 * MM);
        let b = Shape2D::disc(pt(MM, 0), 2 * MM); // overlap (centers 1mm, radii 2+2)
        assert!(clearance_violated(&a, &b, 1));
        assert!(clearance_violated(&a, &b, 10 * MM));
    }

    #[test]
    fn point_inside_filled_rect_is_overlap() {
        let rect = Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM); // covers [-2,2]^2
        let dot = Shape2D::disc(pt(0, 0), 1); // center inside the rect
        assert!(
            clearance_violated(&rect, &dot, 1),
            "a point inside the pad area is an overlap"
        );
    }

    #[test]
    fn trace_near_pad_edge_to_edge() {
        // Horizontal trace y=0, width 0.2mm (r=0.1mm); square pad centered (0,1mm) 0.4mm.
        let trace = Shape2D::trace(vec![pt(-5 * MM, 0), pt(5 * MM, 0)], MM / 5);
        let pad = Shape2D::rect(pt(0, MM), 4 * MM / 10, 4 * MM / 10); // [-0.2,0.2]x[0.8,1.2]
        // edge-to-edge gap: pad bottom at y=0.8mm, trace top at y=0.1mm → 0.7mm.
        let gap = 7 * MM / 10;
        assert!(clearance_violated(&trace, &pad, gap + 1));
        assert!(!clearance_violated(&trace, &pad, gap));
    }

    #[test]
    fn round_rect_corner_is_rounded_not_sharp() {
        // A sharp rect's corner reaches farther than a rounded one; a probe disc just
        // off the corner clears the round_rect but not the sharp rect.
        let sharp = Shape2D::rect(pt(0, 0), 2 * MM, 2 * MM); // corner at (1,1)mm
        let round = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, MM / 2);
        // Probe just beyond the corner diagonally: sharp corner gap ≈ 0.14mm,
        // rounded corner gap ≈ 0.35mm. A 0.2mm rule separates them.
        let probe = Shape2D::disc(pt(11 * MM / 10, 11 * MM / 10), 1);
        assert!(
            clearance_violated(&sharp, &probe, MM / 5),
            "sharp corner within 0.2mm"
        );
        assert!(
            !clearance_violated(&round, &probe, MM / 5),
            "rounded corner beyond 0.2mm"
        );
    }

    #[test]
    fn round_rect_radius_is_clamped_not_ballooned() {
        // r far larger than the box: clamps to min(w,h)/2 instead of growing. A
        // square 2mm box with r=10mm clamps to r=1mm → a disc of radius 1mm, so a
        // probe 1.5mm from center clears it (a ballooned shape would not).
        let rr = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, 10 * MM);
        let near = Shape2D::disc(pt(15 * MM / 10, 0), 1); // 1.5mm from center
        assert!(
            !clearance_violated(&rr, &near, MM / 10),
            "clamped shape stays ~1mm radius"
        );
        // A probe 0.9mm from center is inside the ~1mm clamped radius.
        let inside = Shape2D::disc(pt(9 * MM / 10, 0), 1);
        assert!(clearance_violated(&rr, &inside, 1));
    }

    #[test]
    fn z_overlap_gates_clearance() {
        let su = Stackup::default_2layer();
        let top = su.slab_z("F.Cu").unwrap();
        let bot = su.slab_z("B.Cu").unwrap();
        assert!(
            !top.overlaps(&bot),
            "top and bottom copper z must not overlap"
        );
        // Two overlapping discs on opposite layers do NOT clash; same layer they do.
        let s = Shape2D::disc(pt(0, 0), MM);
        let a_top = Feature::prism(Role::Conductor, s.clone(), top);
        let b_top = Feature::prism(Role::Conductor, s.clone(), top);
        let b_bot = Feature::prism(Role::Conductor, s, bot);
        assert!(
            !a_top.clears(&b_top, MM),
            "coincident copper on the same layer clashes"
        );
        assert!(
            a_top.clears(&b_bot, MM),
            "different layers do not clash geometrically"
        );
    }

    #[test]
    fn stackup_copper_accessors_are_ordered_top_down() {
        let su = Stackup::default_2layer();
        let cu = su.copper_slabs();
        assert_eq!(cu.len(), 2, "default 2-layer has two copper slabs");
        assert_eq!(cu[0].name, "F.Cu", "top-most copper is index 0");
        assert_eq!(cu[1].name, "B.Cu", "bottom copper is last");
        assert_eq!(su.top_copper(), su.slab_z("F.Cu"));
        assert_eq!(su.bottom_copper(), su.slab_z("B.Cu"));
        assert_eq!(su.nth_copper_from_top(0), su.slab_z("F.Cu"));
        assert_eq!(su.nth_copper_from_top(1), su.slab_z("B.Cu"));
        assert_eq!(su.nth_copper_from_top(2), None, "no third copper layer");
        let bz = su.board_z().unwrap();
        assert_eq!(
            (bz.lo, bz.hi),
            (0, BOARD_THICKNESS),
            "board_z spans the copper+substrate body, not the surface mask/silk"
        );
    }

    /// A zero-height slab (`lo == hi`, permitted by `ZRange::new`) flows through the
    /// stackup like any other: `slab_z`/`slab` resolve it, its range is degenerate but
    /// well-formed, and it z-*touches* an adjacent slab (closed `overlaps`) — the
    /// property a fab `Role::Datum` slab relies on (Decision 15).
    #[test]
    fn zero_height_slab_resolves_through_stackup() {
        let su = Stackup {
            slabs: vec![
                Slab {
                    name: "F.Cu".into(),
                    z: ZRange::new(0, 35_000),
                    role: Role::Conductor,
                    material: None,
                },
                Slab {
                    name: "F.Fab".into(),
                    z: ZRange::new(35_000, 35_000),
                    role: Role::Datum,
                    material: None,
                },
            ],
        };
        let fab = su.slab("F.Fab").expect("datum slab resolves");
        assert_eq!(fab.role, Role::Datum);
        assert_eq!(fab.z.lo, fab.z.hi, "zero-height: lo == hi");
        assert_eq!(su.slab_z("F.Fab"), Some(fab.z));
        assert!(
            fab.z.overlaps(&su.slab_z("F.Cu").unwrap()),
            "zero-height datum z-touches the copper it sits on"
        );
    }

    #[test]
    fn stackup_mask_accessors_resolve_by_role_and_position() {
        // Default stackup: role+position resolution agrees with the named mask slabs.
        let su = Stackup::default_2layer();
        assert_eq!(su.top_mask(), su.slab_z("F.Mask"), "top mask is F.Mask");
        assert_eq!(
            su.bottom_mask(),
            su.slab_z("B.Mask"),
            "bottom mask is B.Mask"
        );

        // A custom stackup whose mask is named `TopMask` still resolves by role + z.
        let custom = Stackup {
            slabs: vec![
                Slab {
                    name: "F.Cu".into(),
                    z: ZRange::new(0, 35_000),
                    role: Role::Conductor,
                    material: None,
                },
                Slab {
                    name: "TopMask".into(),
                    z: ZRange::new(35_000, 60_000),
                    role: Role::Mask,
                    material: None,
                },
            ],
        };
        assert_eq!(
            custom.top_mask(),
            custom.slab_z("TopMask"),
            "the mask above top copper is found by role, whatever its name"
        );
        assert_eq!(custom.bottom_mask(), None, "no mask below the only copper");
    }

    #[test]
    fn netfeature_same_net_is_identity_not_presence() {
        use crate::id::NetId;
        let s = Shape2D::disc(pt(0, 0), MM);
        let z = Stackup::default_2layer().top_copper().unwrap();
        let f = Feature::prism(Role::Conductor, s, z);
        let gnd1 = NetFeature::new(Some(NetId::new("GND")), f.clone());
        let gnd2 = NetFeature::new(Some(NetId::new("GND")), f.clone());
        let vcc = NetFeature::new(Some(NetId::new("VCC")), f.clone());
        let floating = NetFeature::netless(f);
        assert!(gnd1.same_net(&gnd2), "equal net ids are the same net");
        assert!(!gnd1.same_net(&vcc), "different net ids are not");
        assert!(
            !floating.same_net(&floating),
            "an unnetted piece shares net identity with nothing"
        );
    }

    #[test]
    fn capsule_distance_is_to_the_segment() {
        // Pill from (0,0) to (4mm,0), r=0.5mm; probe disc at (2mm, 2mm) r=0.5mm.
        let pill = Shape2D::capsule(pt(0, 0), pt(4 * MM, 0), MM / 2);
        let probe = Shape2D::disc(pt(2 * MM, 2 * MM), MM / 2);
        // gap = 2mm − 0.5 − 0.5 = 1mm.
        assert!(clearance_violated(&pill, &probe, MM + 1));
        assert!(!clearance_violated(&pill, &probe, MM));
    }

    #[test]
    fn circumcenter_is_exact() {
        // Three points on the circle centred (1mm, 0), radius 1mm.
        let (ux, uy, den) = circumcenter(pt(0, 0), pt(MM, MM), pt(2 * MM, 0));
        assert_ne!(den, 0);
        assert_eq!(ux / den, MM as i128, "cx = 1mm");
        assert_eq!(uy / den, 0, "cy = 0");
        // Collinear ⇒ no finite centre.
        let (_, _, d0) = circumcenter(pt(0, 0), pt(MM, 0), pt(2 * MM, 0));
        assert_eq!(d0, 0, "collinear triple has den == 0");
    }

    #[test]
    fn arc_tessellates_onto_its_circle() {
        // Semicircle: (-10mm,0) → (0,10mm) → (10mm,0), centre (0,0), R = 10mm.
        let r = 10 * MM;
        let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), 0);
        let pts = arc.path().flatten(DEFAULT_CHORD_TOL);
        assert!(
            pts.len() > 16,
            "a 10mm semicircle subdivides into many chords"
        );
        assert_eq!(
            *pts.first().unwrap(),
            pt(-r, 0),
            "endpoints stay exact lattice pts"
        );
        assert_eq!(*pts.last().unwrap(), pt(r, 0));
        assert!(
            pts.contains(&pt(0, r)),
            "the defining midpoint is on the polyline"
        );
        // Every vertex lies on the circle to within rounding (a few nm).
        for p in &pts {
            let d = (((p.x as f64).powi(2) + (p.y as f64).powi(2)).sqrt() - r as f64).abs();
            assert!(d < 4.0, "tessellation point off-circle by {d} nm");
        }
    }

    #[test]
    fn arc_bbox_covers_the_bulge() {
        // The same semicircle bows up to y = R between its two endpoints (both at
        // y = 0). The bbox must reach the bulge, not just the corner vertices.
        let r = 10 * MM;
        let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), 0);
        let (min, max) = arc.bbox().unwrap();
        assert_eq!(min.y, 0, "endpoints sit on y = 0");
        assert_eq!(max.y, r, "bbox reaches the arc's top (the bulge)");
        assert_eq!((min.x, max.x), (-r, r));
    }

    #[test]
    fn collinear_arc_degenerates_to_a_chord() {
        // An arc whose three points are collinear is just the straight chord.
        let arc = Shape2D::arc(pt(0, 0), pt(MM, 0), pt(2 * MM, 0), 0);
        let pts = arc.path().flatten(DEFAULT_CHORD_TOL);
        assert_eq!(
            pts,
            vec![pt(0, 0), pt(2 * MM, 0)],
            "collinear ⇒ a single chord"
        );
    }

    // Perpendicular distance from `(px,py)` to segment `a`–`b`, in f64 nm (test only).
    fn pt_seg_dist_f64(px: f64, py: f64, a: Point, b: Point) -> f64 {
        let (ax, ay) = (a.x as f64, a.y as f64);
        let (bx, by) = (b.x as f64, b.y as f64);
        let (ex, ey) = (bx - ax, by - ay);
        let len2 = ex * ex + ey * ey;
        let t = if len2 == 0.0 {
            0.0
        } else {
            (((px - ax) * ex + (py - ay) * ey) / len2).clamp(0.0, 1.0)
        };
        let (cx, cy) = (ax + t * ex, ay + t * ey);
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
    }

    #[test]
    fn straight_cubic_flattens_to_a_chord() {
        // Controls lying on the segment ⇒ the curve is the chord ⇒ no subdivision.
        let poly = Path {
            start: pt(0, 0),
            segs: vec![Seg::Cubic {
                c1: pt(3 * MM, 0),
                c2: pt(6 * MM, 0),
                end: pt(9 * MM, 0),
            }],
        }
        .flatten(DEFAULT_CHORD_TOL);
        assert_eq!(poly, vec![pt(0, 0), pt(9 * MM, 0)]);
    }

    #[test]
    fn cubic_flatten_approximates_the_curve_within_tolerance() {
        let (p0, c1, c2, p3) = (
            pt(0, 0),
            pt(0, 4 * MM),
            pt(10 * MM, -4 * MM),
            pt(10 * MM, 0),
        );
        let poly = Path {
            start: p0,
            segs: vec![Seg::Cubic { c1, c2, end: p3 }],
        }
        .flatten(DEFAULT_CHORD_TOL);
        assert_eq!(poly.first().copied(), Some(p0));
        assert_eq!(poly.last().copied(), Some(p3));
        assert!(poly.len() > 2, "a curved cubic must subdivide");
        // Every analytic curve sample lies within tol (+ rounding slack) of the polyline.
        let eval = |t: f64| {
            let u = 1.0 - t;
            let bx = u * u * u * p0.x as f64
                + 3.0 * u * u * t * c1.x as f64
                + 3.0 * u * t * t * c2.x as f64
                + t * t * t * p3.x as f64;
            let by = u * u * u * p0.y as f64
                + 3.0 * u * u * t * c1.y as f64
                + 3.0 * u * t * t * c2.y as f64
                + t * t * t * p3.y as f64;
            (bx, by)
        };
        for i in 0..=200 {
            let (x, y) = eval(i as f64 / 200.0);
            let best = poly
                .windows(2)
                .map(|w| pt_seg_dist_f64(x, y, w[0], w[1]))
                .fold(f64::MAX, f64::min);
            assert!(
                best <= DEFAULT_CHORD_TOL as f64 + 50.0,
                "curve sample {i} is {best} nm off the flattened polyline"
            );
        }
    }

    #[test]
    fn quadratic_flatten_captures_the_apex() {
        // Apex at t=½ is (p0 + 2c + p2)/4 = (5mm, 3mm).
        let (p0, c, p2) = (pt(0, 0), pt(5 * MM, 6 * MM), pt(10 * MM, 0));
        let poly = Path {
            start: p0,
            segs: vec![Seg::Quadratic { ctrl: c, end: p2 }],
        }
        .flatten(DEFAULT_CHORD_TOL);
        assert_eq!(poly.first().copied(), Some(p0));
        assert_eq!(poly.last().copied(), Some(p2));
        assert!(
            poly.iter().any(|p| (p.y - 3 * MM).abs() < 200_000),
            "a flattened vertex tracks the apex bulge near y=3mm"
        );
    }

    #[test]
    fn cubic_bbox_covers_the_bulge() {
        // A cubic bowing up to ~3.75mm; the bbox (via flatten) must reach the bulge.
        let s = Shape2D::cubic(
            pt(0, 0),
            pt(0, 5 * MM),
            pt(10 * MM, 5 * MM),
            pt(10 * MM, 0),
            MM / 10,
        );
        let (_min, max) = s.bbox().unwrap();
        assert!(max.y > 2 * MM, "bbox reaches the upward bulge: {max:?}");
    }

    #[test]
    fn major_arc_with_skewed_midpoint_stays_on_the_intended_side() {
        // A major arc (sweep > 180°) whose `mid` is skewed so one half-arc exceeds 180°.
        // A chord-midpoint projection would flip that half onto the complementary
        // (minor) side; the turn-aware bisection must keep every point on the true arc.
        // Circle centre origin, R = 10mm. start 0°, mid 200°, end 210° (CCW through mid).
        let r = 10 * MM;
        let f = |deg: f64| {
            let a = deg.to_radians();
            pt(
                (r as f64 * a.cos()).round() as Nm,
                (r as f64 * a.sin()).round() as Nm,
            )
        };
        let (start, mid, end) = (f(0.0), f(200.0), f(210.0));
        let pts = Shape2D::arc(start, mid, end, 0)
            .path()
            .flatten(DEFAULT_CHORD_TOL);
        // All points lie on the circle…
        for p in &pts {
            let d = (((p.x as f64).powi(2) + (p.y as f64).powi(2)).sqrt() - r as f64).abs();
            assert!(d < 4.0, "off-circle by {d} nm");
        }
        // …the 0°→200° half must sweep through ~100° (its interior)…
        let saw_100 = pts.iter().any(|p| {
            let ang = (p.y as f64)
                .atan2(p.x as f64)
                .to_degrees()
                .rem_euclid(360.0);
            (90.0..110.0).contains(&ang)
        });
        assert!(
            saw_100,
            "the 0°→200° half must pass through ~100°, not the minor side"
        );
        // …and no point may stray onto the complementary arc (215°..355°).
        let on_minor = pts.iter().any(|p| {
            let ang = (p.y as f64)
                .atan2(p.x as f64)
                .to_degrees()
                .rem_euclid(360.0);
            (215.0..355.0).contains(&ang)
        });
        assert!(!on_minor, "no point may stray onto the complementary arc");
    }

    #[test]
    fn arc_trace_clearance_is_measured_to_the_curve() {
        // A semicircular trace (-10mm,0)→(0,10mm)→(10mm,0), width 0.2mm (r=0.1mm),
        // bulging up to (0,10mm). A probe disc (r=0.1mm) sits 1mm above the bulge at
        // (0,11mm). Edge-to-edge gap = 1mm − 0.1 − 0.1 = 0.8mm — measured to the ARC's
        // top. (The straight chord between the endpoints lies on y=0, ~11mm away; if the
        // kernel mis-measured to the chord this rule would clear.)
        let r = 10 * MM;
        let trace = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), MM / 5);
        let probe = Shape2D::disc(pt(0, 11 * MM), MM / 10);
        let gap = 8 * MM / 10;
        assert!(
            clearance_violated(&trace, &probe, gap + 1),
            "0.8mm gap to the bulge"
        );
        assert!(
            !clearance_violated(&trace, &probe, gap),
            "0.8mm is not < 0.8mm"
        );
        // Sanity: a far probe well outside the arc clears a small rule.
        let far = Shape2D::disc(pt(0, 13 * MM), MM / 10);
        assert!(!clearance_violated(&trace, &far, MM / 2));
    }

    #[test]
    fn convex_hull_of_a_square_is_its_four_corners() {
        // The four corners plus an interior point and edge-midpoints; the hull keeps
        // only the corners (interior + collinear points dropped). CCW order.
        let pts = vec![
            pt(0, 0),
            pt(2 * MM, 0),
            pt(2 * MM, 2 * MM),
            pt(0, 2 * MM),
            pt(MM, MM),     // interior
            pt(MM, 0),      // collinear on bottom edge
            pt(2 * MM, MM), // collinear on right edge
            pt(MM, 2 * MM), // collinear on top edge
            pt(0, MM),      // collinear on left edge
        ];
        let hull = convex_hull(&pts);
        assert_eq!(hull.len(), 4, "only the four corners survive");
        // It is a CCW ring (every consecutive turn is a left turn).
        let n = hull.len();
        for i in 0..n {
            let a = hull[i];
            let b = hull[(i + 1) % n];
            let c = hull[(i + 2) % n];
            assert_eq!(
                orient(a, b, c),
                1,
                "hull winds CCW with no collinear corners"
            );
        }
        // The corner set matches.
        let mut got = hull.clone();
        got.sort_by(|a, b| a.x.cmp(&b.x).then(a.y.cmp(&b.y)));
        assert_eq!(
            got,
            vec![pt(0, 0), pt(0, 2 * MM), pt(2 * MM, 0), pt(2 * MM, 2 * MM)]
        );
    }

    #[test]
    fn convex_hull_handles_degenerate_inputs() {
        assert_eq!(convex_hull(&[]), vec![]);
        assert_eq!(convex_hull(&[pt(MM, MM)]), vec![pt(MM, MM)]);
        // Duplicates collapse; two distinct points have no polygon hull.
        assert_eq!(
            convex_hull(&[pt(0, 0), pt(0, 0), pt(MM, 0), pt(MM, 0)]),
            vec![pt(0, 0), pt(MM, 0)]
        );
        // Three collinear points: no area, reduces to the two extremes.
        assert_eq!(
            convex_hull(&[pt(0, 0), pt(MM, 0), pt(2 * MM, 0)]),
            vec![pt(0, 0), pt(2 * MM, 0)]
        );
    }

    #[test]
    fn determinism_same_inputs_same_answer() {
        let a = Shape2D::round_rect(pt(0, 0), 3 * MM, 2 * MM, MM / 4);
        let b = Shape2D::trace(vec![pt(0, 5 * MM), pt(10 * MM, 5 * MM)], MM / 5);
        let r1 = clearance_violated(&a, &b, 2 * MM);
        let r2 = clearance_violated(&a, &b, 2 * MM);
        assert_eq!(r1, r2);
    }

    /// A 10 mm square with a 2 mm square hole (walls at ±1 mm), as a `Shape2D::Area`:
    /// `radius`/`bbox`/`contains_point`/`points`/`inflated`/`closest_boundary_point` all
    /// respect the hole.
    #[test]
    fn area_shape_geometry_ops() {
        use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region};
        let outer = shape_to_region(
            &Shape2D::rect(pt(0, 0), 10 * MM, 10 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let hole = shape_to_region(
            &Shape2D::rect(pt(0, 0), 2 * MM, 2 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let area = Shape2D::Area {
            region: difference(&outer, &hole),
        };

        assert_eq!(area.radius(), 0, "an Area has no inflation radius");
        assert_eq!(
            area.bbox().unwrap(),
            (pt(-5 * MM, -5 * MM), pt(5 * MM, 5 * MM)),
            "bbox is the outer boundary"
        );
        assert!(area.contains_point(pt(4 * MM, 0)), "inside the filled ring");
        assert!(!area.contains_point(pt(0, 0)), "the hole is not filled");
        assert!(!area.contains_point(pt(9 * MM, 0)), "outside the board");
        assert!(area.points().len() >= 8, "outer + hole ring vertices");

        // A point deep in the hole is not filled; after a 1 mm dilation (walls at ±1 mm
        // move inward, closing the hole) that same point is filled.
        assert!(!area.contains_point(pt(0, 9 * MM / 10)));
        assert!(
            area.inflated(MM).contains_point(pt(0, 9 * MM / 10)),
            "dilation shrinks/closes the hole"
        );
        // Dilation also grows the outer boundary outward.
        let (lo, hi) = area.inflated(MM).bbox().unwrap();
        assert!(
            lo.x < -5 * MM && hi.x > 5 * MM,
            "outer boundary grew by ~1 mm"
        );

        // The nearest boundary point to the hole centre lands on a hole wall (~1 mm away).
        let q = area.closest_boundary_point(pt(0, 0));
        let d2 = (q.x as i128).pow(2) + (q.y as i128).pow(2);
        let mm = MM as i128;
        assert!(
            d2 >= (9 * mm / 10).pow(2) && d2 <= (11 * mm / 10).pow(2),
            "closest boundary is the hole wall at ~1 mm: {q:?}"
        );
    }

    /// `clears()` for an `Area` substrate: a shape inside the filled island violates (an
    /// overlap), a shape deep in a hole clears, and a shape near a hole wall violates.
    #[test]
    fn area_clears_island_versus_hole() {
        use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region};
        // 20 mm square, 4 mm square hole (walls at ±2 mm).
        let outer = shape_to_region(
            &Shape2D::rect(pt(0, 0), 20 * MM, 20 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let hole = shape_to_region(
            &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let z = ZRange::new(0, 1000);
        let area = Feature::prism(
            Role::Substrate,
            Shape2D::Area {
                region: difference(&outer, &hole),
            },
            z,
        );

        // Inside the filled island: overlaps ⇒ violates even at zero clearance.
        let inside = Feature::prism(Role::Conductor, Shape2D::disc(pt(5 * MM, 0), MM / 10), z);
        assert!(
            !area.clears(&inside, 0),
            "a shape inside the filled area is a violation"
        );

        // Deep in the hole (walls ≥ ~1.9 mm away): clears a 1 mm rule.
        let in_hole = Feature::prism(Role::Conductor, Shape2D::disc(pt(0, 0), MM / 10), z);
        assert!(
            area.clears(&in_hole, MM),
            "a shape deep inside a hole clears the hole walls"
        );

        // Near a hole wall (edge gap ~0 mm): violates a 0.5 mm rule.
        let near_wall = Feature::prism(
            Role::Conductor,
            Shape2D::disc(pt(19 * MM / 10, 0), MM / 10),
            z,
        );
        assert!(
            !area.clears(&near_wall, MM / 2),
            "a shape hard against a hole wall violates"
        );
    }

    /// A reflecting `map_points` (a bottom-side flip, `(x,y)→(−x,y)`, negative
    /// determinant) reverses every ring's signed area. `map_points` must re-sign the
    /// rings so islands stay islands and holes stay holes — otherwise `holes()`/
    /// `islands()` invert (a wave-2 bottom-silk glyph would render as its own negative).
    #[test]
    fn area_map_points_preserves_orientation_under_reflection() {
        use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region, signed_area2};
        // 10 mm square centred at +6 mm x (so a reflection across x=0 relocates it), with
        // a 4 mm hole. Filled ring x∈[1,11]∖[4,8]; hole centre at (6,0).
        let outer = shape_to_region(
            &Shape2D::rect(pt(6 * MM, 0), 10 * MM, 10 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let hole = shape_to_region(
            &Shape2D::rect(pt(6 * MM, 0), 4 * MM, 4 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let area = Shape2D::Area {
            region: difference(&outer, &hole),
        };

        let flipped = area.map_points(|p| Point { x: -p.x, y: p.y });
        let region = flipped.region().unwrap();

        // Fill semantics survive: a filled-ring point (was (9,0) → (−9,0)) is on the
        // board; the hole centre (was (6,0) → (−6,0)) is still excluded.
        assert!(
            flipped.contains_point(pt(-9 * MM, 0)),
            "island still filled"
        );
        assert!(
            !flipped.contains_point(pt(-6 * MM, 0)),
            "hole still excluded"
        );

        // Orientation survives: exactly one CCW island ring, and holes()/islands() still
        // classify correctly (the actual regression surface).
        assert_eq!(
            region.rings.iter().filter(|r| signed_area2(r) > 0).count(),
            1,
            "one CCW island ring"
        );
        assert_eq!(region.holes().rings.len(), 1, "the hole is still a hole");
        let islands = region.islands();
        assert_eq!(islands.len(), 1, "one island");
        assert_eq!(
            islands[0].rings.len(),
            2,
            "island keeps its hole (outer + hole)"
        );
    }
}
