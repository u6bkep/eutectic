//! Connectivity: union-find over a net's pins + traces + vias by geometric
//! incidence. Two pins are electrically joined iff they end up in one component.
//!
//! Also home to the private integer geometry kernel (exact `i128`, squared
//! thresholds — no floating point). That kernel is duplicated against
//! `autoroute`'s `within`; a later wave dedups both into `geom`. For now it stays
//! sectioned here.

use crate::doc::{Doc, Nm, PinRef, Point};
use crate::geom::{Extent, Stackup};
use crate::part::PartLib;

use super::model::{Trace, Via};

/// A pin's world centre plus the copper slabs its pad copper actually occupies — the
/// datum layer-honest pour incidence (Decision 19c) pivots on. `slabs` is the set of
/// copper-slab **names** the pad's `Conductor` features land on: one slab for an SMD
/// pad, every copper slab for a drilled/through pad. `all_layers` is the padless
/// compatibility case (Decision 19c): a pin whose footprint carries **no** pad copper
/// (the toy library's bare terminals — real footprints always have copper) keeps the
/// old all-layer incidence, joining any same-net island it sits over regardless of slab.
#[derive(Clone, Debug)]
pub(super) struct PinPoint {
    pub(super) at: Point,
    /// Copper-slab names the pad copper occupies; empty iff `all_layers`.
    slabs: std::collections::BTreeSet<String>,
    /// True for a bare (padless) terminal — join islands on any slab.
    all_layers: bool,
}

impl PinPoint {
    /// Does this pin's copper exist on the copper slab named `layer` (so it may join a
    /// pour island there)? The padless-compatibility pin exists on every layer.
    fn on_slab(&self, layer: &str) -> bool {
        self.all_layers || self.slabs.contains(layer)
    }
}

/// The copper slabs a pin's pad occupies, as a [`PinPoint`]. Derives occupancy from the
/// *same* pad-feature lowering `world_features`/`net_features` use (a pad's `Conductor`
/// feature z matched to a copper slab) — never a parallel notion. A pin whose footprint
/// carries no pad copper (or whose component/part/pin cannot be resolved) is flagged
/// `all_layers` (Decision 19c padless compatibility).
pub(super) fn pin_point(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
    at: Point,
    pr: &PinRef,
) -> PinPoint {
    let cu = su.copper_slabs();
    let mut slabs = std::collections::BTreeSet::new();
    if let Some(c) = doc.components.get(&pr.comp)
        && let Some(def) = lib.get(&c.part)
        && let Some(pin) = def.pins.iter().find(|p| p.number == pr.pin)
    {
        for f in pin.pad_features(c, su) {
            if f.role != crate::geom::Role::Conductor {
                continue;
            }
            let Extent::Prism { z, .. } = &f.extent;
            if let Some(s) = cu.iter().find(|s| s.z == *z) {
                slabs.insert(s.name.clone());
            }
        }
    }
    let all_layers = slabs.is_empty();
    PinPoint {
        at,
        slabs,
        all_layers,
    }
}

struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Number of connected components among a net's *pins*, joining them through the
/// net's copper. Nodes: pins, then traces, then vias, then **pour islands**. Incidence
/// (within `tol`): pin↔pin (coincident pads), pin↔trace and pin↔via (pads are
/// all-layer points), trace↔trace (same layer), trace↔via and via↔via (via must span
/// the layer), and copper↔island (a pin/trace/via landing on a filled pour island
/// joins everything else on that island). Pin↔island incidence is **layer-honest**
/// (Decision 19c): a pin joins a plane only where its pad copper exists on that plane's
/// slab (a [`PinPoint`] carries the occupancy), so an SMD pad does not falsely connect to
/// an inner plane it merely overlaps. Distinct islands are *not* joined to each other, so
/// a pour fragmented by its knockouts leaves pads on different islands in separate
/// components — reported honestly as remaining ratsnest islands.
pub(super) fn pin_islands(
    pins: &[PinPoint],
    traces: &[&Trace],
    vias: &[&Via],
    pour_islands: &[(String, crate::region::Region)],
    su: &Stackup,
    tol: Nm,
) -> usize {
    let cu = su.copper_slabs();
    // A via's spanned copper-slab index range (from `cu`, top-down), for the span-overlap
    // tests. An unresolvable named span occupies nothing.
    let via_span = |v: &Via| -> Option<(usize, usize)> {
        match &v.span {
            None => (!cu.is_empty()).then(|| (0, cu.len() - 1)),
            Some((from, to)) => {
                let a = cu.iter().position(|s| s.name == *from)?;
                let b = cu.iter().position(|s| s.name == *to)?;
                Some((a.min(b), a.max(b)))
            }
        }
    };
    // Does a via span the copper slab named `name`?
    let via_spans_name = |v: &Via, name: &str| -> bool {
        let (Some((lo, hi)), Some(idx)) = (via_span(v), cu.iter().position(|s| s.name == name))
        else {
            return false;
        };
        lo <= idx && idx <= hi
    };
    let (np, nt, nv) = (pins.len(), traces.len(), vias.len());
    let mut uf = UnionFind::new(np + nt + nv + pour_islands.len());
    let trace_node = |i: usize| np + i;
    let via_node = |i: usize| np + nt + i;
    let island_node = |i: usize| np + nt + nv + i;

    // pin ↔ pin
    for i in 0..np {
        for j in (i + 1)..np {
            if seg_within(pins[i].at, pins[i].at, pins[j].at, pins[j].at, tol, false) {
                uf.union(i, j);
            }
        }
    }
    // pin ↔ trace, pin ↔ via
    for (pi, p) in pins.iter().enumerate() {
        for (ti, t) in traces.iter().enumerate() {
            if point_on_polyline(p.at, &t.path, tol) {
                uf.union(pi, trace_node(ti));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if seg_within(p.at, p.at, v.at, v.at, tol, false) {
                uf.union(pi, via_node(vi));
            }
        }
    }
    // trace ↔ trace (same layer)
    for i in 0..nt {
        for j in (i + 1)..nt {
            if traces[i].layer == traces[j].layer
                && polylines_closer_than_inc(&traces[i].path, &traces[j].path, tol)
            {
                uf.union(trace_node(i), trace_node(j));
            }
        }
    }
    // trace ↔ via (via spans the trace's layer)
    for (ti, t) in traces.iter().enumerate() {
        for (vi, v) in vias.iter().enumerate() {
            if via_spans_name(v, &t.layer) && point_on_polyline(v.at, &t.path, tol) {
                uf.union(trace_node(ti), via_node(vi));
            }
        }
    }
    // via ↔ via (coincident, spans overlap)
    for i in 0..nv {
        for j in (i + 1)..nv {
            let (u, w) = (vias[i], vias[j]);
            let overlap = match (via_span(u), via_span(w)) {
                (Some((ulo, uhi)), Some((wlo, whi))) => ulo <= whi && wlo <= uhi,
                _ => false,
            };
            if overlap && seg_within(u.at, u.at, w.at, w.at, tol, false) {
                uf.union(via_node(i), via_node(j));
            }
        }
    }
    // copper ↔ pour island: a pad/trace/via whose copper lands on a filled island is
    // electrically that island. A pin joins an island only where its pad copper actually
    // exists on that island's slab (Decision 19c, PoC finding F1): an SMD pad on F.Cu is
    // NOT connected to an inner-layer plane it merely overlaps in XY — that would report
    // connectivity with zero stitching vias, the one direction this model never lies. A
    // drilled/through pad, whose barrel spans every copper slab, joins an island on any
    // slab it sits over. The padless-compatibility pin (bare toy-library terminal, no pad
    // copper) keeps all-layer incidence — see [`PinPoint`]. Traces and vias ARE
    // layer-specific, so they join an island only on the pour's own layer (a trace/via
    // overlapping the pour in XY but on another layer reaches it only through a via).
    for (ii, (layer, isl)) in pour_islands.iter().enumerate() {
        for (pi, p) in pins.iter().enumerate() {
            if p.on_slab(layer) && isl.contains_point(p.at) {
                uf.union(pi, island_node(ii));
            }
        }
        for (ti, t) in traces.iter().enumerate() {
            if t.layer == *layer && t.path.iter().any(|p| isl.contains_point(*p)) {
                uf.union(trace_node(ti), island_node(ii));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if via_spans_name(v, layer) && isl.contains_point(v.at) {
                uf.union(via_node(vi), island_node(ii));
            }
        }
    }

    let mut roots = std::collections::BTreeSet::new();
    for i in 0..np {
        let r = uf.find(i);
        roots.insert(r);
    }
    roots.len()
}

// ----------------------------------------------------------------------------
// Integer geometry. All comparisons are exact (i128, squared thresholds); nothing
// here uses floating point.
//
// NOTE: this kernel is duplicated against `autoroute`'s `within`; a later wave
// dedups both into `geom`. It is sectioned here unchanged for now.
// ----------------------------------------------------------------------------

/// `(a-o) × (b-o)` — twice the signed area of triangle o,a,b. Sign = orientation.
pub(super) fn cross(o: Point, a: Point, b: Point) -> i128 {
    let (ax, ay) = ((a.x - o.x) as i128, (a.y - o.y) as i128);
    let (bx, by) = ((b.x - o.x) as i128, (b.y - o.y) as i128);
    ax * by - ay * bx
}

/// Is collinear point `p` within the bounding box of segment a–b?
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    a.x.min(b.x) <= p.x && p.x <= a.x.max(b.x) && a.y.min(b.y) <= p.y && p.y <= a.y.max(b.y)
}

/// Do segments a–b and c–d intersect (proper crossing or collinear touch)?
fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    (d1 == 0 && on_segment(c, d, a))
        || (d2 == 0 && on_segment(c, d, b))
        || (d3 == 0 && on_segment(a, b, c))
        || (d4 == 0 && on_segment(a, b, d))
}

