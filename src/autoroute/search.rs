//! Per-net maze routing: the A* search over `(i, j, layer)` and the machinery that
//! turns a found path into `AddTrace`/`AddVia` commands.

use crate::command::Command;
use crate::doc::{Nm, Point, Provenance};
use crate::id::{NetId, TraceId, ViaId};
use crate::route::{Trace, Via};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};

use super::grid::Grid;
use super::ingest::Pad;
use super::obstacles::BlockMap;

pub(super) type State = (usize, usize, usize); // (i, j, layer)
/// A polyline run on one layer (world points), as produced from an A* path; the `usize`
/// is the layer ordinal.
type Run = (usize, Vec<Point>);

#[allow(clippy::too_many_arguments)]
pub(super) fn route_net(
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
