//! A basic deterministic grid/maze autorouter — a *transaction-proposer*.
//!
//! Per docs/architecture.md ("Solvers as transaction-proposers, not owners"), the
//! autorouter is a pure function `(netlist, placement, pinned-routes) -> route
//! transaction`. It does **not** mutate the `Doc`: it reads the elaborated facts and
//! returns a `Vec<Command>` (`AddTrace`/`AddVia`, all `Provenance::Free`) plus a
//! report of the nets it could not route. Applying those commands goes through the
//! ordinary atomic [`crate::command::apply`] path, exactly like a hand edit — the
//! GUI cannot tell an autoroute trace from a hand route except by its provenance bit.
//!
//! ## Algorithm (Lee/A* maze routing, net by net)
//!
//! The board (the source `Board` outline if present, else the placement bounding
//! box) is discretised into a square routing grid. We A* over `(x, y, layer)` with
//! `Top`/`Bottom` copper and vias to change layer, routing each net's pins together
//! incrementally (MST-style: each remaining pin is routed to the net's existing
//! connected copper). Obstacles — the board exterior, other-net pads, other-net
//! pre-existing (`Pinned`/`Free`) traces & vias, and copper already routed this run
//! for *other* nets — map to blocked grid cells; same-net copper is never blocked.
//!
//! ## Grid pitch (and why clearance falls out)
//!
//! `pitch = via_pad + min_clearance`, with `via_pad = 2 * min_trace_width`,
//! `via_drill = min_trace_width`. All routed copper lies on grid nodes / axis-aligned
//! grid edges, and **distinct nets never share a grid node** (node ownership). The
//! minimum distance between two grid nodes used by different nets is therefore exactly
//! `pitch`, which was chosen so that *every* adjacent-node copper pairing meets the
//! edge-to-edge clearance rule:
//!
//! - track↔track: `pitch − width  = via_pad + clr − width ≥ clr`
//! - track↔via:   `pitch − pad/2 − width/2 ≥ clr`
//! - via↔via:     `pitch − pad/2 − pad/2   = clr` (exactly — passes, DRC is strict `<`)
//!
//! So routed-vs-routed clearance is *heuristically* clean on-grid; off-grid obstacles
//! (pads and pre-existing traces/vias) get radius-based cell blocking. But this
//! construction invariant is **not trusted**: it fails at sub-grid pitch and never
//! covered the off-grid pad stubs, so [`autoroute`] runs a final pad-aware clearance
//! check ([`verify_and_prune`]) and drops any net whose proposed copper actually
//! clashes. `routed` therefore means *verified clean*, not clean-by-assertion
//! (issue 0003).
//!
//! ## Honest limitations (documented, by design)
//!
//! This is greedy net-by-net maze routing — deliberately basic. There is **no
//! rip-up-and-retry, no topological/push-and-shove, no length/impedance matching**.
//! Consequently **net ordering matters**: a net that fails may well be routable in a
//! different order (an earlier net can wall off a later one). Failures are *reported*
//! (the net goes in `unrouted`), never fatal and never emitted as partial/overlapping
//! copper — a net that cannot connect all its pins contributes **no** commands.
//! Pads are points (the model carries no pad size); existing *same-net* copper is
//! treated as a non-obstacle but is not used as a routing seed (the router re-routes
//! a net from its pins). All geometry is integer nm; everything is deterministic.

use crate::command::Command;
use crate::doc::{Doc, Nm, PinRef, Point, Provenance};
use crate::geom::{clearance_violated, Shape2D};
use crate::id::{NetId, TraceId, ViaId};
use crate::part::{pin_world, PartLib, PinRole};
use crate::route::{
    copper_layers_present, net_copper, CopperPiece, DesignRules, Layer, PieceLayers, Trace, Via,
};
use crate::solve::Rect;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

/// The proposed routing transaction plus a report of what could not be routed.
///
/// `commands` are ready to feed to [`crate::command::apply`] (atomic, all-or-nothing);
/// they are exclusively `AddTrace`/`AddVia` carrying `Provenance::Free`. `routed` and
/// `unrouted` list the nets the run succeeded / failed on (a multi-pin net is "routed"
/// only when *all* its pins were connected; a failed net emits no copper).
#[derive(Clone, Debug, Default)]
pub struct AutorouteResult {
    pub commands: Vec<Command>,
    pub routed: Vec<NetId>,
    pub unrouted: Vec<NetId>,
}

// Layer index in the 2-layer grid (only Top/Bottom are routed).
const TOP: usize = 0;
const BOT: usize = 1;
fn layer_of(l: usize) -> Layer {
    if l == TOP { Layer::Top } else { Layer::Bottom }
}

