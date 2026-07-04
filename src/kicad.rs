//! Import KiCad footprints (`.kicad_mod`) into the part model.
//!
//! A `.kicad_mod` file is a single S-expression. We hand-roll a tiny tokenizer +
//! recursive reader (zero dependencies — no serde/sexp crates) and lift the parts
//! we care about into a [`PartDef`](crate::part::PartDef).
//!
//! ## Module layout
//! This is a facade over four submodules; every historical `crate::kicad::…` path
//! still resolves through the `pub use` re-exports below:
//! - [`sexp`] — the shared S-expression layer: tokenizer, reader, and the
//!   fixed-point `mm→nm` converter (no dependency on `part`/`geom`).
//! - [`footprint`] — [`import_footprint`]/[`import_footprint_file`] and the
//!   pad/graphic/text readers (it also owns the `gr_*` primitive readers the
//!   board-outline importer reuses).
//! - [`outline`] — [`import_board_outline`]/[`import_board_outline_file`]:
//!   `Edge.Cuts` stitching + ring classification.
//! - [`symbol`] — the symbol/role layer: [`Symbol`], [`ElecType`],
//!   [`import_symbol`], [`join_symbol_footprint`], [`import_part`],
//!   [`apply_role_map`].
//! - [`iface_infer`] — conservative interface inference (issue 0010), a kicad-only
//!   consumer moved under this module.
//!
//! ## What a footprint *is* (and is not)
//! A footprint is **geometry**: copper pads at positions, silkscreen, courtyard,
//! 3D models. It carries **no electrical roles** — whether a pad is power, an
//! input, or passive comes from the *schematic symbol*, not the footprint. So
//! every imported pin gets [`PinRole::Passive`](crate::part::PinRole::Passive);
//! roles must be supplied elsewhere when a footprint is paired with a symbol. A
//! footprint alone defines no typed [`InterfaceDef`](crate::part::InterfaceDef)s.
//! Once a symbol supplies functional pin names, [`join_symbol_footprint`] runs a
//! conservative interface-inference pass
//! ([`iface_infer::infer_interfaces`](crate::kicad::iface_infer::infer_interfaces),
//! issue 0010) over the joined part, so `PartDef.interfaces` gains a typed port only
//! where the pin names form a complete, unambiguous registry match (empty otherwise).
//!
//! What we *do* import is the pad-to-pin geometry: one [`PinDef`](crate::part::PinDef)
//! per pad, named by the pad's number/name, positioned at the pad's `(at x y)`
//! converted mm→nm — plus the footprint's non-copper **graphics** (issue 0016):
//! - `fp_line`/`fp_arc`/`fp_circle`/`fp_poly`/`fp_rect` on `F.SilkS`/`B.SilkS` and
//!   `F.Fab`/`B.Fab` → [`PartDef::graphics`](crate::part::PartDef). Their
//!   [`Role`](crate::geom::Role) is taken from the resolved slab by
//!   [`part::graphic_features`](crate::part::graphic_features): silk slabs are
//!   [`Role::Marking`](crate::geom::Role); a fab slab is [`Role::Datum`](crate::geom::Role)
//!   (Decision 15). Because `graphic_features` skips a slab absent from the stackup, fab
//!   graphics materialize into features **only** if the user authors an `F.Fab`/`B.Fab`
//!   slab — the default stackup has none.
//! - A courtyard polygon (`fp_poly`/`fp_rect` on `F.CrtYd`/`B.CrtYd`) →
//!   [`PartDef::courtyard`](crate::part::PartDef), the authoritative courtyard
//!   (Decision 10). Loose `fp_line`/`fp_arc` courtyard *segments* are not yet stitched
//!   into a loop.
//! - **Footprint text** (`fp_text reference|value|user`, and the v7
//!   `property "Reference"|"Value"` form) → [`PartDef::texts`](crate::part::PartDef) as
//!   [`FpText`](crate::part::FpText) anchors (Decision 14):
//!   `reference`→[`FpTextKind::Reference`](crate::part::FpTextKind),
//!   `value`→[`FpTextKind::Label`](crate::part::FpTextKind) (both discard their
//!   placeholder string — the anchor re-derives it live at lowering),
//!   `user`→[`FpTextKind::Literal`](crate::part::FpTextKind) (except a whole-string
//!   `${REFERENCE}`/`${VALUE}` KiCad text variable, which resolves to the live
//!   Reference/Label anchor). Height is the font-size *height* component; the stroke
//!   thickness is ignored (the pen is the `height / 8` rule); `hide` is lifted (a hidden
//!   anchor round-trips as data but produces no features). Lowered by
//!   [`part::text_features`](crate::part::text_features).
//!
//! Still **skipped**: paste (`F.Paste`/`B.Paste`) — paste is *derived* at export from
//! pad geometry, never authored (Decision 15).
//! Layer references are **side-relative**: a footprint is authored top-side, so its
//! `F.*` graphics swap to `B.*` when the component is placed bottom-side (see
//! [`part::swap_side`](crate::part::swap_side)).
//!
//! ## Mapping decisions (documented contract)
//! - **Shared pad ids** (e.g. two `MP` mounting pads, or a split thermal pad that
//!   reuses one number): we keep the **first** occurrence and drop later pads with
//!   an already-seen id. They are the same electrical pad — pad id (the pad number)
//!   is the stable identity a `PinRef` keys on, so it must stay unique within a
//!   part. (Distinct pads that share a *functional name* after a symbol join — six
//!   `IOVDD` — are all kept; names may collide, ids may not.)
//! - **Unnamed pads** (`name == ""`, used for thermal/exposed pads and mechanical
//!   features): **skipped**. An empty name carries no electrical identity, and a
//!   footprint's roles come from the symbol anyway.
//! - The pad rotation in `(at x y angle)` is **ignored** for the offset (we import
//!   the pad *position* only).
//!
//! Both the modern `(footprint "name" ...)` and the legacy `(module name ...)`
//! headers are accepted; pad names may be quoted or bare.

mod sexp;

pub mod footprint;
pub mod iface_infer;
pub mod outline;
pub mod symbol;

pub use footprint::{import_footprint, import_footprint_file};
pub use outline::{import_board_outline, import_board_outline_file};
pub use symbol::{
    ElecType, JoinReport, Symbol, SymbolPin, apply_role_map, import_part, import_symbol,
    import_symbol_named, join_symbol_footprint,
};

// Made visible to the test module's `use super::*;` (these were module-scope `use`s in
// the pre-split single-file kicad.rs). Private — not part of the public kicad API.
#[cfg(test)]
use crate::{
    doc::{Orient, Point},
    geom::Shape2D,
    part::{Drill, FpTextKind, PadLayers, PartDef, PinRole},
};

#[cfg(test)]
mod tests;
