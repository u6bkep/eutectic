---
id: d23
title: "The schematic realized-geometry tier: `schematic_features`, artwork as a seam, two library vocabularies"
date: 2026-07-09
status: items 1–4 implemented (2026-07-09, main `669b2f7`); item 5 (native footprint/symbol authoring) recorded for the library campaign; junction dots tracked as gw-26
---

> Context: restated in [architecture.md §3](../architecture.md#3-schematic-front-end-connectivity-is-truth-drawing-is-a-view).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 23 — the schematic gets its realized-geometry tier: `schematic_features`, artwork as a seam, two library vocabularies (2026-07-09, ruled; items 1–4 implemented same day, main `669b2f7`; item 5 recorded for the library campaign)

Surfaced while designing the owned-canvas renderer (gui-architecture.md "Canvas
strategy"): the renderer wants to consume both views through one ingest contract,
and the board side has the right shape for that — `route::world_features`, the
single realized-geometry stream every consumer (DRC, router, Gerber, SVG, GUI
render, GUI pick) filters. The schematic side has **no equivalent tier**. Its model
is healthy (`SchematicLayout` authored tree → `reflow` → per-component
`Placement`s), but everything that makes the drawing a drawing — pin stubs,
headers, net tags, nc marks, wires meeting stub tips, the unplaced-bin divider —
is realized *inside the views, twice*: once as SVG strings in
`schematic_svg.rs`, once as VectorAssets in `eutectic-gui`'s `schematic_view.rs`,
with constants copy-synced under "kept in sync" comments. That is the same
duplicated-conventions disease the board side already cured, and pointing the new
renderer at `SchematicLayout + reflow` directly would write the conventions a
third time.

Ruling:

1. **A core query, `schematic_features(doc, lib)`, becomes the one place the
   schematic drawing is realized.** It emits typed primitives in schematic space
   (strokes, discs, polygons, text **runs**) covering everything the SVG draws
   today, each carrying semantic provenance (component path / pin / net / wire /
   bin chrome) and a **style class** (symbol outline, pin stub, wire, net tag,
   header, …). No colors, no fonts-as-geometry, no view-toolkit types in the
   contract. Deterministic order, like every producer in this codebase.
2. **Text is a run, not glyphs.** The stream carries position/height/justify/
   content; each consumer realizes it (SVG `<text>` today, MSDF atlas in the
   owned renderer). Stroked-glyph realization is reserved for fab ink
   (board silk via `world_features`), where the glyphs *are* the artifact.
3. **Views become pure consumers.** `schematic_svg` is rewired as a dumb
   serializer of the stream — byte-identical output, it remains the headless/
   agent artifact and the test oracle, but conventions no longer live there. The
   GUI's `schematic_view` becomes a thin stream→VectorAsset projection (itself
   scheduled for deletion with the viewport path), and its pick candidates
   derive from stream provenance, so hit-testing and rendering cannot drift.
   The duplicated constants get one home in core.
4. **Symbol artwork is a seam, not a feature (yet).** A symbol's body is
   realized by a single function — "body primitives for this part def" —
   whose only implementation today is the derived box-with-pins
   (`symbol_extent`/`pin_slots`). Authored artwork (a resistor squiggle with two
   pin anchors and no box; line art plus **semantic anchors** — pin anchor =
   point + approach direction, label slots for derived text) later replaces the
   default *behind the seam*, with no contract change.
5. **The library direction (recorded, not built): footprints and symbols are the
   same kind of thing in two vocabularies.** Both are authored primitive bundles
   plus semantic anchors. A footprint speaks the *fab* vocabulary — "an
   instantiated pcb without the FR4": pads/silk/mask/drills with real roles and
   z, the pin anchor being the pad group (`world_features` already instantiates
   `PinDef::pad_features` this way; KiCad import is one producer into the same
   model). A symbol speaks the *annotation* vocabulary — line art + anchors, no
   fab meaning. Native authoring for both belongs in the text grammar (def-style
   blocks), **not** literal SVG — "an SVG sketch with semantic markers" describes
   the editing experience, and SVG is what it exports as. Consequence: a symbol
   or footprint editor is the owned renderer pointed at a def's elaboration —
   the WYSIWYG library editor falls out of the tier structure.
6. **Connections stay authored; derived presentation must never lie.**
   Reaffirms §20d: the netlist is truth, drawn wires are authored documentation
   (the schematic is the EE's composed artifact, as wikis are for software
   engineers), tags/ratsnest are the derived fallback for the unwired. Derived
   decoration may only restate authored facts — e.g. a junction dot may be
   derived where authored same-net wires *share an endpoint or waypoint*, never
   at a mere visual crossing. (Junction dots themselves are a follow-up feature,
   deliberately excluded from the refactor slice.)

Sheets/hierarchy stay out of the contract (a sheet is a plane/group when it
comes; an Altium-style stacked-instances affordance is a style bit later).

Rejected alternative: keep per-view realization and let the owned renderer be a
third writer of the conventions. Three copies of stub/tag/header geometry that
must agree to the pixel, in three dialects — the exact class of drift the
`world_features` convergence existed to kill.
