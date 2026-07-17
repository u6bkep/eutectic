# Project log — decision records & historical snapshots

The in-repo history of this project: dated decision records, milestone status
snapshots, and superseded plans, one entry per file (the same pattern as the
issue tracker). Design docs (`architecture.md`, `gui-architecture.md`,
`renderer-spec.md`) are **crisp, declarative, current-state only**; this
directory is where the dates live.

**The protocol, going forward:**

1. **A new decision = a new `dNN` file here PLUS a declarative edit to the
   owning design doc.** The log file carries the ruling verbatim — the
   reasoning, the rejected alternatives, the date, the implementing
   commit/branch. The design doc states the resulting model in present tense
   and, where provenance matters, cites the log inline ("ruled in \[dNN\]").
2. **Design docs never accumulate dated entries again.** No datelines, no
   "implemented same day, main `abc123`" breadcrumbs, no decision numbers in
   headings. Status snapshots, execution ledgers, and superseded plans get an
   `mN`/`nNN` file here instead.
3. **Entries here are records — never rewritten**, only added to (e.g. a
   status-line update in frontmatter when a ruling lands). Superseding a
   ruling takes a new `dNN` file that names what it supersedes.

Frontmatter per entry: `id`, `title`, `date`, `status` (with the
implemented-on-`main` commit where one was recorded).

## Decisions

| ID | Date | Status | Entry |
|----|------|--------|-------|
| [d01](d01-feature-single-currency.md) | 2026-06-30 | implemented | `Feature` is the single physical-geometry currency — `BoardShape`/`PadGeo`/`RegionDecl` become sugar or derived views over role-tagged prism features |
| [d02](d02-z-authoritative.md) | 2026-06-30 | implemented | z is authoritative now — vias/through-pads are tall conductor prisms; clearance gates on z-overlap, not a layer enum |
| [d03](d03-reserved-not-built.md) | 2026-06-30 | standing policy | The volume fence: `Extent::Solid`, non-box z-profiles, and true-3D solvers stay reserved; representation volumetric, solvers 2.5D |
| [d04](d04-connectivity-authoritative.md) | 2026-06-30 | implemented | Connectivity is never derived from copper geometry; slices/fills are derived fab views |
| [d05](d05-geometry-tracked-fold.md) | 2026-06-30 | implemented | Geometry is a demand-driven tracked fold of compact authoritative records; the prism soup is derived, never stored |
| [d06](d06-integer-quaternion-orient.md) | 2026-06-30 | implemented | Orientation is an exact integer quaternion — no mirror flag, no trig; bottom-side is a rotation, "which side" is derived |
| [d07](d07-derived-geometry-rounded.md) | 2026-06-30 | adopted invariant | Derived world geometry is correctly-rounded, never authoritative; predicates are tolerance-aware under non-cardinal rotation |
| [d08](d08-3d-view-first-class.md) | 2026-06-30 | adopted stance | The 3D view is the natural primary view; the 2D top view is a locked projection of it |
| [d09](d09-text-string-authoritative.md) | 2026-06-30 | implemented | Text: authoritative string + font + transform; strokes are a derived cache — no `Text` variant in `Shape2D` |
| [d10](d10-courtyard-polygonal-truth.md) | 2026-06-30 | implemented | Courtyard: real polygon truth, cheap solver proxy, honest verify of the result |
| [d11](d11-content-addressed-library-refs.md) | 2026-06-30 | adopted (model) | Library references are content-addressed handles + instantiations — never a bare path, never expanded geometry (incl. 11a: source-agnostic refs, the cargo-for-ecad door) |
| [d12](d12-phase0-foundation.md) | 2026-06-30 | implemented | Phase-0 foundation: net is an annotation beside `Feature`, `Stackup` goes live in `Source`, `PadGeo` derives features |
| [d13](d13-slab-name-identity.md) | 2026-07-02 | implemented (`869f458`) | Layer identity is a slab *name*; projections are queries, never inputs; no inverse projections; no negative layers |
| [d14](d14-refdes-derived-class-registry.md) | 2026-07-02 | implemented (`659d82a`) | Refdes/label are derived display; params are authored strings; one class registry holds prefix/template/defaults |
| [d15](d15-paste-derived-fab-slab.md) | 2026-07-02 | implemented (`659d82a`) | Paste is derived at export; fab is an ordinary authorable zero-height `Datum` slab |
| [d16](d16-area-unified-producer.md) | 2026-07-03 | implemented (`a6e389d`) | `Shape2D::Area`, the hole/void rule, one `world_features` producer, the prismatic-matter assumption named |
| [d17](d17-ttf-outline-text.md) | 2026-07-03 | implemented (`a6e389d`) | TTF outline text rides `Area`; `ttf-parser` accepted as the first dependency; stroke font stays the default |
| [d18](d18-routes-persisted.md) | 2026-07-03 | implemented (`a6e389d`) | The autorouter is an editing tool; routes persist in the `# routes` state zone with slab names + provenance; load never re-solves |
| [d19](d19-punchable-planes.md) | 2026-07-03 | implemented | Planes are punchable: foreign derived pours are via-permeable, same-net fills are stitching targets, pad↔plane incidence is layer-honest |
| [d20](d20-schematic-derived-view.md) | 2026-07-04 | implemented (`468fe23`) | The schematic is a derived view: authored flexbox-subset layout tree, deterministic reflow, tags-first wires, no solver on the view path |
| [d21](d21-source-language-core.md) | 2026-07-04 | implemented (`468fe23`) | The source language: declarative core, hermetic non-Turing-complete expressions, `def` reuse; computation stays at the rim (the Onshape clause) |
| [d22](d22-route-identity-persists.md) | 2026-07-07 | implemented (`8f7c1ec`) | Route identity persists: small-integer ids serialize in the state zone, lenient parse, one engine allocator |
| [d23](d23-schematic-features-tier.md) | 2026-07-09 | items 1–4 implemented (`669b2f7`) | The schematic realized-geometry tier: `schematic_features` is the one home for drawing conventions; symbol artwork is a seam; footprints and symbols are one thing in two vocabularies |
| [d24](d24-ui-usability-rulings.md) | 2026-07-16 | implemented (`8d1d71d`) | UI usability rulings: the oracle owns the tool strips (supersedes Select-only schematic), showcase opens by default, explorer rows show values; wave 1 delivered; the hotkeys-beat-text-inputs gotcha |

