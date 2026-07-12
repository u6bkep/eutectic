# GUI Architecture

Companion to `architecture.md` (the engine design of record). This note covers
the GUI layer: the toolkit decision, the canvas strategy, the editing
philosophy, the structural commitments v1 must bake in, and the v1 scope.
Deferred features live as tickets in `issues/gui-wishlist/`.

## Toolkit and workspace

- **Toolkit: damascene, tracked as a rev-pinned git dependency on upstream
  main** (`damascene-core` + `damascene-winit-wgpu`; since 2026-07-07 —
  upstream moves fast and we occasionally request changes, so we bump
  deliberately and often; the pin may be AHEAD of the latest crates.io
  release, see the note in `eutectic-gui/Cargo.toml`). The local clone at
  `reference/damascene` is kept on the same rev for source reading.
  Damascene is a thin GPU UI library
  that renders through the host's wgpu pass; apps implement
  `App { build(&self) -> El, on_event(&mut self), before_build, ... }` — a
  pure projection from app state to a widget tree, which matches the engine's
  source → derived-views shape exactly.
- **Repo layout: same repo, cargo workspace.** `eutectic-core` (the existing
  crate, untouched, keeps its single ttf-parser dependency) + `eutectic-gui`
  (new crate; the only crate that depends on damascene/wgpu). The workspace
  root manifest carries damascene's documented `[profile.dev.package.*]`
  opt-level overrides (MSDF glyph generation is ~500× slower unoptimized;
  without the block, debug startup is ~19 s vs ~40 ms).
- **Headless review loop.** Damascene renders any `App` to SVG + tree dump +
  lint report with no GPU or window, as a plain `#[test]`. GUI panels get the
  same fixture-and-artifact review discipline the engine's fab outputs get:
  canned scenes in a `fixtures.rs`, lint-clean assertions in CI, SVG/tree
  diffs in adversarial review.

## Canvas strategy (owned canvas — decided 2026-07-07; supersedes the pure-damascene canvas)

**The canvas is ours: an app-rendered wgpu surface composited by damascene.**
This supersedes the v1 "pure damascene, no custom GPU pipeline" ruling, which
delivered milestones 2–6 but whose interaction model was diagnosed (2026-07-07,
from the pan/grid/hover evidence) as a structural mismatch, not a bag of bugs:
`viewport()` is built for **El-shaped content** — discrete widgets whose
identity drives hit-testing, hover, and pan-on-background — while a board is a
**dense picture** with app-side semantics. Our full-bleed layer Els suppressed
the native pan everywhere (patched per view kind with `CameraPanState`), hover
identity could never change across one monolithic El (so pointer-move never
fired), the camera was readable only post-layout at a one-frame lag (forcing
the windowed grid-lattice cache and its first-frame fallback), and the overlay
re-tessellated every frame. Damascene's own doctrine for this situation —
its winit host is a quickstart to copy, not a dependency to twist; apps whose
content isn't El-shaped should own a canvas and write shaders — is the
documented escape hatch, and we take it.

**The load-bearing engine fact that makes this cheap:** `route::world_features`
is already the single realized-geometry stream (substrate, copper with pours
boolean-resolved, mask, silk, text, drills — every feature carrying provenance
and net), and DRC, the autorouter, Excellon, GUI rendering, *and* GUI picking
are already pure filters over it. The owned renderer is the stream's next
consumer; only the presentation back-half changes. Picking, selection, tools,
findings, and the command layer are untouched.

- **Boundary.** Damascene keeps all chrome: split tree, menus, panels, tool
  strips, dialogs, status bar. Each canvas pane is one keyed
  `surface(AppTexture)` El — zero-copy composite + clip; pointer events arrive
  on the wrapper El. Board and schematic view kinds both live on this path
  (the schematic's pan gap, issue 0035, retires with the old model rather
  than being patched).
