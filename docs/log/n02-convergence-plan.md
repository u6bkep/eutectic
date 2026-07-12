---
id: n02
title: "Convergence plan: sequential foundation → parallel fan-out → sequential spine"
date: 2026-06-30
status: executed (2026-06-30; every phase and post-convergence step landed on main — see [n03](n03-convergence-status-ledger.md))
---

> Context: the model the plan built is [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

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
