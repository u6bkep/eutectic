---
id: m1
title: "Prototype status snapshot: M1 — the engine core vertical slice"
date: 2026-06-28
status: snapshot (superseded by growth; the architecture it proved — commands, query engine, reconciliation — is current)
---

> Context: current subsystem state is the "Prototype status" sections of architecture.md.
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

## Prototype status (M1)

A zero-dependency Rust crate (`eutectic-core`, edition 2024) implements a full vertical slice of the
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
