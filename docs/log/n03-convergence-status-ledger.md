---
id: n03
title: "Convergence status ledger (the running header of the decision record)"
date: 2026-06-30 → 2026-07-09
status: closed ledger (verbatim snapshot of the retired doc's Status header; commits and test counts as recorded at the time)
---

> Context: one dated paragraph per implementation batch; the per-decision records are [d01](d01-feature-single-currency.md)–[d23](d23-schematic-features-tier.md).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

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
**Decisions 20 + 21 IMPLEMENTED end-to-end (drafted and landed 2026-07-04, `main`
@ `468fe23`, 559 lib tests, seven branches in four waves)** — the schematic front:
nested block grammar with trivia-preserving interiors (feat/block-syntax); typed-
interface inference at the import join, identity-unified on pad numbers — **0010
resolved** (feat/iface-infer); the hermetic expression tier — `param`, decimal-exact
arithmetic, `inst path[lo..hi]` ranges, `if=` variants, depth-bounded (feat/expr-tier);
the `schematic { row/column/sym }` layout tree + deterministic reflow + unplaced bin
(feat/schematic-model); `def` reuse with typed ports on pad identity, E_DEF_*
diagnostics, DNP prefix rule (feat/def-construct); the SVG renderer with net tags,
presentational `wire … via (x,y)` polylines, and the **0029** fix
(feat/schematic-render); def-embedded fragments stamped per instance, refdes headers,
and the RP2350A capstone schematic — `poc/out/schematic.svg`, all 44 components
placed, round-trip byte-lossless with schematic blocks (feat/schematic-capstone).
Every branch adversarially reviewed pre-merge; majors caught and fixed: expr
stack-overflow abort, quoting escape hatch, schematic MAX_COORD bypass, rot=90/270
stub detach, dual pad identity (blocker), deep-DNP asymmetry, silent internal-net
merge. Round-3 findings ledger (F1–F10 + F-def-fit) in `poc-rp2350-result.md` —
layout quality (pin-label-blind packing, tag overprint, wires through bodies) is the
named next frontier. Interface-typed def ports descoped (fails loud); `.kicad_sym`
body graphics deferred (§20e); comment-trivia normalization filed as 0030.
