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
    /// former `keys().map(|k| k.0 + 1).max().unwrap_or(1)` derivation for every id below
    /// the `u64` ceiling. Callers pass *every* id they must clear — for the parser this
    /// includes explicit ids on lines not yet reached, so parse order can never make a
    /// mint collide.
    ///
    /// The `+ 1` is **saturating**, so a hand-authored `u64::MAX` id (Decision 22 point 2:
    /// "hand-editing can never brick a file") seeds the cursor at `u64::MAX` rather than
    /// overflowing — a debug panic on every parse, or a release wrap to `0` that would let
    /// a later mint silently clobber low-id routes. At that ceiling the namespace is truly
    /// full at the top: `mint_*` then hands back `u64::MAX` (see below), which *can* collide
    /// with the sole hand-authored `u64::MAX` route. That single top-of-namespace collision
    /// is inherent to a full `u64` and is the only residual — orders of magnitude milder
    /// than renumbering the whole file from `0`, and unreachable by actual minting.
    pub fn above(
        traces: impl IntoIterator<Item = u64>,
        vias: impl IntoIterator<Item = u64>,
    ) -> Self {
        RouteIdAlloc {
            next_tid: traces.into_iter().max().map_or(1, |m| m.saturating_add(1)),
            next_vid: vias.into_iter().max().map_or(1, |m| m.saturating_add(1)),
        }
    }

    /// Mint the next trace id. The cursor advance is saturating: at the `u64::MAX` ceiling
    /// (only reachable via a hand-authored max-value id — see [`above`](Self::above)) it
    /// stays pinned rather than overflowing to `0`, so a mint near the top can never wrap
    /// back to renumber low-id routes.
    pub fn mint_trace(&mut self) -> TraceId {
        let id = TraceId(self.next_tid);
        self.next_tid = self.next_tid.saturating_add(1);
        id
    }

    /// Mint the next via id. Saturating advance, as [`mint_trace`](Self::mint_trace).
    pub fn mint_via(&mut self) -> ViaId {
        let id = ViaId(self.next_vid);
        self.next_vid = self.next_vid.saturating_add(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_one_above_max() {
        let mut a = RouteIdAlloc::above([3, 7, 1], [2]);
        assert_eq!(a.mint_trace(), TraceId(8));
        assert_eq!(a.mint_via(), ViaId(3));
    }

    #[test]
    fn empty_namespace_starts_at_one() {
        let mut a = RouteIdAlloc::above([], []);
        assert_eq!(a.mint_trace(), TraceId(1));
        assert_eq!(a.mint_via(), ViaId(1));
    }

    /// A hand-authored `u64::MAX` id must not overflow the seed: in debug this would
    /// panic on *any* parse (bricking a file that needs no minting at all), and in
    /// release it would wrap the cursor to `0` so a later mint clobbers low-id routes.
    /// Saturating seed + saturating advance pin the cursor at `u64::MAX` instead — the
    /// mint stays at the top of the namespace and never wraps back over live ids.
    #[test]
    fn max_id_saturates_instead_of_overflowing() {
        let mut a = RouteIdAlloc::above([u64::MAX], [u64::MAX]);
        // Seed pinned at the ceiling, not wrapped to 0.
        assert_eq!(a.mint_trace(), TraceId(u64::MAX));
        assert_eq!(a.mint_via(), ViaId(u64::MAX));
        // The advance also saturates: still pinned, never 0.
        assert_eq!(a.mint_trace(), TraceId(u64::MAX));
        assert_eq!(a.mint_via(), ViaId(u64::MAX));
    }
}
