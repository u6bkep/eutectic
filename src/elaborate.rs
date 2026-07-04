//! Elaboration: generative source -> materialized instances, with ID-keyed
//! overrides reconciled on top.
//!
//! This is the load-bearing primitive of the whole architecture, exercised here
//! at the schematic-authoring level: clean generative truth + override deltas +
//! reconciliation. The same shape recurs at placement and routing.
//!
//! Reconciliation rules:
//!   - re-elaborating the same source reproduces the same entity ids (paths),
//!     so an override stays attached across a source change (minimal perturbation).
//!   - an override whose target no longer exists is *surfaced as a conflict*,
//!     never silently dropped.
//!
//! The facade keeps the [`Elaborated`] type and the main [`elaborate`] pass; the
//! generative expansion engine, the read-only source projections, the demo builders,
//! and the placement/geometry support helpers live in the private submodules below and
//! are re-exported so every existing `crate::elaborate::{...}` path keeps compiling.

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::geom::{Role, Shape2D};
use crate::id::{EntityId, NetId};
use crate::part::PartLib;
use crate::solve::{
    COURTYARD_VERIFY_TOL, Constraint, PLACE_TOL, courtyard_overlap_depth, dist, solve,
};
use std::collections::{BTreeMap, BTreeSet};

mod builders;
mod expand;
mod expr;
mod query;
mod support;

#[cfg(test)]
mod tests;

use expand::expand_generative;
// Kept reachable at `crate::elaborate::MAX_DEF_DEPTH` for the intra-doc links in
// `crate::schematic` (the depth cap is the shared fault-class reference). Re-exported
// (not a plain `use`) so the reference stays a resolvable path rather than an
// unused-import.
pub use expand::MAX_DEF_DEPTH;
use support::{assemble_problem, available_pins, component_courtyard, known_parts, note_missing};

// Re-export the read-only source projections and demo builders so every existing
// `crate::elaborate::{...}` path keeps compiling unchanged.
pub use builders::{connect_interface, psu_module, ring};
pub use query::{board_region, features, font_load_failure, regions, resolve_font, stackup};

// The directive IR (RegionDecl, GenDirective, Source, DefNode, MAX_RANGE_INSTANCES
// and the coords/refs queries) lives in `crate::ir`, the common downward dependency of
// both `text` and `elaborate`. Re-exported here so every existing
// `crate::elaborate::{...}` path keeps compiling unchanged.
pub use crate::ir::{
    DefNode, GenDirective, MAX_RANGE_INSTANCES, RegionDecl, Source, board_rect, directive_coords,
    directive_refs,
};

/// Result of elaboration before it is folded into a Doc.
pub struct Elaborated {
    pub components: BTreeMap<EntityId, Component>,
    pub nets: BTreeMap<NetId, Net>,
    pub no_connects: BTreeSet<PinRef>,
    pub report: ReconReport,
    /// Instance paths a false `if=` population conditional depopulated (Decision 21b DNP).
    /// These are *intentionally* absent from `components` — not faults, not typos — so a
    /// consumer distinguishing "unknown to the source" from "deliberately unpopulated"
    /// (e.g. the schematic-layout gate, Decision 20c) reads this rather than treating an
    /// absent path as an error. Empty when no `if=` dropped anything.
    pub dnp_dropped: BTreeSet<String>,
    /// Per-instance stamped schematic layout fragments (Decision 20 embedded in a def),
    /// keyed by def-instance path (`sense[0]`). Each is the def's internal `schematic { … }`
    /// fragment with every `sym` path / wire endpoint prefixed by the instance path, so the
    /// derived reflow can expand a doc-level `sym <instance>` into the fragment's placements
    /// (a reused circuit renders identically at every instantiation). Empty when no
    /// instantiated def carries a layout fragment.
    pub def_fragments: BTreeMap<String, crate::schematic::SchematicLayout>,
}