/// Propose a routing transaction for `doc`. Pure: reads facts, returns commands.
pub fn autoroute(doc: &Doc, lib: &PartLib, rules: &DesignRules) -> AutorouteResult {
    let width = rules.min_trace_width;
    let via_pad = 2 * rules.min_trace_width;
    let via_drill = rules.min_trace_width;
    let pitch = via_pad + rules.min_clearance;

    // World pad positions for every net's pins, keyed by net (BTreeMap → sorted,
    // deterministic). A pin with no resolvable world position is dropped.
    let mut net_pads: BTreeMap<NetId, Vec<Point>> = BTreeMap::new();
    for (nid, net) in &doc.nets {
        let mut pts = Vec::new();
        for pr in &net.members {
            if let Some(c) = doc.components.get(&pr.comp)
                && let Some(def) = lib.get(&c.part)
                && let Some(p) = pin_world(c, def, &pr.pin)
            {
                pts.push(p);
            }
        }
        net_pads.insert(nid.clone(), pts);
    }

    // Routing area: the source Board outline, else the bounding box of all pads.
    let Some(area) = routing_area(doc, &net_pads, pitch) else {
        // Nothing to route (no geometry).
        return AutorouteResult::default();
    };
    let grid = Grid::new(area, pitch);
    if grid.cols == 0 || grid.rows == 0 {
        return AutorouteResult::default();
    }

    // Ownership of each (node, layer) by a net (its routed copper passes through).
    // -1 = free. Distinct nets never share a node ⇒ clearance falls out of `pitch`.
    let mut owner = vec![[-1i32; 2]; grid.cols * grid.rows];

    // Id minting: continue past any ids already in the doc (caller-assigned, like
    // KiCad UUIDs — a hand edit and the autorouter mint the same way).
    let mut next_tid = doc.traces.keys().map(|t| t.0 + 1).max().unwrap_or(1);
    let mut next_vid = doc.vias.keys().map(|v| v.0 + 1).max().unwrap_or(1);

    let mut result = AutorouteResult::default();

    // Route net by net, in NetId order (deterministic). A net seq id tags ownership.
    for (net_seq, (nid, pads)) in net_pads.iter().enumerate() {
        // Nets with <2 reachable pins are trivially "routed" (nothing to connect).
        if pads.len() < 2 {
            continue;
        }

        // Per-net obstacle map: every *other* net's pads, and all pre-existing
        // traces/vias whose net differs (Pinned hand routes are fixed obstacles).
        let obstacles = collect_obstacles(doc, &net_pads, nid);
        let block = BlockMap::build(&grid, &obstacles, rules, width, via_pad);

        match route_net(
            &grid,
            &block,
            &mut owner,
            net_seq as i32,
            nid,
            pads,
            width,
            via_pad,
            via_drill,
            &mut next_tid,
            &mut next_vid,
        ) {
            Some(cmds) => {
                result.commands.extend(cmds);
                result.routed.push(nid.clone());
            }
            None => result.unrouted.push(nid.clone()),
        }
    }

    // Don't trust the construction invariant — verify the proposed copper against the
    // same pad-aware clearance DRC uses, and drop any net that actually clashes.
    verify_and_prune(doc, lib, rules, &mut result);
    result
}

