# GUI Architecture

Companion to `architecture.md` (the engine design of record). This note covers
the GUI layer: the toolkit decision, the canvas strategy, the editing
philosophy, the structural commitments v1 must bake in, and the v1 scope.
Deferred features live as tickets in `issues/gui-wishlist/`.

## Toolkit and workspace

- **Toolkit: damascene v0.4.5 from crates.io** (`damascene-core` +
  `damascene-winit-wgpu`). A local clone of the exact release tag lives at
  `reference/damascene` for source reading. Damascene is a thin GPU UI library
  that renders through the host's wgpu pass; apps implement
  `App { build(&self) -> El, on_event(&mut self), before_build, ... }` — a
  pure projection from app state to a widget tree, which matches the engine's
  source → derived-views shape exactly.
- **Repo layout: same repo, cargo workspace.** `ecad-core` (the existing
  crate, untouched, keeps its single ttf-parser dependency) + `ecad-gui`
  (new crate; the only crate that depends on damascene/wgpu). The workspace
  root manifest carries damascene's documented `[profile.dev.package.*]`
  opt-level overrides (MSDF glyph generation is ~500× slower unoptimized;
  without the block, debug startup is ~19 s vs ~40 ms).
- **Headless review loop.** Damascene renders any `App` to SVG + tree dump +
  lint report with no GPU or window, as a plain `#[test]`. GUI panels get the
  same fixture-and-artifact review discipline the engine's fab outputs get:
  canned scenes in a `fixtures.rs`, lint-clean assertions in CI, SVG/tree
  diffs in adversarial review.

## Canvas strategy (decided)

**Pure damascene, no custom GPU pipeline in v1.** The board canvas is a
`viewport()` widget (first-class pan/zoom: drag-pan, wheel-zoom-to-cursor,
fit, reset; camera state lives in damascene's `UiState` keyed by the pane's
El key) containing El nodes that carry `VectorAsset`s — programmatic vector
paths built with `PathBuilder` (lines/quads/cubics, fill rules, strokes),
tessellated by lyon in the backend, cached by `content_hash`.

- **Layered from day one.** Static content (board layers: copper, silk, mask,
  outline, …) is tessellated into per-layer vector assets cached against a
  doc revision counter — rebuilt only when the doc changes. Dynamic content
  (selection/hover highlights, DRC halos, tool previews, ratsnest, measure
  overlays) is a separate small overlay asset rebuilt every frame. Tools and
  highlights never force re-tessellation of the board.
- **Hit-testing is ours.** `ViewportView::{project, unproject}` maps pointer
  ↔ board coordinates; picking queries the engine's geometry kernel.
  Damascene handles chrome hit-testing; the canvas interior is one keyed El.
- **The swap seam.** The canvas is wrapped behind a small internal interface
  (features in → El out). If boards outgrow the vector path (tessellation or
  draw cost), the escape ladder is: per-El custom WGSL shaders riding
  damascene's paint stream, then a host-painted region with our own pipeline.
  Not built preemptively — same ruling as ObstacleField.

## Editing philosophy: permissive, never a hard "no"

The editor **never refuses an edit for legality reasons**. Placing a part on
top of another part, drawing a trace through a package, wiring across a
symbol — all commit fine. Violations surface as live highlights (clearance
halos, colored markers) on the offending elements, and the user refines
incrementally: add a waypoint, nudge the part, reroute a segment. Rationale:
hard refusal causes frustration and over-careful fine adjustment; incremental
correction feels polished.

Consequences:

- **The doc may hold DRC-violating state.** DRC is a continuous *view* over
  the doc (live check, not a check button), never a gate on edits. Tools have
  no rejection path.
- **Wiring flow** (manual trace / schematic wire): click source, click
  destination — a naive direct connection is committed immediately, however
  much it overlaps. The user then adds/drag waypoints to refine the path.
  The classic click-waypoints-in-order flow also works; the two compose.
- Solver-gated commits (Decision 18) are unchanged: *solvers* propose and
  gate on acceptance; *manual edits* commit unconditionally.

## Interactive routing ladder

Shared GUI shape: tool produces preview geometry per pointer-move; click
commits (Decision 18's gate at the mouse button, but per the permissive
philosophy, commit is never legality-gated).

1. **Manual point-to-point** — click pads/waypoints; snapping; via drop on
   layer switch; violations highlight live rather than block. *(v1 target)*
2. **Assisted manual** — the pending segment continuously re-derives as a
   legal 45° connection hugging obstacles (walkaround); never moves existing
   copper. *(roadmap)*
3. **Push-and-shove** — existing copper displaced in real time with
   spring-back. A genuinely different algorithm (incremental rip-up +
   topological shove); this is the "second router" that cashes in the
   documented ObstacleField seam. *(open, research-sized)*
4. **Route assist** — user freeform-sketches a corridor for a *selected group
   of traces*; the router routes the group along it, respecting per-net
   clearances and impedance/matching constraints, walking around simple
   obstacles (vias etc.). The current A* is a crude ancestor of this and
   will need wholesale replacement before it is good. *(open)*

## Structural commitments for v1 (the five through-lines)

Cheap now, expensive to retrofit. Every roadmap feature reduces to these:

1. **Layered canvas** — cached static layer assets + per-frame dynamic
   overlay (see above).
2. **Semantic selection model** — selection/hover is one shared set of
   semantic ids (net names, refdes, pins, feature ids) in domain state,
   never per-view geometry. Each pane projects it into its own highlight
   overlay ⇒ cross-view highlighting (schematic ↔ layout ↔ source) is free.
