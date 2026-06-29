//! Routing: the trace/via/layer representation (tier-2 materialized state) and the
//! geometry + connectivity kernel the DRC query (tier-3) runs on.
//!
//! Per docs/architecture.md, routed copper is **tier-2 materialized state** that
//! lives in the `Doc` alongside component placement, each carrying a `Provenance`
//! bit: a hand-routed trace is `Pinned` (user-authored, treated by a future
//! autorouter as a fixed obstacle), a `Free` trace is solver/auto-driven and
//! regen-able. One provenance ladder governs placement and routing alike — there
//! is no separate "auto" subsystem.
//!
//! The DRC checks themselves are tier-3 (pure, deterministic, cheaply comparable):
//! [`check_drc`] is the reusable query body, called from the incremental engine in
//! `query.rs` (mirroring how ERC is computed there). All geometry is integer
//! nanometres; distance comparisons are done in exact `i128` arithmetic against
//! *squared* thresholds, so no float nondeterminism leaks into a violation set.

use crate::doc::{Doc, Nm, Point, PinRef, MM};
use crate::geom::{clearance_violated, Shape2D};
use crate::id::{NetId, TraceId};
use crate::part::{pad_copper_world, pin_world, PadLayers, PartLib, PinRole};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// A copper layer. `Top`/`Bottom` are the outer copper; `Inner(n)` keeps the model
/// trivially extensible to multilayer boards (n = 0-based inner-layer index). The
/// ordering is the physical stack-up top→bottom, which is what via spans test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Top,
    Inner(u8),
    Bottom,
}

impl Layer {
    /// Position in the physical stack-up (top = 0, descending). Used to test
    /// whether a layer falls within a via's spanned range. `Bottom` sits below any
    /// representable inner layer.
    pub fn depth(self) -> i32 {
        match self {
            Layer::Top => 0,
            Layer::Inner(n) => 1 + n as i32,
            Layer::Bottom => 1 + 256,
        }
    }
}

// Order layers by physical depth so a `Violation` set sorts canonically.
impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.depth().cmp(&other.depth())
    }
}

/// A routed copper polyline on one layer, belonging to one net. `width` is the
/// finished copper width (nm); `prov` is `Pinned` for hand/agent routing and
/// `Free` for a future autorouter's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub net: NetId,
    pub layer: Layer,
    /// Polyline centreline. Two or more points; consecutive points are segments.
    pub path: Vec<Point>,
    pub width: Nm,
    pub prov: crate::doc::Provenance,
}

/// A via: a plated point connecting copper across the layers it spans (`from`..`to`,
/// inclusive). Pads are points in this prototype, so a via is its centre `at`, a
/// `drill`, and a `pad` (annular copper diameter).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Via {
    pub net: NetId,
    pub at: Point,
    pub from: Layer,
    pub to: Layer,
    pub drill: Nm,
    pub pad: Nm,
    pub prov: crate::doc::Provenance,
}

impl Via {
    /// Does this via connect copper on `layer`? (Is `layer` within its span?)
    pub fn spans(&self, layer: Layer) -> bool {
        let (lo, hi) = (self.from.depth().min(self.to.depth()), self.from.depth().max(self.to.depth()));
        let d = layer.depth();
        lo <= d && d <= hi
    }
}

/// Manufacturing design rules consumed by DRC. Defaults are a generic 2-layer
/// process: 0.15 mm clearance and 0.15 mm minimum trace width. In production these
/// would be read from the board/source; the prototype carries them as a constant
/// (the DRC query uses `DesignRules::default()`), documented as the one knob to
/// wire to the source when a stack-up/process definition exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesignRules {
    /// Minimum copper-to-copper gap between *different* nets (edge to edge).
    pub min_clearance: Nm,
    /// Minimum finished trace width.
    pub min_trace_width: Nm,
    /// Tolerance for geometric incidence ("touching") in the connectivity/ratsnest
    /// check. Hand-placed coordinates are exact integers, so coincident endpoints
    /// have distance 0; this small slop absorbs deliberate near-misses without
    /// fusing genuinely separate copper.
    pub touch_tol: Nm,
}

impl Default for DesignRules {
    fn default() -> Self {
        DesignRules {
            min_clearance: 150_000,        // 0.15 mm
            min_trace_width: 150_000,      // 0.15 mm
            touch_tol: MM / 100,           // 0.01 mm
        }
    }
}

