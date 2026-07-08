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
///
/// The integer **serializes** into the `route` line (Decision 22): a route lives
/// in the machine-written `# routes` state zone and has no name, so a small id
/// token is its persistent identity across a serialize/parse boundary (what
/// waivers / length-tuning groups / identity diff key on). It is *not* an array
/// index — see [`RouteIdAlloc`] for how ids are minted above the current max, so
/// a deleted id leaves a permanent gap rather than renumbering its neighbours.
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

/// Monotone allocator for the trace/via id namespaces (Decision 22). Seeded strictly
/// **above** every id currently present, it hands out increasing ids so a batch mint
/// (the parser's re-mint path, the autorouter, a GUI trace+vias commit) never collides
/// with an existing id or with an earlier mint in the same batch. Trace and via ids are
/// separate namespaces — one allocator carries a cursor for each.
///
/// This is the single definition of "the next free route id". Before Decision 22 the
/// `max(id) + 1` derivation was triplicated in the parser, [`crate::autoroute`], and the
/// GUI editing layer; those three now all mint through this one type.
#[derive(Clone, Copy, Debug)]
pub struct RouteIdAlloc {
    next_tid: u64,
    next_vid: u64,
}

impl RouteIdAlloc {
    /// Seed the allocator above the given trace ids and via ids. The first minted id in
    /// each namespace is `max + 1` (or `1` when that namespace is empty), matching the
    /// former `keys().map(|k| k.0 + 1).max().unwrap_or(1)` derivation exactly. Callers
    /// pass *every* id they must clear — for the parser this includes explicit ids on
    /// lines not yet reached, so parse order can never make a mint collide.
    pub fn above(
        traces: impl IntoIterator<Item = u64>,
        vias: impl IntoIterator<Item = u64>,
    ) -> Self {
        RouteIdAlloc {
            next_tid: traces.into_iter().max().map_or(1, |m| m + 1),
            next_vid: vias.into_iter().max().map_or(1, |m| m + 1),
        }
    }

    /// Mint the next trace id.
    pub fn mint_trace(&mut self) -> TraceId {
        let id = TraceId(self.next_tid);
        self.next_tid += 1;
        id
    }

    /// Mint the next via id.
    pub fn mint_via(&mut self) -> ViaId {
        let id = ViaId(self.next_vid);
        self.next_vid += 1;
        id
    }
}
