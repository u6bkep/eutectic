# ECAD-from-scratch: Architecture & Representation

**Status:** theoretical design. No code yet. This document captures the architecture
converged on in design discussion, *including the open questions and hard parts* — it is
meant to be argued with, not treated as settled.

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

## Recommended next steps

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
  mutate them atomically, and DRC is a tier-3 query (clearance, min-width, ratsnest). The
  **autorouter itself is the next step** — it will write `Free` trace DOFs on top of this
  representation, treating `Pinned` traces as fixed obstacles.
- The end-to-end PoC target (a single-PCB chip-down rework of the RP2350-Zero SWD-probe carrier)
  needs: real parts/footprints with pin geometry, a netlist→placement→route flow, and fab output.
  **Footprint *geometry* import now exists** (see "Prototype status (footprint import)" below): real
  KiCad `.kicad_mod` files (incl. the PoC's JST-SH headers and the QFN ICs) parse into `PartDef`s
  with per-pad pin offsets. **Electrical roles now exist too** (see "Prototype status (symbol/role
  layer)" below): a `.kicad_sym` *symbol* supplies the functional pin names + electrical types that a
  footprint lacks, and the two are joined by pad number into a real `PartDef` with mapped `PinRole`s.
  **Netlist and placement export now exist too** (see "Prototype status (export)" below): the
  connectivity and pick-and-place artifacts a board is checked/assembled against are emitted
  deterministically from a `Doc`. What's still missing for the PoC: typed `InterfaceDef`s inferred
  from symbols (the join produces discrete roled pins, not interfaces yet), the **router**, and
  **Gerber/drill output** (deferred until routing — it describes copper geometry the model does not
  yet carry).

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
  invariant; the rotation angle is ignored for the offset). Everything else (silkscreen, courtyard,
  fab, 3D models, sizes, layers, zones) is ignored.
- **Role-less by design (footprint alone).** A footprint carries **no electrical roles** —
  whether a pad is power, input, or passive comes from the *schematic symbol*, not the footprint.
  So an imported footprint *on its own* gives every pin `PinRole::Passive` and an empty `interfaces`.
  **This gap is now closed by the symbol/role layer** (see "Prototype status (symbol/role layer)"
  below): a `.kicad_sym` symbol is parsed for electrical types + functional names and joined to the
  footprint by pad number, yielding real `PinRole`s. Typed `InterfaceDef` inference from symbols
  remains future work.
- **Mapping decisions:** pads that **share a name** (e.g. two `MP` mounting pads, or a split
  thermal pad reusing one number) keep the **first** occurrence — a duplicate pin name would
  silently break `pin_offset`/`pin_role`, which resolve by first match. **Unnamed pads** (`name ==
  ""`, used for thermal/exposed pads and mechanical features) are **skipped** (no electrical
  identity).

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
  drawn at its position with its pin pads (via `pin_world`) and an id label. The model's y axis
  points up (ECAD convention) and SVG's points down, so y is flipped within the content bounds to
  keep the sketch upright. Element order follows `EntityId` order; no timestamps.

**Gerber/drill is deliberately deferred.** Those formats describe *copper geometry* — trace
polygons, pad stacks, drill hits — and there is **no router yet**, so the model carries no copper
traces to emit. The artifacts above cover exactly what the model has today: placement (positions +
cardinal orientation) and connectivity (the net hypergraph). Gerber becomes meaningful once a
routing layer writes trace geometry into the document.

`cargo run --example export` elaborates a small power-supply board on a 60×40 mm outline and prints
all three artifacts. Tested (7 new unit tests, 64 total): netlist nets/pins for `psu_module(2)`;
P&P header + exact rows + row count + a rotated component's rotation column; SVG outline (explicit
board *and* bbox fallback), component ids, labels, and pads; `fmt_mm` sign/fraction handling; and
determinism (each exporter called twice yields identical strings). Zero new dependencies.

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
- **Pads are points.** A footprint import carries no pad *size*, so a pad is its `pin_world` centre
  (radius 0) for both clearance and incidence; pads are treated as present on **all layers**
  (through-hole assumption). Trace/via copper *does* carry width/pad size in the clearance threshold.
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

**Explicitly deferred (next agent / later work):** the **autorouter** (writes `Free` trace DOFs onto
this representation, treats `Pinned` traces as obstacles); **serializing routes** in the text
front-end (`text` module — routes are not yet part of the canonical tier-1/tier-2 text projection);
and **rendering traces** in the export SVG / Gerber (`export` module — copper geometry now exists in
the model, but emitting it is out of scope here).
