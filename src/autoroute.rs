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
//! preferred-direction cost would slot into [`astar`](search)'s orthogonal step cost; it
//! is not built — issue 0008 owns that design cycle). Consequently **net ordering
//! matters**: a net that fails may well be routable in a different order. Failures are
//! *reported* (the net goes in `unrouted`), never fatal and never emitted as partial/
//! overlapping copper. Vias are always through (full copper extent, `span: None`);
//! blind/buried vias are out of scope. All geometry is integer nm; everything is
//! deterministic.
//!
//! ## Module layout
//!
//! This file is the driver facade; the external surface is just [`autoroute`] plus the
//! [`AutorouteResult`]/[`AutorouteStats`] result types. The pieces:
//! - [`grid`] — the [`Grid`](grid::Grid) discretisation and `routing_area`.
//! - [`obstacles`] — the board mask, per-net block map, own-copper seeds, and the
//!   private segment-distance helper.
//! - [`ingest`] — reading the doc into `Pad`s and the `verify_and_prune` backstop.
//! - [`search`] — the A* maze search and path→command lowering.

mod grid;
mod ingest;
mod obstacles;
mod search;

use crate::command::Command;
use crate::doc::Doc;
use crate::elaborate::stackup;
use crate::id::NetId;
use crate::part::{PartLib, pin_world};
use crate::route::{DesignRules, Layer, copper_layers_z, layer_slab_name};
use std::collections::{BTreeMap, BTreeSet};

use grid::{Grid, routing_area};
use ingest::{Pad, doc_netlist, pad_layers, verify_and_prune};
use obstacles::{BlockMap, BoardMask, own_copper_cells, own_plane_cells, pad_on_own_copper};
use search::route_net;

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

// Test-only imports for `tests`: the types the former monolithic `autoroute.rs`
// imported at file scope and forwarded to the test module through its `use super::*`,
// but which the driver facade itself no longer references. Not part of any surface.
#[cfg(test)]
use {
    crate::doc::{Nm, Provenance},
    crate::route::{Trace, Via},
};

#[cfg(test)]
mod tests;
