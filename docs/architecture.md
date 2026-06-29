# ECAD-from-scratch: Architecture & Representation

**Status:** design of record, now substantially **implemented** as the `ecad-core` Rust prototype
(see [`../README.md`](../README.md) and the "Prototype status (...)" sections throughout this
document). This file captures the architecture converged on in design discussion *including the
open questions and hard parts* — the prose sections (§1–§6) are the reasoning; each "Prototype
status" section records what the corresponding code actually does and its honest limits. Treat it
as a living document, not settled dogma.

## Motivation

Three fundamental complaints with existing ECAD (KiCad as the reference point) drive this design:

1. **Near-total lack of symbolic representation, worst in schematics.** The lines on screen
   *are* the connections — description by finger-painting. Tolerable for layout, a footgun
   factory for schematics.
2. **No constraint-based drawing.** Especially painful in layout. After authoring a precise
   mechanical design in MCAD (Onshape/Fusion), you drop into ECAD and place parts by eyeballing
   raw floating-point coordinates, re-entering information you just specified.
3. **Instability.** Crashes easily; large parts of the programmatic API crash when driven by an
   agent. This is a *symptom* of a bad data representation — mutable, redundant, denormalized
   state with weak invariants and a scripting layer bolted onto a GUI-first object graph.

**Goal:** both interaction modes first-class — interactive GUI editing *and*
declarative/programmatic agent-driven editing — with the **agent-driven path arguably the more
important** of the two. Find the data representation that makes a useful implementation fall out
as a consequence.

## The core reframe

An ECAD suite is not a drawing program. It is a **compiler + a database + a constraint solver**,
with the GUI as one of several clients. Nearly every complaint above is downstream of legacy
tools having the opposite architecture.

The suite has two domains that drive largely separately but **share one connectivity truth**:

- **Schematic** — quick to edit/read, the effective documentation output, the thing you consult
  when writing software for the board. Sub-problems: symbol/pin management, organizing complex
  designs logically, making errors (e.g. serial TX/RX swap) *unrepresentable*.
- **Layout** — what the board physically is. Sub-problems: integrating mechanical information
  (connector positions, keepouts, height), placing components, routing traces in 2.5–3D,
  respecting manufacturing limits, detecting mechanical collisions, and electrical concerns
  (signal integrity, impedance, trace current capacity).

---

## 1. The three-tier data model (the whole game)

Do **not** model the document as "inputs → derived outputs." Model it as three tiers; the
interesting design work is entirely at the boundaries.

1. **Authoritative facts.** The netlist hypergraph, component instances, typed ports,
   constraints, and any hand-authored geometry. Minimal, non-redundant. This is what the source
   format serializes.
2. **Materialized solver state.** Solved placement coordinates, routed trace geometry. Outputs
   of solvers, but **persisted** — they are expensive to recompute, must be *stable* (not jumping
   each edit), and the human's hand-placement/hand-routing lives in this same tier as
   authoritative data.
3. **Pure derived cache.** Ratsnest, DRC violations, "what net is this pin on," rendered
   geometry, spatial indices. Recomputed on demand, never persisted as truth, free to discard.

### Provenance per degree of freedom — the unifying primitive

Every geometric DOF (a component's x/y/rotation, a trace's vertices) carries a value **and** a
provenance bit: **free (solver-driven)** or **pinned (user-authored)**.

- A solver consumes pinned DOFs *as constraints* and writes only free DOFs.
- "Pinning" is just flipping the bit — it turns an output into an input.

This single mechanism delivers several requirements at once:

- *"Leave the micro under-constrained in the center until something requires it to move"*: nothing
  pins it → it's a free DOF → the solver leaves it where it is.
- *Routing split (most traces auto, special ones by hand)*: autorouted traces are free/regen-able;
  hand-routing or locking a critical signal flips it to pinned, and the autorouter treats it as a
  fixed obstacle. Not a separate subsystem — one provenance bit on trace facts.

### Least-change solving (the constraint-UX antidote)

The placement solver is **not** pure constraint satisfaction. For free DOFs it has gauge freedom,
so it **minimizes change** (stay where you are) subject to hard constraints, plus optional weak
soft objectives (minimize ratsnest, stay near a datum). Under-constrained sketches don't explode —
they sit still and move minimally. This kills the "why did the solver fling it across the board"
failure mode and *is* the geometric fallback path — not a separate mode. Freehand = everything
pinned; full parametric = everything constrained; real work lives in the middle.

### Solvers as transaction-proposers, not owners

