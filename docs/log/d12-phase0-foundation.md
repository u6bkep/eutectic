---
id: d12
title: "Phase-0 foundation: net is an annotation, Stackup goes live, PadGeo derives features"
date: 2026-06-30
status: implemented (Phase 0, merged 2026-06-30)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model); consumer survey in [n01](n01-geometry-fracture-finding.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 12 — Phase-0 foundation (resolves the §7 surface questions)

1. **Net is an annotation alongside a feature, not a field on `Feature`.** The derived
   piece is `(net?, Feature)` (matching today's `CopperPiece { net, shape, layers }`),
   keeping `Feature` pure physical geometry per "connectivity is authoritative and
   separate." Net is the recurring orphan the survey found (`RegionDecl.net`,
   `CopperPiece.net`; `Feature` carries `material`, not net).
2. **`Stackup` becomes live, stored in `Source`** (tier-1 design fact), defaulting to
   `default_2layer`. It is currently `default_2layer()` + tests only; the `Layer` /
   `PadLayers` → `ZRange` lowering that both PadGeo and RegionDecl need requires it.
3. **`PadGeo` stays stored on `PinDef`; it *derives* features, never *becomes* them.**
   `PadLayers` is deliberately stackup-relative (Top/Bottom/Through) so footprints are
   reusable, while `Feature` needs an absolute `ZRange` — so the shape is
   `PadGeo::features(comp, &Stackup) -> Vec<Feature>`, with the Through→all-copper-layer
   fan-out moving inside. This *confirms* Decision 5: the compact form is stored, the
   features are the fold.
4. **`Board`/`Cutout`/`Region` text directives stay as authoring sugar that lowers into
   features** (low churn). The two readers (`board_shape`, `regions`) collapse into one
   role-filtered `features()` view; Board's "last `Board` wins" single-outline semantics
   must be preserved when it becomes one Substrate feature among many.

Survey cleanup riders (additive, ride along with the relevant phase): the SVG render
draws a fixed `r=0.3` circle at `pin_world` instead of real pad copper, and pad `drill`
is stored but never exported — both fixable as PadGeo is converted.
