---
id: d14
title: "Refdes/label are derived display; params are strings; the class registry holds the conventions"
date: 2026-07-02
status: implemented (2026-07-03, main `659d82a` — branches feat/class-registry, feat/auto-text; refdes pinning 2026-07-03, branch feat/refdes-pin — see [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Annotation and text").
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 14 — refdes/label are derived display; params are strings; class registry holds the conventions (2026-07-02)

Auto-text (the 0016 follow-up) forced the question "what does a `Reference`/`Value`
text anchor resolve to?", and the answer exposed two needs that must not share a field:
**part identity** (exactly what is placed — for the BOM, and eventually simulation) and
**display** (what the silk says). The model conflates neither with the identity spine:
`EntityId` (the hierarchical instance path) stays untouched as source identity; a
reference designator is a *different namespace* — flat, compact, conventionally
prefixed, consumed by manufacturing-time humans — and is therefore **derived**, the
classic annotation pass recast as a query.

**Identity: `(part, effective params)`.** `Component` gains
`params: BTreeMap<String, String>` (empty for most ICs — an MCU's identity is its
`PartDef` name; a resistor's is its parameter set). Params are **authored strings at
rest** — the display-normal spelling (`4.7k`) is the source of truth, and **consumers
parse at their own boundary** (the label formatter today, simulation later, at which
point a commit-time `E_BAD_QUANTITY` diagnostic can arrive *for the params that
consumer reads*). No speculative type ontology: the key vocabulary approaches the
number of component kinds, and typed storage would have to re-format authored
spellings for display (owning SI-prefix formatting and drift between "what was typed"
and "what the silk says"). MPN/sourcing is a *later BOM-export resolution* of
(footprint, params) → orderable part — a lookup table in a future BOM module, not a
`Component` field.

**Display: `label: Option<String>`** on `Component` — optional, cosmetic, no identity
weight. Display derives from identity, never the reverse.

**The class registry** is one authored, seeded table keying everything conventional:

```
class → { prefix?, template?, defaults? }
```

- `class(comp)` query: `PartDef.class` override, else the leading alpha run of the
  part name (`R_0402`→`R`, `LED_0603`→`LED`), else `U`. One concept, two consumers.
- `prefix` (default: the class name itself) feeds the **refdes annotation query**:
  deterministic per-class numbering over components in path order. Insertion-unstable
  by accepted trade-off; the EntityId-keyed **override system is the reserved stability
  mechanism** (pin assignments when a board ships) — not built now, kept open.
- `template` feeds the **label query**: instance `label` (itself a template) →
  registry template → built-in `"{value}"`; if the rendered result is empty
  (referenced keys absent), fall through to the part name — one rule covers passives
  *and* ICs before any table entry is authored.
- `defaults` are class-default params (`R → tol=5%`); instance params override, and
  BOM identity uses the *merged* effective params.

**Template display semantics** keep the software unopinionated: `{key}` substitutes
verbatim (authored spelling wins); `{key:si:Ω}` and `{key:iec}` parse-and-render
(`2.6kΩ` vs `2R6` — the convention lives in the user's table entry). Parse failure
degrades to verbatim substitution, never an error. The quantity parser is the first
boundary-parser and the one simulation inherits.

**Text anchors** (the auto-text mechanism): `PartDef` gains `texts: Vec<FpText>` with
`kind: Reference | Label | Literal(String)` — an *anchor* (position, height, layer,
orient), never a frozen string, per Decision 9 (strokes derived) and the salsa
principle (refdes edits re-render; it's a query over component state). KiCad
`fp_text reference "REF**"` imports as a `Reference` anchor, discarding the
placeholder; `fp_text value` imports as `Label` (our vocabulary does not inherit
KiCad's identity/display conflation). Footprint text generates strokes in
footprint-local frame through the same `to_world` as graphics — bottom-side mirroring
falls out of the orientation quaternion with zero special-case code. The shared stroke
lowering gains **justification** (KiCad text is center-anchored, board text stays
left-origin; content is live, so the offset cannot be baked at import). Pen width
stays the `height/8` rule — KiCad's explicit thickness is not stored.
