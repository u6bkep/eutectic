# ecad-core

A from-scratch ECAD (electronic design automation) engine prototype — schematic capture and PCB
layout — built around one premise: **the agent-driven / programmatic path is a first-class citizen,
not a scripting layer bolted onto a GUI.** Rust, with a single dependency (`ttf-parser`, for font
outlines).

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
[`docs/architecture.md`](docs/architecture.md).

## How it works — the pipeline

The system is easiest to hold as a compiler pipeline: source text → parse → elaborate into a
uniform IR → analysis passes over one derived world → fab back ends. With one deliberate break in
the analogy at the solvers.

**The source language.** A board is authored as `.ecad` text — declarative directives (`part`,
`net`, `board`, `hole`, `text`, `class`, parameter overrides) plus a `# routes` state zone.
Foreign front ends (KiCad footprint/symbol/outline import, SVG import) translate into the same
directives.

**Parsing** (`text`). Text → a flat `Parsed` structure (directives, overrides, refdes pins,
traces, vias). Purely syntactic: quote-aware tokenizing and ingest validation (coordinate-range
ceilings, `E_COORD_RANGE`).

**Elaboration** (`elaborate`) — lowering, macro expansion and type checking rolled together.
Directives become the real model: parts pull footprints from the library, text renders through the
stroke font or TTF outlines into geometry, `hole` lowers to a full-stackup void, class labels
resolve, refdes annotation assigns names. Diagnostics are rustc-inspired: `E_` errors when user
input is wrong, `W_` warnings that degrade gracefully, and internal panics reserved for "our code
broke" (the ICE-vs-diagnostic split).

**The IR** is the `Doc`: entities carrying **Features** — the single geometric currency. A Feature
is a 2.5D prism: a `Shape2D` (including `Area`, an exact polygon region with holes) extruded
through named stackup slabs, with a role (conductor, mask, silk, courtyard, keepout, void).
Everything — pads, pours, text, mounting holes, the board substrate itself — is this one
primitive. Coordinates are integer nanometers; all geometry runs through an exact integer polygon
kernel (`region`: booleans and offsets, no floats).

**Analysis passes** (`route` queries). One producer, `world_features`, derives the physical world
from the Doc: pour fills are *computed* (plane outline minus foreign copper dilated by clearance —
knockouts and anti-pads fall out automatically), never stored. Everything downstream — DRC
(clearance, keepout, edge), connectivity/ratsnest (island analysis), ERC — consumes this one
derivation, so DRC and the exporters cannot disagree about what copper exists.

**The back ends** (`export`): Gerber per stackup slab (copper, mask, silk, fab), Excellon drill
files (plated/non-plated split, driven by void features), SVG renders, placement CSV, and the text
projection itself.

**Where the analogy breaks — on purpose.** In a compiler, optimization passes transform the IR
in-flight and their output is ephemeral. Here the solvers (placement packer, autorouter) are
explicitly *not* pipeline stages: the autorouter is an **editing tool**. It reads the derived
world, searches (A\* over a multi-layer grid, potentially stochastic), and writes its result back
into persistent state — the `# routes` zone of the text file. Loading a document never re-solves
anything. It is less like `-O2` and more like an IDE refactoring: the tool proposes edits, the
edits become source, and a commit-time gate (every transaction re-elaborates; `validate_routes`
checks routes against real slabs and nets) plays the type checker on the result.

The corollary is the **serializer contract**: derivable state (pour fills, DRC results, ratsnest)
is never written to the file; solver-produced state (routes, placement) always is, tagged with
provenance (`pinned` / `free` / `hint`) so a partial re-route can touch only what you select.
Text ↔ Doc round-trips byte-losslessly.

Beneath all of this sit the engine mechanics from the original design:

- **Three-tier model** — authoritative facts (netlist + constraints) / materialized solver state
  (placement, routes) / pure derived cache (ERC, DRC, ratsnest).
- **Command algebra + version DAG** — the *only* mutation surface is atomic transactions over an
  immutable document; an invalid transaction never commits (no half-applied state → the crash
  class is gone). History is a DAG → undo / branch / replay.
- **Incremental query engine** — a hand-rolled, Salsa-style memoized query layer with dependency
  tracking and early cutoff recomputes only what actually changed.
- **Least-change placement solver** — geometry is the *solution* to a constraint system; an
  unconstrained part doesn't move (the "why did it fling across the board" antidote).

