//! The `ecad-gui` application shell — milestone 1 skeleton.
//!
//! This is the *workspace-conversion + skeleton* milestone (see
//! `docs/gui-architecture.md`, "v1 scope", milestone 1): the crate compiles,
//! a window can open, and the headless fixture/lint review loop is in place.
//! The interactive machinery — layered canvas, semantic selection, split-tree
//! panes, tools, findings — is milestones 2–6 and is deliberately *absent*
//! here. Where a future struct belongs, a stub with a doc-comment points at the
//! architecture through-line it will implement.

use crate::canvas::pick::{self, Candidate};
use crate::canvas::{BoardLayer, Canvas, Overlay};
use crate::inspector::InspectorData;
use crate::selection::SelectionModel;
use crate::tool::{MeasureState, Tool, format_readout};
use damascene_core::prelude::*;
use ecad_core::doc::Doc;
use ecad_core::geom::Shape2D;
use std::cell::{Cell, RefCell};

/// Domain state: the source-of-truth half of `gui-architecture.md` through-line
/// 3 (domain state / pane state split).
///
/// In v1 this grows to hold the source text, the elaborated [`Doc`], derived
/// caches, the semantic selection set, and findings. Milestone 1 loads a
/// document once at startup and holds only the pieces the skeleton renders.
///
/// The full split — domain state shared across a *tree* of panes, each pane
/// projecting the shared semantic selection into its own overlay — is
/// milestones 3–4. This struct is intentionally the shared, view-independent
/// half so that later panes hang off it without a rewrite.
pub struct DomainState {
    /// The `.ecad` source text the document was loaded from (empty for the
    /// no-document state). Editing this and re-elaborating is the source-first
    /// mutation loop of milestone 5+; here it is load-once and read-only.
    pub source: String,
    /// The elaborated document, or the parse/elaborate error to surface in the
    /// UI. Per the permissive philosophy (`gui-architecture.md`, "Editing
    /// philosophy"), a bad load never crashes — it renders as an alert.
    pub doc: Result<Doc, String>,
    /// The part library used to elaborate and (later) render. The built-in
    /// library is enough for the skeleton; a real project supplies its own.
    pub lib: ecad_core::part::PartLib,
    /// The filename the document was loaded from, for the toolbar badge.
    /// `None` in the no-document state.
    pub filename: Option<String>,
    /// The semantic selection + hover model (structural commitment 2). Lives in
    /// domain state — shared, view-independent — so every pane projects the same
    /// selection into its own overlay (milestone 4's schematic pane reuses it
    /// untouched). `RefCell` for the damascene interior-mutability pattern: written in
    /// `on_event`, read in `build` through `&self`.
    pub selection: RefCell<SelectionModel>,
}

impl DomainState {
    /// The empty state: no document loaded.
    pub fn empty() -> Self {
        DomainState {
            source: String::new(),
            doc: Err("no document".to_string()),
            lib: ecad_core::part::part_library(),
            filename: None,
            selection: RefCell::new(SelectionModel::new()),
        }
    }

    /// Load a document from `.ecad` source text, parsing + elaborating it
    /// through `ecad-core`'s public command API (the same entry point
    /// `examples/poc_multiprobe.rs` and `examples/schematic.rs` use:
    /// `History` + `Command::LoadText`). Never panics: an elaboration failure
    /// is captured in [`DomainState::doc`] as `Err` for the UI to display.
    pub fn from_source(source: String, filename: Option<String>) -> Self {
        Self::from_source_with(source, filename, ecad_core::part::part_library(), |_| {
            Vec::new()
        })
    }