- **Own winit host**, seeded as a copy of `damascene-winit-wgpu` around one
  `RunnerCore` (the author's explicitly intended use). This is also the gw-21
  prerequisite (one Runner per OS window over shared domain state), and it
  hands us raw cursor motion — **free hover on day one**, no upstream ask.
- **Renderer.** Tessellate the feature stream once per doc revision (same
  cadence as today's derived caches) into persistent per-layer vertex
  buffers. Layer color/visibility/dim are per-layer uniforms — layer toggles
  and future high-contrast/flip modes are uniform writes, never rebuilds.
  Selection/hover/DRC-halo/tool-preview geometry is one small dynamic overlay
  buffer (the layered-canvas commitment, restated in GPU terms). The dot grid
  is a procedural fragment shader — infinite, pitch-adaptive, zero CPU; the
  windowed lattice cache is deleted, not fixed.
- **Camera is app state**, per pane, in `PaneState` — where the domain/pane
  split always said it belonged. Precision rule: integer-nm coordinates
  exceed f32's mantissa, so vertex buffers upload anchor-relative f32 and the
  camera composes in f64 on the CPU; the integer core lets us choose anchors
  freely. Gesture contract: middle-drag pan (CAD idiom; left stays select),
  wheel zoom-at-cursor, fit/frame-rect as plain camera math (no request
  queue, no one-frame lag).
- **Anti-aliasing is deliberately iterative**: crib from damascene-wgpu and
  computer-whisperer's other consumers first; MSAA 4× is the acceptable MVP
  floor; analytic edge AA is the upgrade path if thin traces at low zoom
  demand it. Not settled in this record on purpose.
- **Testing.** The headless SVG + lint bundle is unchanged (it is ours, off
  the same stream). The gesture harness improves: synthetic winit-level
  events replace reverse-engineered `RunnerCore` routing. GPU image goldens
  may join later under the gw-25 ruling. Damascene's tree/draw-op dumps stop
  covering canvas interiors (they covered them only nominally).
- **Migration.** Board pane first, behind the existing pane interface;
  schematic second; the viewport-based path is **deleted** when both are
  across (old code removed, not morphed). Retired with it: `viewport()`
  usage, the `VectorAsset` layer path, `CameraPanState`, the
  `ViewportRequest` queue, the grid cache, and the one-frame-lag camera
  reads.
- **Two producers, one ingest.** The renderer consumes a shared primitive
  vocabulary (analytic strokes/discs/arcs, polygons, areas, text runs), each
  primitive carrying a plane, a semantic id (net/entity — indexes a GPU state
  buffer so hover/selection/dim are one-integer writes, no rebuilds), and a
  style class resolved through per-plane tables. The board producer lowers
  `world_features`; the schematic producer is `schematic_features`
  ([d23](log/d23-schematic-features-tier.md)) — the core realized-geometry
  query that also feeds the schematic SVG and pick, so the owned renderer is
  never a second home for drawing conventions. Annotation text renders via an
  MSDF glyph atlas (cribbed from damascene-wgpu); fab ink (silk/copper text)
  arrives as glyph geometry from the stream because there the glyphs are the
  artifact.
- **Engine rider — collapse the export tier onto the stream.** The one known
  duplication left: `export::role_features` re-runs the per-component
  graphics/text loop that `world_features` runs, and copper Gerber/SVG
  re-walk the Doc rather than driving off the stream's provenance. Folding
  them in makes fab output and on-screen pixels provably the same geometry.
  Gerber keeps its native aperture/flash form — it just sources from the
  stream.

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

1. **Layered canvas** — static per-layer geometry cached by doc revision +
   a small per-frame dynamic overlay (see Canvas strategy; under the owned
   canvas this is persistent vertex buffers + a dynamic overlay buffer).
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
   over shared domain state). *Updated 2026-07-07:* the custom winit host is
   no longer deferred — the owned-canvas decision (see Canvas strategy)
   requires it, so it lands as that campaign's first slice; per-pane cameras
   move from damascene `UiState` into `PaneState` with it.
4. **Tools as a mode state machine with a preview channel** — the active tool
   owns uncommitted preview geometry rendered into the overlay; commit writes
   to the source (via the command layer), then re-elaborates.
   *Revised 2026-07-07:* the mode variable is keyed **per view kind**, not
   app-global (Blender semantics: schematic panes share one active tool,
   board panes another; moving between panes swaps which is live). Tool
   state lives in view state so a future popped-out window carries it. The
   preview/commit machinery is unchanged. Rationale: a single global tool
   rail plus hover-focus is geometrically broken — traveling from a board
   pane to an app-edge rail transits other panes and dims the tools you
   are heading for. Tools therefore render as per-pane overlay strips
   (see the UI oracle), never as an app-edge rail.
5. **Findings as data** — DRC/connectivity/ERC findings carry stable feature
   references + locations so they render as canvas halos and populate a
   findings panel with click-to-zoom. Toasts only for genuinely transient
   events.

Editing is source-first: every mutation is a command against the `.eut`
source; re-elaborate derives everything else; undo/redo is source snapshots
(byte-lossless serializer makes them exact — and identity-exact once
Decision 22 lands: today a snapshot round-trip re-mints route ids, which is
issue 0034's undo-renumbering gap). Background work (file watch,
debounced DRC) arrives via the mailbox pattern (`before_build` drain +
external wakeup).

**Save model (decided 2026-07-05, m6):** edits live in memory as dirty state;
the GUI never overwrites the user's file autonomously in an interactive
session. Explicit save writes `serialize(doc)` to the file (and suppresses
the watcher echo of our own write). If the watcher delivers an external
change while the doc is dirty, that is a **conflict banner** — an explicit
reload-or-keep choice, never silent last-writer. A clean doc follows external
edits automatically, as today. The always-current sidecar variant (agent
visibility of live editing state) is deferred as gw-24.

## Library resolution (the single Libraries menu)

Engine-side design: `architecture.md` §9 (library packages, `use` directive,
permissive unresolved parts). The GUI's share, delivered 2026-07-05:

- **One per-machine registry** — `$XDG_CONFIG_HOME/eutectic/libraries` (fallback
  `~/.config/eutectic/libraries`), plain `NAME <absolute path>` lines, read/written
  only by `eutectic-gui/src/registry.rs` (path-injectable; tests never touch the
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

## Design reference: the UI oracle (binding)

The shell/anatomy design of record is **`docs/ui-oracle/shell.dc.html`** —
a working HTML mockup (serve the directory over HTTP and click around; see
`docs/ui-oracle/README.md` for how to view it and, crucially, for the
**binding vs decorative** split, the color-token → damascene-theme table,
and the view-kind ↔ wishlist-ticket map). Implementation slices are checked
against that page, not against memory of it. Adopted 2026-07-07 after
windowed review; it supersedes the generic "Circuit Studio" mockup
(`reference/eda-ui-mockup/`, kept only as provenance).

Binding headlines (details and exact vocabulary live in the oracle):

- **Blender-style split tree** of panes — split right / split down / close
  in every pane header; no fixed dual/stacked layouts. Each pane header
  carries a view-kind dropdown over six kinds: Schematic (with in-pane
  sheet breadcrumb), PCB Layout, 3D (gw-09), Gerber preview (gw-08),
  Source, Diff/Review (gw-20). A reserved, deliberately inert
  pop-out-to-OS-window header button marks gw-21's future home.
- **Per-pane overlay tool strips** with per-view-kind tool memory (see the
  revised structural commitment #4). No app-edge tool rail, ever.
- **Menu bar** enumerating the command surface (File/Edit/View/Place/Route/
  Inspect/Tools/Help, including Autoroute), filename + dirty dot, and
  per-source findings chips (DRC/ERC/NET/LIB) — no "Run DRC" button;
  checks are live and the chips are the status.
- **Right sidebar**: four sections whose headers are always visible —
  Properties (editable fields, gw-05, with the permissive-editing hint),
  Layers (visibility, swatches, active-layer radio), Explorer (search,
  class-grouped nets, gw-01), Findings (rule ids, hover-waive gw-02,
  waived subsection).
- **Canvas furniture**: dot grid + axes, per-pane zoom chip; layer-colored
  copper; ratsnest and DRC halos as overlay. **Status bar**: X/Y, dx/dy,
  grid, zoom, focused tool, selection-filter state, active layer, net,
  findings state.
- Overlays: Ctrl+K command palette (gw-12), Libraries dialog (registry
  semantics), library-browser flyout on place-symbol (gw-03),
  selection-filter popover (gw-04).

## v1 scope (decided)

Delivered as milestones, roughly in order:

1. **Workspace conversion + `eutectic-gui` skeleton** — workspace manifest,
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