The placement solver, the autorouter, and a human dragging a part are the *same kind of actor*:
they read facts and **propose transactions** that write DOF values. The router does not "own" the
copper; it is a function `(netlist, placement, pinned-routes) → route transaction` that writes
only free trace DOFs. The GUI cannot tell (and shouldn't care) whether a route came from the
autorouter or a hand edit — only the provenance bit differs. This is how we keep KiCad's genuine
virtue (fast, drawing-like interactive routing) without "the line *is* the net": a fast geometric
route representation lives in the derived/index tier, but the *truth* is a materialized route fact,
stable-ID-linked to a net.

---

## 2. The engine core (the load-bearing layer)

### Command algebra + version DAG

- The **only** mutation surface is a command algebra. Primitives: assert/retract a fact, set a
  DOF value, flip provenance. A transaction is an ordered batch, validated against invariants at
  commit, applied **all-or-nothing**. Atomicity deletes KiCad's entire crash class — there is no
  half-applied mutation, because an invalid transaction never commits.
- The document is a **persistent (structurally-shared immutable) structure**; each commit yields a
  new version; history is a **DAG of versions**. This buys, for free: undo/redo, branching, and
  **replay/time-travel** — the last especially valuable for agent workflows (replay what an agent
  did, diff two attempts, branch-and-merge).
- The GUI lowers gestures to commands; the agent emits commands; the file is a serialization of
  facts (or a replayable command log). **One path, no privileged back door** — the absence of a
  separate scripting API that can reach states the GUI can't is the fix for the crash-on-API
  problem.

### Stable identity (unglamorous, load-bearing)

Every entity has a **stable persistent ID** independent of name, position, or array index. This
enables: agent references that survive edits ("the decoupler on U3's VDD"), back-annotation and the
schematic↔layout link (both tiers reference one ID space), and **3-way merge on IDs, not text
lines**. Hierarchical-path + stable-local-ID is a foundational invariant, not an afterthought.
(KiCad's UUIDs are the right idea, half-committed.)

### Incremental query engine (Salsa-style) for tier 3

Use a **demand-driven, memoized query system** (Salsa / Adapton / rust-analyzer lineage) for the
derived tier:

- **Inputs** = tier-1 authoritative facts. **Queries** = elaboration, netlist-from-pins, ERC,
  ratsnest, DRC, rendered geometry. Dependencies are *observed automatically* (recorded as queries
  read each other), not declared by hand.
- A global revision counter; each memoized value stores `changed_at` and `verified_at`. On change,
  a query is revalidated by recursively asking whether its dependencies' *values* changed; if none
  did, it's restamped without recompute. If recomputed, the new result is compared to the old —
  **early cutoff / firewall**: an edit whose semantic result is unchanged does not propagate.
- Consequence: edit cost is proportional to the *true semantic blast radius*, not project size.
  Nudging a component that doesn't change connectivity must not rerun ERC; moving a trace that
  stays within clearance must not re-run whole-board DRC. Laziness: only DRC the region in view,
  only re-render what's visible.

**Critical caveat:** Salsa-style auto-incrementality only works for **pure, deterministic queries
whose results compare cheaply for equality** — that is tier 3. It is the *wrong* tool for tier 2
(materialized solver state), because solver output is non-deterministic, expensive, and also
user-editable; you must never let a query engine "helpfully recompute" a route and hand back a
different valid one, vaporizing stable geometry. Tier 2 is therefore deliberately **not** a query:
it is persisted state, mutated by explicit "run the solver" transactions, with coarse,
fingerprint-based reconciliation. The merge/coherence problem lives entirely in the one tier the
elegant query engine refuses to manage.

### Rule checking (ERC/DRC/SI)

Express rules as **queries over the connectivity and geometry relations** (Datalog / incremental
view-maintenance flavor), maintaining the violation *set* incrementally. Layer this over the Salsa
relations rather than choosing one religion. Reach for differential-dataflow only if DRC
incrementality becomes the bottleneck — start simpler. Spatial queries (DRC, routing) ride a
maintained **spatial index** (R-tree/BVH) in tier 3.

---

## 3. Schematic front-end: connectivity is truth, drawing is a view

- Ground truth is a **typed hypergraph**: components are instances with *pins*; a net is a
  hyperedge joining a set of pins. Wire routes, symbol positions, label placement are a
  *presentation/annotation layer* that renders the graph — never authoritative for connectivity.
  The schematic cannot lie about what's connected.
- **Make illegal states unrepresentable via a type system on ports.** Pins carry electrical roles
  (power-in/out, push-pull, input, bidir, hi-Z), and connections are made through
  **interfaces/buses** — compound port types with directions. `uartA <=> uartB` maps tx→rx by
  role; connecting two outputs, or swapping a diff pair's P/N individually, becomes a *type error
  at elaboration*. ERC stops being a heuristic pass and becomes "does this typecheck."
- **Hierarchy = composition with typed ports.** A module is a function with typed I/O; instantiate
  it N times, parameterized. This is "organize complex designs logically."

Prior art for the semantic core: **atopile** (`.ato`), **tscircuit**, **SKiDL**; typed-interface
ideas from **Chisel/SpinalHDL** bundles and **Amaranth**.

---

## 4. Layout front-end: geometry is the solution to a constraint system

Stratified constraints (mirroring MCAD sketch-solver intuition):

- **Mechanical/geometric**, ideally imported *live* from the MCAD model as **datums** rather than
  re-entered. Board outline, mounting holes, connector positions, keepouts, height zones. A
  connector is "mated to the USB-cutout datum from the STEP model," not `(34.5, 12.0)`.
- **Relational placement**: decoupler within X mm of the IC power pin; matched pairs symmetric
  about an axis; a block placed as a rigid group.
- **Electrical**: impedance target → constrains width given the stackup; length-match group →
  constrains routed length; current → constrains min width.
- **Manufacturing (DRC)**: clearances, min trace/space, via rules — enforced *during* editing.

Routing reframes as: connectivity is already known (the netlist); routing is **finding a geometric
realization of known nets that satisfies the constraint set.** Mature approach: topological-first
(crossing order/topology) then push-and-shove to geometry (see **TopoR**, **FreeRouting**).
Realistic stance: make *interactive* constraint-aware routing excellent (impedance/length-aware
push-and-shove); treat full autoroute as aspirational. This is simultaneously the moat and the
biggest risk.

---

## 5. Source representation: model-as-truth, text as a projection

**Decision: the structured fact store is the single source of truth. Text is a deterministic
rendering of it — like the visual is. Edits from anyone go through the command algebra, never by
parsing freeform text.** (This is forced by the "command algebra is the sole mutation surface"
decision: a freely-parsed text file would be a second mutation surface, reintroducing the
API-back-door bug class.)

### The OpenSCAD problem, precisely

Two separate failures, only one about text:

1. **No direct manipulation** — you script and preview, can't grab the thing. A missing-GUI
   problem.
2. **Non-correspondence** — a `for` loop emits 100 holes; none exist as addressable entities in the
   source, so you can't click one and edit it. The program's structure doesn't correspond to the
   artifact's.

The cure for (2) is the stable-ID fact store: every entity is addressable. Lesson: the canonical
model must be **declarative and addressable**, never an imperative generator whose output you can't
point at.

### Don't build a text↔visual transform — build two views of one model

The way to make the text↔visual transform reliable is **to not have one.** No bidirectional sync
between a text file and a layout file (two truths = endless merge bugs). Instead, one model with:

- `render_text: model → text` (pure)
- `render_visual: model → pixels` (pure)
- `parse: text → commands` and `gesture → commands` (both lower to the one mutation path)

Text and visual are never converted into each other; they are two windows on one room. The
reliability problem *dissolves* — nothing to keep in sync. For this, the text projection must be a
**canonical serialization**: deterministic, normalized, ID-bearing, so re-rendering is byte-stable
and parsing is unambiguous.

### Generativity vs. addressability (the real tension) → elaboration + ID-keyed overrides

Agents and power users want concise generative description ("one decoupler per power pin"); but
generativity reintroduces non-correspondence. Resolution — the same compile-and-materialize move
that recurs everywhere:

- A **generative authoring layer** (concise, optional; modules, parameters, loops). Text-native.
- An **elaborated instance layer** — the result of running it — where every entity has a stable ID.
  This is what the GUI binds to and what layout attaches to. (Elaboration is a Salsa query.)

When the user direct-manipulates an elaborated instance a generator produced:
- Edits that map to a **parameter** update the source parameter.
- Edits that are **exceptions** to a generative rule are recorded as **ID-keyed override deltas**
  layered on elaboration. The generative source stays clean; an override store holds per-instance
  exceptions.

This is the *same pattern* as the provenance/pin bit in layout and the tier-1↔tier-2
reconciliation: **clean generative truth + ID-keyed override deltas + reconciliation.** It recurs
at the schematic-authoring level, the placement level, and the routing level. One hard problem to
solve well, not three.

### Who gets what

- **EEs**: a schematic/layout editor that looks like what they know; never have to read or write
  text. OpenSCAD's "not how I think" problem gone — fully visual, every object grabbable.
- **Agents**: a clean, concise, diffable text projection as native surface; canonical + ID-bearing
  ⇒ reliable, reviewable edits.
- **Team**: because text is deterministic, an EE can read a clean PR diff in review without text
  being their authoring tool — code-review workflows without code-authoring.

### Costs (honest)

1. **No formatting freedom in the canonical layer** — no arbitrary whitespace/comment placement/
   ordering. Comments/annotations attach to entities *by ID*, not as floating text, or they won't
   survive a re-render.
2. **The override/reconciliation layer is the hard heart.** "Edit a generator's output and have it
   stick, without the next elaboration clobbering it, and without over-eagerly pinning everything"
   is *the same problem* as the layout minimal-perturbation nudge-vs-pin worry. Design it **once**,
   carefully, as a first-class engine primitive.

---

## 6. Git interaction

KiCad is the best-behaved of existing tools only because its files are S-expression text (git can
diff them at all). It still hurts: UUID/coordinate churn → noisy diffs; schematic and PCB are
separate files that can desync on merge; PCB merges are effectively impossible; it commits derived
junk.

This architecture targets all four, three structurally:

- **Minimal semantic diffs** (from deterministic rendering): a one-component change is a
  one-component diff; no coordinate/UUID/reorder noise.
- **Identity-based diff/merge** (from stable IDs): reordering the serialized file produces no diff;
  renaming a net doesn't break references. The thing line-based git can't do alone.
- **No schematic↔layout desync** (from shared connectivity): connectivity is one shared truth;
  layout references it by ID; a merge *cannot* leave schematic and board disagreeing. An entire
  corruption class becomes inexpressible.
- **Tier split → git hygiene:** tier 1 committed & code-reviewed; tier 2 committed **like a
  lockfile** (a chosen solver solution you pin, not regenerated on checkout); tier 3 `.gitignore`d.
- **Visual diff for review**: holding both model versions + a pure render function lets us render a
  *graphical* diff ("what moved, what re-routed, what changed electrically").

**Honest hard part — tier-2 layout merge.** Improved, not eliminated. Two people routing the same
region conflict *spatially*; no text merge can reason about it. Mitigations enabled by our model:
- A **custom git merge driver** doing semantic 3-way merge *on the model* (conflicts surface as
  "these two routes overlap" — sometimes auto-resolvable by re-routing a free trace, sometimes
  needs a human; meaningful, not an unresolvable text hunk).
- Aspirational ceiling: **operation-based merge** (rebase/replay command sequences from a common
  ancestor, OT/CRDT-style) — only possible *because* commands are the sole mutation path. Flagged
  "later, maybe"; heavy, with its own semantic conflicts. Semantic merge driver is the realistic
  v1.

**Disciplines this demands:**
1. **Determinism is enforced, not free** — no wall-clock, no map-iteration-order leaks, no
   nondeterministic serializer ordering, or byte-stable diffs silently break. Tier-2 solver output
   *is* nondeterministic, which is exactly why it's committed lockfile-style (don't regenerate on
   checkout, or every pull re-routes the board).
2. **Genuinely binary assets** (imported STEP, meshes) → git-lfs; the model references them, never
   inlines them.

---

## 7. Error handling: structured diagnostics (the stability pattern)

Stability is a founding goal — "a crash or panic is not a pleasant user experience" — and it has
**two layers** that are easy to conflate:

1. **No invalid states.** The command algebra (§2) guarantees a transaction either commits a valid
   document or aborts whole. There is no half-applied state, so the crash-on-bad-API class is
   designed out. *This layer was already real;* the error model does not change it.
2. **How the reasons surface.** Every fallible operation yields **structured diagnostics** instead
   of panicking or returning flat strings. This is the layer this section defines, and the pattern
   every future subsystem follows. The north star is `rustc`: text that a human reads fluently *and*
   a tool can parse, and which reports **everything wrong in one pass**, not just the first fault.

**Audiences, and why there is only one rendering problem.** Agents work against the API and text
surfaces (CLI, scripting); humans work through a GUI that passes errors up to display (red
highlights, popups). Both consume the *same* structured value: the GUI reads its `Location` to
highlight the offending entity; the CLI/agent reads the rendered text. So the rule is **structured
internally, rendered at the edge** — never pre-formatted prose buried in a `String`.

### The vocabulary (`diagnostic` module)

- **`Diagnostic`** — `severity` + a stable, closed-set **`code`** (`"E_UNKNOWN_PIN"`,
  `"W_HINT_DECAYED"`) + a human `message` + a **`Location`** + optional `help`. The `code` is the
  agent/CLI parse anchor: tooling greps the code, so messages can be reworded freely.
- **`Location` is semantic-first.** Because of model-as-truth (§5), most issues locate by *model
  identity* — `Entity`/`Net`/`Pin`/`Trace`/`Via` — which is exactly what a GUI highlights. `Span
  { line, col }` is the textual location only the text front-end supplies (today: line number from
  the parser; column is best-effort).
- **`Severity` is seriousness, not blocking.** A pin-vs-constraint conflict is an `Error` (genuinely
  wrong, surfaced loudly, kept until resolved) yet rides on a *valid* document — it does not abort
  the commit that produced it. What decides blocking is the **channel**, not the severity.
- **`Diagnose` trait + `render`.** Domain results that callers consume as *data* (`route::Violation`,
  `doc::ReconReport`) stay typed and implement `Diagnose` → `Diagnostic` for *rendering*.
  `Diagnostic` is the presentation lingua franca, not a replacement for them. `render` emits
  deterministic rustc-style text (errors before warnings, then by code/location — byte-stable and
  diffable, which agents rely on).

### Two channels, one vocabulary

- **Hard faults** — a transaction that cannot build a valid model returns `Err(Vec<Diagnostic>)` and
  aborts atomically. **Collect-all**: elaboration accumulates every independent fault it can find in
  one pass rather than bailing on the first (`command::apply`, `elaborate`, `text::parse` all do
  this). The text parser reports every malformed line; elaboration reports every unknown pin/part.
- **Findings on a valid model** — reconciliation (`ReconReport`), ERC, DRC (`Key::Drc` →
  `Vec<Violation>`), and the floating-pad check (`Key::Floating`) attach diagnostics to a document
  that *did* elaborate. "Can this tape out?" = "are there any `Error`-severity diagnostics across
  either channel?" (`diagnostic::has_errors`).

### Error recovery / cascade suppression

Collect-all without recovery would spew noise. The policy (rustc's): an **independent** mistake is
reported individually (every unknown *pin* on a real component), but a **poisoned entity** — an
instance that failed to build or was never declared — is reported **once**, and every downstream
reference to it is suppressed (`elaborate`'s `reported_missing` set). So a single missing component
yields one diagnostic, not one per net that referenced it.

### Status

Implemented across the mutation/query path: `command`/`history`/`elaborate`/`text` (hard-fault
channel, collect-all), `query` ERC + floating + DRC-via-`Diagnose`, and `ReconReport`/`Violation`
`Diagnose` impls. The production panic surface is down to a handful of *invariant/misuse* asserts
(the `QueryValue::as_*` accessors, two solver "exists by construction" unwraps) — none reachable by
user/agent input. **Follow-ups:** (1) the `kicad` import layer still returns `Result<_, String>` —
a natural next application of the pattern (data-ingestion surface, not the runtime mutation path);
(2) real text spans (column tracking) in the parser; (3) fuzzy "did you mean" suggestions (the
`help:` line currently lists candidate names verbatim); (4) designing out the `as_*` accessor panics
via a typed query key.

---

## 8. Geometry: purposed regions (the physical model)

The physical side of the model — copper, the board body, holes, keep-outs — is **not** "2D shapes
on named layers" (KiCad's model). That is a *2.5D projection* of a 3D reality, and encoding meaning
as a magic layer name (`F.Cu`, `Edge.Cuts`, `F.CrtYd`) is stringly-typed semantics that doesn't
generalize. The reframe: **everything physical is a region of space with a purpose.**

### The unit

```
Feature { role: Role, material: Option<Material>, extent: Extent }
Extent  = Prism { shape: Shape2D, z: ZRange }   // the 2.5D common case
        | Solid { … }                            // reserved: arbitrary 3D (mesh/brep)
```

- **`Shape2D` = a skeleton inflated by a radius** (Minkowski ⊕ disc): `Stroke{points, r}` (a point⊕r
  = disc, a segment⊕r = capsule/oval, a polyline⊕w/2 = a trace) and `Polygon{points, r}` (r=0 sharp,
  rect⊕r = rounded rect, arbitrary filled). One shape type subsumes every pad primitive *and* traces
  *and* via annuli — clearance is uniform: `skeleton_distance(a,b) − rₐ − r_b ≥ clearance`, computed
  in exact i128 (the segment-distance kernel already used for traces). Compound pads (BMP581) are a
  *union of features*; clearance is the min over the union.
- **z is real**, backed by a **stackup** (named slabs with thickness + material; sensible defaults —
  1.6 mm board, 1 oz copper). **A "layer" is just a named z-slab**, never a primitive. Clearance is
  "roles have a rule ∧ z-ranges overlap ∧ 2D shapes within distance"; with discrete slabs "z overlaps"
  collapses to "same layer", recovering ordinary 2.5D behaviour — but the model isn't limited to it.
  Below-surface bodies (a module in a cutout, low-profile USB-C) live at *negative z*, which a fixed
  layer enum cannot express.

### Roles stay few and physical

`Conductor | Substrate | Void | Keepout(kind) | Marking | MaskOpening | Datum`. Richness comes from
**geometry + composition (footprints)**, not from proliferating roles — the rule that keeps this from
sprawling:

- *drill / board cutout / milled pocket* → `Void` (a drill is not special; it is one void among many)
- *board outline* → the boundary of a `Substrate` prism (an arbitrary CAD-imported polygon)
- *courtyard / mechanical clearance* → `Keepout` (3D extent, for interference detection)
- *fiducial* → a footprint with `Marking` + `MaskOpening` features (no new role)
- *mouse-bite* → a footprint with `Void` perforations and **no** `Conductor`
- *thermal relief* → a `Conductor` pad whose `Shape2D` *is* the spoke-and-gap geometry

The `Role` enum stays extensible, but we resist growing it: a named PCB feature is a composition over
the base set, not a new kind.

### Why this is the right foundation

- **2.5D is the default *view*, not the storage.** A normal project uses stackup defaults and edits
  in the familiar layer view; z is filled in automatically. A future 3D router/editor reads the same
  model without the 2.5D lens. **3D-printed (polymer/metal) boards become representable** via
  `Extent::Solid` — *reserved, not built*: the data model won't have to be thrown away, but the
  solvers (router/placement/DRC) stay 2.5D for now (true-3D solving is a research project).
- **Simulation falls out of honest geometry.** A `Conductor` carries real cross-section (width ×
  *thickness*, from the stackup z-range) and a `Material` (resistivity, permittivity, thermal), so
  trace resistance `R = ρ·L/A`, impedance, and thermal become computable later *from the same
  geometry* — no separate sim model. Design constraint: never discard thickness or material.
- **It closes complaint #2.** A CAD-imported outline + cutouts become tier-1 authoritative facts
  (`Substrate`/`Void`), exactly what constraint-based "fit the parts into this MCAD shape" reads from.
- **The open routing tickets become consequences, not separate fights:** pad-as-real-copper (0006)
  is just `Conductor` features with extent; the fine-pitch router lie (0003) goes away once its
  obstacle model and DRC share honest feature clearance; courtyard/overlap avoidance (0005) is the
  `Keepout` role.

### Status / plan

Design of record (this section). **Stages 1–3 implemented.** (1) the `geom` core;
(2) pads are real `PadGeo` copper + drill geometry, imported from KiCad
(circle/rect/roundrect/oval exactly, custom→bounding-box, with pad
rotation/drill/layers) and rendered to Gerber via bounding apertures; (3) **DRC
clearance is pad-aware** — all copper (traces, vias, pads) reduces to a world-frame
`geom::Shape2D` and a different-net pair sharing a layer is checked edge-to-edge by
`geom::clearance_violated` (**resolves 0006**; trace-near-pad-edge and pad-vs-pad
fine-pitch clearance are now visible, gated by the 2.5D `Layer` model). **Router
self-honesty done (resolves 0003):** the autorouter no longer trusts its
clean-by-construction invariant (which fails at sub-grid pitch / off-grid pad
stubs) — it verifies its proposed copper against the same pad-aware clearance and
drops any net that actually clashes, so `routed` means *verified clean*. (On the
PoC this honestly drops from a lying "19 routed / 5 violations" to "4 routed / 0
router-introduced violations"; the remaining clearances are all pre-routing
pad-pad placement issues — 0005.) **Placement overlap-avoidance done (resolves 0005):** each part has a **courtyard**
(`part::courtyard_half_extents` — the origin-centred bbox of its pad copper + a
margin; footprint-less parts have none and are exempt), and elaboration emits a
`solve::Constraint::NoOverlap` for every component pair so the solver pushes
overlapping courtyards apart (AABB min-translation, fixed parts immovable →
unresolvable overlaps reported). On the PoC this cuts the pre-routing pad-pad
clashes the honest DRC surfaced from **16 → 1** (the residual is a +3V3/GND pair the
*approximate* solver (0007) can't fully separate on a dense board, not an
overlap-avoidance gap). Courtyards are origin-centred symmetric boxes (tight for
origin-centred footprints, conservative otherwise) and the pass is O(N²) — noted
limits. **Board outline + cutouts done (Stage B2 — the MCAD-fit representation):** the board
is one `geom::BoardShape { outline: Shape2D, cutouts: Vec<Shape2D> }` — a `Substrate`
outline (rounded/concave/CAD-imported all expressible) with `Void` cutouts;
`board_rect(min,max)` is a constructor over it, not a parallel rectangle. Authored via
`GenDirective::Board { outline }` + `Cutout { shape }`, assembled by the shared
`elaborate::board_shape(&Source)` that the solver, autorouter, and export all read.
The solver containment is now polygon-aware (movable parts pulled inside the outline,
pushed out of cutouts — `BoardShape::contain`, approximate boundary projection); fab
export draws the real outline + cutout contours (Gerber `Edge.Cuts` + SVG); text
round-trips `board`/`cutout` (corner-radius serialization is a noted follow-up).
**Still pending:** the routing grid still spans the outline *bbox* (cells outside the
outline / inside cutouts are not yet masked — a small follow-up); the router's obstacle
model still blocks on pad *points* (so it drops more than a pad-extent-aware or
rip-up router would keep — 0008); full `ZRange`-stackup gating; `Solid`/true-3D. Implementation is staged: **(1)** the `geom` core — `Shape2D`,
`ZRange`, `Extent::Prism`, `Role`, `Material`, `Feature`, the stackup + defaults, and the 2.5D
clearance kernel (additive, self-contained); **(2)** pads → `Conductor` features + KiCad pad import
(smd/thru-hole/custom-primitives/drill/layers) + render; **(3)** unified feature clearance in DRC
(0006); **(4)** board outline / cutouts / keep-outs as features + router obstacle model (0005);
**(5)** router honesty as the downstream consequence (0003). `Solid` and true-3D solving are out of
scope; the representation merely keeps the door open.

### Copper pours / solder mask: the region kernel (0004)

A copper pour, a solder-mask layer, a paste stencil, and a keep-out-aware fill are **one operation**:
*offset some shapes, then boolean-combine regions*. A pour is `zone − ⋃(foreign_copper ⊕ clearance)`
(with same-net thermal spokes); a mask is `⋃(pad ⊕ mask_expansion)`; paste is the same with a
reduction. So instead of a one-off "pour" feature we build the shared **offset + polygon-boolean
kernel** once (`src/region.rs`) and let every consumer fall out of it.

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
  `union`, so there is exactly one boolean engine. Arcs are tessellated at a fixed fine resolution
  (integer direction table; the only float is the correctly-rounded IEEE `sqrt` for an edge normal,
  matching the `closest_on_segment` precedent). The fill is a **derived (tier-3)** result, so this
  tessellation is never baked into stored state — keeping the door open to arc-exact boundaries later
  without a data migration.

**Stage 1 done:** the `region` kernel — `Region`, `union`/`intersection`/`difference`,
`shape_to_region` (offset via dilation), and exact-integer predicates — landed standalone with a
degenerate-case test suite (shared edges, corner-touch, concave dilation, multi-knockout pours,
containment edge cases, determinism). **Stage 2 done:** the region *primitive* — an authored
`elaborate::RegionDecl` (`Shape2D` + `Role` + optional `net` + copper `Layer`), exposed as a
`GenDirective::Region`, assembled by the shared `elaborate::regions(&Source)` reader (mirroring
`board_shape`), and round-tripped by the text front-end (`region <role> [net=..] layer=.. <pts>`, with
keep-out kinds and inner layers). It is tier-1 authoritative; the knockout fill stays derived.
**Stage 3 done:** the **derived pour-fill query** — `route::pour_fills(...)` computes, for each
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
(holes read as voids), under the components/traces. The shared `export::pour_fills_of` builds the
membership netlist and calls `route::pour_fills`, so DRC and fab see identical fills. Tests: Gerber
emits `G36`/`G37` with outer + knockout-hole contours (bottom layer has none); SVG draws the pour
path; fab output deterministic with a pour. **Scope note:** the custom-pad / rounded-outline
bounding-box-collapse fidelity debt was *not* repaid here — true `G02`/`G03` arc export needs the
arc-capable `Shape2D` (the deferred representation extension), and routing complex *pads* through
region fills would churn the existing aperture-flash path; both are left as focused follow-ups (the
bounding-box pad flash is conservative, not a regression). **Stage 6 done (the family is complete):**
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
orbit. The DRC pass is `O(N²)` (broadphase spatial index deferred — see performance notes); arc-exact
boundaries and the 3D-`Solid` boolean are deferred but representable. (Noted limits: custom-pad /
rounded-outline still flash as bounding boxes — awaits arc-capable `Shape2D`; floating/unnetted pads
not yet knocked out of a pour; SMD-pad↔pour incidence is all-layer like the rest of the pin model;
Gerber not yet viewer-validated — 0009.)

---

## Open questions / hard parts (carry these forward)

- **The reconciliation / minimal-perturbation engine** is the single load-bearing risk. Precise
  semantics of: least-change solving, override decay (when does a stale override get dropped vs.
  surfaced?), explicit vs. inferred pins, and "nudge without the next rebuild moving it back, but
  don't pin over-enthusiastically." Same primitive at schematic-authoring, placement, and routing
  levels.
- **Constraint-solver UX at board scale** — over/under-constrained diagnostics, locality (solve
  regions independently), no-solution explanations.
- **Routing** — the genuinely unsolved part; interactive-first, autoroute aspirational.
- **Part library** — hard because of *scale*, not difficulty. Plan: import KiCad's existing
  detailed libraries, add type information via good import tools + agent-in-the-loop editing.

## Prior art to mine

- **Horizon EDA** — modern C++ EDA with a more database-like model and a single shared "pool"; the
  closest existing thing to this direction. Mine for what worked / didn't.
- **atopile, tscircuit, SKiDL** — declarative/code schematic capture.
- **Chisel / SpinalHDL / Amaranth** — typed interfaces/bundles, elaboration.
- **Salsa / Adapton / rust-analyzer** — incremental demand-driven query engines.
- **TopoR / FreeRouting** — topological routing.
- **Onshape/Fusion sketch solvers** — geometric constraint solving, DOF analysis, least-change.

## Roadmap (historical)

> This was the original build order. It has since been overtaken by events: the engine core, the
> placement solver, KiCad import, routing + DRC + a basic autorouter, and fab export are all built.
> See the "Prototype status (...)" sections below and [`../README.md`](../README.md) for the current
> state; the items here are kept to show the sequence the work actually followed.

1. **This document** — the synced design of record. ✅ done
2. **Prototype the engine core only** — fact store + command algebra + Salsa-style query layer +
   the reconciliation/override engine. Not the GUI, not the router (large but architecturally
   conventional). The novel, load-bearing risk is the data engine; prove or break the central bet
   first. ✅ done — see "Prototype status (M1)" below.
3. **Prior-art pass on Horizon EDA's data model** specifically. (Sources cloned in `reference/`.)

## Prototype status (M1)

A zero-dependency Rust crate (`ecad-core`, edition 2024) implements a full vertical slice of the
engine core. Decisions locked in during prototyping: **hand-rolled** incremental query engine (not
the `salsa` crate); `BTreeMap` for deterministic/canonical serialization (persistent `im` maps are
the production swap); entity id = hierarchical path string for M1 (opaque-handle + path table is
the production refinement).

Modules: `id`, `part` (typed pins/interfaces), `doc` (three-tier immutable model + provenance
DOFs), `elaborate` (generative source → instances + ID-keyed override reconciliation), `command`
(atomic transactions, the sole mutation surface), `history` (version DAG), `query` (hand-rolled
memoized engine with dependency tracking + early cutoff), `project` (deterministic text view).

Demonstrated & tested (6 passing tests + `cargo run --example m1`):
- Typed interface connection auto-crosses UART tx↔rx — the serial swap is unrepresentable.
- ERC as a query over pin roles (catches multi-driver contention).
- Incremental engine: a geometry nudge skips Netlist+ERC entirely (dependency-skip); adding an
  unconnected component recomputes Netlist but skips ERC via **early cutoff** (value unchanged).
- Generative reconciliation: a pinned override survives the generator growing 3→5 caps (minimal
  perturbation); shrinking 5→1 surfaces the orphaned override as a conflict, never silently dropped.
- Atomic transactions (a bad source leaves head untouched); version-DAG undo.

## Prototype status (M2)

M2 attacked the load-bearing risk directly: **override decay and reconciliation precedence.**
"Decay" is defined concretely, not as intent-guessing — an override is *ineffective* iff removing
it yields the same final position (this generalises to "doesn't change the solved result" once a
real solver exists).

Model added:
- **Override strength** (`doc::Strength`): a `Nudge` records a weak **Hint**; an explicit `Pin` is
  strong. ("Don't pin over-enthusiastically.")
- **Hard constraint** (`GenDirective::Fix`): mechanical-domain placement (a connector mated to a
  datum). **Precedence: Fix > Pin > Hint > generated default.** Provenance ladder
  `Free < Hint < Pinned < Fixed`.
- **Decay / reconciliation rules**, emitted as a structured `doc::ReconReport` (no more ad-hoc
  strings): an ineffective Hint is **garbage-collected** at commit; an ineffective Pin is **flagged
  but kept**; a Pin contradicted by a Fix raises a **loud conflict** (kept until resolved); a Hint
  contradicted by a Fix **yields silently** and decays. Strength = how loudly an override objects.

Tested (10 passing tests total) + `cargo run --example m2`: redundant-hint decay+GC,
hint-yields-to-constraint, pin-conflicts-loudly-and-kept, redundant-pin-flagged-not-dropped, plus
the M1 suite still green under the new semantics (a nudge is now a Hint that survives while
effective).

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

## Prototype status (text front-end)

The `text` module makes §5's "text as a projection" concrete: a **canonical serializer + parser**
for the *authoritative* tier-1 state (the generative `source` directives **and** the ID-keyed
`overrides` map). This is the agent/git-facing authoring surface — *not* a synced second artifact.
`serialize` and `parse` are the two halves of one projection; materialized positions/nets are
deliberately **not** serialized (they are derived and re-elaborated on load — `project` renders
those for viewing).

**Grammar** — one directive per line, `#` line comments, whitespace-tokenized, coordinates `(x, y)`:

```text
inst    <path> <part>                 place  <path> (<x>, <y>)
fix     <path> (<x>, <y>)             board  (<x>, <y>) (<x>, <y>)
near    <a> <b> <len>                 minsep <a> <b> <len>
alignx  <node> ...                    aligny <node> ...
connect <compA>.<port> <compB>.<port> net    <name> <comp>.<pin> ...
hint    <path> (<x>, <y>)             pin    <path> (<x>, <y>)   # ID-keyed overrides
```

It covers every `GenDirective` variant and both override strengths (`hint` = weak/`Hint`,
`pin` = strong/`Pin`). Lengths accept `30mm` (decimal ok), `30000000nm`, or a bare integer (nm);
a `<comp>.<pin>` reference splits at the last dot so hierarchical paths (`psu.dec[0].p1`) survive.

**Guarantees (tested — 14 new unit tests, 29 total):**
- *Deterministic / canonical:* `serialize` is a pure function with stable output — source
  directives in source order (instance order is itself tier-1 truth, driving default placement),
  overrides in `BTreeMap` id order, every coordinate in one canonical mm form.
- *Idempotent:* `serialize(parse(serialize(doc)))` byte-equals `serialize(doc)`.
- *Round-trips:* `parse(serialize(doc))` reproduces `(source, overrides)` exactly; re-elaborating
  it reproduces the same `components`/`nets`/`report` (verified on `psu_module`, the UART-link
  design, and a Board/Near/MinSep/AlignY/Fix scene).
- *Tolerant in, canonical out:* mm/nm/bare units, comments, and extra whitespace all parse; output
  is always the one canonical form. Parse errors return `Err(String)` naming the offending line —
  never a panic.

`Command::LoadText(String)` lowers the text front-end onto the sole mutation surface: it parses and
replaces source+overrides in one atomic transaction (a malformed document aborts the commit, so the
file is never a back door to an inconsistent state). Zero new dependencies — the parser is
hand-rolled (line-based).

## Prototype status (physical parts)

Gives parts real planar geometry so proximity constraints can target an actual pin, not just a
component centroid. Still zero-dependency.

- **Pin offsets.** Every discrete pin (`part::PinDef.offset`) and every interface signal
  (`part::InterfaceDef.offsets`, keyed by signal name) carries a local 2D offset (`doc::Point`, nm)
  from the component origin. `PartDef::pin_offset(pin)` resolves a reference (`VOUT`, or
  `uart.tx` for interface signals, mirroring `pin_role`) to its local offset. `part_library` gives
  the LDO / Cap / MCU / Sensor plausible pin geometry.
- **Component orientation.** `doc::Orient` is a cardinal-only rotation enum (`Deg0/90/180/270`,
  default `Deg0`), kept exact/integer so rotated coordinates compare deterministically — no trig,
  no float drift. `Component.orient` holds it; `Orient::rotate(Point)` is exact integer rotation;
  `Orient::from_deg` normalises any multiple of 90 (so `-90 → 270`) and rejects off-axis angles.
  It is a *settable attribute*, **not** a solver DOF (optimising over rotation is nonlinear; out of
  scope). Set from the source via `GenDirective::Rotate { path, deg }` (off-axis aborts the
  transaction).
- **Pin world positions.** `part::pin_world(comp, def, pin)` returns
  `comp.pos + rotate(local offset, comp.orient)` — exact for the four cardinal rotations.
- **Near-to-a-pin.** `GenDirective::NearPin { a, b_comp, b_pin, within }` (and `solve::Constraint::
  NearPin { a, b, b_off, within }`) pulls component `a` to within `within` of a specific pin on
  `b`. Elaboration pre-rotates the target pin's local offset by `b`'s orientation into a constant
  `b_off`; the solver tracks the pin's world position as `pos[b] + b_off` each iteration (moving
  `b` carries its pin rigidly). Component-level `Near` is unchanged and still works.

**Text front-end:** extended (no breakage). `rotate <path> <deg>` and
`nearpin <a> <bComp>.<bPin> <len>` serialize/parse and round-trip; the `<bComp>.<bPin>` reference
splits at the last dot so hierarchical comp paths survive. `project::render` shows ` rot=<deg>` for
non-default orientations.

**Tested (38 passing total, +9 new):** `pin_offset` for discrete + interface pins; `pin_world`
exact under each cardinal rotation (plus rotation reversibility and `from_deg` normalisation);
Near-to-pin drags a component onto a *rotated* pin's world position; orientation round-trips through
elaboration; off-axis rotation is rejected atomically; `rotate`/`nearpin` parse and round-trip
through text + re-elaboration.

**Limitations / follow-ups:** orientation is not optimised by the solver (settable only); interface
signal offsets live on the shared `InterfaceDef`, so the same interface type places its pins at the
same local spot on every part that uses it (fine for the demo, would be per-instance in production);
`MinSep`-to-pin is not implemented (only `Near`); a component-orientation *change* does not yet bump
`geom_rev` (no geometry query consumes it today, so unobservable — left for when one does).

## Prototype status (resolution UX)

M2/M3 made reconciliation *surface* outcomes in a structured `ReconReport` (decayed hints,
`pin_conflicts`, `redundant_pins`, `orphaned`) but gave no way to **act** on them — the top open
limitation. This milestone closes that gap, keeping the architectural rule that the command algebra
is the **sole** mutation surface: every resolution is an ordinary atomic transaction down the same
`command::apply` path, not a side channel.

- **`Command::Resolve(EntityId, Resolution)`** — one new command variant plus a `Resolution` enum,
  rather than several discrete commands. Chosen because the resolution vocabulary is a closed set
  keyed by report-entry kind: a single command keeps the `Command` surface from sprawling, lets the
  discoverability helper return `(EntityId, Resolution)` pairs uniformly, and groups all
  report-acting intent in one place. Variants:
  - `DropOrphan` — drop an override whose target entity no longer exists (`orphaned`).
  - `AcceptConstraint` — clear a pin contradicted by a hard `Fix` (`pin_conflicts`), so the part
    sits at the Fix position with no lingering conflict.
  - `RePin(Point)` — keep the pin but move it (`pin_conflicts`); the Fix still wins physically, so
    this may remain a conflict (or go redundant if re-pinned onto the Fix) — deliberately the
    user's call. Equivalent to a fresh `Pin`, but validated as a conflict response.
  - `DropRedundant` — un-pin a pin the solver would satisfy anyway (`redundant_pins`).
- **Validated against the live report.** A `Resolve` aborts the transaction unless the entity is
  actually flagged in the matching category. This is what distinguishes a resolution from the raw
  `ClearOverride`/`Pin` primitives it shares a mutation with: it must target a genuinely outstanding
  issue. After the mutation, the normal re-elaborate/re-reconcile pass produces a fresh report — so
  a successfully resolved entry simply isn't flagged again (no bookkeeping of "resolved" state).
- **Discoverability:** `command::suggested_resolutions(&ReconReport) -> Vec<Suggestion>` enumerates,
  per actionable entry, the ready-to-apply command(s) plus a short rationale — so a GUI/agent can
  list "here's what you can do about each issue." A `pin_conflicts` entry yields two suggestions
  (accept-constraint, ready; re-pin, `command: None` because it needs a user-supplied position).
  `decayed` entries are omitted: a decayed hint is already GC'd at commit, so nothing remains to act
  on.

Tested (6 new unit tests, 35 total): each report condition (orphan, pin-vs-`Fix` conflict, redundant
pin) constructed, resolved, and asserted gone with the resulting state correct (e.g. accept-constraint
leaves the part `Fixed` at the Fix position, no override, clean report); re-pin shown to be the user's
call (persists or goes redundant); invalid resolves rejected atomically; and the suggested command
applied end-to-end to clear the report. Zero new dependencies.

## Prototype status (real solver)

Replaces M3's fixed-iteration relaxation with a **convergence-based** solver that offers explicit
guarantees instead of "300 sweeps then stop." Still zero-dependency.

**Method — projected Gauss-Seidel with a convergence loop.** Each *sweep* projects the current
positions onto every constraint's feasible set in turn (Gauss-Seidel: later projections see earlier
ones' updates within the same sweep), then clamps movable nodes into the board. The inequality
constraints (`Near`, `MinSep`, `NearPin`) use an implicit **active set** — a projection is a no-op
while the constraint has slack and fires only when violated, which is exactly active-set handling
for one-sided distance constraints. Crucially there is **no anchor-spring penalty term** (M3 had
one): a node is moved only by a constraint that is actually violated, and only by the minimal
displacement that satisfies it. So feasible sets are satisfied *exactly* (to tolerance) rather than
approximately, and least-change falls out for free — a part touched by no violated constraint never
moves.

**Guarantees:**
- *Iterate to convergence, not a fixed count.* The loop runs until the max constraint residual is
  below `RES_TOL` (**1 µm** — ~100–200× tighter than M3's ~0.1–0.2 mm), or the max per-sweep node
  movement falls below `MOVE_TOL` (a geometric stall: projections can no longer make progress), or a
  `MAX_ITERS` safety cap is hit. The new return type `solve::Solution { positions, converged, iters,
  unsatisfied }` records *which* happened (`converged` = residual tolerance actually met; `iters` =
  sweeps taken).
- *Feasible sets satisfied tightly.* The motivating case M3 got wrong — three decouplers each `Near`
  a regulator within 6 mm **and** pairwise `MinSep` 3 mm — converges with every relation satisfied
  to within `RES_TOL` (test asserts ≤ 0.01 mm).
- *Infeasibility reported, not hidden.* When the residual tolerance is not met (cap hit, or stall on
  an unsatisfiable set — a `MinSep` larger than the board can fit, contradictory `Fix`es, etc.),
  `converged` is `false` and `unsatisfied: Vec<Unsatisfied { constraint, residual }>` lists exactly
  which constraints remain violated and by how much — instead of returning a wrong-but-plausible
  placement. (Board containment of *movable* nodes is always achievable by clamping, so it is never
  itself listed; an unsatisfiable board manifests as the relational constraint it defeats.)
- *Deterministic.* No RNG; stable `BTreeMap`/`Vec` order; coincident points break ties on a fixed
  axis; f64 working math rounded to integer nm on output. Same `Problem` → identical `Solution`,
  bit for bit.

**Callers (`elaborate.rs`).** The three solves (full, per-override solve-without effectiveness
check, final decayed solve) now read `solve(...).positions`; reconciliation/decay semantics are
unchanged because they are defined purely by *where* nodes are placed. `converged`/`unsatisfied`
(placement infeasibility) is available for the engine to surface in a future milestone but is not
yet threaded into `ReconReport`.

**Honest limits.** Not a research-grade general geometric constraint solver: no DOF analysis, no
graph decomposition into independently-solvable subsystems, no global-optimality claim for the
least-change objective (projection finds a feasible point via minimal *local* corrections, not the
global minimum-movement solution). `MinSep` makes the feasible region non-convex, so a pathological
start could settle into a poor local configuration; for the prototype's well-separated scenes it
does not bite. Convergence of projected Gauss-Seidel on coupled inequality systems is reliable in
practice but not formally guaranteed for every input — which is *why* feasibility is checked and
reported rather than assumed.

**Tested (5 new unit tests, 49 total):** the 3-decoupler `Near`+`MinSep` case satisfied to ≤ 0.01 mm;
an infeasible set (two `Fix`ed nodes a `Near` cannot reconcile) reported `!converged` with the right
residual; a `MinSep` larger than the board reported; an unconstrained node staying bit-exactly at its
anchor; determinism (same `Problem` twice → identical positions/flag/iters). The full M1–M4 suite
(44 tests) stays green under the tighter solver. Zero new dependencies.

## Prototype status (footprint import)

The `kicad` module imports real KiCad footprints (`.kicad_mod`) into the part model, so the
built-in toy library is no longer the only source of parts with pin geometry. A `.kicad_mod` is a
single S-expression; we hand-roll a tiny tokenizer + recursive reader (zero dependencies — no
serde/sexp crates) and lift out the bits we model.

- **API:** `import_footprint(text: &str) -> Result<PartDef, String>` and the file wrapper
  `import_footprint_file(path: &str)`. Both modern `(footprint "name" ...)` and legacy
  `(module name ...)` headers are accepted; pad names may be quoted or bare.
- **What is imported is geometry, not electrics.** One `PinDef` per `pad`, named by the pad's
  number/name, positioned at the pad's `(at x y [angle])` converted mm→nm (decimal mm parsed by
  hand into integer nm, half-away-from-zero rounding — no float, preserving the fixed-point
  invariant; the rotation angle is ignored for the offset). The pad's **shape + `(size w h)`** are
  also captured into `PinDef.pad: Option<PadGeo>` — **real** copper geometry (§8): DRC checks it
  edge-to-edge, it flashes to Gerber and is knocked out of pours, and the placement solver derives a
  part's courtyard from it. Everything else (silkscreen, 3D models, explicit zones in the source
  footprint) is ignored on import.
- **Role-less by design (footprint alone).** A footprint carries **no electrical roles** —
  whether a pad is power, input, or passive comes from the *schematic symbol*, not the footprint.
  So an imported footprint *on its own* gives every pin `PinRole::Passive` and an empty `interfaces`.
  **This gap is now closed by the symbol/role layer** (see "Prototype status (symbol/role layer)"
  below): a `.kicad_sym` symbol is parsed for electrical types + functional names and joined to the
  footprint by pad number, yielding real `PinRole`s. Typed `InterfaceDef` inference from symbols
  remains future work.
- **Mapping decisions:** pads that **share a pad id** (e.g. two `MP` mounting pads, or a split
  thermal pad reusing one number) keep the **first** occurrence — they are the *same electrical
  pad*, and pad id is the stable identity (see "Prototype status (pin identity)"). **Unnamed pads**
  (`name == ""`, used for thermal/exposed pads and mechanical features) are **skipped** (no
  electrical identity). Note this dedup is by pad *id*; distinct pads that later share a *functional
  name* via a symbol join (six `IOVDD` pads, numbers `1/11/…`) are all kept — names may collide,
  ids may not.

**Verified on real PoC footprints** (from the Orbiter_Ultra.pretty library): the JST-SH headers and
the QFN ICs parse correctly — e.g. `JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical` → 4 pins
(`1,2,3,MP`; the two `MP` pads dedupe, the exposed pad is skipped) with pad 1 at
`(-1000000, 1325000)` nm; `Texas_X2QFN-12` → 12 pins; `QFN-80-1EP` → 81 pins (80 + the named EP;
its unnamed thermal sub-pads skipped).

Tested (8 new unit tests, 52 total): an embedded JST-SH-like fixture (name, pad count, specific
offsets in nm, all-`Passive`/no-interface); shared-pad dedup; unnamed-pad skipping; legacy
`(module ...)` + bare pad names + ignored rotation angle; quoted name with spaces/parens; sub-nm
fractional rounding; a battery of malformed inputs that return `Err` without panicking; and an
existence-guarded smoke test against a real on-disk footprint. Zero new dependencies.

## Prototype status (symbol/role layer)

Closes the footprint importer's headline gap: a footprint is pure geometry (every pad lands as a
`Passive` pin), but the *electrical* truth — which pad is power, which is an output, what each pad is
functionally called — lives in the schematic **symbol** (`.kicad_sym`). This layer parses a symbol
and **joins it to a footprint by pad number** into a real, roled `PartDef`. Lives in `kicad.rs`
(it reuses that module's S-expression tokenizer/reader — a `.kicad_sym` is the same S-expr dialect as
a `.kicad_mod`, so there is exactly one parser). Still zero-dependency.

