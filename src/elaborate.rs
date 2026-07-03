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
use crate::geom::{BoardShape, Feature, NetFeature, Role, Shape2D, Slab, Stackup, ZRange};
use crate::id::{EntityId, NetId};
use crate::part::{Dir, PartDef, PartLib, courtyard_half_extents, courtyard_shape};
use crate::solve::{Constraint, PLACE_TOL, Problem, courtyard_overlap_depth, dist, solve};
use std::collections::{BTreeMap, BTreeSet};

/// An authored **filled region**: a `Shape2D` area carrying a [`Role`] — a copper
/// pour (`Conductor`, with the `net` it belongs to and the `layer` slab it fills),
/// a keep-out (`Keepout`), or a filled void (`Void`). This is the *authoritative
/// declaration* (tier-1, in the generative `Source`); the actual knockout fill
/// (`region − foreign_copper ⊕ clearance`) is **derived** later (0004 stage 3), so it
/// is never stored and never goes stale. The shape is in absolute board coordinates
/// (like the board outline), not a footprint-local transform.
///
/// `layer` is a **slab name** (Decision 13) — an arbitrary token resolved against the
/// [`Stackup`] at elaboration (`F.Cu`, `B.Cu`, `F.SilkS`, or any authored slab); an
/// unknown name is a hard error, and a `Conductor` region whose slab is not copper is
/// nonsense (rejected by [`features`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionDecl {
    pub shape: Shape2D,
    pub role: Role,
    pub net: Option<String>,
    pub layer: String,
}

