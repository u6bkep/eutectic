---
id: m2
title: "Prototype status snapshot: M2 — override decay and reconciliation precedence"
date: 2026-06-28
status: snapshot (the Fix > Pin > Hint > default ladder and decay rules are current engine semantics)
---

> Context: current subsystem state is the "Prototype status" sections of architecture.md.
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

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
