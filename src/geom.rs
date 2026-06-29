//! Purposed regions: the physical-geometry foundation (see docs/architecture.md §8).
//!
//! Everything physical — copper, the board body, holes, keep-outs — is a
//! [`Feature`]: a `(role, material?, extent)`. This module is the **2.5D core**:
//! the shape vocabulary, the z-stackup, and an exact-integer clearance kernel. It is
//! deliberately *standalone and additive* — it does not yet replace `route::Pad` or
//! `route::Layer`; later stages migrate those consumers onto it.
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

/// Default board thickness: 1.6 mm, in nm.
pub const BOARD_THICKNESS: Nm = 1_600_000;
/// Default finished copper thickness: ~1 oz (35 µm), in nm.
pub const COPPER_THICKNESS: Nm = 35_000;

// ----------------------------------------------------------------------------
// Shape2D — a skeleton ⊕ radius.
// ----------------------------------------------------------------------------

/// A 2D region: a skeleton inflated by `radius` (Minkowski ⊕ a disc of that radius).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape2D {
    /// An open polyline (≥ 1 point) inflated by `radius`. One point ⇒ a disc; one
    /// segment ⇒ a capsule/oval; many points ⇒ a trace of width `2*radius`.
    Stroke { points: Vec<Point>, radius: Nm },
    /// A filled simple polygon (CCW or CW; ≥ 3 points), with corners rounded by
    /// `radius` (`0` ⇒ sharp; a rectangle with `radius` ⇒ a rounded rect).
    Polygon { points: Vec<Point>, radius: Nm },
}

impl Shape2D {
    /// A round pad / via annulus: a disc of `radius` centred at `c`.
    pub fn disc(c: Point, radius: Nm) -> Shape2D {
        Shape2D::Stroke { points: vec![c], radius }
    }
    /// A pill/oval: the `radius`-inflation of segment `a`–`b`.
    pub fn capsule(a: Point, b: Point, radius: Nm) -> Shape2D {
        Shape2D::Stroke { points: vec![a, b], radius }
    }
    /// A trace: a polyline of copper `width` wide (inflation `width/2`).
    pub fn trace(points: Vec<Point>, width: Nm) -> Shape2D {
        Shape2D::Stroke { points, radius: width / 2 }
    }
    /// An axis-aligned rectangle (sharp corners) of size `w`×`h` centred at `c`.
    pub fn rect(c: Point, w: Nm, h: Nm) -> Shape2D {
        let (hw, hh) = (w / 2, h / 2);
        Shape2D::Polygon {
            points: vec![
                Point { x: c.x - hw, y: c.y - hh },
                Point { x: c.x + hw, y: c.y - hh },
                Point { x: c.x + hw, y: c.y + hh },
                Point { x: c.x - hw, y: c.y + hh },
            ],
            radius: 0,
        }
    }
    /// An axis-aligned rounded rectangle: the core rect inset by `r`, ⊕ `r`.
    pub fn round_rect(c: Point, w: Nm, h: Nm, r: Nm) -> Shape2D {
        let (hw, hh) = (w / 2 - r, h / 2 - r);
        Shape2D::Polygon {
            points: vec![
                Point { x: c.x - hw, y: c.y - hh },
                Point { x: c.x + hw, y: c.y - hh },
                Point { x: c.x + hw, y: c.y + hh },
                Point { x: c.x - hw, y: c.y + hh },
            ],
            radius: r,
        }
    }
    /// A filled polygon from explicit points (e.g. a rotated or custom pad).
    pub fn polygon(points: Vec<Point>) -> Shape2D {
        Shape2D::Polygon { points, radius: 0 }
    }

    fn radius(&self) -> Nm {
        match self {
            Shape2D::Stroke { radius, .. } | Shape2D::Polygon { radius, .. } => *radius,
        }
    }

