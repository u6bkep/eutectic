---
id: d19
title: "Planes are punchable: via-permeable derived pours, stitching vias, layer-honest pad incidence"
date: 2026-07-03
status: implemented (2026-07-03, PoC round-2 campaign — e.g. main `b214f3f`; 19a/19b in `autoroute/ingest.rs`+`search.rs`, 19c in `route/connect.rs`)
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Planes are punchable").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

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
