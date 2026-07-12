---
id: d21
title: "The source language: declarative core, hermetic expressions, `def` reuse; computation stays at the rim"
date: 2026-07-04
status: implemented (2026-07-04, main `468fe23` — branches feat/expr-tier, feat/def-construct; interface-typed def ports descoped, fails loud; comment-trivia normalization filed as 0030)
---

> Context: restated in [architecture.md §5](../architecture.md#5-source-representation-model-as-truth-text-as-a-projection).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 21 — the source language: declarative core, hermetic expressions, `def` reuse; computation stays at the rim (2026-07-04, implemented same day — see Status header)

**§21a `def` implemented (branch feat/def-construct).** Grammar:
`def <name> [param <k>=<default> ...] { body }`, a **top-level-only** block (a def body
may *instantiate* another def but may not *define* one — a nested `def { … }` is a hard
`E_DEF`). Body directives are `inst` / `net` / `nc` / `connect` / `port`; layout,
placement, board/stackup, and route directives are out of a def body in v1 (Phase 3 owns
layout trees inside defs). Instantiation reuses the ordinary `inst` grammar
(`inst <path> <def-name> [p:<k>=<v|(expr)>] [if=] [range]`); part-vs-def is decided by
name lookup at elaboration, and a def whose name **also** names a library part is rejected
as `E_DEF_PART_AMBIGUOUS` (surface the collision, never silently let one win). Ports are
**bare typed** v1: `port <name> = <internal-path>.<selector>` exposes an internal pin
outward; a connection to `<inst>.<port>` resolves through to the bound pin's **pad
identity** (no new namespace — Decision 21's pad-number-is-identity), transitively (a def
may re-export a nested def's port). Named-InterfaceDef ports (`port <name> : <iface>`) are
**descoped** — not implemented; the interface-port spelling fails loud rather than silently.
Elaboration stamps a def body per instantiation with **path prefixing** (`sense[0].R1`,
internal nets `sense[0].fb`); def params bind from the instantiation's `p:` (defaults
else), evaluated in the def's scope where **outer doc params are visible and a def param
shadows an outer one of the same name** (innermost-wins). The **range loop variable `i` is
deliberately NOT visible inside a def body** — the body is a pure function of its declared
params; a ranged instantiation (`inst sense[0..n] S`) binds `i` only in its own
`if=`/`p:`, so to use the index inside, forward it explicitly (`inst sense[0..n] S
p:idx=(i)`). A body reference to `i` is an `E_EXPR` unknown variable (pinned by test).
Recursion (a def reaching itself through any chain) is `E_DEF_CYCLE` naming the cycle —
*dynamic* detection: a cycle reachable only through a false `if=` is never walked, hence
silent by design. Nesting is depth-capped (`MAX_DEF_DEPTH`). `if=false` on a def instance
drops the whole stamped subtree; an external ref to the instance **or to any pin beneath
it** (`net OUT a.R1.p2` when `inst a … if=false`) dangles as `W_DNP` via a prefix rule
(path == dropped **or** `<dropped>.…`), never a hard `E_UNKNOWN_INSTANCE`. An authored
top-level `net` whose name collides with a stamped def-internal net (`net sense[0].fb …`
vs instance `sense[0]`'s internal `fb`) is a hard `E_DEF_NET_COLLISION` naming both sides —
silent merge (a silent-wrong-connectivity class) is refused; deliberate internal-net
tapping is a future feature behind explicit syntax. Refdes stays **board-global flat** over
the hierarchical paths (stamped instances flow through annotation unchanged). Def bodies
round-trip (interior trivia preserved via `DefNode`); ports serialize in canonical name
order after the body; def-free docs stay byte-identical.

Settles, deliberately and in advance, the language question that `def` + parameters +
ranges would otherwise decide by accretion — the **Onshape trap**: FeatureScript began
as a text representation for a kernel wrapper and grew language features one
reasonable step at a time until it was a janky JavaScript nobody chose. We are
standing at exactly that first step, so we choose now.

**21a — the truth, restated precisely.** The source of truth is not the netlist — it
is the **generative description**; the flat netlist is its first derived view (it is
already a query), the schematic its second. Reuse today exists only as Rust functions
(`psu_module(n)`), i.e. outside the source language. Decision 21 completes the
language instead: a **`def`** is a named sub-circuit — parts, internal nets, optionally
its Decision-20 layout tree — with a **typed I/O surface** (bare typed pins and/or a
named `InterfaceDef`, the part-level typed-mating machinery lifted one level), declared
parameters with defaults, instantiable at a hierarchical path. The mental model is the
React component (`def` ≈ component, ports ≈ props) — the deepest well of agent fluency
for "reusable thing with a typed surface." Nesting is allowed (paths compose); refdes
annotation stays board-global flat (industry convention) while paths stay
hierarchical; internal nets elaborate path-prefixed (`sense[0].fb`).

**21b — the expression tier is hermetic and non-Turing-complete, and that is an
architectural invariant, not taste.** Two existing commitments force it:

1. **Elaboration is the commit gate** — it runs on every transaction, and eventually
   at interactive GUI rates. The document language must therefore be pure,
   deterministic, terminating, and ~O(output size). A Turing-complete document breaks
   all four and degrades diagnostics from "typecheck errors" to "your script crashed."
2. **Reconciliation requires stable identity.** ID-keyed overrides address generated
   instances by *source-analyzable* paths; parameterized ranges keep `sense[2].R1`
   stable under `n: 3→4`. Arbitrary code generating instances makes identity an
   accident of execution order (Onshape's derived-feature fragility).

v1 power budget (HCL/Terraform-shaped — the other deep agent-fluency well):
**parameters + arithmetic + bounded ranges (`[0..n]` iteration) + a conditional for
population variants (DNP)**. Explicitly excluded: user-defined functions, string
manipulation, recursion, unbounded loops, I/O. Functions are where "expression layer"
quietly becomes "language."

**21c — the Onshape clause.** If in-document scripting is ever truly needed, we embed
an existing hermetic language (Starlark is the standing candidate: deterministic,
sandboxed, built for exactly this at Bazel) — **we never grow our own.** General
computation meanwhile stays **at the rim**, where it already lives: agents and Rust
programs (the command API, `psu_module`-style generators) are the Turing-complete
layer, and the document is the *output* of computation, never the site of it. Rust
"not being real-time" stops mattering because Rust never runs inside the commit gate.

**21d — the three-mode editing model (human-first-class, not agent-only).** The GUI
is a full authoring surface, not an override editor — possible precisely because the
document is declarative data, not code (the second half of the Onshape trap is a GUI
and a language fighting over the same file; you cannot WYSIWYG-author a program, but
you can WYSIWYG-author records):

1. **Flat authored content** (instances, nets, placements, layout containers): full
   GUI CRUD through the same command algebra the agent uses. The blank-canvas EE
   workflow — open an empty schematic, drop parts, draw wires — is first-class with
   zero text contact; the text updates because text is a projection.
2. **Parameters**: direct GUI editing — they are named data (`n: 3→4` binds to a
   slider without source surgery).
3. **Generated content** (instances stamped from a `def`/range): per-instance edits
   are ID-keyed overrides (survive re-elaboration, decay when stale) — *and* the def
   itself is editable, because a def body is just mode-1 content in a scope: "edit
   the component" opens the def as its own canvas (the Figma component model,
   including the creation gesture: select a cluster → "make component" → the GUI
   extracts a def and replaces the selection with an instance).

Only the expression tier is text-exclusive — progressive disclosure, not relegation.

**Filed as a design requirement, not a future bug report:** mixed authorship of the
text form. When the GUI mutates the doc and the file re-serializes, hand-written
artifacts — comments, ordering, grouping chosen for readability by an agent or human —
need an explicit preservation story (comment anchoring, stable section ordering)
beyond today's canonical-serialization determinism.