    /// Load a document from `.ecad` source with an explicit part library and a
    /// post-load command batch — the general path [`from_source`](Self::from_source)
    /// specialises. The `extra` closure sees the loaded [`Doc`] (so it can free
    /// trace / via ids and reference committed nets) and returns commands committed
    /// in one follow-up transaction. Used by the board fixture to add routed copper
    /// (traces / vias), which is command-authored, not source-authored. Never
    /// panics: any failure is captured in [`DomainState::doc`] as `Err`.
    pub fn from_source_with(
        source: String,
        filename: Option<String>,
        lib: ecad_core::part::PartLib,
        extra: impl FnOnce(&Doc) -> Vec<ecad_core::command::Command>,
    ) -> Self {
        use ecad_core::command::{Command, Transaction};
        use ecad_core::history::History;

        let fmt = |diags: Vec<ecad_core::diagnostic::Diagnostic>| {
            diags
                .iter()
                .map(|d| format!("[{}] {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut history = History::new(Doc::default());
        let doc = history
            .commit(
                Transaction::one(Command::LoadText(source.clone())),
                &lib,
                "load",
            )
            .map_err(fmt)
            .and_then(|_| {
                let cmds = extra(history.doc());
                if cmds.is_empty() {
                    Ok(history.doc().clone())
                } else {
                    history
                        .commit(Transaction(cmds), &lib, "fixture-route")
                        .map(|_| history.doc().clone())
                        .map_err(fmt)
                }
            });

        DomainState {
            source,
            doc,
            lib,
            filename,
            selection: RefCell::new(SelectionModel::new()),
        }
    }
}

/// Per-pane view state: the *view-dependent* half of through-line 3.
///
/// A pane is one view (board / schematic / source) over the shared
/// [`DomainState`], with its own camera keyed by the pane's El key. v1 renders
/// a single pane; milestone 4 grows this into a Blender-style split tree
/// (`resize_handle`) of panes over the same domain state, and the semantic
/// selection projects into each pane's own highlight overlay.
///
/// Milestone 1 needs none of that machinery, so this is a placeholder: it names
/// the seam without building the split-tree / camera / canvas state that
/// milestones 2–4 own.
pub struct PaneState {
    /// The El key this pane's camera state lives under in damascene's
    /// `UiState`. Distinct per pane so two panes on the same doc get
    /// independent cameras (through-line 3). Unused until the viewport canvas
    /// arrives in milestone 2.
    pub key: String,
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState {
            key: "pane:main".to_string(),
        }
    }
}

/// The El key of the board canvas viewport — the camera state lives under this in
/// damascene's `UiState` (structural through-line 3: per-pane camera by key).
const CANVAS_KEY: &str = "board-canvas";

/// The pick grab radius in screen (logical) px — converted to a board distance
/// through the current zoom by [`pick::tolerance_nm`], so the on-screen radius is
/// zoom-independent.
const PICK_TOL_PX: f32 = 6.0;

/// The milestone-2 application: a [`DomainState`], one [`PaneState`], and the
/// board-view state (the cached layered canvas + per-layer visibility + live
/// interaction state).
///
/// Implements [`App`] as a pure projection from state to a widget tree — the
/// shape `gui-architecture.md` calls out as matching the engine's source →
/// derived-views model. The static layer assets are the *layered canvas*
/// structural commitment: built **once** when the document loads (in [`new`]) and
/// held here, so `build` only clones them into `El`s per frame — never
/// re-tessellates. Interaction state (`RefCell`/`Cell` per the damascene interior-
/// mutability pattern) is written in `on_event` / `before_build` and read in
/// `build`.
///
/// [`new`]: EcadApp::new
pub struct EcadApp {
    pub domain: DomainState,
    #[allow(dead_code)] // camera keying arrives with the split tree in milestone 4.
    pub pane: PaneState,
    /// The board projection + cached per-layer assets, or `None` when no document
    /// is loaded / the load failed / projection failed. Built once in [`new`].
    board: Option<BoardView>,
    /// Which layers are visible, keyed by [`LayerId::key`]. Absent ⇒ visible
    /// (layers default on). Mutated by the layer-panel toggles in `on_event`.
    hidden: RefCell<std::collections::HashSet<String>>,
    /// Viewport requests (Fit / Reset) queued from toolbar clicks, drained once per
    /// frame by the host.
    pending: RefCell<Vec<ViewportRequest>>,
    /// The last pointer position over the canvas in **board mm** (`None` until the
    /// pointer has moved over the canvas), for the status-bar cursor readout.
    cursor_board_mm: Cell<Option<(f32, f32)>>,
    /// Whether the initial fit-to-content has been queued yet (once, on first
    /// build after a document loads).
    fitted: Cell<bool>,
    /// The active tool (structural commitment 4). Global mode; `Cell` because it is
    /// flipped in `on_event` and read in `build`.
    tool: Cell<Tool>,
    /// The measure tool's uncommitted preview state (the preview channel — renders
    /// only to the overlay, never the doc).
    measure: Cell<MeasureState>,
}

/// The board projection held in app state: the [`Canvas`] (for coordinate
/// inversion), the tessellated per-layer assets it built once, and the pre-built
/// pick candidates (folded from the `world_features` stream via each feature's
/// `FeatureOrigin` — see [`crate::canvas::pick`]). All built once per (doc
/// revision) load.
struct BoardView {
    canvas: Canvas,
    layers: Vec<BoardLayer>,
    /// Pickable candidates (pins / traces / vias / pours), folded from the same
    /// `world_features` stream the canvas renders and rebuilt only when the doc
    /// loads — the hit-test input.
    candidates: Vec<Candidate>,
}

impl EcadApp {
    pub fn new(domain: DomainState) -> Self {
        // Build the layered canvas once, when the document loads. A projection
        // failure (unreachable for a committed doc) degrades to "no board view"
        // rather than crashing — the permissive philosophy.
        let board = match &domain.doc {
            Ok(doc) => Canvas::new(doc, &domain.lib)
                .and_then(|canvas| {
                    let layers = canvas.build_layers(doc, &domain.lib)?;
                    let su = ecad_core::elaborate::stackup(&doc.source);
                    let candidates = pick::candidates(doc, &domain.lib, &su);
                    Ok(BoardView {
                        canvas,
                        layers,
                        candidates,
                    })
                })
                .ok(),
            Err(_) => None,
        };
        EcadApp {
            domain,
            pane: PaneState::default(),
            board,
            hidden: RefCell::new(std::collections::HashSet::new()),
            pending: RefCell::new(Vec::new()),
            cursor_board_mm: Cell::new(None),
            fitted: Cell::new(false),
            tool: Cell::new(Tool::default()),
            measure: Cell::new(MeasureState::default()),
        }
    }

    /// Set the active tool — for fixtures / tests that want a canned tool mode. The
    /// interactive path flips this in `on_event`.
    pub fn set_tool(&self, tool: Tool) {
        self.tool.set(tool);
    }

    /// Set the measure preview state — for fixtures / tests that render a
    /// measure-in-progress scene without driving live pointer events.
    pub fn set_measure(&self, m: MeasureState) {
        self.measure.set(m);
    }
}

/// Cheap summary stats over an elaborated [`Doc`], for the skeleton's status
/// card. Everything here is read straight off the public `ecad-core` API — no
/// routing, no export — so it is safe to compute every frame.
struct DocStats {
    parts: usize,
    nets: usize,
    layers: usize,
    /// Board outline extent in mm (width, height), if the source authored a
    /// board outline.
    board_mm: Option<(f64, f64)>,
}

impl DocStats {
    fn of(doc: &Doc) -> Self {
        let stackup = ecad_core::elaborate::stackup(&doc.source);
        // Layer count = copper slabs (the meaningful "layers" a board has).
        let layers = stackup.copper_slabs().len();
        let board_mm = ecad_core::elaborate::board_region(&doc.source)
            .and_then(|region| region.bbox())
            .map(|(min, max)| {
                let mm = ecad_core::doc::MM as f64;
                ((max.x - min.x) as f64 / mm, (max.y - min.y) as f64 / mm)
            });
        DocStats {
            parts: doc.components.len(),
            nets: doc.nets.len(),
            layers,
            board_mm,
        }
    }
}

impl EcadApp {
    /// Is the layer with `key` currently visible? Layers default on; the toggle
    /// records only the *hidden* set.
    fn layer_visible(&self, key: &str) -> bool {
        !self.hidden.borrow().contains(key)
    }

    /// The board viewer body: the layered canvas viewport (center) + the inspector +
    /// layer panels (right). Only reached when a [`BoardView`] projected successfully.
    fn board_body(&self, cx: &BuildCx, view: &BoardView) -> El {
        // Static layer Els, stacked in draw order, filtered by visibility. Cloning
        // cached assets only — no re-tessellation (the layered-canvas commitment).
        let mut canvas_children = view
            .canvas
            .layer_els(&view.layers, |id| self.layer_visible(&id.key()));
        // The per-frame dynamic overlay: selection / hover highlights + the measure
        // preview. Rebuilt each frame from the live selection + tool state; it never
        // touches the cached static layers above.
        let overlay = self.build_overlay(view);
        if let Some(el) = view.canvas.overlay_el(&overlay) {
            canvas_children.push(el);
        }

        let board_pane = viewport(canvas_children)
            .key(CANVAS_KEY)
            .min_zoom(0.1)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));

        let zoom = cx.viewport_view(CANVAS_KEY).map_or(1.0, |v| v.zoom);

        column([
            self.viewer_toolbar(zoom),
            row([board_pane, self.right_sidebar(view)])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            self.status_bar(zoom),
        ])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    /// Build the per-frame dynamic overlay from the live selection + measure state:
    /// the world-space shapes to highlight (selection bright, hover dim) and the
    /// measure segment. Re-derives highlight geometry from the pick candidates by id —
    /// geometry is never stored in the selection model (commitment 2).
    fn build_overlay(&self, view: &BoardView) -> Overlay {
        let sel = self.domain.selection.borrow();
        let mut highlights: Vec<(Shape2D, bool)> = Vec::new();
        for c in &view.candidates {
            // Skip highlights on hidden layers (a selected feature on a hidden layer
            // stays selected but is not drawn — it can't be seen anyway).
            if !self.layer_visible(&c.layer.key()) {
                continue;
            }
            if sel.is_selected(&c.id) {
                highlights.push((c.shape.clone(), false));
            } else if sel.is_hovered(&c.id) {
                highlights.push((c.shape.clone(), true));
            }
        }
        let measure = if self.tool.get() == Tool::Measure {
            self.measure.get().segment()
        } else {
            None
        };
        Overlay {
            highlights,
            measure,
        }
    }

    /// The right sidebar: the properties inspector (above) over the layer panel
    /// (below), matching the mockup anatomy. The inspector shows the selected entity;
    /// with nothing selected it is the m2 stats card (the empty state).
    fn right_sidebar(&self, view: &BoardView) -> El {
        // A fixed-width scrollable column: the inspector (Hug) above the layer panel.
        // Scrollable so a long inspector (many pin rows) + the full layer list never
        // overflow the pane height — the two panels share the column and clip cleanly.
        scroll([
            column([self.inspector_panel(view), self.layer_panel(&view.layers)])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0)),
        ])
        .width(Size::Fixed(248.0))
        .height(Size::Fill(1.0))
    }

    /// The inspector panel: an identity card + key/value rows for the single selected
    /// entity, or the m2 stats card when nothing is selected. Every value is projected
    /// live from the doc via [`InspectorData::project`] — nothing hardcoded.
    fn inspector_panel(&self, view: &BoardView) -> El {
        let doc = match &self.domain.doc {
            Ok(doc) => doc,
            Err(_) => return self.empty_inspector(view),
        };
        let sel = self.domain.selection.borrow();
        let Some(id) = sel.single() else {
            return self.empty_inspector(view);
        };
        let Some(data) = InspectorData::project(id, doc, &self.domain.lib) else {
            return self.empty_inspector(view);
        };

        let mut children: Vec<El> = vec![
            // Identity card: kind label + large primary id.
            column([text(data.kind).muted().mono(), h3(data.primary)]).gap(tokens::SPACE_1),
        ];
        for r in &data.rows {
            children.push(field_row(r.key.clone(), text(r.value.clone()).mono()));
        }
        sidebar([sidebar_header([h3("Properties")]), sidebar_group(children)])
            .width(Size::Fill(1.0))
            .height(Size::Hug)
    }

    /// The inspector's empty state: the m2 doc stats (kept — it is the no-selection
    /// content, per the spec), rendered as sidebar rows so it fits the narrow column.
    fn empty_inspector(&self, _view: &BoardView) -> El {
        match &self.domain.doc {
            Ok(doc) => {
                let s = DocStats::of(doc);
                let board = match s.board_mm {
                    Some((w, h)) => format!("{w:.1} x {h:.1} mm"),
                    None => "none".to_string(),
                };
                sidebar([
                    sidebar_header([h3("Properties")]),
                    sidebar_group([
                        text("No selection").muted(),
                        field_row("Parts", text(s.parts.to_string()).mono()),
                        field_row("Nets", text(s.nets.to_string()).mono()),
                        field_row("Copper layers", text(s.layers.to_string()).mono()),
                        field_row("Board", text(board).mono()),
                    ]),
                ])
                .width(Size::Fill(1.0))
                .height(Size::Hug)
            }
            Err(_) => sidebar([sidebar_header([h3("Properties")])])
                .width(Size::Fill(1.0))
                .height(Size::Hug),
        }
    }

    /// The toolbar: app title, filename badge, Fit / Reset framing buttons, and a
    /// live zoom-percent readout.
    fn viewer_toolbar(&self, zoom: f32) -> El {
        let name = self
            .domain
            .filename
            .clone()
            .unwrap_or_else(|| "untitled".into());
        let active = self.tool.get();
        // The global tool palette (structural commitment 4): two toggle buttons, the
        // active one filled (`.primary()`), matching the mockup's active-tool cue.
        let tool_buttons: Vec<El> = Tool::all()
            .into_iter()
            .map(|t| {
                let b = button(t.label()).key(t.key());
                if t == active { b.primary() } else { b }
            })
            .collect();
        toolbar([
            toolbar_title("ecad"),
            badge(name).info(),
            spacer(),
            row(tool_buttons).gap(tokens::SPACE_1),
            text(format!("{:.0}%", zoom * 100.0)).muted().mono(),
            button("Fit").key("fit"),
            button("Reset").key("reset"),
        ])
        .gap(tokens::SPACE_2)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    }

    /// The right sidebar layer panel: one row per layer (top of the stack first),
    /// each a colour swatch, name, and a visibility switch. Order mirrors draw
    /// order reversed, so the top copper reads at the top of the list.
    fn layer_panel(&self, layers: &[BoardLayer]) -> El {
        // Draw order is bottom-first; the panel lists top-first.
        let rows: Vec<El> = layers.iter().rev().map(|l| self.layer_row(l)).collect();
        sidebar([
            sidebar_header([h3("Layers")]),
            sidebar_group([
                sidebar_group_label("Board"),
                column(rows).gap(tokens::SPACE_1),
            ]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One layer-panel row: colour swatch + name + a visibility [`switch`].
    fn layer_row(&self, l: &BoardLayer) -> El {
        let key = l.id.key();
        let swatch = El::new(Kind::Custom("layer-swatch"))
            .fill(l.color)
            .stroke(tokens::BORDER)
            .radius(3.0)
            .width(Size::Fixed(14.0))
            .height(Size::Fixed(14.0));
        row([
            swatch,
            text(l.name.clone()).width(Size::Fill(1.0)),
            switch(switch_key(&key), self.layer_visible(&key)),
        ])
        .align(Align::Center)
        .gap(tokens::SPACE_2)
        .padding(Sides::y(tokens::SPACE_1))
    }

    /// The bottom status bar (mockup taste): the live cursor position in board
    /// coordinates and the zoom percent. The cursor readout updates on pointer
    /// enter and while panning — see the module deviation note on free-hover.
    fn status_bar(&self, zoom: f32) -> El {
        let cursor = match self.cursor_board_mm.get() {
            Some((x, y)) => format!("X {x:.2}  Y {y:.2} mm"),
            None => "X --  Y -- mm".to_string(),
        };
        let mut items: Vec<El> = vec![text(cursor).muted().mono()];

        // The measure readout (mockup taste: dx/dy/dist in the status bar) — shown only
        // in Measure mode with a segment in progress.
        if self.tool.get() == Tool::Measure
            && let Some((dx, dy, dist)) = self.measure.get().readout()
        {
            items.push(text(format_readout(dx, dy, dist)).mono());
        }

        items.push(spacer());

        // The selected net name (mockup taste: the status bar carries the selected
        // net). Derived from the single selection via the inspector projection.
        if let Some(net) = self.selected_net() {
            items.push(badge(format!("net {net}")).info());
        }
        items.push(text(format!("Zoom {:.0}%", zoom * 100.0)).muted().mono());

        toolbar(items)
            .gap(tokens::SPACE_3)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_1))
    }

    /// The net name of the current single selection, if it belongs to one (a trace /
    /// via / pin / pour / net selection). `None` for a part or empty selection.
    fn selected_net(&self) -> Option<String> {
        let doc = self.domain.doc.as_ref().ok()?;
        let sel = self.domain.selection.borrow();
        let id = sel.single()?;
        InspectorData::project(id, doc, &self.domain.lib)?.net
    }
}

impl App for EcadApp {
    fn build(&self, cx: &BuildCx) -> El {
        // The board view when a document projected; otherwise the m1 no-document /
        // error states, kept working.
        match (&self.domain.doc, &self.board) {
            (Ok(_), Some(view)) => page([self.board_body(cx, view)]),
            // Loaded but projection failed (or no board outline): fall back to the
            // stats card so the user still sees something, never a blank window.
            (Ok(doc), None) => {
                let chrome = toolbar([
                    toolbar_title("ecad"),
                    spacer(),
                    badge(
                        self.domain
                            .filename
                            .clone()
                            .unwrap_or_else(|| "untitled".into()),
                    )
                    .info(),
                ])
                .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2));
                page([column([chrome, stats_card(&DocStats::of(doc))])
                    .gap(tokens::SPACE_4)
                    .height(Size::Fill(1.0))])
            }
            (Err(message), _) => {
                let chrome = toolbar([
                    toolbar_title("ecad"),
                    spacer(),
                    badge("no document").muted(),
                ])
                .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2));
                page([column([chrome, error_card(message)])
                    .gap(tokens::SPACE_4)
                    .height(Size::Fill(1.0))])
            }
        }
    }

    fn before_build(&mut self) {
        // Queue the initial fit-to-content once, on the first frame after a board
        // loaded — the layout pass resolves it against the live viewport rect and
        // content extents (only known mid-frame).
        if self.board.is_some() && !self.fitted.get() {
            self.pending.borrow_mut().push(ViewportRequest::FitContent {
                key: CANVAS_KEY.to_string(),
                padding: 24.0,
            });
            self.fitted.set(true);
        }
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // Tool palette toggles (structural commitment 4). Switching tools cancels any
        // in-progress measure preview (clean cancel on mode change).
        for t in Tool::all() {
            if event.is_click_or_activate(t.key()) {
                if self.tool.get() != t {
                    self.measure.set(MeasureState::default());
                }
                self.tool.set(t);
                return;
            }
        }

        // Escape: cancel a measure in progress if any; else clear the selection.
        // Routed as `UiEventKind::Escape` (window-level when nothing is focused).
        if event.kind == UiEventKind::Escape {
            let mut m = self.measure.get();
            if self.tool.get() == Tool::Measure && m.segment().is_some() {
                m.cancel();
                self.measure.set(m);
            } else {
                self.domain.selection.borrow_mut().clear();
            }
            return;
        }

        // Toolbar framing buttons.
        if event.is_click_or_activate("fit") {
            self.pending.borrow_mut().push(ViewportRequest::FitContent {
                key: CANVAS_KEY.to_string(),
                padding: 24.0,
            });
            return;
        }
        if event.is_click_or_activate("reset") {
            self.pending.borrow_mut().push(ViewportRequest::ResetView {
                key: CANVAS_KEY.to_string(),
            });
            return;
        }

        // Layer visibility switches: route is `switch:layer:<name>`. Controlled
        // widget — fold the event over the derived `visible` bool with the switch's
        // own `apply_event` (README idiom), then reconcile the `hidden` set (our
        // canonical state) to the flipped value.
        if let Some(view) = &self.board {
            for l in &view.layers {
                let key = l.id.key();
                let sk = switch_key(&key);
                let mut visible = self.layer_visible(&key);
                if switch::apply_event(&mut visible, &event, &sk) {
                    let mut hidden = self.hidden.borrow_mut();
                    if visible {
                        hidden.remove(&key);
                    } else {
                        hidden.insert(key);
                    }
                    return;
                }
            }
        }

        // Canvas pointer interaction. The canvas interior is one keyed viewport; a
        // pointer event over it routes to `CANVAS_KEY` (empty board) or to a stacked
        // layer / overlay El. Any of those is a canvas target. We need the viewport
        // rect + view to map the pointer to board coordinates.
        if !is_canvas_target(event.target_key()) {
            return;
        }
        let (Some(pos), Some(view)) = (event.pointer_pos(), &self.board) else {
            return;
        };
        let (Some(rect), Some(vv)) = (cx.rect_of_key(CANVAS_KEY), cx.viewport_view(CANVAS_KEY))
        else {
            return;
        };
        let el_rect = (rect.x, rect.y, rect.w, rect.h);

        // Cursor readout in board mm (any pointer-carrying canvas event updates it;
        // free hover emits no event — the known 0.4.5 limit).
        let content_px = vv.unproject(pos, (rect.x, rect.y));
        if let Some(mm) = view.canvas.content_px_to_board_mm(content_px, el_rect) {
            self.cursor_board_mm.set(Some(mm));
        }

        // The board point in nm for hit-testing (composes unproject + viewBox/scale +
        // y-flip + mm→nm).
        let Some(p) = pick::pointer_to_board_nm(&view.canvas, pos, el_rect, vv) else {
            return;
        };
        let tol = pick::tolerance_nm(PICK_TOL_PX, vv.zoom);

        match (self.tool.get(), event.kind) {
            // Select tool, click: pick and single-select (or clear on empty).
            (Tool::Select, UiEventKind::Click) => {
                let hit =
                    pick::resolve(&view.candidates, p, tol, |id| self.layer_visible(&id.key()));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.select_only(pick.id),
                    None => sel.clear(),
                }
            }
            // Select tool, hover-class event (enter/drag): update the hover highlight.
            // Free hover emits nothing, so this only fires on enter/drag/down.
            (Tool::Select, UiEventKind::PointerEnter | UiEventKind::Drag) => {
                let hit =
                    pick::resolve(&view.candidates, p, tol, |id| self.layer_visible(&id.key()));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.hover_only(pick.id),
                    None => sel.clear_hover(),
                }
            }
            (Tool::Select, UiEventKind::PointerLeave) => {
                self.domain.selection.borrow_mut().clear_hover();
            }
            // Measure tool, click: anchor then set the second point.
            (Tool::Measure, UiEventKind::Click) => {
                let mut m = self.measure.get();
                m.click(p);
                self.measure.set(m);
            }
            // Measure tool, live move (enter/drag): drag the moving end.
            (Tool::Measure, UiEventKind::PointerEnter | UiEventKind::Drag) => {
                let mut m = self.measure.get();
                m.hover(p);
                self.measure.set(m);
            }
            _ => {}
        }
    }

    fn drain_viewport_requests(&mut self) -> Vec<ViewportRequest> {
        std::mem::take(&mut self.pending.borrow_mut())
    }
}

