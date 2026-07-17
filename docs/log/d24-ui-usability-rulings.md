---
id: d24
title: "UI usability rulings: the oracle owns the tool strips, showcase-by-default, usability wave 1"
date: 2026-07-16
status: implemented (usability wave 1 merged 2026-07-17, main `8d1d71d`)
---

> Context: the GUI shell anatomy these rulings bind against is
> [gui-architecture.md](../gui-architecture.md) and the live mockup at
> [ui-oracle/](../ui-oracle/README.md).

### Decision 24 — UI usability rulings (2026-07-16, ruled; wave 1 merged 2026-07-17)

Scoping the first post-renderer usability campaign against the UI oracle
produced four rulings:

1. **Tools per pane: the oracle wins.** The per-pane strips converge on
   `shell.dc.html`'s enumeration — a shared head of select · pan · measure ·
   delete for both canvas kinds, board adding its routing/authoring tools,
   schematic adding its own authoring vocabulary as it grows one. This
   **supersedes the 2026-07-11 "schematic strip is Select-only" ruling**: that
   ruling existed because the old schematic Measure was a structural no-op
   (board-space preview); the tool returned as a real schematic-space measure.
   Schematic *delete* stays deferred to the schematic-editing campaign — its
   semantics belong to that design, not to the board's.
2. **The oracle's zoom clamp (0.15–6) is decorative.** The app's
   0.1–10000 px/mm clamp stands.
3. **First-run / create-project flow is deferred** to the schematic-editor
   campaign (the schematic is the first surface a new user wants to edit).
   Compromise: a rich showcase document (`examples/showcase.eut`) exercises
   the full feature set, and a no-argument launch opens it instead of an
   empty doc.
4. **Explorer component rows display refdes + value** (the oracle's component
   row form), not refdes + part name.

**Wave 1 delivered against these rulings** (four parallel branches, merged
2026-07-16/17): the showcase document + default-open; chrome wiring (Gerber/
Excellon/SVG export of the live doc, Ctrl+±/= zoom, mm/in display units,
dots/lines grid, Quit/About/Keymap); explorer filter + Ctrl+K command palette
(fuzzy jump-to + data-driven command registry); direct manipulation (oracle
strips, delete via key/menu/tool, R-rotate, editable inspector for position/
rotation/trace width/layer, schematic-space measure). Engine side effects:
`geom_rev` now bumps on orientation and component removal (closed issue 0013),
and a named net with zero members parses canonically (the serializer already
emitted that form; deleting a net's last member made it reachable).

**Design gotcha recorded:** damascene matches registered hotkeys *ahead of*
focused-widget key capture and consumes the keystroke. Two branches
independently shipped bare-character chords (`+`/`-`, `Del`/`r`) that hijacked
text inputs. House rule since: chrome actions get modifier chords; bare
editing keys are handled on the raw window-level KeyDown path, which damascene
only emits when no capturing widget holds focus.
