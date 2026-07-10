# The owned-canvas renderer — implementation spec

> Status: **agreed design, 2026-07-09** — the implementation spec for the
> owned-canvas decision (gui-architecture.md "Canvas strategy") and its
> "Two producers, one ingest" contract. Companion rulings: Decision 23
> (geometry-model-convergence.md — the schematic producer) and gw-25
> (golden render snapshots). This document is the work-package source for
> the fan-out; it becomes module-doc material once the code exists.

## 1. Purpose & boundary

A renderer that turns realized design geometry into pane textures. It is a
pure function of: a **scene** (typed primitives from a producer), a
**camera** (per-pane app state), **style tables** (per-plane appearance),
and a **semantic state buffer** (hover/selection flags). It renders into
an app-owned wgpu texture composited by damascene via one keyed
`surface(AppTexture)` El per pane; damascene keeps all chrome. Nothing in
the renderer knows about documents, tools, or damascene Els — producers
lower domain data to scenes, the app layer owns cameras and state.

Two producers feed it: the board lowering over `route::world_features` and
the schematic lowering over `schematic_features` (Decision 23, in flight).
Later consumers ride the same contract: the CAM pane (gw-08) points it at
a Gerber parser's stream; the library editors (Decision 23 §5) point it at
a def's elaboration; a 3D mode (gw-09) is a sibling *shading strategy*,
not a fork — see §12.

## 2. Ingest contract

A `Scene` is:

- an **anchor** (integer-nm point; see §7 precision) and content bounds;
- an ordered list of **planes** (copper slab, mask, silk, drills, wires,
  symbol bodies, overlay, …), each a stable key with a z-order; plane
  *appearance* (color, alpha, dim, visibility, pattern) lives in style
  tables, never in the scene;
