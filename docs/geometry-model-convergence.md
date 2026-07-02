# Geometry-model convergence — decision record

Status: **Phases 0–2 implemented and merged to `main` (2026-06-30).** The single
`Feature` primitive is now the live clearance currency end-to-end — DRC, pours,
Gerber, and the autorouter all gate on `Feature::clears`, and the parallel
`route::Layer` copper-piece model (`CopperPiece`/`PieceLayers`/`net_copper`/
`copper_layers_present`) has been deleted. Commits `45d3df6` (Phase 0) →
`53f344a`/`f1a59e3` (Phase 1 lowerings) → `b2aa6d9`/`5d5d517`/`812a203` (Phase 2a–d)
→ `0c124f8` (review fixes). Decisions 1, 2, 4, 5, 11, 12 are realized in code.
**Bézier curve primitive** done (`Seg::Quadratic`/`Cubic`, integer de Casteljau,
SVG/Gerber export, text grammar — commits `…`→`9e98a26`), unblocking outline fonts +
SVG import + curved traces. **Placement transform (Decisions 6–8) complete**:
`doc::Orient` is an **integer quaternion** (Decision 6, refined — no mirror flag,
bottom-side is a rotation, side derived); Stages 1+1b (`3ec4fa6`/`3f60b5d`/`92d6e2a`)
gave the representation + **bottom-side placement**; **Stage 2** gives **arbitrary
planar-angle authoring** (`rotate <p> <any-deg>` lowers to a quaternion at parse;
non-cardinals serialise as `quat=(…)`) + a **ring-of-N** generative helper (the
side-firing-LED case). Off-axis rotation is no longer rejected.
**Parallel batch (2026-06-30)** cleared four more: the **SVG render** now draws real pad
copper (not a dot); the **`.kicad_pcb` Edge.Cuts importer** (`import_board_outline`,
resolves 0017's core); the **`slab` stackup grammar** (authorable `Stackup`); and the
**polygonal courtyard** geometry (`geom::convex_hull` + `part::courtyard_shape`,
Decision 10 — the geometry; the solver still uses the AABB proxy, issue 0019).
**Text/fonts first slice (Decision 9)** done: a built-in stroke font (`font.rs`,
A–Z/0–9/punct), a board-level `text "…" (x,y) h= layer=` entity lowering to `Role::Marking`
features, SVG silk render. **SVG board-outline import** done (`svg_import`, Béziers).
**Still open** (see §7): footprint graphics + auto-text (0016, builds on text);
real non-copper layers / silk at honest z (0020 — **Decision 13 recorded 2026-07-02**:
layer identity = slab name, projections are queries, no negative layers; spine in
progress); courtyard solver packing (0019); text follow-ups
(lowercase/TTF outline / footprint+refdes text / Gerber silk). This record is still
meant to be folded into `architecture.md` §8.

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

### Decision 1 — `Feature` is the single physical-geometry currency

`BoardShape`, `PadGeo`/`PadCopper`, and `RegionDecl` converge onto `Feature`, as
either (a) thin authoring-sugar that **lowers into** `Feature`s, or (b) **derived
views** over them. Concretely:

- The board is a set of `Substrate` features (+ `Void` cutout features). `BoardShape`
  becomes a derived view: union the substrate features, the boundary is the edge —
  produced by a `board_shape()`-style query for the solver/export to consume.
- A pad's copper is one or more `Conductor` features. A compound pad is a union of
  features; clearance is the min over the union (already how DRC frames it).
- Pours / keep-outs lower to features with the authored role.

New non-copper features (silk, courtyard, fab, mask) then **fall out for free** —
they are just `Feature`s with a different role. This is the "good architecture makes
features cheap" thesis: the import work (0016/0017) becomes "produce `Feature`s and a
`BoardShape` *view* that already exist as targets."

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

---

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

### Decision 2 — z is authoritative and load-bearing now

- `ZRange`/`Prism` are genuinely authoritative. Vias and through-hole pads are tall
  conductor prisms. Clearance is "roles have a rule ∧ z-ranges overlap ∧ 2D shapes
  within distance," end-to-end.
- **Verify during convergence:** whether the *live DRC path* uses the z-aware
  `Feature::clears` primitive or still shortcuts through the 2.5D `Layer` enum. If the
  latter, routing the live path through the primitive is part of the convergence.
- Component bodies enter as prism features for **box-profile** collision (kept simple).

### Decision 3 — what is reserved, not built

`Extent::Solid` and non-box z-profiles; true-3D **solvers** (router/placement
optimisation stay 2.5D-projected); continuous off-axis 3D rotation (render-only — see
Decision 7). The *representation* is volumetric now; the *solvers* stay 2.5D.

### Decision 4 — connectivity stays authoritative; the slice/fill view is derived

Connectivity is **never** derived from copper geometry (a hairline gap or overlap must
not be able to redefine a net; user intent "this is GND" must survive). Nets remain an
authoritative fact that geometry is checked *against*. Likewise the manufacturing view
— per-layer slices, and the "filled rectangle minus knockouts" pour form — are
**derived materialized/fab artifacts**, not storage (pour fill is already derived
downstream). Authoritative copper is the set of positive role-tagged prisms.

### Decision 5 — the entire geometry model is a tracked fold; the prism soup is derived

The whole physical-geometry model is a **demand-driven, dependency-tracked fold**, not
stored geometry. What is *authoritative* is a set of compact, high-level records:

- a **trace** is a polyline of points + widths,
- a **footprint** is a stored description (parametric pads, graphics, courtyard, text),
- a **via / through-hole** is a small parametric record (drill Ø, pad Ø, from-layer,
  to-layer),
- a **region** is an outline + role, a **text** is a string + font + transform.

`Vec<Feature>` — the prism soup — is the **derived, cached, Salsa-style output** of
folding those records through the placement transforms. The volume is **never stored**;
move one component and only the affected features re-expand, everything else is a cache
hit. "Using the pads from a library footprint" is a tracked function from the
footprint's storage to its geometry, and because it is memoised and dependency-tracked,
the conversion work runs *rarely* — only when its inputs change.

A via is therefore **not** a special geometry type; it is one such parametric footprint
that derives a cylinder + drill void + connecting discs. (The router's `Via` stays as a
*routing-tier* abstraction — "transition between layers here" — that lowers to a tall
conductor prism for DRC/sim. Two tiers, one derived from the other.)

This is also the lens that exposes the current **import** behaviour as a divergence —
see the note in §1.

---

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

### Decision 6 — orientation is an exact integer quaternion (no mirror flag)

Authoritative orientation = an **integer quaternion** `q = (w, x, y, z): i64`
(`doc::Orient`), the 3D-general form of the rotation. `apply` is

```
apply(p) = M(q) · p / |q|²      where |q|² = w²+x²+y²+z², M(q) integer
```

— an integer matrix·point then **one integer rounding division** (round-half-away):
**no `sin`/`cos`, no `sqrt`**, deterministic across libms, and exact when `|q|²`
divides cleanly. This refines the original "2D direction vector": a quaternion is its
honest 3D generalisation (a planar rotation about z is `(w,0,0,z)`; an off-axis tilt is
any `(w,x,y,z)`) and gives an even cleaner `apply` (no `sqrt` at all). It was chosen
over a stored angle because deriving a rotation from an angle needs `cos`/`sin`
(not IEEE-correctly-rounded — the `hypot` trap); the quaternion stores exact integer
data that *defines* the rotation, deriving the irrational matrix correctly-rounded.

- **No mirror flag.** Bottom-side placement is a *rotation* (a 180° flip about an
  in-plane axis, determinant +1), fully a quaternion — `q` with an x/y component. The
  mirrored *appearance* is a property of the 2D top-view **projection**, not the stored
  transform. "Which side" is **derived** (`Orient::is_bottom` — the sign of where local
  `+z` maps), and a flipped component's pad layers swap Top↔Bottom from that, with no
  bool to keep in sync.
- **Cardinals/flips are exact**, tiny quaternions (`|q|²` ∈ {1, 2}); the existing
  exact-position tests hold unchanged.
- **Arbitrary planar angle**: `30°` lowers to the best integer planar quaternion
  `(w,0,0,z)` with `(w²−z²):2wz ≈ cos:sin` — a one-time rational approximation at
  authoring/parse time (never re-derived at elaboration, so no `cos`/`sin` determinism
  hole). **Authoring intent** ("ring of N, facing outward") lowers to N concrete
  quaternions; the materialised placements are exact-as-stored. (Stage 2.)
- **V1 (Stage 1)** constructs only the 8 board-plane-preserving orientations (4
  about-z × top/flip), all exact; `apply` runs on planar `z = 0` points. Off-axis tilt
  + `Point3D` + 3D solving stay reserved (Decision 3).

### Decision 7 — derived world geometry is correctly-rounded; predicates are tolerance-aware (stated invariant)

Derived world positions (rotated pins/pads) are the **correctly-rounded** application
of the transform — deterministic and fab-exact at nm resolution, but **not** exact
lattice points. Consequences, accepted as invariants:

- **Never** store a rounded world coordinate or a float angle as authoritative;
  **never** diff on derived geometry. Diffs and the Salsa cache key on the transform
  parameters (exact), not the derived coords. This is the rule that keeps determinism
  and clean diffs.
- Geometric predicates (clearance, containment, coincidence) become **uniformly
  tolerance-aware** wherever rotation is non-cardinal. This is the same class of
  concession arcs already made (DRC is "optimistic by ≤ one sagitta") — not a new kind.
- nm-scale tolerance is orders of magnitude below PCB needs; it would only bite ASIC
  design, which is **not on the roadmap**.
- **Audit obligation:** code assuming exact coincidence of *derived* positions ("two
  pins at the same point", a pad corner on a grid) must switch to tolerance. The arc
  work already forced some of this (`segments()` coincidence handling).

### Decision 8 — the 3D view is first-class; the 2D top view is a locked projection

Orientation is described **fully and generally** (Decision 6 — a quaternion), so a 3D
view of the board is the natural primary view and the familiar 2D top view is a *locked
projection* of it — not the other way round, and not a special "side" flag. "Top vs
bottom" is just a quaternion that includes an in-plane flip (a rotation); the mirrored
*appearance* belongs to the 2D projection. Continuous off-axis 3D rotation (a tilted
body) stays a **render-only** annotation that never feeds DRC, placement, or diffs.

---

## 4. Text, courtyard

### Decision 9 — text: authoritative string + font, strokes derived

Users edit *mutable text* (refdes, values, notes), so the authoritative form is the
**string + font reference + transform + role/z**, and the `Shape2D` strokes are a
**derived** tier-3 cache for render/DRC/export. `Shape2D` stays a pure geometric
kernel — **no `Text` variant**. Text is its own authored entity (sibling to
`RegionDecl`) that lowers into `Marking` (or any-role) stroke features via a built-in
zero-dependency stroke font (lines + arcs, both of which `Shape2D` already supports).
Refdes/value text stays live — it re-derives when you rename, never baked geometry.

### Decision 10 — courtyard: polygonal truth, cheap solver proxy, honest verify

A courtyard is a `Keepout(KeepoutKind::Component)` `Feature` with a real `Shape2D` —
it falls out of Decision 1, and an imported KiCad courtyard outline *is* that feature
(no bbox re-derivation). The placement solver keeps its cheap AABB/convex-hull
penetration push as a **proxy**, then **verifies** the result against the real polygon
and reports residual overlaps — the same "propose cheap, verify against honest
geometry, drop/flag what actually clashes" pattern the router already adopted. The
lower-level overlap solver stays swappable (non-convex MTV later if needed).

---

## 5. Library references & import storage (closes the §1 import boundary)

### Decision 11 — content-addressed library references + instantiations; never a bare path, never expanded geometry

A part is **referenced, not inlined as geometry**. The authoritative storage is a
small reference plus instantiations; geometry is the tracked fold of (resolved source
→ `Feature`s), per Decision 5.

- **`LibraryRef`** = an abstract handle (`library_id : part_name`) **plus a content
  hash** of the source. *Not* a filesystem path. The hash is what the geometry fold
  keys on, so the cache is correct by construction (source changes → hash changes →
  re-fold).
- **Library table** resolves `library_id` → a location (vendored blob, CAS cache, or
  an fs path for local dev). This is the path-abstraction — KiCad's lib-table
  indirection — with the content pin added.
- **Vendored content-addressed store**: the resolved source is vendored into the
  project (or a CAS cache keyed by hash) and committed alongside, so the document is
  **self-contained and reproducible** while storage stays tiny.
- **Instantiation** = `(LibraryRef, transform, overrides)`, where `transform` is the
  Decision-6 exact transform and `overrides` reuse the existing tier-1 provenance
  ladder (a changed value or a moved silk label is the *same kind of thing* as a
  placement override).

What is stored alongside a ref is only what **overrides or selects** — transform,
per-instance overrides, the symbol↔footprint join, the content hash. **Never** the
expanded geometry; that is always the derived fold.

**Why not a bare path.** Everything in this system is built for deterministic,
diffable, reproducible. A raw fs path is a reference into the *environment*, not the
document: the same file folds differently (or fails) on another machine or next year,
and a board diff could hide an invisible library edit. That is the single biggest hole
we could punch in the reproducibility thesis — and it is *the* perennial ECAD pain
("missing footprint", "which library version?"). The fix is the Cargo/Nix pattern:
name it in the ref, pin it by content hash, vendor the content. This is the synthesis
of by-reference (small, single-source-of-truth, dependency-tracked fold) and inlining
(self-contained, reproducible — which is why modern KiCad embeds footprints).

### Decision 11a — the reference is source-agnostic; a native part type is coming

`LibraryRef` points to *some* resolvable source folded to `Feature`s — it does not
care whether that source is a KiCad sexp (today) or a **native component type**
(later, using the *same serialization PCBs use* — just defining pins, pads, graphics,
courtyard, text). Both import paths fold identically and both get cargo-style pinning.

This deliberately opens the door to a **cargo-for-ecad** dependency resolve/fetch
ecosystem — a direct answer to the KiCad library-repo problem that is a chronic sharp
edge in this space (unpinned, environment-dependent, hard to reproduce). Content
hashing now is what makes that future coherent rather than bolted-on.

**Scope:** the *model* (content-addressed ref + vendored source + instantiations) is
decided. The *mechanism* (lockfile format, network fetch, a real resolver) is
deferred — V1 can be vendored files resolved by a trivial table; the hash buys
correctness now and the upgrade path to fetch later. We do not build a package manager
to commit to the model.

---

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

### Decision 12 — Phase-0 foundation (resolves the §7 surface questions)

1. **Net is an annotation alongside a feature, not a field on `Feature`.** The derived
   piece is `(net?, Feature)` (matching today's `CopperPiece { net, shape, layers }`),
   keeping `Feature` pure physical geometry per "connectivity is authoritative and
   separate." Net is the recurring orphan the survey found (`RegionDecl.net`,
   `CopperPiece.net`; `Feature` carries `material`, not net).
2. **`Stackup` becomes live, stored in `Source`** (tier-1 design fact), defaulting to
   `default_2layer`. It is currently `default_2layer()` + tests only; the `Layer` /
   `PadLayers` → `ZRange` lowering that both PadGeo and RegionDecl need requires it.
3. **`PadGeo` stays stored on `PinDef`; it *derives* features, never *becomes* them.**
   `PadLayers` is deliberately stackup-relative (Top/Bottom/Through) so footprints are
   reusable, while `Feature` needs an absolute `ZRange` — so the shape is
   `PadGeo::features(comp, &Stackup) -> Vec<Feature>`, with the Through→all-copper-layer
   fan-out moving inside. This *confirms* Decision 5: the compact form is stored, the
   features are the fold.
4. **`Board`/`Cutout`/`Region` text directives stay as authoring sugar that lowers into
   features** (low churn). The two readers (`board_shape`, `regions`) collapse into one
   role-filtered `features()` view; Board's "last `Board` wins" single-outline semantics
   must be preserved when it becomes one Substrate feature among many.

Survey cleanup riders (additive, ride along with the relevant phase): the SVG render
draws a fixed `r=0.3` circle at `pin_world` instead of real pad copper, and pad `drill`
is stored but never exported — both fixable as PadGeo is converted.

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

## 7. Convergence plan: sequential foundation → parallel fan-out → sequential spine

> **Status: executed (2026-06-30).** Every phase and post-convergence step below has
> landed on `main` — see the Status header at the top of this doc for the commits and
> what's still open. This section is retained as the *record of how it was sequenced*
> (the foundation→fan-out→spine shape, and the parallel-batch approach), not a live TODO.

Contention is concentrated in `route.rs` (`check_drc`/`net_copper`/`pour_fills`) and
`export.rs`, which both the DRC migration and the Region/Pad lowerings touch. That
bounds parallelism to a single fan-out, bracketed by sequential work:

- **Phase 0 — foundation (sequential, one owner, small).** Canonical `Feature` + the
  `(net?, Feature)` piece; `Stackup` live in `Source` + `Layer`/`PadLayers`→`ZRange`
  lowering; the `features()` derivation API surface. Everything depends on this.
- **Phase 1 — parallel fan-out (3 worktrees off the Phase-0 base, disjoint files):**
  - **A · BoardShape** → derived Substrate/Void view (`elaborate.rs`, `geom.rs`;
    consumers `solve`/`autoroute`-bbox/`export`-outline). Touches none of the DRC core.
  - **B · `PadGeo::features(comp, stackup)`** + Drill→Void + Through fan-out (`part.rs`);
    rewrite the courtyard consumer. API + part-local consumers only.
  - **C · RegionDecl→Feature** + Board/Cutout/Region unification (`elaborate.rs`,
    `text.rs` authoring). Lowering only.
- **Phase 2 — integration spine (sequential, one owner).** Migrate `route.rs`
  (`check_drc`, `net_copper`, `pour_fills`) and `export.rs` (pad flashes, pour fills,
  outline) onto Phase-1 features, retiring the `route::Layer` clearance shortcut onto
  `Feature::clears` + `ZRange::overlaps`. Behavior-preserving for a default 2-layer
  stackup (discrete same-layer ≡ z-overlap). DRC-correctness-critical → one owner.

Then the post-convergence steps proceed on the corrected foundation:

5. **General placement transform** (Decisions 6, 7, 8) — **done**: an integer
   *quaternion* (no mirror flag — refined from the original "direction + mirror"; side
   derived), derived geometry correctly-rounded, arbitrary planar angle + ring-of-N.
6. **Text** (Decision 9) — **first slice done**: stroke font + board-level text →
   `Marking` features. Outline/TTF, footprint/auto-text, real silk layer (0020) follow.
7. **Importers** — `.kicad_pcb` Edge.Cuts (**0017 done**) + SVG board outline **done**;
   footprint graphics (**0016**) is the remaining one (builds on text + 0020).

## 8. Open items

- ~~Precision policy for the angle→quaternion lowering~~ — **resolved**:
  `ORIENT_ANGLE_SCALE = 1e6` (≈1e-6 rad; see `doc::Orient::from_angle_deg`).
- **Real non-copper layers (0020)** — **decided (Decision 13, 2026-07-02)**:
  geometry-layer identity is a slab *name*; projections are queries, never inputs;
  no negative layers (mask = solid + `Void` deletion volumes). Implementation spine
  in progress. Pairs with 0016 (footprint graphics are side-relative).
- **Trace/via slab-name migration** rides with 0011 (route serialization): serialized
  routes reference slab names; `route::Layer` ordinals become router-internal only
  (Decision 13 rule 2).
- Whether component bodies get a dedicated role/material or reuse `Keepout`.
- Relation to issue 0004 (planes / multilayer): the volumetric convergence is the
  natural home for that work.
- Coordinate-range / i128 ceiling (0018); polygon-courtyard solver packing (0019).
- This record still wants folding into `architecture.md` §8 (whose "Stages 1–3" prose
  predates the convergence — flagged by the §8 callout there).
</content>
</invoke>