/// Exact squared distance from point `p` to segment a–b, as a rational `num/den`
/// (`den` > 0). A degenerate segment (a == b) yields the point-to-point distance.
fn point_seg_dist2(p: Point, a: Point, b: Point) -> (i128, i128) {
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy; // |v|^2
    if den == 0 {
        return (wx * wx + wy * wy, 1);
    }
    let tnum = wx * vx + wy * vy; // w·v
    if tnum <= 0 {
        return (wx * wx + wy * wy, 1); // closest endpoint a
    }
    if tnum >= den {
        let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
        return (ux * ux + uy * uy, 1); // closest endpoint b
    }
    // Interior: |w|^2 - (w·v)^2 / |v|^2  ==  (|w|^2·|v|^2 - (w·v)^2) / |v|^2.
    let ww = wx * wx + wy * wy;
    (ww * den - tnum * tnum, den)
}

/// Compare `dist(p, seg a–b)` against threshold `t` (t >= 0): orders the real
/// distance by comparing squared values, exact in i128.
fn point_seg_cmp(p: Point, a: Point, b: Point, t: Nm) -> std::cmp::Ordering {
    let (num, den) = point_seg_dist2(p, a, b);
    let t = t as i128;
    (num).cmp(&(t * t * den))
}

/// Is the minimum distance between segments a–b and c–d within `t`? `strict`
/// selects `< t` (clearance: violation) vs `<= t` (incidence: touching).
fn seg_within(a: Point, b: Point, c: Point, d: Point, t: Nm, strict: bool) -> bool {
    use std::cmp::Ordering;
    if segments_intersect(a, b, c, d) {
        return if strict { t > 0 } else { true };
    }
    let hit = |ord: Ordering| {
        if strict {
            ord == Ordering::Less
        } else {
            ord != Ordering::Greater
        }
    };
    hit(point_seg_cmp(a, c, d, t))
        || hit(point_seg_cmp(b, c, d, t))
        || hit(point_seg_cmp(c, a, b, t))
        || hit(point_seg_cmp(d, a, b, t))
}

/// Iterate the segments of a polyline (a lone point becomes a degenerate segment).
fn segments(path: &[Point]) -> Vec<(Point, Point)> {
    match path.len() {
        0 => Vec::new(),
        1 => vec![(path[0], path[0])],
        _ => path.windows(2).map(|w| (w[0], w[1])).collect(),
    }
}

/// Is point `p` within `tol` (inclusive) of any segment of `path`?
fn point_on_polyline(p: Point, path: &[Point], tol: Nm) -> bool {
    segments(path)
        .iter()
        .any(|(a, b)| seg_within(p, p, *a, *b, tol, false))
}

/// Are two polylines within `tol` (inclusive) anywhere? (incidence)
fn polylines_closer_than_inc(p: &[Point], q: &[Point], tol: Nm) -> bool {
    let (sp, sq) = (segments(p), segments(q));
    sp.iter().any(|(a, b)| {
        sq.iter()
            .any(|(c, d)| seg_within(*a, *b, *c, *d, tol, false))
    })
}
