//! A deterministic, convergence-based least-change placement solver.
//!
//! ## Method
//!
//! Projected Gauss-Seidel (a.k.a. sequential constraint projection / position-based
//! relaxation) wrapped in a real convergence loop. Each *sweep* visits the
//! constraints in their stable `Vec` order and projects the current positions onto
//! each constraint's feasible set in turn (using the just-updated positions —
//! Gauss-Seidel, not Jacobi), then clamps movable nodes into the board. A node is
//! only moved by a constraint that is actually *violated*, and only by the minimal
//! displacement that satisfies it, so:
//!   - a part touched by no (violated) constraint never moves — it stays exactly at
//!     its anchor (least change — the "why did it jump across the board" antidote);
//!   - there is no anchor-spring penalty term fighting the constraints, so feasible
//!     sets are satisfied *exactly* (to tolerance), not approximately.
//!
//! Inequality constraints (`Near`, `MinSep`, `NearPin`) are handled by an implicit
//! active set: the projection is a no-op while the constraint has slack and fires
//! only when violated, which is exactly active-set handling for these one-sided
//! distance constraints.
//!
//! ## Guarantees
//!
//! - **Iterate to convergence, not a fixed count.** The loop runs until the maximum
//!   constraint residual drops below `RES_TOL`, or the maximum per-sweep node
//!   movement drops below `MOVE_TOL` (a geometric stall: the projections can no
//!   longer make progress), or a `MAX_ITERS` safety cap is hit. `Solution.converged`
//!   records whether the residual tolerance was actually met; `Solution.iters`
//!   records how many sweeps it took.
//! - **Feasible sets are satisfied to a tight tolerance** (`RES_TOL`, 1 µm — about
//!   two orders of magnitude tighter than the old fixed-iteration relaxation's
//!   ~0.1–0.2 mm). The motivating case — three decouplers each `Near` a regulator
//!   and pairwise `MinSep` apart — converges to within `RES_TOL` of every relation.
//! - **Infeasibility is reported, not hidden.** When the loop ends without the
//!   residual tolerance met (cap hit or geometric stall on a set that cannot be
//!   satisfied — e.g. a `MinSep` larger than the board can fit, or two `Fix`ed nodes
//!   a `Near` cannot reconcile), `Solution.converged` is `false` and
//!   `Solution.unsatisfied` lists exactly which constraints are still violated and
//!   by how much, instead of silently returning a wrong-but-plausible placement.
//! - **Deterministic.** No RNG; stable `BTreeMap`/`Vec` iteration order; coincident
//!   points break ties on a fixed axis; f64 working math is rounded to integer nm on
//!   output. Same `Problem` in → identical `Solution` out, bit for bit.
//!
//! ## Honest limits
//!
//! This is *not* a research-grade general geometric constraint solver: there is no
//! DOF analysis, no graph decomposition into independently-solvable subsystems, and
//! no global-optimality claim for the least-change objective (projection yields a
//! feasible point with minimal *local* corrections, not the global minimum-movement
//! solution). `MinSep` makes the feasible region non-convex, so on a pathological
//! start the projection can settle into a poor local configuration; for the
//! prototype's well-separated scenes this does not bite. Convergence of projected
//! Gauss-Seidel on coupled inequality systems is reliable in practice here but not
//! formally guaranteed for every input — which is exactly why feasibility is
//! *checked and reported* rather than assumed.

use crate::doc::{Nm, Point};
use crate::geom::BoardShape;
use crate::id::EntityId;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

