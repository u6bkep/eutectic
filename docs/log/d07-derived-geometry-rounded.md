---
id: d07
title: "Derived world geometry is correctly-rounded; predicates are tolerance-aware"
date: 2026-06-30
status: adopted (stated invariant)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 7 — derived world geometry is correctly-rounded; predicates are tolerance-aware (stated invariant)

Derived world positions (rotated pins/pads) are the **correctly-rounded** application
of the transform — deterministic and fab-exact at nm resolution, but **not** exact
lattice points. Consequences, accepted as invariants:

- **Never** store a rounded world coordinate or a float angle as authoritative;
  **never** diff on derived geometry. Diffs and the Salsa cache key on the transform
  parameters (exact), not the derived coords. This is the rule that keeps determinism
  and clean diffs.
- Geometric predicates (clearance, containment, coincidence) become **uniformly
  tolerance-aware** wherever rotation is non-cardinal. This is the same class of
  concession arcs already made (DRC is "optimistic by ≤ one sagitta") — not a new kind.
- nm-scale tolerance is orders of magnitude below PCB needs; it would only bite ASIC
  design, which is **not on the roadmap**.
- **Audit obligation:** code assuming exact coincidence of *derived* positions ("two
  pins at the same point", a pad corner on a grid) must switch to tolerance. The arc
  work already forced some of this (`segments()` coincidence handling).
