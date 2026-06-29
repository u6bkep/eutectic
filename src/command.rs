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
