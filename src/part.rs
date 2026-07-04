//! Part library: typed pins and typed interfaces.
//!
//! This is where "make the serial-wire swap unrepresentable" lives. A connection
//! between two devices is made at the *interface* level, and the interface type
//! itself encodes how two instances mate (UART crosses tx<->rx). A designer never
//! wires individual signals, so connecting tx-to-tx is not expressible.
//!
//! The module is a facade over three private submodules — the type/geometry model
//! stays here, [`geometry`] holds the world-transform + feature-producing fold, and
//! [`library`] holds the built-in toy `part_library` fixture — all re-exported so every
//! `crate::part::` path keeps resolving.

use crate::doc::{Nm, Orient, Point};
use crate::geom::Shape2D;
use crate::part::Dir::*;
use std::collections::BTreeMap;

mod geometry;
mod library;

pub use geometry::{
    COURTYARD_MARGIN, courtyard_half_extents, courtyard_shape, graphic_features, pad_copper_world,
    pin_world, swap_side, text_features, to_world,
};
pub use library::part_library;
#[cfg(test)]
pub(crate) use library::pin;

/// Which copper layer(s) a pad's copper occupies. SMD pads sit on one outer layer;
/// a plated through-hole's copper is `Through` (top + bottom, conceptually a barrel
/// between). The board stackup resolves these to real z when the pad is placed
/// (`geom::Stackup`); this is the layer-relative, stackup-independent form a
/// reusable footprint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadLayers {
    Top,
    Bottom,
    Through,
}

/// A drilled hole in a pad (a [`geom::Role::Void`](crate::geom::Role) once placed),
/// in **component-local** coordinates — round, or a slot between two points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drill {
    Round { d: Nm },
    Slot { a: Point, b: Point, d: Nm },
}

/// One copper region of a pad: a real [`Shape2D`] (so a custom/compound pad is a
/// *union* of these — the BMP581 case) on a set of layers, in **component-local**
/// coordinates (same frame as [`PinDef::offset`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadCopper {
    pub shape: Shape2D,
    pub layers: PadLayers,
}

/// The physical copper + drill geometry of a pad, attached to a [`PinDef`], in
/// component-local coordinates. `copper` is a union of regions (a simple pad has
/// one; a compound pad has several); `drill` is the optional hole. Unlike the old
/// render-only `Pad`, this is the *real* geometry — render uses it now, and DRC /
/// the router consume it once migrated (it is the honest copper extent, no longer a
/// point). World coordinates come from the component's position + orientation
/// (an [`Orient`](crate::doc::Orient) quaternion), applied with [`Shape2D::map_points`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadGeo {
    pub copper: Vec<PadCopper>,
    pub drill: Option<Drill>,
}

/// A footprint **graphic** element — silkscreen or courtyard outline lifted from a
/// `.kicad_mod`, in **component-local** coordinates (the same frame as [`PinDef::offset`]
/// / [`PadCopper`]). The stroke width is realised the way all copper/text geometry in
/// this crate is: baked into the [`Shape2D`]'s Minkowski inflation radius (an `fp_line`
/// of width `w` is a `radius = w/2` capsule, exactly as [`Shape2D::trace`] lowers board
/// text), so there is no separate width field — the shape is the single source of truth.
///
/// `layer` is a **side-relative** slab name held with the footprint's authored spelling
/// (a top-authored footprint's silk is `F.SilkS` = "silk on *my* side"). A bottom-side
/// component swaps the leading `F.`↔`B.` at lowering ([`swap_side`]) — the same side
/// derivation [`PinDef::pad_features`] applies to pad copper via `is_bottom`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FpGraphic {
    pub shape: Shape2D,
    pub layer: String,
}

/// What a footprint text **anchor** resolves to at lowering time (Decision 14). An
/// anchor is never a frozen string: `Reference`/`Label` are re-derived from the
/// component's live state (refdes annotation query / label query) every time features
/// are lowered, so a refdes renumber or a params edit re-renders the silk. Only
/// [`FpTextKind::Literal`] carries its own text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FpTextKind {
    /// The reference designator — [`annotate::refdes`](crate::annotate::refdes) output
    /// (KiCad `fp_text reference`, its `"REF**"` placeholder discarded on import).
    Reference,
    /// The rendered display label — [`annotate::label`](crate::annotate::label) output
    /// (KiCad `fp_text value`; our vocabulary does not inherit KiCad's identity/display
    /// conflation, so `value` is a display label, not identity).
    Label,
    /// A fixed string (KiCad `fp_text user`).
    Literal(String),
}