- **Symbol import.** `import_symbol(text) -> Result<Symbol, String>` (first symbol) and
  `import_symbol_named(text, name)` (a named symbol out of a multi-symbol library) parse a bare
  `(symbol ...)` or a `(kicad_symbol_lib ...)`. Pins are gathered by **recursing into nested child
  unit symbols** (`(symbol "Name_0_1" ...)`, `_1_1`, …) so multi-unit parts yield all their pins;
  pins are deduped by `number` (first wins). Each `Symbol` pin is `(number, name, ElecType)`; the
  symbol's `(property "Footprint" "Lib:Name")` is captured (it names the mating footprint).
- **Electrical type → `PinRole` mapping** (`ElecType::role`). The KiCad pin-type vocabulary is a
  closed enum (`ElecType`); an unknown token is a parse **error**, never a silent default. Mapping:
  `power_in → PowerIn`, `power_out → PowerOut`, `output → Output`, `input → Input`,
  `bidirectional → Bidir`. Everything else — `passive`, `free`, `unspecified`, `no_connect`,
  `tri_state`, `open_collector`, `open_emitter` — maps to **`Passive`**. This is a *deliberate
  conservative default*: `free`/`unspecified`/`no_connect` have no driving role, and
  `tri_state`/`open_collector`/`open_emitter` only drive under bus/wired-OR semantics this
  prototype's ERC doesn't model yet — calling them `Passive` never invents a spurious
  driver-vs-driver conflict. This is the one documented place to refine when ERC grows wired-OR rules.
