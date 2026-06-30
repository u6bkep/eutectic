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

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::geom::{BoardShape, Role, Shape2D};
use crate::id::{EntityId, NetId};
use crate::part::{Dir, PartDef, PartLib, courtyard_half_extents};
use crate::route::Layer;
use crate::solve::{Constraint, PLACE_TOL, Problem, dist, solve};
use std::collections::{BTreeMap, BTreeSet};

/// An authored **filled region**: a `Shape2D` area carrying a [`Role`] — a copper
/// pour (`Conductor`, with the `net` it belongs to and the copper `layer` it fills),
/// a keep-out (`Keepout`), or a filled void (`Void`). This is the *authoritative
/// declaration* (tier-1, in the generative `Source`); the actual knockout fill
/// (`region − foreign_copper ⊕ clearance`) is **derived** later (0004 stage 3), so it
/// is never stored and never goes stale. The shape is in absolute board coordinates
/// (like the board outline), not a footprint-local transform. `layer` is carried for
/// every role; for non-`Conductor` roles it is advisory until the fill stage gives it
/// meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionDecl {
    pub shape: Shape2D,
    pub role: Role,
    pub net: Option<String>,
    pub layer: Layer,
}

/// A directive in the generative program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenDirective {
    /// Instantiate `part` at hierarchical `path`.
    Instance {
        path: String,
        part: String,
    },
    /// Source-provided default placement (a *free* DOF unless overridden).
    Place {
        path: String,
        pos: Point,
    },
    /// A hard placement constraint (e.g. a connector mated to a mechanical
    /// datum). Outranks user overrides; surfaces conflicts rather than yielding.
    Fix {
        path: String,
        pos: Point,
    },
    /// Board outline (a [`Shape2D`] — rounded/concave/CAD-imported all expressible);
    /// movable components are kept inside it. Use [`board_rect`] for the common
    /// rectangle. The last `Board` in the source wins.
    Board {
        outline: Shape2D,
    },
    /// An interior board cutout / void ([`Shape2D`]); components are kept out of it.
    Cutout {
        shape: Shape2D,
    },
    /// An authored filled region — a copper pour, keep-out, or filled void. See
    /// [`RegionDecl`]. Read by [`regions`]; the knockout fill is derived downstream.
    Region(RegionDecl),
    /// Relational placement constraint solved by the least-change solver.
    Near {
        a: String,
        b: String,
        within: Nm,
    },
    MinSep {
        a: String,
        b: String,
        gap: Nm,
    },
    AlignX {
        nodes: Vec<String>,
    },
    AlignY {
        nodes: Vec<String>,
    },
    /// Connect two interface ports. The crossing is determined by the interface
    /// type's mate map, so it cannot be wired backwards.
    ConnectInterface {
        a: (String, String), // (component path, port name)
        b: (String, String),
    },
    /// Connect discrete pins onto a named net. Each `(comp path, selector)` is
    /// resolved against the component's part: a functional name fans out to *every*
    /// pad with that name (so `IOVDD` connects all six pads), a pad number selects
    /// that one pad. An unresolvable selector aborts elaboration (no silent dangle).
    ConnectPins {
        net: String,
        pins: Vec<(String, String)>,
    }, // (comp path, selector)
    /// Mark pads as deliberately unconnected. Same `(comp path, selector)` shape as
    /// `ConnectPins`; the resolved pads are exempt from the floating-pad check.
    NoConnect {
        pins: Vec<(String, String)>,
    },
    /// Set a component's planar orientation (cardinal degrees: 0/90/180/270). A
    /// settable attribute, not a solver DOF.
    Rotate {
        path: String,
        deg: i32,
    },
    /// Like `Near`, but the target is a specific *pin* (`b_comp`.`b_pin`) rather
    /// than a component centroid. The pin's world position tracks its component's
    /// position + orientation during solving.
    NearPin {
        a: String,
        b_comp: String,
        b_pin: String,
        within: Nm,
    },
}

/// The generative program (tier 1 authoritative).
pub type Source = Vec<GenDirective>;

