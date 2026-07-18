# eutectic UI oracle

`shell.dc.html` is the authoritative shell/anatomy oracle for **eutectic-gui**. It supersedes the
generic "Circuit Studio" mockup it was derived from: every region, menu entry, view kind, and
interaction affordance in this file is a statement about what the real GUI should eventually
contain. Implementation slices should be checked against this page, not against memory of it.

## Viewing

```sh
cd docs/ui-oracle && python -m http.server 8000   # then open http://localhost:8000/shell.dc.html
```

`file://` does not work — `support.js` (the dc-runtime) fetches the page and React over HTTP.
Network access is needed for React (unpkg) and the two Google Fonts (JetBrains Mono, Material
Symbols). Everything in the mockup is live: pane split/close/drag, menus, palette (`Ctrl+K`),
dialogs, accordion, per-pane tool strips with per-view-kind tool memory, selection sync,
editable properties.

## Binding vs decorative

**Binding** (the oracle): the region inventory (menu bar, toolbar, split-tree pane area with
per-pane overlay tool strips, right accordion, status bar); the six view kinds and their
dropdown; the menu command enumeration; the interaction affordances (Blender-style split/close
per pane header; a reserved, deliberately inert pop-out-to-OS-window header button — gw-21,
roadmap; per-pane pan/zoom; **per-view-kind tool memory**: `tool` is a map keyed by view kind,
so all schematic panes share one active tool while board panes independently keep theirs, and
the status bar shows the focused pane's slot; floating tool strips overlaid inside each canvas
pane — tools shown in a pane are inherently applicable there, strips collapse to a single chip
in panes narrower than ~300px; waive-on-hover; selection sync canvas ↔ explorer ↔ properties ↔
status bar); the accordion structure (all four headers always visible);
the chips/status vocabulary (per-source `DRC/ERC/NET/LIB` chips — no "Run DRC" button); the
color tokens below; the permissive-editing hint ("Edits commit to source · violations become
findings, never blocks"); the Libraries dialog semantics (per-machine `use NAME` bindings,
absolute paths never in the document, builtin toy library registered last).
The board Place Part tool opens a docked, palette-like library browser: document-used
packages lead in use order, remaining successfully resolved packages follow in registry
order, and `builtin` is last; filtering owns bare typing, rows are grouped by package,
and choosing a row arms repeated placement without closing the browser. Its preview is a
fit-once, non-interactive `AppTexture` rendered by the owned board renderer
(no grid, crosshair, gestures, or overlay).

**Decorative**: the STM32/USB demo data, exact pixel positions, the SVG doodles (iso board,
symbol preview), the invented `.eut` source text, exact px paddings where damascene has its own
spacing scale.

## Color tokens

| Token | Hex | Damascene theme role |
|---|---|---|
| bg-0 … bg-5 | `#09090b` `#0d0d10` `#0f0f12` `#141417` `#18181c` `#1a1a1f` | app bg → sidebar → bars → headers → popover → chip |
| border-1/2/3 | `#212127` `#26262c` `#2a2a30` | hairline / control border / popover border |
| text-1/2/3/4 | `#e4e4e7` `#a1a1aa` `#71717a` `#52525b` | primary / secondary / muted / faint |
| accent | `#3b82f6` | selection, active tool/layer, focused pane ring |
| ok / warn / err | `#22c55e` `#f59e0b` `#ef4444` | chips, findings severities, schematic wire = ok-green |
| ratsnest | `#8b5cf6` | unrouted-connection dashes |
| layer F.Cu / B.Cu | `#e05c4a` `#4a7fd6` | copper trace colors |
| layer F.Silk / B.Silk / F.Mask / Edge | `#d4d4d8` `#8b8b93` `#22633f` `#eab308` | layers panel swatches; Edge also strokes the board outline |

All identifiers, coordinates, and numerics render in JetBrains Mono; icons are Material Symbols
Outlined ligatures.

## View kinds

| Kind | Icon | Intent | Tools | Wishlist |
|---|---|---|---|---|
| Schematic | `schema` | wires, junction dots, net labels, power symbols; hierarchical sheet breadcrumb lives inside the view | select, pan, measure, delete · wire, net label, place symbol, power | — |
| PCB Layout | `grid_on` | traces colored by layer, vias, ratsnest, DRC halos, selected-trace vertex handles | select, pan, measure, delete · **place part**, route, via, copper pour, silk, text, dimension · selection filter | — |
| 3D View | `deployed_code` | extruded board placeholder, orbit hint | orbit/pan | gw-09 |
| Gerber preview | `layers` | single-layer aperture-flash render of what the fab receives | pan | gw-08 |
| Source | `code` | read-only `.eut` text with line numbers (text-first architecture; will follow selection) | none | — |
| Diff / Review | `difference` | git-native visual diff: red = removed/old position, green = added/new | pan | gw-20 |

Other oracle ↔ wishlist mappings: command palette gw-12, waivable findings gw-02, net classes in
explorer gw-01, library browser flyout gw-03, selection filter gw-04, editable inspector gw-05,
flip board gw-11, pane pop-out to OS window gw-21 (header button + View menu row, both inert —
they reserve the anatomy so no future slice paints over the space).

## Deliberate deviations from the original mockup

- The Side-by-side/Stacked toggle is gone — replaced by a recursive Blender-style split tree
  (split right / split down / close in every pane header, capped at 6 panes).
- The global left tool rail from the first oracle revision was replaced by per-pane
  Blender-style overlay toolbars: a global rail plus hover-focus is geometrically broken —
  moving the mouse from the layout pane to the rail transits the schematic pane, which steals
  focus and dims the very board tools you're heading for. Tools now live inside the pane they
  apply to, and each view kind remembers its own active tool.
- No "Run DRC" toolbar button — checks are live; the per-source chip row is the status.
- Explorer nets are grouped by net class (power/signal/usb) instead of a flat list.
- The Layers panel is promoted to a first-class accordion section with visibility eyes and an
  active-layer radio, rather than being buried in a menu.
- Single-file: the NetExplorer child component was inlined; no `dc-import` remains.

## Format notes

The page is a single self-contained `.dc.html` (template in `<x-dc>`, logic in the
`data-dc-script` block). One runtime gotcha worth knowing: text interpolations render wrapped in
an HTML `<span>`, which is invalid inside SVG `<text>` — bindings used as SVG text content must
be passed as `tspan` React elements (see the `tsp()` helper in the script).