#[derive(Clone, Debug)]
pub enum Constraint {
    /// Keep `a` and `b` no further apart than `within`.
    Near { a: EntityId, b: EntityId, within: Nm },
    /// Keep `a` and `b` at least `gap` apart (clearance / non-overlap).
    MinSep { a: EntityId, b: EntityId, gap: Nm },
    /// Make all nodes share an x coordinate (a vertical line).
    AlignX { nodes: Vec<EntityId> },
    /// Make all nodes share a y coordinate (a horizontal line).
    AlignY { nodes: Vec<EntityId> },
    /// Keep node `a` no further than `within` from a *pin* on node `b`. The pin's
    /// world position is `pos[b] + b_off` each iteration, where `b_off` is the
    /// pin's local offset already rotated by `b`'s (fixed) orientation. Moving `b`
    /// carries its pin rigidly, so the correction is applied to `b`'s position.
    NearPin { a: EntityId, b: EntityId, b_off: Point, within: Nm },
    /// Two component courtyards (axis-aligned boxes, centred on each node, with the
    /// given half-extents already oriented) must not overlap (issue 0005). When they
    /// do, the nodes are pushed apart along the axis of least penetration. Two
    /// overlapping *fixed* parts cannot be separated and are reported as unsatisfied.
    NoOverlap { a: EntityId, b: EntityId, a_half: (Nm, Nm), b_half: (Nm, Nm) },
}

pub struct Problem {
    /// Anchor (also the initial position) of every node.
    pub anchors: BTreeMap<EntityId, Point>,
    /// Nodes that cannot move (Fixed / Pinned provenance).
    pub fixed: BTreeSet<EntityId>,
    /// Board boundary; movable nodes are kept inside the outline and out of cutouts.
    pub board: Option<BoardShape>,
    pub constraints: Vec<Constraint>,
}

/// A constraint the solver could not satisfy, with its residual (how far from
/// satisfied, in nm; 0 means satisfied). Reported instead of silently returning a
/// wrong placement.
#[derive(Clone, Debug)]
pub struct Unsatisfied {
    pub constraint: Constraint,
    pub residual: Nm,
}

/// The result of a solve. `positions` is the placement (always populated, rounded to
/// integer nm); `converged` says whether every constraint met `RES_TOL`; `iters` is
/// how many sweeps ran; `unsatisfied` lists the still-violated constraints when
/// `!converged` (empty when converged).
pub struct Solution {
    pub positions: BTreeMap<EntityId, Point>,
    pub converged: bool,
    pub iters: usize,
    pub unsatisfied: Vec<Unsatisfied>,
}

/// Safety cap on sweeps. Reached only when the system neither converges nor stalls
/// (e.g. an oscillating infeasible set) — the result is then reported as unsatisfied.
const MAX_ITERS: usize = 5000;
/// Convergence tolerance on the max constraint residual, in nm. 1 µm — about two
/// orders of magnitude tighter than the old fixed-iteration relaxation (~0.1–0.2 mm).
const RES_TOL: f64 = 1_000.0;
/// If the largest node movement in a whole sweep falls below this (nm) while the
/// residual is still above `RES_TOL`, the projection has geometrically stalled: the
/// remaining constraints cannot be satisfied. Tiny so a still-converging system is
/// never mistaken for a stalled one (movement and residual shrink together).
const MOVE_TOL: f64 = 1.0;

/// Two placements within this distance are "the same" for effectiveness checks.
pub const PLACE_TOL: Nm = 100_000; // 0.1 mm

pub fn dist(a: Point, b: Point) -> f64 {
    let dx = (a.x - b.x) as f64;
    let dy = (a.y - b.y) as f64;
    (dx * dx + dy * dy).sqrt()
}

