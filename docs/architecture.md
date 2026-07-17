# ECAD-from-scratch: Architecture & Representation

**Status:** design of record, now substantially **implemented** as the `eutectic-core` Rust prototype
(see [`../README.md`](../README.md) and the "Prototype status (...)" sections throughout this
document). This file captures the architecture converged on in design discussion *including the
open questions and hard parts* — the prose sections are the reasoning; each "Prototype status"
section records what the corresponding code actually does and its honest limits. Treat it as a
living document, not settled dogma. This document is current-state only: dated decision records,
milestone snapshots, and superseded plans live in the project log ([`docs/log/`](log/README.md)),
cited inline as "\[dNN\]" where a ruling's provenance matters.

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
  it N times, parameterized. This is "organize complex designs logically." (The concrete construct
  is the `def` — see §5.)

### The schematic is a derived view (ruled in [d20](log/d20-schematic-derived-view.md))

The schematic is the **second derived projection** of the generative truth (the flat netlist is the
first, the board the third). The drawing is never authoritative: a GUI wire-draw gesture is not
"creating a wire" — it means `ConnectPins`; the command mutates truth, and the drawn wire then
renders *because the netlist says so*. Consequence, stated as a feature: **forward/back annotation
ceases to exist** — text, schematic, and board are projections of one document and cannot disagree.

- **No solver on the view path.** Authored: a structural **layout tree** — nested containers with
  direction, symbols as leaves — persisted as tier-1 native grammar directives (siblings of `inst`)
  with real diagnostics. Derived: the coordinates, by a pure, deterministic, terminating **reflow**
  of the tree — the computational class of *elaboration*, not routing; never serialized. The
  vocabulary is a deliberately tiny flexbox subset with literal CSS names (`row`/`column`, `gap`,
  `align`, plus a pinned offset within a container as the escape hatch). The tree is the
  reconciliation unit: adding a part is "insert a child into the row," and the reflow is
  least-change by construction.
- **The view is total and honest.** Any symbol not in the layout tree renders in a derived
  "unplaced" bin; every connection renders at least as a **named net tag at the pin**. The
  schematic never silently omits a part or a connection — quality is added incrementally by
  authoring structure, never required up front. Tags remain the default connection rendering even
  for placed symbols.
- **Connections stay authored; derived presentation must never lie** (ruled in
  [d23](log/d23-schematic-features-tier.md)). Drawn wires are authored documentation — straight
  lines or simple splines pin-to-pin, optionally directed by **waypoints** that are pure
  presentation, a no-op to the netlist truth. No wire autorouting, ever, on the view path; anything
  smarter is a future *editing tool* proposing waypoints under [d18](log/d18-routes-persisted.md)
  semantics. Derived decoration may only restate authored facts — per the ruling: "a junction dot
  may be derived where authored same-net wires *share an endpoint or waypoint*, never at a mere
  visual crossing."

### One realized-geometry tier: `schematic_features` (ruled in [d23](log/d23-schematic-features-tier.md))

`schematic_features(doc, lib)` is the single place the schematic drawing is realized — the
schematic-side twin of the board's `world_features`. It emits typed primitives in schematic space
(strokes, discs, polygons, text **runs**) covering everything the drawing shows, each carrying
semantic provenance (component path / pin / net / wire / bin chrome) and a **style class** — no
colors, no fonts-as-geometry, no view-toolkit types, deterministic order. Views are pure consumers:
the SVG backend is a dumb serializer of the stream (the headless/agent artifact and test oracle),
the GUI renders and picks from the same stream, so hit-testing and rendering cannot drift. Text is
a **run, not glyphs** — each consumer realizes it; stroked-glyph realization is reserved for fab
ink (board silk via `world_features`), where the glyphs *are* the artifact.

- **Symbol artwork is a seam, not a feature (yet).** A symbol's body is realized by a single
  function — "body primitives for this part def" — whose only implementation today is the derived
  box-with-pins. Authored artwork (line art plus **semantic anchors**: pin anchor = point +
  approach direction, label slots for derived text) later replaces the default *behind the seam*,
  with no contract change.
