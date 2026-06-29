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

use crate::doc::*;
use crate::id::{EntityId, NetId};
use crate::part::{Dir, PartLib};
use crate::solve::{dist, solve, Constraint, Problem, Rect, PLACE_TOL};
use std::collections::{BTreeMap, BTreeSet};

/// A directive in the generative program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenDirective {
    /// Instantiate `part` at hierarchical `path`.
    Instance { path: String, part: String },
    /// Source-provided default placement (a *free* DOF unless overridden).
    Place { path: String, pos: Point },
    /// A hard placement constraint (e.g. a connector mated to a mechanical
    /// datum). Outranks user overrides; surfaces conflicts rather than yielding.
    Fix { path: String, pos: Point },
    /// Board outline; all components are kept within it.
    Board { min: Point, max: Point },
    /// Relational placement constraint solved by the least-change solver.
    Near { a: String, b: String, within: Nm },
    MinSep { a: String, b: String, gap: Nm },
    AlignX { nodes: Vec<String> },
    AlignY { nodes: Vec<String> },
    /// Connect two interface ports. The crossing is determined by the interface
    /// type's mate map, so it cannot be wired backwards.
    ConnectInterface {
        a: (String, String), // (component path, port name)
        b: (String, String),
    },
    /// Connect discrete pins onto a named net.
    ConnectPins { net: String, pins: Vec<(String, String)> }, // (comp path, pin)
    /// Set a component's planar orientation (cardinal degrees: 0/90/180/270). A
    /// settable attribute, not a solver DOF.
    Rotate { path: String, deg: i32 },
    /// Like `Near`, but the target is a specific *pin* (`b_comp`.`b_pin`) rather
    /// than a component centroid. The pin's world position tracks its component's
    /// position + orientation during solving.
    NearPin { a: String, b_comp: String, b_pin: String, within: Nm },
}

/// The generative program (tier 1 authoritative).
pub type Source = Vec<GenDirective>;

/// Result of elaboration before it is folded into a Doc.
pub struct Elaborated {
    pub components: BTreeMap<EntityId, Component>,
    pub nets: BTreeMap<NetId, Net>,
    pub report: ReconReport,
}

