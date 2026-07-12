---
id: d15
title: "Paste is derived; fab is an ordinary authorable slab"
date: 2026-07-02
status: implemented (2026-07-03, main `659d82a` — branch feat/datum-slabs; fab SVG consumer feat/fab-svg; fab Gerber in the mechanical batch, main `e4996ae` — see [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Annotation and text").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 15 — paste is derived; fab is an ordinary authorable slab (2026-07-02)

The "virtual layer" question dissolves under Decision 13 — no new machinery:

- **Paste is derived, not authored.** Stencil apertures are a function of pad geometry
  (pad shrunk by a paste margin), exactly as mask openings are pad copper inflated.
  When stencil Gerbers are wanted, that is a forward query over pads — no slab, no
  role, no authoring vocabulary today.
- **Fab is just a named slab you may choose to author.** Fab graphics/text import as
  ordinary `FpGraphic`/`FpText` with layer `"F.Fab"`; `graphic_features` already skips
  layers absent from the stackup, so they materialize only if the user authors an
  `F.Fab` slab — zero-height (`ZRange` permits `lo == hi`), `Role::Datum` (already in
  the enum; becomes parseable). Datum is excluded from physical clash queries —
  zero-height ranges *touch* their neighbours since `ZRange::overlaps` is closed.
  Graphic lowering takes its `Role` from the resolved slab's role rather than
  hardcoding `Marking`. Consequence: silk identity is **role-driven, not
  name-driven** — a stackup that names a slab `F.SilkS` but gives it a non-Marking
  role silently drops that silk from every output (the name is a reference, the
  role is the meaning; Decision 13).
