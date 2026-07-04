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
///
/// `stats` records the *pre-verify* search result — how many nets the greedy search
/// connected and how much copper it proposed BEFORE `verify_and_prune` culled clashers and
/// `reconcile_routed_with_ratsnest` demoted fragmented nets. The gap between `stats` and
/// the final `routed`/`commands` is the cost of the fenced greedy-no-rip-up model: the
/// search finds many routes that mutually clash and get pruned. The pre-verify number is
/// the capability signal for the rip-up/negotiation discussion (issue 0008), so it is a
/// reproducible field, not transient instrumentation.
#[derive(Clone, Debug, Default)]
pub struct AutorouteResult {
    pub commands: Vec<Command>,
    pub routed: Vec<NetId>,
    pub unrouted: Vec<NetId>,
    pub stats: AutorouteStats,
}

/// The greedy search's result *before* verification/reconciliation mutate it (see
/// [`AutorouteResult`]). Populated once, immediately after the net-by-net pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct AutorouteStats {
    /// Nets the search connected (tree completed) before verify pruned any.
    pub pre_verify_routed: usize,
    /// Total commands (traces + vias) the search proposed before pruning.
    pub pre_verify_commands: usize,
    /// Of `pre_verify_commands`, how many were vias (the stitching-via signal).
    pub pre_verify_vias: usize,
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

        // Pre-connected tree membership seeded into the A* tree:
        //  - Decision 19b — the net's OWN pour fill cells (stitching targets): the search
        //    then discovers pad→plane stitching vias as ordinary via drops. Seeding ALL
        //    own-fill cells lets the router treat a fragmented plane as one node —
        //    acceptable because pin_islands (layer-honest, Task A) is the downstream judge.
        //  - F1 — the net's OWN already-committed trace/via copper: makes a rerun build on
        //    prior copper (idempotent no-op for a connected net, extension for a partial
        //    one) instead of silently duplicating it (same-net overlap is invisible to
        //    verify and DRC). This matters because a demoted net keeps its partial copper,
        //    so iterative reruns are the expected workflow.
        let mut seeds = own_plane_cells(&grid, doc, lib, rules, &su, &netlist, nid);
        seeds.extend(own_copper_cells(
            &grid,
            doc,
            &su,
            &layer_names,
            nid,
            via_pad,
        ));
        seeds.sort_unstable();
        seeds.dedup();
        // Per-pad: is this pad already tied to the net's committed copper (a prior pass, a
        // hand route)? Used by route_net to skip re-stubbing a connected pad (F1
        // idempotency) — a geometric test on the pad centre, robust to the pad's grid node
        // shifting between passes (which node-set matching alone was not).
        let pad_connected: Vec<bool> = pads
            .iter()
            .map(|p| pad_on_own_copper(doc, &su, nid, p))
            .collect();

        match route_net(
            &grid,
            &block,
            &mut owner,
            net_seq as i32,
            nid,
            pads,
            &seeds,
            &pad_connected,
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

    // Snapshot the pre-verify search result (the capability signal for the rip-up
    // discussion, issue 0008) BEFORE verify/reconcile mutate `routed`/`commands`.
    result.stats = AutorouteStats {
        pre_verify_routed: result.routed.len(),
        pre_verify_commands: result.commands.len(),
        pre_verify_vias: result
            .commands
            .iter()
            .filter(|c| matches!(c, Command::AddVia(..)))
            .count(),
    };

    // Don't trust the construction invariant — verify the proposed copper against the
    // real DRC and drop any net that actually clashes.
    verify_and_prune(doc, lib, rules, &mut result);
    // Reconcile `routed` with the ratsnest (Decision 19b honesty): tree completion is not
    // proof of connectivity once the plane is a stitching target — seeding the tree with
    // all own-fill cells can let a net's tree "complete" while its pads land on different
    // (fragmented) plane islands, or while a stitching via was pruned by verify. So the
    // final `routed`/`unrouted` split is taken from the *committed* board's DRC ratsnest,
    // not from tree completion: apply the surviving commands to a scratch doc and demote
    // any net the ratsnest still reports as `Unrouted` (>1 island). This makes the
    // autorouter's `routed` claim agree with the DRC the example independently runs.
    reconcile_routed_with_ratsnest(doc, lib, rules, &mut result);
    result
}

