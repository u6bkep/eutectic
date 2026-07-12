---
id: d05
title: "The entire geometry model is a tracked fold; the prism soup is derived"
date: 2026-06-30
status: implemented (Phases 0–2, merged 2026-06-30; the one producer is `route::world_features`, [d16](d16-area-unified-producer.md))
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 5 — the entire geometry model is a tracked fold; the prism soup is derived

The whole physical-geometry model is a **demand-driven, dependency-tracked fold**, not
stored geometry. What is *authoritative* is a set of compact, high-level records:

- a **trace** is a polyline of points + widths,
- a **footprint** is a stored description (parametric pads, graphics, courtyard, text),
- a **via / through-hole** is a small parametric record (drill Ø, pad Ø, from-layer,
  to-layer),
- a **region** is an outline + role, a **text** is a string + font + transform.

`Vec<Feature>` — the prism soup — is the **derived, cached, Salsa-style output** of
folding those records through the placement transforms. The volume is **never stored**;
move one component and only the affected features re-expand, everything else is a cache
hit. "Using the pads from a library footprint" is a tracked function from the
footprint's storage to its geometry, and because it is memoised and dependency-tracked,
the conversion work runs *rarely* — only when its inputs change.

A via is therefore **not** a special geometry type; it is one such parametric footprint
that derives a cylinder + drill void + connecting discs. (The router's `Via` stays as a
*routing-tier* abstraction — "transition between layers here" — that lowers to a tall
conductor prism for DRC/sim. Two tiers, one derived from the other.)

This is also the lens that exposes the current **import** behaviour as a divergence —
see the note in §1.
