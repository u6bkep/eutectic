//! Routing: the trace/via/layer representation (tier-2 materialized state) and the
//! geometry + connectivity kernel the DRC query (tier-3) runs on.
//!
//! Per docs/architecture.md, routed copper is **tier-2 materialized state** that
//! lives in the `Doc` alongside component placement, each carrying a `Provenance`
//! bit: a hand-routed trace is `Pinned` (user-authored, treated by a future
//! autorouter as a fixed obstacle), a `Free` trace is solver/auto-driven and
//! regen-able. One provenance ladder governs placement and routing alike — there
//! is no separate "auto" subsystem.
//!
//! The DRC checks themselves are tier-3 (pure, deterministic, cheaply comparable):
//! [`check_drc`] is the reusable query body, called from the incremental engine in
//! `query.rs` (mirroring how ERC is computed there). All geometry is integer
//! nanometres; distance comparisons are done in exact `i128` arithmetic against
//! *squared* thresholds, so no float nondeterminism leaks into a violation set.
//!
//! ## Module layout
//!
//! This file is a thin facade over the submodules; the sections above describe the
//! subsystem as a whole. The pieces:
//! - [`model`] — the leaf data model: [`Layer`], [`Trace`], [`Via`], [`DesignRules`].
//! - [`world`] — the world-frame derivation producer cluster ([`world_features`],
//!   [`net_features`], the layer/slab ordinal bridges, [`Pour`]/[`pours`]).
//! - [`drc`] — the [`Violation`] type and the [`check_drc`] query body.
//! - [`connect`] — the union-find connectivity check plus the private i128 segment
//!   kernel (a later wave dedups that kernel against `autoroute`).

mod connect;
mod drc;
mod model;
mod world;

// Leaf data model — widely consumed across the crate.
pub use model::{DesignRules, Layer, Trace, Via};

// DRC query surface.
pub use drc::{Violation, check_drc};

// World-frame producer surface. `world_features`, `Pour`, and `pours` are public; the
// autorouter consumes the ordinal↔slab bridges via `crate::route::`.
pub use world::{Pour, pours, world_features};
pub(crate) use world::{copper_layers_z, layer_slab_name};

// `net_features` and `slab_layer` are consumed only by this module's own test suite
// (`pour_tests`, via `use super::*`); no non-test crate consumer reaches them through
// the facade. Their `pub(crate) fn` definitions in `world` stay put; only the facade
// re-export is test-scoped so it does not read as dead in a normal build.
#[cfg(test)]
use world::{net_features, slab_layer};

// Test-only imports for this module's own children: the external types the former
// monolithic `route.rs` imported at file scope and forwarded to `pour_tests` through
// its `use super::*`. Not part of the crate's external API.
#[cfg(test)]
use {
    crate::doc::{Doc, Nm, PinRef},
    crate::elaborate::stackup,
    crate::geom::Extent,
    crate::id::{NetId, TraceId},
    crate::part::{PartLib, PinRole},
    std::collections::BTreeMap,
};

#[cfg(test)]
mod pour_tests;
