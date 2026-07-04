//! Geometric coordinate primitives: the fixed-point unit `Nm` and the planar
//! `Point`. A leaf module with no crate dependencies, so the geometry kernels
//! (`geom`, `region`) can build on it without pulling in `doc`.

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
