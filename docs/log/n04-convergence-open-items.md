---
id: n04
title: "Convergence open-items ledger (§8 of the retired decision record)"
date: 2026-07-03 → 2026-07-09
status: closed ledger (reconciled 2026-07-11: every bullet is either struck-resolved below, tracked in the issue tracker — 0004/0008/0024 —, or restated as an open question in architecture.md §8; component-body role/material is carried in architecture.md §8's open list)
---

> Context: implementation outcomes for [d06](d06-integer-quaternion-orient.md), [d10](d10-courtyard-polygonal-truth.md), [d13](d13-slab-name-identity.md)–[d18](d18-routes-persisted.md), [d22](d22-route-identity-persists.md), [d23](d23-schematic-features-tier.md), plus the router-honesty rework and refdes pinning.
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

## 8. Open items

- ~~Decision 22 (route ids in the state zone)~~ — **resolved (implemented
  2026-07-08, main `8f7c1ec`)**: id token in `route`/`via` lines, lenient parse
  (incl. a saturating allocator so a hand-authored `u64::MAX` id can't brick a
  file), one engine allocator (`RouteIdAlloc`), gapped round-trip test.
  Resolved 0034; unblocks gw-02/gw-14/gw-20.
- ~~Decision 23 (the schematic realized-geometry tier)~~ — **items 1–4 resolved
  (implemented 2026-07-09, main `669b2f7`)**: `schematic_features` query with
  semantic provenance, text as runs, style classes, single-homed bounds; SVG
  rewired as a serializer (byte-identical, golden fixture committed); GUI
  schematic view + pick fold from the stream (duplicated constants deleted);
  `symbol_body` artwork seam (derived box is the default realization). Still
  open from the ruling: item 5 (native footprint/symbol authoring — the
  library campaign) and gw-26 (junction dots).
- ~~Precision policy for the angle→quaternion lowering~~ — **resolved**:
  `ORIENT_ANGLE_SCALE = 1e6` (≈1e-6 rad; see `doc::Orient::from_angle_deg`).
- ~~Real non-copper layers (0020)~~ — **resolved (Decision 13, implemented 2026-07-02)**:
  slab-name identity, mask solids + `Void` openings, real silk/mask Gerbers,
  `z_to_layer` deleted. 0016 (footprint graphics) resolved alongside (side-relative).
- ~~Trace/via slab-name migration / route serialization (0011)~~ — **resolved
  (Decision 18, implemented 2026-07-03, branch feat/route-serialize)**: `Trace.layer`
  is a slab name, `Via.span: Option<(String,String)>` (None = full copper extent);
  routes persist in the `# routes` text state zone (pinned default, free/hint/fixed
  explicit — all four provenances round-trip); `route::Layer` ordinals are
  router-internal only; commit-time `validate_routes` gate (E_UNKNOWN_SLAB /
  E_NON_COPPER_SLAB / E_UNKNOWN_NET, post-elaborate — re-elaboration provably
  preserves routes); copper export = per-copper-slab forward query (byte-identical
  default-stackup Gerbers); `PromoteRoutes{nets}` freeze command (Decision 18's
  lockfile move, net-scoped minimal core).
- ~~Decision 16/17 implementation~~ — **implemented end-to-end (2026-07-03, four
  branches)**: `Shape2D::Area` + `BoardShape` deletion (feat/area-shape — plus
  map_points winding renormalization under reflection, erosion panics, region-fill
  G01 self-containment); unified `world_features` producer, pours→`NetFeature`,
  via/pad drill `Void`s, Excellon forward query with PTH/NPTH split = 0022, keepout +
  edge-clearance DRC = 0023 (feat/unified-features); mask-export slab iteration
  (feat/fab-svg); TTF outline fonts on `Area` (feat/ttf-fonts — `ttf-parser` is the
  first dependency, hand-assembled minimal TTF test fixture, `W_FONT_LOAD` is the
  crate's first warning-class diagnostic). Every branch adversarially reviewed
  pre-merge; all findings fixed (incl. one review premise refuted with evidence by
  the implementer — the SetSource commit gate already existed).
- ~~Auto-text~~ — **resolved (Decision 14, implemented 2026-07-03)**: `FpText` anchors
  + class registry (params/label/refdes queries), KiCad `fp_text`/`property` import
  (branches feat/class-registry, feat/auto-text). Follow-ups: refdes pinning via
  EntityId overrides (reserved); typed quantities at the simulation boundary.
- ~~Paste/fab virtual layers~~ — **resolved (Decision 15, implemented 2026-07-03)**:
  paste derived at export, fab an ordinary authorable zero-height `Datum` slab
  (branch feat/datum-slabs). **Fab output consumer landed (branch feat/fab-svg)**: a
  per-fab-slab SVG pass (`export::svg_fab` / `fab_svg_set`) iterates `Role::Datum` slabs
  by name (a `datum_slabs` sibling of `marking_slabs`) and renders each fab slab's
  `Role::Datum` features + board outline like silk — closing the "renders nowhere" gap.
  Fab *Gerber* output is still deferred: a `gerber_fab` would slot beside `gerber_silk`
  over the same `datum_slabs` (noted at `svg_fab`). Board-level `text` lowering is now
  **role-driven off the resolved slab** too (`elaborate::features`' text path forward-
  queries `Stackup::slab`, paralleling `part::graphic_features`), so `text layer=F.Fab`
  lands on fab, not silk — fixing a latent wrong-output bug where fab-slab board text
  shipped visibly on `F_SilkS`; silk output is byte-identical for the default stackup.
- ~~Bottom-flip axis convention~~ — **resolved (2026-07-03, branch feat/flip-axis)**:
  `Orient::flipped()` is Ry(180) (x-negates, y preserved — KiCad/fab board-turn
  convention, bottom silk upright); placement CSV decomposes the flip and reports the
  authored angle for bottom parts (KiCad `.pos` style). Quaternions are what's
  serialized, so no data migration. Future `.kicad_pcb` *placement* import maps
  side=back via Ry(180) — recorded as issue 0021.
- Whether component bodies get a dedicated role/material or reuse `Keepout`.
- Relation to issue 0004 (planes / multilayer): the volumetric convergence is the
  natural home for that work.
- ~~Coordinate-range / i128 ceiling (0018)~~ — **resolved (feat/checked-coords,
  2026-07-03)**: two-ceiling model — `geom::MAX_COORD` = 1e9 nm inclusive ingest bound
  (`E_COORD_RANGE` at text/command boundaries, `Err(String)` at kicad/svg import) and
  `KERNEL_SAFE_COORD` = 1.276e9 (the true 64·C⁴ i128 ceiling, compile-time-guarded)
  for the kernel debug_asserts, leaving composition headroom.
- ~~Polygon-courtyard solver packing (0019)~~ — **resolved (feat/courtyard-pack,
  2026-07-03)**: exact-integer convex SAT (edge normals + vertex-vertex axes, rounded
  margins folded in as g² ≥ r²·|n|²) replaces the AABB proxy in `NoOverlap`; imported
  courtyards flatten + hull; honest verify reports residual overlaps > 3µm as
  `E_COURTYARD_OVERLAP`.
- **Refdes pinning landed** (feat/refdes-pin, 2026-07-03): `refdes <path> <string>`
  override lines → `Doc::refdes_pins`; pins consulted first (opaque), parseable pins
  reserve their number (incl. against digit-suffixed registry prefixes), duplicate
  pins = non-blocking `E_REFDES_PIN_DUP` (the E_PIN_CONFLICT precedent).
- Follow-up lint: copper slab without a mask slab is silent (issue 0024, from the
  fab-svg review).
- ~~Autorouter honesty + N-layer + fine pitch (0003, most of 0004, part of 0023)~~ —
  **resolved (feat/router-honest, 2026-07-03)**: the autorouter's obstacle/grid/layer
  machinery reworked. Obstacles now derive from `route::world_features` (the unified DRC
  stream) — real pad **extents**, other-net traces/vias on their true slabs, copper
  **pours** (`Area` conductors), and `Role::Keepout` copper/route regions, rasterized per
  copper slab via the exact clearance kernel; inner-layer copper is no longer dropped. The
  grid is genuinely **N-layer** (`copper_slabs().len()`; A* over `(i,j,layer)`, via moves
  between adjacent layers at a per-crossed-layer cost; a through via blocks/needs room on
  every copper layer at its site). The **trace/via pitch split** (the QFN fix) decouples
  grid pitch (`min_trace_width + min_clearance`, resolving 0.4 mm pad pitch) from via size:
  via legality is a separate per-cell mask, and a via must additionally keep
  `via_pad/2 + width/2 + clearance` from any *other* net's same-run copper (an owner-ring
  check in A* — a coarser grid used to hide this). Board **masking** carves the grid to the
  real outline ∖ cutouts and pulls back from every edge by the edge clearance.
  `verify_and_prune` extended to also check keepout intrusion + board edge, so `routed`
  means DRC-clean (judgment call, flagged). `autoroute()`'s signature is unchanged (stackup
  derived internally). Adversarially reviewed — no correctness findings. **Explicitly still
  open (issue 0008, a future design cycle owns them):** rip-up/negotiation, topological /
  push-and-shove routing, net-ordering optimization, H/V per-layer directionality bias,
  length/impedance, blind/buried vias. Consequence: the greedy router routes fewer nets on
  a dense board than a rip-up router would (the PoC's toy-library fanout dropped vs. the old
  *permissive* point model), but everything it lays is genuinely clearance-clean — honesty
  over count.
- ~~Folding into `architecture.md` §8~~ — **done (2026-07-03)**: §8 rewritten to the
  current model (Decisions 13–18); the retired Stages-1–3 prose replaced; pour-kernel
  and arc narratives kept there as marked historical records. This document remains
  the authoritative decision-by-decision history.
