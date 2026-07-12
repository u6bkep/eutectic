//! Purposed regions: the physical-geometry foundation (see docs/architecture.md §8).
//!
//! Everything physical — copper, the board body, holes, keep-outs — is a
//! [`Feature`]: a `(role, material?, extent)`. This module is the **2.5D core**:
//! the shape vocabulary, the z-stackup, and an exact-integer clearance kernel. As of
//! the geometry-model convergence (docs/architecture.md §8 is the current model;
//! docs/log/n02-convergence-plan.md records the Phases 0–2 execution) this **is** the
//! live clearance model: DRC, pours, Gerber, and the autorouter all
//! reduce copper to [`Feature`]s and gate on [`Feature::clears`]; the former
//! `route::Layer`-based copper-piece model has been retired. `route::Layer` survives
//! only as the routing/trace/via tier and the violation-report granularity.
//!
//! ## One shape: a skeleton inflated by a radius
//!
//! [`Shape2D`] is a skeleton (a polyline, or a filled polygon) **⊕ a radius** — the
//! Minkowski sum with a disc. This single type subsumes every pad primitive *and*
//! traces *and* via annuli:
//!   - point ⊕ r  = a round pad / via
//!   - segment ⊕ r = an oval/pill pad
//!   - open polyline ⊕ (width/2) = a trace
//!   - rectangle polygon ⊕ r = a rounded rect (r = 0 ⇒ sharp); arbitrary polygon = a
//!     trapezoid / custom pad; a *union* of shapes = a compound pad (e.g. BMP581).
//!
//! Clearance is then uniform and exact: the edge-to-edge gap is
//! `skeleton_distance(a, b) − rₐ − r_b`, and a violation is that gap `< min_clearance`.
//! All distance math is `i128` squared-distance comparison — no float, deterministic.
//!
//! ## z is real; a "layer" is a named z-slab
//!
//! An [`Extent::Prism`] carries a [`ZRange`]. Two features can clash only if their
//! z-ranges overlap; with the discrete slabs of a [`Stackup`] that collapses to
//! "same layer", recovering ordinary 2.5D behaviour — but nothing is *limited* to
//! discrete layers, so below-surface bodies (negative/arbitrary z) are expressible,
//! and `Extent::Solid` is reserved for true 3D. Net-aware *policy* (which feature

pub mod feature;
pub mod kernel;
pub mod limits;
mod seg;
pub mod shape;

#[cfg(test)]
mod geom_tests;

// Re-export the whole subsystem at the facade so every existing `crate::geom::` path
// keeps resolving after the split (shape vocabulary, feature model, ceilings/limits).
// The boolean/offset `kernel` keeps its own `crate::geom::kernel::` namespace.
pub use feature::*;
pub use limits::*;
pub use shape::*;
