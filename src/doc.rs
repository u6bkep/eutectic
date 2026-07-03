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
        Point {
            x: x * MM,
            y: y * MM,
        }
    }
}

/// A component's orientation: an **integer quaternion** rotation (see
/// docs/geometry-model-convergence.md, Decision 6). Storing the quaternion — rather
/// than a cardinal enum or a float angle — keeps orientation exact and deterministic
/// while generalising cleanly to 3D: a planar rotation about z is `(w,0,0,z)`, a
/// flip-to-bottom is a 180° rotation about an in-plane axis, an off-axis tilt is any
/// `(w,x,y,z)`. There is **no mirror flag**: bottom-side is a *rotation* (determinant
/// +1, you flip the part over), and the mirrored *appearance* is a property of the 2D
/// top-view projection, not of the stored transform. "Which side" is derived
/// ([`is_bottom`](Orient::is_bottom)).
///
/// [`apply`](Orient::apply) is the world-map: an integer matrix·point ÷ `|q|²` — no
/// `sin`/`cos`, no `sqrt`. It is **exact** for the lattice-symmetry orientations
/// (cardinals + flips, where `|q|²` divides cleanly) and correctly-rounded
/// (round-half-away) otherwise. Orientation is a *settable* DOF; the solver does not
/// optimise over it (nonlinear; out of scope).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Orient {
    pub w: i64,
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl Default for Orient {
    fn default() -> Self {
        Orient::IDENTITY
    }
}

impl Orient {
    /// No rotation.
    pub const IDENTITY: Orient = Orient {
        w: 1,
        x: 0,
        y: 0,
        z: 0,
    };

    /// Build a planar (about-z) orientation from a cardinal degree count. Accepts any
    /// integer congruent to 0/90/180/270 mod 360 (so −90 == 270, 450 == 90); returns
    /// `None` for off-axis angles (arbitrary planar angles arrive via the Stage-2
    /// authoring lowering). Cardinals are tiny exact quaternions (`|q|²` is 1 or 2).
    pub fn from_deg(d: i32) -> Option<Orient> {
        let q = |w, x, y, z| Some(Orient { w, x, y, z });
        match d.rem_euclid(360) {
            0 => q(1, 0, 0, 0),
            90 => q(1, 0, 0, 1),
            180 => q(0, 0, 0, 1),
            270 => q(1, 0, 0, -1),
            _ => None,
        }
    }

    /// `|q|² = w²+x²+y²+z²` — the rotation matrix's common (integer) denominator.
    fn norm2(self) -> i128 {
        let (w, x, y, z) = (
            self.w as i128,
            self.x as i128,
            self.y as i128,
            self.z as i128,
        );
        w * w + x * x + y * y + z * z
    }

    /// Apply this orientation to a planar (z = 0) local point, rotating about the
    /// origin. Exact integer for cardinals/flips; correctly-rounded otherwise. (Only
    /// the top-left 2×2 of the quaternion rotation matrix is needed, since the input
    /// lies in the z = 0 plane — out-of-plane rotation of a 3D point is reserved.)
    ///
    /// The i128 product `m·p` (`|m| ≤ 4c²`, c = max component magnitude, `|p| ≤ ~1e9`)
    /// stays well within range for V1 (cardinals/flips, c ≤ 1) and overflows only for
    /// `c ≳ 1e14` — far beyond the Stage-2 angle-approximation quaternions, but a bound
    /// to respect when scaling those.
    pub fn apply(self, p: Point) -> Point {
        let (w, x, y, z) = (
            self.w as i128,
            self.x as i128,
            self.y as i128,
            self.z as i128,
        );
        let m00 = w * w + x * x - y * y - z * z;
        let m01 = 2 * (x * y - w * z);
        let m10 = 2 * (x * y + w * z);
        let m11 = w * w - x * x + y * y - z * z;
        let den = self.norm2();
        if den == 0 {
            return p; // a degenerate (zero) quaternion isn't a rotation — never divide by 0
        }
        let (px, py) = (p.x as i128, p.y as i128);
        Point {
            x: rdiv_i128(m00 * px + m01 * py, den) as Nm,
            y: rdiv_i128(m10 * px + m11 * py, den) as Nm,
        }
    }

