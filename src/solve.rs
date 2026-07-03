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
use crate::geom::Shape2D;
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
    Near {
        a: EntityId,
        b: EntityId,
        within: Nm,
    },
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
    NearPin {
        a: EntityId,
        b: EntityId,
        b_off: Point,
        within: Nm,
    },
    /// Two component courtyards must not overlap (issues 0005 / 0019). Each courtyard
    /// is a **rounded convex polygon** — a hull of vertices ⊕ a disc of `radius` (the
    /// courtyard margin) — given in the node's local frame *already rotated by its
    /// orientation* but not translated, so its world vertices are `pos[node] + vertex`.
    /// A footprint whose only proxy is an axis-aligned box arrives as a 4-vertex
    /// polygon with `radius = 0`, so the same code path serves both (issue 0019
    /// replaced the axis-aligned-box push with exact convex-polygon SAT; the box is now
    /// just a degenerate polygon).
    ///
    /// When they overlap the nodes are pushed apart along the minimum-translation axis
    /// (see [`poly_push`]). Two overlapping *fixed* parts cannot be separated and are
    /// reported as unsatisfied.
    NoOverlap {
        a: EntityId,
        b: EntityId,
        a_poly: Vec<Point>,
        a_r: Nm,
        b_poly: Vec<Point>,
        b_r: Nm,
    },
}

