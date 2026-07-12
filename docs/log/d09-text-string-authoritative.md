---
id: d09
title: "Text: authoritative string + font, strokes derived"
date: 2026-06-30
status: implemented (first slice 2026-06-30 — built-in stroke font; outline fonts continue in [d17](d17-ttf-outline-text.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Annotation and text"); code home `eutectic-core/src/font.rs`.
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 9 — text: authoritative string + font, strokes derived

Users edit *mutable text* (refdes, values, notes), so the authoritative form is the
**string + font reference + transform + role/z**, and the `Shape2D` strokes are a
**derived** tier-3 cache for render/DRC/export. `Shape2D` stays a pure geometric
kernel — **no `Text` variant**. Text is its own authored entity (sibling to
`RegionDecl`) that lowers into `Marking` (or any-role) stroke features via a built-in
zero-dependency stroke font (lines + arcs, both of which `Shape2D` already supports).
Refdes/value text stays live — it re-derives when you rename, never baked geometry.