- **Name vs number on `PinDef`.** `PinDef` gained an additive `number: String` field. The functional
  `name` (`GPIO0`, `VDD`) is what nets/humans reference and what `pin_role`/`pin_offset` resolve by;
  `number` (`12`, `MP`) is the geometry/manufacturing key and the symbol↔footprint **join key**. For
  parts with no distinct numbering (the toy `part_library`, a raw footprint import) `number` defaults
  to `name`, so all prior callers and the existing footprint/`pin_offset`/`pin_role` behaviour are
  unchanged.
- **The join.** `join_symbol_footprint(&Symbol, &PartDef) -> JoinReport` is the tolerant core: the
  footprint is the geometry source of truth, so the result has **one pin per footprint pad**; where a
  symbol pin shares the pad's `number`, that pin takes the symbol's functional **name** + mapped
  **role**, while the **offset** always comes from the footprint pad. Pads with no symbol match stay
  `Passive` with `name = number`. **Mismatches are reported, never silently dropped:**
  `JoinReport.symbol_only` lists `(number, name, role)` for symbol pins with no pad (so a dropped
  *power* pin is visible) and `footprint_only` lists pads with no symbol pin. `import_part(symbol_text,
  footprint_text) -> Result<PartDef, String>` is the **strict** convenience wrapper: any mismatch is
  an `Err` naming the offending pads, so a missing power pin can't pass unnoticed; callers wanting to
  tolerate mismatches use `join_symbol_footprint` and inspect the report.