- **Footprints and symbols are the same kind of thing in two vocabularies** (recorded direction,
  not yet built): both are authored primitive bundles plus semantic anchors. A footprint speaks the
  *fab* vocabulary — pads/silk/mask/drills with real roles and z ("an instantiated pcb without the
  FR4"); a symbol speaks the *annotation* vocabulary — line art + anchors, no fab meaning. Native
  authoring for both belongs in the text grammar (def-style blocks), **not** literal SVG; a symbol
  or footprint editor is the owned renderer pointed at a def's elaboration.
- Sheets/hierarchy stay out of the contract (a sheet is a plane/group when it comes).

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

### The source language: declarative core, hermetic expressions, `def` reuse (ruled in [d21](log/d21-source-language-core.md))

The language question is settled deliberately, not by accretion — the **Onshape trap** is growing
a janky language one reasonable step at a time. The source of truth is not the netlist; it is the
**generative description** — the flat netlist is its first derived view, the schematic its second.

- **`def` is the reuse construct.** A named sub-circuit — parts, internal nets, optionally its
  schematic layout tree — with a typed I/O surface and declared parameters with defaults,
  instantiable at a hierarchical path through the ordinary `inst` grammar. The mental model is the
  React component (`def` ≈ component, ports ≈ props). Ports resolve through to the bound pin's
  **pad identity** (no new namespace), transitively through nested defs. Nesting composes paths;
  refdes annotation stays board-global flat (industry convention) while paths stay hierarchical;
  internal nets elaborate path-prefixed. A def body is a pure function of its declared params (the
  range loop variable is deliberately not visible inside — forward it explicitly); recursion is a
  hard error; a false `if=` drops the whole stamped subtree, with dangling references degrading to
  `W_DNP` — never a silent net merge, which is refused as a hard collision error.
- **The expression tier is hermetic and non-Turing-complete — an architectural invariant, not
  taste.** Two commitments force it: elaboration is the **commit gate** (it runs on every
  transaction, eventually at interactive GUI rates, so the document language must be pure,
  deterministic, terminating, ~O(output size)); and **reconciliation requires stable identity**
  (ID-keyed overrides address source-analyzable paths; `sense[2].R1` stays stable under `n: 3→4`).
  The power budget is HCL/Terraform-shaped: parameters + decimal-exact arithmetic + bounded ranges
  + a conditional for population variants (DNP). Explicitly excluded: user-defined functions,
  string manipulation, recursion, unbounded loops, I/O.
- **The Onshape clause.** If in-document scripting is ever truly needed, we embed an existing
  hermetic language (Starlark is the standing candidate) — **we never grow our own.** General
  computation stays **at the rim**: agents and Rust programs (the command API, generator programs)
  are the Turing-complete layer, and the document is the *output* of computation, never the site
  of it.
- **Three editing modes, human-first-class.** Flat authored content gets full GUI CRUD through the
  same command algebra the agent uses (the blank-canvas EE workflow is first-class with zero text
  contact); parameters bind to direct GUI editing; generated content takes ID-keyed per-instance
  overrides — *and* the def itself is editable as its own canvas (the Figma component model,
  including "make component"). Only the expression tier is text-exclusive — progressive
  disclosure, not relegation.
- **Two identity strategies, one per zone** (ruled in [d22](log/d22-route-identity-persists.md)).
  Design-zone entities (parts, nets, pins, overrides) get identity from their *names* —
  hierarchical paths humans author; no visible id syntax. Routes are the one feature class with no
  natural name; they live in the machine-written `# routes` state zone and carry small persistent
  integer ids (see §8, "Routes are persisted state").
- **Mixed authorship of the text form is a filed design requirement**: comments, ordering, and
  grouping chosen for readability need a preservation story beyond canonical-serialization
  determinism (issue 0030 records the current block-interior normalization limit).

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
user/agent input. The severity ladder now has a live **warning class** (`W_FONT_LOAD` first):
warnings ride `ReconReport`/`Diagnose` like errors but are deliberately excluded from `is_clean()` —
degradations the doc survives (a missing font falls back to strokes) are surfaced without blocking.
The rustc-shaped split is a stated rule: *panic/ICE when the caller is our code; `E_` diagnostic
when the cause is the user's input; `W_` when the doc degrades but stays valid.* Coordinate-range
ingest validation (`E_COORD_RANGE`, `geom::MAX_COORD`) guards every Nm entry point (text, commands,
imports — imports via their existing `Err(String)` channel). **Follow-ups:** (1) the `kicad` import layer still returns `Result<_, String>` —
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
  *union of features*; clearance is the min over the union. The third variant, **`Area(Region)`**
  (ruled in [d16](log/d16-area-unified-producer.md)), is a filled area *with holes* — a set of oriented rings under the non-zero winding
  rule. It carries what a simple polygon cannot: the board substrate (outline ∖ cutouts), pour fills
  with knockouts, TTF glyphs with counters. Clearance generalizes (ring edge-distance + containment);
  the exact-integer region kernel (`geom/kernel.rs`) provides its booleans and offsets.
- **Two kinds of negative space, deliberately not interchangeable** (ruled in [d16](log/d16-area-unified-producer.md), 16b): a hole in an
  `Area` is *what the entity is* — intrinsic, in-plane, full-z for that feature (board cutouts, glyph
  counters, pour knockouts) — and reaches fab output as a **routed contour** (Edge.Cuts rings). A
  `Role::Void` feature is *what one entity does to the rest of the board* — cross-entity, individually
  enumerable, or z-partial (pad/via drills, mask openings, blind cuts) — and reaches fab output as
  **drill data** (exact center + diameter; plated = copper material → PTH file, else NPTH; capsules =
  G85 slots). Extracting drill data from `Area` holes is banned permanently: the region kernel
  polygonizes at construction, so the diameter is already gone — recovering it would be an inverse
  projection ([d13](log/d13-slab-name-identity.md)). The representation *is* the manufacturing intent.
- **Evaluation is two-level 2.5D CSG**: union of solid prisms, minus void prisms, done. No solids
  nested inside voids, no re-additions, no curved z. Every consumer evaluates it the same way
  (filter by role/slab, subtract voids at its own boundary); anything fancier must argue its way in
  as a new decision.
- **z is real**, backed by a **stackup** (named slabs with thickness + material; sensible defaults —
  1.6 mm board, 1 oz copper). **A "layer" is just a named z-slab**, never a primitive. Clearance is
  "roles have a rule ∧ z-ranges overlap ∧ 2D shapes within distance"; with discrete slabs "z overlaps"
  collapses to "same layer", recovering ordinary 2.5D behaviour — but the model isn't limited to it.
  Below-surface bodies (a module in a cutout, low-profile USB-C) live at *negative z*, which a fixed
  layer enum cannot express.

### Roles stay few and physical

`Conductor | Substrate | Void | Keepout(kind) | Marking | Mask | Datum`. Richness comes from
**geometry + composition (footprints)**, not from proliferating roles — the rule that keeps this from
sprawling:

- *drill / board cutout / milled pocket* → `Void` (a drill is not special; it is one void among many)
- *board outline* → the boundary of a `Substrate` prism (an arbitrary CAD-imported polygon)
- *courtyard / mechanical clearance* → `Keepout` (3D extent, for interference detection)
- *fiducial* → a footprint with `Conductor` + a `Void` mask opening (no new role; mask is
  positive `Mask` material, openings are ordinary deletion volumes — [d13](log/d13-slab-name-identity.md))
- *mouse-bite* → a footprint with `Void` perforations and **no** `Conductor`
- *thermal relief* → a `Conductor` pad whose `Shape2D` *is* the spoke-and-gap geometry

The `Role` enum stays extensible, but we resist growing it: a named PCB feature is a composition over
the base set, not a new kind.

### Why this is the right foundation

- **The prismatic-matter assumption is named, justified, and fenced** (ruled in [d16](log/d16-area-unified-producer.md), 16d). The *spatial*
  vocabulary is fully 3D — poses are integer quaternions ([d06](log/d06-integer-quaternion-orient.md)), z is authoritative `Nm`
  ([d02](log/d02-z-authoritative.md)), slabs are z-intervals — but *matter occupancy* is deliberately prismatic (extrusions
  along the board normal). Two justifications: (1) the manufacturing process only makes prisms —
  etching, lamination, plating, drilling are extrusions along one axis, and Gerber/Excellon cannot
  express anything else, so prisms are **exact** for everything a fab can build; (2) exact integer
  booleans are achievable in 2D (the region kernel) and are a research problem in 3D — a 3D-matter
  model would cost the zero-dependency exactness the DRC's honesty is built on. Named escape
  hatches, so lifting the ceiling is additive rather than a migration: rigid-flex/folded boards
  become an *assembly* level above features (rigid sections, each locally prismatic, posed in 3D by
  the existing quaternion machinery); tilted component bodies become a separate posed body-volume
  feature kind (visualization/interference, never in the exact-DRC path). `Extent::Solid` stays
  *reserved, not built*; data accumulated under this assumption (quaternions, Nm z-ranges) remains
  valid in a true-3D future.
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

### The convergence — current model

This section *is* the design of record for geometry; the full decision-by-decision
narrative (findings, rejected alternatives, staging, review history) lives in the
project log ([`docs/log/`](log/README.md), entries [d01](log/d01-feature-single-currency.md)–[d19](log/d19-punchable-planes.md)).
The model as it stands:

**Identity and vocabulary** (ruled in [d13](log/d13-slab-name-identity.md)). A "layer" is a **slab name** — a named
z-interval in the authored `Stackup`, carrying a `Role`. Slab names are the universal
layer vocabulary: regions, text, footprint graphics, and routed copper all reference
slabs by name; the name is a reference, the role is the meaning (a slab named
`F.SilkS` with a non-Marking role silently drops from every output, by design).
Projections (which slab is "top mask", which copper is "layer 2") are **queries,
never inputs**; there are no inverse projections. Unknown/mis-roled slab references
are hard commit diagnostics (`E_UNKNOWN_SLAB` family) — `command::apply`
re-elaborates on every transaction, so a committed doc always resolves.

**One producer, two consumers** (ruled in [d16](log/d16-area-unified-producer.md), 16c). `route::world_features` is the single
producer of world-frame features — substrate, mask solids, voids, keepouts, graphics,
text, and *all* copper (pads, traces, vias, pours). DRC and every exporter are
filters over that one stream by role/net/slab. Pours are ordinary `NetFeature`s with
`Area` fills (the former `PourFill` side-channel is deleted); a via lowers to a
conductor prism **plus** a `Void` drill prism; the board is one `Substrate` feature
carrying `Area(outline ∖ cutouts)` (the former `BoardShape` struct is deleted —
`board_region()` is the accessor). Consequences that used to be bugs: the drill file
is a forward query over through-cut `Void`s (pad **and** via drills, PTH/NPTH split —
issue 0022), and keepout + board-edge clearance are DRC-enforced (issue 0023).
Materialization failures are **fail-loud** (`expect`, never empty-clean).

**Placement honesty.** Courtyards are convex polygons in the solver, not AABB
proxies: exact-integer SAT (edge normals + vertex-vertex axes, rounded margins folded
in as `g² ≥ r²·|n|²`) drives `NoOverlap`, and an **honest verify** re-checks final
placements against the true polygons, reporting residuals > 3 µm as
`E_COURTYARD_OVERLAP` ([d10](log/d10-courtyard-polygonal-truth.md)'s third leg; resolves 0019).

**Routes are persisted state; the autorouter is an editing tool** (ruled in [d18](log/d18-routes-persisted.md), route ids in [d22](log/d22-route-identity-persists.md)).
`Trace.layer` is a slab name; `Via.span` is `Option<(String,String)>` (None = full
copper extent). All routes serialize in the text file's **state zone** (`# routes`,
beside `# overrides`): `pinned` is the keyword-less default, `free`/`hint`/`fixed`
explicit, so all four provenance values round-trip. Load = parse, never re-solve —
the router may be stochastic, anytime-improving, or replaced without touching a doc;
the serializer contract is "**re-derivable** state is not emitted" (routes are
materialized but *not derivable*: expensive, stochastic, user-blessed). Staleness is
handled by checking (DRC/ratsnest), not re-deriving. `PromoteRoutes{nets}` flips
Free→Pinned (the lockfile move); partial reroute = transactional rip-up of a
selection with pinned copper as obstacles (machinery future). `route::Layer` ordinals
survive **only inside the router grid**; commit-time `validate_routes` gates
slab/net references on every mutation path. Resolves 0011.

Route **identity** persists too (ruled in [d22](log/d22-route-identity-persists.md)):
every `route`/`via` line carries a small integer id, emitted by the serializer and
parsed back verbatim, so serialize→parse preserves `TraceId`/`ViaId` including gapped
sets — undo is identity-exact, and waivers/length-tuning/diff may key on route ids.
Ids are **advisory-but-stable, never load-bearing for correctness**: a line missing
an id gets one minted, a duplicate is re-minted (warnings, never a parse failure —
hand-editing cannot brick a file), and one engine-side allocator (`RouteIdAlloc`)
mints above the current max. The design zone is untouched — named entities need no
id syntax (§5, "two identity strategies, one per zone").

**Planes are punchable** (ruled in [d19](log/d19-punchable-planes.md)). A pour fill
is derived and self-knocking-out (`fill = outline − ⋃(foreign_copper ⊕ clearance)`),
so a **foreign pour's fill is via-permeable**: a via may land within it, and the
knockout carves the anti-pad on re-derivation — the via still needs clearance from
*authored/routed* copper on every layer; only the derived fill yields. Verification
judges proposed copper against pours *re-derived with that copper included* (the fill
that will actually exist), never the stale pre-route fill. **Same-net fills are
stitching targets**: cells over a net's own fill islands count as already-connected
tree membership, so routing a pad to its plane is a via drop the ordinary search
discovers. **Pad↔plane incidence is layer-honest**: a pin joins a pour island only
where its pad copper actually exists on that island's slab (SMD pad → its own slab;
drilled pad → every slab its barrel spans). Consequence, stated as a feature: **plane
health is a first-class, checkable property** — the per-layer pour-island ratsnest
honestly reports whether a perforated plane survived as one island. Inner-layer
*traces* through foreign planes stay blocked (legal in principle; a cost-model
question for the fenced router-research cycle).