pub fn solve(p: &Problem) -> Solution {
    let mut pos: BTreeMap<EntityId, (f64, f64)> = p
        .anchors
        .iter()
        .map(|(k, v)| (k.clone(), (v.x as f64, v.y as f64)))
        .collect();

    let mut iters = 0;
    let mut converged = false;
    for it in 1..=MAX_ITERS {
        iters = it;
        let before = pos.clone();

        // Relational constraints (Gauss-Seidel: each projection sees the previous
        // ones' updates within the same sweep).
        for c in &p.constraints {
            apply_constraint(c, &mut pos, &p.fixed);
        }
        // Containment has the last word (movable nodes only; a fixed datum may sit
        // outside the outline). Only out-of-bounds nodes are moved — an in-bounds
        // node keeps its exact position (no per-sweep rounding that would stall
        // convergence). For a rectangular board this reproduces the old clamp.
        if let Some(board) = &p.board {
            for (id, pp) in pos.iter_mut() {
                if p.fixed.contains(id) {
                    continue;
                }
                let pt = Point { x: pp.0.round() as Nm, y: pp.1.round() as Nm };
                if !board.contains(pt) {
                    let q = board.contain(pt);
                    pp.0 = q.x as f64;
                    pp.1 = q.y as f64;
                }
            }
        }

        // Largest movement this sweep, and the worst remaining residual.
        let mut max_move: f64 = 0.0;
        for (id, &(x, y)) in &pos {
            let (bx, by) = before[id];
            let d = ((x - bx).powi(2) + (y - by).powi(2)).sqrt();
            if d > max_move {
                max_move = d;
            }
        }
        let max_res = p
            .constraints
            .iter()
            .map(|c| constraint_residual(c, &pos))
            .fold(0.0_f64, f64::max);

        if max_res <= RES_TOL {
            converged = true;
            break;
        }
        // Residual still high but nothing is moving any more: geometric stall — the
        // remaining constraints are mutually infeasible. Stop and report them.
        if max_move <= MOVE_TOL {
            converged = false;
            break;
        }
    }

    let positions = pos
        .iter()
        .map(|(k, &(x, y))| (k.clone(), Point { x: x.round() as i64, y: y.round() as i64 }))
        .collect();

    let unsatisfied = if converged {
        Vec::new()
    } else {
        p.constraints
            .iter()
            .filter_map(|c| {
                let r = constraint_residual(c, &pos);
                (r > RES_TOL).then(|| Unsatisfied { constraint: c.clone(), residual: r.round() as Nm })
            })
            .collect()
    };

    Solution { positions, converged, iters, unsatisfied }
}

/// World point of `id` plus a local offset (used for pins rigidly attached to a node).
fn point_of(pos: &BTreeMap<EntityId, (f64, f64)>, id: &EntityId, off: (f64, f64)) -> Option<(f64, f64)> {
    pos.get(id).map(|p| (p.0 + off.0, p.1 + off.1))
}

/// How far a constraint is from satisfied, in nm (0.0 = satisfied). The residual the
/// convergence loop drives to zero and the value reported on infeasibility.
fn constraint_residual(c: &Constraint, pos: &BTreeMap<EntityId, (f64, f64)>) -> f64 {
    let zero = (0.0, 0.0);
    match c {
        Constraint::Near { a, b, within } => sep_residual(pos, a, zero, b, zero, *within as f64, true),
        Constraint::MinSep { a, b, gap } => sep_residual(pos, a, zero, b, zero, *gap as f64, false),
        Constraint::NearPin { a, b, b_off, within } => {
            sep_residual(pos, a, zero, b, (b_off.x as f64, b_off.y as f64), *within as f64, true)
        }
        Constraint::AlignX { nodes } => align_residual(pos, nodes, true),
        Constraint::AlignY { nodes } => align_residual(pos, nodes, false),
        Constraint::NoOverlap { a, b, a_half, b_half } => {
            let (Some(&pa), Some(&pb)) = (pos.get(a), pos.get(b)) else { return 0.0 };
            aabb_push(pa, half_f64(*a_half), pb, half_f64(*b_half)).map_or(0.0, |(d, _, _)| d)
        }
    }
}

fn half_f64(h: (Nm, Nm)) -> (f64, f64) {
    (h.0 as f64, h.1 as f64)
}

