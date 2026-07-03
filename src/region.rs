//! Regions: filled areas with holes, and the exact-integer **boolean + offset
//! kernel** that produces them (see docs/architecture.md §8).
//!
//! This is the one hard geometry capability the pour / solder-mask / paste work all
//! sit on. A [`Region`] is a set of oriented closed rings — outer boundaries CCW,
//! holes CW — interpreted by the **non-zero winding rule**, so "board minus the
//! clearance around every foreign pad" (an area with holes), disjoint copper islands,
//! and nested cut-outs are all one type. Every boolean (`union` / `intersection` /
//! `difference`) and the `Shape2D`→`Region` dilation return a `Region`.
//!
//! ## Why this shape, and how it stays deterministic
//!
//! - **Boolean ops** subdivide the two inputs' edges at their shared crossings,
//!   classify each fragment by a midpoint test (inside / on-boundary / outside of the
//!   other region), select fragments per the operation, and stitch the survivors back
//!   into rings. Crossing points are computed **once per edge pair** in `i128` and
//!   rounded to the nm grid, then used to split *both* edges — so the two sides agree
//!   to the nanometre and no cracks open. Predicates (orientation, winding,
//!   on-segment) are exact integer; only the shared rounding is approximate, and it is
//!   deterministic.
//! - **Offset is a radius bump, not a new algorithm.** A `Shape2D` is already a
//!   skeleton ⊕ a disc of `radius`; inflating it by a clearance `c` is just
//!   `radius += c` (Minkowski sums of discs add radii — exact). [`shape_to_region`]
//!   then realises that inflated shape as a filled `Region` by the **dilation
//!   decomposition**: the set of points within `r` of the skeleton is the union of the
//!   core area (for a polygon), one rectangle per skeleton edge, and one disc per
//!   skeleton vertex. That reuses `union`, so there is exactly one boolean engine.
//! - **No runtime trig.** The radius-disc steps through the integer [`CIRCLE_DIRS`]
//!   table; a skeleton *arc* edge is flattened at the geometry seam
//!   ([`geom::Path::flatten`], chord tolerance [`geom::DEFAULT_CHORD_TOL`]) into the
//!   chord polyline this kernel sees — so the boolean only ever operates on straight
//!   edges (strategy A). The only float anywhere is the correctly-rounded IEEE `sqrt`
//!   used for those offsets, mirroring the `geom::closest_on_segment` precedent. The
//!   authoritative model now carries arcs (in [`Shape2D`]); flattening is a transient
//!   the kernel consumes, never stored — keeping the door open to an arc-exact boolean
//!   later with no change to the representation or to export.

use crate::doc::{Nm, Point};
use crate::geom::Shape2D;

/// A closed ring of vertices; the closing edge joins the last vertex to the first.
/// Outer boundaries are CCW (positive signed area), holes CW.
pub type Ring = Vec<Point>;

/// A directed edge (a fragment of a ring during the boolean).
type Edge = (Point, Point);

/// A filled region: oriented rings under the **non-zero winding rule**. CCW outer
/// rings minus CW holes give the filled set; disjoint islands are simply several
/// outer rings. The result type of every boolean and offset op.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub rings: Vec<Ring>,
}

/// The boolean operation to perform between two regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    Union,
    Intersection,
    /// `a − b`: the part of `a` not covered by `b`.
    Difference,
}

// 64 unit directions (CCW from +x), scaled by 2^30 = 1073741824.
const CIRCLE_SCALE: i128 = 1073741824;
#[rustfmt::skip] // a hand-laid 4-per-line lookup table; one-per-line is far less legible
const CIRCLE_DIRS: [(i64, i64); 64] = [
    (1073741824, 0), (1068571464, 105245103), (1053110176, 209476638), (1027506862, 311690799),
    (992008094, 410903207), (946955747, 506158392), (892783698, 596538995), (830013654, 681174602),
    (759250125, 759250125), (681174602, 830013654), (596538995, 892783698), (506158392, 946955747),
    (410903207, 992008094), (311690799, 1027506862), (209476638, 1053110176), (105245103, 1068571464),
    (0, 1073741824), (-105245103, 1068571464), (-209476638, 1053110176), (-311690799, 1027506862),
    (-410903207, 992008094), (-506158392, 946955747), (-596538995, 892783698), (-681174602, 830013654),
    (-759250125, 759250125), (-830013654, 681174602), (-892783698, 596538995), (-946955747, 506158392),
    (-992008094, 410903207), (-1027506862, 311690799), (-1053110176, 209476638), (-1068571464, 105245103),
    (-1073741824, 0), (-1068571464, -105245103), (-1053110176, -209476638), (-1027506862, -311690799),
    (-992008094, -410903207), (-946955747, -506158392), (-892783698, -596538995), (-830013654, -681174602),
    (-759250125, -759250125), (-681174602, -830013654), (-596538995, -892783698), (-506158392, -946955747),
    (-410903207, -992008094), (-311690799, -1027506862), (-209476638, -1053110176), (-105245103, -1068571464),
    (0, -1073741824), (105245103, -1068571464), (209476638, -1053110176), (311690799, -1027506862),
    (410903207, -992008094), (506158392, -946955747), (596538995, -892783698), (681174602, -830013654),
    (759250125, -759250125), (830013654, -681174602), (892783698, -596538995), (946955747, -506158392),
    (992008094, -410903207), (1027506862, -311690799), (1053110176, -209476638), (1068571464, -105245103),
];

/// Default arc resolution: full table (64-gon) per circle. Chord error for a radius
/// `R` is `R·(1−cos(π/64)) ≈ 0.0012·R` — sub-µm for pad-scale radii.
pub const DEFAULT_CIRCLE_SEGS: usize = 64;

// ----------------------------------------------------------------------------
// Exact integer predicates.
// ----------------------------------------------------------------------------

/// `(a→b) × (a→p)`: twice the signed area of triangle (a, b, p). Sign = orientation.
fn cross(a: Point, b: Point, p: Point) -> i128 {
    (b.x - a.x) as i128 * (p.y - a.y) as i128 - (b.y - a.y) as i128 * (p.x - a.x) as i128
}

/// 2D cross product of vectors `u` and `v` (as displacement points from origin).
fn cross_vec(ux: i128, uy: i128, vx: i128, vy: i128) -> i128 {
    ux * vy - uy * vx
}

