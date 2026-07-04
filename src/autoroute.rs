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
//! box) is discretised into a square routing grid over **all** copper layers of the
//! stackup. We A* over `(i, j, layer)` and change layer with a via, routing each net's
//! pins together incrementally (MST-style: each remaining pin is routed to the net's
//! existing connected copper). Obstacles are derived from [`crate::route::world_features`]
//! — the same unified stream DRC reads — so they are *honest*: real pad **extents**
//! (not points), other-net traces/vias on their true slabs, copper **pours** (`Area`
//! conductors), and hard `Role::Keepout` copper/route regions all map to blocked grid
//! cells on the slabs they occupy. Same-net copper is never blocked. Cells outside the
//! board region (or within the edge clearance of its boundary, including inside cutout
//! holes) are unroutable on every layer. A pin whose footprint carries **no** pad copper
//! (a bare terminal — the toy library, an unmodelled footprint) has no extent to stamp,
//! so its world point is added as a small point obstacle for *other* nets, preserving the
//! "don't run copper through another net's terminal" guarantee the old point model gave.
//!
//! ## Grid pitch, via legality, and clearance (the trace/via pitch split)
//!
//! The routing grid pitch is `min_trace_width + min_clearance` — fine enough to resolve
//! adjacent fine-pitch (e.g. 0.4 mm) pads, which the old `via_pad + clearance` pitch
//! could not. A **via** needs more room than a trace (its pad is wider), so via legality
//! is a *separate* per-cell mask, not a coarser grid: a via may be placed at a node only
//! if `via_pad + clearance` of room clears every obstacle there. Two adjacent grid nodes
//! used by different nets are `pitch` apart, which is exactly the trace↔trace clearance
//! floor; the wider via↔copper cases are handled by the via mask + the obstacle blocking
//! radii. As before this construction invariant is **not trusted** — [`verify_and_prune`]
//! re-checks the proposed copper against the real DRC and drops any net that clashes.
//!
//! ## Honest limitations (documented, by design)
//!
//! This is greedy net-by-net maze routing — deliberately basic. There is **no
//! rip-up-and-retry, no topological/push-and-shove, no length/impedance matching, no
//! net-ordering optimization, and no H/V per-layer directionality bias** (a per-layer
//! preferred-direction cost would slot into [`astar`]'s orthogonal step cost; it is not
//! built — issue 0008 owns that design cycle). Consequently **net ordering matters**: a
//! net that fails may well be routable in a different order. Failures are *reported* (the
//! net goes in `unrouted`), never fatal and never emitted as partial/overlapping copper.
//! Vias are always through (full copper extent, `span: None`); blind/buried vias are out
//! of scope. All geometry is integer nm; everything is deterministic.