/// Elaborate a source program into materialized instances + connectivity,
/// applying ID-keyed overrides. On a structural fault the whole elaboration aborts
/// (atomic transaction) and returns **all** independent faults it found in one pass
/// (collect-all), suppressing only the cascade from a poisoned entity. Findings on
/// a *valid* model (reconciliation outcomes) ride in the returned [`Elaborated`]'s
/// [`ReconReport`], not in this error channel.
pub fn elaborate(
    source: &Source,
    overrides: &BTreeMap<EntityId, Override>,
    refdes_pins: &BTreeMap<EntityId, String>,
    lib: &PartLib,
) -> Result<Elaborated, Vec<Diagnostic>> {
    // Lower the generative tier (params, ranged/conditional/expression `inst`) into
    // concrete declarative directives *first*, so every pass below — including
    // reconciliation, which addresses instances by their `path[i]` — sees only plain
    // `Instance` directives (Decision 21b). A generative fault (bad expression, cycle,
    // out-of-range bound) aborts the whole transaction, like any structural fault.
    let (expanded, dnp_dropped, def_fragments) = expand_generative(source, lib)?;
    let source = &expanded;

    let mut components: BTreeMap<EntityId, Component> = BTreeMap::new();
    let mut nets: BTreeMap<NetId, Net> = BTreeMap::new();
    let mut order = 0i64; // deterministic default placement counter

    // Collect-all: accumulate structural faults instead of returning the first.
    // `reported_missing` is the cascade-suppression set — an entity that does not
    // exist (failed to instantiate, or never declared) is reported once, and all
    // later references to it are silenced so the real fault isn't buried.
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut reported_missing: BTreeSet<EntityId> = BTreeSet::new();
    // A path depopulated by a false `if=` (Decision 21b DNP) is *intentionally* absent,
    // not a fault: seed it into the cascade-suppression set so **every** reference to it
    // (connection *or* placement) skips silently via `note_missing` instead of raising
    // `E_UNKNOWN_INSTANCE`. The dangling references are surfaced uniformly as `W_DNP`
    // warnings by the single scan below (symmetric across directive kinds — a `near` on a
    // depopulated part is as visible as a `net` on it). `dnp_dangling` collects those.
    let mut dnp_dangling: Vec<(String, String)> = Vec::new();
    for p in &dnp_dropped {
        reported_missing.insert(EntityId::new(p.clone()));
    }
    // A reference is "into a dropped subtree" if its path equals a dropped path *or* lies
    // beneath one (`<dropped>.…`). The latter matters for a `def` instance depopulated by
    // `if=false` (Decision 21a): the whole stamped subtree is never materialized, so a
    // ref to a *leaf pin* of that module (`net OUT a.R1.p2` when `inst a … if=false`) is
    // as intentionally-absent as a ref to `a` itself — it must degrade to `W_DNP`, not
    // hard-error `E_UNKNOWN_INSTANCE`. The prefix rule captures both.
    let is_dnp_dropped = |path: &str| -> bool {
        dnp_dropped.iter().any(|d| {
            path == d.as_str() || path.starts_with(d.as_str()) && path[d.len()..].starts_with('.')
        })
    };
    if !dnp_dropped.is_empty() {
        for d in source {
            for (ctx, path) in directive_refs(d) {
                if is_dnp_dropped(&path) {
                    // Pre-seed the cascade-suppression set so the pass that would resolve
                    // this ref finds it already "reported" and skips it silently (no
                    // `E_UNKNOWN_INSTANCE`), exactly as an exact dropped-path ref is
                    // suppressed. This handles deep refs (`a.R1`) whose specific id was
                    // never in `dnp_dropped` (which holds only the dropped instance path).
                    reported_missing.insert(EntityId::new(path.clone()));
                    dnp_dangling.push((ctx, path));
                }
            }
        }
    }

    // Pass 1: instances.
    for d in source {
        if let GenDirective::Instance {
            path,
            part,
            params,
            label,
        } = d
        {
            let id = EntityId::new(path.clone());
            if !lib.contains_key(part) {
                errors.push(
                    Diagnostic::error(
                        "E_UNKNOWN_PART",
                        format!("instance `{path}` uses unknown part `{part}`"),
                        Location::Entity(id.clone()),
                    )
                    .with_help(known_parts(lib)),
                );
                // Poison: the instance does not exist, so suppress its cascade.
                reported_missing.insert(id);
                continue;
            }
            if components.contains_key(&id) {
                // The first definition wins; the entity exists, so it is NOT poisoned.
                errors.push(Diagnostic::error(
                    "E_DUPLICATE_INSTANCE",
                    format!("duplicate instance `{path}`"),
                    Location::Entity(id),
                ));
                continue;
            }
            // Default placement: a free DOF, laid out in a row.
            let pos = Dof {
                value: Point {
                    x: order * 10 * MM,
                    y: 0,
                },
                prov: Provenance::Free,
            };
            order += 1;
            components.insert(
                id.clone(),
                Component {
                    id,
                    part: part.clone(),
                    pos,
                    orient: Orient::default(),
                    params: params.clone(),
                    label: label.clone(),
                },
            );
        }
    }

    // Pass 2: source-provided default placement (still free).
    for d in source {
        if let GenDirective::Place { path, pos } = d {
            let id = EntityId::new(path.clone());
            if note_missing(
                &id,
                &components,
                &mut reported_missing,
                &mut errors,
                "place",
            ) {
                continue;
            }
            components
                .get_mut(&id)
                .expect("note_missing confirmed presence")
                .pos
                .value = *pos;
        }
    }

    // Pass 2a: orientation (a settable attribute, resolved before constraints so a
    // NearPin target's pin offset can be rotated correctly).
    for d in source {
        if let GenDirective::Rotate { path, orient } = d {
            let id = EntityId::new(path.clone());
            if note_missing(
                &id,
                &components,
                &mut reported_missing,
                &mut errors,
                "rotate",
            ) {
                continue;
            }
            // The quaternion is already valid by construction (the text front-end lowers
            // any angle, so there is no off-axis rejection anymore) — just assign it.
            components
                .get_mut(&id)
                .expect("note_missing confirmed presence")
                .orient = *orient;
        }
    }

    // Pass 2b: collect hard placement constraints (Fix), the board outline, and
    // relational constraints for the solver.
    let mut fixmap: BTreeMap<EntityId, Point> = BTreeMap::new();
    // The board region (outline ∖ cutouts) as a `Shape2D::Area`; movable components are
    // kept inside it (and out of its holes) by the solver.
    let board = board_region(source).map(|region| Shape2D::Area { region });
    let mut relational: Vec<Constraint> = Vec::new();
    for d in source {
        match d {
            GenDirective::Fix { path, pos } => {
                let id = EntityId::new(path.clone());
                if note_missing(&id, &components, &mut reported_missing, &mut errors, "fix") {
                    continue;
                }
                fixmap.insert(id, *pos);
            }
            GenDirective::Near { a, b, within } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                // Evaluate both so both are reported if both are missing.
                let am = note_missing(&a, &components, &mut reported_missing, &mut errors, "near");
                let bm = note_missing(&b, &components, &mut reported_missing, &mut errors, "near");
                if am || bm {
                    continue;
                }
                relational.push(Constraint::Near {
                    a,
                    b,
                    within: *within,
                });
            }
            GenDirective::NearPin {
                a,
                b_comp,
                b_pin,
                within,
            } => {
                let aid = EntityId::new(a.clone());
                let bid = EntityId::new(b_comp.clone());
                let am = note_missing(
                    &aid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "nearpin",
                );
                let bm = note_missing(
                    &bid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "nearpin",
                );
                if am || bm {
                    continue;
                }
                // Pre-rotate the target pin's local offset by b's orientation; the
                // result is a constant offset the solver adds to b's position.
                let bc = &components[&bid];
                let bdef = &lib[&bc.part];
                // A selector may name several pads (a power rail); for a geometric
                // anchor we target the first by pad order — deterministic and enough
                // for a placement hint.
                match bdef.resolve_selector(b_pin).into_iter().next() {
                    // A discrete pad always has an offset; an interface signal could
                    // be in `signals` but absent from `offsets` (a malformed
                    // InterfaceDef) — surface that, never panic.
                    Some(num) => match bdef.pin_offset(&num) {
                        Some(off) => relational.push(Constraint::NearPin {
                            a: aid,
                            b: bid,
                            b_off: bc.orient.apply(off),
                            within: *within,
                        }),
                        None => errors.push(Diagnostic::error(
                            "E_PIN_NO_OFFSET",
                            format!("nearpin: `{b_comp}` pin `{b_pin}` has no offset"),
                            Location::Entity(bid),
                        )),
                    },
                    None => errors.push(
                        Diagnostic::error(
                            "E_UNKNOWN_PIN",
                            format!(
                                "nearpin: `{b_comp}` (part `{}`) has no pin `{b_pin}`",
                                bc.part
                            ),
                            Location::Entity(bid),
                        )
                        .with_help(available_pins(bdef)),
                    ),
                }
            }
            GenDirective::MinSep { a, b, gap } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                let am = note_missing(
                    &a,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "minsep",
                );
                let bm = note_missing(
                    &b,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "minsep",
                );
                if am || bm {
                    continue;
                }
                relational.push(Constraint::MinSep { a, b, gap: *gap });
            }
            GenDirective::AlignX { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                let mut any_missing = false;
                for n in &nodes {
                    any_missing |=
                        note_missing(n, &components, &mut reported_missing, &mut errors, "alignx");
                }
                if !any_missing {
                    relational.push(Constraint::AlignX { nodes });
                }
            }
            GenDirective::AlignY { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                let mut any_missing = false;
                for n in &nodes {
                    any_missing |=
                        note_missing(n, &components, &mut reported_missing, &mut errors, "aligny");
                }
                if !any_missing {
                    relational.push(Constraint::AlignY { nodes });
                }
            }
            _ => {}
        }
    }

    // Pass 2c: overlap-avoidance (issues 0005 / 0019). No two component courtyards may
    // overlap; generate a NoOverlap constraint for every pair (O(N²), as noted in
    // the ticket). Each courtyard is lowered once per component to a rounded convex
    // polygon in its local frame, already rotated by its orientation (see
    // [`component_courtyard`]); a part with no geometry has none and is dropped here.
    // `components` is a BTreeMap, so the order — and thus the constraint set — is
    // deterministic.
    let courts: Vec<(EntityId, Vec<Point>, Nm)> = components
        .iter()
        .filter_map(|(id, c)| {
            component_courtyard(&lib[&c.part], c.orient).map(|(poly, r)| (id.clone(), poly, r))
        })
        .collect();
    for i in 0..courts.len() {
        for j in (i + 1)..courts.len() {
            relational.push(Constraint::NoOverlap {
                a: courts[i].0.clone(),
                a_poly: courts[i].1.clone(),
                a_r: courts[i].2,
                b: courts[j].0.clone(),
                b_poly: courts[j].1.clone(),
                b_r: courts[j].2,
            });
        }
    }

    // Pass 3: connections. A selector resolves against the part: a functional name
    // fans out to every pad with that name (so a six-pad power rail gets six
    // members), a pad number picks one pad. An unresolvable selector — a typo or a
    // pin the part doesn't have — is reported (each, they don't cascade) and the
    // member is skipped; a reference to a missing component is cascade-suppressed.
    let mut no_connects: BTreeSet<PinRef> = BTreeSet::new();
    for d in source {
        match d {
            GenDirective::ConnectInterface { a, b } => {
                let aid = EntityId::new(a.0.clone());
                let bid = EntityId::new(b.0.clone());
                let am = note_missing(
                    &aid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "connect",
                );
                let bm = note_missing(
                    &bid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "connect",
                );
                if am || bm {
                    continue;
                }
                connect_interface(&components, lib, a, b, &mut nets, &mut errors);
            }
            GenDirective::ConnectPins { net, pins } => {
                let id = NetId::new(net.clone());
                let entry = nets.entry(id.clone()).or_insert_with(|| Net {
                    id,
                    name: net.clone(),
                    members: BTreeSet::new(),
                });
                for (comp, sel) in pins {
                    let cid = EntityId::new(comp.clone());
                    let ctx = format!("net `{net}`");
                    if note_missing(&cid, &components, &mut reported_missing, &mut errors, &ctx) {
                        continue;
                    }
                    let def = &lib[&components[&cid].part];
                    let nums = def.resolve_selector(sel);
                    if nums.is_empty() {
                        errors.push(
                            Diagnostic::error(
                                "E_UNKNOWN_PIN",
                                format!(
                                    "{ctx}: `{comp}` (part `{}`) has no pin `{sel}`",
                                    components[&cid].part
                                ),
                                Location::Entity(cid.clone()),
                            )
                            .with_help(available_pins(def)),
                        );
                        continue;
                    }
                    for n in nums {
                        entry.members.insert(PinRef::new(&cid, &n));
                    }
                }
            }
            GenDirective::NoConnect { pins } => {
                for (comp, sel) in pins {
                    let cid = EntityId::new(comp.clone());
                    if note_missing(
                        &cid,
                        &components,
                        &mut reported_missing,
                        &mut errors,
                        "no-connect",
                    ) {
                        continue;
                    }
                    let def = &lib[&components[&cid].part];
                    let nums = def.resolve_selector(sel);
                    if nums.is_empty() {
                        errors.push(
                            Diagnostic::error(
                                "E_UNKNOWN_PIN",
                                format!(
                                    "no-connect: `{comp}` (part `{}`) has no pin `{sel}`",
                                    components[&cid].part
                                ),
                                Location::Entity(cid.clone()),
                            )
                            .with_help(available_pins(def)),
                        );
                        continue;
                    }
                    for n in nums {
                        no_connects.insert(PinRef::new(&cid, &n));
                    }
                }
            }
            _ => {}
        }
    }

    // Validate region declarations: a copper pour names the net it belongs to, and
    // that net must exist (be connected by some `net`/ConnectPins directive) — a
    // pour on a typo'd or never-connected net is a hard fault, never a silent dangle,
    // the same guarantee `ConnectPins`/`NoConnect` give for pins. Collected, not
    // aborting early.
    for d in source {
        if let GenDirective::Region(r) = d
            && r.role == Role::Conductor
        {
            match &r.net {
                Some(name) if !nets.contains_key(&NetId::new(name.clone())) => {
                    errors.push(
                        Diagnostic::error(
                            "E_UNKNOWN_NET",
                            format!(
                                "copper pour references net `{name}`, which no directive connects"
                            ),
                            Location::Net(NetId::new(name.clone())),
                        )
                        .with_help(
                            "connect that net (e.g. `net <name> ...`), or fix the pour's net name",
                        ),
                    );
                }
                None => errors.push(
                    Diagnostic::error(
                        "E_POUR_NO_NET",
                        "copper pour has no net; a conductor region must name the net it fills",
                        Location::None,
                    )
                    .with_help(
                        "add `net=<name>` to the region, or make it a keep-out/void instead",
                    ),
                ),
                _ => {}
            }
        }
    }

    // Validate slab-name targets (Decision 13): every region / text `layer` must name a
    // slab in the stackup, and a `Conductor` pour must target a copper slab. An unknown
    // name — or a net-bound pour on a non-copper slab (silk) — is a hard fault here (no
    // silent board-z/copper-z fallback), so a committed document always resolves cleanly
    // to the `Feature` model. Collected, not aborting early.
    let su = stackup(source);
    let unknown_slab = |name: &str| -> Diagnostic {
        let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
        Diagnostic::error(
            "E_UNKNOWN_SLAB",
            format!("layer `{name}` names no slab in the stackup"),
            Location::None,
        )
        .with_help(format!("available slabs: {}", names.join(", ")))
    };
    for d in source {
        match d {
            GenDirective::Region(r) => match su.slabs.iter().find(|s| s.name == r.layer) {
                None => errors.push(unknown_slab(&r.layer)),
                Some(slab) if r.role == Role::Conductor && slab.role != Role::Conductor => {
                    errors.push(
                        Diagnostic::error(
                            "E_POUR_NON_COPPER",
                            format!(
                                "copper pour on non-copper slab `{}` (its role is {:?})",
                                r.layer, slab.role
                            ),
                            Location::None,
                        )
                        .with_help(
                            "target a copper slab (e.g. F.Cu / B.Cu), or change the region role",
                        ),
                    );
                }
                _ => {}
            },
            GenDirective::Text { layer, .. } if su.slab_z(layer).is_none() => {
                errors.push(unknown_slab(layer));
            }
            _ => {}
        }
    }

    // Collect-all gate: if the model could not be built cleanly, abort the whole
    // transaction with every fault found. The partial model above is discarded.
    if !errors.is_empty() {
        return Err(errors);
    }

    // Pass 4: place everything with the least-change solver, then reconcile
    // overrides against the solved result.
    //
    // Precedence (via movability): Fix/Pin are immovable anchors; Hint is a
    // movable soft anchor; Free is anchored at the generated default. An override
    // is *ineffective* iff freeing it and re-solving lands it in the same place —
    // i.e. the solver/constraints would have put it there anyway. Ineffective
    // hints decay, ineffective pins are flagged, and a pin a hard Fix contradicts
    // raises a loud conflict.
    let base: BTreeMap<EntityId, Point> = components
        .iter()
        .map(|(k, c)| (k.clone(), c.pos.value))
        .collect();
    let no_suppress = BTreeSet::new();
    // We use only `.positions` here: reconciliation's least-change/decay logic is
    // defined purely by where the solver places nodes. The new `Solution` also
    // carries `converged`/`unsatisfied` (infeasibility), which the engine could
    // surface in a future milestone; today the placement is what reconciliation
    // consumes, so the semantics below are unchanged from the relaxation solver.
    let solved_all = solve(&assemble_problem(
        &base,
        &fixmap,
        overrides,
        board.as_ref(),
        &relational,
        &no_suppress,
    ))
    .positions;

    let mut report = ReconReport::default();
    let mut decayed: BTreeSet<EntityId> = BTreeSet::new();
    let mut prov_map: BTreeMap<EntityId, Provenance> = BTreeMap::new();
    // Decision 17: a doc-wide `font` that fails to load degrades to the stroke font — a
    // finding on a valid doc (a `W_FONT_LOAD` warning), never a fault.
    report.font_load_failure = font_load_failure(source);
    // Issue 0024: an outer copper side with no mask slab, while the stackup does carry a
    // mask — the forgot-one-side footgun. A degrade (a `W_COPPER_NO_MASK` warning), not a
    // fault; the side resolution reuses the same top_mask/bottom_mask query pad openings
    // use, so the lint agrees with what the mask export actually covers.
    report.unmasked_copper = stackup(source).unmasked_outer_copper();
    // DNP variant (Decision 21b): connections referencing an `if=false` depopulated
    // instance were skipped above; surface each dangling reference as a `W_DNP` warning
    // (deduped + sorted for a deterministic report).
    dnp_dangling.sort();
    dnp_dangling.dedup();
    report.dnp_dangling = dnp_dangling;

    for (id, ov) in overrides {
        if !base.contains_key(id) || ov.pos.is_none() {
            continue; // orphans handled below; empty overrides ignored
        }
        let fix = fixmap.get(id).copied();

        // A hard constraint outranks the override regardless of geometry.
        if let Some(fp) = fix {
            match ov.strength {
                Strength::Hint => {
                    report
                        .decayed
                        .push((id.clone(), DecayReason::OverriddenByConstraint));
                    decayed.insert(id.clone());
                }
                Strength::Pin => {
                    if ov.pos != Some(fp) {
                        report.pin_conflicts.push(id.clone());
                    } else {
                        report.redundant_pins.push(id.clone());
                    }
                }
            }
            continue;
        }

        // No hard constraint: is the override doing anything? Re-solve without it.
        let mut suppress = BTreeSet::new();
        suppress.insert(id.clone());
        let solved_wo = solve(&assemble_problem(
            &base,
            &fixmap,
            overrides,
            board.as_ref(),
            &relational,
            &suppress,
        ))
        .positions;
        let effective = dist(solved_all[id], solved_wo[id]) > PLACE_TOL as f64;

        match ov.strength {
            Strength::Hint => {
                if effective {
                    prov_map.insert(id.clone(), Provenance::Hint);
                } else {
                    report
                        .decayed
                        .push((id.clone(), DecayReason::RedundantWithDefault));
                    decayed.insert(id.clone());
                }
            }
            Strength::Pin => {
                if !effective {
                    report.redundant_pins.push(id.clone());
                }
                prov_map.insert(id.clone(), Provenance::Pinned); // pins are kept
            }
        }
    }

    // Final placement with decayed hints freed back to their defaults. This is
    // what a fresh elaboration (after GC) would produce, so the result is stable.
    let solved_final = solve(&assemble_problem(
        &base,
        &fixmap,
        overrides,
        board.as_ref(),
        &relational,
        &decayed,
    ))
    .positions;

    for (id, c) in components.iter_mut() {
        let prov = if fixmap.contains_key(id) {
            Provenance::Fixed
        } else {
            prov_map.get(id).copied().unwrap_or(Provenance::Free)
        };
        c.pos = Dof {
            value: solved_final[id],
            prov,
        };
    }

    // Orphaned overrides: target no longer exists. Surfaced, never dropped. Refdes
    // pins share the orphan channel (same "override targets a dead id" semantics);
    // dedupe so an entity with both a pos override and a refdes pin is flagged once.
    for id in overrides.keys() {
        if !components.contains_key(id) {
            report.orphaned.push(id.clone());
        }
    }
    for id in refdes_pins.keys() {
        if !components.contains_key(id) && !report.orphaned.contains(id) {
            report.orphaned.push(id.clone());
        }
    }

    // Colliding refdes pins (two entities pinned to one string): an authoring
    // conflict surfaced loudly, non-blocking like the pos findings above.
    report.refdes_pin_dups = crate::annotate::duplicate_refdes_pins(refdes_pins);

    // Honest verify (Decision 10's third leg / issue 0019). The solver now pushes the
    // *true* polygonal courtyards, but a converged placement can still leave a residual
    // overlap the push could not clear — two fixed/pinned parts placed into each other.
    // Re-check every NoOverlap pair against the real rounded polygons at the final
    // placement and report any that still overlap. Because the solver push consumes the
    // polygon itself (not the looser AABB proxy), this is the tighter truth — the check
    // deliberately *not* shipped pre-0019, when it could only ever false-positive.
    for c in &relational {
        if let Constraint::NoOverlap {
            a,
            a_poly,
            a_r,
            b,
            b_poly,
            b_r,
        } = c
        {
            let (Some(&pa), Some(&pb)) = (solved_final.get(a), solved_final.get(b)) else {
                continue;
            };
            let world = |poly: &[Point], o: Point| -> Vec<Point> {
                poly.iter()
                    .map(|p| Point {
                        x: p.x + o.x,
                        y: p.y + o.y,
                    })
                    .collect()
            };
            // Report only overlaps beyond the verify tolerance: a converged movable pair
            // carries at most the solver's ~µm residual (convergence slop), which is not
            // a collision. A genuine unresolvable overlap (two fixed parts pinned into
            // each other) penetrates by tens of µm or more. See [`COURTYARD_VERIFY_TOL`].
            let depth = courtyard_overlap_depth(&world(a_poly, pa), *a_r, &world(b_poly, pb), *b_r);
            if depth > COURTYARD_VERIFY_TOL as f64 {
                report.courtyard_overlaps.push((a.clone(), b.clone()));
            }
        }
    }

    Ok(Elaborated {
        components,
        nets,
        no_connects,
        report,
        dnp_dropped,
        def_fragments,
    })
}

// `board_rect` (the pure `GenDirective` builder that used to live here) now lives
// in `crate::ir` and is re-exported via the glob at the top of this module.
