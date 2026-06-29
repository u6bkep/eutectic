//! The document: the immutable model that is the single source of truth.
//!
//! Tiers (see docs/architecture.md):
//!   - tier 1 authoritative: `source` (generative program) + `overrides`.
//!   - tier 2 materialized:  `components` + `nets` (produced by elaboration;
//!     positions carry provenance).
//!   - tier 3 derived cache: NOT stored here — computed by the query engine.
//!
//! `conn_rev` / `geom_rev` are coarse input revisions the query engine keys on:
//! connectivity-affecting edits bump `conn_rev`; geometry-only edits bump
//! `geom_rev`. This is what lets a nudge skip ERC entirely (dependency-skip) and
//! a cosmetic net rename skip ERC via early-cutoff.
//!
//! BTreeMap (not HashMap) everywhere: deterministic iteration order is required
//! for canonical, byte-stable serialization (the git story). Persistent
//! structural-sharing maps (`im`) are the production swap for a cheap version DAG.

use crate::id::{EntityId, NetId, TraceId, ViaId};
use crate::route::{Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

/// Fixed-point coordinate in nanometres. Integers so positions compare exactly
/// (no float nondeterminism leaking into diffs or query equality).
pub type Nm = i64;
pub const MM: Nm = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub x: Nm,
    pub y: Nm,
}

impl Point {
    pub fn mm(x: i64, y: i64) -> Point {
        Point { x: x * MM, y: y * MM }
    }
}

/// A component's planar orientation, restricted to the four cardinal rotations so
/// that rotated pin positions stay exact integers (no float/trig nondeterminism
/// leaking into stored coordinates or diffs). Orientation is a *settable* DOF for
/// now — the solver does not optimise over it (that is nonlinear; out of scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Orient {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Orient {
    /// Build from a degree count. Accepts any integer congruent to 0/90/180/270
    /// mod 360 (so -90 == 270, 450 == 90); returns `None` for off-axis angles.
    pub fn from_deg(d: i32) -> Option<Orient> {
        match d.rem_euclid(360) {
            0 => Some(Orient::Deg0),
            90 => Some(Orient::Deg90),
            180 => Some(Orient::Deg180),
            270 => Some(Orient::Deg270),
            _ => None,
        }
    }

    pub fn to_deg(self) -> i32 {
        match self {
            Orient::Deg0 => 0,
            Orient::Deg90 => 90,
            Orient::Deg180 => 180,
            Orient::Deg270 => 270,
        }
    }

    /// Rotate a local offset about the origin by this orientation. Exact for all
    /// four cardinal rotations (integer arithmetic, no trig).
    pub fn rotate(self, p: Point) -> Point {
        match self {
            Orient::Deg0 => p,
            Orient::Deg90 => Point { x: -p.y, y: p.x },
            Orient::Deg180 => Point { x: -p.x, y: -p.y },
            Orient::Deg270 => Point { x: p.y, y: -p.x },
        }
    }
}

/// What is driving a degree of freedom, in order of increasing authority.
/// `Free` is solver/generator-driven; `Hint`/`Pinned` are user-authored (weak vs
/// strong); `Fixed` is a hard constraint (e.g. a mechanical datum) that outranks
/// everything. The same provenance ladder governs auto- vs hand-routed traces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    Free,
    Hint,
    Pinned,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dof<T> {
    pub value: T,
    pub prov: Provenance,
}

/// A reference to a specific *physical pad* on a specific component instance.
///
/// `pin` is the **stable pad identity**, not the functional name: a pad **number**
/// for a discrete pin (`"30"`, `"MP"`), or `"port.signal"` for an interface signal.
/// This is what lets a part with six pads named `IOVDD` carry six distinct members
/// on a net — identity is `(comp, number)`, and the six numbers differ even though
/// the names collide. The two axes of identity are orthogonal: `comp` (the
/// `EntityId` / instance path) separates *instances* (three chained `D1`/`D2`/`D3`,
/// two MCUs); `pin` (the pad number) separates *pads within one instance*. A
/// functional name is only ever a per-component selector
/// ([`PartDef::resolve_selector`](crate::part::PartDef::resolve_selector)) that
/// fans out to these identities at connection time — it never crosses instances.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PinRef {
    pub comp: EntityId,
    pub pin: String,
}

impl PinRef {
    /// Construct a reference from a component id and a stable pad identity (a pad
    /// number, or `port.signal`). Callers turning a user-facing *name* into refs
    /// must fan out through [`PartDef::resolve_selector`](crate::part::PartDef::resolve_selector)
    /// first — this constructor does no name resolution.
    pub fn new(comp: &EntityId, pin: &str) -> PinRef {
        PinRef { comp: comp.clone(), pin: pin.into() }
    }
}

/// A component instance (materialized by elaboration).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub id: EntityId,
    pub part: String,
    pub pos: Dof<Point>,
    /// Planar orientation (cardinal only). Default `Deg0`. Set from the generative
    /// source via `GenDirective::Rotate`; used to place pins in world space.
    pub orient: Orient,
}

/// A net is a hyperedge over a set of pins. Membership *is* the connectivity
/// truth; the schematic drawing would be a view of this, never the reverse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Net {
    pub id: NetId,
    pub name: String,
    pub members: BTreeSet<PinRef>,
}

