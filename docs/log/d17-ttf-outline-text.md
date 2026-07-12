---
id: d17
title: "TTF outline text rides `Area`"
date: 2026-07-03
status: implemented (2026-07-03, main `a6e389d` — branch feat/ttf-fonts; kerning in the mechanical batch, main `e4996ae`; implementation record shared with [d16](d16-area-unified-producer.md) in [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Annotation and text"); continues [d09](d09-text-string-authoritative.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 17 — TTF outline text rides `Area` (2026-07-03)

The continuation of Decision 9 (authoritative string + font, strokes derived) —
outline fonts change the derivation, never the authority:

- Glyph contours (TrueType quadratics) flatten to integer polygons and land in the
  region kernel — outer ∖ counters, exactly what the boolean kernel does — producing
  one `Area`-shaped `Feature` per glyph (or per run). Text lowers like every other
  graphic; silk export needs zero new paths. `font::text_regions(str, height, justify,
  &font)` sits beside `text_strokes`.
- **`ttf-parser` is accepted as the crate's first dependency** (no-std, zero-dep
  itself, well-fuzzed; a minimal own glyf/loca/cmap/hmtx reader is feasible but the
  composite-glyph + cmap zoo isn't worth owning).
- **Fonts are user-supplied paths; the built-in stroke font stays the default.** No
  embedded blob, no license questions, zero behavior change for existing docs. The
  text front-end grows a font directive when this lands.
- **Metrics match the stroke font's conventions**: scale so cap height = the authored
  height (not em-square, which renders ~30% smaller); ink-bbox Center for footprint
  text (swapping fonts must not shift existing labels); baseline/advance from `hmtx`
  for left-justified runs. Lowercase stops case-folding when a real font is active.
