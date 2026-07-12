---
id: d10
title: "Courtyard: polygonal truth, cheap solver proxy, honest verify"
date: 2026-06-30
status: implemented (geometry 2026-06-30; solver SAT packing + honest verify 2026-07-03, branch feat/courtyard-pack, issue 0019 — see [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Placement honesty").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 10 — courtyard: polygonal truth, cheap solver proxy, honest verify

A courtyard is a `Keepout(KeepoutKind::Component)` `Feature` with a real `Shape2D` —
it falls out of Decision 1, and an imported KiCad courtyard outline *is* that feature
(no bbox re-derivation). The placement solver keeps its cheap AABB/convex-hull
penetration push as a **proxy**, then **verifies** the result against the real polygon
and reports residual overlaps — the same "propose cheap, verify against honest
geometry, drop/flag what actually clashes" pattern the router already adopted. The
lower-level overlap solver stays swappable (non-convex MTV later if needed).