**Coordinate exactness has a named ceiling (issue 0018).** Two constants:
`geom::MAX_COORD` = 1e9 nm (±1 m) is the inclusive **ingest** bound — `E_COORD_RANGE`
diagnostics at text/command boundaries, `Err` at the import boundaries — and
`KERNEL_SAFE_COORD` ≈ 1.276e9 is the true i128 ceiling (worst chain 64·C⁴,
compile-time-guarded) that kernel `debug_assert!`s check, leaving headroom for
world-frame composition. The diagnostics split follows rustc: **panic/ICE when the
caller is our code; `E_` diagnostic when the cause is the user's input; `W_`
warnings degrade without gating `is_clean()`** (first instances: `W_FONT_LOAD`).

**Annotation and text** (ruled in [d14](log/d14-refdes-derived-class-registry.md), [d17](log/d17-ttf-outline-text.md)). Part identity = (part, effective
params); params are authored *strings* at rest, parsed by consumers at their own
boundary (`quantity.rs` decimal-exact SI/IEC first). Refdes is **derived** (a query
with per-prefix counters), pinnable via EntityId-keyed `refdes` override lines;
labels are template cascades with display-format specs (`{value:si:Ω}`, `{value:iec}`)
that degrade verbatim on parse failure. Footprint text anchors
(Reference/Label/Literal) resolve live at lowering. Text renders through the built-in
stroke font by default, or a user-supplied TTF (`font "<path>"` directive) whose
glyphs flatten to `Area` regions — `ttf-parser` is the crate's first and only
dependency, confined to `src/ttf.rs`; load failure degrades to strokes with
`W_FONT_LOAD`. Paste is derived at export; fab is an ordinary authorable zero-height
`Datum` slab with per-slab SVG output (Gerber fab deferred).