fn dist2(a: Point, b: Point) -> i128 {
    let (dx, dy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    dx * dx + dy * dy
}

/// Is collinear point `p` within segment a–b's bounding box (⇒ on the segment)?
fn on_seg_bbox(a: Point, b: Point, p: Point) -> bool {
    p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
}

/// Is `p` exactly on segment a–b (collinear and within the box)?
fn point_on_seg(a: Point, b: Point, p: Point) -> bool {
    cross(a, b, p) == 0 && on_seg_bbox(a, b, p)
}

/// Round `num/den` to the nearest integer (half away from zero); `den > 0`.
fn round_div(num: i128, den: i128) -> i128 {
    debug_assert!(den > 0);
    if num >= 0 {
        (num + den / 2) / den
    } else {
        -((-num + den / 2) / den)
    }
}

/// Squared distance from point `p` to segment `a`–`b`, as `(num, den)` with
/// `dist² = num/den`, `den > 0` (exact i128). Mirrors the geom kernel; kept local so
/// `region` stays self-contained.
fn pt_seg_d2(p: Point, a: Point, b: Point) -> (i128, i128) {
    // Worst i128 chain (`|w|²·den ≤ 64·C⁴`) — the true ceiling is `KERNEL_SAFE_COORD`
    // (composition-frame, above the `MAX_COORD` ingest bound).
    debug_assert!(
        [p, a, b].iter().all(|&q| crate::geom::point_kernel_safe(q)),
        "region::pt_seg_d2 coordinate exceeds KERNEL_SAFE_COORD; i128 product may overflow (issue 0018)"
    );
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy;
    if den == 0 {
        return (wx * wx + wy * wy, 1);
    }
    let t = wx * vx + wy * vy;
    if t <= 0 {
        (wx * wx + wy * wy, 1)
    } else if t >= den {
        let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
        (ux * ux + uy * uy, 1)
    } else {
        // |w|² − t²/den = (|w|²·den − t²)/den. Exact in i128 at board scale (±1e9 nm:
        // |w|²·den ≲ 4e36 ≪ i128::MAX).
        let ww = wx * wx + wy * wy;
        (ww * den - t * t, den)
    }
}

/// Is `dist²(p, seg a–b) < thr2`? Compared as `num < thr2·den` (no cross-multiplying
/// two large numerators — that would overflow i128 at board scale).
fn pt_seg_lt(p: Point, a: Point, b: Point, thr2: i128) -> bool {
    let (num, den) = pt_seg_d2(p, a, b);
    num < thr2 * den
}

/// Do segments `a1a2` and `b1b2` intersect (touch / collinear-overlap counts)?
fn segs_intersect(a1: Point, a2: Point, b1: Point, b2: Point) -> bool {
    let d1 = cross(b1, b2, a1).signum();
    let d2 = cross(b1, b2, a2).signum();
    let d3 = cross(a1, a2, b1).signum();
    let d4 = cross(a1, a2, b2).signum();
    if d1 != d2 && d3 != d4 {
        return true;
    }
    (d1 == 0 && on_seg_bbox(b1, b2, a1))
        || (d2 == 0 && on_seg_bbox(b1, b2, a2))
        || (d3 == 0 && on_seg_bbox(a1, a2, b1))
        || (d4 == 0 && on_seg_bbox(a1, a2, b2))
}

/// Is the minimum distance between segments `a1a2` and `b1b2` `< thr` (thr² = `thr2`)?
/// Intersection ⇒ distance 0. Else the min is at one of the four endpoint-to-opposite
/// distances.
fn seg_seg_within2(a1: Point, a2: Point, b1: Point, b2: Point, thr2: i128) -> bool {
    if segs_intersect(a1, a2, b1, b2) {
        return true;
    }
    pt_seg_lt(a1, b1, b2, thr2)
        || pt_seg_lt(a2, b1, b2, thr2)
        || pt_seg_lt(b1, a1, a2, thr2)
        || pt_seg_lt(b2, a1, a2, thr2)
}

// ----------------------------------------------------------------------------
// Region queries.
// ----------------------------------------------------------------------------

/// Where a point sits relative to a set of rings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Loc {
    Inside,
    Boundary,
    Outside,
}

/// Iterate a ring's directed edges (closing edge included).
fn ring_edges(ring: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    let n = ring.len();
    (0..n).map(move |i| (ring[i], ring[(i + 1) % n]))
}

/// Non-zero winding number of `p` w.r.t. all rings (exact integer).
fn winding(p: Point, rings: &[Ring]) -> i32 {
    let mut wn = 0i32;
    for ring in rings {
        for (a, b) in ring_edges(ring) {
            if a.y <= p.y {
                if b.y > p.y && cross(a, b, p) > 0 {
                    wn += 1;
                }
            } else if b.y <= p.y && cross(a, b, p) < 0 {
                wn -= 1;
            }
        }
    }
    wn
}

/// Locate `p`: on any ring's boundary ⇒ `Boundary`; else by winding.
fn locate(p: Point, rings: &[Ring]) -> Loc {
    for ring in rings {
        for (a, b) in ring_edges(ring) {
            if point_on_seg(a, b, p) {
                return Loc::Boundary;
            }
        }
    }
    if winding(p, rings) != 0 {
        Loc::Inside
    } else {
        Loc::Outside
    }
}

/// Twice the signed area of a ring (shoelace). CCW > 0, CW < 0. Public so orientation-
/// sensitive callers (e.g. re-signing an `Area`'s rings after a reflecting transform)
/// share the one exact computation.
pub fn signed_area2(ring: &[Point]) -> i128 {
    let n = ring.len();
    if n < 3 {
        return 0;
    }
    let mut s = 0i128;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        s += a.x as i128 * b.y as i128 - b.x as i128 * a.y as i128;
    }
    s
}

/// Reorder a ring to CCW (positive signed area) if needed.
fn ensure_ccw(mut ring: Ring) -> Ring {
    if signed_area2(&ring) < 0 {
        ring.reverse();
    }
    ring
}

impl Region {
    pub fn new(rings: Vec<Ring>) -> Region {
        Region { rings }
    }

