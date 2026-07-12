---
id: d16
title: "One hole-capable currency: `Shape2D::Area`, a single feature producer, the hole/void rule"
date: 2026-07-03
status: implemented (2026-07-03, main `a6e389d` — branches feat/area-shape, feat/unified-features, feat/fab-svg; resolves 0022, 0023 — see [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) (the unit; "One producer, two consumers"; the hole/void rule; the prismatic-matter fence).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 16 — one hole-capable currency: `Shape2D::Area`, a single feature producer, the hole/void rule (2026-07-03)

Scoping TTF outline fonts surfaced the question "how does a filled glyph with counters
(holes) enter the geometry currency?" — and auditing that question found the currency
is not single. A systematic survey (2026-07-03) of everything that carries geometry
found four genuine bypasses and two vocabulary shadows:

- **Copper pours** flow as `route::PourFill { layer, fill: region::Region }` — their own
  clearance predicate (`regions_within`), their own ratsnest incidence, their own Gerber
  path. The known violation.
- **Via drills are scalars, never geometry.** `Via.drill: Nm` fans into copper discs for
  DRC but never emits a `Role::Void` drill prism — unlike pad drills, which do.
- **`excellon_drill` reads `doc.vias` directly and only vias** — plated through-hole
  *pad* drills are missing from `board.drl` even though the pad `Void` features that
  describe them already exist and correctly reach the mask Gerbers (issue 0022).
- **Two parallel `Feature` producers that never meet**: `elaborate::features()`
  (substrate, mask, voids, keepouts, text) is consumed *only by export*; DRC and the
  autorouter re-derive copper independently via `route::net_features`. Consequence:
  keepout features are produced but no DRC rule consumes them — keepouts are
  unenforced (issue 0023).
- Vocabulary shadows: `route::Layer{Top,Inner,Bottom}` ordinals (the known 0011
  migration; regions and text already carry slab names — copper is the last holdout),
  and mask export re-entering through `gerber_mask(side: Layer)` instead of iterating
  `Role::Mask` slabs the way silk already does.
- **`geom::BoardShape` is the same smell with a Decision-1 blessing**: a bespoke
  outline+cutouts struct exists *because* `Shape2D` could not say "filled area with
  holes". Edge.Cuts export and placement containment consume it instead of the
  Substrate features carrying the same truth.

All of it reduces to two root causes, and Decision 16 is the fix for both:

**16a — `Shape2D` grows an `Area(region::Region)` variant.** A filled area with holes
becomes a first-class shape, so a `Feature` can carry it like any disc or capsule.
`radius()` is 0; `inflated()` delegates to the region kernel's exact offset;
`bbox`/`contains_point`/`points`/`closest_boundary_point` delegate to `Region` (rings
are polylines). Exporters gain one arm each: SVG emits an even-odd `<path>`, Gerber
emits G36/G37 per ring — the code the pour output already has, relocated to the shape
level. `clears()` generalizes (z-overlap unchanged; edge distance over rings plus
containment).

**16b — the hole/void rule.** Two mechanisms for negative space, deliberately not
interchangeable:

> A hole in an `Area` shape is *what the entity is*; a `Void` feature is *what one
> entity does to the rest of the board*. Holes when the negative is intrinsic to one
> entity's own cross-section, in-plane, and full-z for that feature (board cutouts,
> glyph counters, pour knockouts — knockouts are computed from other nets, but the
> result is the pour's own fill). `Void` features when the negative is contributed
> across entities, must stay individually enumerable, or spans a partial z (drills,
> mask openings, blind/buried cuts).

This maps exactly onto fab conventions, and the representation *is* the manufacturing
intent:

- **`Void` → drill data.** A `Void` preserves what Excellon needs — exact center +
  diameter (disc) or a capsule (G85 slot). Voids gain a **plated/non-plated bit**
  (pad/via voids plated; standalone authored voids default non-plated) driving the
  PTH/NPTH file split. A mounting hole is an authored NPTH `Void`, not a board cutout.
- **`Area` holes → routed contours (Edge.Cuts).** By the time a cutout is a hole in
  the substrate `Area` it is a polygonized ring (the region kernel flattens circles at
  construction — the diameter is *gone*). Extracting drills from `Area` holes is
  therefore banned permanently: it would be heuristic circle-recognition, an inverse
  projection (Decision 13). A hole in the `Area` declares "route this" the same way a
  `Void` disc declares "drill this". A user who authors a round cutout where an NPTH
  drill was better intent gets manufacturable output; at most a lint later, never a
  silent promotion.
- Consequences: vias lower to a conductor prism **plus** a `Void` drill prism
  (Decision 5's "cylinder + drill void", finally realized); `excellon_drill` becomes a
  forward query over through-cut/plated `Void` features (fixes 0022 structurally).

**16c — one producer of world-frame features.** A single `features()`-style query emits
*everything* — substrate, copper (pads, traces, vias, pours), voids, mask, keepouts,
graphics, text — and DRC and every exporter become filters over that one stream by
role/net/slab. `route::net_features` dissolves into it; pours become ordinary
`NetFeature`s with `Area` shapes (deleting `PourFill` and its bespoke DRC/ratsnest/
Gerber branches — ratsnest keeps its union-find, gated on the same features); keepout
enforcement turns on (fixes 0023). This also *supersedes Decision 1's `BoardShape`
representation while keeping its principle*: the outline stays authored input and the
board stays a derived query, but the query's result is now the `Role::Substrate`
feature itself carrying `Area(outline ∖ cutouts)` — one feature, no reassembly, plus a
`board_region()` convenience accessor. The `BoardShape` struct is deleted; solver
containment, autoroute bbox, Edge.Cuts, and SVG consume the substrate feature.

**16d — the prismatic-matter assumption, named.** `Feature` says all matter is a prism
(an extrusion along the board normal), and the evaluation model is **two-level 2.5D
CSG: union of solid prisms, minus void prisms, done** — no solids nested inside voids,
no re-additions, no curved z. This is a deliberate 2.5D commitment inside a 3D-first
model, justified by: (1) the manufacturing process only makes prisms — etching,
lamination, plating, drilling are extrusions along one axis, and Gerber/Excellon
cannot express anything else, so prisms are *exact* for everything a fab can build;
(2) exact integer booleans are achievable in 2D (the region kernel) and are a research
problem in 3D — a 3D-matter model would cost the zero-dependency exactness the DRC's
honesty is built on. The *spatial* vocabulary stays fully 3D: poses are quaternions
(Decision 6), z is authoritative `Nm` (Decision 2), slabs are z-intervals — data
accumulated under this decision remains valid in a true-3D future. Named escape
hatches, so lifting the ceiling is additive rather than a migration: rigid-flex /
folded boards become an *assembly* level above features (rigid sections, each locally
prismatic, posed in 3D by the existing quaternion machinery); tilted component bodies
become a separate posed body-volume feature kind (visualization/interference, never in
the exact-DRC path). Anything fancier must argue its way in as a new decision.

Expected behavior changes when implemented (deliberate, not regressions): pad drills
appear in `board.drl` (0022); keepout DRC may surface new violations on existing
boards (0023).

Staging: (1) `Area` + exporter arms, with the `BoardShape` supersession as the proof
case (the substrate is the simplest Area — one island, few holes, no nets); (2) the
unified producer — pours→`NetFeature`, via conductor+`Void` lowering, the Excellon
rewrite, keepout enforcement; (3) the trace/via slab-name migration rides here
naturally (`net_features`' `(Layer, NetFeature)` keying dissolves into the stream) —
0011's serialization design is Decision 18; (4) trailing: mask export
iterates `Role::Mask` slabs by name (dropping `side: Layer`) — **done (branch
feat/fab-svg)**: `gerber_mask` now takes the `Role::Mask` `Slab`, `gerber_set` loops
`role_slabs(Role::Mask)` (top-down, F before B), `mask_slab_of`/`mask_name` deleted;
output byte-identical for the default stackup (verified by diffing the `gerber` example
before/after).