**Bottom-side convention.** `Orient::flipped()` is Ry(180) — x-negates, y preserved
(the KiCad/fab board-turn convention; bottom silk reads upright); placement CSV
reports the authored angle for bottom parts. Orientations serialize as quaternions,
so the convention lives in one constructor.

**Still open here:** multilayer *routing* (stackup-driven layer count, via-span
selection — 0004-remainder/0008, a deliberate future design cycle); the placement
solver's approximation (0007); `Solid`/true-3D per the fenced assumption above;
whether component bodies get a dedicated role/material or reuse `Keepout`; Gerber
viewer validation (0009).

### The region kernel and arc support

The exact-integer **offset + polygon-boolean kernel** (`geom/kernel.rs`) is the one
boolean engine behind pours, mask, the substrate, and every `Shape2D::Area`: a
`Region` is a set of oriented rings under the non-zero winding rule; booleans use
exact `i128` predicates with one shared deterministic rounding; offsetting a shape is
a radius bump realised by the dilation decomposition. Skeleton **arcs** are
authoritative 3-point circular arcs on the `Path` skeleton ("strategy A"): the kernel
only ever sees a transient flattening (`Path::flatten`, inscribed ⇒ DRC optimistic by
≤ one sagitta, ~1 µm), while export reads arcs directly (`G02`/`G03`, SVG `A`). How
both were built and proven, stage by stage — including the pour/mask/DRC wiring that
has since moved under [d16](log/d16-area-unified-producer.md)'s unified producer — is
recorded in the log: [n05](log/n05-region-kernel-record.md) (the region kernel),
[n06](log/n06-arc-support-record.md) (arc support).

---

## 9. Library packages: parts as data, names as the dependency key

*(Implemented in `eutectic-core/src/library.rs` — motivated by the GUI: a `.eut` file must be
openable standalone, and part libraries must not exist only as Rust code.)*

A **library package** is a directory containing a manifest (`eutectic.lib`, see `library::MANIFEST_NAME`)
plus the asset files it references. The manifest is a hand-rolled line grammar in the `.eut` family:
`part NAME footprint=REL [symbol=REL:SYMBOL_NAME]`, optionally opening a `{ role NUMBER NAME KIND }`
block — the serialized home for the authored knowledge (pin-role relabeling, symbol↔footprint joins)
that previously lived in example code. `library::load_library(dir) -> Result<PartLib, String>` is the
import boundary; symbol joins are strict (a pad/pin mismatch is a load error).

**Path hygiene is enforced at the format level**: manifest paths are relative to the manifest's
directory; absolute paths and `..` traversal are parse errors. A package is relocatable and
self-contained by construction — the KiCad failure mode (machine-local absolute paths committed into
project files) is unrepresentable.

**Documents declare libraries by name**: the `use NAME` source directive (inert to elaboration,
round-trips through the serializer). The committed document carries only names; binding a name to a
directory is the *caller's* job — `elaborate` keeps its `PartLib` parameter, and the app resolves
`library::use_names(...)` through its registry, loads each package, and unions them
(`library::union`, deterministic first-wins with collision notes). Part references stay bare names
in v1; qualified `lib:part` names can come later without a format break.

**Unresolved parts degrade, never abort** (the permissive philosophy applied to loading): an
instance whose part is not in the lib is skipped, its downstream references are cascade-suppressed
through the same machinery as DNP drops (schematic `sym`/`wire` included), and a non-blocking
`W_UNRESOLVED_PART` finding (with a known-parts hint) rides `ReconReport`. A doc with missing
libraries still opens; the findings say what's missing.

**Dependency-manager direction** (deliberate, not yet built): the name is the resolution key, so a
future cargo-like resolver is just another way to bind it — name → pinned checkout in a
content-addressed cache, with a lockfile recording url/rev/hash per library. Those fields land in
the manifest additively. The per-machine name→path registry lives at the app layer (GUI Libraries
menu), never in the engine and never in committed artifacts.