## Analyses, plans, ledgers

| ID | Date | Status | Entry |
|----|------|--------|-------|
| [n01](n01-geometry-fracture-finding.md) | 2026-06-30 | historical analysis | The fracture finding (four parallel geometry types vs. one designed primitive) and the consumer survey (two complete clearance models, the target one dormant) that motivated d01–d12 |
| [n02](n02-convergence-plan.md) | 2026-06-30 | executed | The convergence plan: sequential foundation → parallel fan-out → sequential spine, and the post-convergence steps |
| [n03](n03-convergence-status-ledger.md) | 2026-06-30 → 07-09 | closed ledger | The running status header of the retired decision record — one dated paragraph per implementation batch, with commits and test counts |
| [n04](n04-convergence-open-items.md) | 2026-07-03 → 07-09 | closed ledger | The open-items ledger (§8 of the retired record): implementation outcomes for d06/d10/d13–d18/d22/d23, the router-honesty rework, refdes pinning; reconciled 2026-07-11 |
| [n05](n05-region-kernel-record.md) | 2026-06-29 → 06-30 | historical record | How the region kernel (exact-integer booleans + dilation offset, issue 0004) was built and proven, stage by stage |
| [n06](n06-arc-support-record.md) | 2026-06-30 | historical record | How `Shape2D` gained circular arcs (strategy A: arcs authoritative, kernel sees a transient flattening), 5 stages |
| [n07](n07-original-roadmap.md) | 2026-06-28 | overtaken | The original build order (engine core first, GUI/router deferred) |

## Milestone snapshots

| ID | Date | Status | Entry |
|----|------|--------|-------|
| [m1](m1-engine-core.md) | 2026-06-28 | snapshot | M1: the engine-core vertical slice — typed interfaces, ERC-as-query, incremental engine, reconciliation, atomic transactions |
| [m2](m2-override-decay.md) | 2026-06-28 | snapshot | M2: override decay and reconciliation precedence — the Fix > Pin > Hint > default ladder, `ReconReport` |
| [m3](m3-placement-solver.md) | 2026-06-28 | snapshot | M3: the fixed-iteration least-change placement solver (since replaced by the convergence-based solver) and the M4-candidates list |