/// Penetration of two axis-aligned courtyards (centres `pa`/`pb`, half-extents
/// `ah`/`bh`). Returns `(depth, ux, uy)` — the minimum-translation separation: push
/// `b` by `+depth·(ux,uy)` and `a` by `−depth·(ux,uy)` to just clear. `None` if the
/// boxes are disjoint (touching counts as disjoint). Axis of least penetration; the
/// push sign carries `b` to the far side, deterministic when centres coincide.
fn aabb_push(pa: (f64, f64), ah: (f64, f64), pb: (f64, f64), bh: (f64, f64)) -> Option<(f64, f64, f64)> {
    let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
    let ox = (ah.0 + bh.0) - dx.abs();
    let oy = (ah.1 + bh.1) - dy.abs();
    if ox <= 0.0 || oy <= 0.0 {
        return None;
    }
    if ox <= oy {
        Some((ox, if dx >= 0.0 { 1.0 } else { -1.0 }, 0.0))
    } else {
        Some((oy, 0.0, if dy >= 0.0 { 1.0 } else { -1.0 }))
    }
}

/// Residual of a separation constraint: how far `dist(a+a_off, b+b_off)` is from
/// the satisfied side of `target` (`pull` = Near, so violated when too far).
fn sep_residual(
    pos: &BTreeMap<EntityId, (f64, f64)>,
    a: &EntityId,
    a_off: (f64, f64),
    b: &EntityId,
    b_off: (f64, f64),
    target: f64,
    pull: bool,
) -> f64 {
    let (Some(pa), Some(pb)) = (point_of(pos, a, a_off), point_of(pos, b, b_off)) else {
        return 0.0;
    };
    let d = ((pb.0 - pa.0).powi(2) + (pb.1 - pa.1).powi(2)).sqrt();
    if pull { (d - target).max(0.0) } else { (target - d).max(0.0) }
}

/// Residual of an align constraint: the spread (max − min) of the aligned coordinate
/// over the present nodes. Zero once they share a line; non-zero (and irreducible) if
/// two *fixed* members disagree — which is how a contradictory align is reported.
fn align_residual(pos: &BTreeMap<EntityId, (f64, f64)>, nodes: &[EntityId], x_axis: bool) -> f64 {
    let coord = |p: (f64, f64)| if x_axis { p.0 } else { p.1 };
    let present: Vec<f64> = nodes.iter().filter_map(|n| pos.get(n)).map(|p| coord(*p)).collect();
    match (present.iter().cloned().fold(f64::INFINITY, f64::min), present.iter().cloned().fold(f64::NEG_INFINITY, f64::max)) {
        (lo, hi) if lo.is_finite() && hi.is_finite() => hi - lo,
        _ => 0.0,
    }
}

fn apply_constraint(
    c: &Constraint,
    pos: &mut BTreeMap<EntityId, (f64, f64)>,
    fixed: &BTreeSet<EntityId>,
) {
    let zero = (0.0, 0.0);
    match c {
        Constraint::Near { a, b, within } => {
            set_separation(pos, fixed, a, b, zero, zero, *within as f64, true)
        }
        Constraint::MinSep { a, b, gap } => {
            set_separation(pos, fixed, a, b, zero, zero, *gap as f64, false)
        }
        Constraint::AlignX { nodes } => align(pos, fixed, nodes, true),
        Constraint::AlignY { nodes } => align(pos, fixed, nodes, false),
        Constraint::NearPin { a, b, b_off, within } => {
            let off = (b_off.x as f64, b_off.y as f64);
            set_separation(pos, fixed, a, b, zero, off, *within as f64, true)
        }
        Constraint::NoOverlap { a, b, a_half, b_half } => {
            let (Some(&pa), Some(&pb)) = (pos.get(a), pos.get(b)) else { return };
            let Some((depth, ux, uy)) = aabb_push(pa, half_f64(*a_half), pb, half_f64(*b_half)) else {
                return;
            };
            let (a_fixed, b_fixed) = (fixed.contains(a), fixed.contains(b));
            let (sa, sb) = match (a_fixed, b_fixed) {
                (true, true) => (0.0, 0.0),
                (true, false) => (0.0, 1.0),
                (false, true) => (1.0, 0.0),
                (false, false) => (0.5, 0.5),
            };
            if !a_fixed {
                let pa = pos.get_mut(a).unwrap();
                pa.0 -= depth * ux * sa;
                pa.1 -= depth * uy * sa;
            }
            if !b_fixed {
                let pb = pos.get_mut(b).unwrap();
                pb.0 += depth * ux * sb;
                pb.1 += depth * uy * sb;
            }
        }
    }
}