    /// A region from a single CCW outer ring.
    pub fn from_ring(ring: Ring) -> Region {
        Region {
            rings: vec![ensure_ccw(ring)],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rings.iter().all(|r| r.len() < 3)
    }

    /// Is `p` inside the filled region (boundary counts as inside)?
    pub fn contains_point(&self, p: Point) -> bool {
        locate(p, &self.rings) != Loc::Outside
    }

    /// Total signed area ×2 (CCW outer minus CW holes). The filled area is
    /// `area2().abs() / 2`; for a well-formed region the sign is positive.
    pub fn area2(&self) -> i128 {
        self.rings.iter().map(|r| signed_area2(r)).sum()
    }

    /// Axis-aligned bounding box `(min, max)` over all vertices, or `None` if empty.
    pub fn bbox(&self) -> Option<(Point, Point)> {
        let mut pts = self.rings.iter().flatten().copied();
        let first = pts.next()?;
        let (mut min, mut max) = (first, first);
        for p in pts {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        Some((min, max))
    }

    /// Decompose into connected filled components. After a clean boolean each
    /// positive-area (CCW) ring is one disjoint island; each hole (CW ring) is
    /// attached to the island whose outer ring contains it. A pour split into pieces
    /// by its knockouts yields several islands here — the basis for honest "this pad
    /// reaches a copper island that doesn't connect to the rest" reporting.
    pub fn islands(&self) -> Vec<Region> {
        let mut islands: Vec<Region> = Vec::new();
        let mut holes: Vec<Ring> = Vec::new();
        for r in &self.rings {
            match signed_area2(r).cmp(&0) {
                std::cmp::Ordering::Greater => islands.push(Region {
                    rings: vec![r.clone()],
                }),
                std::cmp::Ordering::Less => holes.push(r.clone()),
                std::cmp::Ordering::Equal => {} // degenerate (collinear / <3 verts): drop.
            }
        }
        for h in holes {
            // Attach to the island whose outer ring contains the hole. Test *every*
            // hole vertex (not just the first): a hole vertex can land exactly on the
            // outer boundary — where `winding` is 0 — so a first-vertex-only test could
            // miss the containing island and silently fill the knockout solid.
            if let Some(isl) = islands
                .iter_mut()
                .find(|isl| h.iter().any(|&v| winding(v, &isl.rings[0..1]) != 0))
            {
                isl.rings.push(h);
            }
        }
        islands
    }

    /// The hole rings (CW, negative signed area) as a `Region` — the board region's
    /// cutouts, a pour's knockouts. Each hole becomes its own outer (CCW) ring here, so
    /// the result is the filled set of the negative space. Empty when there are no holes.
    pub fn holes(&self) -> Region {
        Region {
            rings: self
                .rings
                .iter()
                .filter(|r| signed_area2(r) < 0)
                .map(|r| ensure_ccw(r.clone()))
                .collect(),
        }
    }
}

// ----------------------------------------------------------------------------
// Edge-pair split points (shared, rounded once).
// ----------------------------------------------------------------------------

/// Where edges `a=(a1,a2)` and `b=(b1,b2)` force a subdivision, each as
/// `(point, splits_a, splits_b)`. Two cases:
///   - **Proper crossing / T-junction** (lines not parallel): the intersection is
///     computed in `i128`, **rounded to nm once**, and returned to *both* edges — so
///     they share the exact same vertex (no crack). Whether it splits each edge is
///     decided by the exact intersection **parameter** being strictly interior
///     (`0 < tn < d`), *not* by re-testing the rounded point for collinearity — a
///     generic crossing rounds to a lattice point that lies on neither integer
///     segment, so a collinearity re-test would wrongly reject every off-lattice
///     crossing.
///   - **Collinear overlap**: each endpoint of one edge splits the *other* edge where
///     it lands strictly interior to it (these are exact lattice points, so the
///     `point_on_seg` test is exact here).
fn crossings(a1: Point, a2: Point, b1: Point, b2: Point) -> Vec<(Point, bool, bool)> {
    // Intersection numerator is ~`C³` (`tn·dax`, `tn ≤ 8C²`), safe under `KERNEL_SAFE_COORD`.
    debug_assert!(
        [a1, a2, b1, b2]
            .iter()
            .all(|&p| crate::geom::point_kernel_safe(p)),
        "region::crossings coordinate exceeds KERNEL_SAFE_COORD (issue 0018)"
    );
    let (dax, day) = ((a2.x - a1.x) as i128, (a2.y - a1.y) as i128);
    let (dbx, dby) = ((b2.x - b1.x) as i128, (b2.y - b1.y) as i128);
    let den = cross_vec(dax, day, dbx, dby);
    if den != 0 {
        let (wx, wy) = ((b1.x - a1.x) as i128, (b1.y - a1.y) as i128);
        let mut tn = cross_vec(wx, wy, dbx, dby);
        let mut sn = cross_vec(wx, wy, dax, day);
        let mut d = den;
        if d < 0 {
            tn = -tn;
            sn = -sn;
            d = -d;
        }
        if (0..=d).contains(&tn) && (0..=d).contains(&sn) {
            let x = a1.x as i128 + round_div(tn * dax, d);
            let y = a1.y as i128 + round_div(tn * day, d);
            let p = Point {
                x: x as Nm,
                y: y as Nm,
            };
            // Split an edge iff the crossing is strictly interior to it (parameter
            // in (0,d)); an endpoint touch (tn==0 || tn==d) is not a split.
            return vec![(p, tn > 0 && tn < d, sn > 0 && sn < d)];
        }
        return vec![];
    }
    // Parallel; only collinear overlaps subdivide.
    if cross(a1, a2, b1) != 0 {
        return vec![];
    }
    let mut out = Vec::new();
    for &p in &[a1, a2] {
        if p != b1 && p != b2 && point_on_seg(b1, b2, p) {
            out.push((p, false, true)); // an a-endpoint interior to b ⇒ splits b.
        }
    }
    for &p in &[b1, b2] {
        if p != a1 && p != a2 && point_on_seg(a1, a2, p) {
            out.push((p, true, false)); // a b-endpoint interior to a ⇒ splits a.
        }
    }
    out
}

/// Subdivide one edge `(a1,a2)` at the given split points: order them along the edge
/// (by squared distance from `a1`, monotonic), drop the endpoints and duplicates, and
/// emit fragments. Split points come from [`crossings`], which already decided
/// interiority — so no on-segment re-test here (the snapped crossing may sit ~1 nm off
/// the integer line, and re-testing would wrongly discard it). Zero-length fragments
/// are skipped.
fn subdivide(a1: Point, a2: Point, mut splits: Vec<Point>) -> Vec<Edge> {
    splits.retain(|&p| p != a1 && p != a2);
    splits.sort_by_key(|&p| dist2(a1, p));
    splits.dedup();
    let mut out = Vec::new();
    let mut prev = a1;
    for p in splits {
        if p != prev {
            out.push((prev, p));
        }
        prev = p;
    }
    if a2 != prev {
        out.push((prev, a2));
    }
    out
}

// ----------------------------------------------------------------------------
// Boolean engine.
// ----------------------------------------------------------------------------

/// Subdivide both regions' edges at their mutual crossings, returning the fragment
/// lists `(a_frags, b_frags)`. Each region's self-edges are assumed non-crossing
/// (simple rings); only a↔b crossings subdivide.
fn subdivide_pair(a: &Region, b: &Region) -> (Vec<Edge>, Vec<Edge>) {
    let a_edges: Vec<(Point, Point)> = a.rings.iter().flat_map(|r| ring_edges(r)).collect();
    let b_edges: Vec<(Point, Point)> = b.rings.iter().flat_map(|r| ring_edges(r)).collect();
    let mut a_splits: Vec<Vec<Point>> = vec![Vec::new(); a_edges.len()];
    let mut b_splits: Vec<Vec<Point>> = vec![Vec::new(); b_edges.len()];

    for (i, &(a1, a2)) in a_edges.iter().enumerate() {
        for (j, &(b1, b2)) in b_edges.iter().enumerate() {
            for (p, split_a, split_b) in crossings(a1, a2, b1, b2) {
                if split_a {
                    a_splits[i].push(p);
                }
                if split_b {
                    b_splits[j].push(p);
                }
            }
        }
    }

    let a_frags = a_edges
        .iter()
        .enumerate()
        .flat_map(|(i, &(a1, a2))| subdivide(a1, a2, std::mem::take(&mut a_splits[i])))
        .collect();
    let b_frags = b_edges
        .iter()
        .enumerate()
        .flat_map(|(j, &(b1, b2))| subdivide(b1, b2, std::mem::take(&mut b_splits[j])))
        .collect();
    (a_frags, b_frags)
}

/// A point a fraction `num/den` along `a→b`, floored to nm. Used only to pick an
/// interior sample for inside/outside classification.
fn lerp_floor(a: Point, b: Point, num: i64, den: i64) -> Point {
    Point {
        x: a.x + (b.x - a.x) * num / den,
        y: a.y + (b.y - a.y) * num / den,
    }
}

/// Is a fragment strictly inside or outside the *other* region? Coincidence (the
/// fragment running along the other's boundary) is decided separately by exact
/// endpoint-key match, so here a `Boundary` sample is only a rounding artifact of a
/// non-coincident fragment whose floored sample happened to land on an edge — retry at
/// other interior fractions before giving up. Returns `Inside`/`Outside` (never
/// `Boundary`); the safe default for an all-degenerate sliver is `Outside`.
fn side_of(frag: Edge, other: &Region) -> Loc {
    for &(num, den) in &[(1, 2), (1, 3), (2, 3), (1, 4), (3, 4)] {
        match locate(lerp_floor(frag.0, frag.1, num, den), &other.rings) {
            Loc::Boundary => continue,
            l => return l,
        }
    }
    Loc::Outside
}

/// Unordered endpoint key for matching coincident fragments between the two inputs.
/// After subdivision, two fragments are collinear-coincident **iff** they share this
/// key (two straight segments with the same endpoints are the same segment).
fn edge_key(e: Edge) -> Edge {
    if (e.0.x, e.0.y) <= (e.1.x, e.1.y) {
        (e.0, e.1)
    } else {
        (e.1, e.0)
    }
}

/// The boolean of two regions. See the module docs for the method.
pub fn boolean(a: &Region, b: &Region, op: BoolOp) -> Region {
    let (a_frags, b_frags) = subdivide_pair(a, b);

    // Coincident edges are matched by *exact endpoint key*, not a midpoint test — so
    // diagonal shared edges are handled as reliably as axis-aligned ones. Map each
    // b-fragment's key to its directed form (for the same/opposite-direction rule).
    use std::collections::{BTreeMap, BTreeSet};
    let mut b_by_key: BTreeMap<Edge, Edge> = BTreeMap::new();
    for &bf in &b_frags {
        b_by_key.entry(edge_key(bf)).or_insert(bf);
    }
    let a_keys: BTreeSet<Edge> = a_frags.iter().map(|&af| edge_key(af)).collect();

    let mut kept: Vec<Edge> = Vec::new();

    // a-fragments.
    for &af in &a_frags {
        if let Some(&bf) = b_by_key.get(&edge_key(af)) {
            // Coincident with a b-edge. Same direction ⇒ interiors on the same side
            // (shared outer boundary, keep once for ∪/∩, drop for −); opposite ⇒
            // interiors on opposite sides (interior to ∪/∩ ⇒ drop; a real boundary of
            // a−b ⇒ keep a's copy).
            let same_dir = bf.0 == af.0;
            let keep = match op {
                BoolOp::Union | BoolOp::Intersection => same_dir,
                BoolOp::Difference => !same_dir,
            };
            if keep {
                kept.push(af);
            }
            continue;
        }
        match side_of(af, b) {
            Loc::Inside if op == BoolOp::Intersection => kept.push(af),
            Loc::Outside if op != BoolOp::Intersection => kept.push(af),
            _ => {}
        }
    }

    // b-fragments; coincident ones were resolved from a's side, so skip them.
    for &bf in &b_frags {
        if a_keys.contains(&edge_key(bf)) {
            continue;
        }
        match side_of(bf, a) {
            Loc::Inside => match op {
                BoolOp::Intersection => kept.push(bf),
                BoolOp::Difference => kept.push((bf.1, bf.0)), // reversed ⇒ a hole.
                BoolOp::Union => {}
            },
            Loc::Outside if op == BoolOp::Union => kept.push(bf),
            _ => {}
        }
    }

    Region {
        rings: stitch(kept),
    }
}

/// Convenience wrappers.
pub fn union(a: &Region, b: &Region) -> Region {
    boolean(a, b, BoolOp::Union)
}
pub fn intersection(a: &Region, b: &Region) -> Region {
    boolean(a, b, BoolOp::Intersection)
}
pub fn difference(a: &Region, b: &Region) -> Region {
    boolean(a, b, BoolOp::Difference)
}

/// Do regions `a` and `b` come within `thr` of each other edge-to-edge, or overlap?
/// `thr ≥ 0`; overlap (one's filled area containing a vertex of the other, or their
/// boundaries crossing) always returns true. This is the copper-incidence test:
/// clearance uses `thr = min_clearance` (a pour-vs-pour short), connectivity uses a
/// small touch tolerance.
pub fn regions_within(a: &Region, b: &Region, thr: Nm) -> bool {
    // Filled-area overlap: a vertex of one lies inside the other.
    if a.rings.iter().flatten().any(|&p| b.contains_point(p))
        || b.rings.iter().flatten().any(|&p| a.contains_point(p))
    {
        return true;
    }
    let thr2 = (thr as i128) * (thr as i128);
    for ea in a.rings.iter().flat_map(|r| ring_edges(r)) {
        for eb in b.rings.iter().flat_map(|r| ring_edges(r)) {
            if seg_seg_within2(ea.0, ea.1, eb.0, eb.1, thr2) {
                return true;
            }
        }
    }
    false
}

/// Union a list of regions (left fold). Empty ⇒ empty region.
pub fn union_all(mut regions: Vec<Region>) -> Region {
    if regions.is_empty() {
        return Region::default();
    }
    let mut acc = regions.remove(0);
    for r in regions {
        acc = union(&acc, &r);
    }
    acc
}

// ----------------------------------------------------------------------------
// Stitching kept directed edges into rings.
// ----------------------------------------------------------------------------

/// Reassemble directed edges into closed rings. At a vertex with several outgoing
/// edges (a pinch point), take the most counter-clockwise turn relative to the
/// incoming direction — the standard "keep the interior on the left" face-tracing
/// rule, which separates an outer boundary from the holes it encloses.
fn stitch(edges: Vec<(Point, Point)>) -> Vec<Ring> {
    use std::collections::BTreeMap;
    let mut out_edges: BTreeMap<Point, Vec<usize>> = BTreeMap::new();
    for (i, e) in edges.iter().enumerate() {
        out_edges.entry(e.0).or_default().push(i);
    }
    let mut used = vec![false; edges.len()];
    let mut rings: Vec<Ring> = Vec::new();

    for start in 0..edges.len() {
        if used[start] {
            continue;
        }
        let mut ring: Ring = Vec::new();
        let mut cur = start;
        loop {
            used[cur] = true;
            let (from, to) = edges[cur];
            ring.push(from);
            // Candidate next edges leave `to` and are unused.
            let cands: Vec<usize> = out_edges
                .get(&to)
                .map(|v| v.iter().copied().filter(|&k| !used[k]).collect())
                .unwrap_or_default();
            // The ring closes when `to` has no unused outgoing edge — including the
            // start edge, which is already marked used, so `next` is never `start`.
            let next = match cands.len() {
                0 => break,
                1 => cands[0],
                _ => pick_most_ccw(from, to, &cands, &edges),
            };
            cur = next;
        }
        if ring.len() >= 3 {
            rings.push(ring);
        }
    }
    rings
}

/// Among candidate edges leaving `to`, pick the one whose direction makes the most
/// counter-clockwise turn relative to the incoming direction `to−from`.
fn pick_most_ccw(from: Point, to: Point, cands: &[usize], edges: &[(Point, Point)]) -> usize {
    let inx = (to.x - from.x) as i128;
    let iny = (to.y - from.y) as i128;
    let mut best = cands[0];
    let mut best_dir = (
        (edges[best].1.x - to.x) as i128,
        (edges[best].1.y - to.y) as i128,
    );
    for &k in &cands[1..] {
        let d = ((edges[k].1.x - to.x) as i128, (edges[k].1.y - to.y) as i128);
        if more_ccw(inx, iny, d, best_dir) {
            best = k;
            best_dir = d;
        }
    }
    best
}

/// Is direction `cand` a more counter-clockwise turn from incoming `(inx,iny)` than
/// `cur`? Turning angle increases CCW; we compare by half-plane then cross product to
/// order directions around the incoming direction without trig.
fn more_ccw(inx: i128, iny: i128, cand: (i128, i128), cur: (i128, i128)) -> bool {
    // Order candidates by their angle measured CCW starting just past the incoming
    // direction. `turn_rank` gives a monotonic key; larger ⇒ more CCW.
    turn_rank(inx, iny, cand) > turn_rank(inx, iny, cur)
}

/// A rank for direction `d` by CCW angle from incoming `(inx,iny)`, in [0, 4): uses
/// the (half-plane, cross-sign) lexicographic order, no trig. Pure comparison aid.
fn turn_rank(inx: i128, iny: i128, d: (i128, i128)) -> (i32, i128) {
    // Reference direction is the reverse of incoming (where we came from). Sweep CCW.
    let (rx, ry) = (-inx, -iny);
    let crossv = cross_vec(rx, ry, d.0, d.1);
    let dot = rx * d.0 + ry * d.1;
    // Sector by sign so the secondary key (cross) is monotonic within it.
    let sector = if crossv > 0 {
        0 // left half, CCW-near
    } else if crossv < 0 {
        2 // right half
    } else if dot > 0 {
        3 // straight back (same as reference) — sweep last
    } else {
        1 // straight ahead
    };
    (sector, crossv)
}

// ----------------------------------------------------------------------------
// Shape2D → Region (the dilation decomposition; offset = radius bump).
// ----------------------------------------------------------------------------

/// A tessellated disc of `radius` centred at `c`, CCW, using `segs` (≤ 64) directions.
fn disc_ring(c: Point, radius: Nm, segs: usize) -> Ring {
    let segs = segs.clamp(3, 64);
    let step = 64 / segs.max(1);
    let mut ring = Ring::with_capacity(segs);
    let r = radius as i128;
    let mut i = 0;
    while i < 64 {
        let (dx, dy) = CIRCLE_DIRS[i];
        let x = c.x as i128 + round_div(dx as i128 * r, CIRCLE_SCALE);
        let y = c.y as i128 + round_div(dy as i128 * r, CIRCLE_SCALE);
        ring.push(Point {
            x: x as Nm,
            y: y as Nm,
        });
        i += step;
    }
    ring
}

/// The rectangle that is segment a–b inflated by `r` on each side (a "fat segment"),
/// CCW. The perpendicular uses IEEE `sqrt` (correctly-rounded ⇒ deterministic);
/// result vertices are rounded to nm.
fn segment_rect(a: Point, b: Point, r: Nm) -> Option<Ring> {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return None;
    }
    // Left-perpendicular unit × r, rounded to nm.
    let px = (-dy / len * r as f64).round() as Nm;
    let py = (dx / len * r as f64).round() as Nm;
    let off = |p: Point, sx: Nm, sy: Nm| Point {
        x: p.x + sx,
        y: p.y + sy,
    };
    // CCW: a+perp, b+perp, b−perp, a−perp.
    Some(vec![
        off(a, px, py),
        off(b, px, py),
        off(b, -px, -py),
        off(a, -px, -py),
    ])
}

/// Realise a [`Shape2D`] (a skeleton ⊕ `radius`) as a filled [`Region`], tessellating
/// the radius with `circle_segs` per circle. This is the project's **offset**: to
/// inflate a shape by a clearance `c`, build it (or a copy) with `radius + c` and call
/// this. The result is the dilation = union of the core area (polygons), one rectangle
/// per skeleton edge, and one disc per skeleton vertex.
pub fn shape_to_region(shape: &Shape2D, circle_segs: usize) -> Region {
    // An `Area` is already a realised region — return it directly (no skeleton to dilate).
    if let Some(region) = shape.region() {
        return region.clone();
    }
    let radius = shape.radius();
    // Flatten the skeleton (arcs → chord polyline) up front: the dilation operates on
    // straight edges + vertex discs, so an arc edge becomes a fan of fat segments —
    // strategy A, the exact-integer boolean below never sees a curve.
    let mut pts = shape.path().flatten(crate::geom::DEFAULT_CHORD_TOL);
    match shape {
        Shape2D::Polygon { .. } => {
            // Drop a trailing point coincident with the start (an arc that explicitly
            // closes the ring), so the implicit closing edge isn't zero-length.
            if pts.len() >= 2 && pts.first() == pts.last() {
                pts.pop();
            }
            if pts.len() < 3 {
                return Region::default();
            }
            let core = ensure_ccw(pts.clone());
            if radius <= 0 {
                return Region::from_ring(core);
            }
            let mut pieces = vec![Region::from_ring(core)];
            for (a, b) in ring_edges(&pts) {
                if let Some(rect) = segment_rect(a, b, radius) {
                    pieces.push(Region::from_ring(rect));
                }
            }
            for &p in &pts {
                pieces.push(Region::from_ring(disc_ring(p, radius, circle_segs)));
            }
            union_all(pieces)
        }
        Shape2D::Stroke { .. } => {
            if radius <= 0 || pts.is_empty() {
                return Region::default();
            }
            if pts.len() == 1 {
                return Region::from_ring(disc_ring(pts[0], radius, circle_segs));
            }
            let mut pieces = Vec::new();
            for w in pts.windows(2) {
                if let Some(rect) = segment_rect(w[0], w[1], radius) {
                    pieces.push(Region::from_ring(rect));
                }
            }
            for &p in &pts {
                pieces.push(Region::from_ring(disc_ring(p, radius, circle_segs)));
            }
            union_all(pieces)
        }
        // Handled by the early return above (an `Area` is already a region).
        Shape2D::Area { .. } => unreachable!(),
    }
}

/// Dilate a filled [`Region`] by `d > 0`: its exact Minkowski sum with a disc of radius
/// `d`, tessellating the disc with `circle_segs` directions. This is the **same offset
/// decomposition** [`shape_to_region`] applies to a `Shape2D` skeleton (core area ∪ one
/// fat rectangle per boundary edge ∪ one disc per vertex, all unioned), generalized from
/// a single skeleton to a region's rings — so a hole shrinks and an island grows by `d`,
/// exactly as `solid ⊕ disc` demands. Reuses the one boolean engine ([`union_all`]); no
/// new offsetter. `d == 0` is identity; **`d < 0` (erosion) is not implemented and
/// panics** — no consumer needs it (clearance offsets are always positive), and a silent
/// wrong answer is worse than a loud one.
pub fn dilate(region: &Region, d: Nm, circle_segs: usize) -> Region {
    if d == 0 {
        return region.clone();
    }
    if d < 0 {
        unimplemented!("region::dilate: negative offset (erosion) is not implemented");
    }
    let mut pieces = vec![region.clone()];
    for ring in &region.rings {
        for (a, b) in ring_edges(ring) {
            if let Some(rect) = segment_rect(a, b, d) {
                pieces.push(Region::from_ring(rect));
            }
        }
        for &p in ring {
            pieces.push(Region::from_ring(disc_ring(p, d, circle_segs)));
        }
    }
    union_all(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MM: Nm = 1_000_000;
    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }
    fn square(cx: Nm, cy: Nm, half: Nm) -> Ring {
        vec![
            pt(cx - half, cy - half),
            pt(cx + half, cy - half),
            pt(cx + half, cy + half),
            pt(cx - half, cy + half),
        ]
    }
    fn area_abs(r: &Region) -> i128 {
        r.area2().abs() / 2
    }

    #[test]
    fn signed_area_orientation() {
        let ccw = square(0, 0, MM);
        assert!(signed_area2(&ccw) > 0);
        let mut cw = ccw.clone();
        cw.reverse();
        assert!(signed_area2(&cw) < 0);
        assert_eq!(
            area_abs(&Region::from_ring(ccw)),
            4 * MM as i128 * MM as i128
        );
    }

    #[test]
    fn contains_with_hole() {
        // Outer 4mm square, CW hole 2mm square ⇒ a frame; center is in the hole.
        let outer = square(0, 0, 2 * MM);
        let mut hole = square(0, 0, MM);
        hole.reverse(); // CW hole
        let frame = Region::new(vec![outer, hole]);
        assert!(!frame.contains_point(pt(0, 0)), "center is in the hole");
        assert!(frame.contains_point(pt(3 * MM / 2, 0)), "in the frame band");
        assert!(!frame.contains_point(pt(3 * MM, 0)), "outside entirely");
    }

    #[test]
    fn union_overlapping_squares_area() {
        // Two unit squares overlapping by a 1x2 strip.
        let a = Region::from_ring(square(0, 0, MM)); // [-1,1]^2, area 4
        let b = Region::from_ring(square(MM, 0, MM)); // [0,2]x[-1,1], area 4
        let u = union(&a, &b);
        // Union spans [-1,2]x[-1,1] = 3x2 = 6 mm^2.
        assert_eq!(area_abs(&u), 6 * MM as i128 * MM as i128, "union area");
        assert!(u.contains_point(pt(15 * MM / 10, 0)));
    }

    #[test]
    fn intersection_overlapping_squares_area() {
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(MM, 0, MM));
        let i = intersection(&a, &b);
        // Overlap [0,1]x[-1,1] = 1x2 = 2 mm^2.
        assert_eq!(
            area_abs(&i),
            2 * MM as i128 * MM as i128,
            "intersection area"
        );
    }