## What works today

A full, if prototype-grade, flow:

**typed schematic / netlist → ERC → placement → routing → DRC → fab export**

- Typed pins & interfaces that make e.g. a serial TX/RX swap *unrepresentable*; ERC as a query.
- A generative source elaborated into instances, with ID-keyed overrides that *reconcile* on
  re-elaboration — casual nudges decay when they stop mattering; explicit pins are kept and
  conflict loudly.
- **Pad-identity net membership**: a net references pads by stable identity, and a functional name
  is a *selector* that fans out to every matching pad — an MCU's six `IOVDD` pads all connect,
  none silently float.
- A deterministic least-change constraint solver (`Near` / `MinSep` / `AlignX/Y` / `Board`
  containment / convex-courtyard `NoOverlap` packing) that satisfies feasible sets tightly and
  *reports* infeasibility.
- Import of real **KiCad footprints** (`.kicad_mod`, including graphics) and **symbols**
  (`.kicad_sym`), joined by pad number into parts with real names, electrical roles, and pad
  geometry; **SVG import** for arbitrary outlines/art.
- **Named stackup slabs** with real non-copper layers: solder mask (solids + openings), silkscreen
  (including board text via a built-in stroke font or **TTF outlines** with kerning), paste, fab.
- **Net-bound copper pours / planes** on any copper slab: derived, self-knocking-out fills with
  automatic anti-pads, via-permeable to stitching vias, with pour-island connectivity honesty.
- A multi-layer trace/via routing representation serialized in the text source; a DRC query
  (clearance / keepout / edge / ratsnest over derived world geometry); an honest N-layer grid
  autorouter whose `routed` claim means *committed-board-DRC-connected*.
- Export: netlist, pick-and-place CSV, SVG, **Gerber (RS-274X)** per slab, and **Excellon** drill
  (plated/non-plated split, slots).

**559 tests (510 lib + 49 integration), one dependency, `cargo clippy --all-targets` clean.**

## Modules (`src/`)

Most subsystems are a facade module (`foo.rs`) over a submodule directory (`foo/`);
the "key submodules" note names the internal split where there is one.

| Module | Responsibility |
|---|---|
| `doc` | The immutable three-tier document; provenance-tagged DOFs; coarse query inputs. |
| `ir` | The directive/source intermediate representation (`GenDirective`, defs) the text tier parses into and elaboration consumes. |
| `coord` | Integer-nanometer `Nm` / `Point` coordinate primitives (the geometry base type). |
| `command` | The sole mutation surface — atomic transactions; the commit-time re-elaboration gate. |
| `history` | Version DAG (undo / branch / replay). |
| `query` | Hand-rolled incremental query engine (Netlist, ERC, DRC); dependency tracking + early cutoff. |
| `text` | Parser + canonical serializer for the `.ecad` source (the agent/git authoring surface). |
| `elaborate` | Directives → instances; override reconciliation/decay; text/hole/class lowering. Key submodules: `expr` (the expression tier), `features`/`query` (the forward views), `expand` (def-expansion). |
| `annotate` | Refdes annotation with explicit pinning. |
| `diagnostic` | The rustc-style diagnostic machinery (`E_` errors, `W_` warnings). |
| `geom` | The 2.5D geometry foundation: `Shape2D` (incl. hole-capable `Area`), the `Feature`/z-stackup model, and the exact-integer boolean/offset clearance kernel. Key submodules: `shape`, `feature`, `kernel` (the former `region` polygon kernel — booleans + offsets for pours, masks, text, courtyards), `seg` (shared point/segment predicates), `limits` (coordinate ceilings). |
| `quantity` | Integer-nanometer lengths and units. |
| `part` | Typed pins, roles, interfaces, pad geometry; the built-in toy library. |
| `kicad` | Import `.kicad_mod` footprints (+ graphics) and `.kicad_sym` symbols. Key submodule: `iface_infer` (conservative interface inference). |
| `svg_import` | SVG path import → regions. |
| `font` / `ttf` | Built-in stroke font; TTF outline rendering + kerning (the one dependency lives here). |
| `solve` | Least-change placement constraint solver; convex-courtyard packing; feasibility reporting. |
| `route` | Trace/via/slab model; the `world_features` producer + `doc_netlist` membership map; DRC; pours; connectivity/ratsnest. Key submodules: `world`, `drc`, `connect`, `model`. |
| `autoroute` | N-layer grid A\* autorouter (an editing tool proposing transactions; DRC-verified claims). Key submodules: `grid`, `obstacles`, `ingest`, `search`. |
| `export` | netlist / pick-and-place / SVG / Gerber / Excellon output. |
| `schematic` / `schematic_svg` | Schematic view derivation and its SVG render. |
| `project` | Deterministic text projection of the model (human/debug view). |
| `id` | Stable entity / net / trace / via identifiers. |

