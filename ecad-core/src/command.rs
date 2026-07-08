//! The command algebra: the *only* mutation surface.
//!
//! Both a GUI gesture and an agent edit lower to a `Transaction` of `Command`s.
//! Applying a transaction is a pure function `Doc -> Result<Doc>`: it is validated
//! and either fully applies or fully rejects. There is no half-applied state, so
//! the crash-on-bad-API class cannot occur.
//!
//! After mutating tier-1 (source/overrides), `apply` re-elaborates and diffs the
//! result against the prior doc to bump the coarse input revisions the query
//! engine keys on. This diff *is* the minimal-perturbation machinery made visible
//! to the derived layer.

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::elaborate::{Source, directive_coords, elaborate};
use crate::id::{EntityId, NetId, TraceId, ViaId};
use crate::part::PartLib;
use crate::route::{Trace, Via};

#[derive(Clone, Debug)]
pub enum Command {
    /// Replace the generative program (e.g. change the decoupler count).
    SetSource(Source),
    /// Interactive nudge of an elaborated instance: records a *hint* (weak)
    /// position override. It sticks across re-elaboration while it is doing
    /// something, but decays automatically once it isn't — so casual nudges don't
    /// accumulate as permanent pins.
    Nudge(EntityId, Point),
    /// Explicitly pin an instance: a *strong* override. Kept until cleared, and
    /// surfaced loudly if a hard constraint contradicts it.
    Pin(EntityId, Point),
    /// Drop an override.
    ClearOverride(EntityId),
    /// Replace the *entire* tier-1 state (source + overrides) from canonical text,
    /// parsed by [`crate::text::parse`]. This is the text front-end lowering to the
    /// one mutation surface: a malformed document aborts the transaction (atomic),
    /// so the file is never a back door to an inconsistent state.
    LoadText(String),
    /// Act on a [`ReconReport`] entry, turning a surfaced conflict/orphan into a
    /// resolution. Lowers to an ordinary override mutation (no side channel) and is
    /// validated against the *current* report: resolving an entity that the report
    /// does not actually flag aborts the transaction. See [`Resolution`].
    Resolve(EntityId, Resolution),
    /// Add a routed trace under a caller-supplied stable id. This is the
    /// hand-routing / agent-routing API: the `Trace` carries its own provenance
    /// (`Pinned` for a hand/agent edit, `Free` for a future autorouter). Per-command
    /// checks: the id must be free, the polyline must have >= 2 points, and the width
    /// must be positive. Net existence and slab resolution are checked *post-elaborate*
    /// by [`apply`]'s `validate_routes` (so creating a net and routing it in one
    /// transaction works); any failure aborts the whole transaction (atomicity), so a
    /// dangling trace can never commit.
    AddTrace(TraceId, Trace),
    /// Remove a trace by id (errors if absent).
    RemoveTrace(TraceId),
    /// Add a via under a caller-supplied stable id. Same validation shape as
    /// [`Command::AddTrace`]: free id + positive drill/pad per-command, net/span-slab
    /// resolution post-elaborate.
    AddVia(ViaId, Via),
    /// Remove a via by id (errors if absent).
    RemoveVia(ViaId),
    /// Freeze the routing (Decision 18's workflow payoff): flip the provenance of the
    /// named nets' `Free` (router-owned) traces/vias to `Pinned`, so a subsequent
    /// partial reroute treats them as immovable. An empty net set freezes **every**
    /// net's routing. Only `Free` copper is promoted; `Pinned`/`Hint`/`Fixed` are left
    /// as-is. Pure state edit — no geometry changes, so DRC/ratsnest are unaffected.
    PromoteRoutes { nets: Vec<NetId> },
}

/// How to resolve a single [`ReconReport`] entry. Each variant lowers to an
/// ordinary mutation of the `overrides` map inside [`apply`] — the same path
/// everything else takes — so a resolution re-elaborates, re-reconciles, and is
/// atomic/undoable like any other commit. A `Resolve` command pairs one of these
/// with the [`EntityId`] it applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// For an `orphaned` entry (target entity no longer exists): drop the dead
    /// override.
    DropOrphan,
    /// For a `pin_conflicts` entry (a Pin contradicted by a hard `Fix`): accept the
    /// constraint by clearing the pin, so the part sits at the Fix position with no
    /// lingering conflict.
    AcceptConstraint,
    /// For a `pin_conflicts` entry: keep the pin but move it to a new position. The
    /// hard `Fix` still wins physically, so this may *remain* a conflict (or become
    /// redundant if re-pinned onto the Fix) — that is deliberately the user's call.
    /// Equivalent to a fresh [`Command::Pin`], but validated as a conflict response.
    RePin(Point),
    /// For a `redundant_pins` entry (a Pin the solver would satisfy anyway): drop
    /// the now-pointless pin.
    DropRedundant,
}

