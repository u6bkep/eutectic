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

    /// This shape's skeleton path.
    pub fn path(&self) -> &Path {
        match self {
            Shape2D::Stroke { path, .. } | Shape2D::Polygon { path, .. } => path,
        }
    }

    /// The inflation radius (the Minkowski disc). Public so the offset kernel can
    /// realise `skeleton ⊕ disc(radius)` as a filled region.
    pub fn radius(&self) -> Nm {
        match self {
            Shape2D::Stroke { radius, .. } | Shape2D::Polygon { radius, .. } => *radius,
        }
    }

    /// This shape inflated (Minkowski ⊕ a disc) by `d`: the skeleton is unchanged and
    /// the inflation radius grows by `d`. Offsetting copper by a clearance is *exactly*
    /// this — disc Minkowski sums add radii — which is why a pour knockout never needs
    /// a bespoke polygon-offset. `d` may be negative (deflate); the radius floors at 0.
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
        self.path().corners()
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
        }
    }

    /// The flattened skeleton polyline (arcs subdivided). For a polygon this is the
    /// boundary ring's points, not wrapped.
    fn skeleton_points(&self) -> Vec<Point> {
        self.path().flatten(DEFAULT_CHORD_TOL)
    }

    /// Does this shape's *filled area* contain point `p`? Strokes have no area
    /// (their copper extent comes entirely from the radius), so always `false`.
    fn area_contains(&self, p: Point) -> bool {
        match self {
            Shape2D::Stroke { .. } => false,
            Shape2D::Polygon { .. } => point_in_polygon(p, &self.skeleton_points()),
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
/// safe at board scale (`|coord| ≤ ~1e9` nm, ~2.7× margin) — the crate-wide
/// coordinate-range assumption, not specific to Béziers (see issue 0018).
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
    /// An opening in the solder mask.
    MaskOpening,
    /// A mechanical/reference datum (e.g. an MCAD fit point).
    Datum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// The familiar default: 1.6 mm board, 1 oz copper top and bottom. Bottom copper
    /// at `[0, C]`, top copper at `[T−C, T]`, core dielectric between.
    pub fn default_2layer() -> Stackup {
        let t = BOARD_THICKNESS;
        let c = COPPER_THICKNESS;
        Stackup {
            slabs: vec![
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
            ],
        }
    }

    /// The z-range of a named slab (the bridge a 2.5D "place this on F.Cu" uses).
    pub fn slab_z(&self, name: &str) -> Option<ZRange> {
        self.slabs.iter().find(|s| s.name == name).map(|s| s.z)
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

    /// The full board vertical extent — lowest slab face to highest. The z a board
    /// substrate prism or a through-hole/plated barrel spans.
    pub fn board_z(&self) -> Option<ZRange> {
        let lo = self.slabs.iter().map(|s| s.z.lo).min()?;
        let hi = self.slabs.iter().map(|s| s.z.hi).max()?;
        Some(ZRange::new(lo, hi))
    }
}

/// The board boundary: an `outline` ([`Role::Substrate`]) with interior `cutouts`
/// ([`Role::Void`]). The **one** board representation — `outline`/`cutouts` are
/// [`Shape2D`]s, so rounded corners (a `Polygon` radius) and concave / arbitrary
/// (CAD-imported) outlines are expressible; `BoardShape::rect` is just a constructor
/// for the common case. The interior of the board is "inside the outline and outside
/// every cutout".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardShape {
    pub outline: Shape2D,
    pub cutouts: Vec<Shape2D>,
}

impl BoardShape {
    /// A rectangular board from opposite corners — sugar over the polygon form.
    pub fn rect(min: Point, max: Point) -> BoardShape {
        let c = Point {
            x: (min.x + max.x) / 2,
            y: (min.y + max.y) / 2,
        };
        BoardShape {
            outline: Shape2D::rect(c, max.x - min.x, max.y - min.y),
            cutouts: vec![],
        }
    }

    /// Is `p` on the board: inside the outline and outside every cutout?
    pub fn contains(&self, p: Point) -> bool {
        self.outline.contains_point(p) && !self.cutouts.iter().any(|c| c.contains_point(p))
    }

    /// The nearest on-board point to `p`: if `p` is outside the outline, pull it to
    /// the outline boundary; if it then sits inside a cutout, push it to that
    /// cutout's boundary. Approximate (snaps to a boundary), enough to keep a placed
    /// component on the board.
    pub fn contain(&self, p: Point) -> Point {
        let mut q = p;
        if !self.outline.contains_point(q) {
            q = self.outline.closest_boundary_point(q);
        }
        for c in &self.cutouts {
            if c.contains_point(q) {
                q = c.closest_boundary_point(q);
            }
        }
        q
    }

    /// The outline's bounding box `(min, max)` — the area a routing grid spans.
    pub fn bbox(&self) -> Option<(Point, Point)> {
        self.outline.bbox()
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
            "board_z spans the whole stack"
        );
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
}
