---
id: n05
title: "Copper pours / solder mask: the region kernel (issue 0004) — staged build record"
date: 2026-06-29 → 2026-06-30
status: historical record (the kernel — `Region`, exact-integer booleans, dilation offset — is unchanged and live; consumer names superseded by [d16](d16-area-unified-producer.md))
---

> Context: the kernel's role in the current model is stated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model).
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

### Copper pours / solder mask: the region kernel (0004) — historical record

> The staged narrative below is retained as the *record of how the region kernel was
> built and proven*; names it mentions have since moved under Decision 16 (pours flow
> as `NetFeature` `Area`s from `route::world_features`; `route::pours` is the view;
> mask export iterates `Role::Mask` slabs by name via `gerber_mask(&Slab)`; region
> declarations reference slab *names*, not `Layer` ordinals). The kernel itself —
> `Region`, the exact-integer booleans, the dilation offset — is unchanged and is now
> also the backbone of `Shape2D::Area`.

A copper pour, a solder-mask layer, a paste stencil, and a keep-out-aware fill are **one operation**:
*offset some shapes, then boolean-combine regions*. A pour is `zone − ⋃(foreign_copper ⊕ clearance)`
(with same-net thermal spokes); a mask is `⋃(pad ⊕ mask_expansion)`; paste is the same with a
reduction. So instead of a one-off "pour" feature we build the shared **offset + polygon-boolean
kernel** once (`src/geom/kernel.rs`) and let every consumer fall out of it.

- **`Region` = a set of oriented rings** (CCW outer, CW holes) under the non-zero winding rule — so a
  pour with knockouts (area + holes), disjoint copper islands, and nested cut-outs are one type. It is
  the result of every boolean.
- **Boolean** (`union`/`intersection`/`difference`) subdivides the two inputs' edges at their shared
  crossings (each crossing rounded to nm **once** and used to split *both* edges, so no cracks open),
  classifies each fragment by a midpoint inside/on-boundary/outside test, selects per the operation
  (with explicit coincident-edge rules), and stitches survivors back into rings. Predicates
  (orientation, winding, on-segment) are exact `i128`; only the shared rounding is approximate, and it
  is deterministic.
- **Offset is a radius bump, not a new algorithm.** A `Shape2D` is already a skeleton ⊕ a disc of
  `radius`; inflating by clearance `c` is `radius += c` (disc Minkowski sums add radii — exact).
  `region::shape_to_region` then realises any inflated shape as a filled `Region` by the **dilation
  decomposition** (core area ∪ one rect per skeleton edge ∪ one disc per vertex) — which reuses
  `union`, so there is exactly one boolean engine. The radius-disc is tessellated at a fixed fine
  resolution (integer direction table; the only float is the correctly-rounded IEEE `sqrt` for an
  edge normal, matching the `closest_on_segment` precedent). **Skeleton arcs** (a `Seg::Arc` edge,
  3-point start/mid/end) are flattened at the geometry seam (`Path::flatten`, chord tolerance
  `DEFAULT_CHORD_TOL`) so the boolean only ever sees straight edges (**strategy A**); the
  authoritative model carries the arc, the flattening is a transient the kernel/derived-fill consume,
  never stored — so an arc-exact boolean could replace the tessellation later with no change to the
  representation or to export. The fill is itself a **derived (tier-3)** result, doubly transient.