**Verified on a real symbol+footprint pair.** TI `TPS25981x` eFuse symbol (from
`Power_Management_TI.kicad_sym`, whose own `Footprint` property names
`eFuse_TI:Texas_RPW9919A_VQFN-HR-10`) joined to that `.kicad_mod`: a clean **10/10** join, no
orphans. Sample joined pins (number, name, role, offset nm): `5, IN, PowerIn, (-250000, 0)`;
`6, OUT, PowerOut, (250000, 0)`; `8, GND, PowerIn, (900000, 225000)`; `3, PG, Passive,
(-900000, 225000)` (`open_collector → Passive`); `7, DVDT, Output, (725000, 875000)`.

**Tested (5 new unit tests, 62 total):** an embedded multi-unit symbol fixture (pins gathered across
child units, footprint property captured); the full electrical-type→role table including the
unknown-type error; a hermetic symbol+footprint join asserting functional names, mapped roles, pad
numbers, and offsets (nm); the pin-mismatch path (a symbol-only power pin and a footprint-only pad
both surfaced, nothing dropped, strict `import_part` erroring); and an existence-guarded real-data
join (the `TPS25981x` ↔ `Texas_RPW9919A_VQFN-HR-10` pair above). The existing 57 tests stay green.
Zero new dependencies.

