//! The shape vocabulary: [`Seg`]/[`Path`]/[`Shape2D`], the arc/Bézier flatteners,
//! [`convex_hull`], [`circumcenter`], [`clearance_violated`], [`closest_on_segment`]
//! and the low-level segment/orientation predicates they build on. See the
//! [`geom`](crate::geom) module docs for the model; coordinate ceilings and
//! kernel-safety live in [`limits`](super::limits).

use super::limits::*;
use crate::coord::{Nm, Point};

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
pub(super) fn orient(a: Point, b: Point, c: Point) -> i32 {
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
