---
id: n07
title: "Original roadmap (the first build order)"
date: 2026-06-28
status: overtaken by events (engine core, placement solver, KiCad import, routing + DRC + autorouter, and fab export are all built)
---

> Context: current subsystem state is the "Prototype status" sections of architecture.md.
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

## Roadmap (historical)

> This was the original build order. It has since been overtaken by events: the engine core, the
> placement solver, KiCad import, routing + DRC + a basic autorouter, and fab export are all built.
> See the "Prototype status (...)" sections below and [`../../README.md`](../../README.md) for the current
> state; the items here are kept to show the sequence the work actually followed.

1. **This document** — the synced design of record. ✅ done
2. **Prototype the engine core only** — fact store + command algebra + Salsa-style query layer +
   the reconciliation/override engine. Not the GUI, not the router (large but architecturally
   conventional). The novel, load-bearing risk is the data engine; prove or break the central bet
   first. ✅ done — see "Prototype status (M1)" below.
3. **Prior-art pass on Horizon EDA's data model** specifically. (Sources cloned in `reference/`.)