    #[test]
    fn difference_makes_a_hole() {
        // 4mm square minus a centered 2mm square ⇒ frame, area 16-4 = 12 mm^2.
        let a = Region::from_ring(square(0, 0, 2 * MM));
        let b = Region::from_ring(square(0, 0, MM));
        let d = difference(&a, &b);
        assert_eq!(area_abs(&d), 12 * MM as i128 * MM as i128, "frame area");
        assert!(!d.contains_point(pt(0, 0)), "hole punched at center");
        assert!(d.contains_point(pt(15 * MM / 10, 0)), "band remains");
    }

    #[test]
    fn difference_partial_overlap() {
        // a=[-1,1]^2 minus b=[0,2]x[-1,1] ⇒ left half [-1,0]x[-1,1] = 2 mm^2.
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(MM, 0, MM));
        let d = difference(&a, &b);
        assert_eq!(area_abs(&d), 2 * MM as i128 * MM as i128);
        assert!(d.contains_point(pt(-MM / 2, 0)));
        assert!(!d.contains_point(pt(MM / 2, 0)));
    }

    #[test]
    fn difference_identical_is_empty() {
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(0, 0, MM));
        let d = difference(&a, &b);
        assert_eq!(area_abs(&d), 0, "A−A is empty");
    }

    #[test]
    fn union_identical_is_same_area() {
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(0, 0, MM));
        let u = union(&a, &b);
        assert_eq!(area_abs(&u), 4 * MM as i128 * MM as i128, "A∪A = A");
    }

    #[test]
    fn disjoint_union_keeps_both() {
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(10 * MM, 0, MM));
        let u = union(&a, &b);
        assert_eq!(area_abs(&u), 8 * MM as i128 * MM as i128);
        assert!(u.contains_point(pt(0, 0)) && u.contains_point(pt(10 * MM, 0)));
    }

    #[test]
    fn disc_region_area_approximates_pi_r2() {
        // A 64-gon disc of r=1mm: area ≈ π mm^2 = 3.1415e12 nm^2. 64-gon is slightly
        // under: (1/2) n sin(2π/n) r^2 ≈ 0.99949·π.
        let region = shape_to_region(&Shape2D::disc(pt(0, 0), MM), DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64;
        let pi_r2 = std::f64::consts::PI * (MM as f64) * (MM as f64);
        let ratio = a / pi_r2;
        assert!(ratio > 0.995 && ratio <= 1.0, "disc area ratio {ratio}");
    }

    #[test]
    fn rounded_rect_area_matches_geometry() {
        // round_rect(2mm,2mm,r=0.5) is a 2x2 rounded rect = a 1x1 core ⊕ 0.5mm. Its
        // area = core 1 + perimeter 4*0.5 + π*0.5^2 = 1 + 2 + π/4 ≈ 3.785 mm^2 (the
        // four rounded corners replace the four corner squares with quarter-discs).
        let rr = Shape2D::round_rect(pt(0, 0), 2 * MM, 2 * MM, MM / 2);
        let region = shape_to_region(&rr, DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
        assert!(
            (a - 3.785).abs() < 0.05,
            "rounded-rect area {a} mm^2 ~ 3.785"
        );
    }

    #[test]
    fn capsule_region_is_stadium() {
        // Pill from (0,0) to (4mm,0) r=1mm: area = rect(4x2) + circle(r1) = 8 + π.
        let cap = Shape2D::capsule(pt(0, 0), pt(4 * MM, 0), MM);
        let region = shape_to_region(&cap, DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
        let expect = 8.0 + std::f64::consts::PI;
        assert!((a - expect).abs() < 0.05, "stadium area {a} ~ {expect}");
        assert!(region.contains_point(pt(2 * MM, 0)));
    }

    #[test]
    fn dilate_grows_islands_and_shrinks_holes() {
        // A 10mm square with a 4mm square hole (walls at ±2mm). Dilating by 1mm grows the
        // outer boundary outward and moves the hole walls inward by 1mm (to ±1mm).
        let outer = shape_to_region(
            &Shape2D::rect(pt(0, 0), 10 * MM, 10 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let hole = shape_to_region(
            &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let holed = difference(&outer, &hole);
        assert_eq!(holed.holes().rings.len(), 1, "one cutout hole");

        let big = dilate(&holed, MM, DEFAULT_CIRCLE_SEGS);
        // Outer boundary grew: a point 0.5mm outside the old edge is now filled.
        assert!(
            big.contains_point(pt(55 * MM / 10, 0)),
            "outer edge grew outward"
        );
        // Hole shrank: a point 1.5mm from centre (was inside the ±2mm hole) is now filled.
        assert!(
            !holed.contains_point(pt(0, 15 * MM / 10)),
            "was in the hole"
        );
        assert!(
            big.contains_point(pt(0, 15 * MM / 10)),
            "hole wall moved inward"
        );
        // d == 0 is identity.
        assert_eq!(dilate(&holed, 0, DEFAULT_CIRCLE_SEGS), holed);
    }

    #[test]
    #[should_panic(expected = "erosion")]
    fn dilate_negative_offset_panics() {
        let sq = shape_to_region(
            &Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
            DEFAULT_CIRCLE_SEGS,
        );
        let _ = dilate(&sq, -MM, DEFAULT_CIRCLE_SEGS);
    }

    #[test]
    fn offset_via_radius_bump_inflates() {
        // The pour use case: inflate a pad by clearance = build it with radius+clr.
        // A 1mm disc inflated by 0.5mm clearance ⇒ a 1.5mm disc.
        let inflated = shape_to_region(&Shape2D::disc(pt(0, 0), MM + MM / 2), DEFAULT_CIRCLE_SEGS);
        assert!(inflated.contains_point(pt(14 * MM / 10, 0)), "within 1.5mm");
        assert!(
            !inflated.contains_point(pt(16 * MM / 10, 0)),
            "beyond 1.5mm"
        );
    }

    #[test]
    fn pour_knockout_end_to_end() {
        // The headline: a board-area pour minus the clearance around one pad. Board
        // 10x10 (area 100), pad a 1mm disc at center inflated by 0.5mm clearance
        // (area ≈ π·1.5² ≈ 7.07) ⇒ pour with a hole, area ≈ 92.9 mm^2.
        let board = Region::from_ring(square(0, 0, 5 * MM)); // 10x10
        let knock = shape_to_region(&Shape2D::disc(pt(0, 0), MM + MM / 2), DEFAULT_CIRCLE_SEGS);
        let pour = difference(&board, &knock);
        let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
        assert!((a - 92.93).abs() < 0.1, "pour area {a} ~ 92.93");
        assert!(!pour.contains_point(pt(0, 0)), "pad knocked out");
        assert!(
            pour.contains_point(pt(4 * MM, 4 * MM)),
            "copper in the corner"
        );
    }

    #[test]
    fn union_shared_edge_merges() {
        // Two squares sharing the exact edge x=1mm (A right edge ↑, B left edge ↓ ⇒
        // opposite-direction coincident ⇒ interior, dropped). Merge ⇒ 4x2 = 8 mm^2.
        let a = Region::from_ring(square(0, 0, MM)); // [-1,1]^2
        let b = Region::from_ring(square(2 * MM, 0, MM)); // [1,3]x[-1,1]
        let u = union(&a, &b);
        assert_eq!(
            area_abs(&u),
            8 * MM as i128 * MM as i128,
            "shared-edge union area"
        );
        assert_eq!(u.rings.len(), 1, "merges into one ring, no internal edge");
        assert!(
            u.contains_point(pt(MM, 0)),
            "former shared edge is now interior"
        );
    }

    #[test]
    fn union_corner_touch_keeps_both() {
        // Two squares touching only at the corner (1,1): no shared edge, both kept.
        let a = Region::from_ring(square(0, 0, MM)); // corner at (1,1)
        let b = Region::from_ring(square(2 * MM, 2 * MM, MM)); // corner at (1,1)
        let u = union(&a, &b);
        assert_eq!(area_abs(&u), 8 * MM as i128 * MM as i128);
    }

    #[test]
    fn difference_b_outside_is_a() {
        let a = Region::from_ring(square(0, 0, MM));
        let b = Region::from_ring(square(10 * MM, 0, MM));
        let d = difference(&a, &b);
        assert_eq!(
            area_abs(&d),
            4 * MM as i128 * MM as i128,
            "A−(disjoint B) = A"
        );
    }

    #[test]
    fn difference_b_contains_a_is_empty() {
        let a = Region::from_ring(square(0, 0, MM)); // small
        let b = Region::from_ring(square(0, 0, 3 * MM)); // engulfs A
        let d = difference(&a, &b);
        assert_eq!(area_abs(&d), 0, "A−(B⊇A) is empty");
    }

    #[test]
    fn concave_l_shape_dilation_is_consistent() {
        // An L (concave) polygon, area 3 mm^2, dilated by 0.25mm (radius on the
        // Polygon) via the union decomposition — the reflex vertex must not break the
        // union. Dilation area = core 3 + perimeter*r + π r^2; the L's perimeter
        // (2x2 minus a 1x1 notch) = 8mm.
        let l = Shape2D::polygon_path(
            crate::geom::Path::polyline(vec![
                pt(0, 0),
                pt(2 * MM, 0),
                pt(2 * MM, MM),
                pt(MM, MM),
                pt(MM, 2 * MM),
                pt(0, 2 * MM),
            ]),
            MM / 4,
        );
        let region = shape_to_region(&l, DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
        let expect = 3.0 + 8.0 * 0.25 + std::f64::consts::PI * 0.25 * 0.25;
        assert!((a - expect).abs() < 0.05, "L dilation area {a} ~ {expect}");
        // The reflex notch interior (just outside the core, within r) is now filled.
        assert!(region.contains_point(pt(11 * MM / 10, 11 * MM / 10)));
    }

    #[test]
    fn pour_with_two_knockouts() {
        // Closer to the real pipeline: board minus the union of two inflated pads.
        // Board 10x10 = 100 mm^2; two 1mm discs (at ±2,0) each inflated to 1.5mm
        // (area π·1.5² ≈ 7.07 each, disjoint) ⇒ pour ≈ 100 − 14.14 = 85.86 mm^2.
        let board = Region::from_ring(square(0, 0, 5 * MM));
        let pads = union_all(vec![
            shape_to_region(
                &Shape2D::disc(pt(-2 * MM, 0), MM + MM / 2),
                DEFAULT_CIRCLE_SEGS,
            ),
            shape_to_region(
                &Shape2D::disc(pt(2 * MM, 0), MM + MM / 2),
                DEFAULT_CIRCLE_SEGS,
            ),
        ]);
        let pour = difference(&board, &pads);
        let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
        assert!(
            (a - 85.86).abs() < 0.2,
            "two-knockout pour area {a} ~ 85.86"
        );
        assert!(!pour.contains_point(pt(-2 * MM, 0)) && !pour.contains_point(pt(2 * MM, 0)));
        assert!(
            pour.contains_point(pt(0, 0)),
            "copper survives between the two pads"
        );
    }

    #[test]
    fn arc_stroke_dilates_to_a_curved_tube() {
        // A semicircular trace (skeleton arc, R=2mm) of width 1mm (r=0.5mm). Its copper
        // is the r-tube around the arc: a half annulus from 1.5 to 2.5mm plus the two
        // end-cap half-discs. Area = ½π(2.5²−1.5²) + π·0.5² = 2π + 0.25π ≈ 7.07 mm².
        // This exercises the boolean closing a fan of fat segments + discs along a curve
        // (no cracks) — the integration test for arc input to the kernel. (R is kept
        // small: at 1µm tol a large arc flattens to hundreds of edges and the O(N²)
        // union dominates — a documented perf characteristic, not needed to test here.)
        let r = 2 * MM;
        let arc = Shape2D::arc(pt(-r, 0), pt(0, r), pt(r, 0), MM); // width 1mm
        let region = shape_to_region(&arc, DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
        let expect = 2.0 * std::f64::consts::PI + 0.25 * std::f64::consts::PI;
        assert!((a - expect).abs() < 0.1, "arc tube area {a} ~ {expect}");
        // A point on the arc centreline (the apex) is inside the copper; the hollow
        // centre of the semicircle (origin) is not.
        assert!(
            region.contains_point(pt(0, r)),
            "copper covers the arc apex"
        );
        assert!(
            !region.contains_point(pt(0, 0)),
            "the tube is hollow at the centre"
        );
    }

    #[test]
    fn arc_edged_polygon_is_a_filled_half_disc() {
        // A filled half-disc (D-shape): start (-2mm,0), an Arc through the apex (0,2mm)
        // to (2mm,0), closed by the implicit straight diameter. Area = ½πR² = 2π
        // ≈ 6.28 mm². Confirms a Polygon's arc edge tessellates into the core ring and
        // the implicit closing Line seals it.
        let r = 2 * MM;
        let half = Shape2D::polygon_path(
            crate::geom::Path {
                start: pt(-r, 0),
                segs: vec![crate::geom::Seg::Arc {
                    mid: pt(0, r),
                    end: pt(r, 0),
                }],
            },
            0,
        );
        let region = shape_to_region(&half, DEFAULT_CIRCLE_SEGS);
        let a = area_abs(&region) as f64 / (MM as f64 * MM as f64);
        let expect = 2.0 * std::f64::consts::PI;
        assert!((a - expect).abs() < 0.1, "half-disc area {a} ~ {expect}");
        assert!(region.contains_point(pt(0, MM)), "interior point is filled");
        assert!(
            region.contains_point(pt(0, 18 * MM / 10)),
            "point under the apex is filled"
        );
        assert!(
            !region.contains_point(pt(0, -MM / 2)),
            "below the diameter is outside"
        );
        assert!(
            !region.contains_point(pt(18 * MM / 10, 18 * MM / 10)),
            "outside the circle"
        );
    }

    #[test]
    fn nonlattice_diagonal_crossings() {
        // Regression for the snapped-crossing bug: a rect and a triangle whose slanted
        // edges cross the rect's top edge at NON-integer-nm points (x = 1.6̄ mm,
        // 2.3̄ mm), so the crossing rounds to a lattice point on neither integer line.
        // A = 4x2 rect (area 8). B = triangle (1,0),(3,0),(2,3) (area 3); B's tip above
        // y=2 is clipped, so A∩B = 3 − tip(1/3) = 8/3 ≈ 2.667 mm^2; A∪B = 25/3 ≈ 8.333.
        let a = Region::from_ring(vec![
            pt(0, 0),
            pt(4 * MM, 0),
            pt(4 * MM, 2 * MM),
            pt(0, 2 * MM),
        ]);
        let b = Region::from_ring(vec![pt(MM, 0), pt(3 * MM, 0), pt(2 * MM, 3 * MM)]);
        let inter = intersection(&a, &b);
        let uni = union(&a, &b);
        let mm2 = MM as f64 * MM as f64;
        assert!(
            (area_abs(&inter) as f64 / mm2 - 8.0 / 3.0).abs() < 0.001,
            "A∩B = 8/3"
        );
        assert!(
            (area_abs(&uni) as f64 / mm2 - 25.0 / 3.0).abs() < 0.001,
            "A∪B = 25/3"
        );
        // The crossing must split BOTH edges identically (no crack): re-running is
        // byte-identical, and a point just inside the clipped overlap is contained.
        assert_eq!(
            inter,
            intersection(&a, &b),
            "deterministic under non-lattice crossings"
        );
        assert!(inter.contains_point(pt(2 * MM, MM)), "overlap interior");
    }

    #[test]
    fn shared_diagonal_edge_merges() {
        // Regression for diagonal coincident-edge handling: two triangles sharing the
        // diagonal (0,0)-(2,2) in opposite directions merge into the 2x2 square.
        let t1 = Region::from_ring(vec![pt(0, 0), pt(2 * MM, 0), pt(2 * MM, 2 * MM)]);
        let t2 = Region::from_ring(vec![pt(0, 0), pt(2 * MM, 2 * MM), pt(0, 2 * MM)]);
        let u = union(&t1, &t2);
        assert_eq!(
            area_abs(&u),
            4 * MM as i128 * MM as i128,
            "two triangles → square"
        );
        assert_eq!(
            u.rings.len(),
            1,
            "shared diagonal becomes interior, one ring"
        );
        // Intersection of the two triangles (share only the diagonal) has no area.
        assert_eq!(
            area_abs(&intersection(&t1, &t2)),
            0,
            "triangles meet only on the diagonal"
        );
    }

    #[test]
    fn rotated_pad_pour_knockout() {
        // The real-world Bug-1 trigger: a diamond (45°-rotated square) knockout whose
        // edges cross the board boundary at non-lattice points. Board 10x10 (area 100);
        // a diamond of "radius" 3mm centered exactly on the board corner (5,5). Only the
        // one quadrant with x≤5 ∧ y≤5 lies inside the board — a triangle of area
        // (1/2)*3*3 = 4.5 mm^2 (the diamond's full area 2*3^2=18 split four ways). So
        // the pour ≈ 100 − 4.5 = 95.5 mm^2.
        let board = Region::from_ring(square(0, 0, 5 * MM)); // [-5,5]^2
        let d = 3 * MM;
        let diamond = Region::from_ring(vec![
            pt(5 * MM + d, 5 * MM),
            pt(5 * MM, 5 * MM + d),
            pt(5 * MM - d, 5 * MM),
            pt(5 * MM, 5 * MM - d),
        ]);
        let pour = difference(&board, &diamond);
        let a = area_abs(&pour) as f64 / (MM as f64 * MM as f64);
        assert!(
            (a - 95.5).abs() < 0.01,
            "rotated-diamond pour area {a} ~ 95.5"
        );
        assert!(
            !pour.contains_point(pt(4 * MM, 5 * MM)),
            "copper knocked out near the corner"
        );
    }

    #[test]
    fn boolean_is_deterministic() {
        let a = Region::from_ring(square(0, 0, 2 * MM));
        let b = shape_to_region(&Shape2D::disc(pt(MM, MM), MM), DEFAULT_CIRCLE_SEGS);
        let d1 = difference(&a, &b);
        let d2 = difference(&a, &b);
        assert_eq!(d1, d2, "same inputs ⇒ identical region");
    }
}
