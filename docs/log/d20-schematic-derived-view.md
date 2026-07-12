---
id: d20
title: "The schematic is a derived view: authored flow layout, tags-first wires"
date: 2026-07-04
status: implemented (2026-07-04, main `468fe23` — branches feat/block-syntax, feat/schematic-model, feat/schematic-render, feat/schematic-capstone; §20e symbol body graphics deferred)
---

> Context: restated in [architecture.md §3](../architecture.md#3-schematic-front-end-connectivity-is-truth-drawing-is-a-view).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 20 — the schematic is a derived view: authored flow layout, tags-first wires (2026-07-04, implemented same day — see Status header)

Opens the schematic front. The conventional flow — draw a schematic, generate the
netlist from the drawing — is the finger-painting failure mode this project was
founded against, and we reject it structurally: **the schematic is the second derived
projection of the generative truth** (the flat netlist is the first, `Key::Netlist`;
the board is the third). The drawing is never authoritative. A future GUI wire-draw
gesture is not "creating a wire" — it is a gesture that means `ConnectPins`; the
command mutates truth, and the drawn wire then renders *because the netlist says so*.
Consequence stated as a feature: **forward/back annotation ceases to exist** — text,
schematic, and board are projections of one document and cannot disagree.

**20a — no solver on the view path (the Decision-18 lesson, applied).** Schematic
auto-layout in the classic sense (a placement/routing solver producing a diagram) is
hard, quality-uncertain, and never runs at view time. Instead the split is:

- **Authored: a structural layout tree** — nested containers with direction, symbols
  as leaves. This is intent and persists as **tier-1 native grammar directives**
  (siblings of `inst`, *not* a state zone — there is no solver output here), so it
  elaborates and gets real diagnostics (`E_` unknown path in the tree, `W_` part
  unplaced).
- **Derived: the coordinates**, by a pure, deterministic, terminating reflow of the
  tree — the computational class of *elaboration*, not routing. Milliseconds, same
  output every time, never serialized (the serializer contract: re-derivable state is
  not emitted).

The structural tree is also what makes the diagram **robust to netlist evolution**:
adding a fourth decoupling cap is "insert a child into the row" and the reflow is
least-change by construction — where absolute coordinates would shuffle or overlap.
The tree is the reconciliation unit.

**20b — the vocabulary is a deliberately tiny flexbox subset, with literal CSS
names.** Containers with `row`/`column` direction, `gap`, `align` (wrap TBD at
implementation), and one escape hatch: a **pinned offset within a container** (the CSS
absolute-positioning analog, reusing the provenance vocabulary). No cascade, no
styling, no percentages, no renaming cleverness — agents carry enormous training
distribution on exactly these names (the shadcn/react → damascene lesson: shape the
API to what agents are already fluent in, without the baggage). Symbol orientation is
an authored leaf attribute with a sensible default — no auto-orient cleverness in v1.

**20c — the view is total and honest from day one.** Any symbol not in the layout
tree renders in a derived "unplaced" bin (a plain grid); every connection renders as a
**named net tag at the pin**. The schematic never silently omits a part or a
connection — quality is added incrementally by authoring structure, never required
up front. Tags remain the default connection rendering even for placed symbols
(real schematics tag global rails and draw only local connections).

**20d — drawn wires ignore the routing problem.** A drawn wire in v1 is a straight
line or simple spline pin-to-pin. The author (human or agent) may add **waypoints**
("routing nodes") to visually direct a wire — pure presentation, a no-op to the
netlist truth downstream, letting an engineer draw their schematic however they like.
No wire autorouting, ever, on the view path; anything smarter is a future *editing
tool* proposing waypoints under Decision-18 semantics.

**20e — symbol bodies are boxes-with-pins in v1.** `.kicad_sym` body graphics import
later as additive `PartDef` data (the renderer keys on `PartDef`, so nothing here
excludes it); pins, names, and net tags carry the electrical content meanwhile.

A layout tree inside a `def` (Decision 21) is stamped per instance — reused circuits
render identically everywhere, the thing hierarchical-sheet tools never quite deliver.
