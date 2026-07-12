---
id: d01
title: "`Feature` is the single physical-geometry currency"
date: 2026-06-30
status: implemented (Phases 0–2, merged 2026-06-30 — commits `45d3df6` → `0c124f8`)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model); framing narrative in [n01](n01-geometry-fracture-finding.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 1 — `Feature` is the single physical-geometry currency

`BoardShape`, `PadGeo`/`PadCopper`, and `RegionDecl` converge onto `Feature`, as
either (a) thin authoring-sugar that **lowers into** `Feature`s, or (b) **derived
views** over them. Concretely:

- The board is a set of `Substrate` features (+ `Void` cutout features). `BoardShape`
  becomes a derived view: union the substrate features, the boundary is the edge —
  produced by a `board_shape()`-style query for the solver/export to consume.
- A pad's copper is one or more `Conductor` features. A compound pad is a union of
  features; clearance is the min over the union (already how DRC frames it).
- Pours / keep-outs lower to features with the authored role.

New non-copper features (silk, courtyard, fab, mask) then **fall out for free** —
they are just `Feature`s with a different role. This is the "good architecture makes
features cheap" thesis: the import work (0016/0017) becomes "produce `Feature`s and a
`BoardShape` *view* that already exist as targets."
