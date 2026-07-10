//! The schematic layout tree (Decision 20) — authored structure, derived coordinates.
//!
//! Decision 20 opens the schematic front as *the second derived projection of the
//! generative truth* (the flat netlist is the first). Two things live here, on the two
//! sides of the tier line the whole architecture turns on (docs/architecture.md, §20a):
//!
//!   - **Authored (tier 1):** [`SchematicLayout`] — a tiny nested-container tree
//!     (`row`/`column` with symbols as leaves), a deliberately small CSS-flexbox subset
//!     (§20b). It parses from the `schematic { … }` block grammar in [`crate::text`],
//!     elaborates with real diagnostics ([`validate`]: `E_SCHEMATIC` unknown/duplicate
//!     comp paths and duplicate sibling names, plus a `W_SCHEMATIC_UNPLACED` warning for
//!     any component not in the tree), and round-trips byte-identically.
//!
//!   - **Derived (tier 3):** the *coordinates*, produced by [`reflow`] — a pure,
//!     deterministic, terminating flow of the tree into per-component positions in a
//!     schematic coordinate space independent of the board. It is elaboration-class, not
//!     routing (§20a): no solver, milliseconds, byte-identical every run. Coordinates are
//!     **never serialized** (§20a: re-derivable state is not emitted) — [`reflow`] is an
//!     on-demand function, the same shape as [`crate::elaborate::regions`]/`stackup`
//!     (pure over the authored state), *not* a memoized [`crate::query`] key: the query
//!     engine memoizes on the coarse `conn/geom/route` input revisions, and the layout
//!     tree is not one of those inputs, so a memo keyed on them would go stale on a
//!     tree-only edit. A pure recompute is correct and cheap.
//!
//! The view is **total** (§20c): [`reflow`] always returns a coordinate for *every*
//! component — anything absent from the tree lands in a derived "unplaced bin" (a plain
//! grid), so the schematic never silently omits a part.
//!
//! On top of the coordinates sits the **realized-geometry tier** (Decision 23):
//! [`schematic_features`] emits every primitive the drawing consists of — typed shapes +
//! text runs with semantic provenance and style classes, plus the content bounds — so the
//! SVG renderer, the GUI projection/pick, and the owned renderer to come are all pure
//! consumers of one stream (see [`features`] for the contract, and [`symbol_body`] for
//! the symbol-artwork seam).
//!
//! This module is a facade over five private submodules — [`model`] (the authored
//! tree), [`symbol`] (box-with-pins sizing), [`validate`] (tier-1 diagnostics),
//! [`reflow`] (the derived-coordinate flexbox engine), and [`features`] (the realized
//! drawing) — all re-exported so every `crate::schematic::` consumer path keeps
//! resolving.

mod features;
mod model;
mod reflow;
mod symbol;
mod validate;

pub use features::{
    Bounds, HEADER_GAP, HEADER_TEXT_H, LABEL_PAD, MARGIN, PIN_TEXT_H, PinAnchor, Provenance,
    STUB_LEN, SYMBOL_STROKE, SchematicFeature, SchematicFeatures, Shape, StyleClass, SymbolBody,
    TAG_TEXT_H, TextJustify, TextRun, WIRE_STROKE, schematic_features, symbol_body,
};
pub use model::{Align, Container, Direction, LayoutNode, SchematicLayout, Symbol, Wire, WireEnd};
pub use reflow::{Placement, reflow};
pub use symbol::{Extent, PinSide, PinSlot, header_width, pin_slots, symbol_extent};
pub use validate::{validate, validate_wires};

#[cfg(test)]
pub(crate) use reflow::{MAX_FRAGMENT_DEPTH, MIN_EXTENT};
#[cfg(test)]
pub(crate) use symbol::{MIN_BOX_H, MIN_BOX_W, edge_pins};

#[cfg(test)]
mod tests;
