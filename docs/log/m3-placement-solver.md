---
id: m3
title: "Prototype status snapshot: M3 — deterministic least-change placement solver (and the M4-candidates list)"
date: 2026-06-28
status: snapshot (the fixed-iteration relaxation described here was replaced by the convergence-based solver — architecture.md "Prototype status (real solver)"; the M4-candidates list is a dated to-do whose items all landed)
---

> Context: current subsystem state is the "Prototype status" sections of architecture.md.
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

## Prototype status (M3)

M3 added a **deterministic least-change placement solver** (`solve` module), turning M2's decay
definition from a row-layout hack into the principled one: an override is *ineffective* iff freeing
it and re-solving lands it in the same place (within `PLACE_TOL` = 0.1 mm).

- Solver: relaxation / constraint-projection. Nodes start at their anchor and only move to satisfy
  constraints (unconstrained parts stay put — least change). Provenance sets movability:
  `Fixed`/`Pinned` are immovable anchors, `Hint` is a movable soft anchor, `Free` anchors at the
  generated default. Deterministic (no RNG, fixed iterations, f64 math rounded to integer nm).
- Constraints (`solve::Constraint` + `GenDirective`): `Board` (containment), `Near` (proximity),
  `MinSep` (clearance/non-overlap), `AlignX`/`AlignY`. Matches the doc's constraint stratification.
- Decay now generalizes: a hint the constraints would satisfy anyway decays (new `DecayReason`
  case folded into `RedundantWithDefault`); the whole M2 classification rides on top unchanged.
- Reconciliation re-solves per non-fixed override to test effectiveness, then does a final solve
  with decayed hints freed — so the committed placement is exactly what a post-GC re-elaboration
  produces (idempotent, stable diffs).

Tested (15 passing tests total) + `cargo run --example m3` (a mini RP2350-Zero-carrier placement:
module fixed at a datum, decouplers clustered near it, JST-SH headers in an aligned top-edge row,
all inside the outline; moving the datum perturbs only the decouplers — locality demonstrated).

**Note (M3 → M5):** M3's solver was a *fixed-iteration* relaxation — it satisfied a set of mutually
constraining relations only to within ~0.1–0.2 mm, with no convergence or feasibility guarantee
(300 sweeps, then stop and hope). **This has been replaced** by a convergence-based solver — see
"Prototype status (real solver)" below. The remaining honest limit is that it is still not a
research-grade general constraint solver (no DOF analysis / decomposition); it converges, satisfies
feasible sets tightly, and *reports* infeasibility rather than proving global optimality.

**Open limitations / next prototype targets (M4 candidates):**
- **Resolution UX** for conflicts/orphans now exists (see "Prototype status (resolution UX)"
  below); what remains is presenting it in a GUI and richer multi-issue batching.
- Solver now converges to a tight tolerance and reports infeasibility (see "Prototype status (real
  solver)" below), but still does no DOF analysis / subsystem decomposition and makes no
  global-optimum claim; no keepouts. (`Near`-to-a-*pin* and a settable rotation/orientation
  attribute now exist — see "Prototype status (physical parts)" below; the solver still does not
  *optimise* over orientation.)
- Query dependencies are recorded explicitly, not auto-tracked; inputs are coarse
  (`conn_rev`/`geom_rev`/`route_rev`).
- **Routing representation + DRC now exist** (see "Prototype status (routing core)" below):
  provenance-tagged trace/via/layer facts live in the `Doc` (tier 2), routing commands
  mutate them atomically, and DRC is a tier-3 query (clearance, min-width, ratsnest). A
  **basic deterministic grid/maze autorouter now exists** (see "Prototype status (autorouter)"
  below): it writes `Free` trace DOFs as a *proposed transaction* on top of this representation,
  treats `Pinned` traces as fixed obstacles, and verifies clean against the DRC query. Still
  missing: rip-up/retry, topological/push-and-shove, and length/impedance matching.
- The end-to-end PoC target (a single-PCB chip-down rework of the RP2350-Zero SWD-probe carrier)
  needs: real parts/footprints with pin geometry, a netlist→placement→route flow, and fab output.
  **Footprint *geometry* import now exists** (see "Prototype status (footprint import)" below): real
  KiCad `.kicad_mod` files (incl. the PoC's JST-SH headers and the QFN ICs) parse into `PartDef`s
  with per-pad pin offsets. **Electrical roles now exist too** (see "Prototype status (symbol/role
  layer)" below): a `.kicad_sym` *symbol* supplies the functional pin names + electrical types that a
  footprint lacks, and the two are joined by pad number into a real `PartDef` with mapped `PinRole`s.
  **Netlist and placement export now exist too** (see "Prototype status (export)" below): the
  connectivity and pick-and-place artifacts a board is checked/assembled against are emitted
  deterministically from a `Doc`. The **router** now exists (see "Prototype status (autorouter)"),
  and **Gerber/drill output now exists too** (see "Prototype status (Gerber/fab output)"): RS-274X
  per copper layer + `Edge.Cuts` + an Excellon drill program, emitted deterministically from routed
  copper, with footprint pads flashing as copper, plus copper-pour region fills and solder mask. (Pad
  copper is now *real* geometry that DRC checks edge-to-edge — see §8 — not a render-only point; only a
  roundrect/custom pad's Gerber *aperture* is still a conservative bounding box.) It is **not yet
  validated against a real Gerber viewer**. What's still missing for the PoC:
  typed `InterfaceDef`s inferred from symbols (the join produces discrete roled pins, not interfaces
  yet), and serializing routes in the canonical text projection.