## Build & run

The repo is a cargo workspace: `ecad-core` (the engine) + `ecad-gui` (the
damascene GUI). Examples live in `ecad-core`; run them with `-p ecad-core`.

```sh
cargo test --workspace           # 559 ecad-core tests (510 lib + 49 integration) + the ecad-gui fixtures
cargo clippy --workspace --all-targets
cargo run -p ecad-core --example m1           # typed interfaces, ERC, incremental recompute, reconciliation
cargo run -p ecad-core --example m2           # override strength, decay, constraint precedence
cargo run -p ecad-core --example m3           # least-change placement (mini RP2350-carrier scene)
cargo run -p ecad-core --example export       # netlist + pick-and-place + SVG from a small board
cargo run -p ecad-core --example autoroute    # place → autoroute → DRC-clean, end to end
cargo run -p ecad-core --example gerber       # Gerber/Excellon fab output
cargo run -p ecad-core --example schematic    # authored schematic layout → rendered schematic SVG
cargo run -p ecad-core --example svg_outline  # SVG import: outline + cutout → board → rendered SVG
cargo run -p ecad-core --example poc_multiprobe  # the capstone: 4-layer RP2350A board, full pipeline
cargo run -p ecad-gui [path.ecad]             # the GUI (opens a window; optional .ecad file to load)
```

## Repository layout

The working tree lives in a sibling worktree of a bare repository:

```
ecad/
├── .git/         bare repository
├── main/         this worktree  (src/, docs/, examples/, README.md)
├── issues/       file-based issue tracker (outside the repo; migrates to GitHub Issues on upload)
└── reference/    KiCad + Horizon EDA source mirrors (untracked, prior-art study)
```

Feature work is done in additional sibling worktrees
(`git -C ../.git worktree add ../<name> -b feat/<name>`) and merged back to `main`. The bulk of the
implementation was built this way by focused sub-agents, each adversarially reviewed before merge.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — the design of record: the reasoning (§1–§6)
  plus a "Prototype status" section per implemented subsystem, with honest limits for each.
- [`docs/geometry-model-convergence.md`](docs/geometry-model-convergence.md) — the decision record
  (Decisions 1–19) of the geometry/representation convergence: how pads, pours, masks, text, and
  holes all became one Feature primitive, and why routes are persisted state.
- [`docs/poc-rp2350-spec.md`](docs/poc-rp2350-spec.md) — design spec for the capstone proof of
  concept: a chip-down rework of a multi-SWD debug probe (bare RP2350A + JST-SH headers), used to
  drive the whole flow end-to-end on a real board.
- [`docs/poc-rp2350-result.md`](docs/poc-rp2350-result.md) — the PoC build result and, more
  valuably, the **framework-friction findings** it surfaced. See also `examples/poc_multiprobe.rs`
  and the vendored parts/outputs under `poc/`.

The capstone PoC is a real 44-component / 44-net RP2350A board, now built as a 4-layer stackup
with GND and +3V3 inner planes, mounting holes, silkscreen text, and byte-lossless text
round-trip. It is reported **honestly**: the current greedy autorouter's verified completion is
low (the search finds 21/44; conservative whole-net verification keeps 2/44), which is precisely
the measurement that scopes the next round of router work. Every earlier gap the PoC exposed is
tracked in the issue backlog.

## Honest limitations

It is a prototype. The dominant open front is **autorouter completion quality**: the router is
honest (its `routed` claim is DRC-verified against the committed board) but greedy — no rip-up,
retry, or negotiation — so verified completion on the dense capstone board is low. The placement
solver is an approximate relaxation. Smaller representation gaps (authored voids invisible to
mask/pour DRC, corner-radius serialization loss) and quality items are filed as a standing backlog
in the file-based `issues/` tracker kept beside the repo; each subsystem's honest limits are also
recorded in its "Prototype status" section in `docs/architecture.md`.
