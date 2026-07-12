---
id: d08
title: "The 3D view is first-class; the 2D top view is a locked projection"
date: 2026-06-30
status: adopted (design stance; the shipped views are 2D projections, a 3D view is future work)
---

> Context: the current model lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 8 — the 3D view is first-class; the 2D top view is a locked projection

Orientation is described **fully and generally** (Decision 6 — a quaternion), so a 3D
view of the board is the natural primary view and the familiar 2D top view is a *locked
projection* of it — not the other way round, and not a special "side" flag. "Top vs
bottom" is just a quaternion that includes an in-plane flip (a rotation); the mirrored
*appearance* belongs to the 2D projection. Continuous off-axis 3D rotation (a tilted
body) stays a **render-only** annotation that never feeds DRC, placement, or diffs.