pub struct Problem {
    /// Anchor (also the initial position) of every node.
    pub anchors: BTreeMap<EntityId, Point>,
    /// Nodes that cannot move (Fixed / Pinned provenance).
    pub fixed: BTreeSet<EntityId>,
    /// Board boundary as the substrate [`Shape2D::Area`] (outline ∖ cutouts); movable
    /// nodes are kept inside the filled area and out of its holes.
    pub board: Option<Shape2D>,
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

/// Tolerance (nm) for the honest courtyard-overlap verify. The solver drives every
/// `NoOverlap` residual — the courtyard penetration depth — below `RES_TOL` (1 µm) at
/// convergence, so a converged *movable* pair carries at most that, plus sub-nm
/// integer-rounding of the final positions. This threshold is a small multiple of
/// `RES_TOL` (3 µm) so that convergence slop is not reported as a collision, while a
/// genuine unresolvable overlap — two fixed parts pinned into each other — is caught: it
/// penetrates by tens of µm or more, never a few. Using `PLACE_TOL` (0.1 mm) here would
/// silently swallow a 50 µm fixed/fixed collision, so this is deliberately tighter.
pub const COURTYARD_VERIFY_TOL: Nm = 3 * RES_TOL as Nm; // 3 µm ≈ 3·RES_TOL

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
                let pt = Point {
                    x: pp.0.round() as Nm,
                    y: pp.1.round() as Nm,
                };
                if !board.contains_point(pt) {
                    let q = board.closest_boundary_point(pt);
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
        .map(|(k, &(x, y))| {
            (
                k.clone(),
                Point {
                    x: x.round() as i64,
                    y: y.round() as i64,
                },
            )
        })
        .collect();

    let unsatisfied = if converged {
        Vec::new()
    } else {
        p.constraints
            .iter()
            .filter_map(|c| {
                let r = constraint_residual(c, &pos);
                (r > RES_TOL).then(|| Unsatisfied {
                    constraint: c.clone(),
                    residual: r.round() as Nm,
                })
            })
            .collect()
    };

    Solution {
        positions,
        converged,
        iters,
        unsatisfied,
    }
}

/// World point of `id` plus a local offset (used for pins rigidly attached to a node).
fn point_of(
    pos: &BTreeMap<EntityId, (f64, f64)>,
    id: &EntityId,
    off: (f64, f64),
) -> Option<(f64, f64)> {
    pos.get(id).map(|p| (p.0 + off.0, p.1 + off.1))
}

/// How far a constraint is from satisfied, in nm (0.0 = satisfied). The residual the
/// convergence loop drives to zero and the value reported on infeasibility.
fn constraint_residual(c: &Constraint, pos: &BTreeMap<EntityId, (f64, f64)>) -> f64 {
    let zero = (0.0, 0.0);
    match c {
        Constraint::Near { a, b, within } => {
            sep_residual(pos, a, zero, b, zero, *within as f64, true)
        }
        Constraint::MinSep { a, b, gap } => sep_residual(pos, a, zero, b, zero, *gap as f64, false),
        Constraint::NearPin {
            a,
            b,
            b_off,
            within,
        } => sep_residual(
            pos,
            a,
            zero,
            b,
            (b_off.x as f64, b_off.y as f64),
            *within as f64,
            true,
        ),
        Constraint::AlignX { nodes } => align_residual(pos, nodes, true),
        Constraint::AlignY { nodes } => align_residual(pos, nodes, false),
        Constraint::NoOverlap {
            a,
            b,
            a_poly,
            a_r,
            b_poly,
            b_r,
        } => {
            let (Some(&pa), Some(&pb)) = (pos.get(a), pos.get(b)) else {
                return 0.0;
            };
            let aw = world_poly(a_poly, pa);
            let bw = world_poly(b_poly, pb);
            poly_push(&aw, *a_r, &bw, *b_r).map_or(0.0, |(d, _, _)| d)
        }
    }
}

/// The world integer vertices of a rotated local polygon translated to a node whose
/// (f64) position is `pos`, rounded to nm. The solver's working state is f64, but the
/// separation *decision* is exact i128 on these integer vertices — the containment
/// clamp rounds to nm the same way, so the two stay consistent.
fn world_poly(local: &[Point], pos: (f64, f64)) -> Vec<Point> {
    let (ox, oy) = (pos.0.round() as Nm, pos.1.round() as Nm);
    local
        .iter()
        .map(|p| Point {
            x: p.x + ox,
            y: p.y + oy,
        })
        .collect()
}

/// Minimum-translation push separating two **rounded convex polygons** — hulls `a`/`b`
/// (world integer vertices, either winding) each ⊕ a disc of radius `ar`/`br`. Returns
/// `(depth, ux, uy)`: push `b` by `+depth·(ux,uy)` and `a` by `−depth·(ux,uy)` to just
/// clear; `None` when the rounded shapes are disjoint (touching counts as disjoint).
///
/// Exact convex-polygon SAT. The candidate separating axes are every edge normal of
/// each hull (covering edge/edge and vertex/edge contact) **plus** every vertex-to-
/// vertex direction (covering the rounded corner/corner case, whose separating axis is
/// no edge normal). For two convex hulls the closest-feature direction is always in
/// this set, so the axis of greatest separation — equivalently least penetration once
/// the disc radii are folded in — is found exactly. The overlap test and axis choice
/// are exact i128 (dot products of integer vertices with integer axis vectors); only
/// the returned magnitude and unit direction are f64, applied to the f64 working state.
///
/// Determinism: axes are visited in a fixed order (a's edges, b's edges, then vertex
/// pairs in nested order); the disjoint test short-circuits on the first separating
/// axis; the penetration axis is chosen by strict `<`, so the first minimiser wins.
fn poly_push(a: &[Point], ar: Nm, b: &[Point], br: Nm) -> Option<(f64, f64, f64)> {
    // Coordinate-magnitude bound. The exact overlap test squares a scaled projection
    // gap in i128; that stays in range while vertex coordinates stay within
    // [`geom::MAX_COORD`] (±1 m). Beyond that the i128 products can overflow. Mirroring
    // `Orient::apply`, the invariant is asserted in debug rather than range-checked
    // here; the guarantee in release is the ingest-boundary `E_COORD_RANGE` validation
    // that now enforces `MAX_COORD` crate-wide (issue 0018, resolved).
    debug_assert!(
        a.iter().chain(b).all(|&p| crate::geom::point_ok(p)),
        "poly_push vertex magnitude exceeds MAX_COORD (issue 0018)"
    );
    let r = (ar + br) as i128; // combined disc radius (nm); the rounded margin
    let r2 = r * r;

    // Track the minimum-penetration axis (the MTV) across all candidate axes.
    let mut best_pen = f64::INFINITY;
    let mut best_dir = (0.0_f64, 0.0_f64);

    let mut consider = |nx: i128, ny: i128| -> Option<()> {
        if nx == 0 && ny == 0 {
            return Some(()); // a zero axis (coincident vertices / degenerate edge)
        }
        let n2 = nx * nx + ny * ny;
        let proj = |poly: &[Point]| {
            let mut lo = i128::MAX;
            let mut hi = i128::MIN;
            for p in poly {
                let d = p.x as i128 * nx + p.y as i128 * ny;
                lo = lo.min(d);
                hi = hi.max(d);
            }
            (lo, hi)
        };
        let (a_lo, a_hi) = proj(a);
        let (b_lo, b_hi) = proj(b);
        // Signed hull overlap along this axis, in |n|-scaled units.
        let depth = a_hi.min(b_hi) - a_lo.max(b_lo);
        if depth <= 0 {
            // Hull projections disjoint on this axis by gap = -depth (scaled). The
            // rounded shapes clear iff that real gap ≥ r, i.e. gap² ≥ r²·|n|² (exact).
            let gap = -depth;
            if gap * gap >= r2 * n2 {
                return None; // separating axis found ⇒ shapes disjoint
            }
        }
        // No separation on this axis: real penetration is depth/|n| + r. Fold the disc
        // radius into the depth and keep the least-penetration axis for the MTV.
        let inv_len = 1.0 / (n2 as f64).sqrt();
        let pen = depth as f64 * inv_len + r as f64;
        if pen < best_pen {
            best_pen = pen;
            // Orient the axis so it carries b to the side it already lies on.
            let sign = if (b_lo + b_hi) - (a_lo + a_hi) >= 0 {
                1.0
            } else {
                -1.0
            };
            best_dir = (nx as f64 * inv_len * sign, ny as f64 * inv_len * sign);
        }
        Some(())
    };

    // Edge normals of each hull: edge (dx,dy) ⇒ normal (dy,-dx). `consider` yields
    // `None` on the first axis that separates the shapes, which `?` turns into an early
    // "disjoint ⇒ no push" return from `poly_push`.
    for poly in [a, b] {
        let n = poly.len();
        for i in 0..n {
            let p = poly[i];
            let q = poly[(i + 1) % n];
            consider((q.y - p.y) as i128, -((q.x - p.x) as i128))?;
        }
    }
    // Vertex-to-vertex directions (the rounded corner/corner separating axes).
    for &pa in a {
        for &pb in b {
            consider((pb.x - pa.x) as i128, (pb.y - pa.y) as i128)?;
        }
    }

    if best_pen.is_finite() && best_pen > 0.0 {
        Some((best_pen, best_dir.0, best_dir.1))
    } else {
        None
    }
}

/// Penetration depth (nm) of two rounded convex courtyards at integer **world**
/// vertices, or `0.0` when disjoint (touching counts as disjoint). The honest verify's
/// measurement (Decision 10's third leg): exact-i128 SAT decision, run on the solver's
/// final placement against the real polygon courtyards. Reported against a tolerance,
/// not `> 0`, because the solver converges only to within [`RES_TOL`] — a sub-µm
/// residual is convergence slop, not a collision, so a bare "overlaps?" bool would fire
/// on every normal placement. A thin wrapper over [`poly_push`].
pub fn courtyard_overlap_depth(a: &[Point], ar: Nm, b: &[Point], br: Nm) -> f64 {
    poly_push(a, ar, b, br).map_or(0.0, |(d, _, _)| d)
}

/// True iff two rounded convex courtyards strictly overlap (touching counts as
/// disjoint) — the exact geometric predicate, no tolerance. Used by tests and callers
/// wanting the raw truth; the honest verify uses [`courtyard_overlap_depth`] with a
/// tolerance.
pub fn courtyards_overlap(a: &[Point], ar: Nm, b: &[Point], br: Nm) -> bool {
    poly_push(a, ar, b, br).is_some()
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
    if pull {
        (d - target).max(0.0)
    } else {
        (target - d).max(0.0)
    }
}

/// Residual of an align constraint: the spread (max − min) of the aligned coordinate
/// over the present nodes. Zero once they share a line; non-zero (and irreducible) if
/// two *fixed* members disagree — which is how a contradictory align is reported.
fn align_residual(pos: &BTreeMap<EntityId, (f64, f64)>, nodes: &[EntityId], x_axis: bool) -> f64 {
    let coord = |p: (f64, f64)| if x_axis { p.0 } else { p.1 };
    let present: Vec<f64> = nodes
        .iter()
        .filter_map(|n| pos.get(n))
        .map(|p| coord(*p))
        .collect();
    match (
        present.iter().cloned().fold(f64::INFINITY, f64::min),
        present.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    ) {
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
        Constraint::NearPin {
            a,
            b,
            b_off,
            within,
        } => {
            let off = (b_off.x as f64, b_off.y as f64);
            set_separation(pos, fixed, a, b, zero, off, *within as f64, true)
        }
        Constraint::NoOverlap {
            a,
            b,
            a_poly,
            a_r,
            b_poly,
            b_r,
        } => {
            let (Some(&pa), Some(&pb)) = (pos.get(a), pos.get(b)) else {
                return;
            };
            let aw = world_poly(a_poly, pa);
            let bw = world_poly(b_poly, pb);
            let Some((depth, ux, uy)) = poly_push(&aw, *a_r, &bw, *b_r) else {
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
    let (pa, pb) = (
        (pa.0 + a_off.0, pa.1 + a_off.1),
        (pb.0 + b_off.0, pb.1 + b_off.1),
    );
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
            let present: Vec<f64> = nodes
                .iter()
                .filter_map(|n| pos.get(n))
                .map(|p| coord(*p))
                .collect();
            if present.is_empty() {
                0.0
            } else {
                present.iter().sum::<f64>() / present.len() as f64
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A square courtyard hull, half-side `s`, centred at `(cx, cy)` (world nm).
    fn square(cx: Nm, cy: Nm, s: Nm) -> Vec<Point> {
        vec![
            Point {
                x: cx + s,
                y: cy + s,
            },
            Point {
                x: cx - s,
                y: cy + s,
            },
            Point {
                x: cx - s,
                y: cy - s,
            },
            Point {
                x: cx + s,
                y: cy - s,
            },
        ]
    }

    // ---- SAT correctness pins: overlapping / touching / separated / contained ----

    #[test]
    fn sat_overlapping_squares_push_apart() {
        // Two 1000-half squares (radius 0), centres 500 apart on x: they overlap by
        // 2·1000 − 500 = 1500 along x, and x is the min-penetration axis.
        let a = square(0, 0, 1000);
        let b = square(500, 0, 1000);
        let (depth, ux, uy) = poly_push(&a, 0, &b, 0).expect("overlap");
        assert!((depth - 1500.0).abs() < 1e-6, "depth {depth}");
        assert!(
            (ux - 1.0).abs() < 1e-9 && uy.abs() < 1e-9,
            "push along +x: ({ux},{uy})"
        );
    }

    #[test]
    fn sat_touching_squares_are_disjoint() {
        // Edge-to-edge (centres 2000 apart, each 1000 half): touching counts as clear.
        assert!(poly_push(&square(0, 0, 1000), 0, &square(2000, 0, 1000), 0).is_none());
    }

    #[test]
    fn sat_separated_squares_no_push() {
        assert!(poly_push(&square(0, 0, 1000), 0, &square(5000, 0, 1000), 0).is_none());
    }

    #[test]
    fn sat_contained_square_pushes_out() {
        // A small square wholly inside a large one still overlaps (Some push).
        assert!(poly_push(&square(0, 0, 200), 0, &square(0, 0, 2000), 0).is_some());
    }

    // ---- The disc (courtyard-margin) radius folds into the separation exactly. ----

    #[test]
    fn sat_radius_separation_is_the_sum_of_radii() {
        // Hulls 1000-half, centres 2000 apart ⇒ hull edges touch (gap 0). With radii
        // 400 + 600 = 1000 the rounded shapes overlap by exactly 1000.
        let a = square(0, 0, 1000);
        let b = square(2000, 0, 1000);
        let (depth, _, _) = poly_push(&a, 400, &b, 600).expect("rounded overlap");
        assert!((depth - 1000.0).abs() < 1e-6, "depth {depth}");
        // Move them so the hull gap equals the summed radii ⇒ exactly touching ⇒ clear.
        assert!(poly_push(&a, 400, &square(3000, 0, 1000), 600).is_none());
        // One nm closer than that ⇒ overlap.
        assert!(poly_push(&a, 400, &square(2999, 0, 1000), 600).is_some());
    }

    // ---- Rotation: a rotated hull is NOT its axis-aligned box (issue 0019). ----

    #[test]
    fn sat_rotated_hull_beats_the_aabb_proxy() {
        // `a` is a diamond (a 45°-rotated square): vertices on the axes at ±1414. Its
        // axis-aligned bounding box is [-1414, 1414]². `b` is a small square parked in
        // that box's corner at (1100, 1100) — inside the AABB, but well outside the
        // diamond (|1100|+|1100| = 2200 > 1414). The polygon SAT clears them; an AABB
        // proxy (the pre-0019 push) would have seen the corner as occupied and shoved.
        let diamond = vec![
            Point { x: 1414, y: 0 },
            Point { x: 0, y: 1414 },
            Point { x: -1414, y: 0 },
            Point { x: 0, y: -1414 },
        ];
        let b = square(1100, 1100, 100);
        assert!(
            poly_push(&diamond, 0, &b, 0).is_none(),
            "rotated diamond clears the corner square the AABB would flag"
        );
        // Sanity: the same square at the diamond's centre does overlap.
        assert!(poly_push(&diamond, 0, &square(0, 0, 100), 0).is_some());
    }

    #[test]
    fn sat_corner_corner_separates_where_edge_normals_would_not() {
        // Two 1000-half squares at (0,0) and (2500,2500), radii 300 + 300 = 600. On the
        // x and y edge normals the hull gap is only 1500 − 1000 = 500 < 600, so those
        // axes alone report overlap. The true separating axis is the corner-to-corner
        // diagonal: nearest corners (1000,1000) and (1500,1500) are √(500²+500²) ≈ 707
        // apart ≥ 600, so the rounded shapes clear. The vertex-vertex axes catch this.
        let a = square(0, 0, 1000);
        let b = square(2500, 2500, 1000);
        assert!(
            poly_push(&a, 300, &b, 300).is_none(),
            "corner-corner must separate"
        );
        // A hair closer: at (2400,2400) the nearest corners (1000,1000)/(1400,1400) are
        // √(400²+400²) ≈ 566 apart < 600 ⇒ the rounded shapes overlap.
        assert!(poly_push(&a, 300, &square(2400, 2400, 1000), 300).is_some());
    }

    #[test]
    fn courtyards_overlap_matches_push() {
        assert!(courtyards_overlap(
            &square(0, 0, 1000),
            0,
            &square(500, 0, 1000),
            0
        ));
        assert!(!courtyards_overlap(
            &square(0, 0, 1000),
            0,
            &square(3000, 0, 1000),
            0
        ));
    }

    // ---- The relaxation loop converges (no oscillation) on a crowded cluster. ----

    #[test]
    fn crowded_cluster_converges_without_overlap() {
        use crate::id::EntityId;
        // Six identical square courtyards all dropped at the origin. The Gauss-Seidel
        // NoOverlap projections must fan them out and settle (converge), leaving no
        // residual overlap — the anti-oscillation check.
        let ids: Vec<EntityId> = (0..6).map(|i| EntityId::new(format!("c{i}"))).collect();
        let local = square(0, 0, 1000); // centred hull, in local frame
        let mut anchors = BTreeMap::new();
        for id in &ids {
            anchors.insert(id.clone(), Point { x: 0, y: 0 });
        }
        let mut constraints = Vec::new();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                constraints.push(Constraint::NoOverlap {
                    a: ids[i].clone(),
                    a_poly: local.clone(),
                    a_r: 250_000,
                    b: ids[j].clone(),
                    b_poly: local.clone(),
                    b_r: 250_000,
                });
            }
        }
        let p = Problem {
            anchors,
            fixed: BTreeSet::new(),
            board: None,
            constraints,
        };
        let sol = solve(&p);
        assert!(
            sol.converged,
            "crowded cluster must converge (iters {})",
            sol.iters
        );
        // Every pair clears at the final placement, up to the solver's sub-µm residual
        // (the honest verify's own tolerance) — no oscillation, no gross residual.
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let wa: Vec<Point> = local
                    .iter()
                    .map(|q| Point {
                        x: q.x + sol.positions[&ids[i]].x,
                        y: q.y + sol.positions[&ids[i]].y,
                    })
                    .collect();
                let wb: Vec<Point> = local
                    .iter()
                    .map(|q| Point {
                        x: q.x + sol.positions[&ids[j]].x,
                        y: q.y + sol.positions[&ids[j]].y,
                    })
                    .collect();
                let depth = courtyard_overlap_depth(&wa, 250_000, &wb, 250_000);
                assert!(
                    depth <= PLACE_TOL as f64,
                    "pair {i},{j} overlaps by {depth} nm"
                );
            }
        }
    }

    // ---- Honest verify: two fixed parts pushed into each other cannot separate. ----

    #[test]
    fn fixed_overlap_is_unsatisfiable() {
        use crate::id::EntityId;
        let (a, b) = (EntityId::new("a"), EntityId::new("b"));
        let local = square(0, 0, 1000);
        let mut anchors = BTreeMap::new();
        anchors.insert(a.clone(), Point { x: 0, y: 0 });
        anchors.insert(b.clone(), Point { x: 500, y: 0 }); // overlapping
        let mut fixed = BTreeSet::new();
        fixed.insert(a.clone());
        fixed.insert(b.clone());
        let p = Problem {
            anchors,
            fixed,
            board: None,
            constraints: vec![Constraint::NoOverlap {
                a: a.clone(),
                a_poly: local.clone(),
                a_r: 0,
                b: b.clone(),
                b_poly: local.clone(),
                b_r: 0,
            }],
        };
        let sol = solve(&p);
        assert!(
            !sol.converged,
            "two fixed overlapping parts cannot be separated"
        );
        assert_eq!(sol.unsatisfied.len(), 1);
    }
}
