//! Obstacles → blocked grid cells, derived honestly from the unified
//! [`world_features`](crate::route::world_features) stream: the board [`BoardMask`],
//! the per-net [`BlockMap`], the own-copper seed producers, and the private `within`
//! segment-distance helper (a later wave dedups it against `route`'s kernel).

use crate::doc::{Doc, Nm, Point};
use crate::geom::{Extent, KeepoutKind, Role, Shape2D, Stackup, ZRange};
use crate::id::NetId;
use crate::part::{PartLib, PinRole};
use crate::route::{DesignRules, world_features};
use std::collections::BTreeMap;

use super::grid::Grid;
use super::ingest::Pad;
use super::search::State;

// ----------------------------------------------------------------------------
// Board masking: cells outside the board (or too near an edge) are unroutable.
// ----------------------------------------------------------------------------

/// Cells the board itself forbids on every layer: a node outside the board region
/// (outline ∖ cutouts) or within `edge_clearance + half_width + half_edge` of its
/// boundary. Shared across nets (the board doesn't change per net).
pub(super) struct BoardMask {
    blocked: Vec<bool>,
}

impl BoardMask {
    pub(super) fn build(doc: &Doc, grid: &Grid, rules: &DesignRules, width: Nm) -> BoardMask {
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
pub(super) struct BlockMap {
    pub(super) trace: Vec<bool>,
    pub(super) via: Vec<bool>,
    pub(super) via_layer: Vec<bool>,
}

impl BlockMap {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build(
        grid: &Grid,
        board: &BoardMask,
        doc: &Doc,
        lib: &PartLib,
        rules: &DesignRules,
        su: &Stackup,
        netlist: &BTreeMap<NetId, Vec<(crate::doc::PinRef, PinRole)>>,
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
pub(super) fn own_plane_cells(
    grid: &Grid,
    doc: &Doc,
    lib: &PartLib,
    rules: &DesignRules,
    su: &Stackup,
    netlist: &BTreeMap<NetId, Vec<(crate::doc::PinRef, PinRole)>>,
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
pub(super) fn own_copper_cells(
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
/// through [`own_plane_cells`]). Used to skip re-stubbing an already-connected pad on a
/// rerun. `at` is checked in world XY; a pad is an all-layer point for incidence just as
/// the ratsnest treats it, so no per-layer gate is applied here (matching
/// route::pin_islands pin↔trace/via incidence, which is all-layer). Robust to the pad's
/// grid node shifting between passes, which node-set matching alone was not.
pub(super) fn pad_on_own_copper(doc: &Doc, _su: &Stackup, cur: &NetId, p: &Pad) -> bool {
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

/// Is the distance from point `p` to segment `a`–`b` within `r` (inclusive)? Exact
/// i128 squared-distance comparison (a rational `num/den`) — no float, deterministic.
///
/// NOTE: duplicated against `route`'s segment kernel; a later wave dedups both into
/// `geom`. Left here unchanged for now.
pub(super) fn within(p: Point, a: Point, b: Point, r: Nm) -> bool {
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
