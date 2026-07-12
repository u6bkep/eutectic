---
id: n01
title: "The geometry fracture finding and consumer survey (convergence framing narrative)"
date: 2026-06-30
status: historical analysis (the convergence it motivated is executed; current model in architecture.md §8)
---

> Context: the decisions this narrative motivated are [d01](d01-feature-single-currency.md)–[d12](d12-phase0-foundation.md); the plan it fed is [n02](n02-convergence-plan.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

This record captures the foundation decisions; it *realigned the implementation* with
what §8 already stated and sharpened three points (the single primitive, the placement
transform, and how far "volume" goes now).

The trigger was scoping footprint-graphics / board-outline import (issues 0016/0017).
Pulling that thread surfaced that the *design* already wants one uniform geometry
model, but the *implementation* has fractured into several parallel geometry types.
Importers built on the fractured types would be redone by the convergence, so the
convergence comes first.

---

## 1. The finding: geometry has fractured; the design wants one primitive

Conceptually there is **exactly one physical-geometry primitive**: a role-tagged,
material-tagged **prism** — a `Shape2D` extruded over a `ZRange`. That is literally
`Feature { role, material, extent: Prism { shape, z } }`. Board substrate, copper
traces, pads, vias, silk, mask openings, courtyards, cutouts, component bodies — *all
of them are that one thing*, differing only in `role`, `material`, and z-range.
"Layers" are not a primitive; they are **named default z-ranges** (`Stackup` slabs)
that make 2.5D authoring ergonomic. §8 already says this ("a layer is just a named
z-slab, never a primitive"; "richness comes from geometry + composition, not from
proliferating roles").

The implementation drifted: geometry is actually stored through **several parallel
types** that each grew where first needed —

- `BoardShape { outline, cutouts }` — a bespoke type for the board edge
- `PadGeo` / `PadCopper` — a bespoke type for pad copper
- `RegionDecl { shape, role, layer }` — for pours / keep-outs
- `Feature` — the *nominal* unifying type, which **almost nothing stores through**

This fragmentation is the "premature specialization of layers" smell. `BoardShape`
is the clearest case: per §8 a board outline is "the boundary of a `Substrate`
prism" and a cutout is "a `Void`", so the board should be Substrate/Void *features*
and `BoardShape` should be a **derived view**, not authoritative storage. (Note that
`elaborate::board_shape` is *already a function* — we are halfway there.)

### How imported KiCad data is stored today, and why it diverges

`import_footprint(text) -> PartDef` parses the `.kicad_mod` sexp **once, eagerly**, and
builds a `PartDef` whose pads (`PinDef.pad: PadGeo`) already hold **expanded `Shape2D`
copper** — the pad `(at)` angle baked in (cardinal exact; off-axis float-rotated and
rounded to nm *at import*), and lossy fallbacks (custom/trapezoid → bounding box) baked
silently. The result sits in `PartLib = BTreeMap<String, PartDef>`, a plain map; the
raw sexp is **discarded**. Measured against Decision 5 (geometry is a *derived* fold),
this diverges three ways the convergence should fix:

1. **The derivation boundary is at the geometry layer, not above it.** `PadGeo` is
   expanded `Shape2D` stored as authoritative — the Decision-1 fragmentation. It should
   *be*, or *derive*, `Vec<Feature>`.
2. **Import is eager and one-shot, not a tracked query node.** "Footprint → geometry"
   is a manual call whose output is frozen, not a memoised dependency-tracked function.
3. **The authoritative input is lost.** With the source sexp discarded, the lossy steps
   are irreversible and there is nothing high-level to re-fold from.

This is the concrete instance of "draw the import boundary at the right layer" —
resolved by Decision 11 (§5).

## 2. Volumetric-honest, because 2.5D special-cases the features we need now

The V1 question is **not** "how far do we take volume?" but "how much special-case
handling do the genuine 3D features we need *now* — through-hole pads, vias,
component bodies for collision — require to fit a simplified 2.5D model?" Worked
through, the usual assumption inverts:

- **Through-hole pad, 2.5D** = copper on F.Cu + copper on B.Cu + a drill + *an
  assertion they are the same pad linked through z*. That linkage is invented special
  data. **Volume**: one conductor prism spanning the board thickness. No linkage
  concept.
- **Via, 2.5D** = drill + pad-on-A + pad-on-B + connect-assertion. **Volume**: one
  *tall* conductor prism (a parametric footprint — see Decision 5). Same story.
- **Component body / collision** is inherently 3D. The 2.5D shoehorn is "2D footprint
  + height scalar," which breaks on any non-constant z-profile (a connector narrow at
  the board and wide above; a part at *negative* z in a cutout — §8 already flags
  low-profile USB-C). Height-scalar is leaky; `Prism` (later `Solid`) is honest and
  already reserved.

So the 3D features we need now are exactly the ones 2.5D handles *worst* — each
becomes a multi-layer linkage or a height hack. Volume represents them as a single
prism with **zero** special-casing. Holding to 2.5D here is *more* code now **and**
footguns later, not less.

And we are closer to volume than it looks: the geom primitive is **already z-aware** —
`Feature::clears` gates on `ZRange::overlaps`, i.e. "same layer" is already "z-ranges
overlap." Volume is not a rewrite; it is finishing what the geom layer started while
the authoring/storage layer stayed 2.5D-flavored.

## 3. Placement transform: exact-but-general, derived geometry rounded

The motivating case is a round PCB with side-firing LEDs around the perimeter:
cardinal-only orientation cannot express it. The hard fact: **the integer lattice is
closed only under cardinal (and Pythagorean-triple) rotations** — any other rotation
maps a lattice point to an irrational coordinate. You cannot have *both* arbitrary
rotation *and* exact-integer world positions; one must give.

The arc work already showed which one and how: **separate authoritative-exact
parameters from deterministically-derived geometry, and only ever diff/cache on the
former.** An arc stores three exact lattice points and derives centre/radius/
tessellation via **correctly-rounded** `sqrt`/`÷` (IEEE-mandated correct rounding →
bit-identical across platforms; this is why `hypot` was rejected — not on the mandated
list). Rotation gets the same treatment.

## 4. Text, courtyard

## 5. Library references & import storage (closes the §1 import boundary)

## 6. Consumer survey: two parallel models, and the convergence spine

A read-only survey of every consumer of `BoardShape` / `PadGeo` / `RegionDecl` /
`Feature` (2026-06-30) reshaped the plan. The tree holds **two complete
geometry-and-clearance models**, and the target one is dormant:

| | **Live model** (load-bearing) | **Dormant model** (target) |
|---|---|---|
| geometry | `Shape2D` + `route::Layer` / `PadLayers` / `PieceLayers` | `Feature { role, material, Prism{shape, ZRange} }` |
| clearance | bare `clearance_violated(Shape2D, Shape2D)` gated by discrete same-layer test | `Feature::clears` (z-overlap ∧ clearance) |
| z | none (2.5D layer enum) | `Stackup` / `Slab` → `ZRange` |
| used by | DRC (`check_drc`, `net_copper`, `pour_fills`), autorouter self-check | **nothing but `geom.rs` tests** |

So "Feature convergence" is **not** refactoring existing Feature consumers — there are
**none**. `Feature`/`Extent`/`Stackup`/`Slab`/`ZRange`/`Material` have zero production
construction, storage, or consumers; only `Role`/`KeepoutKind` crossed into the live
flow (on `RegionDecl`, not inside a `Feature`). The work is *building the consumer side
from scratch* and *retiring the live `route::Layer` model onto it*. The live DRC path is
`query.rs → route::check_drc → clearance_violated(Shape2D, Shape2D)` gated by "share a
discrete `Layer`?" — the migration of that path (and the autorouter's identical
self-check) is the real spine, heavier than "verify/reroute" implied.

Per-type difficulty: **BoardShape** is already a derived view (mechanical);
**RegionDecl→Feature** is low-risk (`role` is a near-vestigial one-bit "is-pour" today);
**PadGeo→Features** is moderate with one blocker (below).