## Open questions / hard parts (carry these forward)

- **The reconciliation / minimal-perturbation engine** is the single load-bearing risk. Precise
  semantics of: least-change solving, override decay (when does a stale override get dropped vs.
  surfaced?), explicit vs. inferred pins, and "nudge without the next rebuild moving it back, but
  don't pin over-enthusiastically." Same primitive at schematic-authoring, placement, and routing
  levels.
- **Constraint-solver UX at board scale** — over/under-constrained diagnostics, locality (solve
  regions independently), no-solution explanations.
- **Routing** — the genuinely unsolved part; interactive-first, autoroute aspirational. [d18](log/d18-routes-persisted.md)
  cleared the ground: routes persist (load never re-solves), so the router is free to be stochastic
  and anytime-improving; the open design is the router *itself* — stackup-driven multilayer
  (0004/0008), via-span selection, rip-up/partial-reroute machinery.
- **Part library** — hard because of *scale*, not difficulty. Plan: import KiCad's existing
  detailed libraries, add type information via good import tools + agent-in-the-loop editing.
  The container is now in place (§9 library packages); the scale problem — populating packages
  from KiCad's libraries with typed roles — remains.

## Prior art to mine

- **Horizon EDA** — modern C++ EDA with a more database-like model and a single shared "pool"; the
  closest existing thing to this direction. Mine for what worked / didn't.
- **atopile, tscircuit, SKiDL** — declarative/code schematic capture.
- **Chisel / SpinalHDL / Amaranth** — typed interfaces/bundles, elaboration.
- **Salsa / Adapton / rust-analyzer** — incremental demand-driven query engines.
- **TopoR / FreeRouting** — topological routing.
- **Onshape/Fusion sketch solvers** — geometric constraint solving, DOF analysis, least-change.

## Prototype status (engine core & reconciliation)

*Status audited 2026-07-11.* The milestone snapshots that built this layer — the M1
vertical slice, M2 override decay, M3's first solver — are preserved in the log
([m1](log/m1-engine-core.md), [m2](log/m2-override-decay.md),
[m3](log/m3-placement-solver.md)), along with the original roadmap
([n07](log/n07-original-roadmap.md)). What exists now:

- **Sole mutation surface.** `command::apply` (`command.rs`) — atomic, collect-all
  validated transactions over the immutable `Doc`; the `Command` algebra covers
  source replacement, overrides (`Nudge`/`Pin`/`ClearOverride`), whole-file
  `LoadText`, routing edits, report resolution (`Resolve`), and route promotion
  (`PromoteRoutes`). `history.rs` is the version DAG (commit / undo / checkout).
- **Incremental query engine.** A hand-rolled memoized engine (`query.rs`) with
  dependency tracking and early cutoff over four keys: `Netlist`, `Erc`, `Floating`,
  `Drc`. Honest limits: dependencies are recorded explicitly, not auto-tracked, and
  inputs are coarse revision counters (`conn_rev`/`geom_rev`/`route_rev`) —
  issue 0012.
- **Reconciliation & override decay.** The strength ladder is
  `Fixed > Pinned > Hint > Free` (`doc::Provenance`, `doc::Strength`): an
  ineffective Hint is garbage-collected at commit, an ineffective Pin is
  flagged-but-kept, a Pin contradicted by a Fix raises a loud conflict, a Hint
  contradicted by a Fix yields silently. Outcomes surface as a structured
  `ReconReport` (`doc.rs`) and are acted on through `Command::Resolve` (see
  "Prototype status (resolution UX)"). "Ineffective" is defined by re-solving:
  an override is ineffective iff freeing it lands the entity in the same place.
- Honest limits: the document store is `BTreeMap`-based with full clones per
  version (not persistent `im` maps) and entity id = hierarchical path string
  (issue 0015). Component position, orientation, addition, and removal all bump
  `geom_rev`, invalidating footprint-local geometry consumers.

## Prototype status (text front-end)

*Status audited 2026-07-11.* The `text` module (facade plus
`scan`/`blocks`/`directive`/`def`/`schematic`/`emit`) makes §5's "text as a
projection" concrete: one **canonical serializer + parser** covering tier-1 source
(flat directives and nested blocks), the ID-keyed `# overrides` zone, and the
machine-written `# routes` state zone ([d18](log/d18-routes-persisted.md),
[d22](log/d22-route-identity-persists.md)).

**Grammar.** One directive per line, `#` line comments, whitespace-tokenized;
`def` and `schematic` (with nested `row`/`column` containers) open `{ … }` block
bodies whose interior trivia round-trips (`text/blocks.rs`; the whitespace
normalization limit is issue 0030). The directive set (`text/directive.rs`):
`inst` (with `[lo..hi]` ranges, `if=` conditionals, `p:` params, `label=`),
`param`, `place`/`fix` and the override lines `hint`/`pin`,
`board`/`boardrect`/`cutout`/`hole`, `region`, `slab` (the authorable stackup),
`class`, `near`/`minsep`/`alignx`/`aligny`, `rotate` (any angle — non-cardinals
serialize as `quat=(…)`), `nearpin`, `text`, `font`, `use`, `connect`/`net`/`nc`,
`refdes`, and the state zone's `route`/`via` lines (persistent id + slab name +
provenance keyword, `pinned` the keyword-less default).

**Guarantees (tested).** Serialization is deterministic and canonical;
`serialize(parse(serialize(doc)))` is byte-identical; parsing is tolerant in,
canonical out. Parse errors are collect-all `Err(Vec<Diagnostic>)` naming every
offending line (`text.rs::parse`) — never a panic — and `Command::LoadText` lowers
the whole file onto the sole mutation surface in one atomic transaction. Route ids
round-trip verbatim including gapped sets; def bodies and schematic blocks
round-trip with interior comments preserved; def-free docs are byte-identical to
their pre-block serialization.