/// Self-honesty (issue 0003): the grid's "clearance-clean by construction" invariant
/// fails at sub-grid pitch (and never covered the off-grid pad stubs), so do not
/// trust it. Check each *proposed* piece of copper against all other-net copper
/// (existing pads / pre-existing traces+vias + other proposed copper) with the same
/// `geom` clearance DRC uses; any routed net whose proposed copper clashes is dropped
/// — its commands removed, the net moved to `unrouted`. So `routed` means *verified
/// clearance-clean*, not clean-by-assertion. Dropping every clashing net is
/// conservative (it can drop a net that a smarter order/rip-up would keep — that is
/// future work, issue 0008); the point here is honesty, not optimality.
fn verify_and_prune(doc: &Doc, lib: &PartLib, rules: &DesignRules, result: &mut AutorouteResult) {
    // Existing copper (pads + pre-existing traces/vias) via the shared machinery.
    // Clearance ignores roles, so a Passive placeholder role is fine.
    let netlist: BTreeMap<NetId, Vec<(PinRef, PinRole)>> = doc
        .nets
        .iter()
        .map(|(nid, net)| {
            (nid.clone(), net.members.iter().map(|m| (m.clone(), PinRole::Passive)).collect())
        })
        .collect();
    let existing = net_copper(doc, lib, &netlist);

    // This run's proposed copper.
    let mut proposed: Vec<CopperPiece> = Vec::new();
    for cmd in &result.commands {
        match cmd {
            Command::AddTrace(_, t) => proposed.push(CopperPiece {
                net: t.net.clone(),
                shape: Shape2D::trace(t.path.clone(), t.width),
                layers: PieceLayers::Trace(t.layer),
            }),
            Command::AddVia(_, v) => proposed.push(CopperPiece {
                net: v.net.clone(),
                shape: Shape2D::disc(v.at, v.pad / 2),
                layers: PieceLayers::Via(v.from, v.to),
            }),
            _ => {}
        }
    }

    let layers = copper_layers_present(doc);
    let shares = |a: &CopperPiece, b: &CopperPiece| {
        layers.iter().any(|&l| a.layers.on(l) && b.layers.on(l))
    };
    let mut unclean: BTreeSet<NetId> = BTreeSet::new();
    for p in &proposed {
        if unclean.contains(&p.net) {
            continue;
        }
        let clashes = existing.iter().chain(proposed.iter()).any(|o| {
            o.net != p.net && shares(p, o) && clearance_violated(&p.shape, &o.shape, rules.min_clearance)
        });
        if clashes {
            unclean.insert(p.net.clone());
        }
    }
    if unclean.is_empty() {
        return;
    }
    result.commands.retain(|c| match c {
        Command::AddTrace(_, t) => !unclean.contains(&t.net),
        Command::AddVia(_, v) => !unclean.contains(&v.net),
        _ => true,
    });
    result.routed.retain(|n| !unclean.contains(n));
    result.unrouted.extend(unclean);
    result.unrouted.sort();
    result.unrouted.dedup();
}

