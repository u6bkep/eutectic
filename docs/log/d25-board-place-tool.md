---
id: d25
title: "Board Place Part tool and owned-renderer library preview"
date: 2026-07-18
status: implemented (`8a7738d`)
---

> Context: this ruling amends the tool-strip enumeration recorded in
> [d24](d24-ui-usability-rulings.md) and binds the gw-03 anatomy in the
> [UI oracle](../ui-oracle/README.md).

### Decision 25 — board placement owns gw-03

1. **The board strip gains a Place Part tool.** This deliberately amends
   d24's board enumeration. Gw-03's library-browser flyout is bound to that
   board tool rather than to the schematic Place Symbol tool: parts are board
   entities with footprints, and the schematic view derives the same new
   instance from source. Schematic Place Symbol still arrives with the
   schematic-authoring campaign.
2. **Part thumbnails use the owned renderer.** The browser elaborates a
   one-instance document, lowers its `world_features` through board scene
   ingest, and renders a fit-once, non-interactive `AppTexture`. The retired
   `VectorAsset`/`PathBuilder` board-lowering path is not revived.

The library browser is a docked, palette-like surface, not a modal. Its
focused filter owns bare typing, while canvas and Ctrl-modified chrome actions
remain live. Choosing a row leaves the palette open and arms repeated
placement; Escape disarms without leaving Place mode.
Each authored placement pins its allocated refdes to the matching instance id so
later insertions cannot renumber already placed parts.

Placement never reorders existing resolution: a part that already resolves in
the document's union is placed under that resolution (no new `use` line), and a
genuinely new package's `use` is inserted after the existing use block, so it
loses every collision against packages the document already uses. One
documented exception rides the architecture's own rule that real libraries
shadow toy names (architecture.md §9): authoring a `use` for a package whose
*other* parts collide with builtin-resolved names rebinds those instances,
exactly as the same hand-authored `use` line would — the W_LIB_COLLISION
finding names the shadowing.