## Prototype status (physical parts)

*Status audited 2026-07-11.* Parts carry real planar geometry, and orientation is
the full [d06](log/d06-integer-quaternion-orient.md) transform:

- **Orientation is an integer quaternion** (`doc::Orient`) — no cardinal enum, no
  mirror flag. `apply` is an integer matrix·point plus one rounding division (no
  trig, no sqrt); cardinals and flips are exact tiny quaternions; `rotate <p> <deg>`
  lowers *any* angle to the best integer planar quaternion once, at parse
  (`Orient::from_angle_deg`, `ORIENT_ANGLE_SCALE = 1e6`). Bottom-side placement is
  `Orient::flipped()` = Ry(180); "which side" is derived (`Orient::is_bottom`), and
  pad layers/silk swap sides from that with no flag to keep in sync.
- **Pin offsets & world positions.** Every discrete pin and interface signal
  carries a local offset; `part::pin_world` maps it through the quaternion — exact
  for cardinal rotations, correctly-rounded otherwise
  ([d07](log/d07-derived-geometry-rounded.md)).
- **Pads are real copper + drill geometry.** `PinDef.pad: Option<PadGeo>` holds
  `PadCopper` pieces (compound pads supported) plus an optional round/slot `Drill`;
  `PinDef::pad_features` derives world-frame features from it (the
  [d12](log/d12-phase0-foundation.md) fold) for DRC, export, and the autorouter.
- **Near-to-a-pin** (`nearpin`) placement constraints work against a specific pad's
  world position; orientation remains a settable attribute, **not** a solver DOF.

Honest limits: the solver does not optimize over orientation; interface-signal
offsets live on the shared `InterfaceDef` (`part.rs`), so one interface type places
its pins identically on every part that uses it. Component position, orientation,
addition, and removal invalidate the geometry query tier through `geom_rev`.

## Prototype status (resolution UX)

