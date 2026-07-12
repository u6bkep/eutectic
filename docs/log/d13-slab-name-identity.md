---
id: d13
title: "Layer identity is a slab name; projections are queries, never inputs"
date: 2026-07-02
status: implemented (2026-07-02, main `869f458` — branches feat/slab-identity, feat/mask-model, feat/fp-graphics, feat/export-slabs; resolves 0020, 0016)
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Identity and vocabulary").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 13 — layer identity is a slab *name*; projections are queries, never inputs (2026-07-02)

The identity-side twin of this section's finding. Issue 0020 (silk stopgapped at
copper-z) and the trace-ordinal question exposed a recurring drift pattern: the 2.5D
layer view was designed as a *derived projection*, but in three places its **working
vocabulary leaked out and became stored identity** — `RegionDecl.layer`/`Text.layer`
store `route::Layer` (a copper-only positional ordinal), the pour bridge matches on it,
and exports run the projection *backwards* (`z_to_layer`, reconstructing layer identity
from derived z). Every convergence step that removed such a leak deleted code and
dissolved bugs (the copper-piece model, the mirror flag); every pain point has been one.

**What a slab is.** A `Slab` is a **named z-interval** — an entry in a lookup table,
not a primitive, not a container, and it holds no geometry or material. `layer=F.Cu` in
tier-1 means nothing more than "my prism's `ZRange` is `stackup.slab_z("F.Cu")`"; the
slab is **resolved away at elaboration** and the 3D ground truth contains only
`Feature`s. Sparse layers are the normal case (F.Cu with three traces is three skinny
prisms sharing a z-interval — no container, no membership); layers with a big solid
(substrate, default mask) are *generators emitting an ordinary solid Feature* whose z
was looked up from the slab, same machinery. Features remain free to ignore slabs
entirely (via barrels span many; component bodies rise above all of them). The *name*
is privileged only as the way to **refer** to a z-interval — stable across stackup
edits, unlike ordinals, unlike raw z; the slab is never privileged as a way to
**represent** anything.

**The rules:**

1. **Projections are queries, never inputs.** No derived view stores state, and no
   view's vocabulary appears in tier-1 source or in bridges between subsystems.
2. **Slab names are the universal layer-identity vocabulary.** Ordinals (`route::Layer`),
   router grids, and file splits are view-internal working forms, derived from the
   stackup at a module's edge and confined behind it.
3. **No inverse projections.** Identity flows forward — carry the name, or
   forward-query per slab ("which features intersect this z-interval?" → that slab's
   Gerber; a via barrel correctly appears on every copper layer it crosses).
   `z_to_layer`-style reconstruction dies.

**No negative layers.** Slabs carry no polarity semantics. Solder mask is a generated
board-area solid `Feature` plus **deletion volumes** (`Role::Void` prisms, no-op where
nothing is present — CSG subtraction, same as board cutouts today). `Role::MaskOpening`
retires in favour of `Void` at mask z. Gerber's draw-the-openings convention is an
**export-format detail** that never leaks inward.

**Consequences:** `RegionDecl`/`Text` (and future footprint graphics) carry a slab
*name*; elaboration resolves it via `Stackup::slab_z` and an unknown name is a **hard
elaboration error** (the silent board-z/`ZRange(0,0)` fallbacks in `elaborate::layer_z`
die). The default stackup gains silk + mask slabs at honest z per side (paste is
derivable-by-default — a stencil artifact ≈ mask openings on SMD pads — authored only
when overridden). Traces/vias keep `route::Layer` **for now** because routes are
unserialized runtime state (issue 0011) and the router's adjacency math is genuinely
positional — but the moment 0011 makes routes authoritative, they serialize slab names,
and the ordinal survives only inside the router. Footprint-local layer references are
**side-relative** (a footprint's silk is "silk on *my* side"; F↔B swaps on flip, exactly
as `pad_features` already swaps pad copper via `is_bottom`) — the 0020↔0016 joint.