/// A single DRC violation. Deliberately small and `Ord` so the violation *set* is
/// canonical and cheaply comparable — that is what lets the query engine's early
/// cutoff fire (an edit that does not change this set does not propagate).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Violation {
    /// Copper of two different nets is closer than the clearance rule allows on a
    /// layer. Net ids are stored sorted so a pair is reported once regardless of
    /// which side was scanned first.
    Clearance { a: NetId, b: NetId, layer: Layer },
    /// A trace narrower than the minimum width rule.
    MinWidth { trace: TraceId, width: Nm },
    /// A net whose pins are not all electrically joined by the routing (ratsnest):
    /// `islands` is how many disconnected pin groups remain (>1 ⇒ unrouted /
    /// partially routed; the net is fully routed iff this would be 1).
    Unrouted { net: NetId, islands: usize },
}

/// DRC violations stay a typed domain result (the autorouter consumes them as
/// data); this renders them into the shared diagnostic vocabulary for display.
impl crate::diagnostic::Diagnose for Violation {
    fn diagnostics(&self) -> Vec<crate::diagnostic::Diagnostic> {
        use crate::diagnostic::{Diagnostic, Location};
        let d = match self {
            Violation::Clearance { a, b, layer } => Diagnostic::error(
                "E_DRC_CLEARANCE",
                format!("nets `{a}` and `{b}` are closer than clearance on {layer:?}"),
                Location::Net(a.clone()),
            ),
            Violation::MinWidth { trace, width } => Diagnostic::error(
                "E_DRC_MIN_WIDTH",
                format!("trace `{trace}` width {width}nm is below the minimum"),
                Location::Trace(*trace),
            ),
            Violation::Unrouted { net, islands } => Diagnostic::error(
                "E_DRC_UNROUTED",
                format!("net `{net}` is not fully routed ({islands} disconnected islands)"),
                Location::Net(net.clone()),
            ),
        };
        vec![d]
    }
}

