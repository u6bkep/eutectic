---
id: d02
title: "z is authoritative and load-bearing now"
date: 2026-06-30
status: implemented (Phases 0–2, merged 2026-06-30)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model); framing narrative in [n01](n01-geometry-fracture-finding.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 2 — z is authoritative and load-bearing now

- `ZRange`/`Prism` are genuinely authoritative. Vias and through-hole pads are tall
  conductor prisms. Clearance is "roles have a rule ∧ z-ranges overlap ∧ 2D shapes
  within distance," end-to-end.
- **Verify during convergence:** whether the *live DRC path* uses the z-aware
  `Feature::clears` primitive or still shortcuts through the 2.5D `Layer` enum. If the
  latter, routing the live path through the primitive is part of the convergence.
- Component bodies enter as prism features for **box-profile** collision (kept simple).