/// A footprint **text anchor** (Decision 14): position, height, layer, and orientation
/// for a piece of footprint text, plus the [`FpTextKind`] that says what string it
/// renders. In **component-local** coordinates (the same frame as [`FpGraphic`] /
/// [`PinDef::offset`]); `layer` is a **side-relative** slab name (swapped `F.`↔`B.` for
/// a bottom-side placement, exactly like graphics). Lowered by [`text_features`], which
/// generates strokes locally (anchor `orient` about `at`, KiCad-style centre-anchored)
/// then maps them through the same `to_world` as graphics, so bottom-side mirroring
/// falls out of the component quaternion. `hide` anchors carry through as data (they
/// round-trip) but produce no features. The pen width is the `height / 8` rule (KiCad's
/// explicit stroke thickness is not stored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FpText {
    pub kind: FpTextKind,
    pub at: Point,
    pub height: Nm,
    pub layer: String,
    pub orient: Orient,
    pub hide: bool,
}

/// Signal/pin electrical direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Bidir,
}

/// Electrical role of a pin, used by ERC (which is just a typecheck over roles).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PinRole {
    PowerIn,
    PowerOut,
    Output,
    Input,
    Bidir,
    Passive,
}

impl PinRole {
    /// Does this role actively drive a net?
    pub fn is_driver(self) -> bool {
        matches!(self, PinRole::PowerOut | PinRole::Output)
    }
    fn from_dir(d: Dir) -> PinRole {
        match d {
            Out => PinRole::Output,
            In => PinRole::Input,
            Bidir => PinRole::Bidir,
        }
    }
}

/// A discrete pin on a part.
///
/// `name` vs `number`: the **functional name** (`GPIO0`, `VDD`, `SWCLK`) is the
/// human/agent-facing *selector* humans reference; the pad **number** (`12`, `MP`)
/// is the geometry/manufacturing key, the join key pairing a symbol pin with a
/// footprint pad, **and the stable identity stored in a [`PinRef`]**. Names repeat
/// (six pads named `IOVDD`); numbers are unique within a part, so identity keys on
/// the number. A name fans out to its pads via
/// [`resolve_selector`](PartDef::resolve_selector); `pin_role`/`pin_offset` resolve
/// the resulting *number*. For parts with no functional naming (a raw footprint
/// import, or the toy `part_library`) the two coincide — `number` defaults to
/// `name` via the [`pin`] constructor.
///
/// [`PinRef`]: crate::doc::PinRef
#[derive(Clone, Debug)]
pub struct PinDef {
    pub name: String,
    /// Pad/manufacturing number used as the symbol↔footprint join key. Defaults to
    /// `name` when there is no distinct numbering.
    pub number: String,
    pub role: PinRole,
    /// Local position of the pin relative to the component origin, in nm. Combined
    /// with the component's position + orientation to get a world position.
    pub offset: Point,
    /// Optional real copper + drill geometry ([`PadGeo`]). `Some` for an imported
    /// footprint pad; `None` for the toy `part_library` pins, which carry no
    /// footprint. This is the honest copper extent (render uses it; DRC/router
    /// consume it once migrated) — no longer a render-only simplification.
    pub pad: Option<PadGeo>,
}

/// A typed interface (e.g. UART). Defined once; encodes the correct mating so
/// that connecting two instances can never be wired backwards.
#[derive(Clone, Debug)]
pub struct InterfaceDef {
    pub type_name: String,
    /// signal name -> direction
    pub signals: BTreeMap<String, Dir>,
    /// signal name -> local position relative to the component origin, in nm.
    /// Carried alongside `signals` so an interface port's pins have geometry just
    /// like discrete pins do.
    pub offsets: BTreeMap<String, Point>,
    /// how to mate two instances: (signal on side A, signal on side B).
    /// For UART: (tx,rx) and (rx,tx) — the crossing is baked in.
    pub mate: Vec<(String, String)>,
    /// Optional binding of each signal to the **pad number** it physically is, for an
    /// interface layered onto a part with real pads (an imported part — see
    /// [`iface_infer`](crate::kicad::iface_infer)). **Empty for the toy library**, whose
    /// interface signals have no underlying [`PinDef`].
    ///
    /// This is what unifies pin identity: a bound signal *is* its pad, so
    /// [`connect_interface`](crate::elaborate) nets it under the pad-number
    /// [`PinRef`](crate::doc::PinRef) — the same identity the discrete pin and the
    /// floating-pad check use — instead of a distinct `port.signal` identity. Without
    /// it a pad wired only through its interface would look floating, and discrete +
    /// interface wiring of the same pad would land on two disconnected net nodes (a
    /// silent short). An abstract interface (`pads` empty) keeps `port.signal` identity,
    /// which is correct precisely because it has no colliding pad.
    pub pads: BTreeMap<String, String>,
}