- per plane, a list of **primitives**:
  - `Capsule { a, b, r }` — trace segments, stroked polyline edges, pins;
  - `Disc { c, r }` — vias, round pads, junction dots;
  - `ArcStroke { center, radius, a0, a1, half_width }` — arc path segments;
  - `Polygon { ring(s) }` — tessellated interiors (pours, outlines,
    glyph ink, rectangular pads);
  - `TextRun { pos, height, justify, content }` — annotation text only
    (§6); fab ink arrives as `Polygon` glyphs, never as runs;
  - (reserved: `Mesh` — the 3D mode's vocabulary; not built now.)
- every primitive carries a **semantic id** (compact index into the state
  buffer — net or entity; a sentinel for chrome) and a **style class**
  (resolved through per-plane tables: filled vs outline, dash pattern id);
- stroke primitives carry **accumulated path length** at their start so
  dash patterns flow continuously through corners (fragment shader
  evaluates the dash procedurally from the along-axis parameter).

Scenes are rebuilt per **doc revision**, never per frame, camera change,
or interaction. Producers guarantee deterministic order.

## 3. Geometry pipeline

**Instanced analytic primitives for everything stroke-shaped.** A capsule,
disc, or arc-stroke is one instance: the vertex shader expands a bounding
quad; the fragment shader evaluates signed distance and writes exact
coverage. Resolution-independent (no re-tessellation at any zoom), exact
AA on precisely the shapes MSAA handles worst, one instance per feature so
edit-rebuilds are trivially cheap. This is ~95% of copper by count.

**Tessellated triangles for `Polygon`/`Area` interiors** (pours, outlines,
glyphs), flattened at a fixed nm tolerance at scene-build time (the rings
arrive already polygonized from the region kernel). Their edge AA comes
from the coverage target's MSAA at MVP (§4); the upgrade path is an
analytic feather fringe along boundary edges — a shader/geometry addition,
not a schema change.

**Buffers** are persistent, keyed by (doc revision, producer), shared
across all panes viewing the same doc: one instance buffer + one triangle
buffer per plane, plus one small **dynamic overlay buffer** (same instance
schema) rebuilt only while a preview is live (rubber-band trace, DRC
halo). Tessellation is CPU-side (earcut-class; dependency choice —
implementer's, with lyon the default candidate) and unit-testable without
a GPU.

## 4. Pass structure — coverage, then colorize

Per visible plane, geometry renders **colorless** into a shared
single-plane offscreen **coverage target**, `Rg8Unorm`, MSAA 4× at MVP,
max-blended so overlapping same-plane primitives (trace end over pad)
saturate instead of double-blending — this is what makes translucent
layers (dimmed inactive copper, mask over copper) correct:

- **R = base coverage**;
- **G = state-flagged coverage**: the vertex shader fetches the
  primitive's state word from the semantic buffer (§5); flagged fragments
  write G as well as R. G ≤ R by construction.

A **composite pass** then lays the resolved coverage into the pane texture
back-to-front with per-plane uniforms from the style tables:
`color = mix(plane_color, emphasis_color, G/R)`, alpha = R × plane_alpha ×
dim. Layer visibility toggles, dimming, color themes, and plane patterns
are composite-pass uniform writes — **never geometry work**. Drills are an
ordinary plane composited after copper that paints the background color
(absence-through-everything, matching fab semantics; coverage max-blend
never needs subtraction).

Also procedural, evaluated in the composite/background stage from camera
uniforms alone (zero CPU, zero geometry): the **dot grid** (1-2-5 pitch
ladder keyed to zoom, one emphasis tier, origin marker; mm-native, mil
toggle later) and the **crosshair cursor** (full-pane hairlines at the
pointer; OS cursor hidden over the canvas).

If profiling shows the per-plane pass count matters, planes can pack four
coverage channels into one RGBA8 target per pass group — an optimization
inside the same architecture; not MVP work.

## 5. Semantic state buffer

A small storage buffer indexed by semantic id: per-net/entity flag words
(hovered, selected, emphasis tier; room to grow). Hover, selection, and
net-highlight changes are **one-integer writes** followed by a texture
re-render — no scene rebuild, no buffer re-upload beyond the touched
words. Cross-view highlight is the same write observed by both panes'
renders. The overlay buffer (§3) is reserved for genuinely dynamic
*geometry* (previews, halos), not for state tinting.

## 6. Text

Two tiers, deliberately:

- **Fab ink** (silk/copper text) is geometry — glyph `Polygon`s from the
  stream, because the glyphs are the artifact. Renders like any area.
- **Annotation text** (schematic labels, net tags, refdes overlays,
  readouts) arrives as `TextRun`s and renders through an **MSDF glyph
  atlas** — crib the pipeline from damascene-wgpu (`src/text.rs`, plus
  the prebaked-atlas machinery in damascene-core). Crisp at UI sizes,
  matches the chrome's text quality.

De-risking note: the **board slice needs no text at all** — board
annotations are chrome-side today, silk is geometry. MSDF lands with the
schematic slice (§12), not the MVP.

## 7. Camera & precision

Integer-nm coordinates exceed the f32 mantissa. Rule (gui-architecture.md):
vertex/instance data uploads **anchor-relative f32** (anchor = scene
bounds center, from §2); the camera composes view transforms in **f64 on
the CPU** (center nm, zoom px/nm, per pane in `PaneState`) and uploads a
per-frame f32 matrix that folds in the anchor offset. Project/unproject
(pointer→board nm) is f64 CPU math on the same state — picking never
round-trips through the GPU.

**Gestures** (behavior-preserving on tools — §11): middle-drag pan (left
stays select), wheel zoom-at-cursor, fit / frame-to-rect / zoom-to-
selection as plain camera math. Free hover via the host's
`raw_window_event` seam.

**Motion style**: camera *targets* glide through a short interruptible
critically-damped ease (~100–150 ms); wheel ticks retarget the same
filter so successive steps feel continuous; **nothing else animates** —
selection, hover, and DRC halos are instant state changes.

**Damage discipline (a contract, not an optimization)**: a pane's texture
re-renders iff one of (doc revision, camera, texture size, state-buffer
generation, overlay generation, theme generation) changed; otherwise
damascene composites the cached texture. Continuous redraw requests only
while a glide or drag is live. Idle cost: zero GPU work.

## 8. Theming & style tables

Style tables are app-owned and feed the composite uniforms. Two sources:

- **Chrome-adjacent colors** (canvas background, grid, crosshair,
  selection/emphasis accent) hold **damascene theme tokens**, resolved at
  uniform-write time via the runner's `Theme` (`RunnerCore::theme()`;
  palette `resolve`/`lookup`) — the canvas follows the active theme, and a
  theme swap is a uniform rewrite, mirroring damascene's own
  token-at-paint-time design.
- **Domain colors** (per-copper-slab palette, mask, silk, drills) are
  app-owned defaults with light/dark variants — domain semantics no
  toolkit theme can own — in one config-shaped table (user-themable
  later for free).

## 9. Integration with damascene

- One keyed `surface(AppTexture)` El per pane; pointer events arrive on
  the wrapper El as today. `SurfaceAlpha::Opaque` (the canvas fills every
  pixel — skips blend math), format `Bgra8UnormSrgb`/`Rgba8UnormSrgb`
  (match the swapchain; sampling decodes to linear like the rest of
  damascene's pipeline). Constructed via `damascene_wgpu::app_texture` on
  the **runner's device/queue** — same-device is what makes compositing
  zero-copy.
- **Sizing**: texture tracks pane rect × scale factor, pixel-accurate
  `surface_fit`; reallocation with hysteresis (grow to a step boundary,
  shrink lazily) so live pane-resize doesn't thrash allocations.
  Fractional DPI comes from the host's scale-factor events.
- **Device loss**: rebuild everything from CPU-side caches (scenes,
  cameras, tables survive); never crash, never blank permanently.
- **Elaboration failure**: keep rendering the **last good revision** with
  a "stale" composite treatment (desaturate/dim uniform); findings/chrome
  carry the error. Matches the text-editor flow's behavior on parse
  errors.

## 10. Testing

1. **CPU tier (the bulk)**: scene lowering (world_features → planes),
   instance building, tessellation, dash arc-length, camera math,
   project/unproject round-trips, damage keys — plain unit tests, no GPU.
2. **GPU goldens (gw-25)**: headless wgpu on a software adapter
   (llvmpipe/lavapipe), small scenes per shader feature (capsule AA, max-
   blend saturation, RG emphasis mix, grid ladder, drill-over-copper),
   tolerance-based comparison (drivers differ), committed PNGs.
3. **Semantic oracle unchanged**: the SVG + lint bundle stays the
   authority on *what* the drawing contains; the renderer goldens only
   guard *how* it rasterizes. The winit-level gesture harness covers input
   plumbing (synthetic events → camera/selection assertions).

## 11. Performance targets & non-goals

Targets: display-refresh pan/zoom on the capstone board (every frame is
one coverage+composite pass chain over persistent buffers — no per-frame
tessellation, ever); doc-edit rebuild well under a frame at capstone
scale; **zero idle GPU work** (§7 damage rule).

Non-goals for the MVP (each has a home, none leak in): culling/LOD (board
feature counts don't need it; coarse instance culling is the escape hatch
if profiling ever disagrees), 3D mode (gw-09 — the `Mesh` slot and shared
camera/state plumbing are its seam), CAM pane (gw-08), minimap, print/
export (SVG owns it), marquee select / box zoom / measurement (tool
tickets), schematic pane in the first slice (§12), rotation of the 2D
view (flip lands later per gw-11 as a camera-matrix negation + winding
flip).

## 12. Migration & work packages

Board first, schematic second, then the viewport path dies (old code
deleted, not morphed — gui-architecture.md). Fan-out shape:

- **WP1 — renderer core + board producer** (`eutectic-gui/src/render/`):
  scene types, board lowering over `world_features`, buffers, coverage +
  composite passes, grid/crosshair, camera, semantic state buffer, CPU
  tests + first goldens. Headless-testable throughout; no pane wiring.
- **WP2 — pane integration**: `AppTexture` panes behind the existing pane
  interface, gestures on the f64 camera, free hover, picking replumbed to
  the camera's unproject (candidates stay CPU-side over `world_features`),
  tool strip and editing flows re-verified. The old board `VectorAsset`
  path stops being reachable; `canvas.rs` stays only as the schematic's
  crutch until WP3.
- **WP3 — schematic slice** (after Decision 23's `schematic_features`
  lands): schematic producer, MSDF annotation text, dash/style classes
  exercised for real; then **delete** the viewport path wholesale
  (`viewport()` usage, `VectorAsset` layers, `CameraPanState`,
  `ViewportRequest` queue, grid cache) and drop the 0035-class issues
  with it.

WP1 and WP2 are sequential (WP2 consumes WP1's API); parallelism lives
inside WP1 (pipeline vs producer vs camera are separable once the scene
types are pinned). Every WP gets the standing adversarial-review train.