/// How strongly an override is held. A casual nudge is a `Hint` (weak: yields to
/// hard constraints and is garbage-collected once it stops doing anything). A
/// `Pin` is strong (explicit user intent: kept and surfaced loudly on conflict).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Strength {
    #[default]
    Hint,
    Pin,
}

/// An ID-keyed override delta layered on top of elaboration. The generative
/// source stays clean; per-instance exceptions live here.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Override {
    pub pos: Option<Point>,
    pub strength: Strength,
}

/// Outcome of reconciling overrides against the (re-)elaborated design. This is
/// the conflict channel as a first-class, structured value rather than ad-hoc
/// strings — nothing about an override is ever silently discarded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconReport {
    /// Hints that were garbage-collected because they had no effect (decay).
    pub decayed: Vec<(EntityId, DecayReason)>,
    /// Pins overridden by a hard constraint — surfaced loudly, kept until resolved.
    pub pin_conflicts: Vec<EntityId>,
    /// Pins that no longer change the outcome — advisory, kept.
    pub redundant_pins: Vec<EntityId>,
    /// Overrides whose target entity no longer exists.
    pub orphaned: Vec<EntityId>,
}

impl ReconReport {
    pub fn is_clean(&self) -> bool {
        self.decayed.is_empty()
            && self.pin_conflicts.is_empty()
            && self.redundant_pins.is_empty()
            && self.orphaned.is_empty()
    }
}

/// Reconciliation outcomes are *findings on a valid document* (see
/// `diagnostic.rs`): they ride alongside a doc that successfully elaborated, never
/// aborting the commit that produced them. Severity reflects seriousness, not
/// blocking — a pin conflict is an `Error` (genuinely wrong, surfaced loudly, kept
/// until resolved) even though it does not stop the commit; decay/redundancy are
/// advisory `Warning`s.
impl crate::diagnostic::Diagnose for ReconReport {
    fn diagnostics(&self) -> Vec<crate::diagnostic::Diagnostic> {
        use crate::diagnostic::{Diagnostic, Location};
        let mut out = Vec::new();
        for (id, reason) in &self.decayed {
            let why = match reason {
                DecayReason::RedundantWithDefault => "equalled the generated/solved default",
                DecayReason::OverriddenByConstraint => "was overridden by a hard constraint",
            };
            out.push(Diagnostic::warning(
                "W_HINT_DECAYED",
                format!("hint on `{id}` {why}; garbage-collected"),
                Location::Entity(id.clone()),
            ));
        }
        for id in &self.pin_conflicts {
            out.push(
                Diagnostic::error(
                    "E_PIN_CONFLICT",
                    format!("pin on `{id}` contradicts a hard constraint"),
                    Location::Entity(id.clone()),
                )
                .with_help("accept the constraint (clear the pin), or re-pin to a new position"),
            );
        }
        for id in &self.redundant_pins {
            out.push(Diagnostic::warning(
                "W_PIN_REDUNDANT",
                format!("pin on `{id}` no longer changes the outcome"),
                Location::Entity(id.clone()),
            ));
        }
        for id in &self.orphaned {
            out.push(Diagnostic::error(
                "E_ORPHAN_OVERRIDE",
                format!("override targets `{id}`, which no longer exists"),
                Location::Entity(id.clone()),
            ));
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecayReason {
    /// The hint equalled the generated/solved default.
    RedundantWithDefault,
    /// A hard constraint overrode the hint, so it no longer mattered.
    OverriddenByConstraint,
}

/// The immutable document.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    /// tier 1: the generative program.
    pub source: crate::elaborate::Source,
    /// tier 1: ID-keyed exceptions.
    pub overrides: BTreeMap<EntityId, Override>,
    /// tier 2: materialized instances.
    pub components: BTreeMap<EntityId, Component>,
    /// tier 2: materialized connectivity.
    pub nets: BTreeMap<NetId, Net>,
    /// tier 2: pads deliberately left unconnected. A pad that is neither a net
    /// member nor in this set is a *floating pad* — surfaced by ERC, never silent
    /// (issue 0001's completeness guarantee). Members are pad identities, same as
    /// [`PinRef`] (a pad number, or `port.signal`).
    pub no_connects: BTreeSet<PinRef>,
    /// tier 2: materialized routed copper. Like placement, this is solver/hand
    /// state (not a derived query): a `Pinned` trace is hand/agent-authored, a
    /// `Free` one is a future autorouter's regen-able output. Mutated only through
    /// the command algebra; DRC reads it as a query input (`route_rev`).
    pub traces: BTreeMap<TraceId, Trace>,
    pub vias: BTreeMap<ViaId, Via>,
    /// Structured outcome of override reconciliation (decay/conflicts/orphans).
    pub report: ReconReport,
    /// Coarse input revisions for the query engine.
    pub conn_rev: u64,
    pub geom_rev: u64,
    /// Bumped when routed copper (traces/vias) changes, parallel to conn/geom so
    /// DRC can be skipped precisely (a placement nudge that touches no copper does
    /// not bump this; a route edit bumps only this).
    pub route_rev: u64,
}

/// The coarse inputs the derived queries depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputId {
    Connectivity,
    Geometry,
    Routing,
}

impl Doc {
    pub fn input_rev(&self, which: InputId) -> u64 {
        match which {
            InputId::Connectivity => self.conn_rev,
            InputId::Geometry => self.geom_rev,
            InputId::Routing => self.route_rev,
        }
    }
}