/// Choose the routing area: the board outline's bounding box if a `Board` is
/// declared, else the bounding box of every pad, padded by two grid pitches so edge
/// pins have room. (The grid spans the bbox; masking cells to a non-rectangular
/// outline / out of cutouts is a follow-up — see architecture.md §8.)
fn routing_area(doc: &Doc, net_pads: &BTreeMap<NetId, Vec<Point>>, pitch: Nm) -> Option<Rect> {
    if let Some(board) = crate::elaborate::board_shape(&doc.source)
        && let Some((min, max)) = board.bbox()
    {
        return Some(Rect { min, max });
    }
    let mut it = net_pads.values().flatten().copied();
    let first = it.next()?;
    let (mut min, mut max) = (first, first);
    for p in net_pads.values().flatten().copied() {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    let m = 2 * pitch;
    Some(Rect {
        min: Point { x: min.x - m, y: min.y - m },
        max: Point { x: max.x + m, y: max.y + m },
    })
}

// ----------------------------------------------------------------------------
// The grid.
// ----------------------------------------------------------------------------

struct Grid {
    origin: Point,
    pitch: Nm,
    cols: usize,
    rows: usize,
}

impl Grid {
    fn new(area: Rect, pitch: Nm) -> Grid {
        let cols = ((area.max.x - area.min.x) / pitch).max(0) as usize + 1;
        let rows = ((area.max.y - area.min.y) / pitch).max(0) as usize + 1;
        Grid { origin: area.min, pitch, cols, rows }
    }
    fn world(&self, i: usize, j: usize) -> Point {
        Point { x: self.origin.x + i as Nm * self.pitch, y: self.origin.y + j as Nm * self.pitch }
    }
    fn idx(&self, i: usize, j: usize) -> usize {
        j * self.cols + i
    }
}

// ----------------------------------------------------------------------------
// Obstacles → blocked cells.
// ----------------------------------------------------------------------------

/// An off-grid obstacle a routed trace/via of the current net must clear. (On-grid
/// same-run copper is handled by node ownership, not here.)
enum Obstacle {
    /// A pad (point, present on all layers — the model carries no pad size).
    Pad(Point),
    /// A pre-existing trace centreline segment on one layer, with its copper width.
    Seg(Point, Point, Nm, Layer),
    /// A pre-existing via: centre, pad diameter, and the layer span it occupies.
    Via(Point, Nm, Layer, Layer),
}

fn collect_obstacles(
    doc: &Doc,
    net_pads: &BTreeMap<NetId, Vec<Point>>,
    cur: &NetId,
) -> Vec<Obstacle> {
    let mut obs = Vec::new();
    // Other nets' pads.
    for (nid, pts) in net_pads {
        if nid == cur {
            continue;
        }
        for p in pts {
            obs.push(Obstacle::Pad(*p));
        }
    }
    // Pre-existing copper of other nets (hand-routed Pinned, or prior Free).
    for t in doc.traces.values() {
        if t.net == *cur {
            continue;
        }
        for w in t.path.windows(2) {
            obs.push(Obstacle::Seg(w[0], w[1], t.width, t.layer));
        }
    }
    for v in doc.vias.values() {
        if v.net == *cur {
            continue;
        }
        obs.push(Obstacle::Via(v.at, v.pad, v.from, v.to));
    }
    obs
}

/// Per-net precomputed blocked-cell map. `trace[layer][idx]` = a trace of the current
/// net may not occupy that node on that layer; `via[idx]` = a via may not be placed
/// there. Sized so a node *and the half-edges leaving it* stay clearance-clean.
struct BlockMap {
    trace: [Vec<bool>; 2],
    via: Vec<bool>,
}

impl BlockMap {
    fn build(grid: &Grid, obs: &[Obstacle], rules: &DesignRules, width: Nm, via_pad: Nm) -> BlockMap {
        let n = grid.cols * grid.rows;
        let mut trace = [vec![false; n], vec![false; n]];
        let mut via = vec![false; n];
        let clr = rules.min_clearance;
        let half_edge = grid.pitch / 2; // a routed edge reaches a neighbour `pitch` away
        for j in 0..grid.rows {
            for i in 0..grid.cols {
                let w = grid.world(i, j);
                let idx = grid.idx(i, j);
                for ob in obs {
                    match ob {
                        Obstacle::Pad(p) => {
                            // Trace-vs-pad threshold + half-edge slop, both layers.
                            if within(w, *p, *p, clr + width / 2 + half_edge) {
                                trace[TOP][idx] = true;
                                trace[BOT][idx] = true;
                            }
                            // Via-vs-pad threshold.
                            if within(w, *p, *p, clr + via_pad / 2 + half_edge) {
                                via[idx] = true;
                            }
                        }
                        Obstacle::Seg(a, b, ow, layer) => {
                            let li = if *layer == Layer::Top { TOP } else { BOT };
                            // Only Top/Bottom obstacle traces map onto the 2-layer grid;
                            // any inner-layer obstacle (not produced here) is ignored.
                            if *layer == Layer::Top || *layer == Layer::Bottom {
                                if within(w, *a, *b, clr + width / 2 + ow / 2 + half_edge) {
                                    trace[li][idx] = true;
                                }
                                // A via's annulus sits on both outer layers, so any
                                // nearby outer-layer obstacle trace blocks via placement.
                                if within(w, *a, *b, clr + via_pad / 2 + ow / 2 + half_edge) {
                                    via[idx] = true;
                                }
                            }
                        }
                        Obstacle::Via(p, opad, from, to) => {
                            let spans = |l: Layer| {
                                let (lo, hi) = (from.depth().min(to.depth()), from.depth().max(to.depth()));
                                lo <= l.depth() && l.depth() <= hi
                            };
                            for (li, l) in [(TOP, Layer::Top), (BOT, Layer::Bottom)] {
                                if spans(l)
                                    && within(w, *p, *p, clr + width / 2 + opad / 2 + half_edge)
                                {
                                    trace[li][idx] = true;
                                }
                            }
                            if within(w, *p, *p, clr + via_pad / 2 + opad / 2 + half_edge) {
                                via[idx] = true;
                            }
                        }
                    }
                }
            }
        }
        BlockMap { trace, via }
    }
}

// ----------------------------------------------------------------------------
// Per-net maze routing.
// ----------------------------------------------------------------------------

type State = (usize, usize, usize); // (i, j, layer)
/// A polyline run on one layer (world points), as produced from an A* path.
type Run = (Layer, Vec<Point>);

#[allow(clippy::too_many_arguments)]
fn route_net(
    grid: &Grid,
    block: &BlockMap,
    owner: &mut [[i32; 2]],
    net_seq: i32,
    nid: &NetId,
    pads: &[Point],
    width: Nm,
    via_pad: Nm,
    via_drill: Nm,
    next_tid: &mut u64,
    next_vid: &mut u64,
) -> Option<Vec<Command>> {
    // Map each pad to the nearest grid node the current net may occupy.
    let mut pin_nodes: Vec<(usize, usize)> = Vec::with_capacity(pads.len());
    for p in pads {
        pin_nodes.push(nearest_routable(grid, block, owner, net_seq, *p)?);
    }

    // Claim list for rollback if any pin fails (so a partial net blocks no one).
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    let claim = |owner: &mut [[i32; 2]], i: usize, j: usize, l: usize, claimed: &mut Vec<(usize, usize)>| {
        let idx = grid.idx(i, j);
        if owner[idx][l] != net_seq {
            owner[idx][l] = net_seq;
            claimed.push((idx, l));
        }
    };

    let mut commands: Vec<Command> = Vec::new();

    // Seed the connected tree at pin 0; stub its pad onto its grid node.
    let (si, sj) = pin_nodes[0];
    claim(owner, si, sj, TOP, &mut claimed);
    let seed_world = grid.world(si, sj);
    if seed_world != pads[0] {
        commands.push(Command::AddTrace(
            TraceId(mint(next_tid)),
            Trace { net: nid.clone(), layer: Layer::Top, path: vec![pads[0], seed_world], width, prov: Provenance::Free },
        ));
    }
    // The set of (node, layer) currently in the net's connected copper.
    let mut tree: Vec<State> = vec![(si, sj, TOP)];

    // Route each remaining pin to the existing tree.
    for k in 1..pin_nodes.len() {
        let goal = pin_nodes[k];
        let Some(path) = astar(grid, block, owner, net_seq, &tree, goal) else {
            // Roll back this net's claims and emit nothing for it.
            for (idx, l) in claimed {
                owner[idx][l] = -1;
            }
            return None;
        };

        // Claim the path's cells, add them to the tree.
        for &(i, j, l) in &path {
            claim(owner, i, j, l, &mut claimed);
            tree.push((i, j, l));
        }

        // Convert the grid path into per-layer trace runs + via points, appending
        // the goal pad onto the final run so the trace literally touches the pad.
        let (runs, vias) = path_to_runs(grid, &path);
        for (vi, vj) in vias {
            commands.push(Command::AddVia(
                ViaId(mint(next_vid)),
                Via {
                    net: nid.clone(),
                    at: grid.world(vi, vj),
                    from: Layer::Top,
                    to: Layer::Bottom,
                    drill: via_drill,
                    pad: via_pad,
                    prov: Provenance::Free,
                },
            ));
        }
        let last = runs.len().saturating_sub(1);
        for (ri, (layer, mut pts)) in runs.into_iter().enumerate() {
            if ri == last {
                pts.push(pads[k]); // stub onto the goal pad
            }
            let pts = coalesce(pts);
            if pts.len() >= 2 {
                commands.push(Command::AddTrace(
                    TraceId(mint(next_tid)),
                    Trace { net: nid.clone(), layer, path: pts, width, prov: Provenance::Free },
                ));
            }
        }
    }

    Some(commands)
}

fn mint(counter: &mut u64) -> u64 {
    let v = *counter;
    *counter += 1;
    v
}

/// Nearest grid node to `p` that the current net may occupy on the Top layer
/// (deterministic: scans in fixed order, picks min squared distance, ties by index).
fn nearest_routable(
    grid: &Grid,
    block: &BlockMap,
    owner: &[[i32; 2]],
    net_seq: i32,
    p: Point,
) -> Option<(usize, usize)> {
    let mut best: Option<((usize, usize), i128)> = None;
    for j in 0..grid.rows {
        for i in 0..grid.cols {
            let idx = grid.idx(i, j);
            if block.trace[TOP][idx] || (owner[idx][TOP] != -1 && owner[idx][TOP] != net_seq) {
                continue;
            }
            let w = grid.world(i, j);
            let dx = (w.x - p.x) as i128;
            let dy = (w.y - p.y) as i128;
            let d2 = dx * dx + dy * dy;
            if best.is_none_or(|(_, bd)| d2 < bd) {
                best = Some(((i, j), d2));
            }
        }
    }
    best.map(|(ij, _)| ij)
}

/// A* over `(i, j, layer)` from any node in `tree` (multi-source) to `goal` (reachable
/// on either layer). Orthogonal steps cost one pitch; a layer change costs a via
/// penalty. Deterministic: the frontier orders by `(f, i, j, layer)`.
fn astar(
    grid: &Grid,
    block: &BlockMap,
    owner: &[[i32; 2]],
    net_seq: i32,
    tree: &[State],
    goal: (usize, usize),
) -> Option<Vec<State>> {
    let pitch = grid.pitch;
    let via_pen = 10 * pitch; // strongly prefer staying on one layer (fewer vias)
    let n = grid.cols * grid.rows;
    let sidx = |s: State| s.2 * n + grid.idx(s.0, s.1);

    let mut g = vec![i64::MAX; n * 2];
    let mut came: Vec<Option<State>> = vec![None; n * 2];
    let mut heap: BinaryHeap<Reverse<(i64, usize, usize, usize)>> = BinaryHeap::new();

    let h = |i: usize, j: usize| -> i64 {
        let di = (i as i64 - goal.0 as i64).abs();
        let dj = (j as i64 - goal.1 as i64).abs();
        (di + dj) * pitch
    };

    for &(i, j, l) in tree {
        let s = (i, j, l);
        if g[sidx(s)] != 0 {
            g[sidx(s)] = 0;
            came[sidx(s)] = None;
            heap.push(Reverse((h(i, j), i, j, l)));
        }
    }

    let passable = |i: usize, j: usize, l: usize| -> bool {
        let idx = grid.idx(i, j);
        !block.trace[l][idx] && (owner[idx][l] == -1 || owner[idx][l] == net_seq)
    };
    let via_ok = |i: usize, j: usize| -> bool {
        let idx = grid.idx(i, j);
        !block.via[idx] && passable(i, j, TOP) && passable(i, j, BOT)
    };

    while let Some(Reverse((f, i, j, l))) = heap.pop() {
        let s = (i, j, l);
        let gs = g[sidx(s)];
        if f > gs.saturating_add(h(i, j)) {
            continue; // stale
        }
        if (i, j) == goal {
            // Reconstruct.
            let mut path = vec![s];
            let mut cur = s;
            while let Some(prev) = came[sidx(cur)] {
                path.push(prev);
                cur = prev;
            }
            path.reverse();
            return Some(path);
        }

        // Orthogonal neighbours on the same layer.
        let mut nbrs: Vec<(usize, usize, usize, i64)> = Vec::new();
        if i + 1 < grid.cols {
            nbrs.push((i + 1, j, l, pitch));
        }
        if i > 0 {
            nbrs.push((i - 1, j, l, pitch));
        }
        if j + 1 < grid.rows {
            nbrs.push((i, j + 1, l, pitch));
        }
        if j > 0 {
            nbrs.push((i, j - 1, l, pitch));
        }
        // Layer change in place (a via).
        let other = if l == TOP { BOT } else { TOP };
        if via_ok(i, j) {
            nbrs.push((i, j, other, via_pen));
        }

        for (ni, nj, nl, step) in nbrs {
            if nl == l && !passable(ni, nj, nl) {
                continue;
            }
            let ns = (ni, nj, nl);
            let ng = gs.saturating_add(step);
            if ng < g[sidx(ns)] {
                g[sidx(ns)] = ng;
                came[sidx(ns)] = Some(s);
                heap.push(Reverse((ng.saturating_add(h(ni, nj)), ni, nj, nl)));
            }
        }
    }
    None
}

/// Split an A* path into per-layer polyline runs (in world coords) plus the grid
/// nodes where a via is dropped. A layer change between consecutive states is a via
/// at that (shared) node; both the closing and opening run carry the node's point, so
/// the via touches copper on both layers (and the ratsnest unions them).
fn path_to_runs(grid: &Grid, path: &[State]) -> (Vec<Run>, Vec<(usize, usize)>) {
    let mut runs: Vec<Run> = Vec::new();
    let mut vias: Vec<(usize, usize)> = Vec::new();
    let (i0, j0, l0) = path[0];
    let mut cur_layer = l0;
    let mut cur_pts = vec![grid.world(i0, j0)];
    for &(i, j, l) in &path[1..] {
        if l == cur_layer {
            cur_pts.push(grid.world(i, j));
        } else {
            runs.push((layer_of(cur_layer), std::mem::take(&mut cur_pts)));
            vias.push((i, j)); // same (i,j) as the previous state
            cur_layer = l;
            cur_pts = vec![grid.world(i, j)];
        }
    }
    runs.push((layer_of(cur_layer), cur_pts));
    (runs, vias)
}

/// Drop interior points that are collinear with their neighbours, and consecutive
/// duplicates — so a straight grid run becomes a single segment (fewer vertices).
fn coalesce(pts: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(pts.len());
    for p in pts {
        if out.last() == Some(&p) {
            continue; // drop duplicate
        }
        while out.len() >= 2 {
            let a = out[out.len() - 2];
            let b = out[out.len() - 1];
            // (b-a) × (p-a) == 0  ⇒  a,b,p collinear ⇒ b is redundant.
            let cross = (b.x - a.x) as i128 * (p.y - a.y) as i128
                - (b.y - a.y) as i128 * (p.x - a.x) as i128;
            if cross == 0 {
                out.pop();
            } else {
                break;
            }
        }
        out.push(p);
    }
    out
}

/// Is the distance from point `p` to segment `a`–`b` within `r` (inclusive)? Exact
/// i128 squared-distance comparison (a rational `num/den`) — no float, deterministic.
fn within(p: Point, a: Point, b: Point, r: Nm) -> bool {
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy;
    let (num, den) = if den == 0 {
        (wx * wx + wy * wy, 1)
    } else {
        let t = wx * vx + wy * vy;
        if t <= 0 {
            (wx * wx + wy * wy, 1)
        } else if t >= den {
            let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
            (ux * ux + uy * uy, 1)
        } else {
            let ww = wx * wx + wy * wy;
            (ww * den - t * t, den)
        }
    };
    let rr = r as i128;
    num <= rr * rr * den
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::doc::Point;
    use crate::elaborate::{board_rect, GenDirective as G, Source};
    use crate::history::History;
    use crate::id::TraceId;
    use crate::part::part_library;
    use crate::query::{Engine, Key};
    use crate::route::Violation;

    /// Elaborate a source into a routed `History` head.
    fn doc_of(src: Source) -> History {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "src").unwrap();
        h
    }

    /// Issue 0003: a proposed trace that clashes a different-net pad is dropped by the
    /// self-verify (the construction invariant is not trusted), and the net is moved
    /// to `unrouted` — `routed` never includes a net whose copper actually violates.
    #[test]
    fn verify_prunes_a_net_whose_trace_clashes_a_pad() {
        use crate::geom::Shape2D;
        use crate::part::{PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole};
        // A part with one pin carrying a 0.5 mm square copper pad.
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: Shape2D::rect(Point { x: 0, y: 0 }, 500_000, 500_000),
                    layers: PadLayers::Top,
                }],
                drill: None,
            }),
        };
        let mut lib = crate::part::PartLib::new();
        lib.insert(
            "PAD".into(),
            PartDef { name: "PAD".into(), pins: vec![pin], interfaces: BTreeMap::new() },
        );
        // Net B's pad sits at the origin; net A is a separate net.
        let src = vec![
            G::Instance { path: "b".into(), part: "PAD".into() },
            G::Fix { path: "b".into(), pos: Point { x: 0, y: 0 } },
            G::ConnectPins { net: "B".into(), pins: vec![("b".into(), "1".into())] },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "src").unwrap();

        // A net-A trace whose centreline runs straight through net B's pad.
        let mut result = AutorouteResult {
            commands: vec![Command::AddTrace(
                TraceId(1),
                Trace {
                    net: NetId::new("A"),
                    layer: Layer::Top,
                    path: vec![Point::mm(-2, 0), Point::mm(2, 0)],
                    width: 200_000,
                    prov: Provenance::Free,
                },
            )],
            routed: vec![NetId::new("A")],
            unrouted: vec![],
        };
        verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
        assert!(result.commands.is_empty(), "trace through a different-net pad must be pruned");
        assert!(result.routed.is_empty(), "the clashing net must leave `routed`");
        assert!(result.unrouted.contains(&NetId::new("A")), "and be reported unrouted");
    }

    /// Apply a proposed transaction's commands to the history head.
    fn apply_all(h: &mut History, cmds: Vec<Command>) {
        let lib = part_library();
        h.commit(Transaction(cmds), &lib, "autoroute").unwrap();
    }

    /// DRC violation set at the current head.
    fn drc(h: &History) -> Vec<Violation> {
        let lib = part_library();
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Drc).as_drc().to_vec()
    }

    fn has_clearance_or_width(v: &[Violation]) -> bool {
        v.iter().any(|x| matches!(x, Violation::Clearance { .. } | Violation::MinWidth { .. }))
    }

    /// A two-net board on an explicit outline: VBUS (reg.VOUT↔dec.p1) and GND
    /// (reg.GND↔dec.p2). reg(LDO)@(0,0), dec(Cap)@(12,0).
    fn two_net_board() -> Source {
        vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance { path: "reg".into(), part: "LDO".into() },
            G::Instance { path: "dec".into(), part: "Cap".into() },
            G::Place { path: "reg".into(), pos: Point::mm(0, 0) },
            G::Place { path: "dec".into(), pos: Point::mm(12, 0) },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("reg".into(), "GND".into()), ("dec".into(), "p2".into())],
            },
        ]
    }

    /// Autoroute makes the previously-unrouted nets pass the ratsnest, and introduces
    /// no clearance/width violations (verified through the real DRC query).
    #[test]
    fn autoroute_two_nets_clean_via_drc() {
        let lib = part_library();
        let mut h = doc_of(two_net_board());

        // Before: both nets are unrouted (ratsnest islands).
        let before = drc(&h);
        assert!(
            before.iter().any(|v| matches!(v, Violation::Unrouted { .. })),
            "expected unrouted nets before routing: {before:?}"
        );

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(r.unrouted, Vec::<NetId>::new(), "both nets should route");
        assert_eq!(r.routed.len(), 2);
        assert!(!r.commands.is_empty());

        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(after.is_empty(), "routed board must be DRC clean, got {after:?}");
    }

    /// Determinism: the same document autoroutes to byte-identical commands.
    #[test]
    fn autoroute_is_deterministic() {
        let lib = part_library();
        let h = doc_of(two_net_board());
        let r1 = autoroute(h.doc(), &lib, &DesignRules::default());
        let r2 = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(format!("{:?}", r1.commands), format!("{:?}", r2.commands));
        assert_eq!(r1.routed, r2.routed);
        assert_eq!(r1.unrouted, r2.unrouted);
    }

    /// A `Pinned` obstacle trace of *another* net, walling the direct path on Top,
    /// is avoided: the net still routes (dropping to Bottom through vias) and DRC is
    /// clean — clearance-clean *is* the proof it was avoided.
    #[test]
    fn pinned_obstacle_is_avoided() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance { path: "reg".into(), part: "LDO".into() },
            G::Instance { path: "dec".into(), part: "Cap".into() },
            G::Place { path: "reg".into(), pos: Point::mm(0, 0) },
            G::Place { path: "dec".into(), pos: Point::mm(12, 0) },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            // Single-pin net carrying a hand-routed wall (not itself routed).
            G::ConnectPins { net: "WALL".into(), pins: vec![("reg".into(), "VIN".into())] },
        ];
        let mut h = doc_of(src);
        // A Pinned wall on net WALL across x=6, full board height (on Top only).
        let wall = Trace {
            net: NetId::new("WALL"),
            layer: Layer::Top,
            path: vec![Point::mm(6, -10), Point::mm(6, 10)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        h.commit(Transaction::one(Command::AddTrace(TraceId(1), wall)), &lib, "wall").unwrap();

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(r.unrouted.is_empty(), "VBUS should route around/under the wall");
        // The detour around a full-height Top wall forces a layer change.
        assert!(
            r.commands.iter().any(|c| matches!(c, Command::AddVia(..))),
            "crossing a full-height Top wall should drop to Bottom via a via"
        );

        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(
            !has_clearance_or_width(&after),
            "routing around a Pinned obstacle must stay clearance-clean: {after:?}"
        );
        assert!(
            !after.iter().any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
            "VBUS must be fully routed: {after:?}"
        );
    }

    /// An intentionally impossible net (walled off on *both* layers) is reported as
    /// unrouted rather than producing bad copper: no commands for it, no new
    /// clearance/width violations, and DRC still flags it unrouted.
    #[test]
    fn impossible_net_is_reported_not_botched() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance { path: "reg".into(), part: "LDO".into() },
            G::Instance { path: "dec".into(), part: "Cap".into() },
            G::Place { path: "reg".into(), pos: Point::mm(0, 0) },
            G::Place { path: "dec".into(), pos: Point::mm(12, 0) },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins { net: "WALL".into(), pins: vec![("reg".into(), "VIN".into())] },
        ];
        let mut h = doc_of(src);
        // Walls on BOTH layers spanning beyond the board: no crossing on either layer.
        for (id, layer) in [(TraceId(1), Layer::Top), (TraceId(2), Layer::Bottom)] {
            let wall = Trace {
                net: NetId::new("WALL"),
                layer,
                path: vec![Point::mm(6, -12), Point::mm(6, 12)],
                width: 200_000,
                prov: Provenance::Pinned,
            };
            h.commit(Transaction::one(Command::AddTrace(id, wall)), &lib, "wall").unwrap();
        }

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(r.unrouted, vec![NetId::new("VBUS")], "VBUS is walled off both layers");
        assert!(r.commands.is_empty(), "a failed net must emit no copper, got {:?}", r.commands);

        // Applying nothing changes nothing; DRC still flags VBUS unrouted, no new DRC errors.
        let after = drc(&h);
        assert!(!has_clearance_or_width(&after), "no spurious clearance/width: {after:?}");
        assert!(
            after.iter().any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
            "VBUS should remain flagged unrouted: {after:?}"
        );
    }

    /// A multi-pin (3-pin) net connects all pins (MST-style) and passes the ratsnest.
    #[test]
    fn autoroute_three_pin_net() {
        let lib = part_library();
        // Three caps' p1 pads + reg.VOUT all on one net.
        let src = vec![
            board_rect(Point::mm(-6, -12), Point::mm(30, 12)),
            G::Instance { path: "reg".into(), part: "LDO".into() },
            G::Instance { path: "c0".into(), part: "Cap".into() },
            G::Instance { path: "c1".into(), part: "Cap".into() },
            G::Place { path: "reg".into(), pos: Point::mm(0, 0) },
            G::Place { path: "c0".into(), pos: Point::mm(12, 6) },
            G::Place { path: "c1".into(), pos: Point::mm(20, -6) },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![
                    ("reg".into(), "VOUT".into()),
                    ("c0".into(), "p1".into()),
                    ("c1".into(), "p1".into()),
                ],
            },
        ];
        let mut h = doc_of(src);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(r.unrouted.is_empty(), "3-pin net should fully route");
        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(after.is_empty(), "3-pin routed net must be DRC clean: {after:?}");
    }
}