    /// Flip the (already-rotated) part to the **board bottom**: compose a 180° rotation
    /// about the in-plane y-axis on top of this orientation. This is a *rotation* (you
    /// turn the part over about the board's vertical axis), not a reflection — there is
    /// no mirror flag. Closed form of `FLIP_y · q` where `FLIP_y = (0,0,1,0)`.
    ///
    /// The y-axis (not x-axis) convention matches KiCad and general fab: turning the
    /// board over about its vertical axis negates x while preserving y, so bottom silk
    /// text reads upright rather than upside-down.
    ///
    /// Note: `flipped().flipped()` returns the **antipode** `−q`, not `q`. As a rotation
    /// `−q ≡ q` (every method here is quadratic in the components, so the two are
    /// functionally identical), but they are **not** `==` under the derived `Eq`. V1
    /// never composes flips (elaboration applies `flipped` at most once), so this is
    /// unreachable today; Stage-2 quaternion composition must sign-normalise (or use a
    /// rotation-aware compare) to avoid `−q ≠ q` surprises.
    pub fn flipped(self) -> Orient {
        Orient {
            w: -self.y,
            x: self.z,
            y: self.w,
            z: -self.x,
        }
    }

    /// Is the component flipped to the board bottom? True iff its local `+z` axis maps
    /// below the board plane — the z-image's z-component `w²−x²−y²+z² < 0` (`den > 0`,
    /// so only the sign matters).
    pub fn is_bottom(self) -> bool {
        let (w, x, y, z) = (
            self.w as i128,
            self.x as i128,
            self.y as i128,
            self.z as i128,
        );
        (w * w - x * x - y * y + z * z) < 0
    }

    /// The planar (about-z) rotation in whole degrees, **for display only** — never
    /// authoritative (the quaternion is). Exact for cardinals; a rounded projection
    /// otherwise.
    pub fn to_deg(self) -> i32 {
        let (w, x, y, z) = (self.w as f64, self.x as f64, self.y as f64, self.z as f64);
        // about-z angle = atan2(2(wz+xy), w²+x²−y²−z²); for planar (x=y=0) this is the
        // pure z-rotation, and for cardinals it lands exactly on 0/90/180/270.
        let deg = (2.0 * (w * z + x * y))
            .atan2(w * w + x * x - y * y - z * z)
            .to_degrees()
            .round() as i32;
        deg.rem_euclid(360)
    }

    /// Lower an **arbitrary planar angle** (degrees, about z) to its integer
    /// quaternion `(w, 0, 0, z) = round(S·cos(θ/2), S·sin(θ/2))`. The `cos`/`sin` runs
    /// **once at authoring/parse time** (never re-derived at elaboration — the quaternion
    /// is what's stored and diffed), so it stays off the deterministic geometry path
    /// (`apply` is pure-integer). The fixed scale `S` ([`ORIENT_ANGLE_SCALE`]) bounds the
    /// angular error to ≈ `1/S` rad (sub-µm placement at board radius). For the four
    /// cardinals prefer [`from_deg`](Orient::from_deg) (it yields the tiny exact form).
    pub fn from_angle_deg(deg: f64) -> Orient {
        let half = deg.to_radians() / 2.0;
        let s = ORIENT_ANGLE_SCALE as f64;
        let w = (s * half.cos()).round() as i64;
        let z = (s * half.sin()).round() as i64;
        Orient { w, x: 0, y: 0, z }
    }

