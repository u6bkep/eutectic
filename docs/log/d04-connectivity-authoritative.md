---
id: d04
title: "Connectivity stays authoritative; the slice/fill view is derived"
date: 2026-06-30
status: implemented (Phases 0–2, merged 2026-06-30)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model); see also §1 (the three-tier model).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 4 — connectivity stays authoritative; the slice/fill view is derived

Connectivity is **never** derived from copper geometry (a hairline gap or overlap must
not be able to redefine a net; user intent "this is GND" must survive). Nets remain an
authoritative fact that geometry is checked *against*. Likewise the manufacturing view
— per-layer slices, and the "filled rectangle minus knockouts" pour form — are
**derived materialized/fab artifacts**, not storage (pour fill is already derived
downstream). Authoritative copper is the set of positive role-tagged prisms.
