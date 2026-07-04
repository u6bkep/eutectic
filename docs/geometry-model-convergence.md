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
**Decision 13 implemented end-to-end (2026-07-02, `main` @ `869f458`, 258 lib tests):**
the slab-name spine (`feat/slab-identity` — `RegionDecl`/`Text` carry slab names, hard
`E_UNKNOWN_SLAB`/`E_POUR_NON_COPPER` commit diagnostics, 7-slab default stackup,
`Role::MaskOpening`→`Mask`); the mask model (`feat/mask-model` — board-area `Mask`
solids per mask slab, pad-opening `Void`s resolved by role+z-adjacency
(`top_mask`/`bottom_mask`), `full_z()` through-cuts for drills/cutouts); KiCad footprint
graphics = **0016 resolved** (`feat/fp-graphics` — `PartDef.graphics`+`courtyard`,
side-relative `swap_side`, silk polygons render filled); and the export forward-query
rework (`feat/export-slabs` — model-derived mask Gerbers incl. cutout openings, real
per-Marking-slab silk Gerbers, `z_to_layer` + dead `DesignRules.mask_expansion`
DELETED, side-aware SVG silk). **0020 resolved.** Every branch got an adversarial
sub-agent review; findings (mask-side-by-name asymmetry, invisible silk polygons, a
Gerber modal-state blocker, cutout-sourcing purity, mask filenames) all fixed pre-merge.
**Decisions 14+15 implemented end-to-end (2026-07-03, `main` @ `659d82a`, 296 lib
tests):** `feat/datum-slabs` (authorable `datum` role, zero-height slabs, Datum
excluded from DRC structurally, graphic `Role` from the resolved slab, KiCad fab-graphic
import); `feat/class-registry` (`Component.params`/`label` + `inst`/`class` grammar,
`src/quantity.rs` decimal-exact SI/IEC parse+format, `src/annotate.rs` class registry +
derived refdes/effective-params/label queries); `feat/auto-text` (`FpText`
Reference/Label/Literal anchors on `PartDef`, shared `font::text_strokes` lowering with
ink-box Center justification, footprint text through `to_world` — mirroring from the
quaternion — into both silk export paths, KiCad `fp_text` + v7 `property` import incl.
`${REFERENCE}`/`${VALUE}` → live anchors, lowercase case-fold + Ω/µ glyphs). Every
branch adversarially reviewed pre-merge; all findings fixed.
**Bottom-flip convention fixed (2026-07-03, `feat/flip-axis`)**: `Orient::flipped()`
is Ry(180) (x-negates — KiCad/fab board-turn convention; bottom silk reads upright);
placement CSV reports the authored angle for bottom parts (KiCad `.pos` style, `0,B`).
**Decisions 16+17+18 implemented end-to-end (2026-07-03, `main` @ `a6e389d`, 385 lib
tests, eight branches in three waves)**: a geometry-currency audit found four bypasses
(pours' `PourFill` side-channel, via drills as scalars, `excellon_drill` missing pad
drills — issue 0022, and two parallel `Feature` producers leaving keepouts
unenforced — issue 0023). Decision 16 (`Shape2D::Area(Region)`, the hole/void rule,
one `world_features` producer for DRC + export, `BoardShape` superseded, the
prismatic-matter assumption named): branches feat/area-shape, feat/unified-features,
feat/fab-svg. Decision 17 (TTF outline text rides `Area`; `ttf-parser` is the crate's
first dependency; `W_FONT_LOAD` its first warning): feat/ttf-fonts. Decision 18 (the
autorouter is an *editing tool* — routes persist in the `# routes` text state zone
with slab names + provenance, load never re-solves, `PromoteRoutes` freeze; resolves
0011): feat/route-serialize, which also completed the Decision 13 rule-2 slab-name
migration (`route::Layer` is router-internal only). Alongside: refdes pinning
(feat/refdes-pin, Decision 14's reserved mechanism), polygonal-courtyard SAT packing
(feat/courtyard-pack, **0019 resolved**), and the i128 coordinate ceiling
(feat/checked-coords, **0018 resolved** — MAX_COORD ingest bound + KERNEL_SAFE_COORD
asserts). **0011, 0018, 0019, 0022, 0023 all resolved**; issue 0024 (copper-without-
mask lint) filed. Every branch adversarially reviewed pre-merge; all findings fixed.
**Mechanical batch (2026-07-03, `main` @ `e4996ae`, 395 lib tests)**: fab Gerber
output (`gerber_fab` via a shared `gerber_role_surface`, board-frame/no-mirror,
branch feat/fab-gerber); `W_COPPER_NO_MASK` lint on the shared `top_mask`/
`bottom_mask` resolution, zero-mask stackups exempt (**0024 resolved**, branch
feat/mask-lint); TTF kerning from the legacy `kern` table, integer font-unit
accumulation (branch feat/kerning). **Folded into `architecture.md` §8
(2026-07-03)** — §8 now states the current model; this record remains the
decision-by-decision history.

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

### Decision 14 — refdes/label are derived display; params are strings; class registry holds the conventions (2026-07-02)

Auto-text (the 0016 follow-up) forced the question "what does a `Reference`/`Value`
text anchor resolve to?", and the answer exposed two needs that must not share a field:
**part identity** (exactly what is placed — for the BOM, and eventually simulation) and
**display** (what the silk says). The model conflates neither with the identity spine:
`EntityId` (the hierarchical instance path) stays untouched as source identity; a
reference designator is a *different namespace* — flat, compact, conventionally
prefixed, consumed by manufacturing-time humans — and is therefore **derived**, the
classic annotation pass recast as a query.

**Identity: `(part, effective params)`.** `Component` gains
`params: BTreeMap<String, String>` (empty for most ICs — an MCU's identity is its
`PartDef` name; a resistor's is its parameter set). Params are **authored strings at
rest** — the display-normal spelling (`4.7k`) is the source of truth, and **consumers
parse at their own boundary** (the label formatter today, simulation later, at which
point a commit-time `E_BAD_QUANTITY` diagnostic can arrive *for the params that
consumer reads*). No speculative type ontology: the key vocabulary approaches the
number of component kinds, and typed storage would have to re-format authored
spellings for display (owning SI-prefix formatting and drift between "what was typed"
and "what the silk says"). MPN/sourcing is a *later BOM-export resolution* of
(footprint, params) → orderable part — a lookup table in a future BOM module, not a
`Component` field.

**Display: `label: Option<String>`** on `Component` — optional, cosmetic, no identity
weight. Display derives from identity, never the reverse.

**The class registry** is one authored, seeded table keying everything conventional:

```
class → { prefix?, template?, defaults? }
```

- `class(comp)` query: `PartDef.class` override, else the leading alpha run of the
  part name (`R_0402`→`R`, `LED_0603`→`LED`), else `U`. One concept, two consumers.
- `prefix` (default: the class name itself) feeds the **refdes annotation query**:
  deterministic per-class numbering over components in path order. Insertion-unstable
  by accepted trade-off; the EntityId-keyed **override system is the reserved stability
  mechanism** (pin assignments when a board ships) — not built now, kept open.
- `template` feeds the **label query**: instance `label` (itself a template) →
  registry template → built-in `"{value}"`; if the rendered result is empty
  (referenced keys absent), fall through to the part name — one rule covers passives
  *and* ICs before any table entry is authored.
- `defaults` are class-default params (`R → tol=5%`); instance params override, and
  BOM identity uses the *merged* effective params.

**Template display semantics** keep the software unopinionated: `{key}` substitutes
verbatim (authored spelling wins); `{key:si:Ω}` and `{key:iec}` parse-and-render
(`2.6kΩ` vs `2R6` — the convention lives in the user's table entry). Parse failure
degrades to verbatim substitution, never an error. The quantity parser is the first
boundary-parser and the one simulation inherits.

**Text anchors** (the auto-text mechanism): `PartDef` gains `texts: Vec<FpText>` with
`kind: Reference | Label | Literal(String)` — an *anchor* (position, height, layer,
orient), never a frozen string, per Decision 9 (strokes derived) and the salsa
principle (refdes edits re-render; it's a query over component state). KiCad
`fp_text reference "REF**"` imports as a `Reference` anchor, discarding the
placeholder; `fp_text value` imports as `Label` (our vocabulary does not inherit
KiCad's identity/display conflation). Footprint text generates strokes in
footprint-local frame through the same `to_world` as graphics — bottom-side mirroring
falls out of the orientation quaternion with zero special-case code. The shared stroke
lowering gains **justification** (KiCad text is center-anchored, board text stays
left-origin; content is live, so the offset cannot be baked at import). Pen width
stays the `height/8` rule — KiCad's explicit thickness is not stored.

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

### Decision 18 — the autorouter is an editing tool; routes are persisted, non-derivable state (2026-07-03)

Resolves 0011 (route serialization) — and repositions the router while doing it. The
first design sketch treated routing as a derivation: hand routes as tier-1 `route`
directives, autorouted (`Free`) traces *not* serialized, re-derived by re-running the
router at load (mirroring placement). That assumption fails three independent ways:
the future router is a research-grade problem that must be free to be **stochastic**
(re-derivation would not reproduce the board the user reviewed); router speed must
never gate **document open/interaction time**; and the real workflow wants **partial
reroute** — select a few atrocious traces out of a mostly-good result and re-fire the
router at just those, in milliseconds. All three point the same direction:

**The autorouter is not a solver layer between source and output — it is a power tool
a user aims at some nets and fires.** Its output, once accepted, is *document state*
(closer to a GUI user drawing traces very fast than to elaboration). Consequences:

- **All routes persist in the text file.** Load = parse, never re-solve. The file
  remembers what the router did, so the algorithm may be stochastic, anytime-improving,
  or replaced wholesale without touching a single existing doc. No determinism
  requirement on the router, ever.
- **Provenance is what makes partial reroute low-friction.** Persisted traces carry
  it: hand-drawn/frozen = `pinned` (default in text), router-owned = `free` ("the
  router may rip this up and replace it"). Partial reroute = transactional rip-up of a
  selection (the `Transaction`/`Command::AddTrace` machinery the router already uses)
  + reroute of just those nets, with `pinned` traces as immovable obstacles. "Freeze"
  is flipping a tag on traces already in the file, not a snapshot operation.
- **Staleness is handled by checking, not re-deriving.** A source edit (moved
  component) may invalidate persisted routes; DRC and ratsnest gate on the real
  geometry, so stale routes surface as violations/unroutedness — honestly.
- **Serialization: routes join the state zone.** The text file already has two zones —
  the generative program, then `# overrides` (non-derivable state, persisted). Routes
  are a second state section, not generative directives; parse fills
  `doc.traces`/`doc.vias` directly, so elaboration never owns routes and re-elaboration
  cannot wipe them. The serializer contract is amended from "materialized state is
  intentionally not emitted" to "**re-derivable** state is not emitted" — routes are
  materialized but *not derivable* (expensive, stochastic, user-blessed), which is
  exactly why they persist. Placement's contract stays as-is (the relaxation solver is
  cheap and re-derives); if that ever stops being true it takes the same escape path.

```
# routes
route gnd F.Cu w=0.15mm (1.2, 3.4) (5.6, 3.4) (5.6, 8.0)   # pinned (default)
route gnd B.Cu w=0.15mm (2.0, 1.0) (2.0, 9.0) free          # router-owned
via   gnd (5.6, 8.0) drill=0.3mm pad=0.6mm                  # full-span default
```

Details: **slab names, not layer ordinals** (this is Decision 13 rule 2 / Decision 16
stage 3 landing — `Trace.layer`/`Via.from,to` migrate to slab-name storage;
`route::Layer` ordinals retreat to router-internal grid state); unknown slab /
non-copper slab / unknown net at parse → hard diagnostics (the `E_UNKNOWN_SLAB`
family); **no trace IDs in the text** (`TraceId`s minted at parse/routing,
session-local); via span defaults to full copper extent, explicit `F.Cu..In1.Cu` for
blind/buried when multilayer arrives; polyline-only paths now, the grammar does not
preclude arc segments later.

Accepted consequence, stated as a decision: **autoroute output causes file churn** —
running the router changes what serializes. That is the truth, not a bug: the file
records what the router did, which is what makes rerolling it safe; diffs stay
reviewable because `free` traces are labeled.

### Decision 19 — planes are punchable: via-permeable derived pours, stitching vias, layer-honest pad incidence (2026-07-03)

The PoC round-2 campaign produced the finding: with full-board GND/+3V3 pours on the
inner layers, the honest router routed **2/44** nets — not a router failure but a
semantics error. The router treated the pours' *current fills* as static blocked
copper, so a through-via (needing room on all four copper layers) had nowhere to
land, confining signals to the outer layers. But a pour is **derived and
self-knocking-out** (`fill = outline − ⋃(foreign_copper ⊕ clearance)`, Decision 16):
the moment copper lands, the plane retreats around it at exactly `min_clearance` on
re-derivation — the anti-pad is automatic and always has been. Treating the momentary
fill as a wall ignores that the wall regenerates out of the way. Industry models
exactly this: vias pass through planes and get anti-pads; planes yield.

Three parts, landing together so honesty and capability arrive simultaneously:

- **19a — foreign derived-pour fills are via-permeable.** A via may be placed within
  a foreign pour's fill; the knockout carves the anti-pad on re-derivation. The via
  still needs clearance from *authored/routed* copper on every layer — only the
  derived fill yields (the `is_pour ⟺ Shape2D::Area` invariant from Decision 16
  identifies it). Inner-layer *traces* through foreign planes stay blocked this
  round: legal in principle, but shredding planes with signal traces is a cost-model
  question for the fenced router-research cycle. Verification consequence: the
  self-check must judge proposed copper against pours *re-derived with that copper
  included* (the fill that will actually exist), not the stale pre-route fill.
- **19b — same-net plane fills are stitching targets.** For a net with its own pour,
  cells over the net's own fill islands count as already-connected tree membership;
  routing a pad to the plane is a via drop discovered by the ordinary search. Island
  multiplicity is judged honestly downstream (a fragmented plane leaves ratsnest
  islands), not papered over by the router.
- **19c — layer-honest pad incidence (closes PoC finding F1).** A pin joins a pour
  island only where its pad copper actually exists on that island's slab (SMD pad →
  its own slab; drilled pad → every slab its barrel spans). Today's XY-only
  incidence reports a top-layer pad as connected to an inner plane with zero
  stitching vias — connectivity optimism, the one direction this model never lies
  in anywhere else.

Consequence, stated as a feature: **plane health becomes a first-class, checkable
property.** Every via punched through a plane fragments it slightly; after 19c the
pour-island ratsnest (which already exists per-layer) honestly reports whether GND
survived its perforation as one island. The campaign's fenced question — does the
QFN fan-out need negotiated rip-up? — gets answered by re-measuring routed/44 under
these semantics, not before.

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
</content>
</invoke>
