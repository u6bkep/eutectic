//! The leaf data model of routed copper: [`Layer`], [`Trace`], [`Via`], and the
//! [`DesignRules`] the DRC query reads. Widely consumed outside `route`, so these
//! are re-exported from the crate's `route` facade.

use crate::doc::{MM, Nm, Point};
use crate::geom::ZRange;
use crate::id::NetId;
use std::cmp::Ordering;

/// A copper layer *ordinal* — a router-internal working form, not stored identity.
/// `Top`/`Bottom` are the outer copper; `Inner(n)` keeps the model trivially
/// extensible to multilayer boards (n = 0-based inner-layer index). The ordering is
/// the physical stack-up top→bottom, which is what via spans test.
///
/// Per Decision 13 rule 2 / Decision 18, layer *identity* is a slab **name**
/// ([`Trace::layer`], [`Via::span`]); this ordinal survives only inside the
/// autorouter's grid and the DRC/export forward-query at their own boundaries
/// (`slab_layer` ↔ `copper_layers_z`), never as persisted state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Top,
    Inner(u8),
    Bottom,
}

impl Layer {
    /// Position in the physical stack-up (top = 0, descending). Used to test
    /// whether a layer falls within a via's spanned range. `Bottom` sits below any
    /// representable inner layer.
    pub fn depth(self) -> i32 {
        match self {
            Layer::Top => 0,
            Layer::Inner(n) => 1 + n as i32,
            Layer::Bottom => 1 + 256,
        }
    }
}

// Order layers by physical depth so a `Violation` set sorts canonically.
impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.depth().cmp(&other.depth())
    }
}

/// A routed copper polyline on one copper slab, belonging to one net. `layer` is the
/// slab **name** (Decision 13 rule 2 — identity is the name, never a positional
/// ordinal); `width` is the finished copper width (nm); `prov` is `Pinned` for
/// hand/agent routing and `Free` for a future autorouter's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub net: NetId,
    pub layer: String,
    /// Polyline centreline. Two or more points; consecutive points are segments.
    pub path: Vec<Point>,
    pub width: Nm,
    pub prov: crate::doc::Provenance,
}

/// A via: a plated point connecting copper across the copper slabs it spans. Modelled
/// by its centre `at`, a `drill`, a `pad` (annular copper diameter) — a disc of that
/// diameter on every copper slab it spans — and a `span`.
///
/// `span` is `None` for the common **through** via (the full copper extent, top-most
/// to bottom-most copper slab — Decision 18's full-span default), or
/// `Some((from, to))` naming the two copper slabs a blind/buried via terminates on
/// (order-insensitive; the barrel spans every copper slab between them inclusive). The
/// two-name form is parseable and stored even though multilayer stackups are rare
/// today, so blind/buried vias round-trip when they arrive. Names, not ordinals
/// (Decision 13 rule 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Via {
    pub net: NetId,
    pub at: Point,
    pub span: Option<(String, String)>,
    pub drill: Nm,
    pub pad: Nm,
    pub prov: crate::doc::Provenance,
}

impl Via {
    /// Does this via connect copper on the slab with z-range `z`, given the stackup's
    /// copper slabs top-down (`cu`, as [`Stackup::copper_slabs`](crate::geom::Stackup::copper_slabs)
    /// orders them)? A `None` span is the full copper extent (every copper slab); a
    /// `Some((from, to))` span is every copper slab whose depth lies between `from` and
    /// `to` inclusive. An unresolvable named endpoint spans nothing (a committed via
    /// always resolves — the commit-time slab gate).
    pub fn spans_z(&self, cu: &[&crate::geom::Slab], z: &ZRange) -> bool {
        let Some(idx) = cu.iter().position(|s| s.z == *z) else {
            return false;
        };
        match &self.span {
            None => true,
            Some((from, to)) => {
                let (Some(a), Some(b)) = (
                    cu.iter().position(|s| s.name == *from),
                    cu.iter().position(|s| s.name == *to),
                ) else {
                    return false;
                };
                a.min(b) <= idx && idx <= a.max(b)
            }
        }
    }

    /// The copper slabs (top-down, from `cu`) this via's barrel connects — used by the
    /// connectivity check to know which trace/pour layers a via bridges.
    pub fn spanned_slabs<'a>(&self, cu: &'a [&'a crate::geom::Slab]) -> Vec<&'a crate::geom::Slab> {
        cu.iter()
            .filter(|s| self.spans_z(cu, &s.z))
            .copied()
            .collect()
    }
}

/// Manufacturing design rules consumed by DRC. Defaults are a generic 2-layer
/// process: 0.15 mm clearance and 0.15 mm minimum trace width. In production these
/// would be read from the board/source; the prototype carries them as a constant
/// (the DRC query uses `DesignRules::default()`), documented as the one knob to
/// wire to the source when a stack-up/process definition exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesignRules {
    /// Minimum copper-to-copper gap between *different* nets (edge to edge).
    pub min_clearance: Nm,
    /// Minimum finished trace width.
    pub min_trace_width: Nm,
    /// Tolerance for geometric incidence ("touching") in the connectivity/ratsnest
    /// check. Hand-placed coordinates are exact integers, so coincident endpoints
    /// have distance 0; this small slop absorbs deliberate near-misses without
    /// fusing genuinely separate copper.
    pub touch_tol: Nm,
    /// Minimum gap from copper to a copper/route **keep-out** region's edge (issue
    /// 0023). Default `0`: copper that *overlaps* the keep-out violates, but copper
    /// merely touching its edge (an exact zero-gap tangency) does not — the underlying
    /// clearance test is strict `<`. Set a positive value for a real pull-back margin.
    pub keepout_clearance: Nm,
    /// Minimum gap from copper to the **board edge** — the substrate boundary and the
    /// walls of any cutout (issue 0023). Copper closer than this, or spilling outside
    /// the board, violates. Applies to traces/vias/pads; a copper pour is expected to
    /// be authored inset (its relationship to the edge is a fill-pullback concern), so
    /// pours are exempt from this check — see [`check_drc`](super::check_drc).
    pub edge_clearance: Nm,
}

impl Default for DesignRules {
    fn default() -> Self {
        DesignRules {
            min_clearance: 150_000,   // 0.15 mm
            min_trace_width: 150_000, // 0.15 mm
            touch_tol: MM / 100,      // 0.01 mm
            keepout_clearance: 0,     // a keep-out edge is a hard boundary
            edge_clearance: 200_000,  // 0.2 mm from the board edge
        }
    }
}