/// Elaborate a source program into materialized instances + connectivity,
/// applying ID-keyed overrides. Returns an error (aborting the whole elaboration)
/// on a structural fault — this is what makes a transaction atomic.
pub fn elaborate(
    source: &Source,
    overrides: &BTreeMap<EntityId, Override>,
    lib: &PartLib,
) -> Result<Elaborated, String> {
    let mut components: BTreeMap<EntityId, Component> = BTreeMap::new();
    let mut nets: BTreeMap<NetId, Net> = BTreeMap::new();
    let mut order = 0i64; // deterministic default placement counter

    // Pass 1: instances.
    for d in source {
        if let GenDirective::Instance { path, part } = d {
            if !lib.contains_key(part) {
                return Err(format!("unknown part `{part}` for `{path}`"));
            }
            let id = EntityId::new(path.clone());
            if components.contains_key(&id) {
                return Err(format!("duplicate instance `{path}`"));
            }
            // Default placement: a free DOF, laid out in a row.
            let pos = Dof {
                value: Point { x: order * 10 * MM, y: 0 },
                prov: Provenance::Free,
            };
            order += 1;
            components.insert(
                id.clone(),
                Component { id, part: part.clone(), pos, orient: Orient::default() },
            );
        }
    }

    // Pass 2: source-provided default placement (still free).
    for d in source {
        if let GenDirective::Place { path, pos } = d {
            let id = EntityId::new(path.clone());
            let c = components
                .get_mut(&id)
                .ok_or_else(|| format!("Place targets unknown instance `{path}`"))?;
            c.pos.value = *pos;
        }
    }

    // Pass 2a: orientation (a settable attribute, resolved before constraints so a
    // NearPin target's pin offset can be rotated correctly).
    for d in source {
        if let GenDirective::Rotate { path, deg } = d {
            let id = EntityId::new(path.clone());
            let c = components
                .get_mut(&id)
                .ok_or_else(|| format!("Rotate targets unknown instance `{path}`"))?;
            c.orient = Orient::from_deg(*deg)
                .ok_or_else(|| format!("Rotate `{path}` by {deg}deg: only 0/90/180/270 supported"))?;
        }
    }

    // Pass 2b: collect hard placement constraints (Fix), the board outline, and
    // relational constraints for the solver.
    let mut fixmap: BTreeMap<EntityId, Point> = BTreeMap::new();
    let mut board: Option<Rect> = None;
    let mut relational: Vec<Constraint> = Vec::new();
    let check = |id: &EntityId| -> Result<(), String> {
        if components.contains_key(id) {
            Ok(())
        } else {
            Err(format!("constraint references unknown instance `{id}`"))
        }
    };
    for d in source {
        match d {
            GenDirective::Fix { path, pos } => {
                let id = EntityId::new(path.clone());
                check(&id)?;
                fixmap.insert(id, *pos);
            }
            GenDirective::Board { min, max } => board = Some(Rect { min: *min, max: *max }),
            GenDirective::Near { a, b, within } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                check(&a)?;
                check(&b)?;
                relational.push(Constraint::Near { a, b, within: *within });
            }
            GenDirective::NearPin { a, b_comp, b_pin, within } => {
                let aid = EntityId::new(a.clone());
                let bid = EntityId::new(b_comp.clone());
                check(&aid)?;
                check(&bid)?;
                // Pre-rotate the target pin's local offset by b's orientation; the
                // result is a constant offset the solver adds to b's position.
                let bc = &components[&bid];
                let bdef = &lib[&bc.part];
                let off = bdef.pin_offset(b_pin).ok_or_else(|| {
                    format!("NearPin: `{b_comp}` ({}) has no pin `{b_pin}`", bc.part)
                })?;
                let b_off = bc.orient.rotate(off);
                relational.push(Constraint::NearPin { a: aid, b: bid, b_off, within: *within });
            }
            GenDirective::MinSep { a, b, gap } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                check(&a)?;
                check(&b)?;
                relational.push(Constraint::MinSep { a, b, gap: *gap });
            }
            GenDirective::AlignX { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                for n in &nodes {
                    check(n)?;
                }
                relational.push(Constraint::AlignX { nodes });
            }
            GenDirective::AlignY { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                for n in &nodes {
                    check(n)?;
                }
                relational.push(Constraint::AlignY { nodes });
            }
            _ => {}
        }
    }

    // Pass 3: connections.
    for d in source {
        match d {
            GenDirective::ConnectInterface { a, b } => {
                connect_interface(&components, lib, a, b, &mut nets)?;
            }
            GenDirective::ConnectPins { net, pins } => {
                let id = NetId::new(net.clone());
                let entry = nets.entry(id.clone()).or_insert_with(|| Net {
                    id,
                    name: net.clone(),
                    members: BTreeSet::new(),
                });
                for (comp, pin) in pins {
                    let cid = EntityId::new(comp.clone());
                    if !components.contains_key(&cid) {
                        return Err(format!("net `{net}` references unknown `{comp}`"));
                    }
                    entry.members.insert(PinRef::new(&cid, pin));
                }
            }
            _ => {}
        }
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
    let base: BTreeMap<EntityId, Point> =
        components.iter().map(|(k, c)| (k.clone(), c.pos.value)).collect();
    let no_suppress = BTreeSet::new();
    // We use only `.positions` here: reconciliation's least-change/decay logic is
    // defined purely by where the solver places nodes. The new `Solution` also
    // carries `converged`/`unsatisfied` (infeasibility), which the engine could
    // surface in a future milestone; today the placement is what reconciliation
    // consumes, so the semantics below are unchanged from the relaxation solver.
    let solved_all = solve(&assemble_problem(
        &base, &fixmap, overrides, board, &relational, &no_suppress,
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
                    report.decayed.push((id.clone(), DecayReason::OverriddenByConstraint));
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
        let solved_wo =
            solve(&assemble_problem(&base, &fixmap, overrides, board, &relational, &suppress))
                .positions;
        let effective = dist(solved_all[id], solved_wo[id]) > PLACE_TOL as f64;

        match ov.strength {
            Strength::Hint => {
                if effective {
                    prov_map.insert(id.clone(), Provenance::Hint);
                } else {
                    report.decayed.push((id.clone(), DecayReason::RedundantWithDefault));
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
    let solved_final =
        solve(&assemble_problem(&base, &fixmap, overrides, board, &relational, &decayed))
            .positions;

    for (id, c) in components.iter_mut() {
        let prov = if fixmap.contains_key(id) {
            Provenance::Fixed
        } else {
            prov_map.get(id).copied().unwrap_or(Provenance::Free)
        };
        c.pos = Dof { value: solved_final[id], prov };
    }

    // Orphaned overrides: target no longer exists. Surfaced, never dropped.
    for id in overrides.keys() {
        if !components.contains_key(id) {
            report.orphaned.push(id.clone());
        }
    }

    Ok(Elaborated { components, nets, report })
}

/// Build a solver problem from base placements + overrides + constraints.
/// `suppress` lists override ids to ignore (treat the node as Free at its
/// default) — used to test whether an override is doing anything.
fn assemble_problem(
    base: &BTreeMap<EntityId, Point>,
    fixmap: &BTreeMap<EntityId, Point>,
    overrides: &BTreeMap<EntityId, Override>,
    board: Option<Rect>,
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
        let ov = if suppress.contains(id) { None } else { overrides.get(id) };
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
    Problem { anchors, fixed, board, constraints: relational.to_vec() }
}

/// Connect two interface ports using the interface type's mate map. The mate map
/// is the single place the tx<->rx crossing is defined, so connecting two ports
/// always produces correctly-crossed nets — the swap footgun is unrepresentable.
fn connect_interface(
    components: &BTreeMap<EntityId, Component>,
    lib: &PartLib,
    a: &(String, String),
    b: &(String, String),
    nets: &mut BTreeMap<NetId, Net>,
) -> Result<(), String> {
    let (ap, aport) = a;
    let (bp, bport) = b;
    let aid = EntityId::new(ap.clone());
    let bid = EntityId::new(bp.clone());
    let ac = components
        .get(&aid)
        .ok_or_else(|| format!("connect: unknown instance `{ap}`"))?;
    let bc = components
        .get(&bid)
        .ok_or_else(|| format!("connect: unknown instance `{bp}`"))?;
    let adef = &lib[&ac.part];
    let bdef = &lib[&bc.part];
    let aiface = adef
        .interfaces
        .get(aport)
        .ok_or_else(|| format!("`{}` has no interface port `{aport}`", ac.part))?;
    let biface = bdef
        .interfaces
        .get(bport)
        .ok_or_else(|| format!("`{}` has no interface port `{bport}`", bc.part))?;
    if aiface.type_name != biface.type_name {
        return Err(format!(
            "interface type mismatch: {} vs {}",
            aiface.type_name, biface.type_name
        ));
    }

    for (sa, sb) in &aiface.mate {
        let da = aiface.signals.get(sa).copied();
        let db = biface.signals.get(sb).copied();
        let (Some(da), Some(db)) = (da, db) else {
            return Err(format!("interface `{}` mate references missing signal", aiface.type_name));
        };
        // Direction sanity: a mated pair must be drive/receive, not both drivers.
        if matches!((da, db), (Dir::Out, Dir::Out)) {
            return Err(format!("drive conflict mating {sa}<->{sb}"));
        }
        let net_name = format!("{ap}.{aport}.{sa}");
        let nid = NetId::new(net_name.clone());
        let net = nets.entry(nid.clone()).or_insert_with(|| Net {
            id: nid,
            name: net_name,
            members: BTreeSet::new(),
        });
        net.members.insert(PinRef::new(&aid, &format!("{aport}.{sa}")));
        net.members.insert(PinRef::new(&bid, &format!("{bport}.{sb}")));
    }
    Ok(())
}

// ---- source-building helpers (a stand-in for the textual generative layer) ----

/// Build the demo power-supply module with `n` decoupling caps fanned off the
/// regulator output. This is the "generator" whose output we later override and
/// re-elaborate to test minimal-perturbation reconciliation.
pub fn psu_module(n: usize) -> Source {
    let mut s = vec![GenDirective::Instance { path: "psu.reg".into(), part: "LDO".into() }];
    for i in 0..n {
        let dec = format!("psu.dec[{i}]");
        s.push(GenDirective::Instance { path: dec.clone(), part: "Cap".into() });
        s.push(GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("psu.reg".into(), "VOUT".into()), (dec.clone(), "p1".into())],
        });
        s.push(GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("psu.reg".into(), "GND".into()), (dec, "p2".into())],
        });
    }
    s
}
