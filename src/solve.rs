//! A deterministic least-change placement solver.
//!
//! This is a relaxation/constraint-projection solver, deliberately simple:
//! positions start at each node's anchor and only move to satisfy constraints, so
//! an unconstrained part stays put (least change — the "why did it jump across the
//! board" antidote). A weak pull toward the anchor keeps under-determined DOFs
//! near where they were; relational constraints, applied after the anchor pull
//! each iteration, win when they disagree.
//!
//! It is NOT a production geometric constraint solver (no DOF analysis, no
//! decomposition, no guaranteed global optimum). It is enough to make "least
//! change" and constraint-driven placement real, and to give the reconciliation
//! layer a principled definition of an *ineffective* override: one that, when
//! freed, the solver puts back in the same place.
//!
//! Determinism: no RNG, fixed iteration count, BTreeMap/Vec iteration order is
//! stable, and the f64 working math is rounded to integer nm on output so stored
//! values stay exact and canonical.

use crate::doc::{Nm, Point};
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
}

pub struct Problem {
    /// Anchor (also the initial position) of every node.
    pub anchors: BTreeMap<EntityId, Point>,
    /// Nodes that cannot move (Fixed / Pinned provenance).
    pub fixed: BTreeSet<EntityId>,
    pub board: Option<Rect>,
    pub constraints: Vec<Constraint>,
}

const ITERS: usize = 300;
/// Weak pull toward the anchor each iteration: small enough that constraints win,
/// large enough that under-constrained DOFs settle back to their anchor.
const ANCHOR_W: f64 = 0.10;
/// Two placements within this distance are "the same" for effectiveness checks.
pub const PLACE_TOL: Nm = 100_000; // 0.1 mm

pub fn dist(a: Point, b: Point) -> f64 {
    let dx = (a.x - b.x) as f64;
    let dy = (a.y - b.y) as f64;
    (dx * dx + dy * dy).sqrt()
}

pub fn solve(p: &Problem) -> BTreeMap<EntityId, Point> {
    let mut pos: BTreeMap<EntityId, (f64, f64)> = p
        .anchors
        .iter()
        .map(|(k, v)| (k.clone(), (v.x as f64, v.y as f64)))
        .collect();
    let anchor = pos.clone();

    for _ in 0..ITERS {
        // Weak anchor pull (movable nodes only).
        for (id, a) in &anchor {
            if p.fixed.contains(id) {
                continue;
            }
            let pp = pos.get_mut(id).unwrap();
            pp.0 += ANCHOR_W * (a.0 - pp.0);
            pp.1 += ANCHOR_W * (a.1 - pp.1);
        }
        // Relational constraints (hard projection; later ones win within an iter).
        for c in &p.constraints {
            apply_constraint(c, &mut pos, &p.fixed);
        }
        // Containment has the last word.
        if let Some(r) = &p.board {
            for (id, pp) in pos.iter_mut() {
                if p.fixed.contains(id) {
                    continue;
                }
                pp.0 = pp.0.clamp(r.min.x as f64, r.max.x as f64);
                pp.1 = pp.1.clamp(r.min.y as f64, r.max.y as f64);
            }
        }
    }

    pos.into_iter()
        .map(|(k, (x, y))| (k, Point { x: x.round() as i64, y: y.round() as i64 }))
        .collect()
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