/// Drive the distance between the points `a + a_off` and `b + b_off` to `target`,
/// but only if violated in the relevant direction: `pull` (Near) acts when too
/// far; otherwise (MinSep) when too close. Offsets let an endpoint be a pin rigidly
/// attached to its node (a constant local offset); corrections are applied to the
/// node positions, which carries the offset with them. Correction is split between
/// the two, or taken wholly by whichever is movable.
#[allow(clippy::too_many_arguments)]
fn set_separation(
    pos: &mut BTreeMap<EntityId, (f64, f64)>,
    fixed: &BTreeSet<EntityId>,
    a: &EntityId,
    b: &EntityId,
    a_off: (f64, f64),
    b_off: (f64, f64),
    target: f64,
    pull: bool,
) {
    let (Some(&pa), Some(&pb)) = (pos.get(a), pos.get(b)) else {
        return;
    };
    let (pa, pb) = ((pa.0 + a_off.0, pa.1 + a_off.1), (pb.0 + b_off.0, pb.1 + b_off.1));
    let (mut dx, mut dy) = (pb.0 - pa.0, pb.1 - pa.1);
    let mut d = (dx * dx + dy * dy).sqrt();
    if d < 1e-6 {
        // Coincident: pick a deterministic axis so we don't divide by zero.
        dx = 1.0;
        dy = 0.0;
        d = 1.0;
    }
    let violated = if pull { d > target } else { d < target };
    if !violated {
        return;
    }
    let (ux, uy) = (dx / d, dy / d);
    let delta = target - d; // move b by +delta*u, a by -delta*u to reach target
    let a_fixed = fixed.contains(a);
    let b_fixed = fixed.contains(b);
    let (sa, sb) = match (a_fixed, b_fixed) {
        (true, true) => (0.0, 0.0),
        (true, false) => (0.0, 1.0),
        (false, true) => (1.0, 0.0),
        (false, false) => (0.5, 0.5),
    };
    if !a_fixed {
        let pa = pos.get_mut(a).unwrap();
        pa.0 -= delta * ux * sa;
        pa.1 -= delta * uy * sa;
    }
    if !b_fixed {
        let pb = pos.get_mut(b).unwrap();
        pb.0 += delta * ux * sb;
        pb.1 += delta * uy * sb;
    }
}

fn align(
    pos: &mut BTreeMap<EntityId, (f64, f64)>,
    fixed: &BTreeSet<EntityId>,
    nodes: &[EntityId],
    x_axis: bool,
) {
    let coord = |p: (f64, f64)| if x_axis { p.0 } else { p.1 };
    // Align to a fixed member if there is one, else to the group mean.
    let target = nodes
        .iter()
        .find(|n| fixed.contains(*n))
        .and_then(|n| pos.get(n).copied())
        .map(coord)
        .unwrap_or_else(|| {
            let present: Vec<f64> = nodes.iter().filter_map(|n| pos.get(n)).map(|p| coord(*p)).collect();
            if present.is_empty() { 0.0 } else { present.iter().sum::<f64>() / present.len() as f64 }
        });
    for n in nodes {
        if fixed.contains(n) {
            continue;
        }
        if let Some(p) = pos.get_mut(n) {
            if x_axis {
                p.0 = target;
            } else {
                p.1 = target;
            }
        }
    }
}