/// Result of elaboration before it is folded into a Doc.
pub struct Elaborated {
    pub components: BTreeMap<EntityId, Component>,
    pub nets: BTreeMap<NetId, Net>,
    pub no_connects: BTreeSet<PinRef>,
    pub report: ReconReport,
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
    lib: &PartLib,
) -> Result<Elaborated, Vec<Diagnostic>> {
    let mut components: BTreeMap<EntityId, Component> = BTreeMap::new();
    let mut nets: BTreeMap<NetId, Net> = BTreeMap::new();
    let mut order = 0i64; // deterministic default placement counter

    // Collect-all: accumulate structural faults instead of returning the first.
    // `reported_missing` is the cascade-suppression set — an entity that does not
    // exist (failed to instantiate, or never declared) is reported once, and all
    // later references to it are silenced so the real fault isn't buried.
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut reported_missing: BTreeSet<EntityId> = BTreeSet::new();

    // Pass 1: instances.
    for d in source {
        if let GenDirective::Instance { path, part } = d {
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
        if let GenDirective::Rotate { path, deg } = d {
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
            match Orient::from_deg(*deg) {
                Some(o) => {
                    components
                        .get_mut(&id)
                        .expect("note_missing confirmed presence")
                        .orient = o
                }
                None => errors.push(Diagnostic::error(
                    "E_BAD_ROTATION",
                    format!("rotate `{path}` by {deg}deg: only 0/90/180/270 supported"),
                    Location::Entity(id),
                )),
            }
        }
    }

    // Pass 2b: collect hard placement constraints (Fix), the board outline, and
    // relational constraints for the solver.
    let mut fixmap: BTreeMap<EntityId, Point> = BTreeMap::new();
    // The board outline + cutouts (assembled from Board/Cutout directives); movable
    // components are kept inside it by the solver.
    let board = board_shape(source);
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
                            b_off: bc.orient.rotate(off),
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

    // Pass 2c: overlap-avoidance (issue 0005). No two component courtyards may
    // overlap; generate a NoOverlap constraint for every pair (O(N²), as noted in
    // the ticket). Courtyards are computed once per component (oriented; a part with
    // no geometry has none and is dropped here), then paired. `components` is a
    // BTreeMap, so the order — and thus the constraint set — is deterministic.
    let courts: Vec<(EntityId, (Nm, Nm))> = components
        .iter()
        .map(|(id, c)| (id.clone(), oriented_courtyard(&lib[&c.part], c.orient)))
        .filter(|(_, h)| *h != (0, 0))
        .collect();
    for i in 0..courts.len() {
        for j in (i + 1)..courts.len() {
            relational.push(Constraint::NoOverlap {
                a: courts[i].0.clone(),
                b: courts[j].0.clone(),
                a_half: courts[i].1,
                b_half: courts[j].1,
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

    // Orphaned overrides: target no longer exists. Surfaced, never dropped.
    for id in overrides.keys() {
        if !components.contains_key(id) {
            report.orphaned.push(id.clone());
        }
    }

    Ok(Elaborated {
        components,
        nets,
        no_connects,
        report,
    })
}

/// Record (once) that a referenced entity does not exist, and report it as a
/// structural fault. Returns `true` if `id` is missing (so the caller skips it).
/// The `reported_missing` set is the cascade-suppression mechanism: an entity is
/// reported the *first* time it's found missing, and later references are silenced
/// so the genuine fault (its failed/absent instantiation) isn't buried under noise.
fn note_missing(
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

/// A part's courtyard half-extents oriented for a placed component: a cardinal
/// 90°/270° turn swaps width and height.
fn oriented_courtyard(def: &PartDef, orient: Orient) -> (Nm, Nm) {
    let (hw, hh) = courtyard_half_extents(def);
    match orient {
        Orient::Deg90 | Orient::Deg270 => (hh, hw),
        Orient::Deg0 | Orient::Deg180 => (hw, hh),
    }
}

/// A `help:` line listing a part's distinct functional pin names — the candidates
/// for an unresolved selector (the "did you mean" surface; fuzzy matching later).
fn available_pins(def: &PartDef) -> String {
    let mut names: Vec<&str> = def.pins.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    format!("available pins: {}", names.join(", "))
}

/// A `help:` line listing the known part names — candidates for an unknown part.
fn known_parts(lib: &PartLib) -> String {
    let names: Vec<&str> = lib.keys().map(String::as_str).collect();
    format!("known parts: {}", names.join(", "))
}

/// Build a solver problem from base placements + overrides + constraints.
/// `suppress` lists override ids to ignore (treat the node as Free at its
/// default) — used to test whether an override is doing anything.
fn assemble_problem(
    base: &BTreeMap<EntityId, Point>,
    fixmap: &BTreeMap<EntityId, Point>,
    overrides: &BTreeMap<EntityId, Override>,
    board: Option<&BoardShape>,
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

/// Assemble the board outline + cutouts from the source. The outline is the last
/// `Board` directive's [`Shape2D`] (`None` if there is none — the solver then leaves
/// placement unbounded); cutouts are every `Cutout` directive's shape. This is the
/// single shared reader (elaboration, autorouter, export all call it).
pub fn board_shape(source: &Source) -> Option<BoardShape> {
    let outline = source.iter().rev().find_map(|d| match d {
        GenDirective::Board { outline } => Some(outline.clone()),
        _ => None,
    })?;
    let cutouts = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Cutout { shape } => Some(shape.clone()),
            _ => None,
        })
        .collect();
    Some(BoardShape { outline, cutouts })
}

/// Assemble every authored [`RegionDecl`] from the source, in declaration order. The
/// single shared reader for pours / keep-outs / filled voids — the derived fill query
/// (0004 stage 3), DRC, and export all call this, exactly as [`board_shape`] is the
/// shared reader for the outline.
pub fn regions(source: &Source) -> Vec<RegionDecl> {
    source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Region(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// Build a rectangular [`Board`](GenDirective::Board) directive from opposite corners
/// — sugar over the polygon outline form for the common case.
pub fn board_rect(min: Point, max: Point) -> GenDirective {
    let c = Point {
        x: (min.x + max.x) / 2,
        y: (min.y + max.y) / 2,
    };
    GenDirective::Board {
        outline: Shape2D::rect(c, max.x - min.x, max.y - min.y),
    }
}

/// Connect two interface ports using the interface type's mate map. The mate map
/// is the single place the tx<->rx crossing is defined, so connecting two ports
/// always produces correctly-crossed nets — the swap footgun is unrepresentable.
///
/// Both components are assumed present (the caller cascade-checks them); any port /
/// type / drive fault is pushed onto `errors` (the transaction aborts on it), and a
/// fault that prevents wiring returns early without producing partial nets.
fn connect_interface(
    components: &BTreeMap<EntityId, Component>,
    lib: &PartLib,
    a: &(String, String),
    b: &(String, String),
    nets: &mut BTreeMap<NetId, Net>,
    errors: &mut Vec<Diagnostic>,
) {
    let (ap, aport) = a;
    let (bp, bport) = b;
    let aid = EntityId::new(ap.clone());
    let bid = EntityId::new(bp.clone());
    let ac = &components[&aid];
    let bc = &components[&bid];
    let adef = &lib[&ac.part];
    let bdef = &lib[&bc.part];
    let (Some(aiface), Some(biface)) = (adef.interfaces.get(aport), bdef.interfaces.get(bport))
    else {
        if !adef.interfaces.contains_key(aport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{ap}` (part `{}`) has no interface port `{aport}`",
                    ac.part
                ),
                Location::Entity(aid),
            ));
        }
        if !bdef.interfaces.contains_key(bport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{bp}` (part `{}`) has no interface port `{bport}`",
                    bc.part
                ),
                Location::Entity(bid),
            ));
        }
        return;
    };
    if aiface.type_name != biface.type_name {
        errors.push(Diagnostic::error(
            "E_INTERFACE_MISMATCH",
            format!(
                "interface type mismatch: {} vs {}",
                aiface.type_name, biface.type_name
            ),
            Location::Entity(aid),
        ));
        return;
    }

    for (sa, sb) in &aiface.mate {
        let da = aiface.signals.get(sa).copied();
        let db = biface.signals.get(sb).copied();
        let (Some(da), Some(db)) = (da, db) else {
            errors.push(Diagnostic::error(
                "E_INTERFACE_SIGNAL",
                format!(
                    "interface `{}` mate references a missing signal",
                    aiface.type_name
                ),
                Location::Entity(aid.clone()),
            ));
            continue;
        };
        // Direction sanity: a mated pair must be drive/receive, not both drivers.
        if matches!((da, db), (Dir::Out, Dir::Out)) {
            errors.push(Diagnostic::error(
                "E_DRIVE_CONFLICT",
                format!("drive conflict mating {sa}<->{sb}"),
                Location::Entity(aid.clone()),
            ));
            continue;
        }
        let net_name = format!("{ap}.{aport}.{sa}");
        let nid = NetId::new(net_name.clone());
        let net = nets.entry(nid.clone()).or_insert_with(|| Net {
            id: nid,
            name: net_name,
            members: BTreeSet::new(),
        });
        net.members
            .insert(PinRef::new(&aid, &format!("{aport}.{sa}")));
        net.members
            .insert(PinRef::new(&bid, &format!("{bport}.{sb}")));
    }
}

// ---- source-building helpers (a stand-in for the textual generative layer) ----

/// Build the demo power-supply module with `n` decoupling caps fanned off the
/// regulator output. This is the "generator" whose output we later override and
/// re-elaborate to test minimal-perturbation reconciliation.
pub fn psu_module(n: usize) -> Source {
    let mut s = vec![GenDirective::Instance {
        path: "psu.reg".into(),
        part: "LDO".into(),
    }];
    for i in 0..n {
        let dec = format!("psu.dec[{i}]");
        s.push(GenDirective::Instance {
            path: dec.clone(),
            part: "Cap".into(),
        });
        s.push(GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("psu.reg".into(), "VOUT".into()),
                (dec.clone(), "p1".into()),
            ],
        });
        s.push(GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("psu.reg".into(), "GND".into()), (dec, "p2".into())],
        });
    }
    s
}