#[derive(Clone, Debug, Default)]
pub struct Transaction(pub Vec<Command>);

impl Transaction {
    pub fn one(c: Command) -> Transaction {
        Transaction(vec![c])
    }
}

/// Range-check ingress coordinates against [`crate::geom::MAX_COORD`] (issue 0018),
/// rejecting on the first out-of-range value with an `E_COORD_RANGE` error at `loc`.
/// Commands abort atomically on the first fault, mirroring the other ingress guards.
fn check_coord_range(
    coords: impl IntoIterator<Item = Nm>,
    loc: Location,
) -> Result<(), Vec<Diagnostic>> {
    for n in coords {
        if !crate::geom::coord_ok(n) {
            return Err(vec![Diagnostic::error(
                "E_COORD_RANGE",
                format!(
                    "coordinate {n} nm exceeds the ±{} nm (±1 m) range",
                    crate::geom::MAX_COORD
                ),
                loc,
            )]);
        }
    }
    Ok(())
}

/// Apply a transaction to a document. `tick` is the monotonic global revision
/// used to stamp whichever inputs changed. Returns the new doc, or an error
/// (leaving the caller's original untouched — atomicity).
pub fn apply(
    doc: &Doc,
    txn: &Transaction,
    lib: &PartLib,
    tick: u64,
) -> Result<Doc, Vec<Diagnostic>> {
    // Work on a candidate clone; only the return value is observed by the caller.
    let mut next = doc.clone();

    // Lenient route-id findings from any `LoadText` in this transaction (Decision 22).
    // Accumulated here because re-elaboration below replaces `next.report` wholesale, so
    // they are stamped onto the fresh report after that (the `schematic_wire_warnings`
    // idiom). Empty for a transaction with no `LoadText`, or one whose routes all carried
    // distinct ids (the serializer's own output — undo/redo snapshots never warn).
    let mut route_id_warnings: Vec<Diagnostic> = Vec::new();

    for cmd in &txn.0 {
        match cmd {
            Command::SetSource(s) => {
                check_coord_range(s.iter().flat_map(directive_coords), Location::None)?;
                next.source = s.clone();
            }
            Command::Nudge(id, p) => {
                check_coord_range([p.x, p.y], Location::Entity(id.clone()))?;
                next.overrides.insert(
                    id.clone(),
                    Override {
                        pos: Some(*p),
                        strength: Strength::Hint,
                    },
                );
            }
            Command::Pin(id, p) => {
                check_coord_range([p.x, p.y], Location::Entity(id.clone()))?;
                next.overrides.insert(
                    id.clone(),
                    Override {
                        pos: Some(*p),
                        strength: Strength::Pin,
                    },
                );
            }
            Command::ClearOverride(id) => {
                next.overrides.remove(id);
            }
            Command::LoadText(text) => {
                let parsed = crate::text::parse(text)?;
                route_id_warnings.extend(parsed.warnings);
                next.source = parsed.source;
                next.overrides = parsed.overrides;
                next.refdes_pins = parsed.refdes_pins;
                // Routes are materialized tier-2 state (Decision 18): the parser fills
                // them directly, elaboration never owns them. Their net/slab names are
                // validated below (post-elaborate, against the fresh doc), the same gate
                // AddTrace/AddVia hit.
                next.traces = parsed.traces;
                next.vias = parsed.vias;
                // The authored schematic layout tree (Decision 20). Tier-1 authored state,
                // like `source`; validated post-elaborate below (it needs the elaborated
                // component universe to resolve `sym` paths).
                next.schematic = parsed.schematic;
            }
            Command::Resolve(id, res) => {
                apply_resolution(&mut next, id, res).map_err(|d| vec![d])?
            }
            Command::AddTrace(id, trace) => {
                if next.traces.contains_key(id) {
                    return Err(vec![Diagnostic::error(
                        "E_TRACE_ID_TAKEN",
                        format!("AddTrace: id `{id}` already exists"),
                        Location::Trace(*id),
                    )]);
                }
                if trace.path.len() < 2 {
                    return Err(vec![Diagnostic::error(
                        "E_TRACE_TOO_SHORT",
                        format!("AddTrace `{id}`: a trace needs at least two points"),
                        Location::Trace(*id),
                    )]);
                }
                if trace.width <= 0 {
                    return Err(vec![Diagnostic::error(
                        "E_TRACE_WIDTH",
                        format!("AddTrace `{id}`: width must be positive"),
                        Location::Trace(*id),
                    )]);
                }
                // Net existence (and slab resolution) is validated post-elaborate by
                // `validate_routes` — the single authority. A per-command check here read
                // the *pre*-elaborate net set, which false-rejected a same-transaction
                // `[SetSource(creates net X), AddTrace(net X)]`; dropped so the two agree.
                check_coord_range(
                    trace
                        .path
                        .iter()
                        .flat_map(|p| [p.x, p.y])
                        .chain([trace.width]),
                    Location::Trace(*id),
                )?;
                next.traces.insert(*id, trace.clone());
            }
            Command::RemoveTrace(id) => {
                if next.traces.remove(id).is_none() {
                    return Err(vec![Diagnostic::error(
                        "E_NO_TRACE",
                        format!("RemoveTrace: no trace `{id}`"),
                        Location::Trace(*id),
                    )]);
                }
            }
            Command::AddVia(id, via) => {
                if next.vias.contains_key(id) {
                    return Err(vec![Diagnostic::error(
                        "E_VIA_ID_TAKEN",
                        format!("AddVia: id `{id}` already exists"),
                        Location::Via(*id),
                    )]);
                }
                if via.drill <= 0 || via.pad <= 0 {
                    return Err(vec![Diagnostic::error(
                        "E_VIA_GEOMETRY",
                        format!("AddVia `{id}`: drill and pad must be positive"),
                        Location::Via(*id),
                    )]);
                }
                // Net existence (and span slab resolution) is validated post-elaborate by
                // `validate_routes` — the single authority (see `AddTrace`).
                check_coord_range([via.at.x, via.at.y, via.drill, via.pad], Location::Via(*id))?;
                next.vias.insert(*id, via.clone());
            }
            Command::RemoveVia(id) => {
                if next.vias.remove(id).is_none() {
                    return Err(vec![Diagnostic::error(
                        "E_NO_VIA",
                        format!("RemoveVia: no via `{id}`"),
                        Location::Via(*id),
                    )]);
                }
            }
            Command::PromoteRoutes { nets } => {
                // Empty selection freezes everything; otherwise scope to the named nets.
                let scoped = |net: &NetId| nets.is_empty() || nets.contains(net);
                for t in next.traces.values_mut() {
                    if t.prov == Provenance::Free && scoped(&t.net) {
                        t.prov = Provenance::Pinned;
                    }
                }
                for v in next.vias.values_mut() {
                    if v.prov == Provenance::Free && scoped(&v.net) {
                        v.prov = Provenance::Pinned;
                    }
                }
            }
        }
    }

    // Re-elaborate. A structural fault aborts the whole transaction.
    let elab = elaborate(&next.source, &next.overrides, &next.refdes_pins, lib)?;
    next.components = elab.components;
    next.nets = elab.nets;
    next.no_connects = elab.no_connects;
    // Per-instance stamped schematic fragments (Decision 20 embedded in a def), consumed by
    // `reflow_schematic` and the schematic-layout gate below (a def-instance `sym` is legal
    // and expands, never an unknown-typo error).
    next.def_fragments = elab.def_fragments;
    next.report = elab.report;
    // Stamp the lenient route-id findings onto the fresh report (Decision 22). A
    // non-`LoadText` transaction leaves this empty, clearing any stale prior findings —
    // the report is rebuilt from scratch each commit.
    next.report.route_id_warnings = route_id_warnings;

    // Commit-time route validation (Decision 18 / 13). Traces/vias are tier-2 state, so
    // the tier-1 elaborate gate above never sees them — but the fail-loud `expect()` in
    // `check_drc`/`pours`/drill export relies on every committed route's slab NAME
    // resolving to a copper slab and every route net EXISTING. This is that gate, for the
    // whole route set at once (so LoadText, AddTrace, and AddVia are all covered — an
    // AddTrace whose net is dropped by a same-transaction SetSource is caught here even
    // though the per-command AddTrace check passed against the pre-elaborate net set).
    validate_routes(&next)?;

    // Commit-time schematic-layout validation (Decision 20). Like `validate_routes`, this
    // is a post-elaborate gate over tier-1 authored state that `elaborate` does not own:
    // it needs the freshly-elaborated component universe to resolve `sym` paths. A `sym`
    // path unknown to the *source* is a hard `E_SCHEMATIC` abort (a typo); a path the
    // source *did* declare but a false `if=` depopulated (Decision 21b DNP) — or whose
    // part the library failed to resolve (the permissive `W_UNRESOLVED_PART` skip) — is
    // **not** an error: it degrades to the unplaced bin like any other unplaced part, so
    // toggling a population variant never blocks a commit (§20c totality). Duplicate
    // paths / duplicate
    // sibling names stay hard errors. The `W_SCHEMATIC_UNPLACED` finding (unplaced *and*
    // DNP-dropped placed symbols) is non-blocking and rides the `ReconReport` (the
    // `W_FONT_LOAD` idiom).
    if let Some(layout) = &next.schematic {
        let ids: std::collections::BTreeSet<EntityId> = next.components.keys().cloned().collect();
        // Unresolved-part instance paths (library packages, slice 1): declared by the
        // source but skipped by elaboration because the library lacks the part. A `sym`
        // or wire endpoint on one must DEGRADE (like a DNP drop), not hard-abort — the
        // permissive `W_UNRESOLVED_PART` promise is that the doc still loads.
        let unresolved: std::collections::BTreeSet<String> = next
            .report
            .unresolved_parts
            .iter()
            .map(|(id, _, _)| id.as_str().to_string())
            .collect();
        // Pass the per-instance stamped fragment table (Decision 20 embedded in a def) so a
        // def-instance `sym` is legal (it expands) rather than an unknown-typo error, and so
        // any doc-level sym that overrides a fragment placement surfaces as a warning.
        let (errors, unplaced, override_warnings) = crate::schematic::validate(
            layout,
            &ids,
            &elab.dnp_dropped,
            &unresolved,
            &next.def_fragments,
        );
        if !errors.is_empty() {
            return Err(errors);
        }
        next.report.unplaced_components = unplaced;
        next.report.schematic_override_warnings = override_warnings;

        // Drawn-wire validation (Decision 20d): a sibling gate that also needs the part
        // library (to resolve endpoint pins) and the elaborated netlist (to spot a wire
        // drawn across two nets), which `validate` does not. An unknown endpoint comp/pin
        // is a hard `E_SCHEMATIC` abort (a typo, like an unknown `sym` path); a wire on a
        // DNP-dropped part, or across two nets, degrades to a non-blocking
        // `W_SCHEMATIC_WIRE` finding on the report (§20d — a wire is presentational).
        let parts: std::collections::BTreeMap<EntityId, String> = next
            .components
            .iter()
            .map(|(id, c)| (id.clone(), c.part.clone()))
            .collect();
        // Resolved identity → net name, built once from the materialized netlist.
        let pin_to_net: std::collections::BTreeMap<crate::doc::PinRef, String> = next
            .nets
            .values()
            .flat_map(|net| net.members.iter().map(|m| (m.clone(), net.name.clone())))
            .collect();
        let pin_net = |p: &crate::doc::PinRef| pin_to_net.get(p).cloned();
        let (wire_errors, wire_warnings) = crate::schematic::validate_wires(
            layout,
            &parts,
            lib,
            &elab.dnp_dropped,
            &unresolved,
            &pin_net,
        );
        if !wire_errors.is_empty() {
            return Err(wire_errors);
        }
        next.report.schematic_wire_warnings = wire_warnings;
    } else {
        next.report.unplaced_components.clear();
        next.report.schematic_wire_warnings.clear();
        next.report.schematic_override_warnings.clear();
    }

    // Decay: garbage-collect hints that the reconciliation found ineffective.
    // Removing them does not change positions (they had no effect), so the
    // materialized result above is still correct — we just stop carrying dead
    // overrides forward. Pins are never auto-removed (only flagged).
    for (id, _reason) in &next.report.decayed {
        next.overrides.remove(id);
    }

    // Bump coarse input revisions by diffing materialized state against the prior
    // doc. Connectivity and geometry move independently so the query engine can
    // skip work precisely.
    let connectivity_changed = next.nets != doc.nets
        || next.no_connects != doc.no_connects
        || part_shape_changed(doc, &next);
    // Pours are derived geometry; a region-only edit changes the DRC pour fills, so it
    // must bump geom_rev (which `Drc` depends on) or the fill would go stale.
    let geometry_changed = positions_changed(doc, &next)
        || crate::elaborate::regions(&next.source) != crate::elaborate::regions(&doc.source);
    let routing_changed = next.traces != doc.traces || next.vias != doc.vias;
    next.conn_rev = if connectivity_changed {
        tick
    } else {
        doc.conn_rev
    };
    next.geom_rev = if geometry_changed { tick } else { doc.geom_rev };
    next.route_rev = if routing_changed { tick } else { doc.route_rev };

    Ok(next)
}