    /// The antipodal quaternion `−q`. As a *rotation* `−q ≡ self` (every method here is
    /// quadratic in the components), but it is a distinct value under the derived `Eq` —
    /// used by [`same_rotation`](Orient::same_rotation) and quaternion composition.
    pub fn negated(self) -> Orient {
        Orient {
            w: -self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }

    /// Do two quaternions represent the **same rotation**? True iff equal or antipodal
    /// (`q` and `−q` are the same rotation). The rotation-aware comparison the derived
    /// `Eq` is not.
    pub fn same_rotation(self, o: Orient) -> bool {
        self == o || self == o.negated()
    }
}

/// Fixed scale for [`Orient::from_angle_deg`]: the magnitude the unit half-angle
/// `(cos, sin)` is multiplied by before rounding to an integer quaternion. `1e6` bounds
/// the angular error to ≈ `1e-6` rad (≈ 0.1 µm at a 100 mm placement radius) — far below
/// fab tolerance — while keeping `apply`'s i128 products tiny (~`1e21`, vs the ~`1e38`
/// ceiling).
pub const ORIENT_ANGLE_SCALE: i64 = 1_000_000;

/// Round `num / den` to the nearest integer, half away from zero (`den > 0`). The
/// deterministic rounding [`Orient::apply`] uses for non-exact rotations.
fn rdiv_i128(num: i128, den: i128) -> i128 {
    if num >= 0 {
        (num + den / 2) / den
    } else {
        -((-num + den / 2) / den)
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
        PinRef {
            comp: comp.clone(),
            pin: pin.into(),
        }
    }
}

/// A component instance (materialized by elaboration).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    pub id: EntityId,
    pub part: String,
    pub pos: Dof<Point>,
    /// Orientation ([`Orient`] quaternion). Default identity. Set from the generative
    /// source via `GenDirective::Rotate` (planar rotation + optional bottom-side flip);
    /// used to place pins/pads in world space.
    pub orient: Orient,
    /// Authored identity **parameters** — the display-normal spelling at rest
    /// (`value` → `4.7k`, `tol` → `5%`), never parsed here (Decision 14). Together with
    /// `part` these *are* the component's identity for the BOM (and, later, simulation);
    /// consumers parse at their own boundary. Empty for most ICs, whose identity is the
    /// part name alone. Overlaid on the class `defaults` to form the *effective* params.
    pub params: BTreeMap<String, String>,
    /// Optional display-label override (Decision 14) — a template in its own right,
    /// tried before the class template. Purely cosmetic; carries no identity weight.
    pub label: Option<String>,
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
    /// Refdes pins that collide: each group is the set of entities pinned to one
    /// identical string. A genuine authoring conflict (two parts cannot both be
    /// `C7`) — surfaced loudly as an error, kept until resolved, but non-blocking
    /// like the other override findings (the geometry still elaborated).
    pub refdes_pin_dups: Vec<(String, Vec<EntityId>)>,
    /// Honest verify (Decision 10 / issue 0019): component pairs whose real polygonal
    /// courtyards still overlap at the final placement. Empty when the solver separated
    /// everything; non-empty only when a pair could not be pushed apart (e.g. two
    /// fixed/pinned parts placed into each other). Each pair is ordered `(a, b)` with
    /// `a < b`, matching the deterministic constraint order.
    pub courtyard_overlaps: Vec<(EntityId, EntityId)>,
}

impl ReconReport {
    pub fn is_clean(&self) -> bool {
        self.decayed.is_empty()
            && self.pin_conflicts.is_empty()
            && self.redundant_pins.is_empty()
            && self.orphaned.is_empty()
            && self.refdes_pin_dups.is_empty()
            && self.courtyard_overlaps.is_empty()
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
        for (refdes, ids) in &self.refdes_pin_dups {
            // One diagnostic per collision group, anchored on the first member (ids
            // are in path order); the message names all of them and the string.
            let joined = ids
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Diagnostic::error(
                "E_REFDES_PIN_DUP",
                format!("refdes `{refdes}` is pinned on multiple entities: {joined}"),
                Location::Entity(ids[0].clone()),
            ));
        }
        for (a, b) in &self.courtyard_overlaps {
            out.push(
                Diagnostic::error(
                    "E_COURTYARD_OVERLAP",
                    format!("courtyards of `{a}` and `{b}` overlap at the final placement"),
                    Location::Entity(a.clone()),
                )
                .with_help("free a pin/fix so the solver can separate them, or move one part"),
            );
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
    /// tier 1: ID-keyed **reference-designator pins** (Decision 14's reserved
    /// stability mechanism). A pinned entity takes its string verbatim in the
    /// [`annotate::refdes`](crate::annotate::refdes) query; the auto counter skips a
    /// pinned number so it never collides. Kept separate from [`Override`] because
    /// `strength`/decay/least-change are position concepts with no refdes analogue.
    pub refdes_pins: BTreeMap<EntityId, String>,
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
