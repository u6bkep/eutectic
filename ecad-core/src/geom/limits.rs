//! Coordinate ceilings, kernel-safety predicates, and stackup thickness constants
//! for the geometry subsystem (see the [`geom`](crate::geom) module docs and
//! docs/architecture.md §8). Split out of `shape.rs` to keep the shape vocabulary
//! focused; every item here is re-exported at the [`geom`](crate::geom) facade, so
//! existing `crate::geom::` paths are unchanged.

use crate::coord::{Nm, Point};

/// Default board thickness: 1.6 mm, in nm.
pub const BOARD_THICKNESS: Nm = 1_600_000;
/// Default finished copper thickness: ~1 oz (35 µm), in nm.
pub const COPPER_THICKNESS: Nm = 35_000;
/// Default solder-mask thickness: 25 µm, in nm.
pub const MASK_THICKNESS: Nm = 25_000;
/// Default silkscreen (ink) thickness: 10 µm, in nm.
pub const SILK_THICKNESS: Nm = 10_000;
/// Solder-mask expansion: how much larger a mask opening is than the pad copper, per
/// side (the pad copper is inflated by this to get the opening). The **single source
/// of truth** for that margin — the model's mask-opening `Void`s
/// ([`crate::part::PinDef::pad_features`]), the design-rule default
/// ([`crate::route::DesignRules::default`]), and the Gerber mask path all read it, so
/// there is one value to change. A generic process figure; production reads it from
/// the stack-up/process.
pub const MASK_EXPANSION: Nm = 50_000;
/// Default arc chord tolerance for tessellation: max sagitta (arc-to-chord deviation),
/// in nm. 1 µm — finer than the 64-gon disc approximation at pad scale, coarse enough
/// to keep segment counts modest for large-radius board-outline arcs.
///
/// The flattening is **inscribed** (vertices sit *on* the arc, chords cut inside), so
/// for DRC the tessellated copper is at most one sagitta smaller than the true arc and
/// a clearance check is *optimistic* by at most that amount. At 1 µm against ≥ 100 µm
/// clearances this is < 1 %; keep it well under the fab margin. (A conservative DRC
/// would circumscribe instead — deferred; not worth the complexity at this tolerance.)
pub const DEFAULT_CHORD_TOL: Nm = 1_000;

/// The enforced ceiling on any coordinate magnitude, in nm: **1 m** (`±1e9 nm`).
///
/// The exact-integer kernel keeps its squared-distance math in `i128`, and the
/// worst chain is the perpendicular case of [`pt_seg_d2`]: `|w|²·den` where each of
/// `|w|²` and `den` is a sum of two squared coordinate *differences*. A difference of
/// two coordinates each in `[−C, C]` has magnitude ≤ `2C`, so `|w|², den ≤ 2·(2C)² =
/// 8C²` and the product is ≤ `64·C⁴`. Requiring `64·C⁴ ≤ i128::MAX ≈ 1.70e38` gives
/// `C ≤ (2^127 / 64)^(1/4) ≈ 1.28e9` nm. We round that **down** to a memorable
/// `1e9 nm = 1 m`, which leaves `64·(1e9)⁴ ≈ 6.4e37` — a ~2.7× margin under the
/// `i128` ceiling. Every other integer predicate is lower-order in `C` (the
/// [`circumcenter`]/[`region::crossings`] numerators are ~`C³`, [`orient`] ~`C²`), so
/// this quartic bound is the binding one and protects them all.
///
/// This is the crate-wide operating range — far beyond any real board (a 1 m panel).
/// It is *enforced* at every ingest boundary (text parse, KiCad/SVG import, command
/// ingress) as a hard `E_COORD_RANGE` diagnostic, and *asserted* in the hot kernel
/// predicates in debug builds; release builds trust the boundary guarantee and stay
/// unchecked. This resolves issue 0018 (the former silent-wrap-above-~1.28e9 hazard).
pub const MAX_COORD: Nm = 1_000_000_000;

/// Is a single coordinate within the enforced [`MAX_COORD`] ingest range?
pub fn coord_ok(n: Nm) -> bool {
    n.unsigned_abs() <= MAX_COORD as u64
}

/// Are both components of a point within the [`MAX_COORD`] ingest range? The
/// ingest-boundary validation predicate (text/import/command).
pub fn point_ok(p: Point) -> bool {
    coord_ok(p.x) && coord_ok(p.y)
}

/// The **true** `i128`-safe coordinate ceiling — the largest magnitude for which the
/// worst kernel product `64·C⁴` still fits in `i128` (`64·C⁴ ≤ i128::MAX` ⟹
/// `C ≤ (2^127/64)^(1/4) = 1_276_901_416`). Rounded **down** to `1_276_000_000` for a
/// small safety margin (`64·C⁴ ≈ 1.697e38 < 1.701e38`).
///
/// This is distinct from — and larger than — [`MAX_COORD`] on purpose. Ingest bounds
/// *authored/imported* coordinates at `MAX_COORD` (1 m); the kernel then *composes*
/// them (a placement offset + a footprint-local courtyard extent, an inflation by a
/// clearance), and a composed world coordinate can legitimately exceed `MAX_COORD`
/// while staying correct. The `~0.28e9` gap between the two constants is exactly that
/// composition headroom. The kernel debug_asserts fire at `KERNEL_SAFE_COORD` (the
/// real overflow risk), **not** at `MAX_COORD` — otherwise a part legally placed at the
/// 1 m ingest bound would panic a debug build the instant its courtyard is measured.
pub const KERNEL_SAFE_COORD: Nm = 1_276_000_000;

// Compile-time guards on the two ceilings (issue 0018): the kernel ceiling must sit
// above the ingest ceiling (that gap is the composition headroom), and the worst
// kernel product `64·C⁴` at the kernel ceiling must still fit in `i128`.
const _: () = assert!(KERNEL_SAFE_COORD > MAX_COORD);
const _: () = {
    let c = KERNEL_SAFE_COORD as i128;
    let c2 = c * c;
    assert!(c2.checked_mul(c2).is_some(), "C⁴ overflows i128");
    assert!((c2 * c2).checked_mul(64).is_some(), "64·C⁴ overflows i128");
};

/// Is a single coordinate within the [`KERNEL_SAFE_COORD`] i128-safe range? The
/// debug-assert predicate for the hot integer kernels (composition-frame, not ingest).
pub fn coord_kernel_safe(n: Nm) -> bool {
    n.unsigned_abs() <= KERNEL_SAFE_COORD as u64
}

/// Are both components of a point within [`KERNEL_SAFE_COORD`]? The kernel debug-assert
/// predicate.
pub fn point_kernel_safe(p: Point) -> bool {
    coord_kernel_safe(p.x) && coord_kernel_safe(p.y)
}