**Stage 1 done:** the `region` kernel — `Region`, `union`/`intersection`/`difference`,
`shape_to_region` (offset via dilation), and exact-integer predicates — landed standalone with a
degenerate-case test suite (shared edges, corner-touch, concave dilation, multi-knockout pours,
containment edge cases, determinism). **Stage 2 done:** the region *primitive* — an authored
`elaborate::RegionDecl` (`Shape2D` + `Role` + optional `net` + copper `Layer`), exposed as a
`GenDirective::Region`, assembled by the shared `elaborate::regions(&Source)` reader (mirroring
`board_shape`), and round-tripped by the text front-end (`region <role> [net=..] layer=.. <pts>`, with
keep-out kinds and inner layers). It is tier-1 authoritative; the knockout fill stays derived.
**Stage 3 done:** the **derived pour-fill query** — `route::pours(...)` (a view over the unified
`route::world_features`, Decision 16) computes, for each
`Conductor` region, `fill = outline − ⋃(foreign_copper ⊕ clearance)` via the stage-1 kernel
(`Shape2D::inflated` is the exact Minkowski offset = a radius bump; foreign = different-net copper on
the pour's layer; same-net copper is *not* knocked out — it is what the pour connects to). The fill is
a `region::Region` (outer boundary minus a hole per obstacle), bound to its net + layer, recomputed
not stored. Net-reference validation moved into elaboration: a pour on a typo'd / unconnected net
(`E_UNKNOWN_NET`) or with no net (`E_POUR_NO_NET`) is a hard fault, same no-silent-dangle guarantee as
pins. Tests: foreign-pad knocked out *with clearance*, same-net pad kept, other-layer copper ignored,
determinism, both validation faults. **Scoping note:** the DRC *consumption* of the fill —
clearance (incl. pour-vs-pour shorts) and connectivity-through-the-fill — is folded into the next
stage, because both need the same "region-incidence-with-copper" primitive (is a pad inside / within
clearance of the fill); building it once avoids duplicate machinery, and the knockout's
clearance-correctness is already proven by the stage-3 tests. (At stage 3 pours had no consumer yet,
so deferring the wiring regressed nothing.) **Stage 4 done:** pours are now real copper in DRC. Two new region primitives:
`region::regions_within(a, b, thr)` (do two regions overlap or come within `thr` edge-to-edge — exact
i128 segment distance) and `Region::islands()` (split a fill into connected filled components — each
CCW ring an island, holes attached by containment). DRC wiring: (1) **clearance** — pour-vs-pour: two
different-net pours overlapping/within clearance on a layer is a short (foreign-copper-vs-pour is clean
by construction, so only pour-vs-pour is new); (2) **connectivity** — `pin_islands` gains a node per
pour island, and a pad/trace/via landing on an island joins it, so a pour **collapses the ratsnest**
(the PoC's 54-pin GND problem); a pour *fragmented* by its knockouts leaves pads on different islands
disconnected — surfaced honestly as remaining `Unrouted` islands. A region-only edit now bumps
`geom_rev` (regions diffed in `command.rs`) so the incremental `Drc` query recomputes — no latent
staleness. Tests: pour connects two GND pads (vs unrouted without it), a full-width foreign trace
splits the pour into two islands (pads stay unrouted), overlapping GND/PWR pours short. **0004's
copper-pour half is now functional end-to-end for DRC** (planes for GND/power on 2 layers); the
multilayer-routing half stays in 0008's orbit. **Stage 5 done:** pours reach fab output. Each pour
fill is emitted per layer as an RS-274X `G36`/`G37` **region fill** — the outer ring(s) and hole rings
as contours in one region statement, so the knockouts come out as voids (a fill is already a
tessellated polygon, so no arcs needed). `copper_layers` includes pour layers (an inner-layer pour
gets its own Gerber). SVG draws each pour as a translucent layer-coloured `<path>` with even-odd fill
(holes read as voids), under the components/traces. The shared `export::pours_of` builds the
membership netlist and calls `route::pours`, so DRC and fab see identical fills. Tests: Gerber
emits `G36`/`G37` with outer + knockout-hole contours (bottom layer has none); SVG draws the pour
path; fab output deterministic with a pour. **Scope note:** the custom-pad / rounded-outline
bounding-box-collapse fidelity debt was not repaid by the pour work itself — it was repaid
afterwards by the **arc-capable `Shape2D`** (see "Arc support" below): custom pads now import as
compound copper including `gr_arc` edges, and arc-bearing outlines export as true `G02`/`G03`.
Routing complex *pads* through region fills (vs. the aperture-flash path) is still a focused
follow-up. **Stage 6 done (the family is complete):**
solder mask is the **dual** of the pour, and falls out of the same offset. `export::gerber_mask(side)`
emits the `F.Mask`/`B.Mask` layer as the **openings** — every component pad on that side flashed as
its copper aperture inflated by `DesignRules::mask_expansion` (the fab inverts to coverage); through-
hole pads open on both sides, vias are tented. The fab fileset (`gerber_set`) now ships
`board-F_Mask.gbr` / `board-B_Mask.gbr` alongside the copper, edge-cuts, and drill. So the one
offset+boolean kernel now serves pours (offset + difference) **and** mask (offset only) — exactly the
"getting this right gives us both" the design aimed for; paste stencil is the same with a *reduction*
when wanted. **0004's copper-pour / plane / mask family is now complete end-to-end** (author → DRC
connect+clearance → Gerber/SVG fab output) for 2-layer boards; the separate **multilayer-routing** half
of 0004 (a router that lays inner-layer copper, the stackup driving real layer count) stays in 0008's
orbit. The DRC pass is `O(N²)` (broadphase spatial index deferred — see performance notes); an
arc-*exact* boolean (vs. the current flatten-at-the-seam) and the 3D-`Solid` boolean are deferred but
representable. (Noted limits: floating/unnetted pads not yet knocked out of a pour; SMD-pad↔pour
incidence is all-layer like the rest of the pin model; Gerber not yet viewer-validated — 0009.)