/// Run the design-rule check over a document's routing. Pure and deterministic:
/// the only inputs are the routed copper (tier-2), the placement geometry (for pad
/// world positions, via `lib`), and the resolved netlist (`netlist`, which fixes
/// which pins each net must join). The query engine records those three as its
/// dependencies; this function does no dependency tracking itself.
///
/// Returns a canonical (sorted, de-duplicated) `Vec<Violation>`.
pub fn check_drc(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(crate::doc::PinRef, PinRole)>>,
    rules: &DesignRules,
) -> Vec<Violation> {
    let mut out: Vec<Violation> = Vec::new();

    // --- 1. Minimum width: every trace's width >= the rule. ---
    for (tid, t) in &doc.traces {
        if t.width < rules.min_trace_width {
            out.push(Violation::MinWidth { trace: *tid, width: t.width });
        }
    }

    // World position of every net-member pad, kept per net (pads are points; a
    // footprint carries no pad size, so radius is 0 — documented simplification).
    // Through-hole assumption: a pad participates on every layer.
    let mut net_pads: BTreeMap<NetId, Vec<Point>> = BTreeMap::new();
    for (nid, pins) in netlist {
        let mut pts = Vec::new();
        for (pr, _role) in pins {
            if let Some(c) = doc.components.get(&pr.comp)
                && let Some(def) = lib.get(&c.part)
                && let Some(p) = pin_world(c, def, &pr.pin)
            {
                pts.push(p);
            }
        }
        net_pads.insert(nid.clone(), pts);
    }

    // --- 2. Clearance: copper of *different* nets must be >= min_clearance. ---
    // All copper — traces, vias, AND pads — reduces to a world-frame `geom::Shape2D`
    // tagged with the layer(s) it occupies (the uniform "copper has extent" model;
    // pads are no longer points). A different-net pair sharing a layer is checked
    // edge-to-edge by `geom::clearance_violated`.
    let pieces = net_copper(doc, lib, netlist);
    let layers = copper_layers_present(doc);
    for i in 0..pieces.len() {
        for j in (i + 1)..pieces.len() {
            let (a, b) = (&pieces[i], &pieces[j]);
            if a.net == b.net {
                continue;
            }
            // The first (deterministic) layer both occupy, if any; the 2D shapes are
            // layer-independent, so one geometric test settles the pair.
            let Some(&l) = layers.iter().find(|&&l| a.layers.on(l) && b.layers.on(l)) else {
                continue;
            };
            if clearance_violated(&a.shape, &b.shape, rules.min_clearance) {
                out.push(clearance(&a.net, &b.net, l));
            }
        }
    }

    // --- 3. Connectivity completeness (ratsnest) via union-find. ---
    for (nid, pins) in netlist {
        let pts = &net_pads[nid];
        // A net with fewer than two pins is trivially "routed".
        if pins.len() < 2 {
            continue;
        }
        let net_traces: Vec<&Trace> = doc.traces.values().filter(|t| t.net == *nid).collect();
        let net_vias: Vec<&Via> = doc.vias.values().filter(|v| v.net == *nid).collect();
        let islands = pin_islands(pts, &net_traces, &net_vias, rules.touch_tol);
        if islands > 1 {
            out.push(Violation::Unrouted { net: nid.clone(), islands });
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Normalised clearance violation: net ids sorted so a pair reports once.
fn clearance(a: &NetId, b: &NetId, layer: Layer) -> Violation {
    let (lo, hi) = if a <= b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
    Violation::Clearance { a: lo, b: hi, layer }
}

/// A piece of world-frame copper for clearance: its net, 2D shape, and the layer(s)
/// it occupies. Traces, vias, and pads all reduce to this uniform form. Exposed to
/// the autorouter so it can verify its own proposed copper with the same machinery.
pub(crate) struct CopperPiece {
    pub(crate) net: NetId,
    pub(crate) shape: Shape2D,
    pub(crate) layers: PieceLayers,
}

/// How a copper piece occupies layers (for the same-layer clearance gate).
pub(crate) enum PieceLayers {
    Trace(Layer),
    Via(Layer, Layer),
    Pad(PadLayers),
}

impl PieceLayers {
    pub(crate) fn on(&self, l: Layer) -> bool {
        match self {
            PieceLayers::Trace(tl) => *tl == l,
            PieceLayers::Via(a, b) => {
                let (lo, hi) = (a.depth().min(b.depth()), a.depth().max(b.depth()));
                lo <= l.depth() && l.depth() <= hi
            }
            PieceLayers::Pad(PadLayers::Top) => l == Layer::Top,
            PieceLayers::Pad(PadLayers::Bottom) => l == Layer::Bottom,
            // A through-hole pad's annulus is on every copper layer.
            PieceLayers::Pad(PadLayers::Through) => true,
        }
    }
}

/// Every world-frame copper piece: each trace (polyline ⊕ width/2), each via (a disc
/// of its pad), and each netted pad's copper regions (its real `geom` shape, no
/// longer a point). Pads are attributed to their net via the resolved netlist; a pad
/// on no net (floating) is omitted here — it is surfaced by the `Floating` query.
pub(crate) fn net_copper(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
) -> Vec<CopperPiece> {
    let mut pin_net: BTreeMap<PinRef, NetId> = BTreeMap::new();
    for (nid, pins) in netlist {
        for (pr, _) in pins {
            pin_net.insert(pr.clone(), nid.clone());
        }
    }
    let mut pieces = Vec::new();
    for t in doc.traces.values() {
        pieces.push(CopperPiece {
            net: t.net.clone(),
            shape: Shape2D::trace(t.path.clone(), t.width),
            layers: PieceLayers::Trace(t.layer),
        });
    }
    for v in doc.vias.values() {
        pieces.push(CopperPiece {
            net: v.net.clone(),
            shape: Shape2D::disc(v.at, v.pad / 2),
            layers: PieceLayers::Via(v.from, v.to),
        });
    }
    for c in doc.components.values() {
        let Some(def) = lib.get(&c.part) else { continue };
        for pin in &def.pins {
            let Some(pad) = &pin.pad else { continue };
            let Some(net) = pin_net.get(&PinRef::new(&c.id, &pin.number)) else { continue };
            for cu in &pad.copper {
                pieces.push(CopperPiece {
                    net: net.clone(),
                    shape: pad_copper_world(c, cu),
                    layers: PieceLayers::Pad(cu.layers),
                });
            }
        }
    }
    pieces
}

/// The copper layers present in a design (outer layers always; plus any layer a
/// trace sits on or a via terminates on), sorted — the candidate set for choosing a
/// representative layer to report a clearance violation on.
pub(crate) fn copper_layers_present(doc: &Doc) -> Vec<Layer> {
    let mut set: BTreeSet<Layer> = BTreeSet::new();
    set.insert(Layer::Top);
    set.insert(Layer::Bottom);
    for t in doc.traces.values() {
        set.insert(t.layer);
    }
    for v in doc.vias.values() {
        set.insert(v.from);
        set.insert(v.to);
    }
    set.into_iter().collect()
}

// ----------------------------------------------------------------------------
// Connectivity: union-find over a net's pins + traces + vias by geometric
// incidence. Two pins are electrically joined iff they end up in one component.
// ----------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind { parent: (0..n).collect() }
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
/// net's copper. Nodes: pins, then traces, then vias. Incidence (within `tol`):
/// pin↔pin (coincident pads), pin↔trace and pin↔via (pads are all-layer points),
/// trace↔trace (same layer), trace↔via and via↔via (via must span the layer).
fn pin_islands(pins: &[Point], traces: &[&Trace], vias: &[&Via], tol: Nm) -> usize {
    let (np, nt, nv) = (pins.len(), traces.len(), vias.len());
    let mut uf = UnionFind::new(np + nt + nv);
    let trace_node = |i: usize| np + i;
    let via_node = |i: usize| np + nt + i;

    // pin ↔ pin
    for i in 0..np {
        for j in (i + 1)..np {
            if seg_within(pins[i], pins[i], pins[j], pins[j], tol, false) {
                uf.union(i, j);
            }
        }
    }
    // pin ↔ trace, pin ↔ via
    for (pi, p) in pins.iter().enumerate() {
        for (ti, t) in traces.iter().enumerate() {
            if point_on_polyline(*p, &t.path, tol) {
                uf.union(pi, trace_node(ti));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if seg_within(*p, *p, v.at, v.at, tol, false) {
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
            if v.spans(t.layer) && point_on_polyline(v.at, &t.path, tol) {
                uf.union(trace_node(ti), via_node(vi));
            }
        }
    }
    // via ↔ via (coincident, spans overlap)
    for i in 0..nv {
        for j in (i + 1)..nv {
            let (u, w) = (vias[i], vias[j]);
            let overlap = u.from.depth().min(u.to.depth()) <= w.from.depth().max(w.to.depth())
                && w.from.depth().min(w.to.depth()) <= u.from.depth().max(u.to.depth());
            if overlap && seg_within(u.at, u.at, w.at, w.at, tol, false) {
                uf.union(via_node(i), via_node(j));
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
// ----------------------------------------------------------------------------

/// `(a-o) × (b-o)` — twice the signed area of triangle o,a,b. Sign = orientation.
fn cross(o: Point, a: Point, b: Point) -> i128 {
    let (ax, ay) = ((a.x - o.x) as i128, (a.y - o.y) as i128);
    let (bx, by) = ((b.x - o.x) as i128, (b.y - o.y) as i128);
    ax * by - ay * bx
}

/// Is collinear point `p` within the bounding box of segment a–b?
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    a.x.min(b.x) <= p.x
        && p.x <= a.x.max(b.x)
        && a.y.min(b.y) <= p.y
        && p.y <= a.y.max(b.y)
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
fn point_seg_cmp(p: Point, a: Point, b: Point, t: Nm) -> Ordering {
    let (num, den) = point_seg_dist2(p, a, b);
    let t = t as i128;
    (num).cmp(&(t * t * den))
}

/// Is the minimum distance between segments a–b and c–d within `t`? `strict`
/// selects `< t` (clearance: violation) vs `<= t` (incidence: touching).
fn seg_within(a: Point, b: Point, c: Point, d: Point, t: Nm, strict: bool) -> bool {
    if segments_intersect(a, b, c, d) {
        return if strict { t > 0 } else { true };
    }
    let hit = |ord: Ordering| if strict { ord == Ordering::Less } else { ord != Ordering::Greater };
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
    segments(path).iter().any(|(a, b)| seg_within(p, p, *a, *b, tol, false))
}

/// Are two polylines within `tol` (inclusive) anywhere? (incidence)
fn polylines_closer_than_inc(p: &[Point], q: &[Point], tol: Nm) -> bool {
    let (sp, sq) = (segments(p), segments(q));
    sp.iter().any(|(a, b)| sq.iter().any(|(c, d)| seg_within(*a, *b, *c, *d, tol, false)))
}