/// Commit-time validation of the routing state zone against the freshly-elaborated doc
/// (Decision 18 / 13 rule 2). Every trace/via must (1) carry a net that exists in the
/// materialized `nets`, and (2) name copper slabs that resolve in the stackup — a trace's
/// `layer` and a via's explicit `span` endpoints must each be a real **copper** slab
/// (`E_UNKNOWN_SLAB` for a name absent from the stackup, `E_NON_COPPER_SLAB` for a name
/// that resolves but is not copper). Collect-all: every offending route is reported in
/// one pass. This is the contract `check_drc`/`pours`/drill export lean on — a committed
/// doc's routes always resolve — so it must be airtight.
fn validate_routes(doc: &Doc) -> Result<(), Vec<Diagnostic>> {
    let su = crate::elaborate::stackup(&doc.source);
    let is_copper = |name: &str| -> Option<bool> {
        su.slab(name)
            .map(|s| s.role == crate::geom::Role::Conductor)
    };
    let mut errors: Vec<Diagnostic> = Vec::new();

    // A named slab used by a route must exist AND be copper.
    let check_slab = |name: &str, loc: Location, errors: &mut Vec<Diagnostic>| match is_copper(name)
    {
        Some(true) => {}
        Some(false) => errors.push(Diagnostic::error(
            "E_NON_COPPER_SLAB",
            format!("route references non-copper slab `{name}`"),
            loc,
        )),
        None => errors.push(Diagnostic::error(
            "E_UNKNOWN_SLAB",
            format!("route references unknown slab `{name}`"),
            loc,
        )),
    };

    for (id, t) in &doc.traces {
        if !doc.nets.contains_key(&t.net) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_NET",
                format!("trace `{id}` is on unknown net `{}`", t.net),
                Location::Trace(*id),
            ));
        }
        check_slab(&t.layer, Location::Trace(*id), &mut errors);
    }
    for (id, v) in &doc.vias {
        if !doc.nets.contains_key(&v.net) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_NET",
                format!("via `{id}` is on unknown net `{}`", v.net),
                Location::Via(*id),
            ));
        }
        // A `None` span is the full copper extent (always resolvable); only an explicit
        // blind/buried span names slabs to validate.
        if let Some((from, to)) = &v.span {
            check_slab(from, Location::Via(*id), &mut errors);
            check_slab(to, Location::Via(*id), &mut errors);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Did the set of components or their part types change? (Affects resolved roles.)
fn part_shape_changed(a: &Doc, b: &Doc) -> bool {
    if a.components.len() != b.components.len() {
        return true;
    }
    for (id, ca) in &a.components {
        match b.components.get(id) {
            Some(cb) if cb.part == ca.part => {}
            _ => return true,
        }
    }
    false
}

/// Did any component position (value or provenance) change?
fn positions_changed(a: &Doc, b: &Doc) -> bool {
    for (id, cb) in &b.components {
        match a.components.get(id) {
            Some(ca) if ca.pos == cb.pos => {}
            _ => return true,
        }
    }
    // also catches removals affecting geometry
    a.components.len() != b.components.len()
}

/// Lower a [`Command::Resolve`] to an `overrides` mutation on the candidate doc.
///
/// Validated against the candidate's *current* report (a clone of the doc being
/// applied to): resolving an entity the report does not flag in the matching
/// category is an error, which aborts the whole transaction (atomicity). This is
/// what makes a resolution distinct from the raw `ClearOverride`/`Pin` primitives
/// it shares a mutation with — it must target a genuinely outstanding issue.
fn apply_resolution(next: &mut Doc, id: &EntityId, res: &Resolution) -> Result<(), Diagnostic> {
    match res {
        Resolution::DropOrphan => {
            if !next.report.orphaned.contains(id) {
                return Err(Diagnostic::error(
                    "E_NOT_ORPHAN",
                    format!("Resolve DropOrphan: `{id}` is not an orphaned override"),
                    Location::Entity(id.clone()),
                ));
            }
            next.overrides.remove(id);
        }
        Resolution::AcceptConstraint => {
            if !next.report.pin_conflicts.contains(id) {
                return Err(Diagnostic::error(
                    "E_NOT_CONFLICT",
                    format!("Resolve AcceptConstraint: `{id}` is not a pin conflict"),
                    Location::Entity(id.clone()),
                ));
            }
            next.overrides.remove(id);
        }
        Resolution::RePin(p) => {
            if !next.report.pin_conflicts.contains(id) {
                return Err(Diagnostic::error(
                    "E_NOT_CONFLICT",
                    format!("Resolve RePin: `{id}` is not a pin conflict"),
                    Location::Entity(id.clone()),
                ));
            }
            if !crate::geom::point_ok(*p) {
                return Err(Diagnostic::error(
                    "E_COORD_RANGE",
                    format!(
                        "Resolve RePin `{id}`: ({}, {}) exceeds the ±{} nm (±1 m) range",
                        p.x,
                        p.y,
                        crate::geom::MAX_COORD
                    ),
                    Location::Entity(id.clone()),
                ));
            }
            next.overrides.insert(
                id.clone(),
                Override {
                    pos: Some(*p),
                    strength: Strength::Pin,
                },
            );
        }
        Resolution::DropRedundant => {
            if !next.report.redundant_pins.contains(id) {
                return Err(Diagnostic::error(
                    "E_NOT_REDUNDANT",
                    format!("Resolve DropRedundant: `{id}` is not a redundant pin"),
                    Location::Entity(id.clone()),
                ));
            }
            next.overrides.remove(id);
        }
    }
    Ok(())
}

/// One suggested way to act on a specific [`ReconReport`] entry: the entity, a
/// short rationale, and (when no extra input is needed) a ready-to-apply command.
/// This is the discoverability surface — a GUI or agent can list "here's what you
/// can do about each issue" straight from a report.
#[derive(Clone, Debug)]
pub struct Suggestion {
    /// The entity the report flagged.
    pub entity: EntityId,
    /// Short, stable description of the issue and what this resolution does.
    pub note: &'static str,
    /// A command that applies directly, or `None` when the resolution needs user
    /// input (e.g. a re-pin target position) and so cannot be pre-filled.
    pub command: Option<Command>,
}

/// Enumerate the suggested resolution command(s) for every *actionable* entry in a
/// report. Orphans, pin-vs-constraint conflicts, and redundant pins each map to one
/// or more [`Suggestion`]s; a pin conflict yields two (accept the constraint, or
/// re-pin). `decayed` entries are intentionally omitted: a decayed hint is already
/// garbage-collected at commit, so there is nothing left to act on.
pub fn suggested_resolutions(report: &ReconReport) -> Vec<Suggestion> {
    let mut out = Vec::new();
    for id in &report.orphaned {
        out.push(Suggestion {
            entity: id.clone(),
            note: "orphaned override (target entity is gone): drop it",
            command: Some(Command::Resolve(id.clone(), Resolution::DropOrphan)),
        });
    }
    for id in &report.pin_conflicts {
        out.push(Suggestion {
            entity: id.clone(),
            note: "pin contradicts a hard constraint: accept the constraint (clear the pin)",
            command: Some(Command::Resolve(id.clone(), Resolution::AcceptConstraint)),
        });
        out.push(Suggestion {
            entity: id.clone(),
            note: "pin contradicts a hard constraint: keep it, re-pinning to a new position",
            // Needs a target: Command::Resolve(id, Resolution::RePin(p)) or Command::Pin(id, p).
            command: None,
        });
    }
    for id in &report.redundant_pins {
        out.push(Suggestion {
            entity: id.clone(),
            note: "pin no longer changes the outcome: drop it (un-pin)",
            command: Some(Command::Resolve(id.clone(), Resolution::DropRedundant)),
        });
    }
    out
}

#[cfg(test)]
mod route_commit_tests {
    use super::*;
    use crate::elaborate::{GenDirective as G, board_rect};
    use crate::history::History;
    use crate::route::{Trace, Via};

    /// One single-pad footprint on F.Cu (pad copper at the instance origin).
    fn one_pad() -> crate::part::PartDef {
        crate::kicad::import_footprint(
            r#"(footprint "P1" (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu")))"#,
        )
        .unwrap()
    }

    /// A 20x20 board with two GND pads at (5,5) and (15,5) — a net that exists so a
    /// route may reference it.
    fn scene() -> (History, PartLib) {
        let mut lib = crate::part::part_library();
        lib.insert("P".into(), one_pad());
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "a".into(),
                part: "P".into(),
                params: Default::default(),
                label: None,
            },
            G::Instance {
                path: "b".into(),
                part: "P".into(),
                params: Default::default(),
                label: None,
            },
            G::Place {
                path: "a".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "b".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        (h, lib)
    }

    fn tr(layer: &str, prov: Provenance) -> Trace {
        Trace {
            net: NetId::new("GND"),
            layer: layer.into(),
            path: vec![Point::mm(5, 5), Point::mm(15, 5)],
            width: 150_000,
            prov,
        }
    }

    /// A re-elaboration (a fresh SetSource that keeps the net) must NOT wipe committed
    /// routes — they are tier-2 state the parser/commands own, not GenDirectives
    /// (Decision 18). A `Pinned` trace survives a subsequent source edit.
    #[test]
    fn reelaborate_preserves_pinned_route() {
        let (mut h, lib) = scene();
        h.commit(
            Transaction::one(Command::AddTrace(
                TraceId(1),
                tr("F.Cu", Provenance::Pinned),
            )),
            &lib,
            "t",
        )
        .unwrap();
        assert_eq!(h.doc().traces.len(), 1);
        // Re-issue the same source (a re-elaboration). The trace must persist.
        let src = h.doc().source.clone();
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "reelab")
            .unwrap();
        assert_eq!(
            h.doc().traces.len(),
            1,
            "re-elaboration must not wipe routes"
        );
        assert_eq!(h.doc().traces[&TraceId(1)].prov, Provenance::Pinned);
    }

    /// `LoadText` populates `doc.traces`/`doc.vias` directly (routes are materialized
    /// state, Decision 18) and the loaded routes survive elaboration. Full text path:
    /// serialize a routed doc, LoadText it back, confirm the routes committed.
    #[test]
    fn loadtext_populates_and_preserves_routes() {
        let (mut h, lib) = scene();
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), tr("F.Cu", Provenance::Free))),
            &lib,
            "t",
        )
        .unwrap();
        let text = crate::text::serialize(h.doc());
        // Load that text into a fresh history: the route must materialize on commit.
        let mut h2 = History::new(Default::default());
        h2.commit(Transaction::one(Command::LoadText(text)), &lib, "load")
            .unwrap();
        assert_eq!(h2.doc().traces.len(), 1, "LoadText materializes the route");
        assert_eq!(
            h2.doc().traces[&TraceId(1)].prov,
            Provenance::Free,
            "provenance round-trips"
        );
        assert_eq!(h2.doc().traces[&TraceId(1)].layer, "F.Cu");
    }

    /// End-to-end def-embedded layout stamping (Decision 20 embedded in a def): a `def`
    /// with a `schematic { … }` fragment, instantiated twice at the doc level, elaborates
    /// so `Doc::def_fragments` holds a per-instance stamped fragment, and
    /// `reflow_schematic` expands each doc-level `sym <instance>` into its group — with the
    /// two instances rendering at identical relative internal geometry.
    #[test]
    fn def_embedded_layout_stamps_per_instance() {
        let lib = crate::part::part_library();
        let text = "\
def rc {
  inst R1 Cap
  inst C1 Cap
  net mid R1.p2 C1.p1
  schematic {
    column {
      sym R1
      sym C1
    }
  }
}
inst a rc
inst b rc
schematic {
  row {
    sym a
    sym b
  }
}
";
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::LoadText(text.to_string())),
            &lib,
            "load",
        )
        .expect("def-with-layout doc commits cleanly");
        let doc = h.doc();
        // Both instances recorded a stamped fragment, keyed by instance path.
        assert!(doc.def_fragments.contains_key("a"));
        assert!(doc.def_fragments.contains_key("b"));
        // The internal components elaborated at the prefixed paths.
        assert!(doc.components.contains_key(&EntityId::new("a.R1")));
        assert!(doc.components.contains_key(&EntityId::new("b.C1")));

        // Reflow expands each def-instance sym into its fragment group. All four internal
        // components get a coordinate (none fall to the unplaced bin).
        let placed = doc.reflow_schematic(&lib);
        for p in ["a.R1", "a.C1", "b.R1", "b.C1"] {
            assert!(placed.contains_key(&EntityId::new(p)), "`{p}` placed");
        }
        // Identical relative internal geometry across the two instances.
        let off = |inst: &str| {
            let r = placed[&EntityId::new(format!("{inst}.R1"))].center;
            let c = placed[&EntityId::new(format!("{inst}.C1"))].center;
            (c.x - r.x, c.y - r.y)
        };
        assert_eq!(off("a"), off("b"), "reused circuit renders identically");
        // No unplaced-bin warning (every internal component is placed via its fragment).
        assert!(
            doc.report.unplaced_components.is_empty(),
            "all placed: {:?}",
            doc.report.unplaced_components
        );
    }

    /// Commit-time slab validation: a trace on a typo'd slab is a hard `E_UNKNOWN_SLAB`
    /// fault at commit (the gate `check_drc`/`pours` lean on).
    #[test]
    fn trace_on_unknown_slab_rejected_at_commit() {
        let (mut h, lib) = scene();
        let err = h
            .commit(
                Transaction::one(Command::AddTrace(
                    TraceId(1),
                    tr("F.Cuu", Provenance::Pinned),
                )),
                &lib,
                "bad",
            )
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
            "unknown slab rejected: {err:?}"
        );
    }

    /// Commit-time slab validation: a trace on a real but non-copper slab (silk) is a
    /// hard `E_NON_COPPER_SLAB` fault.
    #[test]
    fn trace_on_non_copper_slab_rejected_at_commit() {
        let (mut h, lib) = scene();
        let err = h
            .commit(
                Transaction::one(Command::AddTrace(
                    TraceId(1),
                    tr("F.SilkS", Provenance::Pinned),
                )),
                &lib,
                "silk",
            )
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_NON_COPPER_SLAB"),
            "non-copper slab rejected: {err:?}"
        );
    }

    /// A route whose net is dropped by a same-transaction SetSource is caught by the
    /// post-elaborate `validate_routes` gate (the single net-existence authority now that
    /// the per-command net check is gone). This is the contract airtightness the fail-loud
    /// DRC `expect()` depends on.
    #[test]
    fn route_orphaned_by_source_edit_rejected() {
        let (mut h, lib) = scene();
        h.commit(
            Transaction::one(Command::AddTrace(
                TraceId(1),
                tr("F.Cu", Provenance::Pinned),
            )),
            &lib,
            "t",
        )
        .unwrap();
        // A new source with NO GND net; the committed GND trace is now orphaned.
        let src = vec![board_rect(Point::mm(0, 0), Point::mm(20, 20))];
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "drop")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_NET"),
            "orphaned route net rejected: {err:?}"
        );
    }

    /// Creating a net and routing it in ONE transaction must succeed: the per-command net
    /// check used to read the pre-elaborate net set and false-reject this; now
    /// `validate_routes` runs post-elaborate, sees the fresh net, and passes. A pad on a
    /// NEW net plus a trace on that net, committed together.
    #[test]
    fn create_and_route_in_one_transaction() {
        let mut lib = crate::part::part_library();
        lib.insert("P".into(), one_pad());
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "a".into(),
                part: "P".into(),
                params: Default::default(),
                label: None,
            },
            G::Instance {
                path: "b".into(),
                part: "P".into(),
                params: Default::default(),
                label: None,
            },
            G::Place {
                path: "a".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "b".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "NEW".into(),
                pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
            },
        ];
        let trace = Trace {
            net: NetId::new("NEW"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(5, 5), Point::mm(15, 5)],
            width: 150_000,
            prov: Provenance::Pinned,
        };
        // One transaction: the SetSource creates net NEW, the AddTrace references it.
        let mut h = History::new(Default::default());
        h.commit(
            Transaction(vec![
                Command::SetSource(src),
                Command::AddTrace(TraceId(1), trace),
            ]),
            &lib,
            "create+route",
        )
        .expect("create-and-route in one txn must succeed");
        assert_eq!(h.doc().traces.len(), 1, "the route committed");
    }

    /// A via whose explicit blind span names an unknown slab is rejected at commit.
    #[test]
    fn via_bad_span_rejected_at_commit() {
        let (mut h, lib) = scene();
        let v = Via {
            net: NetId::new("GND"),
            at: Point::mm(5, 5),
            span: Some(("F.Cu".into(), "Nope.Cu".into())),
            drill: 300_000,
            pad: 600_000,
            prov: Provenance::Pinned,
        };
        let err = h
            .commit(Transaction::one(Command::AddVia(ViaId(1), v)), &lib, "via")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
            "bad via span rejected: {err:?}"
        );
    }

    /// `PromoteRoutes` freezes router-owned copper: a `Free` trace becomes `Pinned`; a
    /// `Pinned` one is untouched. Net-scoped selection only promotes the named nets.
    #[test]
    fn promote_routes_freezes_free_copper() {
        let (mut h, lib) = scene();
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), tr("F.Cu", Provenance::Free))),
            &lib,
            "free",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::AddTrace(
                TraceId(2),
                tr("B.Cu", Provenance::Pinned),
            )),
            &lib,
            "pin",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::PromoteRoutes { nets: vec![] }),
            &lib,
            "freeze",
        )
        .unwrap();
        assert_eq!(
            h.doc().traces[&TraceId(1)].prov,
            Provenance::Pinned,
            "free → pinned"
        );
        assert_eq!(
            h.doc().traces[&TraceId(2)].prov,
            Provenance::Pinned,
            "pinned unchanged"
        );
    }

    /// Net-scoped `PromoteRoutes` only freezes the named nets' free copper.
    #[test]
    fn promote_routes_net_scoped() {
        let (mut h, lib) = scene();
        // A second net so we can scope.
        let src = {
            let mut s = h.doc().source.clone();
            s.push(G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("a".into(), "1".into())],
            });
            s
        };
        // (Can't reuse pad "1" on two nets in reality, but ConnectPins just needs the net
        // to exist for the route validation; use a distinct pad selector is unnecessary —
        // the test only checks provenance flips, not DRC.)
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "sig")
            .unwrap();
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), tr("F.Cu", Provenance::Free))),
            &lib,
            "g",
        )
        .unwrap();
        let mut sig = tr("F.Cu", Provenance::Free);
        sig.net = NetId::new("SIG");
        sig.path = vec![Point::mm(5, 6), Point::mm(15, 6)];
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(2), sig)),
            &lib,
            "s",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::PromoteRoutes {
                nets: vec![NetId::new("GND")],
            }),
            &lib,
            "freeze-gnd",
        )
        .unwrap();
        assert_eq!(
            h.doc().traces[&TraceId(1)].prov,
            Provenance::Pinned,
            "GND frozen"
        );
        assert_eq!(
            h.doc().traces[&TraceId(2)].prov,
            Provenance::Free,
            "SIG untouched"
        );
    }
}
