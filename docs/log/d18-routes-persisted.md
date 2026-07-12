---
id: d18
title: "The autorouter is an editing tool; routes are persisted, non-derivable state"
date: 2026-07-03
status: implemented (2026-07-03, main `a6e389d` — branch feat/route-serialize; resolves 0011; the "no trace IDs in the text" line is amended by [d22](d22-route-identity-persists.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Routes are persisted state").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 18 — the autorouter is an editing tool; routes are persisted, non-derivable state (2026-07-03)

Resolves 0011 (route serialization) — and repositions the router while doing it. The
first design sketch treated routing as a derivation: hand routes as tier-1 `route`
directives, autorouted (`Free`) traces *not* serialized, re-derived by re-running the
router at load (mirroring placement). That assumption fails three independent ways:
the future router is a research-grade problem that must be free to be **stochastic**
(re-derivation would not reproduce the board the user reviewed); router speed must
never gate **document open/interaction time**; and the real workflow wants **partial
reroute** — select a few atrocious traces out of a mostly-good result and re-fire the
router at just those, in milliseconds. All three point the same direction:

**The autorouter is not a solver layer between source and output — it is a power tool
a user aims at some nets and fires.** Its output, once accepted, is *document state*
(closer to a GUI user drawing traces very fast than to elaboration). Consequences:

- **All routes persist in the text file.** Load = parse, never re-solve. The file
  remembers what the router did, so the algorithm may be stochastic, anytime-improving,
  or replaced wholesale without touching a single existing doc. No determinism
  requirement on the router, ever.
- **Provenance is what makes partial reroute low-friction.** Persisted traces carry
  it: hand-drawn/frozen = `pinned` (default in text), router-owned = `free` ("the
  router may rip this up and replace it"). Partial reroute = transactional rip-up of a
  selection (the `Transaction`/`Command::AddTrace` machinery the router already uses)
  + reroute of just those nets, with `pinned` traces as immovable obstacles. "Freeze"
  is flipping a tag on traces already in the file, not a snapshot operation.
- **Staleness is handled by checking, not re-deriving.** A source edit (moved
  component) may invalidate persisted routes; DRC and ratsnest gate on the real
  geometry, so stale routes surface as violations/unroutedness — honestly.
- **Serialization: routes join the state zone.** The text file already has two zones —
  the generative program, then `# overrides` (non-derivable state, persisted). Routes
  are a second state section, not generative directives; parse fills
  `doc.traces`/`doc.vias` directly, so elaboration never owns routes and re-elaboration
  cannot wipe them. The serializer contract is amended from "materialized state is
  intentionally not emitted" to "**re-derivable** state is not emitted" — routes are
  materialized but *not derivable* (expensive, stochastic, user-blessed), which is
  exactly why they persist. Placement's contract stays as-is (the relaxation solver is
  cheap and re-derives); if that ever stops being true it takes the same escape path.

```
# routes
route gnd F.Cu w=0.15mm (1.2, 3.4) (5.6, 3.4) (5.6, 8.0)   # pinned (default)
route gnd B.Cu w=0.15mm (2.0, 1.0) (2.0, 9.0) free          # router-owned
via   gnd (5.6, 8.0) drill=0.3mm pad=0.6mm                  # full-span default
```

Details: **slab names, not layer ordinals** (this is Decision 13 rule 2 / Decision 16
stage 3 landing — `Trace.layer`/`Via.from,to` migrate to slab-name storage;
`route::Layer` ordinals retreat to router-internal grid state); unknown slab /
non-copper slab / unknown net at parse → hard diagnostics (the `E_UNKNOWN_SLAB`
family); **no trace IDs in the text** (`TraceId`s minted at parse/routing,
session-local); via span defaults to full copper extent, explicit `F.Cu..In1.Cu` for
blind/buried when multilayer arrives; polyline-only paths now, the grammar does not
preclude arc segments later.

Accepted consequence, stated as a decision: **autoroute output causes file churn** —
running the router changes what serializes. That is the truth, not a bug: the file
records what the router did, which is what makes rerolling it safe; diffs stay
reviewable because `free` traces are labeled.