**Limitations / follow-ups:** the join produces discrete roled pins only — it does **not** yet infer
typed `InterfaceDef`s (UART/SWD/…) from symbol pin-name patterns, so the "serial-swap-unrepresentable"
interface story still relies on the hand-authored library. Pin `number` dedup keeps the first
definition across units; a symbol that legitimately repeats a number with a different role would lose
the later one (not seen in practice). Alternate-function pin names (KiCad `(alternate ...)`) are
ignored — only the primary `(name ...)` is used.

## Prototype status (pin identity)

Closes issues 0001 + 0002 (the PoC's scariest finding: a real MCU reuses power-pin
*names* — the RP2350A has six pads named `IOVDD`, three `DVDD` — and the original
name-keyed model collapsed them, silently floating 5 of 6 pads with DRC none the
wiser). Two orthogonal axes of identity, deliberately separated:

- **`comp` (the `EntityId` / instance path)** separates *instances*: three chained
  WS2812s, two MCUs.
- **`pin` (the pad number)** separates *pads within one instance*. `PinRef.pin` now
  holds the **stable pad identity** — a pad number for a discrete pin, or
  `port.signal` for an interface signal — never the functional name. `pin_role` /
  `pin_offset` resolve that identity; numbers are unique per part, names are not.

A functional **name is only a selector**, scoped to one part:
`PartDef::resolve_selector("IOVDD")` fans out to every IOVDD pad's number (falling
back to a direct pad-number reference, e.g. `"30"`/`"MP"`). The fan-out happens once,
at connection time in elaboration (`ConnectPins`/`NoConnect`), so a name never
reaches across instances. An unresolvable selector — a typo, or a pin the part lacks
— **aborts the (atomic) elaboration** rather than creating a silently dangling
member: that is issue 0002's connect-time validation. Issue 0002's other half is
`kicad::apply_role_map`, a `(pad_number, name, role)` overlay that roles a bare
footprint without authoring a full `.kicad_sym`.

