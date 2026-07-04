//! Deterministic output artifacts: netlist, pick-and-place, and an SVG sketch.
//!
//! Each exporter is a *pure function* of its inputs (a `Doc`, plus the `PartLib`
//! for geometry) — no wall-clock, no randomness, no iteration over `HashMap`. The
//! model is built on `BTreeMap`/`BTreeSet` precisely so this output is byte-stable
//! and diffable: calling an exporter twice on the same inputs yields identical
//! strings, and a one-thing change produces a one-line diff.
//!
//! Artifacts: the connectivity ([`netlist`]), placement ([`placement_csv`]) and sketch
//! ([`svg`]) exporters, plus **fab output** — RS-274X Gerber per copper layer + an
//! `Edge.Cuts` outline ([`gerber_layer`] / [`gerber_edge_cuts`] / [`gerber_set`])
//! and an Excellon drill program ([`excellon_drill`]) — and a **fab-drawing SVG** pass
//! ([`svg_fab`] / [`fab_svg_set`], Decision 15: one SVG per authored `Role::Datum` slab,
//! the consumer that lets an authored fab slab actually render). Now that routing writes real
//! copper into the `Doc` (traces with width, vias with pad+drill) and footprint pads
//! carry render geometry, the fab artifacts describe genuine copper. All coordinates
//! flow from integer nanometres into each format by integer arithmetic (the Gerber
//! `%FSLAX46Y46*%` fixed-point format *is* nanometres — see `gbr_coord`), so the
//! determinism invariant holds end to end. See docs/architecture.md, "Prototype
//! status (Gerber/fab output)".
//!
//! ## Module layout
//!
//! This module is a thin facade over per-backend submodules; every public and
//! cross-crate item is re-exported here so callers keep resolving it at
//! `crate::export::…`:
//!
//! - [`svg_writer`] — shared SVG emission primitives (`fmt_mm`, `xml_escape`, the
//!   curve-aware path builders). `fmt_mm`/`xml_escape` are also used by the schematic
//!   renderer (`crate::schematic_svg`), which imports them at `crate::export::`.
//! - [`svg`] — the board SVG sketch and the per-side fab-drawing SVG.
//! - [`gerber`] — the RS-274X Gerber backend (copper, mask, silk, fab, edge cuts).
//! - [`excellon`] — the Excellon drill backend.
//! - [`netlist`] — the human-readable connectivity artifact (the shared `doc_netlist`
//!   membership map now lives beside the derivations it feeds, in [`crate::route`]).
//! - [`placement`] — the pick-and-place CSV and `part_pin_ids`.
//! - [`features`] — cross-backend derived-geometry queries (`role_features`, `pours_of`).

mod excellon;
mod features;
mod gerber;
mod netlist;
mod placement;
mod svg;
mod svg_writer;

// Re-exports keep every previously-`crate::export::…` item resolving at that path.

// The public exporter API.
pub use excellon::excellon_drill;
pub use gerber::{
    gerber_edge_cuts, gerber_fab, gerber_layer, gerber_mask, gerber_set, gerber_silk,
};
pub use netlist::netlist;
pub use placement::placement_csv;
pub use svg::{fab_svg_set, svg, svg_fab};

// `fmt_mm`/`xml_escape` are imported by `crate::schematic_svg` at `crate::export::`.
pub(crate) use svg_writer::{fmt_mm, xml_escape};

// The unit tests (`mod tests`, `use super::*`) reach the submodules' internals and the
// model types through the export root, exactly as when everything lived in this file.
// These imports exist only to keep that `super::*` resolving; no non-test code consumes
// them at this path, so they are gated to the test build to avoid dead imports in the
// library.
#[cfg(test)]
use crate::doc::{MM, Nm, Point};
#[cfg(test)]
use crate::geom::{Role, Seg, Shape2D, Slab, Stackup, ZRange};
#[cfg(test)]
use crate::part::{PartDef, PartLib};
#[cfg(test)]
use crate::route::Layer;
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
pub(crate) use {
    excellon::{DrillKind, excellon_files, excellon_program},
    gerber::{arc_ij_turn, gerber_contour},
    placement::part_pin_ids,
    svg_writer::{svg_arc_params, svg_path_d},
};

#[cfg(test)]
mod tests;
