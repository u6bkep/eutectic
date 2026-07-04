//! Shim — the boolean/offset kernel moved into the geometry subsystem; see
//! [`geom::kernel`](crate::geom::kernel). This re-export keeps every existing
//! `crate::region::` consumer path (route, part, elaborate, export, ttf, autoroute)
//! resolving unchanged; a later wave normalizes those paths and deletes the shim.
pub use crate::geom::kernel::*;
