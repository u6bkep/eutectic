//! Stable identity.
//!
//! For M1 an entity's stable id *is* its hierarchical path string (e.g.
//! `psu.dec[2]`). This is the deterministic, human-readable choice that makes the
//! override-survives-re-elaboration demo crisp: elaborating the same source
//! reproduces the same paths, so an override keyed by id stays attached.
//!
//! Production would intern these as opaque handles behind a path<->id table so
//! that ids are independent of any display name; the path is then just one
//! attribute. The architectural invariant we exercise here is the one that
//! matters: identity is stable across edits and independent of position/order.

use std::fmt;

/// Stable entity identity (a hierarchical path for M1).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub String);

impl EntityId {
    pub fn new(s: impl Into<String>) -> Self {
        EntityId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Net identity. Distinct type from EntityId to keep the net namespace separate.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetId(pub String);

impl NetId {
    pub fn new(s: impl Into<String>) -> Self {
        NetId(s.into())
    }
}

impl fmt::Debug for NetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for NetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable identity for a routed trace. A plain monotone integer rather than a
/// path string: a trace has no natural hierarchical name, and both a hand edit
/// and a future autorouter mint ids the same way (caller-assigned, like KiCad's
/// per-object UUIDs). Distinct newtype so the routing namespace stays separate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceId(pub u64);

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// Stable identity for a via. Mirrors [`TraceId`]; separate type so a trace id
/// and a via id can never be confused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViaId(pub u64);

impl fmt::Debug for ViaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl fmt::Display for ViaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}