*Status audited 2026-07-11; the shape is unchanged and the description verified.*
Reconciliation outcomes are actionable through the sole mutation surface:
`Command::Resolve(EntityId, Resolution)` (`command.rs`) pairs an entity with one of
a closed resolution vocabulary — `DropOrphan` (dead override), `AcceptConstraint`
(clear a pin a hard `Fix` contradicts), `RePin(Point)` (keep the pin, move it —
deliberately the user's call whether the conflict persists), `DropRedundant`
(un-pin what the solver satisfies anyway). A `Resolve` aborts the transaction
unless the entity is actually flagged in the matching `ReconReport` category, and a
successful resolution simply is not flagged again on the next re-reconcile — no
"resolved" bookkeeping. `command::suggested_resolutions(&ReconReport)` enumerates
ready-to-apply commands with rationales for a GUI/agent. Remaining gap: GUI
presentation and richer multi-issue batching.

## Prototype status (real solver)

*Status audited 2026-07-11.* The placement solver (`solve.rs`) is **projected
Gauss-Seidel with a convergence loop** — sequential constraint projection with an
implicit active set for the one-sided distance constraints, no anchor-spring
penalty term, so a part touched by no violated constraint never moves and
least-change falls out for free.

- **Convergence, not a fixed count.** Sweeps run until the max residual is below
  `RES_TOL` (1 µm), per-sweep movement stalls below `MOVE_TOL`, or the `MAX_ITERS`
  safety cap (5000) is hit; `solve::Solution { positions, converged, iters,
  unsatisfied }` records which. Infeasibility is **reported, not hidden**:
  `unsatisfied` lists exactly which constraints remain violated and by how much.
  Deterministic: no RNG, stable ordering, fixed tie-breaks, f64 working math
  rounded to integer nm on output.
- **Constraints**: board containment (clamped), `Near`, `MinSep`, `NearPin`,
  `AlignX`/`AlignY`, and `NoOverlap` — exact-integer convex **SAT over polygonal
  courtyards** (edge normals + vertex-vertex axes, rounded margins folded in as
  `g² ≥ r²·|n|²`; [d10](log/d10-courtyard-polygonal-truth.md), resolves 0019).
  Elaboration then **honestly verifies** final placements against the true
  polygons, reporting residuals above `COURTYARD_VERIFY_TOL` (3 µm) as
  `E_COURTYARD_OVERLAP`.

Honest limits (issue 0007): not a research-grade geometric constraint solver — no
DOF analysis, no decomposition into independent subsystems, no global-optimality
claim for least-change; `MinSep`/`NoOverlap` make the feasible region non-convex,
so a pathological start can settle poorly. `Solution.converged`/`unsatisfied` is
not yet threaded into `ReconReport` (noted at the call sites in `elaborate.rs`).
**Min-separation-to-a-pin is not implemented** — `MinSep` is entity-to-entity
only; the pin-relative constraint exists solely as `NearPin` (attraction), with
no repulsive counterpart.

## Prototype status (footprint import)

*Status audited 2026-07-11.* The `kicad` module (one hand-rolled S-expression
reader in `kicad/sexp.rs` feeding `footprint`/`symbol`/`outline`/`iface_infer`)
imports real KiCad data. Everything stays zero-dependency; malformed input returns
`Err(String)` (the one import surface still on strings — a noted §7 follow-up),
never a panic.

- **`import_footprint(text) -> Result<PartDef, String>`** (+ file wrapper): one
  `PinDef` per pad, offsets hand-parsed from decimal mm to integer nm
  (half-away-from-zero, no float). Pads carry **real copper + drill geometry**
  (`PadGeo`): circle/rect/roundrect/oval shapes, `custom` pads as compound copper
  including `gr_arc` edges (3-point and legacy centre/angle), round drills and
  slots. Pads sharing a pad id dedupe to the first (same electrical pad); unnamed
  pads are skipped (no electrical identity).
- **Footprint graphics** (`fp_line`/`fp_arc`/`fp_circle`/`fp_poly`/`fp_rect`) land
  as `PartDef.graphics` (`FpGraphic { shape, layer }`) with **side-relative** slab
  references ([d13](log/d13-slab-name-identity.md)); a closed courtyard outline
  becomes the authoritative polygonal `PartDef.courtyard`
  ([d10](log/d10-courtyard-polygonal-truth.md)). `fp_text`
  (reference/value/user, incl. the v7 `property` form and
  `${REFERENCE}`/`${VALUE}` variables) imports as live `FpText` anchors
  ([d14](log/d14-refdes-derived-class-registry.md)) — never frozen strings.
- **`import_board_outline`** reads a `.kicad_pcb`'s Edge.Cuts into a board outline
  + cutouts (issue 0017).
- **Roles**: a footprint alone is electrically role-less by design;
  `apply_role_map` overlays `(pad_number, name, role)` without authoring a symbol,
  and the symbol join below supplies the real roles.

## Prototype status (symbol/role layer)

*Status audited 2026-07-11.* The electrical truth a footprint lacks comes from the
schematic symbol, joined by pad number:

- **Symbol import.** `import_symbol` / `import_symbol_named` parse a `.kicad_sym`
  (same S-expr reader), recursing into child unit symbols so multi-unit parts yield
  all pins; pins dedupe by number. The KiCad electrical-type vocabulary is a closed
  enum — an unknown token is a parse **error** — mapped conservatively to
  `PinRole` (`power_in → PowerIn`, …; `tri_state`/`open_collector`/`open_emitter`
  and friends map to `Passive` so ERC never invents a spurious driver conflict;
  refine when ERC grows wired-OR rules — issue 0014).
- **The join.** `join_symbol_footprint` is tolerant: one pin per footprint pad
  (geometry source of truth), symbol matches supply functional **name** + mapped
  **role**; mismatches are *reported, never dropped* (`JoinReport.symbol_only` /
  `footprint_only`). `import_part` is the strict wrapper — any mismatch errors,
  so a dropped power pin cannot pass unnoticed.
- **Typed-interface inference now exists** (`kicad/iface_infer.rs`; resolves
  issue 0010). `infer_interfaces` matches joined pin names against a built-in
  pattern registry (UART/SWD/I²C/…) and attaches `InterfaceDef`s
  **identity-unified on pad numbers**. It **never guesses**: an interface attaches
  only for a complete, unambiguous signal set per instance (`UART0_TX`/`UART0_RX`
  → `uart0`), and inference never overwrites an explicit port. `apply_interface`
  is the explicit overlay for what inference cannot see.

Honest limits: alternate-function pin names (`(alternate …)`) are ignored — only
the primary name is used; symbol body graphics are not imported (the
[d23](log/d23-schematic-features-tier.md) artwork seam is where they will land).

## Prototype status (pin identity)

*Status audited 2026-07-11; verified accurate as written.*

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

*Status audited 2026-07-11.* The `export` module
(`netlist`/`placement`/`svg`/`svg_writer`/`features` + the Gerber/Excellon
backends below) turns a `Doc` (+ `PartLib`) into deterministic, diffable
artifacts. Every exporter is a **pure function** — no wall-clock, no randomness,
stable `BTreeMap` iteration, integer-arithmetic coordinate formatting — so output
is byte-stable and a one-thing change yields a one-line diff.

- **`netlist(doc)`** — one net per line, nets in `NetId` order, pins in `PinRef`
  order: the artifact a fabricated/assembled board is checked against.
- **`placement_csv(doc)`** — pick-and-place, six-decimal mm by pure integer
  arithmetic; bottom-side parts report the **authored** angle with the Ry(180)
  flip decomposed out (KiCad `.pos` style, `side=B`).
- **`svg(doc, lib)`** — the board sketch: the real board region (outline ∖
  cutouts; curved edges polylined), components with **real pad copper**, routed
  traces/vias classed per slab, translucent pour fills (even-odd, knockouts read
  as voids), and silk/marking surface geometry classed by z-derived side. Y is
  flipped once so the ECAD y-up model renders upright.
- **`svg_fab` / `fab_svg_set`** — one fab drawing per `Role::Datum` slab
  ([d15](log/d15-paste-derived-fab-slab.md)).
- **`schematic_svg(doc, lib)`** — the schematic view, serialized from the
  `schematic_features` stream ([d23](log/d23-schematic-features-tier.md)); the
  headless/agent artifact and test oracle (golden fixture committed). Lives in
  its own top-level `schematic_svg` module (a sibling of `export` that reuses its
  SVG helpers), not inside `export`.
- **Gerber + Excellon** — see "Prototype status (Gerber/fab output)".

## Prototype status (routing core)

*Status audited 2026-07-11.* Routed copper is tier-2 document state
(`route/model.rs`), fully serialized and identity-stable:

- **Representation.** `Trace { net, layer: <slab name>, polyline, width,
  provenance }`; `Via { net, at, drill, pad, span: Option<(slab, slab)> }` (`None`
  = full copper extent). Both are id-keyed maps in `Doc`; ids are persistent
  ([d22](log/d22-route-identity-persists.md); one `RouteIdAlloc` in `id.rs` mints
  above the current max, saturating). Layer identity is the slab **name**;
  `route::Layer` ordinals are router-internal working forms only (documented on
  the enum). All four provenance values (`Pinned` default, `Free`, `Hint`,
  `Fixed`) round-trip through the `# routes` state zone
  ([d18](log/d18-routes-persisted.md)).
- **Commands.** `AddTrace`/`RemoveTrace`/`AddVia`/`RemoveVia` validate atomically;
  commit-time `validate_routes` (`command.rs`) gates **every** mutation path on
  slab/net references (`E_UNKNOWN_SLAB`/`E_NON_COPPER_SLAB`/`E_UNKNOWN_NET`) — so
  a source edit that deletes a net still carrying copper is refused at commit, not
  silently orphaned. `PromoteRoutes { nets }` flips Free→Pinned (the lockfile
  move). A route edit bumps only `route_rev`.
- **DRC** (`route/drc.rs`, the `Key::Drc` query) runs over the unified
  `world_features` stream: **min width**; **clearance** edge-to-edge between
  different-net copper via the z-aware exact-integer feature kernel — pads are
  their true copper extents on their true slabs, via barrels span layers;
  **keepout intrusion** (`Role::Keepout`, Copper/Route kinds — issue 0023);
  **board-edge clearance**; and **ratsnest connectivity** — union-find over each
  net's pins, traces, vias, and **pour islands**, with layer-honest pad↔island
  incidence ([d19](log/d19-punchable-planes.md)c, `route/connect.rs`: an SMD pad
  joins only an island on its own slab, a drilled pad every slab its barrel
  spans). The violation set is canonical and de-duped (clearance keyed by
  `(net, net, layer)`; location-bearing variants remain a refinement).

Honest limits: the `Drc` query still uses `DesignRules::default()` (`query.rs`) —
wiring rules to a per-board process definition is the documented follow-up.
**Netless copper is invisible to the clearance check** — the conductor list is
built `net = nf.net.as_ref()?` (`route/drc.rs`), so a floating/mounting pad's
copper (`net == None`) participates in no clearance pair. And **ratsnest
connectivity is tolerance-based incidence, not true overlap** — union-find joins
features within `DesignRules::touch_tol` (`route/model.rs`, default 0.01 mm; live
at `route/drc.rs`), not by a geometric-intersection test.

## Prototype status (autorouter)

*Status audited 2026-07-11.* The `autoroute` module (driver facade +
`grid`/`obstacles`/`ingest`/`search`) is the transaction-proposer §1 prescribes:
a pure function returning proposed `AddTrace`/`AddVia` commands (all
`Provenance::Free`) plus routed/unrouted accounting; applying them goes through
the ordinary atomic command path.

- **Honest obstacles.** Blocked cells derive from `route::world_features` — the
  same unified stream DRC reads: real pad **extents** (not points), other-net
  traces/vias on their true slabs, copper **pours** (`Area` conductors), and hard
  `Role::Keepout` copper/route regions; inner-layer copper is not dropped. A
  padless terminal (toy library) still stamps a point obstacle for other nets.
- **Genuinely N-layer.** The grid spans **all** copper slabs of the stackup; A*
  searches `(i, j, layer)` with via moves between adjacent layers at a
  per-crossed-layer cost; a through via needs room on every copper layer at its
  site. The board mask carves the grid to the real outline ∖ cutouts and pulls
  back by the edge clearance.
- **Trace/via pitch split.** Grid pitch is `min_trace_width + min_clearance` —
  fine enough for 0.4 mm pad pitch; via legality is a separate per-cell mask plus
  an owner-ring check (a via must keep `via_pad/2 + width/2 + clearance` from
  other nets' same-run copper).
- **Plane semantics** ([d19](log/d19-punchable-planes.md)): foreign derived pour
  fills are via-permeable; a net's **own** pour fill (and its committed copper)
  seeds the connected tree, so pad→plane stitching vias fall out of the ordinary
  search; `verify_and_prune` re-checks every proposed net against the real DRC
  **with pours re-derived including the proposal** — construction invariants are
  not trusted, and `routed` means DRC-clean. A failing net is reported, its
  claims rolled back; it never emits partial or overlapping copper.

**Honest limits (issue 0008 owns the next design cycle):** greedy net-by-net —
no rip-up/negotiation, no topological/push-and-shove, no length/impedance
matching, no net-ordering optimization, no per-layer H/V directionality bias;
net ordering therefore matters. Vias are always through-span (blind/buried out
of scope). Honesty over count: on the dense PoC board the conservative whole-net
verification keeps 2/44 nets (the search itself finds 21/44) — the measurement
that scopes the router-research cycle (see
[`poc-rp2350-result.md`](poc-rp2350-result.md)).

## Prototype status (Gerber/fab output)

*Status audited 2026-07-11.* `export::gerber_set(doc, lib)` emits the fab
fileset — deterministic, byte-stable, every coordinate flowing from integer nm
into `%FSLAX46Y46*%` mm (the integer written *is* the nanometre value):

- **Copper** — `gerber_layer` per copper slab, stack-up order
  (`board-F_Cu.gbr`, `board-In<n>_Cu.gbr`, `board-B_Cu.gbr`): trace centrelines
  as round-aperture draws, via pads flashed on every slab their span covers,
  component pads flashed from their **real pad copper** by shape, and pour fills
  as `G36`/`G37` region blocks whose knockout holes come out as voids.
- **Mask** — `gerber_mask` per `Role::Mask` slab, a **forward query** drawing the
  openings (the export-format convention stays outside the model): the pad
  `Void`s at that mask slab's z (pad copper inflated by `MASK_EXPANSION`;
  through-hole pads open both sides, vias are tented) plus board cutouts as
  region fills.
- **Silk / fab** — `gerber_silk` per `Role::Marking` slab and `gerber_fab` per
  `Role::Datum` slab ([d15](log/d15-paste-derived-fab-slab.md)), over the same
  role-surface derivation as the SVG renders.
- **Edge.Cuts** — the real board region (outline ∖ cutouts): every ring drawn as
  a closed thin-pen contour. Curved edges and round cutouts are polylined —
  per the hole/void rule ([d16](log/d16-area-unified-producer.md)b), an `Area`
  hole *is* a routed contour; its diameter is gone by design.
- **Drill** — `excellon_drill` is a **forward query over through-cut `Void`
  features**: pad *and* via drills (issue 0022), split by plating into
  `board-PTH.drl` / `board-NPTH.drl`, round holes as coordinates, slots as `G85`.

**Honest limits.** Still **not validated against a real Gerber viewer**
(issue 0009). Basic Gerber apertures cannot express rounded/rotated/custom pad
shapes, so those pads **flash as their bounding rectangle** (`export/gerber.rs`)
— a conservative stand-in at flash fidelity, while DRC checks the exact shapes;
routing complex pads through region fills instead is a focused follow-up. Copper
Gerber/SVG still re-walk the `Doc` rather than filtering `world_features` by
provenance (the one known producer duplication — see gui-architecture.md's
engine rider). The output base filename is the fixed `board`.