use crate::command::Command;
use crate::doc::{Doc, Nm, PinRef, Point, Provenance};
use crate::elaborate::stackup;
use crate::geom::{Extent, Feature, KeepoutKind, NetFeature, Role, Shape2D, Stackup, ZRange};
use crate::id::{NetId, TraceId, ViaId};
use crate::part::{PartLib, PinRole, pin_world};
use crate::route::{
    DesignRules, Layer, Trace, Via, copper_layers_z, layer_slab_name, world_features,
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

/// Propose a routing transaction for `doc`. Pure: reads facts, returns commands.
pub fn autoroute(doc: &Doc, lib: &PartLib, rules: &DesignRules) -> AutorouteResult {
    let width = rules.min_trace_width;
    let via_pad = 2 * rules.min_trace_width;
    let via_drill = rules.min_trace_width;
    // The trace/via pitch split (issue 0003): the grid is as fine as a trace + its
    // clearance needs, *not* as coarse as a via — so adjacent fine-pitch pads resolve.
    // Via legality is a separate per-cell mask (see `BlockMap::via`).
    let pitch = rules.min_trace_width + rules.min_clearance;
    // A via pad's centre must keep this far from any *other* net's trace centreline
    // (edge-to-edge = via_pad/2 + width/2 + clearance). Enforced during A* via placement.
    let via_clear = rules.min_clearance + via_pad / 2 + width / 2;

    // The stackup fixes the layer count and the slab name ↔ ordinal bridge. A stackup
    // with no copper cannot be routed. `layers[k]` is the k-th copper slab top-down
    // (0 = Top, last = Bottom), matching `route::copper_layers_z` / `Layer::depth`.
    let su = stackup(&doc.source);
    let layers: Vec<Layer> = copper_layers_z(&su).into_iter().map(|(l, _)| l).collect();
    if layers.is_empty() {
        return AutorouteResult::default();
    }
    let layer_names: Vec<String> = layers
        .iter()
        .map(|&l| layer_slab_name(&su, l).unwrap_or_default())
        .collect();
    if layer_names.iter().any(String::is_empty) {
        return AutorouteResult::default();
    }
    let nl = layers.len();

    // World pad positions and per-net *layer occupancy* for every net's pins. A pad
    // participates on the copper slabs its geometry actually touches (an SMD pad on one,
    // a through-hole pad on all); a pin with no resolvable world position is dropped.
    // Occupancy drives seeding — an SMD pad seeds only on its own layer.
    let netlist = doc_netlist(doc);
    let mut net_pads: BTreeMap<NetId, Vec<Pad>> = BTreeMap::new();
    for (nid, net) in &doc.nets {
        let mut pads = Vec::new();
        for pr in &net.members {
            if let Some(c) = doc.components.get(&pr.comp)
                && let Some(def) = lib.get(&c.part)
                && let Some(p) = pin_world(c, def, &pr.pin)
            {
                let (pad_layers, has_copper) = pad_layers(doc, lib, &su, pr, &layers);
                pads.push(Pad {
                    at: p,
                    layers: pad_layers,
                    has_copper,
                });
            }
        }
        net_pads.insert(nid.clone(), pads);
    }

    // Routing area: the source Board outline, else the bounding box of all pads.
    let Some(area) = routing_area(doc, &net_pads, pitch) else {
        return AutorouteResult::default();
    };
    let grid = Grid::new(area, pitch, nl);
    if grid.cols == 0 || grid.rows == 0 {
        return AutorouteResult::default();
    }

    // Cells masked out by the board: outside the outline (or in a cutout hole), or within
    // the edge clearance of any boundary. Unroutable on *all* layers, shared across nets.
    let board_mask = BoardMask::build(doc, &grid, rules, width);

    // Ownership of each (node, layer) by a net (its routed copper passes through).
    // -1 = free. Distinct nets never share a node ⇒ trace↔trace clearance falls out of
    // `pitch`. Flat `cols*rows*nl`, layer-minor.
    let mut owner = vec![-1i32; grid.cols * grid.rows * nl];

    // Id minting: continue past any ids already in the doc (caller-assigned, like KiCad
    // UUIDs — a hand edit and the autorouter mint the same way).
    let mut next_tid = doc.traces.keys().map(|t| t.0 + 1).max().unwrap_or(1);
    let mut next_vid = doc.vias.keys().map(|v| v.0 + 1).max().unwrap_or(1);

    let mut result = AutorouteResult::default();

    // Route net by net, in NetId order (deterministic). A net seq id tags ownership.
    for (net_seq, (nid, pads)) in net_pads.iter().enumerate() {
        // Nets with <2 reachable pins are trivially "routed" (nothing to connect).
        if pads.len() < 2 {
            continue;
        }

        // Per-net obstacle map derived from the honest world-feature stream: every
        // *other* net's copper (pads/traces/vias/pours) plus copper/route keep-outs, plus
        // the point terminals of foreign pins that carry no pad copper.
        let block = BlockMap::build(
            &grid,
            &board_mask,
            doc,
            lib,
            rules,
            &su,
            &netlist,
            &net_pads,
            nid,
            width,
            via_pad,
        );

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
            via_clear,
            &layer_names,
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
    // real DRC and drop any net that actually clashes.
    verify_and_prune(doc, lib, rules, &mut result);
    result
}

/// A pad: its world centre, the copper layer ordinals its geometry occupies, and whether
/// it actually carries pad copper (a bare terminal has none — see [`pad_layers`]).
struct Pad {
    at: Point,
    layers: Vec<usize>,
    has_copper: bool,
}

/// The membership-only netlist a `Doc` carries (roles are irrelevant to clearance /
/// world_features, so a `Passive` placeholder is fine).
fn doc_netlist(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|m| (m.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}

/// The grid-layer ordinals a pin's pad copper occupies, and whether it carries any pad
/// copper at all. Scan the pad's conductor features and match each feature's slab z to a
/// copper slab, returning its ordinal: an SMD pad matches one layer; a through-hole pad
/// matches every copper slab. A pin with **no** pad copper (a bare connection point — the
/// toy library's parts, or a footprint whose pads the stackup lacks) is layer-agnostic,
/// so it may seed on *any* layer (mirroring the ratsnest, which treats pads as all-layer
/// points) — the returned `has_copper` is `false` so the caller instead adds it as a
/// point obstacle for other nets.
fn pad_layers(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
    pr: &PinRef,
    layers: &[Layer],
) -> (Vec<usize>, bool) {
    let all = || (0..layers.len()).collect::<Vec<_>>();
    let cu = su.copper_slabs();
    let Some(c) = doc.components.get(&pr.comp) else {
        return (all(), false);
    };
    let Some(def) = lib.get(&c.part) else {
        return (all(), false);
    };
    let Some(pin) = def.pins.iter().find(|p| p.number == pr.pin) else {
        return (all(), false);
    };
    let mut out = Vec::new();
    for f in pin.pad_features(c, su) {
        if f.role != Role::Conductor {
            continue;
        }
        let Extent::Prism { z, .. } = &f.extent;
        if let Some(slab) = cu.iter().find(|s| s.z == *z)
            && let Some(k) = layers
                .iter()
                .position(|&l| layer_slab_name(su, l).as_deref() == Some(slab.name.as_str()))
            && !out.contains(&k)
        {
            out.push(k);
        }
    }
    if out.is_empty() {
        return (all(), false);
    }
    out.sort_unstable();
    (out, true)
}

/// Self-honesty (issue 0003): the grid's "clearance-clean by construction" invariant
/// fails at sub-grid pitch (and never covered the off-grid pad stubs), so do not
/// trust it. Check each *proposed* piece of copper against all other-net copper
/// (existing pads / pre-existing traces+vias + other proposed copper) with the same
/// `geom` clearance DRC uses, and additionally against copper/route keep-outs and the
/// board edge (issue 0023 — the router now masks these out during search, so a clash
/// here means a construction slip; checking it makes `routed` mean *DRC-clean*, not
/// merely copper-vs-copper clean). Any routed net whose proposed copper clashes is
/// dropped — its commands removed, the net moved to `unrouted`. Dropping every clashing
/// net is conservative (it can drop a net that a smarter order/rip-up would keep — that
/// is future work, issue 0008); the point here is honesty, not optimality.
///
/// Decision 19a — the pours re-derive with the proposed copper included. The proposed
/// commands are applied to a scratch `Doc` clone and `world_features` is re-run on it, so
/// the pour fills this check sees have **already retreated** around the proposed vias/
/// traces (the automatic anti-pad). A via punched through a foreign plane verifies clean
/// because the re-derived fill is no longer under it; a via too close to another net's
/// *non-pour* copper (a trace/pad/via) still fails. Pour-vs-solid is therefore skipped
/// here exactly as [`crate::route::check_drc`] skips it — the pour was knocked out at this
/// same `min_clearance`, so any residual proximity is tessellation slop, not a real short.
/// Cost: one extra `world_features` pass (one re-derivation of every pour) per autoroute
/// call — the pour boolean ops dominate; measured acceptable on the PoC (see the branch
/// report).
fn verify_and_prune(doc: &Doc, lib: &PartLib, rules: &DesignRules, result: &mut AutorouteResult) {
    let su = stackup(&doc.source);
    let cu = su.copper_slabs();

    // This run's proposed copper, lowered the same way `net_features` lowers the doc's:
    // a trace is one Conductor prism on its named slab; a via fans out to one prism per
    // copper slab it spans (so every feature is single-slab). Kept as the by-net set we
    // judge; the re-derived `world` below is the obstacle field it is judged against.
    let mut proposed: Vec<NetFeature> = Vec::new();
    for cmd in &result.commands {
        match cmd {
            Command::AddTrace(_, t) => {
                if let Some(z) = cu.iter().find(|s| s.name == t.layer).map(|s| s.z) {
                    let f =
                        Feature::prism(Role::Conductor, Shape2D::trace(t.path.clone(), t.width), z);
                    proposed.push(NetFeature::new(Some(t.net.clone()), f));
                }
            }
            Command::AddVia(_, v) => {
                for s in v.spanned_slabs(&cu) {
                    let f = Feature::prism(Role::Conductor, Shape2D::disc(v.at, v.pad / 2), s.z);
                    proposed.push(NetFeature::new(Some(v.net.clone()), f));
                }
            }
            _ => {}
        }
    }

    // Apply the proposed commands to a scratch clone and re-derive the world from it: the
    // pours retreat around the proposed copper (Decision 19a), so `world` is the fill that
    // will actually exist post-commit — not the stale pre-route fill. `command::apply` is
    // the same atomic path a real commit takes; on the rare chance the transaction is
    // rejected (it should not be — these are ordinary AddTrace/AddVia) fall back to the
    // pre-route world, which is stricter (stale fill blocks more), never falsely clean.
    let txn = crate::command::Transaction(result.commands.clone());
    let scratch = crate::command::apply(doc, &txn, lib, 0).unwrap_or_else(|_| doc.clone());
    let world = world_features(&scratch, lib, &doc_netlist(&scratch), rules, &su)
        .expect("world_features on a committed doc (slab gate enforced at commit)");

    // Keep-out features (copper/route kinds only) and the board substrate region, for the
    // keepout + edge checks that make `routed` mean DRC-clean.
    let keepouts: Vec<&Feature> = world
        .iter()
        .filter(|nf| {
            matches!(
                nf.feature.role,
                Role::Keepout(KeepoutKind::Copper | KeepoutKind::Route)
            )
        })
        .map(|nf| &nf.feature)
        .collect();
    let substrate: Option<&crate::region::Region> = world.iter().find_map(|nf| {
        if nf.feature.role != Role::Substrate {
            return None;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        shape.region()
    });

    // Is a conductor feature a derived pour (the `is_pour ⟺ Shape2D::Area` invariant,
    // Decision 16)? Pour-vs-solid is skipped (the pour retreated at `min_clearance`).
    let is_pour = |f: &Feature| matches!(&f.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. }));

    let mut unclean: BTreeSet<NetId> = BTreeSet::new();
    for p in &proposed {
        let Some(pnet) = &p.net else { continue };
        if unclean.contains(pnet) {
            continue;
        }
        // Copper-vs-copper against the re-derived world (which already contains the
        // proposed copper, so proposed-vs-proposed different-net clashes are caught here
        // too). A proposed solid piece is skipped against foreign *pours*, exactly as
        // `check_drc` skips pour-vs-solid: the re-derived fill retreated around it at this
        // same `min_clearance` (Decision 19a's automatic anti-pad), so any residual
        // proximity is tessellation slop of the circle-approximated fill boundary, not a
        // real short — checking it would false-positive and prune legitimate vias-in-plane
        // (confirmed: removing this skip prunes `via_inside_foreign_plane_verifies_clean`).
        // The via is still checked against foreign SOLID copper, keep-outs, and the edge.
        let copper_clash = world.iter().any(|o| {
            o.feature.role == Role::Conductor
                && o.net.as_ref() != Some(pnet)
                && !is_pour(&o.feature)
                && !p.feature.clears(&o.feature, rules.min_clearance)
        });
        // Keep-out intrusion (z-gated by `Feature::clears`).
        let keepout_clash = keepouts
            .iter()
            .any(|kf| !p.feature.clears(kf, rules.keepout_clearance));
        // Board-edge: solid copper grown by the edge rule must stay inside the board.
        let edge_clash = substrate.is_some_and(|board| {
            let Extent::Prism { shape, .. } = &p.feature.extent;
            let grown = crate::region::shape_to_region(
                &shape.inflated(rules.edge_clearance),
                crate::region::DEFAULT_CIRCLE_SEGS,
            );
            !crate::region::difference(&grown, board).is_empty()
        });
        if copper_clash || keepout_clash || edge_clash {
            unclean.insert(pnet.clone());
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
/// pins have room. The grid spans the bbox; the [`BoardMask`] then carves it to the
/// real (non-rectangular, cutout-holed) outline.
fn routing_area(doc: &Doc, net_pads: &BTreeMap<NetId, Vec<Pad>>, pitch: Nm) -> Option<Rect> {
    if let Some(region) = crate::elaborate::board_region(&doc.source)
        && let Some((min, max)) = region.bbox()
    {
        return Some(Rect { min, max });
    }
    let mut it = net_pads.values().flatten().map(|p| p.at);
    let first = it.next()?;
    let (mut min, mut max) = (first, first);
    for p in net_pads.values().flatten().map(|p| p.at) {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    let m = 2 * pitch;
    Some(Rect {
        min: Point {
            x: min.x - m,
            y: min.y - m,
        },
        max: Point {
            x: max.x + m,
            y: max.y + m,
        },
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
    layers: usize,
}

impl Grid {
    fn new(area: Rect, pitch: Nm, layers: usize) -> Grid {
        let cols = ((area.max.x - area.min.x) / pitch).max(0) as usize + 1;
        let rows = ((area.max.y - area.min.y) / pitch).max(0) as usize + 1;
        Grid {
            origin: area.min,
            pitch,
            cols,
            rows,
            layers,
        }
    }
    fn world(&self, i: usize, j: usize) -> Point {
        Point {
            x: self.origin.x + i as Nm * self.pitch,
            y: self.origin.y + j as Nm * self.pitch,
        }
    }
    fn idx(&self, i: usize, j: usize) -> usize {
        j * self.cols + i
    }
    fn cells(&self) -> usize {
        self.cols * self.rows
    }
    /// Flat index into a `cells * layers` array (layer-minor).
    fn lidx(&self, i: usize, j: usize, l: usize) -> usize {
        self.idx(i, j) * self.layers + l
    }
    /// The inclusive cell index box covering world bbox `(lo, hi)` grown by `margin`,
    /// clamped to the grid — the scan window for stamping one obstacle.
    fn bbox_range(&self, lo: Point, hi: Point, margin: Nm) -> (usize, usize, usize, usize) {
        let clampi =
            |v: Nm| ((v - self.origin.x) / self.pitch).clamp(0, self.cols as Nm - 1) as usize;
        let clampj =
            |v: Nm| ((v - self.origin.y) / self.pitch).clamp(0, self.rows as Nm - 1) as usize;
        // ±one cell of slop is fine — the exact distance test inside decides membership.
        (
            clampi(lo.x - margin - self.pitch),
            clampi(hi.x + margin + self.pitch),
            clampj(lo.y - margin - self.pitch),
            clampj(hi.y + margin + self.pitch),
        )
    }
}

// ----------------------------------------------------------------------------
// Board masking: cells outside the board (or too near an edge) are unroutable.
// ----------------------------------------------------------------------------

/// Cells the board itself forbids on every layer: a node outside the board region
/// (outline ∖ cutouts) or within `edge_clearance + half_width + half_edge` of its
/// boundary. Shared across nets (the board doesn't change per net).
struct BoardMask {
    blocked: Vec<bool>,
}

impl BoardMask {
    fn build(doc: &Doc, grid: &Grid, rules: &DesignRules, width: Nm) -> BoardMask {
        let mut blocked = vec![false; grid.cells()];
        let Some(board) = crate::elaborate::board_region(&doc.source) else {
            // No outline ⇒ nothing to mask (the pad-bbox area is the routable region).
            return BoardMask { blocked };
        };
        // The boundary edges (outer ring + cutout walls) as segments, once.
        let edges: Vec<(Point, Point)> = board
            .rings
            .iter()
            .filter(|r| r.len() >= 2)
            .flat_map(|r| (0..r.len()).map(move |k| (r[k], r[(k + 1) % r.len()])))
            .collect();
        // A trace of `width` at a node stays `edge_clearance` clear only if the node is
        // `edge_clearance + width/2` from any wall; the half-edge slop covers the edges
        // leaving the node.
        let pull = rules.edge_clearance + width / 2 + grid.pitch / 2;
        for j in 0..grid.rows {
            for i in 0..grid.cols {
                let w = grid.world(i, j);
                let inside = board.contains_point(w);
                let near_edge = edges.iter().any(|(a, b)| within(w, *a, *b, pull));
                if !inside || near_edge {
                    blocked[grid.idx(i, j)] = true;
                }
            }
        }
        BoardMask { blocked }
    }
    fn blocked(&self, grid: &Grid, i: usize, j: usize) -> bool {
        self.blocked[grid.idx(i, j)]
    }
}

// ----------------------------------------------------------------------------
// Obstacles → blocked cells (honest, from world_features).
// ----------------------------------------------------------------------------

/// Per-net precomputed blocked-cell map. `trace[idx*nl + l]` = a trace of the current
/// net may not occupy that node on layer `l`; `via[idx]` = a via may not be placed there
/// (a via needs `via_pad + clearance` of room; since a through via touches every layer,
/// the via mask is one per-cell test). Sized so a node *and the half-edges leaving it*
/// stay clearance-clean, and starting from the board mask.
///
/// `via_layer[idx*nl + l]` is the per-layer room test the through-via all-layer check
/// (`via_ok`) consults instead of `trace`: it is `trace` **minus** the via-permeable
/// foreign-pour stamps (Decision 19a). A via barrel may touch a layer where only a
/// foreign derived pour sits (the plane retreats around it), but not one carrying solid
/// non-pour copper, a keep-out, a void, or the board mask — all of which stamp both
/// `trace` and `via_layer`. So `trace` gates trace routing (pours block traces) while
/// `via_layer` gates the via barrel's per-layer room (pours do not block vias).
struct BlockMap {
    trace: Vec<bool>,
    via: Vec<bool>,
    via_layer: Vec<bool>,
}

impl BlockMap {
    #[allow(clippy::too_many_arguments)]
    fn build(
        grid: &Grid,
        board: &BoardMask,
        doc: &Doc,
        lib: &PartLib,
        rules: &DesignRules,
        su: &Stackup,
        netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
        net_pads: &BTreeMap<NetId, Vec<Pad>>,
        cur: &NetId,
        width: Nm,
        via_pad: Nm,
    ) -> BlockMap {
        let cells = grid.cells();
        let nl = grid.layers;
        let mut trace = vec![false; cells * nl];
        let mut via = vec![false; cells];
        // `via_layer` mirrors `trace` but excludes via-permeable pour stamps; the board
        // mask blocks a via barrel on every layer (it needs room outside the board), so
        // it stamps both.
        let mut via_layer = vec![false; cells * nl];
        // The board mask blocks the cell on every layer, and a via there too (it needs
        // room on all layers).
        for j in 0..grid.rows {
            for i in 0..grid.cols {
                if board.blocked(grid, i, j) {
                    let idx = grid.idx(i, j);
                    via[idx] = true;
                    for l in 0..nl {
                        trace[idx * nl + l] = true;
                        via_layer[idx * nl + l] = true;
                    }
                }
            }
        }

        let clr = rules.min_clearance;
        let half_edge = grid.pitch / 2; // a routed edge reaches a neighbour `pitch` away

        // The honest obstacle stream: every different-net copper conductor on its true
        // slab, plus copper/route keep-outs. Pours are `Area` conductors here — treated
        // exactly like solid copper (no special case). We rasterize each onto the grid.
        let world = world_features(doc, lib, netlist, rules, su)
            .expect("world_features on a committed doc (slab gate enforced at commit)");
        let cu = su.copper_slabs();
        let slab_ord = |z: &ZRange| -> Option<usize> { cu.iter().position(|s| s.z == *z) };

        for nf in &world {
            let f = &nf.feature;
            match f.role {
                Role::Conductor => {
                    if nf.net.as_ref() == Some(cur) {
                        continue; // same-net copper is not an obstacle
                    }
                    let Extent::Prism { shape, z } = &f.extent;
                    let Some(l) = slab_ord(z) else { continue };
                    // Decision 19a: a foreign *derived pour* fill (the `is_pour ⟺
                    // Shape2D::Area` invariant, Decision 16) is via-permeable. It still
                    // blocks TRACE placement on its slab — planes are not signal layers
                    // this round — but a via may punch through it: the plane retreats
                    // around the barrel at re-derivation (the anti-pad is automatic), so
                    // the momentary fill is not a wall for vias. Authored/routed (non-pour)
                    // copper — a disc/rect/trace shape — still blocks vias as before.
                    let via_permeable = matches!(shape, Shape2D::Area { .. });
                    Self::stamp(
                        grid,
                        &mut trace,
                        &mut via,
                        &mut via_layer,
                        nl,
                        shape,
                        Some(l),
                        clr,
                        width,
                        via_pad,
                        half_edge,
                        via_permeable,
                    );
                }
                Role::Keepout(KeepoutKind::Copper | KeepoutKind::Route) => {
                    // A copper/route keep-out is a hard block on every copper slab its z
                    // overlaps (netless ⇒ blocks all nets). If it overlaps no copper slab,
                    // block all layers conservatively.
                    //
                    // NOTE: `stamp` uses `clr` (= min_clearance) for the keep-out pull-back,
                    // whereas the DRC/verify gate checks keep-outs at `keepout_clearance`
                    // (default 0). With the defaults (keepout_clearance 0 ≤ min_clearance)
                    // the grid is *stricter* than verify — the safe direction (the router
                    // over-avoids; verify never rejects a route the grid allowed). This
                    // becomes unsafe only if `keepout_clearance` is ever set *larger* than
                    // `min_clearance`: the grid would then under-pull-back and verify could
                    // drop routes the grid thought clean. If that config arises, pass
                    // `keepout_clearance.max(min_clearance)` here.
                    let Extent::Prism { shape, z } = &f.extent;
                    let mut any = false;
                    for (k, s) in cu.iter().enumerate() {
                        if s.z.overlaps(z) {
                            Self::stamp(
                                grid,
                                &mut trace,
                                &mut via,
                                &mut via_layer,
                                nl,
                                shape,
                                Some(k),
                                clr,
                                width,
                                via_pad,
                                half_edge,
                                false,
                            );
                            any = true;
                        }
                    }
                    if !any {
                        Self::stamp(
                            grid,
                            &mut trace,
                            &mut via,
                            &mut via_layer,
                            nl,
                            shape,
                            None,
                            clr,
                            width,
                            via_pad,
                            half_edge,
                            false,
                        );
                    }
                }
                Role::Void => {
                    // A through-cut Void (an authored NPTH mounting/tooling hole, or a
                    // via/pad drill) removes copper on *every* layer, so no routed copper
                    // may run over it — a hard all-layer block (issue 0025's routing side:
                    // `board_region` only subtracts `Cutout`, not `hole`, so holes reach
                    // the router only through this Void arm). Keeping `clr` of room is a
                    // conservative stand-in for a true hole edge-clearance rule.
                    let Extent::Prism { shape, .. } = &f.extent;
                    Self::stamp(
                        grid,
                        &mut trace,
                        &mut via,
                        &mut via_layer,
                        nl,
                        shape,
                        None,
                        clr,
                        width,
                        via_pad,
                        half_edge,
                        false,
                    );
                }
                _ => {}
            }
        }

        // Foreign bare-pin terminals: a pin whose footprint carries no pad copper emits no
        // world feature, so stamp its world point as a small obstacle for *other* nets on
        // the layers it occupies — preserving the "don't route through another net's
        // terminal" guarantee. (A pad *with* copper already blocked via its extent above.)
        for (nid, pads) in net_pads {
            if nid == cur {
                continue;
            }
            for p in pads {
                if p.has_copper {
                    continue;
                }
                let dot = Shape2D::disc(p.at, 0);
                for &l in &p.layers {
                    Self::stamp(
                        grid,
                        &mut trace,
                        &mut via,
                        &mut via_layer,
                        nl,
                        &dot,
                        Some(l),
                        clr,
                        width,
                        via_pad,
                        half_edge,
                        false,
                    );
                }
            }
        }
        BlockMap {
            trace,
            via,
            via_layer,
        }
    }

    /// Stamp one obstacle `shape` onto the grid: block the trace mask on layer `l` (or all
    /// layers if `l` is `None`) for nodes whose routed copper would come within `clr` of
    /// the obstacle, and likewise the via mask (all-layer). The thresholds passed to
    /// [`crate::geom::clearance_violated`] are `clr + width/2 + half_edge` (trace) and
    /// `clr + via_pad/2 + half_edge` (via); `clearance_violated` adds the *obstacle's* own
    /// radius on top, so the effective edge-to-edge block distance is
    /// `clr + our_half + obstacle_half + half_edge` — the half-edge slop covering the
    /// routed edge that reaches a neighbour `pitch` away. Scans only the obstacle's grown
    /// bbox, so a small pad on a big board is cheap.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn stamp(
        grid: &Grid,
        trace: &mut [bool],
        via: &mut [bool],
        via_layer: &mut [bool],
        nl: usize,
        shape: &Shape2D,
        l: Option<usize>,
        clr: Nm,
        width: Nm,
        via_pad: Nm,
        half_edge: Nm,
        via_permeable: bool,
    ) {
        let trace_thr = clr + width / 2 + half_edge;
        let via_thr = clr + via_pad / 2 + half_edge;
        let Some((lo, hi)) = shape.bbox() else { return };
        let (imin, imax, jmin, jmax) = grid.bbox_range(lo, hi, trace_thr.max(via_thr));
        for j in jmin..=jmax {
            for i in imin..=imax {
                let node = Shape2D::disc(grid.world(i, j), 0);
                let idx = grid.idx(i, j);
                if crate::geom::clearance_violated(shape, &node, trace_thr) {
                    match l {
                        Some(l) => {
                            trace[idx * nl + l] = true;
                            // The via barrel's per-layer room test skips via-permeable
                            // pours (Decision 19a) — only solid copper blocks it.
                            if !via_permeable {
                                via_layer[idx * nl + l] = true;
                            }
                        }
                        None => {
                            for ll in 0..nl {
                                trace[idx * nl + ll] = true;
                                if !via_permeable {
                                    via_layer[idx * nl + ll] = true;
                                }
                            }
                        }
                    }
                }
                // A via-permeable obstacle (a foreign derived-pour fill, Decision 19a)
                // does not block the via-site mask — the plane retreats around the barrel
                // on re-derivation. Only solid (non-pour) copper stamps the via mask.
                if !via_permeable && crate::geom::clearance_violated(shape, &node, via_thr) {
                    via[idx] = true;
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------
// Per-net maze routing.
// ----------------------------------------------------------------------------

type State = (usize, usize, usize); // (i, j, layer)
/// A polyline run on one layer (world points), as produced from an A* path; the `usize`
/// is the layer ordinal.
type Run = (usize, Vec<Point>);

#[allow(clippy::too_many_arguments)]
fn route_net(
    grid: &Grid,
    block: &BlockMap,
    owner: &mut [i32],
    net_seq: i32,
    nid: &NetId,
    pads: &[Pad],
    width: Nm,
    via_pad: Nm,
    via_drill: Nm,
    via_clear: Nm,
    layer_names: &[String],
    next_tid: &mut u64,
    next_vid: &mut u64,
) -> Option<Vec<Command>> {
    // Map each pad to the nearest grid node the current net may occupy, on one of the
    // layers the pad exists on (an SMD pad seeds only on its own layer).
    let mut pin_nodes: Vec<State> = Vec::with_capacity(pads.len());
    for p in pads {
        pin_nodes.push(nearest_routable(grid, block, owner, net_seq, p)?);
    }

    // Claim list for rollback if any pin fails (so a partial net blocks no one).
    let mut claimed: Vec<usize> = Vec::new();
    let claim = |owner: &mut [i32], i: usize, j: usize, l: usize, claimed: &mut Vec<usize>| {
        let idx = grid.lidx(i, j, l);
        if owner[idx] != net_seq {
            owner[idx] = net_seq;
            claimed.push(idx);
        }
    };

    let mut commands: Vec<Command> = Vec::new();

    // Seed the connected tree at pin 0; stub its pad onto its grid node.
    let (si, sj, sl) = pin_nodes[0];
    claim(owner, si, sj, sl, &mut claimed);
    let seed_world = grid.world(si, sj);
    if seed_world != pads[0].at {
        commands.push(Command::AddTrace(
            TraceId(mint(next_tid)),
            Trace {
                net: nid.clone(),
                layer: layer_names[sl].clone(),
                path: vec![pads[0].at, seed_world],
                width,
                prov: Provenance::Free,
            },
        ));
    }
    // The set of (node, layer) currently in the net's connected copper.
    let mut tree: Vec<State> = vec![(si, sj, sl)];

    // Route each remaining pin to the existing tree.
    for k in 1..pin_nodes.len() {
        let goal = pin_nodes[k];
        let Some(path) = astar(grid, block, owner, net_seq, &tree, goal, via_clear) else {
            for idx in claimed {
                owner[idx] = -1;
            }
            return None;
        };

        for &(i, j, l) in &path {
            claim(owner, i, j, l, &mut claimed);
            tree.push((i, j, l));
        }

        // Convert the grid path into per-layer trace runs + via points, appending the
        // goal pad onto the final run so the trace literally touches the pad.
        let (runs, vias) = path_to_runs(grid, &path);
        for (vi, vj) in vias {
            commands.push(Command::AddVia(
                ViaId(mint(next_vid)),
                Via {
                    net: nid.clone(),
                    at: grid.world(vi, vj),
                    // A through via — the full copper extent (Decision 18's default).
                    // Blind/buried is out of scope; the grid blocked the via site on every
                    // layer, which is honest for a through barrel.
                    span: None,
                    drill: via_drill,
                    pad: via_pad,
                    prov: Provenance::Free,
                },
            ));
        }
        let last = runs.len().saturating_sub(1);
        for (ri, (layer, mut pts)) in runs.into_iter().enumerate() {
            if ri == last {
                pts.push(pads[k].at); // stub onto the goal pad
            }
            let pts = coalesce(pts);
            if pts.len() >= 2 {
                commands.push(Command::AddTrace(
                    TraceId(mint(next_tid)),
                    Trace {
                        net: nid.clone(),
                        layer: layer_names[layer].clone(),
                        path: pts,
                        width,
                        prov: Provenance::Free,
                    },
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

/// Nearest grid node to `p.at` that the current net may occupy, on one of the copper
/// layers the pad exists on (deterministic: scans in fixed layer→row→col order, picks
/// min squared distance, ties by scan order). Seeding on the pad's own layer keeps a
/// surface pad's stub honest; a via at the seed lets the router reach other layers.
fn nearest_routable(
    grid: &Grid,
    block: &BlockMap,
    owner: &[i32],
    net_seq: i32,
    p: &Pad,
) -> Option<State> {
    let mut best: Option<(State, i128)> = None;
    for &l in &p.layers {
        for j in 0..grid.rows {
            for i in 0..grid.cols {
                let idx = grid.idx(i, j);
                let lidx = grid.lidx(i, j, l);
                if block.trace[idx * grid.layers + l]
                    || (owner[lidx] != -1 && owner[lidx] != net_seq)
                {
                    continue;
                }
                let w = grid.world(i, j);
                let dx = (w.x - p.at.x) as i128;
                let dy = (w.y - p.at.y) as i128;
                let d2 = dx * dx + dy * dy;
                if best.is_none_or(|(_, bd)| d2 < bd) {
                    best = Some(((i, j, l), d2));
                }
            }
        }
    }
    best.map(|(s, _)| s)
}

/// A* over `(i, j, layer)` from any node in `tree` (multi-source) to `goal` (a specific
/// node+layer). Orthogonal steps cost one pitch; a via step to an *adjacent* layer costs
/// a via penalty (so a full N-layer through-hop costs proportionally more than a single
/// layer change). Deterministic: the frontier orders by `(f, i, j, layer)`.
fn astar(
    grid: &Grid,
    block: &BlockMap,
    owner: &[i32],
    net_seq: i32,
    tree: &[State],
    goal: State,
    via_clear: Nm,
) -> Option<Vec<State>> {
    let pitch = grid.pitch;
    let via_pen = 10 * pitch; // strongly prefer staying on one layer (fewer vias)
    let cells = grid.cells();
    let nl = grid.layers;
    let sidx = |s: State| grid.lidx(s.0, s.1, s.2);
    // A via pad must keep `via_clear` from any foreign copper centreline. Same-run copper
    // of *other* nets lives in `owner` (the committed obstacles are already in `block.via`),
    // so a via is illegal if any node within `via_clear` is owned by another net. Scan the
    // Chebyshev box of that radius and test the exact Euclidean distance.
    //
    // This is a NODE approximation of the true via-to-segment distance: it measures the via
    // centre to owned grid *nodes*, not to the foreign net's trace *segments*. A via sitting
    // near a foreign segment's midpoint (whose endpoints are both > via_clear away, so no
    // owned node is within the ring) can slip through here while actually inside via_clear of
    // the centreline. That is deliberate — the exact, segment-accurate backstop is
    // `verify_and_prune`, which re-checks every proposed via against the real DRC geometry
    // and drops the net if it clashes. This ring check is a cheap search-time prune that
    // catches the common axis-aligned adjacency (a via one node from a foreign trunk); the
    // rare oblique near-miss is caught by verify. `routed` is therefore never a false clean.
    let ring = (via_clear / pitch) as usize + 1;
    let vc2 = (via_clear as i128) * (via_clear as i128);

    let mut g = vec![i64::MAX; cells * nl];
    let mut came: Vec<Option<State>> = vec![None; cells * nl];
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
        let lidx = grid.lidx(i, j, l);
        !block.trace[idx * nl + l] && (owner[lidx] == -1 || owner[lidx] == net_seq)
    };
    // The per-layer room a through-via *barrel* needs (Decision 19a): identical to
    // `passable` except it consults `via_layer` (which excludes via-permeable foreign
    // pours) rather than `trace`. A via may punch a foreign plane — the plane retreats —
    // but not sit where solid foreign copper, a keep-out, a void, the board mask, or
    // another same-run net's copper occupies the layer.
    let via_passable = |i: usize, j: usize, l: usize| -> bool {
        let idx = grid.idx(i, j);
        let lidx = grid.lidx(i, j, l);
        !block.via_layer[idx * nl + l] && (owner[lidx] == -1 || owner[lidx] == net_seq)
    };
    // A through via at (i,j) is legal only if the site clears via room (committed
    // obstacles, in `block.via`), has via-barrel room on *every* copper layer (the barrel
    // touches all of them), and keeps `via_clear` from any *other* net's same-run copper.
    let via_ok = |i: usize, j: usize| -> bool {
        let idx = grid.idx(i, j);
        if block.via[idx] || !(0..nl).all(|l| via_passable(i, j, l)) {
            return false;
        }
        let c = grid.world(i, j);
        let lo_i = i.saturating_sub(ring);
        let hi_i = (i + ring).min(grid.cols - 1);
        let lo_j = j.saturating_sub(ring);
        let hi_j = (j + ring).min(grid.rows - 1);
        for nj in lo_j..=hi_j {
            for ni in lo_i..=hi_i {
                if ni == i && nj == j {
                    continue;
                }
                let foreign = (0..nl).any(|l| {
                    let o = owner[grid.lidx(ni, nj, l)];
                    o != -1 && o != net_seq
                });
                if !foreign {
                    continue;
                }
                let w = grid.world(ni, nj);
                let dx = (w.x - c.x) as i128;
                let dy = (w.y - c.y) as i128;
                if dx * dx + dy * dy < vc2 {
                    return false;
                }
            }
        }
        true
    };

    while let Some(Reverse((f, i, j, l))) = heap.pop() {
        let s = (i, j, l);
        let gs = g[sidx(s)];
        if f > gs.saturating_add(h(i, j)) {
            continue; // stale
        }
        if s == goal {
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
        // Via moves to the adjacent layer(s): one hop per crossed layer (so a deep hop
        // costs proportionally). A through via touches every copper layer, so the site
        // must clear via room on all of them (`via_ok`).
        if via_ok(i, j) {
            if l + 1 < nl {
                nbrs.push((i, j, l + 1, via_pen));
            }
            if l > 0 {
                nbrs.push((i, j, l - 1, via_pen));
            }
        }

        for (ni, nj, nlyr, step) in nbrs {
            if nlyr == l && !passable(ni, nj, nlyr) {
                continue;
            }
            let ns = (ni, nj, nlyr);
            let ng = gs.saturating_add(step);
            if ng < g[sidx(ns)] {
                g[sidx(ns)] = ng;
                came[sidx(ns)] = Some(s);
                heap.push(Reverse((ng.saturating_add(h(ni, nj)), ni, nj, nlyr)));
            }
        }
    }
    None
}

/// Split an A* path into per-layer polyline runs (in world coords) plus the grid nodes
/// where a via is dropped. A layer change between consecutive states is a via at that
/// (shared) node. Consecutive via steps at the same node (a multi-layer hop) collapse to
/// one through via — it already spans every layer — so a deep hop emits a single via.
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
            runs.push((cur_layer, std::mem::take(&mut cur_pts)));
            // A through via at (i,j) spans all layers, so record it once per site.
            if vias.last() != Some(&(i, j)) {
                vias.push((i, j));
            }
            cur_layer = l;
            cur_pts = vec![grid.world(i, j)];
        }
    }
    runs.push((cur_layer, cur_pts));
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
    use crate::elaborate::{GenDirective as G, Source, board_rect};
    use crate::history::History;
    use crate::id::TraceId;
    use crate::part::part_library;
    use crate::query::{Engine, Key};
    use crate::route::Violation;

    /// Elaborate a source into a routed `History` head (default part library).
    fn doc_of(src: Source) -> History {
        doc_of_lib(src, &part_library())
    }

    /// Elaborate a source into a `History` head against a caller-supplied library (for
    /// scenes using footprints that the default library lacks, e.g. real SMD pads).
    fn doc_of_lib(src: Source, lib: &crate::part::PartLib) -> History {
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), lib, "src")
            .unwrap();
        h
    }

    /// Issue 0003: a proposed trace that clashes a different-net pad is dropped by the
    /// self-verify (the construction invariant is not trusted), and the net is moved
    /// to `unrouted` — `routed` never includes a net whose copper actually violates.
    #[test]
    fn verify_prunes_a_net_whose_trace_clashes_a_pad() {
        use crate::geom::Shape2D;
        use crate::part::{PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole};
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
            PartDef {
                name: "PAD".into(),
                pins: vec![pin],
                interfaces: BTreeMap::new(),
                graphics: Vec::new(),
                texts: Vec::new(),
                courtyard: None,
                class: None,
            },
        );
        let src = vec![
            G::Instance {
                path: "b".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Fix {
                path: "b".into(),
                pos: Point { x: 0, y: 0 },
            },
            G::ConnectPins {
                net: "B".into(),
                pins: vec![("b".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "src")
            .unwrap();

        let mut result = AutorouteResult {
            commands: vec![Command::AddTrace(
                TraceId(1),
                Trace {
                    net: NetId::new("A"),
                    layer: "F.Cu".into(),
                    path: vec![Point::mm(-2, 0), Point::mm(2, 0)],
                    width: 200_000,
                    prov: Provenance::Free,
                },
            )],
            routed: vec![NetId::new("A")],
            unrouted: vec![],
        };
        verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
        assert!(
            result.commands.is_empty(),
            "trace through a different-net pad must be pruned"
        );
        assert!(
            result.routed.is_empty(),
            "the clashing net must leave `routed`"
        );
        assert!(
            result.unrouted.contains(&NetId::new("A")),
            "and be reported unrouted"
        );
    }

    /// Apply a proposed transaction's commands to the history head (default library).
    fn apply_all(h: &mut History, cmds: Vec<Command>) {
        apply_all_lib(h, cmds, &part_library());
    }

    /// Apply commands against a caller-supplied library.
    fn apply_all_lib(h: &mut History, cmds: Vec<Command>, lib: &crate::part::PartLib) {
        h.commit(Transaction(cmds), lib, "autoroute").unwrap();
    }

    /// DRC violation set at the current head (default library).
    fn drc(h: &History) -> Vec<Violation> {
        drc_lib(h, &part_library())
    }

    /// DRC violation set against a caller-supplied library.
    fn drc_lib(h: &History, lib: &crate::part::PartLib) -> Vec<Violation> {
        let mut eng = Engine::new();
        eng.query(h.doc(), lib, Key::Drc).as_drc().to_vec()
    }

    fn has_clearance_or_width(v: &[Violation]) -> bool {
        v.iter()
            .any(|x| matches!(x, Violation::Clearance { .. } | Violation::MinWidth { .. }))
    }

    /// A two-net board on an explicit outline: VBUS (reg.VOUT↔dec.p1) and GND
    /// (reg.GND↔dec.p2). reg(LDO)@(0,0), dec(Cap)@(12,0).
    fn two_net_board() -> Source {
        vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "dec".into(),
                pos: Point::mm(12, 0),
            },
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

        let before = drc(&h);
        assert!(
            before
                .iter()
                .any(|v| matches!(v, Violation::Unrouted { .. })),
            "expected unrouted nets before routing: {before:?}"
        );

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(r.unrouted, Vec::<NetId>::new(), "both nets should route");
        assert_eq!(r.routed.len(), 2);
        assert!(!r.commands.is_empty());

        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(
            after.is_empty(),
            "routed board must be DRC clean, got {after:?}"
        );
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
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "dec".into(),
                pos: Point::mm(12, 0),
            },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins {
                net: "WALL".into(),
                pins: vec![("reg".into(), "VIN".into())],
            },
        ];
        let mut h = doc_of(src);
        let wall = Trace {
            net: NetId::new("WALL"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(6, -10), Point::mm(6, 10)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), wall)),
            &lib,
            "wall",
        )
        .unwrap();

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(
            r.unrouted.is_empty(),
            "VBUS should route around/under the wall"
        );
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
            !after.iter().any(
                |v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))
            ),
            "VBUS must be fully routed: {after:?}"
        );
    }

    /// An intentionally impossible net (walled off on *both* layers) is reported as
    /// unrouted rather than producing bad copper.
    #[test]
    fn impossible_net_is_reported_not_botched() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "dec".into(),
                pos: Point::mm(12, 0),
            },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins {
                net: "WALL".into(),
                pins: vec![("reg".into(), "VIN".into())],
            },
        ];
        let mut h = doc_of(src);
        for (id, layer) in [(TraceId(1), "F.Cu"), (TraceId(2), "B.Cu")] {
            let wall = Trace {
                net: NetId::new("WALL"),
                layer: layer.to_string(),
                path: vec![Point::mm(6, -12), Point::mm(6, 12)],
                width: 200_000,
                prov: Provenance::Pinned,
            };
            h.commit(Transaction::one(Command::AddTrace(id, wall)), &lib, "wall")
                .unwrap();
        }

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(
            r.unrouted,
            vec![NetId::new("VBUS")],
            "VBUS is walled off both layers"
        );
        assert!(
            r.commands.is_empty(),
            "a failed net must emit no copper, got {:?}",
            r.commands
        );

        let after = drc(&h);
        assert!(
            !has_clearance_or_width(&after),
            "no spurious clearance/width: {after:?}"
        );
        assert!(
            after.iter().any(
                |v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))
            ),
            "VBUS should remain flagged unrouted: {after:?}"
        );
    }

    /// A multi-pin (3-pin) net connects all pins (MST-style) and passes the ratsnest.
    #[test]
    fn autoroute_three_pin_net() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -12), Point::mm(30, 12)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c0".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "c0".into(),
                pos: Point::mm(12, 6),
            },
            G::Place {
                path: "c1".into(),
                pos: Point::mm(20, -6),
            },
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
        assert!(
            after.is_empty(),
            "3-pin routed net must be DRC clean: {after:?}"
        );
    }

    // ------------------------------------------------------------------------
    // N-layer grid, honest masking, pours, keep-outs, pad extents, pitch split.
    // ------------------------------------------------------------------------

    use crate::doc::MM;
    use crate::elaborate::RegionDecl;
    use crate::geom::{KeepoutKind, Material, Role, Shape2D, Slab, ZRange};

    /// A 4-copper stackup: F.Cu / In1.Cu / In2.Cu / B.Cu with the masks the two outer
    /// sides need (so the board stays fully masked / clean), z descending F→B.
    fn four_layer_slabs() -> Vec<G> {
        let cu = |name: &str, lo: Nm, hi: Nm| {
            G::Slab(Slab {
                name: name.into(),
                z: ZRange::new(lo, hi),
                role: Role::Conductor,
                material: Some(Material::named("copper")),
            })
        };
        let other = |name: &str, lo: Nm, hi: Nm, role: Role| {
            G::Slab(Slab {
                name: name.into(),
                z: ZRange::new(lo, hi),
                role,
                material: None,
            })
        };
        // z from bottom (0) up: B.Mask, B.Cu, core, In2, core, In1, core, F.Cu, F.Mask.
        vec![
            other("B.Mask", -25_000, 0, Role::Mask),
            cu("B.Cu", 0, 35_000),
            other("core3", 35_000, 500_000, Role::Substrate),
            cu("In2.Cu", 500_000, 535_000),
            other("core2", 535_000, 1_000_000, Role::Substrate),
            cu("In1.Cu", 1_000_000, 1_035_000),
            other("core1", 1_035_000, 1_565_000, Role::Substrate),
            cu("F.Cu", 1_565_000, 1_600_000),
            other("F.Mask", 1_600_000, 1_625_000, Role::Mask),
        ]
    }

    /// N-layer routing: on a 4-copper board with both *outer* layers walled off by
    /// foreign pinned copper across the whole span, the net still routes — it must use an
    /// inner layer — and stays DRC clean. Proves the grid is genuinely N-layer, not 2.
    #[test]
    fn four_layer_uses_inner_when_outers_blocked() {
        let lib = part_library();
        let mut src = four_layer_slabs();
        src.extend(vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "dec".into(),
                pos: Point::mm(12, 0),
            },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins {
                net: "WALL".into(),
                pins: vec![("reg".into(), "VIN".into())],
            },
        ]);
        let mut h = doc_of(src);
        // Walls on both OUTER copper layers, full board height: no crossing on F/B.
        for (id, layer) in [(TraceId(1), "F.Cu"), (TraceId(2), "B.Cu")] {
            let wall = Trace {
                net: NetId::new("WALL"),
                layer: layer.to_string(),
                path: vec![Point::mm(6, -12), Point::mm(6, 12)],
                width: 200_000,
                prov: Provenance::Pinned,
            };
            h.commit(Transaction::one(Command::AddTrace(id, wall)), &lib, "wall")
                .unwrap();
        }

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(
            r.unrouted.is_empty(),
            "VBUS should route on an inner layer, got unrouted {:?}",
            r.unrouted
        );
        // At least one trace on an inner copper layer proves inner-layer routing.
        let on_inner = r.commands.iter().any(
            |c| matches!(c, Command::AddTrace(_, t) if t.layer == "In1.Cu" || t.layer == "In2.Cu"),
        );
        assert!(
            on_inner,
            "expected a trace on an inner layer: {:?}",
            r.commands
        );
        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(
            !has_clearance_or_width(&after),
            "inner-layer route must stay clearance-clean: {after:?}"
        );
        assert!(
            !after.iter().any(
                |v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))
            ),
            "VBUS must be fully routed: {after:?}"
        );
    }

    /// A through via blocks its own site on *every* copper layer. Build a 4-layer board,
    /// route a net that must change layers (outer walls force a via), and assert the
    /// emitted via is a through via (`span: None`) and DRC (which fans it out to all
    /// spanned slabs) stays clean — a foreign net cannot occupy the via site on any layer.
    #[test]
    fn through_via_blocks_all_four_layers() {
        let lib = part_library();
        let mut src = four_layer_slabs();
        src.extend(vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "dec".into(),
                pos: Point::mm(12, 0),
            },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
            G::ConnectPins {
                net: "WALL".into(),
                pins: vec![("reg".into(), "VIN".into())],
            },
        ]);
        let mut h = doc_of(src);
        let wall = Trace {
            net: NetId::new("WALL"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(6, -10), Point::mm(6, 10)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), wall)),
            &lib,
            "wall",
        )
        .unwrap();

        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(r.unrouted.is_empty(), "VBUS should route around the wall");
        let via = r
            .commands
            .iter()
            .find_map(|c| match c {
                Command::AddVia(_, v) => Some(v),
                _ => None,
            })
            .expect("crossing a full-height wall drops layers via a via");
        assert_eq!(via.span, None, "vias are through (full copper extent)");
        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(
            !has_clearance_or_width(&after),
            "through-via route must stay clearance-clean on all layers: {after:?}"
        );
    }

    /// A one-pad SMD footprint on `layer`, 0.4mm square copper at the instance origin.
    fn smd_pad(layer: &str) -> crate::part::PartDef {
        crate::kicad::import_footprint(&format!(
            r#"(footprint "SP" (pad "1" smd rect (at 0 0) (size 0.4 0.4) (layers "{layer}")))"#
        ))
        .unwrap()
    }

    /// Masking: a route cannot cross a board cutout, cannot leave the outline, and honours
    /// the edge clearance. Two pads on opposite sides of a central slot cutout that spans
    /// the full board height, forcing any route between them through the cutout — which is
    /// masked — so the net cannot route.
    #[test]
    fn route_cannot_cross_a_cutout() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 10)),
            // A full-height slot cutout down the middle (x 9..11), splitting the board.
            G::Cutout {
                shape: Shape2D::rect(Point::mm(10, 5), 2 * MM, 12 * MM),
            },
            G::Instance {
                path: "l".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "r".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "l".into(),
                pos: Point::mm(4, 5),
            },
            G::Place {
                path: "r".into(),
                pos: Point::mm(16, 5),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("l".into(), "1".into()), ("r".into(), "1".into())],
            },
        ];
        let h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(
            r.unrouted,
            vec![NetId::new("SIG")],
            "the cutout splits the board — SIG cannot route across it"
        );
        assert!(r.commands.is_empty(), "a failed net emits no copper");
    }

    /// An authored through-hole (NPTH mounting hole) blocks routing over it on every layer
    /// (issue 0025's routing side): the hole is a full-stackup `Role::Void`, invisible to
    /// `board_region` (which only subtracts `Cutout`), so the router sees it only via the
    /// obstacle stream's Void arm. A big central hole spanning the board height forces any
    /// route between two flanking pads through the hole — which is blocked — so SIG fails.
    #[test]
    fn route_cannot_cross_a_mounting_hole() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 10)),
            // A large central NPTH hole (12mm dia at (10,5)) — taller than the 10mm board,
            // so it fully spans the height and leaves no channel above or below it.
            G::Hole {
                center: Point::mm(10, 5),
                dia: 12 * MM,
            },
            G::Instance {
                path: "l".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "r".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "l".into(),
                pos: Point::mm(3, 5),
            },
            G::Place {
                path: "r".into(),
                pos: Point::mm(17, 5),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("l".into(), "1".into()), ("r".into(), "1".into())],
            },
        ];
        let h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(
            r.unrouted,
            vec![NetId::new("SIG")],
            "an 8mm hole spanning the board height blocks the only channel between the pads"
        );
        assert!(r.commands.is_empty(), "a blocked net emits no copper");
    }

    /// A copper pour of a *foreign* net blocks routing: a board-covering GND pour on F.Cu
    /// leaves no F.Cu channel, so a two-pad SIG net (SMD, F.Cu only) cannot route (it has
    /// no other layer to escape to on this 2-layer default... it can drop to B.Cu — so to
    /// make the pour genuinely block, the pads are B.Cu and the pour is B.Cu too).
    #[test]
    fn foreign_pour_blocks_routing() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("B.Cu"));
        // A GND pad somewhere + a board-covering GND pour on B.Cu; two SIG pads on B.Cu
        // that the pour walls off (their own layer is flooded by a foreign net, and an SMD
        // pad seeds only on its own layer, so there is no escape).
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 10),
            Point::mm(0, 10),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 10)),
            G::Instance {
                path: "g".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "a".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "b".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g".into(),
                pos: Point::mm(1, 1),
            },
            G::Place {
                path: "a".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "b".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: outline,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "B.Cu".into(),
            }),
        ];
        let h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(
            r.unrouted,
            vec![NetId::new("SIG")],
            "a board-covering foreign pour on the pads' only layer blocks SIG"
        );
    }

    /// A copper keep-out blocks routing on its layer: a two-pad net whose only straight
    /// path is walled by a full-height `Role::Keepout(Copper)` region — and, on the
    /// 2-layer default, the keep-out is placed on both copper layers so there is no escape.
    #[test]
    fn keepout_blocks_routing() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        let mut src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 10)),
            G::Instance {
                path: "a".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "b".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "a".into(),
                pos: Point::mm(4, 5),
            },
            G::Place {
                path: "b".into(),
                pos: Point::mm(16, 5),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
            },
        ];
        // Full-height copper keep-out down the middle on BOTH copper layers.
        for layer in ["F.Cu", "B.Cu"] {
            src.push(G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(10, 5), 2 * MM, 12 * MM),
                role: Role::Keepout(KeepoutKind::Copper),
                net: None,
                layer: layer.into(),
            }));
        }
        let h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert_eq!(
            r.unrouted,
            vec![NetId::new("SIG")],
            "a full-height copper keep-out on both layers blocks SIG"
        );
        assert!(r.commands.is_empty(), "a blocked net emits no copper");
    }

    /// Pad extents (not points): two 0.4mm pads only 0.5mm apart on the *same* foreign
    /// net. A route of a third net threading the 0.1mm gap between their copper is
    /// impossible where the old point model (pads as zero-size points) would have let it
    /// through. Here the extents block the channel, so the third net detours (or fails) —
    /// we assert it does not lay copper *through* the gap by checking DRC stays clean.
    #[test]
    fn pad_extents_block_where_points_would_not() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        // Two GND pads 0.5mm apart (centres) — copper edges 0.1mm apart. A SIG net's two
        // pads sit above and below, so the straight route runs through the pad gap.
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Instance {
                path: "g0".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "g1".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "s0".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "s1".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g0".into(),
                pos: Point {
                    x: 5 * MM - 250_000,
                    y: 5 * MM,
                },
            },
            G::Place {
                path: "g1".into(),
                pos: Point {
                    x: 5 * MM + 250_000,
                    y: 5 * MM,
                },
            },
            G::Place {
                path: "s0".into(),
                pos: Point::mm(5, 1),
            },
            G::Place {
                path: "s1".into(),
                pos: Point::mm(5, 9),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g0".into(), "1".into()), ("g1".into(), "1".into())],
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("s0".into(), "1".into()), ("s1".into(), "1".into())],
            },
        ];
        let mut h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        // Whatever the router does, applying it must be DRC clean — the pad extents mean
        // it cannot thread the 0.1mm gap that a point model would have permitted.
        apply_all_lib(&mut h, r.commands, &lib);
        let after = drc_lib(&h, &lib);
        assert!(
            !has_clearance_or_width(&after),
            "routing must respect pad extents, not treat pads as points: {after:?}"
        );
    }

    /// The trace/via pitch split (the QFN fix, distilled): two adjacent fine-pitch (0.4mm)
    /// pads are *individually reachable* — the grid resolves them — where the old
    /// via-sized pitch (0.45mm > 0.4mm) could not place a node on each. Route two 2-pad
    /// nets whose pads are 0.4mm apart and assert both route DRC-clean.
    #[test]
    fn fine_pitch_pads_are_individually_reachable() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        // Four pads in a 0.4mm-pitch row: A B A B (nets NA, NB interleaved). Each net's two
        // pads sit 0.8mm apart with the other net's pad between them.
        let mut src = vec![board_rect(Point::mm(-2, -3), Point::mm(3, 3))];
        let xs = [0, 400_000, 800_000, 1_200_000];
        for (k, x) in xs.iter().enumerate() {
            src.push(G::Instance {
                path: format!("p{k}"),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            });
            src.push(G::Place {
                path: format!("p{k}"),
                pos: Point { x: *x, y: 0 },
            });
        }
        src.push(G::ConnectPins {
            net: "NA".into(),
            pins: vec![("p0".into(), "1".into()), ("p2".into(), "1".into())],
        });
        src.push(G::ConnectPins {
            net: "NB".into(),
            pins: vec![("p1".into(), "1".into()), ("p3".into(), "1".into())],
        });
        let mut h = doc_of_lib(src, &lib);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        // Both nets seed a distinct node on each fine-pitch pad; the fine grid resolves
        // them (a coarser via-sized pitch would collapse adjacent pads onto one node).
        // Routing may still need to detour up/around; the point is each pad is reachable
        // and the result is DRC clean.
        apply_all_lib(&mut h, r.commands, &lib);
        let after = drc_lib(&h, &lib);
        assert!(
            !has_clearance_or_width(&after),
            "fine-pitch routing must be clearance-clean: {after:?}"
        );
        // The grid must have seeded each net (nothing dropped for un-seedability): a net
        // with reachable pins is either routed or reported unrouted, never silently gone.
        assert_eq!(
            r.routed.len() + r.unrouted.len(),
            2,
            "both fine-pitch nets are accounted for"
        );
    }

    /// Via legality is stricter than trace legality (the pitch split): a via pad needs
    /// `via_pad/2 + width/2 + clearance` (0.375 mm) of room from any *other* net's copper,
    /// which is more than one grid `pitch` (0.30 mm) — so a via may not sit one node away
    /// from a foreign trace even though a *trace* one node away is clearance-clean (exactly
    /// `pitch − width = clearance`). This is the invariant the old via-sized grid papered
    /// over; here we assert it directly at the A* boundary.
    #[test]
    fn via_legality_is_stricter_than_trace_at_one_pitch() {
        let rules = DesignRules::default();
        let pitch = rules.min_trace_width + rules.min_clearance; // 0.30 mm
        let via_pad = 2 * rules.min_trace_width;
        // via_pad/2 + width/2 + clr = 0.15 + 0.075 + 0.15 = 0.375 mm
        let via_clear = rules.min_clearance + via_pad / 2 + rules.min_trace_width / 2;
        // A trace one node (pitch) from a foreign centreline is clean: pitch − width/2 −
        // width/2 = clearance. A via one node away is not: it needs `via_clear` > pitch.
        assert!(
            pitch >= rules.min_clearance + rules.min_trace_width / 2 + rules.min_trace_width / 2,
            "a trace one pitch from a foreign trace meets clearance (pitch ≥ clr + w)"
        );
        assert!(
            via_clear > pitch,
            "a via needs more than one pitch of room from foreign copper (the pitch split): \
             via_clear={via_clear} > pitch={pitch}"
        );
    }

    /// End-to-end companion to the invariant above: a dense two-net scene whose greedy
    /// solution puts a via near the other net's trunk only routes cleanly *because* via
    /// legality forbade the too-close via and forced a detour. Both nets route and DRC is
    /// clean — the same scene the Gerber export determinism test exercises, distilled.
    #[test]
    fn dense_scene_places_vias_clear_of_foreign_copper() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
            G::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c0".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "c0".into(),
                pos: Point::mm(12, 5),
            },
            G::Place {
                path: "c1".into(),
                pos: Point::mm(12, -5),
            },
            G::ConnectPins {
                net: "VBUS".into(),
                pins: vec![
                    ("reg".into(), "VOUT".into()),
                    ("c0".into(), "p1".into()),
                    ("c1".into(), "p1".into()),
                ],
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![
                    ("reg".into(), "GND".into()),
                    ("c0".into(), "p2".into()),
                    ("c1".into(), "p2".into()),
                ],
            },
        ];
        let mut h = doc_of(src);
        let r = autoroute(h.doc(), &lib, &DesignRules::default());
        assert!(
            r.unrouted.is_empty(),
            "both nets route (via legality forced clean via placement): {:?}",
            r.unrouted
        );
        apply_all(&mut h, r.commands);
        let after = drc(&h);
        assert!(
            after.is_empty(),
            "dense routed board must be DRC clean: {after:?}"
        );
    }

    // ------------------------------------------------------------------------
    // Decision 19a — via-permeable foreign pours.
    // ------------------------------------------------------------------------

    /// A full-board GND plane on In1.Cu, on a 4-copper board, plus a lone SIG pad.
    /// The scene the 19a verify/grid tests share.
    fn plane_scene(sig_via_at: Point) -> (History, crate::part::PartLib, AutorouteResult) {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 20),
            Point::mm(0, 20),
        ]);
        let mut src = four_layer_slabs();
        src.extend(vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "s".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            // A GND pad in a corner, well away from the plane centre — declares the net
            // the pour carries (a pour on an unconnected net is rejected at commit).
            G::Instance {
                path: "g".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "s".into(),
                pos: Point::mm(10, 10),
            },
            G::Place {
                path: "g".into(),
                pos: Point::mm(2, 2),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("s".into(), "1".into())],
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: outline,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "In1.Cu".into(),
            }),
        ]);
        let h = doc_of_lib(src, &lib);
        // A proposed SIG through via sitting inside the GND plane.
        let result = AutorouteResult {
            commands: vec![Command::AddVia(
                crate::id::ViaId(1),
                Via {
                    net: NetId::new("SIG"),
                    at: sig_via_at,
                    span: None,
                    drill: 300_000,
                    pad: 600_000,
                    prov: Provenance::Free,
                },
            )],
            routed: vec![NetId::new("SIG")],
            unrouted: vec![],
        };
        (h, lib, result)
    }

    /// A via punched into a foreign derived pour verifies **clean** (Decision 19a) AND is
    /// DRC-clean once committed — the two verdicts agree, which is what `routed` means.
    /// `verify_and_prune` re-derives the world (pours retreat around the proposed via, the
    /// automatic anti-pad) and skips pour-vs-solid exactly as `check_drc` does, so a via
    /// in a plane is not pruned. Non-vacuous: the via centre is well inside the plane
    /// outline, so a naive fill-inclusion test would flag it.
    #[test]
    fn via_inside_foreign_plane_verifies_clean() {
        let (mut h, lib, mut result) = plane_scene(Point::mm(10, 10));
        verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
        assert!(
            !result.commands.is_empty() && result.unrouted.is_empty(),
            "a via inside a foreign plane must survive verify (fill retreats on re-derive)"
        );
        // End-to-end: committing the via and running the real DRC must also be clean — the
        // committed doc's pours re-derive with the via's anti-pad, so no pour-vs-via short.
        apply_all_lib(&mut h, result.commands, &lib);
        let after = drc_lib(&h, &lib);
        assert!(
            !has_clearance_or_width(&after),
            "the committed via-in-plane must be DRC clean (re-derived anti-pad): {after:?}"
        );
    }

    /// The complement: a via too close to another net's **non-pour** copper still fails
    /// verify. A GND SMD pad (solid copper) sits at (10,10); a SIG via placed right on top
    /// of it clashes (solid-vs-solid is not exempt — only the pour retreats). Proves the
    /// 19a exemption is scoped to pours, not a blanket "vias never clash".
    #[test]
    fn via_on_foreign_solid_copper_is_pruned() {
        let mut lib = part_library();
        lib.insert("SP".into(), smd_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "g".into(),
                part: "SP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g".into(),
                pos: Point::mm(10, 10),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
        ];
        let h = doc_of_lib(src, &lib);
        // A SIG via directly on the GND pad's F.Cu copper — a real short, must prune.
        let mut result = AutorouteResult {
            commands: vec![Command::AddVia(
                crate::id::ViaId(1),
                Via {
                    net: NetId::new("SIG"),
                    at: Point::mm(10, 10),
                    span: None,
                    drill: 300_000,
                    pad: 600_000,
                    prov: Provenance::Free,
                },
            )],
            routed: vec![NetId::new("SIG")],
            unrouted: vec![NetId::new("SIG")],
        };
        // (SIG already unrouted for its lone pad; the point is the via command is dropped.)
        result.unrouted.clear();
        result.routed = vec![NetId::new("SIG")];
        verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
        assert!(
            result.commands.is_empty(),
            "a via on a foreign net's SOLID pad copper must be pruned (only pours yield)"
        );
        assert!(result.unrouted.contains(&NetId::new("SIG")));
    }

    /// A foreign plane still blocks TRACE placement on its own slab (Decision 19a: planes
    /// are not signal layers this round). Build the grid's per-net obstacle map with a
    /// full-board GND plane on In1.Cu and assert the trace mask on In1.Cu is set inside the
    /// plane, while the via mask at that same cell is NOT (the via is permeable there).
    #[test]
    fn foreign_plane_blocks_trace_but_not_via_in_blockmap() {
        let (h, lib, _r) = plane_scene(Point::mm(10, 10));
        let doc = h.doc();
        let rules = DesignRules::default();
        let su = stackup(&doc.source);
        let layers: Vec<Layer> = copper_layers_z(&su).into_iter().map(|(l, _)| l).collect();
        let nl = layers.len();
        let in1 = layers
            .iter()
            .position(|&l| layer_slab_name(&su, l).as_deref() == Some("In1.Cu"))
            .expect("In1.Cu present");
        let width = rules.min_trace_width;
        let via_pad = 2 * rules.min_trace_width;
        let pitch = rules.min_trace_width + rules.min_clearance;
        let area = crate::solve::Rect {
            min: Point::mm(0, 0),
            max: Point::mm(20, 20),
        };
        let grid = Grid::new(area, pitch, nl);
        let board_mask = BoardMask::build(doc, &grid, &rules, width);
        let netlist = doc_netlist(doc);
        // Per-net pads (needed for the bare-pin obstacle pass, empty here for SIG).
        let mut net_pads: BTreeMap<NetId, Vec<Pad>> = BTreeMap::new();
        net_pads.insert(NetId::new("SIG"), Vec::new());
        net_pads.insert(NetId::new("GND"), Vec::new());
        let block = BlockMap::build(
            &grid,
            &board_mask,
            doc,
            &lib,
            &rules,
            &su,
            &netlist,
            &net_pads,
            &NetId::new("SIG"),
            width,
            via_pad,
        );
        // A cell deep inside the plane, away from the board edge AND from any pad (the
        // SIG pad sits at (10,10), the GND pad at (2,2)).
        let (ci, cj) = (
            ((15 * MM - area.min.x) / pitch) as usize,
            ((15 * MM - area.min.y) / pitch) as usize,
        );
        let idx = grid.idx(ci, cj);
        assert!(
            block.trace[idx * nl + in1],
            "the foreign GND plane must block TRACE placement on In1.Cu"
        );
        assert!(
            !block.via[idx],
            "but a via may punch the plane — the via-site mask is clear (Decision 19a)"
        );
        assert!(
            !block.via_layer[idx * nl + in1],
            "and the via barrel's In1.Cu room test ignores the permeable pour"
        );
    }
}