**Completeness, not just non-collapse.** `Doc.no_connects` (from
`GenDirective::NoConnect`) records intentional opens, and a dedicated `Key::Floating`
query reports every pad that is on no net and not no-connect. It is a *separate*
query from `Key::Erc` on purpose: its dependency footprint is the raw component/pad
universe + no-connect set (not the resolved netlist), so it cannot share — and would
weaken — ERC's netlist-only early-cutoff firewall. Together `Erc` + `Floating` are
the full electrical check. Result: a dropped pad is now *reported*, not merely
un-collapsed — even a single forgotten pad, not just duplicated power names.

The capstone PoC validates this end to end: it nets all six IOVDD / three DVDD pads
(and the EP→GND) with single name connects, declares its intentional opens, and runs
**0 floating pads / ERC 0** — the old `uniquify`/distinct-name workarounds are
deleted. **Limit:** placement that must target one *specific* pad of a duplicated
name (a per-pad decoupler) references it by pad number, since a name there would fan
out (`NearPin` takes the first match).

## Prototype status (export)

The `export` module turns a `Doc` (+ the `PartLib` for geometry) into deterministic, diffable
output artifacts. Each exporter is a **pure function** of its inputs — no wall-clock, no
randomness, all iteration over `BTreeMap`/`BTreeSet` — so output is byte-stable run to run, and a
one-thing change yields a one-line diff. This is the same "render is a pure function of the model"
discipline as the text projection, applied to fab/check artifacts.

- **`netlist(doc) -> String`** — the connectivity artifact. One net per line,
  `name: comp.pin comp.pin ...`, nets in `NetId` order and pins in `PinRef` order. This is what a
  fabricated/assembled board is checked against.
- **`placement_csv(doc) -> String`** — a pick-and-place CSV, `ref,part,x_mm,y_mm,rotation_deg`, one
  row per component in `EntityId` order. Coordinates are six-decimal millimetres formatted by pure
  integer arithmetic (no float — the fixed-point determinism invariant holds end to end); rotation
  is the component's cardinal orientation.
- **`svg(doc, lib) -> String`** — a board sketch for visual sanity-checking: the board outline (the
  source `Board` directive if present, else the bounding box of placed geometry), each component
  drawn at its position with its pin pads (via `pin_world`) and an id label, **and the routed copper**
  (trace polylines coloured/classed per layer, vias as circles). The model's y axis points up (ECAD
  convention) and SVG's points down, so y is flipped within the content bounds to keep the sketch
  upright. Element order follows `EntityId`/`TraceId`/`ViaId` order; no timestamps.

**Gerber/drill output now exists** (see "Prototype status (Gerber/fab output)" below): now that
routing writes real copper into the `Doc` and footprint pads carry render geometry, the fab
artifacts describe genuine copper. `gerber_set` emits an RS-274X Gerber per copper layer + an
`Edge.Cuts` outline and an Excellon drill program.

`cargo run --example export` elaborates a small power-supply board on a 60×40 mm outline and prints
the netlist / P&P / SVG; `cargo run --example gerber` autoroutes a board and dumps the full fab
fileset + SVG. Tested (netlist nets/pins for `psu_module(2)`; P&P header + exact rows + row count +
a rotated component's rotation column; SVG outline (explicit board *and* bbox fallback), component
ids, labels, pads, and now trace/via elements; `fmt_mm` sign/fraction handling; determinism — each
exporter called twice yields identical strings). Zero new dependencies.

## Prototype status (routing core)

Lays the **foundation of the routing subsystem**: a provenance-tagged trace/via representation
(tier-2 materialized state) plus a DRC checker (tier-3 query). The **autorouter is deliberately
deferred** to a later milestone — this milestone is the representation it will write onto and the
DRC query it will validate against. Still zero-dependency; all geometry is integer nm.

**Representation (`route` module, stored in `Doc`).** Routed copper lives in the document alongside
component placement, in the same tier and with the same `Provenance` ladder:
- **`Layer`** — `Top` / `Bottom` outer copper plus `Inner(u8)` so the model extends to multilayer
  without a rework; ordered by physical stack-up depth (which is what via spans test).
- **`Trace`** — a `NetId`, a `Layer`, a centreline polyline (`Vec<Point>`), a `width` (nm), and a
  `Provenance`. **`Pinned`** = hand/agent-routed (the autorouter will treat it as a fixed obstacle);
  **`Free`** = reserved for the future autorouter's regen-able output. One provenance bit, exactly as
  §1 prescribes — not a separate "auto vs manual" subsystem.
- **`Via`** — a centre `Point`, the `from`/`to` layers it spans, its `NetId`, `drill`/`pad` sizes,
  and a `Provenance`.
- Both live in `Doc` as `traces: BTreeMap<TraceId, Trace>` and `vias: BTreeMap<ViaId, Via>`, mirroring
  how placement lives in the doc. New id newtypes `id::TraceId(u64)` / `id::ViaId(u64)` — a trace has
  no natural hierarchical name, so ids are caller-minted monotone integers (KiCad-UUID style), assigned
  the same way by a hand edit or a future autorouter.

**Commands (sole mutation surface).** `Command::{AddTrace, RemoveTrace, AddVia, RemoveVia}` carry a
caller-supplied stable id and (for adds) the fact itself; the hand/agent-routing API passes
`Provenance::Pinned`. Validation is atomic (unknown net, degenerate polyline `<2` points, non-positive
width/drill/pad, duplicate or missing id all abort the whole transaction). A new coarse input revision
**`route_rev`** (with `InputId::Routing`) is bumped by `apply` when `traces`/`vias` change, parallel to
`conn_rev`/`geom_rev` — so a route edit bumps *only* `route_rev`, and a placement nudge that touches no
copper does not bump it.