/// A part definition: discrete pins + named interface ports.
#[derive(Clone, Debug)]
pub struct PartDef {
    pub name: String,
    pub pins: Vec<PinDef>,
    pub interfaces: BTreeMap<String, InterfaceDef>,
    /// Footprint graphics ([`FpGraphic`]) — silkscreen and fab outlines — lowered to
    /// features by [`graphic_features`], each taking its [`Role`](geom::Role) from the
    /// resolved slab (silk → [`Role::Marking`](geom::Role); an authored fab slab →
    /// [`Role::Datum`](geom::Role)). Empty for the toy `part_library` and symbol-only
    /// parts.
    pub graphics: Vec<FpGraphic>,
    /// Footprint **text anchors** ([`FpText`]) — reference/label/literal text — lowered
    /// to features by [`text_features`] (Decision 14). Like `graphics`, these are
    /// import-only data with no native-grammar serialization; empty for the toy
    /// `part_library` and symbol-only parts.
    pub texts: Vec<FpText>,
    /// An imported **courtyard** outline in component-local coordinates, if the
    /// footprint declared one (a `F.CrtYd`/`B.CrtYd` polygon). Per Decision 10 an
    /// imported courtyard IS the authoritative keep-out, so [`courtyard_shape`] and
    /// [`courtyard_half_extents`] prefer it over the derived pad-hull. `None` ⇒ derive
    /// from pad copper as before.
    pub courtyard: Option<Shape2D>,
    /// Manual **class** override (Decision 14) — when `Some`, the annotation query uses
    /// it verbatim instead of deriving the class from the part name (`R_0402` → `R`).
    /// `None` for every imported part (the KiCad importer does not populate it) and for
    /// the toy library; authored only where the name heuristic would guess wrong.
    pub class: Option<String>,
}

impl PartDef {
    /// Resolve the electrical role of a *stored pin identity* (see [`PinRef`]):
    /// a pad **number** for a discrete pin, or `port.signal` for an interface
    /// signal. Pad numbers are unique within a part, so this is unambiguous —
    /// unlike functional names, which repeat (six `IOVDD` pads share a name but
    /// have distinct numbers). Use [`resolve_selector`](Self::resolve_selector) to
    /// turn a user-facing name into the identities this resolves.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn pin_role(&self, id: &str) -> Option<PinRole> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.signals.get(sig).copied().map(PinRole::from_dir)
        } else {
            self.pins.iter().find(|p| p.number == id).map(|p| p.role)
        }
    }

    /// Resolve a *stored pin identity* to its local offset from the component
    /// origin. Identity semantics match [`pin_role`](Self::pin_role): a pad number
    /// for a discrete pin, or `port.signal` for an interface signal.
    pub fn pin_offset(&self, id: &str) -> Option<Point> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.offsets.get(sig).copied()
        } else {
            self.pins.iter().find(|p| p.number == id).map(|p| p.offset)
        }
    }

    /// Resolve a *connection selector* (a user/agent-facing pin reference) to the
    /// set of stable pin identities it names — the pad **numbers** to store as
    /// [`PinRef`]s. This is the one place a functional name fans out to physical
    /// pads, which is what keeps a multi-pad power rail (six `IOVDD`) from
    /// collapsing to a single member.
    ///
    /// Resolution order:
    /// - `port.signal` (contains `.`) → an interface signal: returns that single
    ///   identity if the port and signal exist, else empty.
    /// - otherwise match by functional **name** first (so `IOVDD` → every IOVDD
    ///   pad's number); if no name matches, fall back to matching a pad **number**
    ///   directly (so `30` / `MP` selects that one pad).
    ///
    /// An **empty** result means the selector names nothing on this part — a typo
    /// or a role gap. Callers must treat that as an error, never a silent no-op.
    /// The fanout is scoped to this one part: a name never reaches across instances.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn resolve_selector(&self, sel: &str) -> Vec<String> {
        if let Some((port, sig)) = sel.split_once('.') {
            return match self.interfaces.get(port) {
                Some(iface) if iface.signals.contains_key(sig) => vec![sel.to_string()],
                _ => Vec::new(),
            };
        }
        let by_name: Vec<String> = self
            .pins
            .iter()
            .filter(|p| p.name == sel)
            .map(|p| p.number.clone())
            .collect();
        if !by_name.is_empty() {
            return by_name;
        }
        // Fall back to a direct pad-number reference.
        self.pins
            .iter()
            .filter(|p| p.number == sel)
            .map(|p| p.number.clone())
            .collect()
    }
}

pub type PartLib = BTreeMap<String, PartDef>;

#[cfg(test)]
mod tests;
