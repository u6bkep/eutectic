//! Placement/geometry support helpers for the elaboration pass — courtyard lowering,
//! the missing-entity cascade recorder, `help:` line builders, and the solver-problem
//! assembler. Each is called only from [`super::elaborate`].

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::geom::{DEFAULT_CHORD_TOL, Shape2D, convex_hull};
use crate::id::EntityId;
use crate::part::{PartDef, PartLib, courtyard_half_extents, courtyard_shape};
use crate::solve::{Constraint, Problem};
use std::collections::{BTreeMap, BTreeSet};

/// Record (once) that a referenced entity does not exist, and report it as a
/// structural fault. Returns `true` if `id` is missing (so the caller skips it).
/// The `reported_missing` set is the cascade-suppression mechanism: an entity is
/// reported the *first* time it's found missing, and later references are silenced
/// so the genuine fault (its failed/absent instantiation) isn't buried under noise.
pub(super) fn note_missing(
    id: &EntityId,
    components: &BTreeMap<EntityId, Component>,
    reported_missing: &mut BTreeSet<EntityId>,
    errors: &mut Vec<Diagnostic>,
    ctx: &str,
) -> bool {
    if components.contains_key(id) {
        return false;
    }
    if reported_missing.insert(id.clone()) {
        errors.push(Diagnostic::error(
            "E_UNKNOWN_INSTANCE",
            format!("{ctx} references unknown instance `{id}`"),
            Location::Entity(id.clone()),
        ));
    }
    true
}

/// A placed component's courtyard as a **rounded convex polygon** in its local frame,
/// already rotated by `orient` (not translated — the solver adds the node position each
/// sweep). Returns `(vertices, radius)`, the keep-out being `hull(vertices) ⊕
/// disc(radius)`, or `None` for a footprint-less part (no courtyard ⇒ exempt from
/// overlap-avoidance, exactly as before).
///
/// Prefers the real polygonal courtyard ([`courtyard_shape`] — the convex pad hull ⊕
/// margin): this is issue 0019's whole point. A *rotated* part reserves its rotated
/// hull, so neighbours nestle into concavities the axis-aligned box would over-reserve.
/// A part with copper but no 2-D hull (a lone round pad / collinear pads) has no polygon
/// courtyard; it falls back to the axis-aligned box proxy from [`courtyard_half_extents`]
/// (via [`oriented_courtyard`]), lowered as a 4-vertex radius-0 polygon so the identical
/// SAT path serves it and its behaviour is unchanged from the pre-0019 AABB push.
///
/// The SAT push treats the courtyard as convex, so we make that real here rather than
/// assuming it: the courtyard skeleton is **flattened** (arcs → chords within
/// [`DEFAULT_CHORD_TOL`], the same seam `bbox` uses) and run through [`convex_hull`].
/// This matters for an *imported* courtyard, which may be non-convex or have an
/// outward-bowing arc edge — walking corners alone ([`Shape2D::points`]) would drop the
/// arc bulge and under-cover it (the one true under-report path). Hulling the flattened
/// skeleton is arc-safe (the bulge's subdivided points are inside the hull) and
/// idempotent on the already-convex derived pad hull.
pub(super) fn component_courtyard(def: &PartDef, orient: Orient) -> Option<(Vec<Point>, Nm)> {
    if let Some(shape) = courtyard_shape(def) {
        let hull = convex_hull(&shape.path().flatten(DEFAULT_CHORD_TOL));
        if hull.len() >= 3 {
            let verts = hull.into_iter().map(|p| orient.apply(p)).collect();
            return Some((verts, shape.radius()));
        }
        // A degenerate imported courtyard (collinear / <3 distinct points) has no 2-D
        // hull; fall through to the axis-aligned box proxy below.
    }
    let (hw, hh) = oriented_courtyard(def, orient);
    if (hw, hh) == (0, 0) {
        return None;
    }
    Some((
        vec![
            Point { x: hw, y: hh },
            Point { x: -hw, y: hh },
            Point { x: -hw, y: -hh },
            Point { x: hw, y: -hh },
        ],
        0,
    ))
}

/// A part's courtyard half-extents oriented for a placed component. The courtyard is
/// the axis-aligned box `±hw × ±hh`; under the orientation its AABB half-extents are
/// the summed absolute contributions of each rotated axis (so a cardinal 90°/270° turn
/// swaps w/h exactly, and any orientation is handled). Routes through
/// [`Orient::apply`], so it stays exact for cardinals.
pub(super) fn oriented_courtyard(def: &PartDef, orient: Orient) -> (Nm, Nm) {
    let (hw, hh) = courtyard_half_extents(def);
    let ax = orient.apply(Point { x: hw, y: 0 });
    let ay = orient.apply(Point { x: 0, y: hh });
    (ax.x.abs() + ay.x.abs(), ax.y.abs() + ay.y.abs())
}

/// A `help:` line listing a part's distinct functional pin names — the candidates
/// for an unresolved selector (the "did you mean" surface; fuzzy matching later).
pub(super) fn available_pins(def: &PartDef) -> String {
    let mut names: Vec<&str> = def.pins.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    format!("available pins: {}", names.join(", "))
}

/// A `help:` line listing the known part names — candidates for an unknown part.
pub(super) fn known_parts(lib: &PartLib) -> String {
    let names: Vec<&str> = lib.keys().map(String::as_str).collect();
    format!("known parts: {}", names.join(", "))
}

/// Build a solver problem from base placements + overrides + constraints.
/// `suppress` lists override ids to ignore (treat the node as Free at its
/// default) — used to test whether an override is doing anything.
pub(super) fn assemble_problem(
    base: &BTreeMap<EntityId, Point>,
    fixmap: &BTreeMap<EntityId, Point>,
    overrides: &BTreeMap<EntityId, Override>,
    board: Option<&Shape2D>,
    relational: &[Constraint],
    suppress: &BTreeSet<EntityId>,
) -> Problem {
    let mut anchors = BTreeMap::new();
    let mut fixed = BTreeSet::new();
    for (id, default) in base {
        if let Some(fp) = fixmap.get(id) {
            anchors.insert(id.clone(), *fp);
            fixed.insert(id.clone());
            continue;
        }
        let ov = if suppress.contains(id) {
            None
        } else {
            overrides.get(id)
        };
        match ov.and_then(|o| o.pos.map(|p| (p, o.strength))) {
            Some((p, Strength::Pin)) => {
                anchors.insert(id.clone(), p);
                fixed.insert(id.clone());
            }
            Some((p, Strength::Hint)) => {
                anchors.insert(id.clone(), p); // movable soft anchor
            }
            None => {
                anchors.insert(id.clone(), *default);
            }
        }
    }
    Problem {
        anchors,
        fixed,
        board: board.cloned(),
        constraints: relational.to_vec(),
    }
}
