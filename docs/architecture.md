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

**Honest limitation:** the relaxation solver is *approximate* — it satisfies a set of mutually
constraining relations to within ~0.1–0.2 mm, not exactly, and offers no global-optimum or
feasibility guarantee. A production tool needs a real geometric constraint solver (DOF analysis /
decomposition / Newton). This is the deliberate prototype-scope tradeoff, not a design position.

**Open limitations / next prototype targets (M4 candidates):**
- **Resolution UX** for conflicts/orphans now exists (see "Prototype status (resolution UX)"
  below); what remains is presenting it in a GUI and richer multi-issue batching.
- Solver is approximate relaxation (see above); no keepouts. (`Near`-to-a-*pin* and a settable
  rotation/orientation attribute now exist — see "Prototype status (physical parts)" below; the
  solver still does not *optimise* over orientation.)
- Query dependencies are recorded explicitly, not auto-tracked; inputs are coarse
  (`conn_rev`/`geom_rev`).
- No router.
- The end-to-end PoC target (a single-PCB chip-down rework of the RP2350-Zero SWD-probe carrier)
  needs: real parts/footprints with pin geometry, a netlist→placement→route flow, and fab output.
  **Footprint *geometry* import now exists** (see "Prototype status (footprint import)" below): real
  KiCad `.kicad_mod` files (incl. the PoC's JST-SH headers and the QFN ICs) parse into `PartDef`s
  with per-pad pin offsets. What's still missing for the PoC: electrical roles/interfaces (a
  footprint carries none — they come from the schematic *symbol*), and the netlist→placement→route→fab
  flow.

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
- **Role-less by design (the key limitation).** A footprint carries **no electrical roles** —
  whether a pad is power, input, or passive comes from the *schematic symbol*, not the footprint.
  So every imported pin is `PinRole::Passive` and `interfaces` is empty. Pairing an imported
  footprint with role/interface information from a symbol is future work and a PoC prerequisite.
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
