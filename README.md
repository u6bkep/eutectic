# ecad-core

A from-scratch ECAD (electronic design automation) engine prototype — schematic capture and PCB
layout — built around one premise: **the agent-driven / programmatic path is a first-class citizen,
not a scripting layer bolted onto a GUI.** Zero-dependency Rust.

This is a research prototype, not a product. It exists to test whether a better *data
representation* makes a useful ECAD implementation fall out as a consequence.

## Why

Three durable frustrations with existing tools (KiCad as the reference point) motivate the design:

1. **No symbolic representation, worst in schematics** — the wires on screen *are* the net
   ("description by finger-painting"), a footgun factory.
2. **No constraint-based layout** — after a precise mechanical design in MCAD you re-enter part
   positions as raw floating-point numbers.
3. **Instability** — crashes, and a programmatic API that falls over, both symptoms of a mutable,
   redundant, weakly-invariant data model with a scripting layer bolted on.

The full reasoning, including the open questions and hard parts, lives in
[`docs/architecture.md`](docs/architecture.md). The short version of the architecture:

- **Three-tier model** — authoritative facts (the netlist + constraints) / materialized solver
  state (placement, routes) / pure derived cache (ERC, DRC, ratsnest). The interesting design work
  is at the boundaries.
- **Provenance per degree of freedom** — every position/route is `Free` (solver-driven), `Hint`
  (weak nudge), `Pinned` (explicit), or `Fixed` (hard constraint). One bit unifies "leave it where
  the solver wants" vs. "I placed this by hand," for both placement and routing.
- **Command algebra + version DAG** — the *only* mutation surface is atomic transactions over an
  immutable document; an invalid transaction never commits (no half-applied state → the crash class
  is gone). History is a DAG → undo / branch / replay.
- **Incremental query engine** — a hand-rolled, Salsa-style memoized query layer with dependency
  tracking and early cutoff computes the derived tier (Netlist, ERC, DRC) and recomputes only what
  actually changed.
- **Model-as-truth, text-as-projection** — the structured model is the single source of truth; the
  text form is a *deterministic rendering* of it (and the agent's authoring surface), so there is
  no second artifact to keep in sync.
- **Least-change placement solver** — geometry is the *solution* to a constraint system; an
  unconstrained part doesn't move (the "why did it fling across the board" antidote).

## What works today

A full, if prototype-grade, flow:

**typed schematic / netlist → ERC → placement → routing → DRC → fab export**

- Typed pins & interfaces that make e.g. a serial TX/RX swap *unrepresentable*; ERC as a query.
- A generative source elaborated into instances, with ID-keyed overrides that *reconcile* on
  re-elaboration — casual nudges decay when they stop mattering; explicit pins are kept and conflict
  loudly.
- A deterministic least-change constraint solver (`Near` / `MinSep` / `AlignX/Y` / `Board`
  containment) that satisfies feasible sets tightly and *reports* infeasibility.
- Import of real **KiCad footprints** (`.kicad_mod`) and **symbols** (`.kicad_sym`), joined by pad
  number into parts with real names, electrical roles, and pad geometry.
- A trace/via/layer routing representation, a DRC query (clearance / min-width / ratsnest), and a
  basic deterministic grid autorouter.
- Export: netlist, pick-and-place CSV, SVG, **Gerber (RS-274X)** and **Excellon** drill.

**93 tests, zero dependencies, `cargo clippy --all-targets` clean.**

## Modules (`src/`)

| Module | Responsibility |
|---|---|
| `doc` | The immutable three-tier document; provenance-tagged DOFs; coarse query inputs. |
| `command` | The sole mutation surface — atomic transactions. |
| `history` | Version DAG (undo / branch / replay). |
| `query` | Hand-rolled incremental query engine (Netlist, ERC, DRC); dependency tracking + early cutoff. |
| `elaborate` | Generative source → instances + ID-keyed override reconciliation/decay. |
| `solve` | Least-change placement constraint solver with feasibility reporting. |
| `part` | Typed pins, roles, interfaces, pad geometry; the built-in toy library. |
| `kicad` | Import `.kicad_mod` footprints + `.kicad_sym` symbols; join into roled parts. |
| `route` | Trace/via/layer representation + DRC (clearance / width / ratsnest). |
| `autoroute` | Basic deterministic grid/maze A* autorouter (a transaction-proposer). |
| `text` | Canonical serializer + parser for the tier-1 source (the agent/git authoring surface). |
| `export` | netlist / pick-and-place / SVG / Gerber / Excellon output. |
| `project` | Deterministic text projection of the model (human/debug view). |
| `id` | Stable entity / net / trace / via identifiers. |

## Build & run

```sh
cargo test                       # 93 tests
cargo clippy --all-targets
cargo run --example m1           # typed interfaces, ERC, incremental recompute, reconciliation
cargo run --example m2           # override strength, decay, constraint precedence
cargo run --example m3           # least-change placement (mini RP2350-carrier scene)
cargo run --example export       # netlist + pick-and-place + SVG from a small board
cargo run --example autoroute    # place → autoroute → DRC-clean, end to end
cargo run --example gerber       # Gerber/Excellon fab output
```

## Repository layout

The working tree lives in a sibling worktree of a bare repository:

```
ecad/
├── .git/         bare repository
├── main/         this worktree  (src/, docs/, examples/, README.md)
└── reference/    KiCad + Horizon EDA source mirrors (untracked, prior-art study)
```

Feature work is done in additional sibling worktrees
(`git -C ../.git worktree add ../<name> -b feat/<name>`) and merged back to `main`. The bulk of the
implementation was built this way by focused sub-agents, each verified before merge.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the design of record: the reasoning (§1–§6) plus
  a "Prototype status" section per implemented subsystem, with honest limits for each.
- [`docs/poc-rp2350-spec.md`](docs/poc-rp2350-spec.md) — design spec for the capstone proof of
  concept: a chip-down rework of a multi-SWD debug probe (bare RP2350 + JST-SH headers), used to
  drive the whole flow end-to-end on a real board.

## Honest limitations

It is a prototype. Among the known gaps (see `docs/architecture.md` for the full, current list): the
autorouter is a basic greedy grid router (no rip-up/retry, topological, or length/impedance
matching); DRC treats pads as points; Gerber output is not viewer-validated; the placement solver is
an approximate relaxation, not a DOF-decomposing geometric solver; query dependencies are recorded
explicitly rather than auto-tracked; and persistent structural-sharing maps (`im`) are a noted
production swap for the current `BTreeMap`s.
