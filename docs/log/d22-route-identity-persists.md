---
id: d22
title: "Route identity persists: ids serialize in the state zone"
date: 2026-07-07
status: implemented (2026-07-08, main `8f7c1ec`; resolves 0034; amends one line of [d18](d18-routes-persisted.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Routes are persisted state") and §5 (identity strategies).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 22 — route identity persists: ids serialize in the state zone (2026-07-07; implemented 2026-07-08, main `8f7c1ec`)

Resolves 0034 and **amends one line of Decision 18** ("no trace IDs in the text —
`TraceId`s minted at parse/routing, session-local"). That line quietly contradicted
the architecture's own §2 invariant ("every entity has a stable persistent ID
independent of name, position, or array index"): with the parser re-minting dense
`1..N` ids in file order on every load, a `TraceId` *is* an array index. The GUI made
the gap visible — undo/redo snapshots go through serialize→`LoadText`, so any undo
across a state with an id gap renumbers every later trace and silently drops
selections — and three roadmap features are blocked outright, because each must hold
a route id across a serialize/parse boundary: DRC waivers keyed by feature identity
(gw-02), length-tuning groups (gw-14), identity-based diff/review (gw-20).

The reframe that settles the aesthetics: the format has **two identity strategies,
one per zone**. Design-zone entities (parts, nets, pins, overrides) get identity from
their *names* — hierarchical paths humans author; no visible id syntax, none wanted.
Routes are the one feature class with no natural name, and they live in the
`# routes` **state zone**, which is machine-written (router, GUI tools, agent APIs —
humans place vias in the layout view, not in text). A small integer token in a
machine-maintained zone costs nothing a human reads. KiCad's mistake was not
persistent identity — it was 128-bit UUIDs on entities that already had names.

Ruling:

1. **Trace/via lines carry their id**: `route <id> <net> <slab> …` /
   `via <id> <net> …`. Sequential small integers, emitted by the serializer,
   parsed back verbatim. State zone only; the design zone is untouched.
2. **Lenient parse — ids are advisory-but-stable, never load-bearing for
   correctness.** A line missing an id gets one minted (warning); a duplicate id
   keeps the first and re-mints the second (warning). Hand-editing can never brick
   a file; the guarantee is only "an id nothing disturbed stays put", which is
   exactly what waivers/diff/undo need.
3. **One engine-side allocator** (`Doc`-level next-id helper for traces and vias)
   replaces the max+1 derivation currently triplicated in the parser, the
   autorouter, and the GUI's editing layer.
4. **The round-trip contract gains identity**: serialize→parse preserves
   `TraceId`/`ViaId`, including gapped sets. The existing `routes_round_trip` test
   is upgraded with a gapped fixture (`{1,3,7}`) — today it passes only because its
   dense fixture happens to match what re-minting produces.
5. Consequence for the GUI: snapshot-undo becomes identity-exact with no GUI-side
   change (the snapshots already round-trip bytes; now they round-trip identity),
   selection survives undo, and waivers/tuning/diff may key on route ids.

Rejected alternative: **content-derived identity** (net + endpoint hash). It keeps
the text pristine but mutates identity exactly when the referenced trace is edited —
a waiver would detach the moment the user nudges the trace it waives, and a moved
trace diffs as delete+add. Wrong semantics for every consumer that motivated this.