/// Move nets the committed-board ratsnest still reports unrouted out of `result.routed`
/// (Decision 19b). `routed` must mean DRC-connected, not tree-complete: with plane
/// seeding a tree can complete across a fragmented plane's separate islands. Applies the
/// surviving commands to a scratch clone and consults `check_drc`'s `Unrouted` set.
fn reconcile_routed_with_ratsnest(
    doc: &Doc,
    lib: &PartLib,
    rules: &DesignRules,
    result: &mut AutorouteResult,
) {
    if result.routed.is_empty() {
        return;
    }
    let txn = crate::command::Transaction(result.commands.clone());
    let scratch = match crate::command::apply(doc, &txn, lib, 0) {
        Ok(d) => d,
        Err(_) => return, // ordinary AddTrace/AddVia; on the rare reject, leave as-is
    };
    let drc = crate::route::check_drc(&scratch, lib, &doc_netlist(&scratch), rules);
    let still_unrouted: BTreeSet<NetId> = drc
        .iter()
        .filter_map(|v| match v {
            crate::route::Violation::Unrouted { net, .. } => Some(net.clone()),
            _ => None,
        })
        .collect();
    if still_unrouted.is_empty() {
        return;
    }
    // A net demoted here keeps its (clean) copper committed — it is partial progress, not
    // a clash. Only the routed/unrouted *bookkeeping* moves, so the caller's reported count
    // is honest while the copper the router did lay (e.g. a stitching via to one island)
    // stays. This mirrors how a hand-routed partial net reads on the ratsnest.
    let demoted: Vec<NetId> = result
        .routed
        .iter()
        .filter(|n| still_unrouted.contains(n))
        .cloned()
        .collect();
    if demoted.is_empty() {
        return;
    }
    result.routed.retain(|n| !still_unrouted.contains(n));
    result.unrouted.extend(demoted);
    result.unrouted.sort();
    result.unrouted.dedup();
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

/// The grid cells over a net's **own** pour fill, as A* seed states (Decision 19b). One
/// state per grid node that falls inside the net's own derived-pour fill, on the pour's
/// slab ordinal. Derived from the same `world_features` stream (the net's own `Area`
/// conductors), so the seed geometry is exactly the fill DRC/export see. Cells outside the
/// grid's copper layers, or on a slab the router does not model, are dropped. No board-
/// mask filtering: a plane cell is a connection target, not a routed node the net must
/// keep clear — a via lands on it and its legality is judged by `via_ok` at drop time.
fn own_plane_cells(
    grid: &Grid,
    doc: &Doc,
    lib: &PartLib,
    rules: &DesignRules,
    su: &Stackup,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
    cur: &NetId,
) -> Vec<State> {
    let cu = su.copper_slabs();
    let world = world_features(doc, lib, netlist, rules, su)
        .expect("world_features on a committed doc (slab gate enforced at commit)");
    let mut cells: Vec<State> = Vec::new();
    for nf in &world {
        if nf.net.as_ref() != Some(cur) || nf.feature.role != Role::Conductor {
            continue;
        }
        let Extent::Prism { shape, z } = &nf.feature.extent;
        // Own pours only — the `is_pour ⟺ Shape2D::Area` invariant (Decision 16).
        let Shape2D::Area { region } = shape else {
            continue;
        };
        let Some(l) = cu.iter().position(|s| s.z == *z) else {
            continue;
        };
        if l >= grid.layers {
            continue;
        }
        let Some((lo, hi)) = shape.bbox() else {
            continue;
        };
        let (imin, imax, jmin, jmax) = grid.bbox_range(lo, hi, 0);
        for j in jmin..=jmax {
            for i in imin..=imax {
                if region.contains_point(grid.world(i, j)) {
                    cells.push((i, j, l));
                }
            }
        }
    }
    cells.sort_unstable();
    cells.dedup();
    cells
}

/// The grid cells covered by a net's **own already-committed** trace and via copper, as
/// A* seed states (F1 fix). One state per grid node that lies on a same-net trace (within
/// its half-width, on the trace's slab ordinal) or under a same-net via (within its
/// pad radius, on every copper slab the barrel spans). Seeded into the A* tree exactly
/// like [`own_plane_cells`], so a rerun of the router treats prior copper as
/// pre-connected: a fully-routed net's pads are already reachable through its own copper
/// (tree spans them ⇒ nothing emitted, an idempotent no-op), and a partially-routed net
/// EXTENDS its copper toward the unconnected pads instead of laying a duplicate. Both
/// `Pinned` (hand/frozen) and `Free` (router-owned) copper count — the router builds on
/// whatever copper the net already carries. Same-net overlap is invisible to verify and
/// DRC, so without this a second pass silently duplicates clean nets' copper.
fn own_copper_cells(
    grid: &Grid,
    doc: &Doc,
    su: &Stackup,
    layer_names: &[String],
    cur: &NetId,
    via_pad: Nm,
) -> Vec<State> {
    let cu = su.copper_slabs();
    let ord = |name: &str| layer_names.iter().position(|n| n == name);
    let mut cells: Vec<State> = Vec::new();

    // Traces: cells within the trace's half-width of any centreline segment, on its slab.
    for t in doc.traces.values() {
        if t.net != *cur {
            continue;
        }
        let Some(l) = ord(&t.layer) else { continue };
        let r = t.width / 2;
        for seg in t.path.windows(2) {
            let (a, b) = (seg[0], seg[1]);
            let lo = Point {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
            };
            let hi = Point {
                x: a.x.max(b.x),
                y: a.y.max(b.y),
            };
            let (imin, imax, jmin, jmax) = grid.bbox_range(lo, hi, r);
            for j in jmin..=jmax {
                for i in imin..=imax {
                    if within(grid.world(i, j), a, b, r) {
                        cells.push((i, j, l));
                    }
                }
            }
        }
    }

    // Vias: cells within the via's pad radius of its centre, on every spanned copper slab.
    for v in doc.vias.values() {
        if v.net != *cur {
            continue;
        }
        let r = via_pad.max(v.pad) / 2;
        let lo = Point {
            x: v.at.x - r,
            y: v.at.y - r,
        };
        let hi = Point {
            x: v.at.x + r,
            y: v.at.y + r,
        };
        let (imin, imax, jmin, jmax) = grid.bbox_range(lo, hi, 0);
        for s in v.spanned_slabs(&cu) {
            let Some(l) = ord(&s.name) else { continue };
            for j in jmin..=jmax {
                for i in imin..=imax {
                    if within(grid.world(i, j), v.at, v.at, r) {
                        cells.push((i, j, l));
                    }
                }
            }
        }
    }

    cells.sort_unstable();
    cells.dedup();
    cells
}

/// Is a pad already electrically tied to its net's **committed** copper (F1)? A geometric
/// test on the pad centre mirroring the ratsnest's pin incidence: the pad touches a
/// same-net trace (centre within the trace's half-width of a segment) or a same-net via
/// (centre within the via's pad radius). Pours are handled separately (the pad is seeded
/// through `own_plane_cells`). Used to skip re-stubbing an already-connected pad on a
/// rerun. `at` is checked in world XY; a pad is an all-layer point for incidence just as
/// the ratsnest treats it, so no per-layer gate is applied here (matching route::pin_islands
/// pin↔trace/via incidence, which is all-layer). Robust to the pad's grid node shifting
/// between passes, which node-set matching alone was not.
fn pad_on_own_copper(doc: &Doc, _su: &Stackup, cur: &NetId, p: &Pad) -> bool {
    let touches_seg = |a: Point, b: Point, r: Nm| within(p.at, a, b, r);
    for t in doc.traces.values() {
        if t.net != *cur {
            continue;
        }
        let r = t.width / 2;
        if t.path.windows(2).any(|s| touches_seg(s[0], s[1], r)) {
            return true;
        }
    }
    for v in doc.vias.values() {
        if v.net == *cur && touches_seg(v.at, v.at, v.pad / 2) {
            return true;
        }
    }
    false
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
    seeds: &[State],
    pad_connected: &[bool],
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

    // The set of (node, layer) currently in the net's connected copper.
    let mut tree: Vec<State> = Vec::new();

    // If the net carries pre-connected copper (its own pour fill — Decision 19b — and/or
    // its own already-committed traces/vias — F1), seed the tree with those `seeds` cells
    // and route *every* pad to the tree: a pad→plane stitch is a via the A* path yields,
    // and a pad already sitting on prior copper routes in zero steps (idempotent rerun).
    // Otherwise seed at pin 0 (the classic MST start) and route pins 1..n.
    //
    // Seed cells are NOT `claim`ed into `owner`: pour fill is derived copper, not a routed
    // node, and claiming it would make a foreign net's via see the cell as owned and reject
    // it — defeating 19a via-permeability. They stay `owner == -1` (free), which the own
    // net traverses freely and a foreign net's via may punch; other nets are already barred
    // from *traces* over same-net copper by their BlockMap (world_features stamps it).
    // Seeding all own-fill cells may present a fragmented plane as one node; the ratsnest is
    // the honest downstream judge (see `own_plane_cells`).
    // The tree is seeded from all pre-connected copper the net already carries:
    //  - `seeds`: its own pour fill (Decision 19b stitching targets) and its own committed
    //    trace/via cells (F1 — so a rerun builds on prior copper).
    //  - the grid node of every pad ALREADY on the net's committed copper
    //    (`pad_connected[k]` — a geometric test): that node may not coincide with a seed
    //    cell (the pad's nearest routable node can shift between passes as other nets'
    //    copper lands), so add it explicitly and emit no stub for that pad below.
    //
    // Seed/pad cells are NOT `claim`ed into `owner`: pour fill is derived copper, not a
    // routed node, and claiming it would make a foreign net's via see the cell as owned and
    // reject it — defeating 19a via-permeability. They stay `owner == -1` (free), which the
    // own net traverses freely and a foreign net's via may punch; other nets are already
    // barred from *traces* over same-net copper by their BlockMap (world_features stamps
    // it). Seeding all own-fill cells may present a fragmented plane as one node; the
    // ratsnest is the honest downstream judge (see `own_plane_cells`).
    tree.extend_from_slice(seeds);
    for (k, p) in pin_nodes.iter().enumerate() {
        if pad_connected[k] {
            tree.push(*p);
        }
    }

    // If nothing pre-connects the net, seed the classic MST start at pin 0 (stub its pad
    // onto its grid node) and route pins 1..n; otherwise every not-yet-connected pad routes
    // to the pre-connected tree.
    let first_pin = if tree.is_empty() {
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
        tree.push((si, sj, sl));
        1
    } else {
        0
    };

    // Tree membership for the idempotency skip. A pad whose node is already in the tree is
    // connected — via committed trace/via copper (`pad_connected`), a same-slab plane cell
    // it sits on (seeded from `own_plane_cells`, so a pour-only-connected pad with no stub
    // is caught here even though `pad_connected` only inspects traces/vias), or another
    // pad's already-laid route. Routing it anyway would re-stub it, silently duplicating
    // copper (same-net overlap is invisible to verify AND DRC).
    let tree_set: BTreeSet<State> = tree.iter().copied().collect();

    for k in first_pin..pin_nodes.len() {
        // F1 idempotency: skip a pad that is already connected (committed copper or a tree
        // seed cell) — re-routing it would re-stub it and silently duplicate copper.
        if pad_connected[k] || tree_set.contains(&pin_nodes[k]) {
            continue;
        }
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
mod tests;