**DRC (`Key::Drc` query, modelled on ERC).** `query::QueryValue::Drc(Vec<route::Violation>)` returns a
canonical (sorted, de-duped) violation set from `route::check_drc`. Three checks:
- **Min width** — every `Trace.width >= rules.min_trace_width`.
- **Clearance** — copper of *different* nets must be `>= min_clearance`, **edge to edge** (the
  threshold adds the traces' half-widths / via pad radii). Covers trace-vs-trace (same layer),
  trace-vs-pad, and (bonus) via-vs-trace / via-vs-pad / via-vs-via with layer-span tests. Comparisons
  are exact `i128` against *squared* thresholds (a point↔segment distance kept as a rational
  `num/den`, and an integer segment-intersection test) — no floats, so the violation set is
  byte-stable.
- **Connectivity completeness (ratsnest)** — a **union-find** over each net's pins + traces + vias,
  joined by geometric **incidence** within `DesignRules.touch_tol` (default 0.01 mm): a pin touches a
  trace whose polyline passes within tol of the pad point, a pin touches a coincident via, same-layer
  traces that touch fuse, and a via fuses copper across the layers it spans. A net is fully routed iff
  all its pins land in one component; otherwise an `Unrouted { net, islands }` flags how many
  disconnected groups remain.
- **Design rules** — `route::DesignRules { min_clearance, min_trace_width, touch_tol }` with generic
  2-layer defaults (0.15 mm clearance/width). The DRC query uses `DesignRules::default()`; wiring these
  to a per-board/source process definition is the documented one-line follow-up.

**Wired into the incremental engine.** `Drc` records three dependencies: the `Routing` input and the
`Geometry` input (pads move with components) directly, and the **`Netlist` query** (not raw
`Connectivity`) for the ratsnest pin set. Recording Netlist as a *query* dep is the firewall: a
connectivity edit whose resolved netlist is unchanged is cut off and does **not** recompute DRC.
`Engine::query` now folds `route_rev` into the current revision.

**Modelling decisions / simplifications (documented honestly):**
- **Pads are points.** A pad is its `pin_world` centre (radius 0) for both clearance and incidence;
  pads are treated as present on **all layers** (through-hole assumption). (Footprint pads now carry
  size/shape — `PinDef.pad` — but that is **render-only** for Gerber; DRC deliberately still ignores
  it.) Trace/via copper *does* carry width/pad size in the clearance threshold.
- **A "touch" is incidence within `touch_tol`**, not an overlap area; hand-placed integer coordinates
  make exact-coincident endpoints distance 0, and the tolerance absorbs deliberate near-misses.
- **Clearance violations are keyed by `(net, net, layer)`** (de-duped), not by location — multiple
  breaches of the same pair on the same layer collapse to one entry (keeps the set small and stable for
  early cutoff; a location-bearing variant is a future refinement).
- **Unassigned copper is ignored for clearance** (only net-member pads and net-bearing traces/vias are
  checked); orphaning of traces when their net disappears under a source edit is **not** handled yet.

**Tested (9 new unit tests, 78 total):** a clean hand-routed two-pin net passes; an unrouted net flags
`Unrouted{islands:2}`; a different-net same-layer clearance breach is caught (exact violation asserted);
a too-narrow trace is caught; a two-layer route joined by a via passes the ratsnest (and fails without
the via); adding a trace bumps **only** `route_rev` and re-runs DRC (turning the net clean); a routing
edit does **not** re-run ERC/Netlist (input isolation); a non-routing edit whose netlist value is
unchanged does **not** recompute DRC (early-cutoff firewall, like the ERC test); and routing commands
validate atomically. The existing 69 tests stay green. Zero new dependencies.

**Explicitly deferred (next agent / later work):** ~~the **autorouter**~~ — now built, see
"Prototype status (autorouter)" below; **serializing routes** in the text
front-end (`text` module — routes are not yet part of the canonical tier-1/tier-2 text projection);
and ~~**rendering traces** in the export SVG / Gerber~~ — now done, see "Prototype status
(Gerber/fab output)" below (the SVG draws traces/vias and a Gerber/Excellon fab fileset is emitted).

## Prototype status (autorouter)

A **basic deterministic grid/maze autorouter** (`autoroute` module), built as the
transaction-proposer §1 prescribes: `autoroute(doc, lib, rules) -> AutorouteResult` is a pure
function that **reads** facts (netlist, placement, pinned routes) and **returns** a proposed
`Vec<Command>` (`AddTrace` + `AddVia`, all `Provenance::Free`) plus `routed`/`unrouted` net lists.
It never mutates the `Doc`; applying the commands goes through the ordinary atomic
`command::apply` path, so the GUI cannot tell an autoroute trace from a hand route except by the
provenance bit. Zero new dependencies; all geometry is integer nm; same `Doc` → byte-identical
commands.

**Grid + A\*.** The routing area (the source `Board` outline, else the pad bounding box + margin)
is discretised into a square grid; A\* searches over `(x, y, layer)` with `Top`/`Bottom` copper,
orthogonal steps costing one pitch and a layer change costing a via penalty (10 pitches, so
single-layer routes are strongly preferred and vias appear only when needed). Net order is `NetId`
order; pins within a net are connected MST-style (each remaining pin routed to the net's existing
connected copper). A grid path is coalesced into collinear segments, with `AddVia` emitted at each
layer change and a short stub onto the *exact* pad world point at each pin end (so the trace
literally touches the pad — the ratsnest unions it).

**Grid pitch (clearance falls out).** `pitch = via_pad + min_clearance` with
`via_pad = 2·min_trace_width`, `via_drill = min_trace_width`. Because all routed copper lies on grid
nodes / axis-aligned edges and **distinct nets never share a node** (node ownership), the minimum
distance between different-net copper is exactly `pitch` — chosen so *every* adjacent-node pairing
(track↔track, track↔via, via↔via) meets the edge-to-edge clearance rule. So routed-vs-routed
clearance is guaranteed by construction; only **off-grid** obstacles need radius-based cell
blocking. Obstacles → blocked cells: the board exterior (off-grid), other-net **pads**
(`pin_world`, points, all layers), other-net **pre-existing traces/vias** (`Pinned` hand routes are
fixed obstacles, blocked on their layer/span), and copper **already routed this run** for other nets
(node ownership). Same-net copper is never blocked. Block radii are sized to keep both a node and
the half-edges leaving it clear; correctness is **verified against the real DRC query**, not assumed.

**Failure is reported, not fatal.** A net whose pins cannot all be connected (e.g. walled off on
both layers) is added to `unrouted` and contributes **no** commands — its partial claims are rolled
back, so it never emits dangling/overlapping copper and never blocks later nets with phantom
ownership. Routing then continues with the remaining nets.

**Honest limits (by design).** Greedy net-by-net maze routing only: **no rip-up-and-retry, no
topological/push-and-shove, no length/impedance matching.** Consequently **net ordering matters** —
a net that fails may be routable in a different order (an earlier net can wall off a later one).
Pads are points (the model carries no pad size). Existing *same-net* copper is treated as a
non-obstacle but is not used as a routing seed (a net is re-routed from its pins). Only `Top`/`Bottom`
are routed (the grid is 2-layer); inner layers in the representation are ignored by the router.

**Tested (5 new unit tests, 83 total) — all verified through `Key::Drc`:** a two-net board routes
from all-`Unrouted` to fully DRC-clean (no clearance/width violations introduced); a 3-pin net
connects MST-style and passes the ratsnest; a `Pinned` other-net wall on `Top` is avoided (the route
drops to `Bottom` via a via and stays clearance-clean); an impossible net (walled on both layers) is
reported `unrouted` with **no** commands emitted, leaving DRC flagging it unrouted but introducing no
spurious violations; and determinism (autoroute twice → identical commands). The existing 78 tests
stay green. `cargo run --example autoroute` shows the end-to-end pass: DRC violations (unrouted)
before, autoroute + apply, DRC clean after.

## Prototype status (Gerber/fab output)

The last missing PoC piece: **fab output**. Now that routing writes real copper into the `Doc`
(traces with width, vias with pad + drill) and footprint pads carry render geometry, the `export`
module emits the manufacturing fileset — **RS-274X Gerber** per copper layer + an `Edge.Cuts`
outline, and an **Excellon drill** program. Same discipline as the other exporters: each is a pure
function of the `Doc` (+ `PartLib`), all coordinates flow from integer nanometres into each format
by **integer arithmetic** (no float, no timestamps, stable ordering) → byte-stable, diffable output.

**Pad geometry capture (render-only, additive).** A footprint pad has a position but the model
carried no pad *size/shape*, so a pad could not flash as copper. `import_footprint` now also reads
each pad's **shape** token (`circle`/`rect`/`roundrect`/`oval`; unknown/complex shapes fall back to
their bounding `rect`) and `(size w h)`, stored as `PinDef.pad: Option<part::Pad { size: (Nm, Nm),
shape: PadShape }>`. It rides through the symbol↔footprint join (the footprint is the geometry
source). This is **fab-render metadata only**: DRC and the autorouter still treat a pad as its
`pin_world` *point* (radius 0) and never read it. Toy `part_library` pins carry no footprint, so
`pad` is `None` and they contribute no copper flashes.

**Coordinate format.** Gerber uses `%FSLAX46Y46*%` (absolute, leading-zeros-omitted, 4 integer + 6
fractional digits of mm) with `%MOMM*%`. Because 1 mm = 1_000_000 nm, the integer the file carries
**is exactly the nanometre value** — so a coordinate is just `nm.to_string()`, no float. Aperture
definitions and Excellon coordinates/tool sizes use the same six-decimal-mm `fmt_mm` formatter.

**API (`export` module).**
- **`gerber_layer(doc, lib, layer) -> String`** — one copper layer as RS-274X: format spec, mm
  units, the layer's **aperture table** (distinct apertures, codes 10.. in a canonical `Ord`), then
  objects. **Traces → draws:** each trace's centreline is a `D02` move + `D01` draws with a round
  aperture sized to its `width`. **Vias/pads → flashes:** a via pad (`D03`) on each layer it
  `spans`, with a round aperture sized to its `pad`; a component pad (`D03`) by **shape** —
  `circle→C`, `rect`/`roundrect→R` (bounding box; basic Gerber has no rounded-rect), `oval→O`.
  Component pads flash on **every** copper layer (the all-layer point model). Object order is
  `TraceId`, then `ViaId`, then `(EntityId, pin)` — deterministic. Ends `M02*`.
- **`gerber_edge_cuts(doc, lib) -> String`** — the board outline as a closed rectangle drawn with a
  thin 0.1 mm pen, from the source `Board` rect, else the placement/route bounding box.
- **`excellon_drill(doc) -> String`** — `M48` header, `METRIC`, one **tool** per distinct via drill
  diameter (`T1..`, sorted), then each tool's hole coordinates (`ViaId` order), `M30`. Decimal-point
  coordinates so zero-suppression mode is moot.
- **`gerber_set(doc, lib) -> Vec<(String, String)>`** — the convenient fileset: `board-F_Cu.gbr` /
  `board-B_Cu.gbr` / `board-In<n>_Cu.gbr` (stack-up order) + `board-Edge_Cuts.gbr` + `board.drl`.

**Honest limits.** **Not validated against a real Gerber viewer** — assertions here are
syntactic/structural (format directives, aperture defs, draw/flash counts, exact coordinates). **DRC
still treats pads as points** (radius 0, all layers); the pad size/shape captured here feeds *only*
the copper flash, not clearance/connectivity. Component pads flash on all copper layers (the model
has no per-pad layer), `roundrect`/`custom` pads flash as their bounding rectangle, and the board
base filename is the fixed `board` (the `Doc` carries no board name). Through-holes are vias only.

**Tested (10 new unit tests, 93 total):** footprint import captures pad shape + size (fixture, and a
size-less pad → `None`) and it survives the symbol/footprint join; a hand-routed two-layer fixture
produces the F_Cu/B_Cu Gerbers with the expected format spec, aperture defs, exact trace draws
(`D01` counts + coordinates) and via/pad flashes (`D03`); the Excellon lists the via drill + its
coordinate; `Edge.Cuts` traces the outline rectangle; a part with real pad geometry flashes `R`/`C`
apertures at the right world positions; the SVG now contains `trace`/`via` elements; `gerber_set`
filenames + layer order; and determinism (every fab exporter twice → byte-identical, incl. on an
autorouted board). The existing 83 tests stay green. `cargo run --example gerber` autoroutes a board
and dumps the whole fileset + SVG. Zero new dependencies.