3. **Domain state / pane state split** — domain state (source text, Doc,
   derived caches, selection, findings) is separate from per-pane view state
   (view kind, camera key). Panes live in a split tree (Blender-style
   splitting via `resize_handle`), even while v1 renders one pane. Two panes
   on the same doc get independent cameras by key. This split is also the
   prerequisite for pop-out OS windows later (one damascene Runner per window
   over shared domain state — requires a custom winit host; explicitly out of
   scope until needed).
4. **Tools as a mode state machine with a preview channel** — the active tool
   owns uncommitted preview geometry rendered into the overlay; commit writes
   to the source (via the command layer), then re-elaborates.
5. **Findings as data** — DRC/connectivity/ERC findings carry stable feature
   references + locations so they render as canvas halos and populate a
   findings panel with click-to-zoom. Toasts only for genuinely transient
   events.

Editing is source-first: every mutation is a command against the `.ecad`
source; re-elaborate derives everything else; undo/redo is source snapshots
(byte-lossless serializer makes them exact). Background work (file watch,
debounced DRC) arrives via the mailbox pattern (`before_build` drain +
external wakeup).

## Library resolution (the single Libraries menu)

Engine-side design: `architecture.md` §9 (library packages, `use` directive,
permissive unresolved parts). The GUI's share, delivered 2026-07-05:

- **One per-machine registry** — `$XDG_CONFIG_HOME/ecad/libraries` (fallback
  `~/.config/ecad/libraries`), plain `NAME <absolute path>` lines, read/written
  only by `ecad-gui/src/registry.rs` (path-injectable; tests never touch the
  real config). There is deliberately exactly ONE place paths live — the
  KiCad five-menus failure mode is out of bounds. Absolute paths never
  serialize into a document.
- **The Libraries modal** lists registry rows with live load status (parts
  count / path missing / manifest error) and add/remove; edits save atomically
  and immediately re-resolve + re-elaborate the open doc (cameras, selection,
  layout preserved — same path as a source reload).
- **Resolution is re-derived on every (re)load** from the doc's `use` names,
  in source order, built-in toy lib appended last (real libraries shadow toy
  names). Failures are findings (unregistered name, load error, collision),
  never load blocks; unresolved-part and library rows join the findings panel
  as non-navigating warnings and count into the status chip.

## Design reference: the Circuit Studio mockup

A rough, **non-authoritative** UI mockup lives at
`reference/eda-ui-mockup/eda-tool-ui-prototype/` (Claude Design handoff,
2026-07-04; read `project/Circuit Studio.dc.html` + `NetExplorer.dc.html` —
working HTML/JS prototypes, layout and styling spelled out in source). It
fixes the shell anatomy v1 milestones should follow:

- **Menu bar**: app menus, dual/stacked layout toggle, current filename, and
  a persistent DRC status chip (count + warnings) — live DRC gets a
  glanceable home in the chrome, not only canvas halos.
- **Toolbar**: grouped icon actions (file, undo/redo, clipboard, zoom,
  checks) + units and grid-pitch chips on the right.
- **Left tool palette**: one **global** tool strip grouped by domain
  (select/pan · schematic: wire/bus/label/symbol/power · board:
  route/via/pad/zone · measure/text/delete). One active tool app-wide —
  the tool state machine is global mode with per-view-kind applicability,
  not per-pane tools.
- **Center**: two panes, each with a header holding a view-type dropdown
  (Schematic / PCB Layout / 3D) and a maximize toggle; draggable divider
  (ratio-clamped); dual ↔ stacked orientation. A one-split simplification
  of the split-tree — fine for v1.
- **Right sidebar**: Properties inspector (identity card + key/value rows)
  above an Explorer tree (sheets / components / nets, counts, per-net color
  swatches); explorer selection cross-highlights into panes (the semantic
  selection model made visible). Explorer docks left in stacked layout.
- **Status bar**: cursor X/Y in mm, dx/dy, grid pitch, zoom, active-layer
  chip, selected net, DRC state.

Treat visual specifics (dark palette, mono for identifiers/numerics, spacing)
as taste guidance to re-express through damascene's theme/tokens, not values
to copy. Non-binding details: the "Run DRC" toolbar button (we are
live-check; a jump-to-findings affordance belongs there instead) and the
KiCad-flavored placeholder filenames.

## v1 scope (decided)

Delivered as milestones, roughly in order:

1. **Workspace conversion + `ecad-gui` skeleton** — workspace manifest,
   dev-profile overrides, damascene 0.4.5, window opens, headless fixture
   test harness in place.
2. **Read-only board viewer** — layered canvas rendering `world_features`
   per layer; pan/zoom viewport; layer panel (visibility, color, opacity);
   cursor coordinate readout.
3. **Selection + inspector** — hit-testing via the geometry kernel; semantic
   selection model; hover + click select; read-only inspector panel;
   measure tool.
4. **Pane scaffolding + schematic view** — split tree with resize handles;
   board and schematic (read-only, from the existing reflow layout) panes;
   cross-view highlighting of nets/parts.
5. **Live source loop** — file watcher re-elaborates on external edit
   (author in $EDITOR, GUI follows); findings panel + DRC halos on canvas.
6. **First editing** — interactive part placement (drag with live ratsnest),
   then manual trace drawing at ladder level 1 with the permissive model
   (naive source→dest commit, waypoint refinement); undo/redo via source
   snapshots.

Everything else — net classes, DRC waivers, library browser, Gerber preview,
3D view, diff pairs, length tuning, variants, revision diff/review mode,
def-instance affordances, multi-window, routing levels 3–4, etc. — is filed
in `issues/gui-wishlist/`.
