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

use crate::doc::*;
use crate::elaborate::{elaborate, Source};
use crate::id::EntityId;
use crate::part::PartLib;

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

/// Apply a transaction to a document. `tick` is the monotonic global revision
/// used to stamp whichever inputs changed. Returns the new doc, or an error
/// (leaving the caller's original untouched — atomicity).
pub fn apply(doc: &Doc, txn: &Transaction, lib: &PartLib, tick: u64) -> Result<Doc, String> {
    // Work on a candidate clone; only the return value is observed by the caller.
    let mut next = doc.clone();

    for cmd in &txn.0 {
        match cmd {
            Command::SetSource(s) => next.source = s.clone(),
            Command::Nudge(id, p) => {
                next.overrides
                    .insert(id.clone(), Override { pos: Some(*p), strength: Strength::Hint });
            }
            Command::Pin(id, p) => {
                next.overrides
                    .insert(id.clone(), Override { pos: Some(*p), strength: Strength::Pin });
            }
            Command::ClearOverride(id) => {
                next.overrides.remove(id);
            }
            Command::LoadText(text) => {
                let (source, overrides) = crate::text::parse(text)?;
                next.source = source;
                next.overrides = overrides;
            }
            Command::Resolve(id, res) => apply_resolution(&mut next, id, res)?,
        }
    }

    // Re-elaborate. A structural fault aborts the whole transaction.
    let elab = elaborate(&next.source, &next.overrides, lib)?;
    next.components = elab.components;
    next.nets = elab.nets;
    next.report = elab.report;

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
    let connectivity_changed = next.nets != doc.nets || part_shape_changed(doc, &next);
    let geometry_changed = positions_changed(doc, &next);
    next.conn_rev = if connectivity_changed { tick } else { doc.conn_rev };
    next.geom_rev = if geometry_changed { tick } else { doc.geom_rev };

    Ok(next)
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
fn apply_resolution(next: &mut Doc, id: &EntityId, res: &Resolution) -> Result<(), String> {
    match res {
        Resolution::DropOrphan => {
            if !next.report.orphaned.contains(id) {
                return Err(format!("Resolve DropOrphan: `{id}` is not an orphaned override"));
            }
            next.overrides.remove(id);
        }
        Resolution::AcceptConstraint => {
            if !next.report.pin_conflicts.contains(id) {
                return Err(format!("Resolve AcceptConstraint: `{id}` is not a pin conflict"));
            }
            next.overrides.remove(id);
        }
        Resolution::RePin(p) => {
            if !next.report.pin_conflicts.contains(id) {
                return Err(format!("Resolve RePin: `{id}` is not a pin conflict"));
            }
            next.overrides
                .insert(id.clone(), Override { pos: Some(*p), strength: Strength::Pin });
        }
        Resolution::DropRedundant => {
            if !next.report.redundant_pins.contains(id) {
                return Err(format!("Resolve DropRedundant: `{id}` is not a redundant pin"));
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