/// A directive in the generative program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenDirective {
    /// Instantiate `part` at hierarchical `path`. Optionally carries identity
    /// `params` (authored display-normal strings — copied verbatim onto the
    /// [`Component`]) and a display `label` template override (Decision 14). Both
    /// default empty/None for the common case (an IC identified by part name alone).
    Instance {
        path: String,
        part: String,
        params: BTreeMap<String, String>,
        label: Option<String>,
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
    /// One authored board-stackup [`Slab`] (a named z-slab with a role + optional
    /// material). Accumulated by [`stackup`] into the board [`Stackup`], mirroring how
    /// [`Region`](Self::Region) directives are collected by [`regions`]. This is *not* a
    /// placement/connectivity directive — elaboration's passes ignore it; it is read
    /// only by [`stackup`].
    Slab(Slab),
    /// One authored **class-registry** entry (Decision 14): the conventions —
    /// refdes `prefix`, label `template`, class-default params — for a component
    /// `class`. Accumulated by [`registry`](crate::annotate::registry) over the built-in
    /// seeds, mirroring how [`Slab`](Self::Slab) directives are collected by
    /// [`stackup`]. A display/identity directive — elaboration's placement/connectivity
    /// passes ignore it.
    Class {
        name: String,
        entry: crate::annotate::ClassEntry,
    },
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
    /// Set a component's orientation to a quaternion [`Orient`] (planar rotation,
    /// optionally flipped to the board bottom — both baked into the quaternion at
    /// authoring time). A settable attribute, not a solver DOF. The text front-end
    /// lowers a `<deg> [bottom]` (any angle) or `quat=(w,x,y,z)` into this.
    Rotate {
        path: String,
        orient: Orient,
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
    /// Authored **board text** — a mutable string lowered to silkscreen (per
    /// Decision 9 in docs/geometry-model-convergence.md). The **authoritative** form
    /// is exactly these fields (string + placement + `height` + `layer` + `orient`);
    /// the `Shape2D` strokes are *derived* by [`features`] through the built-in
    /// stroke [`crate::font`] — never stored, so a rename re-derives. `orient`
    /// defaults to [`Orient::IDENTITY`] (rotated labels are a follow-up). This is
    /// **not** a placement/connectivity directive — elaboration's passes ignore it
    /// (the main matches have `_ => {}` arms); it is read only by the lowering.
    Text {
        string: String,
        at: Point,
        height: Nm,
        /// The **slab name** the silk lands on (Decision 13) — resolved against the
        /// [`Stackup`] at lowering; `F.SilkS` by default. An unknown name is a hard
        /// error (silk now lands at the silk slab's honest z, not copper z).
        layer: String,
        orient: Orient,
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
    refdes_pins: &BTreeMap<EntityId, String>,
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
            // Report only overlaps deeper than the placement tolerance: a converged
            // movable pair carries at most the solver's sub-µm residual, which is slop,
            // not a collision. A genuine unresolvable overlap (two fixed parts pinned
            // into each other) penetrates by far more.
            let depth = courtyard_overlap_depth(&world(a_poly, pa), *a_r, &world(b_poly, pb), *b_r);
            if depth > PLACE_TOL as f64 {
                report.courtyard_overlaps.push((a.clone(), b.clone()));
            }
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
fn component_courtyard(def: &PartDef, orient: Orient) -> Option<(Vec<Point>, Nm)> {
    if let Some(shape) = courtyard_shape(def) {
        let verts = shape
            .points()
            .into_iter()
            .map(|p| orient.apply(p))
            .collect();
        return Some((verts, shape.radius()));
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
fn oriented_courtyard(def: &PartDef, orient: Orient) -> (Nm, Nm) {
    let (hw, hh) = courtyard_half_extents(def);
    let ax = orient.apply(Point { x: hw, y: 0 });
    let ay = orient.apply(Point { x: 0, y: hh });
    (ax.x.abs() + ay.x.abs(), ax.y.abs() + ay.y.abs())
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

/// The board [`Stackup`] for a source — the single shared reader that every consumer
/// lowering an abstract layer to a real `ZRange` must go through (sibling to
/// [`board_shape`] / [`regions`]).
///
/// Collects every [`Slab`](GenDirective::Slab) directive, in **declaration order**, into
/// `Stackup { slabs }` — exactly as [`regions`] collects [`RegionDecl`]s. Declaration
/// order is preserved (not sorted): [`Stackup`]'s own accessors order by z where they
/// need to ([`Stackup::copper_slabs`] sorts by z, [`Stackup::board_z`] takes min/max,
/// [`Stackup::slab_z`] looks up by name), so order is functionally irrelevant — and
/// preserving it keeps `parse(serialize(doc)) == doc` trivially. No overlap/gap
/// validation is performed here (`ZRange::new` already normalises `lo ≤ hi`); a future
/// validation pass can layer on top without changing this reader's contract.
///
/// If the source authors **no** slabs, falls back to [`Stackup::default_2layer`] — the
/// unchanged familiar 2-layer default, so existing sources behave exactly as before.
pub fn stackup(source: &Source) -> Stackup {
    let slabs: Vec<Slab> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Slab(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    if slabs.is_empty() {
        Stackup::default_2layer()
    } else {
        Stackup { slabs }
    }
}

/// Resolve a **slab name** to its absolute [`ZRange`] via the stackup (Decision 13).
/// An unknown name is a **hard error** — no board-z / `ZRange(0,0)` fallback — naming
/// the unknown slab and the available slab names, matching the crate's text-parse
/// `Result<_, String>` error idiom.
fn slab_z(su: &Stackup, name: &str) -> Result<ZRange, String> {
    su.slab_z(name).ok_or_else(|| {
        let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
        format!("unknown slab `{name}` (available: {})", names.join(", "))
    })
}

/// Lower the authored board/region geometry of a `Source` into the converged
/// [`NetFeature`] model — a [`Feature`] (pure physical geometry) paired with the
/// optional net it carries. This is the additive producer the convergence's Phase 2
/// will wire DRC/export onto; for now it has no callers besides tests. It is the
/// role-filtered union of what [`board_shape`] and [`regions`] read today
/// (Decision 12.4), kept as one derived view, threading z through [`stackup`].
///
/// Emitted per directive (net stays an *annotation* alongside the feature, never a
/// field on `Feature` — connectivity is authoritative, Decision 12.1):
///   - the **last** `Board` directive → one [`Role::Substrate`] netless feature,
///     preserving [`board_shape`]'s "last `Board` wins" single-outline semantics.
///     (Unioning several `Board` directives into one multi-substrate body is deferred.)
///   - every `Cutout` → a [`Role::Void`] netless feature (mirrors [`board_shape`]).
///   - every `Region` → a feature carrying the authored role + net, at its slab's z
///     (mirrors [`regions`]).
///
/// This is the single **materialization gate** that resolves slab names against the
/// [`Stackup`] (Decision 13), so it is **fallible**: an unknown slab name — on a region
/// or a text label — is a hard error, and a `Conductor` region whose slab is not a
/// copper slab (a net-bound pour on silk) is likewise rejected here.
pub fn features(source: &Source) -> Result<Vec<crate::geom::NetFeature>, String> {
    let su = stackup(source);
    // The physical board *body* extent (the Substrate solid spans it). An empty stackup
    // has no extent — fall back to a zero range so the feature is still emitted.
    let board_z = su.board_z().unwrap_or(ZRange::new(0, 0));
    // The *full* stackup extent — what a through-cut (a board cutout) pierces: the body
    // plus mask and silk, so a milled cutout removes the ink and mask over it too.
    let full_z = su.full_z().unwrap_or(ZRange::new(0, 0));

    let mut out: Vec<NetFeature> = Vec::new();

    // Board: only the LAST `Board` becomes the substrate (reverse-find mirrors
    // `board_shape`'s "last `Board` wins"). The same outline generates the mask solids.
    let board_outline = source.iter().rev().find_map(|d| match d {
        GenDirective::Board { outline } => Some(outline),
        _ => None,
    });
    if let Some(outline) = board_outline {
        out.push(NetFeature::netless(Feature::prism(
            Role::Substrate,
            outline.clone(),
            board_z,
        )));

        // Solder mask: one board-area solid per `Role::Mask` slab in the stackup, at the
        // slab's honest z, carrying the slab's material (Decision 13 — mask is a positive
        // generated solid, and its openings are `Void` deletion volumes; there are no
        // negative layers). A stackup with no mask slab generates nothing; three generate
        // three. No special cases. The board area is the substrate outline, so a custom
        // outline (rounded / imported) masks to its true shape.
        for slab in su.slabs.iter().filter(|s| s.role == Role::Mask) {
            let mut mask = Feature::prism(Role::Mask, outline.clone(), slab.z);
            mask.material = slab.material.clone();
            out.push(NetFeature::netless(mask));
        }
    }

    // Cutouts: every one (mirrors `board_shape`), each a through-cut spanning the full
    // stackup so it pierces mask and silk as well as the body.
    for d in source {
        if let GenDirective::Cutout { shape } = d {
            out.push(NetFeature::netless(Feature::prism(
                Role::Void,
                shape.clone(),
                full_z,
            )));
        }
    }

    // Regions: every one, carrying the authored role + net (mirrors `regions`). The
    // slab name resolves to z; an unknown name is a hard error, and a `Conductor`
    // region on a non-copper slab (a net-bound pour on silk) is nonsense.
    for d in source {
        if let GenDirective::Region(RegionDecl {
            shape,
            role,
            net,
            layer,
        }) = d
        {
            let slab = su.slabs.iter().find(|s| &s.name == layer).ok_or_else(|| {
                let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
                format!("unknown slab `{layer}` (available: {})", names.join(", "))
            })?;
            if *role == Role::Conductor && slab.role != Role::Conductor {
                return Err(format!(
                    "Conductor region on non-copper slab `{layer}` (its role is {:?}) \
                     — a net-bound pour must target a copper slab",
                    slab.role
                ));
            }
            let net_opt = net.as_ref().map(|n| NetId::new(n.clone()));
            out.push(NetFeature::new(
                net_opt,
                Feature::prism(role.clone(), shape.clone(), slab.z),
            ));
        }
    }

    // Text: every authored string lowers to stroke-font `Marking` features (Decision
    // 9). The strokes are derived here, never stored, so a renamed label re-derives.
    for d in source {
        if let GenDirective::Text {
            string,
            at,
            height,
            layer,
            orient,
        } = d
        {
            out.extend(text_features(string, *at, *height, layer, *orient, &su)?);
        }
    }

    Ok(out)
}

/// Lower one authored [`GenDirective::Text`] into stroke-font [`Role::Marking`]
/// features (Decision 9). The shared [`crate::font::text_strokes`] produces the glyph
/// centreline polylines in a local frame (left-origin — board text's authored `at` *is*
/// the origin, so it stays [`Justify::Left`](crate::font::Justify::Left)); each is then
/// rotated by `orient` about that origin (exact for [`Orient::IDENTITY`]), translated to
/// `at`, and traced at a pen width of `height / 8` on the named slab's z (via [`slab_z`]
/// — an unknown name is a hard error). The markings are **netless** — silk carries no
/// electrical identity.
fn text_features(
    string: &str,
    at: Point,
    height: Nm,
    layer: &str,
    orient: Orient,
    su: &Stackup,
) -> Result<Vec<NetFeature>, String> {
    let z = slab_z(su, layer)?;
    let pen = (height / 8).max(1); // a visible stroke width even for tiny heights
    let mut out = Vec::new();
    for stroke in crate::font::text_strokes(string, height, crate::font::Justify::Left) {
        let pts: Vec<Point> = stroke
            .into_iter()
            .map(|local| {
                // rotate about the text origin, then place at `at`.
                let r = orient.apply(local);
                Point {
                    x: r.x + at.x,
                    y: r.y + at.y,
                }
            })
            .collect();
        out.push(NetFeature::netless(Feature::prism(
            Role::Marking,
            Shape2D::trace(pts, pen),
            z,
        )));
    }
    Ok(out)
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
        params: std::collections::BTreeMap::new(),
        label: None,
    }];
    for i in 0..n {
        let dec = format!("psu.dec[{i}]");
        s.push(GenDirective::Instance {
            path: dec.clone(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
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

/// Generate a **ring** of `count` instances of `part`, evenly spaced on a circle of
/// `radius` about `center`, each rotated to **face outward** (local +x points away
/// from the centre). Per instance `i` (path `{prefix}[i]`) it emits an `Instance`, a
/// `Place` at the ring position, and a `Rotate` to the outward orientation — all
/// concrete: the `cos`/`sin` runs **once here, at generation**, producing exact
/// integer positions + quaternions that elaboration never re-derives. The motivating
/// case: side-firing LEDs around a round board (the arbitrary-angle placement that
/// the cardinal-only `Orient` could not express).
pub fn ring(prefix: &str, part: &str, center: Point, radius: Nm, count: usize) -> Source {
    let mut s = Vec::new();
    for i in 0..count {
        let path = format!("{prefix}[{i}]");
        let deg = 360.0 * i as f64 / count as f64;
        let rad = deg.to_radians();
        let pos = Point {
            x: center.x + (radius as f64 * rad.cos()).round() as Nm,
            y: center.y + (radius as f64 * rad.sin()).round() as Nm,
        };
        s.push(GenDirective::Instance {
            path: path.clone(),
            part: part.to_string(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        s.push(GenDirective::Place {
            path: path.clone(),
            pos,
        });
        s.push(GenDirective::Rotate {
            path,
            orient: Orient::from_angle_deg(deg),
        });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Extent;

    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    #[test]
    fn ring_places_instances_around_a_circle_facing_outward() {
        // 12 side-firing LEDs on a 10 mm-radius ring — the arbitrary-angle case.
        let s = ring("led", "LED", pt(0, 0), 10_000_000, 12);
        assert_eq!(s.len(), 36, "12 × (Instance, Place, Rotate)");
        // Pull the (Place, Rotate) for a given index.
        let place_of = |i: usize| {
            s.iter().find_map(|d| match d {
                GenDirective::Place { path, pos } if path == &format!("led[{i}]") => Some(*pos),
                _ => None,
            })
        };
        let rot_of = |i: usize| {
            s.iter().find_map(|d| match d {
                GenDirective::Rotate { path, orient } if path == &format!("led[{i}]") => {
                    Some(*orient)
                }
                _ => None,
            })
        };
        // led[0] at angle 0 → east point, 0°. led[3] at 90° → north, ≈90°. led[6] →
        // west, ≈180°. All exactly on the ring (positions rounded to nm).
        assert_eq!(place_of(0).unwrap(), pt(10_000_000, 0));
        assert_eq!(rot_of(0).unwrap().to_deg(), 0);
        assert_eq!(place_of(3).unwrap(), pt(0, 10_000_000));
        assert_eq!(rot_of(3).unwrap().to_deg(), 90);
        assert_eq!(rot_of(6).unwrap().to_deg(), 180);
        // 30° (= 360/12) is off-axis: led[1] is a real quaternion, not a cardinal.
        assert_eq!(rot_of(1).unwrap().to_deg(), 30);
        assert!(!rot_of(1).unwrap().is_bottom());
    }

    /// Board + cutout + a Top conductor region lower to exactly one Substrate, one
    /// Void, and one Conductor feature; net rides as an annotation and the conductor
    /// sits on the top-copper z.
    #[test]
    fn features_lowers_board_cutout_and_region() {
        let su = Stackup::default_2layer();
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Cutout {
                shape: Shape2D::rect(pt(5 * MM, 5 * MM), MM, MM),
            },
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];

        let feats = features(&src).unwrap();
        // one substrate, two mask solids (F/B.Mask in the default stackup), one void,
        // one conductor.
        assert_eq!(feats.len(), 5, "substrate + 2 masks + void + conductor");

        let subs: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Substrate)
            .collect();
        assert_eq!(subs.len(), 1, "exactly one substrate feature");
        assert!(subs[0].net.is_none(), "substrate is netless");
        let Extent::Prism { z, .. } = subs[0].feature.extent;
        assert_eq!(z, su.board_z().unwrap(), "substrate spans the board body");

        let voids: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Void)
            .collect();
        assert_eq!(voids.len(), 1, "exactly one void feature");
        assert!(voids[0].net.is_none(), "void is netless");
        let Extent::Prism { z, .. } = voids[0].feature.extent;
        assert_eq!(
            z,
            su.full_z().unwrap(),
            "a board cutout pierces the full stackup (mask + silk too)"
        );

        let conds: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Conductor)
            .collect();
        assert_eq!(conds.len(), 1, "exactly one conductor feature");
        assert_eq!(
            conds[0].net,
            Some(NetId::new("GND")),
            "conductor carries its net annotation"
        );
        let Extent::Prism { z, .. } = conds[0].feature.extent;
        assert_eq!(
            z,
            su.top_copper().unwrap(),
            "F.Cu region sits on top copper"
        );
    }

    /// Every `Role::Mask` slab in the stackup yields exactly one solid mask `Feature`
    /// with the board-outline shape at that slab's z, carrying the slab's material
    /// (Decision 13 — mask is a generated positive solid, not a negative layer). The
    /// default stackup has two mask slabs (F/B.Mask), so a board generates two solids;
    /// a boardless source generates none (no board area to cover).
    #[test]
    fn features_generates_one_mask_solid_per_mask_slab() {
        let su = Stackup::default_2layer();
        let outline = Shape2D::rect(pt(0, 0), 8 * MM, 6 * MM);
        let src = vec![GenDirective::Board {
            outline: outline.clone(),
        }];

        let feats = features(&src).unwrap();
        let masks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Mask)
            .collect();

        let mask_slabs: Vec<&Slab> = su.slabs.iter().filter(|s| s.role == Role::Mask).collect();
        assert_eq!(mask_slabs.len(), 2, "default stackup has F.Mask + B.Mask");
        assert_eq!(masks.len(), 2, "one mask solid per mask slab");
        assert!(masks.iter().all(|f| f.net.is_none()), "mask is netless");

        // Each solid has the board outline at its slab's z and the slab's material.
        for slab in &mask_slabs {
            let m = masks
                .iter()
                .find(|f| matches!(f.feature.extent, Extent::Prism { z, .. } if z == slab.z))
                .unwrap_or_else(|| panic!("a mask solid at {:?}", slab.z));
            let Extent::Prism { shape, .. } = &m.feature.extent;
            assert_eq!(*shape, outline, "mask solid uses the board outline");
            assert_eq!(
                m.feature.material, slab.material,
                "carries the slab material"
            );
        }

        // No `Board` ⇒ no board area ⇒ no mask solids.
        let boardless = features(&vec![]).unwrap();
        assert!(
            !boardless.iter().any(|f| f.feature.role == Role::Mask),
            "a boardless source generates no mask"
        );
    }

    /// A custom stackup with no `Role::Mask` slab generates no mask solids (no special
    /// cases — the generator simply finds nothing to emit).
    #[test]
    fn features_no_mask_slab_generates_no_mask() {
        // A minimal 1-copper-slab stackup: no mask, no silk.
        let src: Source = vec![GenDirective::Slab(Slab {
            name: "F.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: Some(crate::geom::Material::named("copper")),
        })]
        .into_iter()
        .chain(std::iter::once(GenDirective::Board {
            outline: Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
        }))
        .collect();
        let feats = features(&src).unwrap();
        assert!(
            !feats.iter().any(|f| f.feature.role == Role::Mask),
            "no mask slab ⇒ no mask solid"
        );
    }

    /// Two `Board` directives: only the last outline becomes the substrate feature
    /// (mirrors `board_shape`'s "last `Board` wins").
    #[test]
    fn features_last_board_wins() {
        let first = Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM);
        let last = Shape2D::rect(pt(0, 0), 8 * MM, 8 * MM);
        let src = vec![
            GenDirective::Board {
                outline: first.clone(),
            },
            GenDirective::Board {
                outline: last.clone(),
            },
        ];

        let feats = features(&src).unwrap();
        let subs: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Substrate)
            .collect();
        assert_eq!(subs.len(), 1, "only one substrate emitted");
        let Extent::Prism { shape, .. } = &subs[0].feature.extent;
        assert_eq!(*shape, last, "the LAST board outline becomes the substrate");
        assert_ne!(*shape, first, "the earlier board is dropped");
    }

    /// A `text` directive lowers to several `Role::Marking` stroke features sitting on
    /// the named silk slab's **honest z** (not copper z — Decision 13), advancing in +x
    /// across the string (Decision 9).
    #[test]
    fn features_lowers_text_to_marking_strokes() {
        let su = Stackup::default_2layer();
        let src = vec![GenDirective::Text {
            string: "R12".into(),
            at: pt(0, 0),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        }];

        let feats = features(&src).unwrap();
        let marks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Marking)
            .collect();
        // "R12": R(2) + 1(2) + 2(1) = 5 strokes; in any case several, all netless.
        assert!(
            marks.len() >= 3,
            "expected several marking strokes, got {}",
            marks.len()
        );
        assert!(marks.iter().all(|f| f.net.is_none()), "silk is netless");

        // All markings sit on the F.SilkS slab's honest z — above the top copper, not
        // aliased onto it (the pre-Decision-13 stopgap).
        let silk_z = su.slab_z("F.SilkS").unwrap();
        assert_ne!(
            silk_z,
            su.top_copper().unwrap(),
            "silk z is distinct from copper z"
        );
        for m in &marks {
            let Extent::Prism { z, .. } = m.feature.extent;
            assert_eq!(z, silk_z, "marking on the F.SilkS z");
        }

        // The text advances in +x: the rightmost stroke point of the 3-char string
        // lies well to the right of the origin (the '1' and '2' are advanced glyphs).
        let max_x = marks
            .iter()
            .flat_map(|m| {
                let Extent::Prism { shape, .. } = &m.feature.extent;
                shape.points().into_iter().map(|p| p.x)
            })
            .max()
            .unwrap();
        assert!(max_x > MM, "string advances past the first glyph in +x");
    }

    /// An unknown slab name is a hard elaboration error (no silent board-z fallback,
    /// Decision 13); the message names the unknown slab and the available names.
    #[test]
    fn features_unknown_slab_name_is_hard_error() {
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Keepout(crate::geom::KeepoutKind::Copper),
                net: None,
                layer: "Q.Cu".into(),
            }),
        ];
        let err = features(&src).unwrap_err();
        assert!(err.contains("Q.Cu"), "names the unknown slab: {err}");
        assert!(err.contains("F.Cu"), "lists available slabs: {err}");

        // A text label on an unknown slab is likewise a hard error.
        let src = vec![GenDirective::Text {
            string: "X".into(),
            at: pt(0, 0),
            height: MM,
            layer: "Nope".into(),
            orient: Orient::IDENTITY,
        }];
        assert!(features(&src).unwrap_err().contains("Nope"));
    }

    /// A net-bound `Conductor` region on a non-copper slab (silk) is nonsense and is
    /// rejected by the materialization gate (Decision 13).
    #[test]
    fn features_conductor_pour_on_non_copper_slab_errors() {
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.SilkS".into(),
            }),
        ];
        let err = features(&src).unwrap_err();
        assert!(
            err.contains("F.SilkS") && err.contains("non-copper"),
            "rejects a pour on silk: {err}"
        );
    }

    /// A source with `Slab` directives makes `stackup()` return *those* slabs, in
    /// declaration order — not the 2-layer default.
    #[test]
    fn stackup_reads_authored_slabs() {
        // A non-default 2 mm board (distinct z's from `default_2layer`), with the middle
        // dielectric left material-less to also exercise the optional-material path.
        let authored = vec![
            Slab {
                name: "B.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: Some(crate::geom::Material::named("copper")),
            },
            Slab {
                name: "core".into(),
                z: ZRange::new(35_000, 1_965_000),
                role: Role::Substrate,
                material: None,
            },
            Slab {
                name: "F.Cu".into(),
                z: ZRange::new(1_965_000, 2_000_000),
                role: Role::Conductor,
                material: Some(crate::geom::Material::named("copper")),
            },
        ];
        let src: Source = authored.iter().cloned().map(GenDirective::Slab).collect();
        let su = stackup(&src);
        assert_eq!(
            su.slabs, authored,
            "stackup() returns the authored slabs verbatim"
        );
        assert_ne!(
            su,
            Stackup::default_2layer(),
            "authored slabs are not the default (distinct z's)"
        );
    }

    /// With no `Slab` directives, `stackup()` falls back to the unchanged 2-layer
    /// default — even when the source has other (non-slab) directives.
    #[test]
    fn stackup_defaults_when_no_slabs() {
        assert_eq!(stackup(&vec![]), Stackup::default_2layer());
        let src = vec![board_rect(pt(0, 0), pt(10 * MM, 10 * MM))];
        assert_eq!(
            stackup(&src),
            Stackup::default_2layer(),
            "non-slab directives don't disturb the default"
        );
    }

    // ---- refdes-pin reconciliation ----

    fn part_lib(name: &str) -> PartLib {
        let mut lib = PartLib::new();
        lib.insert(
            name.to_string(),
            PartDef {
                name: name.to_string(),
                pins: vec![],
                interfaces: BTreeMap::new(),
                graphics: vec![],
                texts: vec![],
                courtyard: None,
                class: None,
            },
        );
        lib
    }

    fn inst(path: &str, part: &str) -> GenDirective {
        GenDirective::Instance {
            path: path.to_string(),
            part: part.to_string(),
            params: BTreeMap::new(),
            label: None,
        }
    }

    /// Two entities pinned to one identical string surface as an `E_REFDES_PIN_DUP`
    /// finding on an otherwise-valid elaboration (non-blocking, like pos findings).
    #[test]
    fn duplicate_refdes_pin_is_surfaced() {
        let src = vec![inst("c0", "C"), inst("c1", "C")];
        let mut pins = BTreeMap::new();
        pins.insert(EntityId::new("c0"), "C7".to_string());
        pins.insert(EntityId::new("c1"), "C7".to_string());
        let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
        assert_eq!(
            elab.report.refdes_pin_dups,
            vec![(
                "C7".to_string(),
                vec![EntityId::new("c0"), EntityId::new("c1")]
            )]
        );
        assert!(!elab.report.is_clean());
        // Distinct pins do not collide.
        let mut ok = BTreeMap::new();
        ok.insert(EntityId::new("c0"), "C7".to_string());
        ok.insert(EntityId::new("c1"), "C8".to_string());
        let clean = elaborate(&src, &BTreeMap::new(), &ok, &part_lib("C")).expect("elaborates");
        assert!(clean.report.refdes_pin_dups.is_empty());
    }

    /// A refdes pin on an entity that does not exist after elaboration is orphaned —
    /// the same channel and behavior as a stale position override.
    #[test]
    fn refdes_pin_on_unknown_id_is_orphaned() {
        let src = vec![inst("c0", "C")];
        let mut pins = BTreeMap::new();
        pins.insert(EntityId::new("ghost"), "C9".to_string());
        let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
        assert!(elab.report.orphaned.contains(&EntityId::new("ghost")));
    }

    /// An entity carrying BOTH a pos override and a refdes pin, orphaned, is flagged
    /// exactly once (the refdes-orphan loop dedups against the pos-orphan loop).
    #[test]
    fn orphan_with_both_pos_override_and_refdes_pin_is_flagged_once() {
        let src = vec![inst("c0", "C")];
        let ghost = EntityId::new("ghost");
        let mut overrides = BTreeMap::new();
        overrides.insert(
            ghost.clone(),
            Override {
                pos: Some(Point { x: 1, y: 2 }),
                strength: Strength::Pin,
            },
        );
        let mut pins = BTreeMap::new();
        pins.insert(ghost.clone(), "C9".to_string());
        let elab = elaborate(&src, &overrides, &pins, &part_lib("C")).expect("elaborates");
        assert_eq!(
            elab.report
                .orphaned
                .iter()
                .filter(|&id| *id == ghost)
                .count(),
            1,
            "orphan reported once despite two override kinds"
        );
    }
}