    /// The skeleton's segments (consecutive vertices; a polygon's boundary closes).
    /// A lone point yields one degenerate segment `(p, p)`.
    fn segments(&self) -> Vec<(Point, Point)> {
        match self {
            Shape2D::Stroke { points, .. } => {
                if points.len() == 1 {
                    vec![(points[0], points[0])]
                } else {
                    points.windows(2).map(|w| (w[0], w[1])).collect()
                }
            }
            Shape2D::Polygon { points, .. } => {
                let n = points.len();
                (0..n).map(|i| (points[i], points[(i + 1) % n])).collect()
            }
        }
    }

    fn vertices(&self) -> &[Point] {
        match self {
            Shape2D::Stroke { points, .. } | Shape2D::Polygon { points, .. } => points,
        }
    }

    /// Does this shape's *filled area* contain point `p`? Strokes have no area
    /// (their copper extent comes entirely from the radius), so always `false`.
    fn area_contains(&self, p: Point) -> bool {
        match self {
            Shape2D::Stroke { .. } => false,
            Shape2D::Polygon { points, .. } => point_in_polygon(p, points),
        }
    }
}

// ----------------------------------------------------------------------------
// Exact clearance kernel.
// ----------------------------------------------------------------------------

/// Is the edge-to-edge gap between two shapes **less than** `min_clr` (a DRC
/// clearance violation; strict `<`, matching the rest of the engine)? `min_clr` is
/// assumed `≥ 0`. Overlapping regions (gap `< 0`) always violate. For disjoint
/// regions the gap is `skeleton_distance(a, b) − radius(a) − radius(b)`, so a
/// violation is `skeleton_distance < min_clr + radius(a) + radius(b)`. All exact i128.
pub fn clearance_violated(a: &Shape2D, b: &Shape2D, min_clr: Nm) -> bool {
    // Overlapping skeletons ⇒ the inflated regions overlap ⇒ gap < 0 ≤ min_clr.
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
    if a.vertices().iter().any(|&p| b.area_contains(p))
        || b.vertices().iter().any(|&p| a.area_contains(p))
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
    let v = (b.x - a.x) as i128 * (c.y - a.y) as i128
        - (b.y - a.y) as i128 * (c.x - a.x) as i128;
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
    // On-boundary ⇒ inside.
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
        ZRange { lo: lo.min(hi), hi: lo.max(hi) }
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
        Feature { role, material: None, extent: Extent::Prism { shape, z } }
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
        assert!(!clearance_violated(&a, &b, MM / 2), "1mm gap clears 0.5mm rule");
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
        assert!(clearance_violated(&rect, &dot, 1), "a point inside the pad area is an overlap");
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
        assert!(clearance_violated(&sharp, &probe, MM / 5), "sharp corner within 0.2mm");
        assert!(!clearance_violated(&round, &probe, MM / 5), "rounded corner beyond 0.2mm");
    }

    #[test]
    fn z_overlap_gates_clearance() {
        let su = Stackup::default_2layer();
        let top = su.slab_z("F.Cu").unwrap();
        let bot = su.slab_z("B.Cu").unwrap();
        assert!(!top.overlaps(&bot), "top and bottom copper z must not overlap");
        // Two overlapping discs on opposite layers do NOT clash; same layer they do.
        let s = Shape2D::disc(pt(0, 0), MM);
        let a_top = Feature::prism(Role::Conductor, s.clone(), top);
        let b_top = Feature::prism(Role::Conductor, s.clone(), top);
        let b_bot = Feature::prism(Role::Conductor, s, bot);
        assert!(!a_top.clears(&b_top, MM), "coincident copper on the same layer clashes");
        assert!(a_top.clears(&b_bot, MM), "different layers do not clash geometrically");
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
    fn determinism_same_inputs_same_answer() {
        let a = Shape2D::round_rect(pt(0, 0), 3 * MM, 2 * MM, MM / 4);
        let b = Shape2D::trace(vec![pt(0, 5 * MM), pt(10 * MM, 5 * MM)], MM / 5);
        let r1 = clearance_violated(&a, &b, 2 * MM);
        let r2 = clearance_violated(&a, &b, 2 * MM);
        assert_eq!(r1, r2);
    }
}