/// The dark canvas background behind the board — an ECAD-dark near-black.
const CANVAS_BG: Color = Color::srgb_token("ecad.canvas.bg", 0x12, 0x14, 0x18, 0xff);

/// The event-route key of a layer's visibility switch.
fn switch_key(layer_key: &str) -> String {
    format!("switch:{layer_key}")
}

/// Is this event target inside the board canvas? The interior is one keyed viewport,
/// but a pointer event may route to the viewport (`CANVAS_KEY`, over bare board) or to
/// one of the stacked layer / overlay `El`s (keyed `layer:*` / `overlay:*`). All of
/// those are canvas hits; chrome (toolbar, sidebar) is not.
fn is_canvas_target(target: Option<&str>) -> bool {
    match target {
        Some(k) => k == CANVAS_KEY || k.starts_with("layer:") || k.starts_with("overlay:"),
        None => false,
    }
}

/// The document-loaded body: a card of cheap doc stats.
fn stats_card(stats: &DocStats) -> El {
    let board = match stats.board_mm {
        Some((w, h)) => format!("{w:.1} x {h:.1} mm"),
        None => "no board outline".to_string(),
    };
    titled_card(
        "Document",
        [
            field_row("Parts", text(stats.parts.to_string())),
            field_row("Nets", text(stats.nets.to_string())),
            field_row("Copper layers", text(stats.layers.to_string())),
            field_row("Board outline", text(board)),
        ],
    )
    .width(Size::Fixed(420.0))
}

/// The parse/elaborate-failure body: surface the error, never crash (the
/// permissive philosophy starts here).
fn error_card(message: &str) -> El {
    // The empty state uses the same path — "no document" is just an `Err`.
    if message == "no document" {
        return titled_card(
            "No document",
            [text("Pass a path to a .ecad file to load a document.").muted()],
        )
        .width(Size::Fixed(420.0));
    }
    alert([
        alert_title("Could not load document"),
        alert_description(message.to_string()),
    ])
    .destructive()
    .width(Size::Fixed(420.0))
}
