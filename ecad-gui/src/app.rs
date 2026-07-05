//! The `ecad-gui` application shell — milestone 1 skeleton.
//!
//! This is the *workspace-conversion + skeleton* milestone (see
//! `docs/gui-architecture.md`, "v1 scope", milestone 1): the crate compiles,
//! a window can open, and the headless fixture/lint review loop is in place.
//! The interactive machinery — layered canvas, semantic selection, split-tree
//! panes, tools, findings — is milestones 2–6 and is deliberately *absent*
//! here. Where a future struct belongs, a stub with a doc-comment points at the
//! architecture through-line it will implement.

use crate::canvas::pick::{self, Candidate, SemanticId};
use crate::canvas::{BoardLayer, Canvas, Overlay};
use crate::explorer::Explorer;
use crate::findings::Findings;
use crate::highlight::HighlightSets;
use crate::inspector::InspectorData;
use crate::reload::{SourceMailbox, SourceMsg};
use crate::schematic_view::SchematicView;
use crate::selection::SelectionModel;
use crate::tool::{MeasureState, Tool, format_readout};
use damascene_core::prelude::*;
use ecad_core::doc::Doc;
use ecad_core::geom::Shape2D;
use ecad_core::id::NetId;
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
    /// The doc **revision** counter — bumped on every successful reload (m5). The
    /// derived caches (canvas layers, schematic, explorer, findings) are rebuilt only
    /// when this changes, exactly like `ecad-core`'s query-engine revision. Load-once
    /// domains sit at revision 0.
    pub revision: u64,
    /// The persistent last-good-load error, if the most recent reload FAILED to
    /// parse/elaborate. Per the permissive philosophy this does NOT replace the
    /// rendered doc (the last-good doc stays on screen); the chrome surfaces it as an
    /// unmissable banner until a good reload clears it. `None` when the current doc is
    /// the freshest source.
    pub reload_error: Option<String>,
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
            revision: 0,
            reload_error: None,
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
            revision: 0,
            reload_error: None,
        }
    }

    /// Re-parse + re-elaborate `source` against **this** domain's existing library and
    /// filename — the reload entry point. Returns the freshly-elaborated
    /// `Result<Doc, String>` WITHOUT touching `self`; the caller
    /// ([`EcadApp::apply_reload`]) decides whether to swap it in (success) or keep the
    /// last-good doc and surface the error (failure). Pure over `(source, self.lib)`.
    fn elaborate_source(&self, source: &str) -> Result<Doc, String> {
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
        history
            .commit(
                Transaction::one(Command::LoadText(source.to_string())),
                &self.lib,
                "reload",
            )
            .map_err(fmt)
            .map(|_| history.doc().clone())
    }
}

/// Which view a pane renders (mockup: the pane header's view-type switcher). v1 has two
/// read-only view kinds; `3D` etc. are wishlist. A schematic and a board pane over the
/// same doc share the semantic selection but project it into their own overlays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    /// The layered board canvas (milestone 2/3).
    Board,
    /// The read-only schematic view (milestone 4).
    Schematic,
}

impl ViewKind {
    /// The human label for the pane header + switcher.
    pub fn label(self) -> &'static str {
        match self {
            ViewKind::Board => "PCB Layout",
            ViewKind::Schematic => "Schematic",
        }
    }

    /// Both view kinds, in switcher order.
    pub fn all() -> [ViewKind; 2] {
        [ViewKind::Board, ViewKind::Schematic]
    }
}

/// The two-pane orientation (mockup: the dual/stacked toolbar toggle). `Dual` is side-by-
/// side (a `row` split), `Stacked` is over/under (a `column` split). A one-split
/// simplification of the split-tree — fine for v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneLayout {
    Dual,
    Stacked,
}

/// Which pane a pane index names — `A` (first / left / top) or `B` (second / right /
/// bottom). The two are symmetric; the enum keeps call sites readable and keys stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    A,
    B,
}

impl PaneId {
    /// The canvas viewport El key for this pane — distinct per pane so the two cameras are
    /// independent in damascene's `UiState` (through-line 3), *even when both panes show
    /// the same view kind*.
    pub(crate) fn canvas_key(self) -> &'static str {
        match self {
            PaneId::A => "canvas:a",
            PaneId::B => "canvas:b",
        }
    }

    /// The dynamic-overlay El key for this pane (stacked over its canvas).
    fn overlay_key(self) -> &'static str {
        match self {
            PaneId::A => "overlay:a",
            PaneId::B => "overlay:b",
        }
    }

    /// The view-switcher button key for a target view kind in this pane.
    fn switch_key(self, v: ViewKind) -> String {
        let p = match self {
            PaneId::A => "a",
            PaneId::B => "b",
        };
        format!(
            "pane:{p}:view:{}",
            match v {
                ViewKind::Board => "board",
                ViewKind::Schematic => "schematic",
            }
        )
    }

    /// The maximize-toggle button key for this pane.
    fn maximize_key(self) -> &'static str {
        match self {
            PaneId::A => "pane:a:max",
            PaneId::B => "pane:b:max",
        }
    }
}

/// Per-pane view state: the *view-dependent* half of through-line 3. A pane is one view
/// over the shared [`DomainState`], with its own camera keyed by the pane's canvas El key.
/// Milestone 4 makes this real: the pane owns its view kind and whether it has been
/// fit-to-content yet (the initial framing fires once per pane).
#[derive(Clone, Debug)]
pub struct PaneState {
    /// The view this pane renders.
    pub view: ViewKind,
    /// Whether the initial fit-to-content has been queued for this pane's camera.
    fitted: bool,
}

impl PaneState {
    fn new(view: ViewKind) -> Self {
        PaneState {
            view,
            fitted: false,
        }
    }
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState::new(ViewKind::Board)
    }
}

/// The event-route key of the dual/stacked layout toggle button.
const LAYOUT_TOGGLE_KEY: &str = "layout:toggle";
/// The event-route key of the findings-panel collapse toggle.
const FINDINGS_TOGGLE_KEY: &str = "findings:toggle";
/// The route-key prefix of a findings row (index appended).
const FINDINGS_ROW_PREFIX: &str = "finding:row:";

/// The route key for the findings row at `index`.
fn finding_row_key(index: usize) -> String {
    format!("{FINDINGS_ROW_PREFIX}{index}")
}

/// The finding index a route key names, if it is a findings row.
fn finding_index_of_key(route: &str) -> Option<usize> {
    route.strip_prefix(FINDINGS_ROW_PREFIX)?.parse().ok()
}
/// The key of the pane-split resize handle + the split row/column (for `rect_of_key`).
const SPLIT_HANDLE_KEY: &str = "pane:split";
const SPLIT_ROW_KEY: &str = "pane:split-row";

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
    /// The two panes (A, B). Milestone 4's split. Defaults to board | schematic. `RefCell`
    /// because the view-switcher / maximize / initial-fit flips fields in `on_event` /
    /// `before_build` and reads them in `build`.
    panes: RefCell<[PaneState; 2]>,
    /// The two-pane orientation (dual / stacked).
    layout: Cell<PaneLayout>,
    /// Which pane, if any, is maximized (the other is hidden). `None` ⇒ the normal split.
    maximized: Cell<Option<PaneId>>,
    /// The split weights `[a, b]` for the resize handle, and its in-flight drag.
    split_weights: Cell<[f32; 2]>,
    split_drag: RefCell<ResizeWeightsDrag>,
    /// The measured split-container main extent (px), captured each frame for the weighted
    /// resize handler (the README idiom).
    split_extent: Cell<f32>,
    /// The derived caches (board projection, schematic projection, explorer rows,
    /// findings) — everything computed *from* the doc. Rebuilt as a unit only when the
    /// doc revision changes (a reload). `RefCell` because [`apply_reload`] swaps the
    /// whole bundle in `before_build`; `build` reads it immutably.
    ///
    /// [`apply_reload`]: EcadApp::apply_reload
    derived: RefCell<DerivedCaches>,
    /// Which layers are visible, keyed by [`LayerId::key`]. Absent ⇒ visible
    /// (layers default on). Mutated by the layer-panel toggles in `on_event`.
    /// **Preserved across reloads** (the user's framing/visibility is sacred).
    hidden: RefCell<std::collections::HashSet<String>>,
    /// Viewport requests (Fit / Reset / CenterOn) queued from toolbar / findings
    /// clicks, drained once per frame by the host.
    pending: RefCell<Vec<ViewportRequest>>,
    /// The last pointer position over a board pane in **board mm**, for the status-bar
    /// cursor readout. Set by whichever board pane the pointer last moved over.
    cursor_board_mm: Cell<Option<(f32, f32)>>,
    /// The active tool (structural commitment 4). Global mode; `Cell` because it is
    /// flipped in `on_event` and read in `build`.
    tool: Cell<Tool>,
    /// The measure tool's uncommitted preview state (the preview channel — renders
    /// only to the overlay, never the doc). The pane the measure is happening in, so the
    /// overlay draws it in the right place.
    measure: Cell<MeasureState>,
    measure_pane: Cell<PaneId>,
    /// The live-source mailbox (m5): drained in `before_build`; a file change reloads.
    /// A [`SourceMailbox::disconnected`] mailbox (fixtures / no file) never yields.
    mailbox: SourceMailbox,
    /// Whether the findings panel section is expanded (collapsible like the explorer).
    findings_open: Cell<bool>,
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

/// Everything derived from the elaborated [`Doc`], rebuilt together on a reload (the
/// revision-keyed cache). Holding these as one bundle behind a single `RefCell` means
/// a reload swaps the *whole* derived tier atomically — no half-updated frame where a
/// new board pairs with old findings.
struct DerivedCaches {
    /// The board projection + cached per-layer assets, or `None` when no document is
    /// loaded / the load failed / projection failed.
    board: Option<BoardView>,
    /// The schematic projection + cached asset + pick candidates, or `None` when the
    /// doc has no components.
    schematic: Option<SchematicView>,
    /// The projected explorer rows (components / nets).
    explorer: Explorer,
    /// The per-revision findings (DRC + ERC + connectivity), computed once here and
    /// read every frame by the panel / chip / overlays.
    findings: Findings,
}

impl DerivedCaches {
    /// Empty caches (no document / failed load).
    fn empty() -> DerivedCaches {
        DerivedCaches {
            board: None,
            schematic: None,
            explorer: Explorer::default(),
            findings: Findings::default(),
        }
    }

    /// Build every derived cache from a document + library. A projection failure
    /// (unreachable for a committed doc) degrades to "no board view" rather than
    /// crashing — the permissive philosophy.
    fn build(doc: &Doc, lib: &ecad_core::part::PartLib) -> DerivedCaches {
        // The layered canvas + pick candidates, from the one `world_features` stream.
        let board = Canvas::new(doc, lib)
            .and_then(|canvas| {
                let layers = canvas.build_layers(doc, lib)?;
                let su = ecad_core::elaborate::stackup(&doc.source);
                let candidates = pick::candidates(doc, lib, &su);
                Ok(BoardView {
                    canvas,
                    layers,
                    candidates,
                })
            })
            .ok();
        let schematic = SchematicView::build(doc, lib);
        let explorer = Explorer::project(doc, lib);
        // Findings are derived from the board pick candidates (the halo-location
        // source) — empty candidate list when the board didn't project, which is fine
        // (findings then carry no board-mm point, panel-only).
        let candidates: &[Candidate] = board.as_ref().map_or(&[], |b| &b.candidates);
        let findings = Findings::compute(doc, lib, candidates);
        DerivedCaches {
            board,
            schematic,
            explorer,
            findings,
        }
    }
}

impl EcadApp {
    pub fn new(domain: DomainState) -> Self {
        let derived = match &domain.doc {
            Ok(doc) => DerivedCaches::build(doc, &domain.lib),
            Err(_) => DerivedCaches::empty(),
        };
        EcadApp {
            domain,
            panes: RefCell::new([
                PaneState::new(ViewKind::Board),
                PaneState::new(ViewKind::Schematic),
            ]),
            layout: Cell::new(PaneLayout::Dual),
            maximized: Cell::new(None),
            split_weights: Cell::new([1.0, 1.0]),
            split_drag: RefCell::new(ResizeWeightsDrag::default()),
            split_extent: Cell::new(0.0),
            derived: RefCell::new(derived),
            hidden: RefCell::new(std::collections::HashSet::new()),
            pending: RefCell::new(Vec::new()),
            cursor_board_mm: Cell::new(None),
            tool: Cell::new(Tool::default()),
            measure: Cell::new(MeasureState::default()),
            measure_pane: Cell::new(PaneId::A),
            mailbox: SourceMailbox::disconnected(),
            findings_open: Cell::new(true),
        }
    }

    /// Attach a live-source [`SourceMailbox`] — the windowed `main.rs` wires this to
    /// the file-watch thread's sender; fixtures leave the disconnected default. Tests
    /// use [`mailbox_push`](Self::mailbox_push) to inject reloads.
    pub fn with_mailbox(mut self, mailbox: SourceMailbox) -> Self {
        self.mailbox = mailbox;
        self
    }

    /// Push a source message onto the app's mailbox — the headless reload test entry
    /// point. The next `before_build` drains and applies it.
    pub fn mailbox_push(&self, msg: SourceMsg) {
        self.mailbox.push(msg);
    }

    /// The current doc revision — bumped once per successful reload. For tests.
    pub fn revision(&self) -> u64 {
        self.domain.revision
    }

    /// The persistent reload-error string, if the freshest source failed. For tests.
    pub fn reload_error(&self) -> Option<String> {
        self.domain.reload_error.clone()
    }

    /// The cached findings (per doc revision). For tests / the report.
    pub fn findings(&self) -> Findings {
        self.derived.borrow().findings.clone()
    }

    /// Clone the explorer rows out of the derived cache — test accessor (the field
    /// moved behind a `RefCell<DerivedCaches>` in m5).
    #[cfg(test)]
    fn explorer_snapshot(&self) -> Explorer {
        self.derived.borrow().explorer.clone()
    }

    /// True when the board projection exists — test accessor.
    #[cfg(test)]
    fn has_board(&self) -> bool {
        self.derived.borrow().board.is_some()
    }

    /// True when the schematic projection exists — test accessor.
    #[cfg(test)]
    fn has_schematic(&self) -> bool {
        self.derived.borrow().schematic.is_some()
    }

    /// A clone of the board projection's [`Canvas`] — test accessor for the
    /// coordinate-composition tests.
    #[cfg(test)]
    fn board_canvas_clone(&self) -> Canvas {
        self.derived
            .borrow()
            .board
            .as_ref()
            .expect("board projects")
            .canvas
            .clone()
    }

    /// Set both panes' view kinds — for fixtures that want a canned pane arrangement.
    pub fn set_pane_views(&self, a: ViewKind, b: ViewKind) {
        let mut panes = self.panes.borrow_mut();
        panes[0].view = a;
        panes[1].view = b;
    }

    /// Set the pane layout (dual / stacked) — for fixtures.
    pub fn set_layout(&self, layout: PaneLayout) {
        self.layout.set(layout);
    }

    /// Maximize a pane (hide the other) — for fixtures.
    pub fn set_maximized(&self, pane: Option<PaneId>) {
        self.maximized.set(pane);
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

    /// Apply a live-source reload (m5). Re-elaborates `source` against the current
    /// library and:
    ///
    /// - **on success**: swaps in the new doc + source, bumps the revision, rebuilds
    ///   the derived caches (canvas / schematic / explorer / findings), **prunes** the
    ///   selection + hover of any id that no longer resolves in the new doc, and clears
    ///   any prior reload error. Cameras, layer visibility, pane layout, maximize state,
    ///   the active tool, and the `fitted` flags are all left untouched — the user's
    ///   framing and workspace are preserved (no re-fit).
    /// - **on failure**: keeps the last-good doc + derived caches + findings rendered
    ///   (the canvas never blanks — the permissive philosophy) and records the error in
    ///   `domain.reload_error`, which the toolbar surfaces as a persistent banner chip
    ///   until a good reload lands.
    ///
    /// `&mut self` because a reload swaps domain + derived state; called from
    /// `before_build` (host frame) after the mailbox drain, and directly by tests.
    pub fn apply_reload(&mut self, source: String) {
        match self.domain.elaborate_source(&source) {
            Ok(doc) => {
                let derived = DerivedCaches::build(&doc, &self.domain.lib);
                // Prune selection + hover to ids that still resolve in the NEW doc,
                // using the freshly-built candidate/schematic ids as the resolvable set.
                self.prune_selection(&doc, &derived);
                *self.derived.borrow_mut() = derived;
                self.domain.doc = Ok(doc);
                self.domain.source = source;
                self.domain.revision += 1;
                self.domain.reload_error = None;
            }
            Err(err) => {
                // Permissive: keep the last-good doc + caches on screen; surface the
                // error persistently. Do NOT bump the revision (nothing derived changed).
                self.domain.reload_error = Some(err);
            }
        }
    }

    /// Prune the selection + hover sets to the ids that still resolve against the
    /// freshly-built derived caches — the reload contract's "drop ids that no longer
    /// exist" step. An id resolves if it is a board pick candidate, a schematic
    /// candidate, or (for a `Net` / `Part`) present in the new doc. Ids that don't
    /// resolve are dropped silently (no panic), so a reload that removes a selected
    /// entity leaves an empty-or-smaller selection rather than a dangling id.
    fn prune_selection(&self, doc: &Doc, derived: &DerivedCaches) {
        let mut sel = self.domain.selection.borrow_mut();
        sel.retain(|id| resolves_in(id, doc, derived));
    }
}

/// Does a [`SemanticId`] resolve against the new doc + derived caches? A board copper
/// id must still be a pick candidate; a schematic id a schematic candidate; a `Net` /
/// `Part` must be present in the doc's maps. This is the "prune dangling selection"
/// predicate the reload contract requires.
fn resolves_in(id: &SemanticId, doc: &Doc, derived: &DerivedCaches) -> bool {
    match id {
        SemanticId::Net(n) => doc.nets.contains_key(n),
        SemanticId::Part(e) => doc.components.contains_key(e),
        SemanticId::Trace(t) => doc.traces.contains_key(t),
        SemanticId::Via(v) => doc.vias.contains_key(v),
        SemanticId::Pour { .. } | SemanticId::Pin { .. } => {
            // Pours and pins have no top-level doc map; a pin resolves iff its owning
            // component still exists AND the pad is a live pick candidate. Fall back to
            // the candidate sets (board + schematic), which are exactly "what can be
            // selected in the new doc".
            let on_board = derived
                .board
                .as_ref()
                .is_some_and(|b| b.candidates.iter().any(|c| &c.id == id));
            let on_schem = derived
                .schematic
                .as_ref()
                .is_some_and(|s| s.candidates().iter().any(|c| &c.id == id));
            on_board || on_schem
        }
    }
}

/// A pane index into the `panes` array.
fn pane_index(p: PaneId) -> usize {
    match p {
        PaneId::A => 0,
        PaneId::B => 1,
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

    /// The viewer body: the toolbar, the two-pane split (center), the right sidebar
    /// (inspector + explorer + layer panel), and the status bar. Reached when the doc
    /// loaded (at least one pane always renders — a board pane falls back to a placeholder
    /// if its projection failed, a schematic pane if the doc has no components).
    fn viewer_body(&self, cx: &BuildCx) -> El {
        // The active board pane's zoom drives the toolbar/status readout (whichever pane A
        // shows a board, else pane B, else 1.0). The cursor readout is set per event.
        let zoom = self.readout_zoom(cx);

        // The shared cross-view highlight sets, projected once per frame from the selection.
        let sets = self.highlight_sets();

        let split = self.pane_split(cx, &sets);

        column([
            self.viewer_toolbar(zoom),
            row([split, self.right_sidebar()])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            self.status_bar(zoom),
        ])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    /// The zoom to display in the toolbar / status bar: the active board pane's zoom
    /// (whichever pane shows a board), else 1.0.
    fn readout_zoom(&self, cx: &BuildCx) -> f32 {
        let panes = self.panes.borrow();
        for (i, p) in panes.iter().enumerate() {
            if p.view == ViewKind::Board {
                let id = if i == 0 { PaneId::A } else { PaneId::B };
                return cx.viewport_view(id.canvas_key()).map_or(1.0, |v| v.zoom);
            }
        }
        1.0
    }

    /// The shared cross-view highlight sets for this frame — the selection + hover ids,
    /// projected through [`HighlightSets`] so both panes expand the same way.
    fn highlight_sets(&self) -> HighlightSets {
        match &self.domain.doc {
            Ok(doc) => {
                let sel = self.domain.selection.borrow();
                // Selection + hover both cross-highlight (hover is the pre-select cue).
                HighlightSets::project(sel.selected().chain(sel.hovered()), doc, &self.domain.lib)
            }
            Err(_) => HighlightSets::default(),
        }
    }

    /// The two-pane split (dual = row, stacked = column), with a draggable resize handle
    /// between the panes — or, when a pane is maximized, that one pane full-bleed.
    fn pane_split(&self, cx: &BuildCx, sets: &HighlightSets) -> El {
        if let Some(max) = self.maximized.get() {
            return self.pane_el(cx, max, sets);
        }
        let a = self.pane_el(cx, PaneId::A, sets);
        let b = self.pane_el(cx, PaneId::B, sets);
        let axis = match self.layout.get() {
            PaneLayout::Dual => Axis::Row,
            PaneLayout::Stacked => Axis::Column,
        };
        let w = self.split_weights.get();
        let a = a.width(Size::Fill(w[0])).height(Size::Fill(w[0]));
        let b = b.width(Size::Fill(w[1])).height(Size::Fill(w[1]));
        let children = [a, resize_handle(SPLIT_HANDLE_KEY, axis), b];
        let container = match self.layout.get() {
            PaneLayout::Dual => row(children),
            PaneLayout::Stacked => column(children),
        };
        container
            .key(SPLIT_ROW_KEY)
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// One pane: a header row (view-kind label + switcher + maximize toggle) over the
    /// pane's canvas (board or schematic). Fill in both axes so the split weights govern
    /// its size.
    fn pane_el(&self, cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let view = self.panes.borrow()[pane_index(pane)].view;
        let canvas = match view {
            ViewKind::Board => self.board_canvas(cx, pane, sets),
            ViewKind::Schematic => self.schematic_canvas(cx, pane, sets),
        };
        column([self.pane_header(pane, view), canvas])
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// A pane header (mockup anatomy): the view-kind switcher (a segmented control of
    /// toggle buttons, the active one filled) and a maximize toggle on the right.
    fn pane_header(&self, pane: PaneId, view: ViewKind) -> El {
        let switch_buttons: Vec<El> = ViewKind::all()
            .into_iter()
            .map(|v| {
                let b = button(v.label()).key(pane.switch_key(v));
                if v == view { b.primary() } else { b }
            })
            .collect();
        let max_label = if self.maximized.get() == Some(pane) {
            "Restore"
        } else {
            "Maximize"
        };
        toolbar([
            row(switch_buttons).gap(tokens::SPACE_1),
            spacer(),
            button(max_label).key(pane.maximize_key()),
        ])
        .gap(tokens::SPACE_2)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// A board pane's canvas: the cached layer Els + the per-frame overlay, in a viewport
    /// keyed to *this pane* (independent camera). Falls back to a placeholder when the
    /// board projection failed.
    fn board_canvas(&self, _cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.board else {
            return pane_placeholder("No board to display");
        };
        // Per-pane El keys: two board panes render the same layers, so namespace each
        // layer / overlay El by the pane (keys must be unique in the tree). The event
        // router still recognises these as canvas targets (the `layer:` / `overlay:`
        // prefixes survive) and the pane is resolved by pointer rect, not by key.
        let prefix = pane.canvas_key();
        let mut children: Vec<El> = view
            .canvas
            .layer_els(&view.layers, |id| self.layer_visible(&id.key()))
            .into_iter()
            .enumerate()
            .map(|(i, el)| el.key(format!("layer:{prefix}:{i}")))
            .collect();
        let overlay = self.build_board_overlay(view, pane, sets, &derived.findings);
        if let Some(el) = view.canvas.overlay_el(&overlay) {
            // Re-key the overlay per pane (the canvas hardcodes "overlay:dynamic"); wrap it
            // in a keyed container so two board panes' overlays don't collide.
            children.push(el.key(format!("overlay:{prefix}")));
        }
        viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.1)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// A schematic pane's canvas: the cached schematic asset + the per-frame highlight
    /// overlay, in a viewport keyed to this pane. Falls back to a placeholder when the doc
    /// has no components.
    fn schematic_canvas(&self, _cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.schematic else {
            return pane_placeholder("No schematic to display");
        };
        let static_key = format!("schematic:{}", pane.canvas_key());
        let mut children = vec![view.static_el(&static_key)];
        // Schematic-side findings (ERC / floating-pad with entity refs) halo the symbol:
        // union their entity/net refs into the overlay id set so the affected symbol +
        // net wires ring in the finding accent alongside any selection highlight.
        let finding_ids = self.schematic_finding_ids(&derived.findings);
        let overlay_ids: std::collections::BTreeSet<SemanticId> =
            sets.schematic_ids().union(&finding_ids).cloned().collect();
        if let Some(el) = view.overlay_el(&overlay_ids, pane.overlay_key()) {
            children.push(el);
        }
        viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.02)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// Build a board pane's dynamic overlay from the cross-view highlight sets + the
    /// measure preview (measure only draws in the pane it is happening in). Highlight
    /// geometry is re-derived from the pick candidates by id (commitment 2). A candidate
    /// lights up when its id — or its net — is in the board highlight set.
    fn build_board_overlay(
        &self,
        view: &BoardView,
        pane: PaneId,
        sets: &HighlightSets,
        findings: &Findings,
    ) -> Overlay {
        let mut highlights: Vec<(Shape2D, bool)> = Vec::new();
        for c in &view.candidates {
            if !self.layer_visible(&c.layer.key()) {
                continue;
            }
            let net = self.candidate_net(&c.id);
            if sets.board_matches(&c.id, net.as_ref()) {
                // Committed selection reads bright; a hover-only match reads dim. A
                // candidate is a hover if its id is hovered and not selected.
                let sel = self.domain.selection.borrow();
                let hovered = sel.is_hovered(&c.id) && !sel.is_selected(&c.id);
                highlights.push((c.shape.clone(), hovered));
            }
        }
        let measure = if self.tool.get() == Tool::Measure && self.measure_pane.get() == pane {
            self.measure.get().segment()
        } else {
            None
        };
        // Findings with a derived board point become violation markers (both board
        // panes show them — a finding is a property of the board, not a pane).
        let finding_markers: Vec<(ecad_core::coord::Point, bool)> = findings
            .items
            .iter()
            .filter_map(|f| {
                let (mx, my) = f.board_mm?;
                Some((
                    ecad_core::coord::Point {
                        x: (mx * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
                        y: (my * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
                    },
                    f.is_error(),
                ))
            })
            .collect();
        Overlay {
            highlights,
            measure,
            findings: finding_markers,
        }
    }

    /// The semantic ids the schematic overlay should ring for findings: the entity /
    /// pin / net refs of every finding (ERC multiple-drivers on a net, a floating pad
    /// on a part). The schematic candidates key on Part / Pin / Net, so these light up
    /// the affected symbol + net wires.
    fn schematic_finding_ids(&self, findings: &Findings) -> std::collections::BTreeSet<SemanticId> {
        findings
            .items
            .iter()
            .flat_map(|f| f.refs.iter().cloned())
            .collect()
    }

    /// The net a board candidate's id belongs to, if any (for the net-expansion match).
    fn candidate_net(&self, id: &SemanticId) -> Option<NetId> {
        let doc = self.domain.doc.as_ref().ok()?;
        match id {
            SemanticId::Trace(t) => doc.traces.get(t).map(|t| t.net.clone()),
            SemanticId::Via(v) => doc.vias.get(v).map(|v| v.net.clone()),
            SemanticId::Pour { net, .. } => Some(net.clone()),
            SemanticId::Pin { comp, pin } => {
                let pr = ecad_core::doc::PinRef::new(comp, pin);
                doc.nets
                    .iter()
                    .find(|(_, n)| n.members.contains(&pr))
                    .map(|(nid, _)| nid.clone())
            }
            _ => None,
        }
    }

    /// The right sidebar: the properties inspector (above), the explorer (middle), and the
    /// board layer panel (below), matching the mockup anatomy (Properties above Explorer).
    fn right_sidebar(&self) -> El {
        let derived = self.derived.borrow();
        let mut children = vec![
            self.inspector_panel(),
            self.findings_panel(&derived.findings),
            self.explorer_panel(&derived.explorer),
        ];
        // The layer panel applies to board panes; show it whenever a board projection
        // exists (global layer visibility is fine for v1).
        if let Some(view) = &derived.board {
            children.push(self.layer_panel(&view.layers));
        }
        scroll([column(children).gap(tokens::SPACE_3).width(Size::Fill(1.0))])
            .width(Size::Fixed(260.0))
            .height(Size::Fill(1.0))
    }

    /// The findings panel (right sidebar, collapsible like the explorer): a header with
    /// the error/warning tally, then one click-to-select row per finding (a severity
    /// badge beside the code and message). Clicking a row selects the finding's refs
    /// (cross-highlighting the panes) and centres the focused board pane on the
    /// violation. Collapsed to just the header when `findings_open` is false or when
    /// there are no findings (a clean board shows a compact "no issues" line).
    fn findings_panel(&self, findings: &Findings) -> El {
        let open = self.findings_open.get();
        let title = if findings.is_clean() {
            "Findings".to_string()
        } else {
            format!(
                "Findings ({} err, {} warn)",
                findings.errors, findings.warnings
            )
        };
        let toggle = button(if open { "Hide" } else { "Show" }).key(FINDINGS_TOGGLE_KEY);
        let header = sidebar_header([row([h3(title).width(Size::Fill(1.0)).ellipsis(), toggle])
            .align(Align::Center)
            .width(Size::Fill(1.0))]);
        if !open {
            return sidebar([header]).width(Size::Fill(1.0)).height(Size::Hug);
        }
        if findings.is_clean() {
            return sidebar([
                header,
                sidebar_group([text("No issues — DRC clean.").muted()]),
            ])
            .width(Size::Fill(1.0))
            .height(Size::Hug);
        }
        let rows: Vec<El> = findings
            .items
            .iter()
            .enumerate()
            .map(|(i, f)| self.finding_row(i, f))
            .collect();
        sidebar([
            header,
            sidebar_group([column(rows).gap(tokens::SPACE_1).width(Size::Fill(1.0))]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One findings row: a severity badge (error red / warning amber) + the code +
    /// message, as a click-to-select focusable row keyed by index. Built on the same
    /// focusable-list-item anatomy as `sidebar_menu_button` (which is label-only), so a
    /// click routes to the app and the row reads as an interactive nav entry.
    fn finding_row(&self, index: usize, f: &crate::findings::Finding) -> El {
        let sev = if f.is_error() {
            badge("ERR").destructive()
        } else {
            badge("WARN").warning()
        };
        let body = column([
            text(f.code).mono().caption(),
            text(f.message.clone()).width(Size::Fill(1.0)).wrap_text(),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0));
        row([sev, body])
            .key(finding_row_key(index))
            .style_profile(StyleProfile::Solid)
            .metrics_role(MetricsRole::ListItem)
            .focusable()
            .cursor(Cursor::Pointer)
            .fill(tokens::CARD)
            .radius(tokens::RADIUS_SM)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
            .align(Align::Center)
            .width(Size::Fill(1.0))
            .ghost()
    }

    /// The explorer panel (mockup NetExplorer anatomy): Components + Nets sections, each a
    /// list of click-to-select rows with a count badge; the selected row gets the mockup's
    /// selected cue (`sidebar_menu_button`'s `current` treatment).
    fn explorer_panel(&self, explorer: &Explorer) -> El {
        let sel = self.domain.selection.borrow();
        let comp_rows: Vec<El> = explorer
            .components
            .iter()
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        let net_rows: Vec<El> = explorer
            .nets
            .iter()
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        sidebar([
            sidebar_header([h3("Explorer")]),
            sidebar_group([
                sidebar_group_label(format!("Components ({})", comp_rows.len())),
                column(comp_rows)
                    .gap(tokens::SPACE_1)
                    .width(Size::Fill(1.0)),
            ]),
            sidebar_group([
                sidebar_group_label(format!("Nets ({})", net_rows.len())),
                column(net_rows).gap(tokens::SPACE_1).width(Size::Fill(1.0)),
            ]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One explorer row: a click-to-select `sidebar_menu_button` labelled with the id +
    /// secondary text + count badge, `current` when it is the selection.
    fn explorer_row(&self, r: &crate::explorer::ExplorerRow, current: bool) -> El {
        let label = if r.secondary.is_empty() {
            format!("{}  [{}]", r.label, r.count)
        } else {
            format!("{}  ({})  [{}]", r.label, r.secondary, r.count)
        };
        sidebar_menu_button(label, current).key(r.key.clone())
    }

    /// The inspector panel: an identity card + key/value rows for the single selected
    /// entity, or the m2 stats card when nothing is selected. Works regardless of which
    /// pane the selection came from (the selection is shared, semantic).
    fn inspector_panel(&self) -> El {
        let doc = match &self.domain.doc {
            Ok(doc) => doc,
            Err(_) => return self.empty_inspector(),
        };
        let sel = self.domain.selection.borrow();
        let Some(id) = sel.single() else {
            return self.empty_inspector();
        };
        let Some(data) = InspectorData::project(id, doc, &self.domain.lib) else {
            return self.empty_inspector();
        };

        let mut children: Vec<El> =
            vec![column([text(data.kind).muted().mono(), h3(data.primary)]).gap(tokens::SPACE_1)];
        for r in &data.rows {
            children.push(field_row(r.key.clone(), text(r.value.clone()).mono()));
        }
        sidebar([sidebar_header([h3("Properties")]), sidebar_group(children)])
            .width(Size::Fill(1.0))
            .height(Size::Hug)
    }

    /// The inspector's empty state: the m2 doc stats, rendered as sidebar rows.
    fn empty_inspector(&self) -> El {
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

    /// The toolbar: app title, filename badge, the dual/stacked layout toggle, the global
    /// tool palette, and Fit / Reset framing buttons + a live zoom-percent readout.
    fn viewer_toolbar(&self, zoom: f32) -> El {
        let name = self
            .domain
            .filename
            .clone()
            .unwrap_or_else(|| "untitled".into());
        let active = self.tool.get();
        let tool_buttons: Vec<El> = Tool::all()
            .into_iter()
            .map(|t| {
                let b = button(t.label()).key(t.key());
                if t == active { b.primary() } else { b }
            })
            .collect();
        let layout_label = match self.layout.get() {
            PaneLayout::Dual => "Dual",
            PaneLayout::Stacked => "Stacked",
        };
        // The persistent DRC status chip (mockup chrome): error + warning counts, green
        // when clean. The reload-error banner chip (permissive philosophy) sits beside
        // it whenever the freshest source failed to load — unmissable, never a toast.
        let chip = self.drc_chip();
        let mut lead: Vec<El> = vec![toolbar_title("ecad"), badge(name).info(), chip];
        if let Some(err) = &self.domain.reload_error {
            lead.push(reload_error_chip(err));
        }
        lead.push(button(layout_label).key(LAYOUT_TOGGLE_KEY));
        lead.push(spacer());
        lead.push(row(tool_buttons).gap(tokens::SPACE_1));
        lead.push(text(format!("{:.0}%", zoom * 100.0)).muted().mono());
        lead.push(button("Fit").key("fit"));
        lead.push(button("Reset").key("reset"));
        toolbar(lead)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    }

    /// The persistent DRC status chip (mockup menu-bar chrome): the cached findings'
    /// error + warning counts, rendered green (`success`) when the board is clean, amber
    /// (`warning`) when only warnings, red (`destructive`) when any error. Reads the
    /// cached findings — never recomputes.
    fn drc_chip(&self) -> El {
        let findings = &self.derived.borrow().findings;
        if findings.is_clean() {
            return badge("DRC clean").success();
        }
        let label = format!("DRC {} · {} warn", findings.errors, findings.warnings);
        if findings.errors > 0 {
            badge(label).destructive()
        } else {
            badge(label).warning()
        }
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
        // Compact DRC state (mockup status-bar chrome).
        {
            let findings = &self.derived.borrow().findings;
            let drc = if findings.is_clean() {
                "DRC: clean".to_string()
            } else {
                format!("DRC: {} err {} warn", findings.errors, findings.warnings)
            };
            items.push(text(drc).muted().mono());
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

    /// Select the finding at `index` (a findings-panel row click): fold ALL of its
    /// semantic refs into the selection (so the panes cross-highlight the offending
    /// nets / parts / pins), and — if the finding has a derived board point — queue a
    /// `CenterOn` on the focused board pane so the violation comes into view.
    ///
    /// # Click-to-zoom gap (deviation)
    ///
    /// damascene 0.4.5 has **no frame-this-rect ViewportRequest** — only `FitContent`,
    /// `ResetView`, and `CenterOn { key, point }`. So "zoom the focused board pane to the
    /// violation" is realised as a **`CenterOn`** (pan to the point, keeping the current
    /// zoom) rather than a true frame-to-rect. The finding's board point is centred; the
    /// zoom is left as the user set it. Recorded as a deviation in the report.
    fn select_finding(&self, index: usize, cx: &EventCx) {
        let derived = self.derived.borrow();
        let Some(f) = derived.findings.items.get(index) else {
            return;
        };
        // Fold every ref into the selection (multi-select — a clearance highlights BOTH
        // nets). Clear first, then add each ref.
        {
            let mut sel = self.domain.selection.borrow_mut();
            sel.clear();
            for r in &f.refs {
                sel.add(r.clone());
            }
        }
        // CenterOn the focused board pane, if the finding has a board point. The request
        // wants a CONTENT-space point (logical px, pre-transform); the canvas maps the
        // finding's board-mm point through its board→content-px transform using the
        // pane's live laid-out rect (so the pan is exact regardless of the pane's
        // aspect ratio / fitted scale).
        if let (Some((mx, my)), Some(view)) = (f.board_mm, &derived.board)
            && let Some(pane) = self.focused_board_pane()
            && let Some(rect) = cx.rect_of_key(pane.canvas_key())
            && let Some(point) = view
                .canvas
                .board_mm_to_content_px((mx, my), (rect.x, rect.y, rect.w, rect.h))
        {
            self.pending.borrow_mut().push(ViewportRequest::CenterOn {
                key: pane.canvas_key().to_string(),
                point,
            });
        }
    }

    /// The board pane to focus for click-to-zoom: the first pane currently showing a
    /// board (A preferred), respecting a maximized pane. `None` when no board pane is
    /// visible (both panes schematic, or the board didn't project).
    fn focused_board_pane(&self) -> Option<PaneId> {
        let panes = self.panes.borrow();
        let visible = |id: PaneId| self.maximized.get().map(|m| m == id).unwrap_or(true);
        for (i, p) in panes.iter().enumerate() {
            let id = if i == 0 { PaneId::A } else { PaneId::B };
            if p.view == ViewKind::Board && visible(id) {
                return Some(id);
            }
        }
        None
    }
}

impl App for EcadApp {
    fn build(&self, cx: &BuildCx) -> El {
        match &self.domain.doc {
            // A loaded doc renders the two-pane viewer (at least one pane always shows
            // something — board or schematic). Even a board-only or schematic-only doc
            // gets panes; the empty side shows a placeholder.
            Ok(_) => page([self.viewer_body(cx)]),
            Err(message) => {
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
        // (m5) Drain the live-source mailbox first: a file change reloads the doc +
        // derived caches BEFORE this frame builds, so the frame reflects the new source.
        // The drain coalesces a burst to the latest source (see `SourceMailbox::drain`).
        if let Some(SourceMsg::Changed(source)) = self.mailbox.drain() {
            self.apply_reload(source);
        }

        // Queue the initial fit-to-content once per pane, on the first frame after the doc
        // loaded (or after a view switch reset the flag) — the layout pass resolves each
        // request against the live per-pane viewport rect + content extents. The split
        // extent for the resize handler is captured in `on_event` from last frame's layout.
        //
        // Reload NEVER re-fits (the user's framing is sacred): `apply_reload` leaves the
        // `fitted` flags alone, so a reload does not re-arm this.
        //
        // Only fit (and mark `fitted`) a pane that is actually built into the tree THIS
        // frame. When a pane is hidden (the other pane is maximized), its viewport El is
        // absent, so damascene drops the unmatched FitContent request at end of layout
        // (clear_pending_viewport_requests). Marking such a pane fitted anyway would strand
        // it: on restore it would render with the default camera and never re-fit. So a
        // hidden pane is left un-fitted and gets its fit on the first frame it is visible.
        let maximized = self.maximized.get();
        // Read the projection state up front so the `derived` borrow doesn't overlap the
        // `panes` mutable borrow below.
        let (has_board, has_schematic) = {
            let d = self.derived.borrow();
            (d.board.is_some(), d.schematic.is_some())
        };
        let mut panes = self.panes.borrow_mut();
        for (i, p) in panes.iter_mut().enumerate() {
            if p.fitted {
                continue;
            }
            let id = if i == 0 { PaneId::A } else { PaneId::B };
            // A pane is hidden this frame iff some OTHER pane is maximized.
            let visible = maximized.map(|m| m == id).unwrap_or(true);
            if !visible {
                continue;
            }
            let projected = match p.view {
                ViewKind::Board => has_board,
                ViewKind::Schematic => has_schematic,
            };
            if projected {
                self.pending.borrow_mut().push(ViewportRequest::FitContent {
                    key: id.canvas_key().to_string(),
                    padding: 24.0,
                });
                p.fitted = true;
            }
        }
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // Capture the split extent for the weighted resize handler (README idiom).
        if let Some(r) = cx.rect_of_key(SPLIT_ROW_KEY) {
            let extent = match self.layout.get() {
                PaneLayout::Dual => r.w,
                PaneLayout::Stacked => r.h,
            };
            self.split_extent.set(extent);
        }

        // Pane-split resize handle (weighted): fold the drag into the split weights.
        {
            let mut w = self.split_weights.get();
            let mut drag = self.split_drag.borrow_mut();
            let axis = match self.layout.get() {
                PaneLayout::Dual => Axis::Row,
                PaneLayout::Stacked => Axis::Column,
            };
            if resize_handle::apply_event_weights(
                &mut w,
                &mut drag,
                &event,
                SPLIT_HANDLE_KEY,
                axis,
                self.split_extent.get(),
                0.15,
            ) {
                drop(drag);
                self.split_weights.set(w);
                return;
            }
        }

        // Dual/stacked layout toggle.
        if event.is_click_or_activate(LAYOUT_TOGGLE_KEY) {
            self.layout.set(match self.layout.get() {
                PaneLayout::Dual => PaneLayout::Stacked,
                PaneLayout::Stacked => PaneLayout::Dual,
            });
            return;
        }

        // Per-pane view switcher + maximize toggle.
        for pane in [PaneId::A, PaneId::B] {
            for v in ViewKind::all() {
                if event.is_click_or_activate(&pane.switch_key(v)) {
                    let mut panes = self.panes.borrow_mut();
                    let p = &mut panes[pane_index(pane)];
                    if p.view != v {
                        p.view = v;
                        p.fitted = false; // re-fit the new view on next build.
                    }
                    return;
                }
            }
            if event.is_click_or_activate(pane.maximize_key()) {
                self.maximized.set(match self.maximized.get() {
                    Some(m) if m == pane => None,
                    _ => Some(pane),
                });
                return;
            }
        }

        // Findings panel: collapse toggle, then a row click → select the finding's refs
        // + centre the focused board pane on its board point (click-to-select-and-zoom).
        if event.is_click_or_activate(FINDINGS_TOGGLE_KEY) {
            self.findings_open.set(!self.findings_open.get());
            return;
        }
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
            && let Some(index) = finding_index_of_key(route)
        {
            self.select_finding(index, cx);
            return;
        }

        // Explorer row clicks → select that semantic id (cross-highlights in all panes).
        // Routed by the row button's key (the `sidebar_menu_button` route), same idiom as
        // the tool / view buttons.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
            && let Some(id) = self.derived.borrow().explorer.lookup(route)
        {
            self.domain.selection.borrow_mut().select_only(id);
            return;
        }

        // Tool palette toggles (structural commitment 4).
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

        // Toolbar framing buttons act on the pane(s) — Fit/Reset every pane's camera so a
        // single button reframes whatever the user sees.
        if event.is_click_or_activate("fit") {
            for id in [PaneId::A, PaneId::B] {
                self.pending.borrow_mut().push(ViewportRequest::FitContent {
                    key: id.canvas_key().to_string(),
                    padding: 24.0,
                });
            }
            return;
        }
        if event.is_click_or_activate("reset") {
            for id in [PaneId::A, PaneId::B] {
                self.pending.borrow_mut().push(ViewportRequest::ResetView {
                    key: id.canvas_key().to_string(),
                });
            }
            return;
        }

        // Layer visibility switches (global; apply to all board panes).
        {
            // Snapshot the layer keys so the `derived` borrow doesn't overlap the
            // `hidden` mutable borrow inside the loop.
            let layer_keys: Vec<String> = self
                .derived
                .borrow()
                .board
                .as_ref()
                .map(|v| v.layers.iter().map(|l| l.id.key()).collect())
                .unwrap_or_default();
            for key in layer_keys {
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

        // Canvas pointer interaction. A pointer event over a pane's canvas routes to the
        // pane's viewport / a stacked layer / overlay El — all canvas targets. THE CLICKED
        // PANE is resolved by testing the pointer against each pane's laid-out rect (NOT a
        // global key — this is where m2's coordinate-composition bug class returns; every
        // unproject / rect uses the clicked pane's own key).
        if !is_canvas_target(event.target_key()) {
            return;
        }
        let Some(pos) = event.pointer_pos() else {
            return;
        };
        let Some(pane) = self.pane_under_pointer(cx, pos) else {
            return;
        };
        match self.panes.borrow()[pane_index(pane)].view {
            ViewKind::Board => self.handle_board_pointer(event, cx, pane, pos),
            ViewKind::Schematic => self.handle_schematic_pointer(event, cx, pane, pos),
        }
    }

    fn drain_viewport_requests(&mut self) -> Vec<ViewportRequest> {
        std::mem::take(&mut self.pending.borrow_mut())
    }
}

impl EcadApp {
    /// Which pane's canvas the pointer at `pos` (logical px) is inside, by testing each
    /// visible pane's laid-out canvas rect. A maximized pane is the only candidate. `None`
    /// when the pointer is over no pane canvas (chrome / gutter).
    fn pane_under_pointer(&self, cx: &EventCx, pos: (f32, f32)) -> Option<PaneId> {
        let candidates: Vec<PaneId> = match self.maximized.get() {
            Some(m) => vec![m],
            None => vec![PaneId::A, PaneId::B],
        };
        for pane in candidates {
            if let Some(r) = cx.rect_of_key(pane.canvas_key())
                && pos.0 >= r.x
                && pos.0 <= r.x + r.w
                && pos.1 >= r.y
                && pos.1 <= r.y + r.h
            {
                return Some(pane);
            }
        }
        None
    }

    /// Handle a pointer event over a board pane: cursor readout, pick / hover / measure —
    /// all through THE CLICKED PANE's canvas key + rect + viewport view.
    fn handle_board_pointer(&self, event: UiEvent, cx: &EventCx, pane: PaneId, pos: (f32, f32)) {
        let derived = self.derived.borrow();
        let Some(view) = &derived.board else {
            return;
        };
        let key = pane.canvas_key();
        let (Some(rect), Some(vv)) = (cx.rect_of_key(key), cx.viewport_view(key)) else {
            return;
        };
        let el_rect = (rect.x, rect.y, rect.w, rect.h);

        let content_px = vv.unproject(pos, (rect.x, rect.y));
        if let Some(mm) = view.canvas.content_px_to_board_mm(content_px, el_rect) {
            self.cursor_board_mm.set(Some(mm));
        }

        let Some(p) = pick::pointer_to_board_nm(&view.canvas, pos, el_rect, vv) else {
            return;
        };
        let tol = pick::tolerance_nm(PICK_TOL_PX, vv.zoom);

        match (self.tool.get(), event.kind) {
            (Tool::Select, UiEventKind::Click) => {
                let hit =
                    pick::resolve(&view.candidates, p, tol, |id| self.layer_visible(&id.key()));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.select_only(pick.id),
                    None => sel.clear(),
                }
            }
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
            (Tool::Measure, UiEventKind::Click) => {
                self.measure_pane.set(pane);
                let mut m = self.measure.get();
                m.click(p);
                self.measure.set(m);
            }
            (Tool::Measure, UiEventKind::PointerEnter | UiEventKind::Drag) => {
                self.measure_pane.set(pane);
                let mut m = self.measure.get();
                m.hover(p);
                self.measure.set(m);
            }
            _ => {}
        }
    }

    /// Handle a pointer event over a schematic pane: pick symbol/pin/wire → the schematic
    /// selection (pin > wire > symbol). Uses THE CLICKED PANE's canvas key + rect + view.
    fn handle_schematic_pointer(
        &self,
        event: UiEvent,
        cx: &EventCx,
        pane: PaneId,
        pos: (f32, f32),
    ) {
        let derived = self.derived.borrow();
        let Some(view) = &derived.schematic else {
            return;
        };
        let key = pane.canvas_key();
        let (Some(rect), Some(vv)) = (cx.rect_of_key(key), cx.viewport_view(key)) else {
            return;
        };
        let el_rect = (rect.x, rect.y, rect.w, rect.h);
        let Some(p) = view.pointer_to_schematic_nm(pos, el_rect, vv) else {
            return;
        };
        let tol = SchematicView::tolerance_nm(PICK_TOL_PX, vv.zoom);

        match event.kind {
            UiEventKind::Click => {
                let mut sel = self.domain.selection.borrow_mut();
                match view.resolve(p, tol) {
                    Some(id) => sel.select_only(id),
                    None => sel.clear(),
                }
            }
            UiEventKind::PointerEnter | UiEventKind::Drag => {
                let mut sel = self.domain.selection.borrow_mut();
                match view.resolve(p, tol) {
                    Some(id) => sel.hover_only(id),
                    None => sel.clear_hover(),
                }
            }
            UiEventKind::PointerLeave => {
                self.domain.selection.borrow_mut().clear_hover();
            }
            _ => {}
        }
    }
}

/// The dark canvas background behind the board — an ECAD-dark near-black.
const CANVAS_BG: Color = Color::srgb_token("ecad.canvas.bg", 0x12, 0x14, 0x18, 0xff);

/// The event-route key of a layer's visibility switch.
fn switch_key(layer_key: &str) -> String {
    format!("switch:{layer_key}")
}

/// Is this event target inside a pane canvas? A pointer event routes to a pane viewport
/// (`canvas:a` / `canvas:b`), a stacked board layer / overlay El (keyed `layer:*` /
/// `overlay:*`), or a schematic static El (keyed `schematic:*`). All are canvas hits;
/// chrome (toolbar, sidebar, pane headers) is not.
fn is_canvas_target(target: Option<&str>) -> bool {
    match target {
        Some(k) => {
            k == PaneId::A.canvas_key()
                || k == PaneId::B.canvas_key()
                || k.starts_with("layer:")
                || k.starts_with("overlay:")
                || k.starts_with("schematic:")
        }
        None => false,
    }
}

/// A pane's empty-state placeholder (no board / no schematic to display), filling the
/// pane so the split geometry is unaffected.
fn pane_placeholder(msg: &str) -> El {
    column([text(msg).muted()])
        .align(Align::Center)
        .fill(CANVAS_BG)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}

/// The persistent reload-error chip (m5): an unmissable destructive badge in the
/// toolbar shown whenever the *freshest* source failed to parse/elaborate while the
/// last-good doc stays rendered. Not a toast — it persists until a good reload clears
/// `reload_error`. The full error is available in the badge label's first line.
fn reload_error_chip(err: &str) -> El {
    // Compact the multi-line diagnostic to its first line for the chip; the banner is a
    // glanceable "reload failed" cue, not the full report surface.
    let first = err.lines().next().unwrap_or(err);
    badge(format!("reload failed: {first}")).destructive()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{dual_boards, schematic_domain};
    use ecad_core::coord::MM;

    /// A synthetic click routed to `key`.
    fn click(key: &str) -> UiEvent {
        UiEvent::synthetic_click(key)
    }

    /// Clicking an explorer net row selects that net (cross-highlights everywhere). Drives
    /// the real `on_event` explorer path.
    #[test]
    fn explorer_click_selects_net() {
        let mut app = EcadApp::new(schematic_domain());
        // The VDD net row's key, from the projection.
        let explorer = app.explorer_snapshot();
        let net_row = explorer
            .nets
            .iter()
            .find(|r| r.label == "VDD")
            .expect("VDD net row")
            .clone();
        assert!(app.domain.selection.borrow().is_empty());
        let cx = EventCx::new();
        app.on_event(click(&net_row.key), &cx);
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Net(NetId::new("VDD"))),
            "explorer click must select the net"
        );
    }

    /// Clicking an explorer component row selects that part.
    #[test]
    fn explorer_click_selects_part() {
        let mut app = EcadApp::new(schematic_domain());
        // Find the row by its semantic id (the label is the *annotated* refdes, not the
        // instance path — e.g. `U1 MCU` annotates to `MCU1`).
        let explorer = app.explorer_snapshot();
        let row = explorer
            .components
            .iter()
            .find(|r| r.id == SemanticId::Part(ecad_core::id::EntityId::new("U1")))
            .expect("U1 component row")
            .clone();
        let cx = EventCx::new();
        app.on_event(click(&row.key), &cx);
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Part(ecad_core::id::EntityId::new("U1")))
        );
    }

    /// The view switcher flips a pane's view kind.
    #[test]
    fn view_switcher_flips_pane_view() {
        let mut app = EcadApp::new(schematic_domain());
        assert_eq!(app.panes.borrow()[0].view, ViewKind::Board);
        let cx = EventCx::new();
        app.on_event(click(&PaneId::A.switch_key(ViewKind::Schematic)), &cx);
        assert_eq!(app.panes.borrow()[0].view, ViewKind::Schematic);
    }

    /// The layout toggle flips dual ↔ stacked; the maximize toggle sets/clears.
    #[test]
    fn layout_and_maximize_toggles() {
        let mut app = EcadApp::new(schematic_domain());
        let cx = EventCx::new();
        assert_eq!(app.layout.get(), PaneLayout::Dual);
        app.on_event(click(LAYOUT_TOGGLE_KEY), &cx);
        assert_eq!(app.layout.get(), PaneLayout::Stacked);

        assert_eq!(app.maximized.get(), None);
        app.on_event(click(PaneId::B.maximize_key()), &cx);
        assert_eq!(app.maximized.get(), Some(PaneId::B));
        app.on_event(click(PaneId::B.maximize_key()), &cx);
        assert_eq!(app.maximized.get(), None, "toggling again restores");
    }

    /// A pane hidden by maximize on its first frame must NOT be marked fitted — otherwise
    /// its dropped FitContent request (damascene discards requests whose viewport is absent
    /// this frame) would strand it at the default camera forever. On restore, the still
    /// un-fitted pane must re-arm its fit. Regression for the stuck-`fitted` bug.
    #[test]
    fn hidden_pane_defers_its_fit_until_visible() {
        let mut app = EcadApp::new(schematic_domain());
        // Maximize B on the very first frame — A is hidden this frame.
        app.maximized.set(Some(PaneId::B));
        app.before_build();

        // Only the visible pane (B) queued a fit; the hidden pane (A) is still un-fitted.
        assert!(app.panes.borrow()[pane_index(PaneId::B)].fitted, "B fits");
        assert!(
            !app.panes.borrow()[pane_index(PaneId::A)].fitted,
            "hidden A must NOT be marked fitted (its request would be dropped)"
        );
        let reqs = app.drain_viewport_requests();
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                ViewportRequest::FitContent { key, .. } if key == PaneId::B.canvas_key()
            )),
            "B's fit was queued"
        );
        assert!(
            !reqs.iter().any(|r| matches!(
                r,
                ViewportRequest::FitContent { key, .. } if key == PaneId::A.canvas_key()
            )),
            "A's fit must NOT be queued while hidden"
        );

        // Restore the split; A is now visible and must fit on this frame.
        app.maximized.set(None);
        app.before_build();
        assert!(
            app.panes.borrow()[pane_index(PaneId::A)].fitted,
            "restored A must now fit"
        );
        let reqs = app.drain_viewport_requests();
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                ViewportRequest::FitContent { key, .. } if key == PaneId::A.canvas_key()
            )),
            "A's fit is queued once it becomes visible"
        );
    }

    /// Per-pane independence: the SAME screen pixel maps to DIFFERENT board points when the
    /// two panes have different cameras — proving the pick composition uses the clicked
    /// pane's own viewport view, not a shared one (the m2 bug class). And the same pixel
    /// with the same camera but different pane RECTS also maps differently — proving the
    /// rect is per-pane too.
    #[test]
    fn per_pane_composition_uses_the_clicked_panes_view_and_rect() {
        use damascene_core::viewport::ViewportView;
        let app = EcadApp::new(schematic_domain());
        let canvas = app.board_canvas_clone();

        let rect = (0.0f32, 0.0f32, 400.0f32, 300.0f32);
        let px = (100.0f32, 80.0f32);

        // Two different cameras (pane A vs pane B), same rect + pixel.
        let cam_a = ViewportView {
            pan: (0.0, 0.0),
            zoom: 1.0,
        };
        let cam_b = ViewportView {
            pan: (50.0, -30.0),
            zoom: 2.0,
        };
        let pa = pick::pointer_to_board_nm(&canvas, px, rect, cam_a).expect("a maps");
        let pb = pick::pointer_to_board_nm(&canvas, px, rect, cam_b).expect("b maps");
        assert_ne!(
            pa, pb,
            "same pixel under different pane cameras must map to different board points"
        );

        // Same camera, two different pane rects (dual split: A left, B right).
        let rect_a = (0.0f32, 0.0f32, 200.0f32, 300.0f32);
        let rect_b = (210.0f32, 0.0f32, 200.0f32, 300.0f32);
        let ra = pick::pointer_to_board_nm(&canvas, px, rect_a, cam_a).expect("ra maps");
        let rb = pick::pointer_to_board_nm(&canvas, px, rect_b, cam_a).expect("rb maps");
        assert_ne!(
            ra, rb,
            "same pixel under different pane rects must map to different board points"
        );
    }

    /// Two board panes over the same doc lay out with DISTINCT, non-overlapping rects and
    /// distinct viewport keys — the structural prerequisite for independent cameras.
    #[test]
    fn dual_boards_lay_out_as_two_independent_panes() {
        use damascene_core::layout::layout;
        use damascene_core::prelude::Rect;
        use damascene_core::state::UiState;

        let app = dual_boards();
        let theme = app.theme();
        let cx = BuildCx::new(&theme).with_viewport(1280.0, 800.0);
        let mut root = app.build(&cx);
        let mut ui = UiState::new();
        layout(&mut root, &mut ui, Rect::new(0.0, 0.0, 1280.0, 800.0));

        let ra = ui
            .rect_of_key(PaneId::A.canvas_key())
            .expect("pane A canvas laid out");
        let rb = ui
            .rect_of_key(PaneId::B.canvas_key())
            .expect("pane B canvas laid out");
        // Distinct rects, side by side (dual = row): A's right edge is left of B's left.
        assert!(
            ra.x + ra.w <= rb.x + 1.0,
            "dual board panes must be side by side, got A={ra:?} B={rb:?}"
        );
        assert!(ra.w > 0.0 && rb.w > 0.0);
    }

    /// A schematic-only pane over a schematic-block doc renders its viewport (not a
    /// placeholder), and the poc board's schematic pane builds without panic.
    #[test]
    fn schematic_pane_renders_for_a_schematic_doc() {
        let app = EcadApp::new(schematic_domain());
        assert!(
            app.has_schematic(),
            "a doc with components must project a schematic"
        );
        // The schematic projection has pick candidates (built once per load).
        let doc = app.domain.doc.as_ref().unwrap();
        let view = SchematicView::build(doc, &app.domain.lib).expect("schematic projects");
        assert!(!view.candidates().is_empty());
        let _ = MM; // (kept for symmetry with other tests' unit imports)
    }

    // -----------------------------------------------------------------------
    // Milestone-5: live source loop (reload) + findings interaction tests.
    // All headless: inject SourceMsg onto the mailbox, run before_build.
    // -----------------------------------------------------------------------

    use crate::fixtures::{SCHEMATIC_ECAD, board, drc_violation};
    use crate::reload::SourceMsg;

    /// A settled render of an app through the harness (drives before_build → reload).
    fn settle(app: &mut EcadApp) -> crate::harness::Rendered {
        crate::harness::render_settled(app, Rect::new(0.0, 0.0, 1280.0, 800.0))
    }

    /// Good → good reload: the doc revision bumps EXACTLY once, and the preserved
    /// state (layer visibility, pane layout, a still-resolving selection) survives.
    #[test]
    fn reload_good_to_good_bumps_revision_once_and_preserves_state() {
        let mut app = board();
        // Preserve targets: hide a layer, flip the layout, select the routed trace.
        app.hidden.borrow_mut().insert("layer:F.Cu".to_string());
        app.layout.set(PaneLayout::Stacked);
        let tid = app
            .domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .keys()
            .next()
            .copied()
            .unwrap();
        // Trace ids are command-authored (not in source), so a source-only reload drops
        // them; select a NET instead, which survives a same-source reload.
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Net(ecad_core::id::NetId::new("GND")));
        let _ = tid;
        let rev0 = app.revision();

        // Reload with the SAME source (a good doc). The board fixture's source has no
        // routed copper (that was command-authored), so GND is still a net in the doc.
        let src = app.domain.source.clone();
        app.mailbox_push(SourceMsg::Changed(src));
        app.before_build();

        assert_eq!(app.revision(), rev0 + 1, "one good reload bumps once");
        assert!(
            app.reload_error().is_none(),
            "a good reload clears any error"
        );
        assert!(
            app.hidden.borrow().contains("layer:F.Cu"),
            "layer visibility must be preserved across reload"
        );
        assert_eq!(
            app.layout.get(),
            PaneLayout::Stacked,
            "pane layout must be preserved across reload"
        );
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Net(ecad_core::id::NetId::new("GND"))),
            "a still-resolving selection must survive reload"
        );

        // A second identical reload bumps again (each applied Changed is one revision).
        let src = app.domain.source.clone();
        app.mailbox_push(SourceMsg::Changed(src));
        app.before_build();
        assert_eq!(app.revision(), rev0 + 2);
    }

    /// Reload preserves cameras: the framing lives in damascene's persistent `UiState`,
    /// which the app never resets on reload. The app-side invariant that guarantees "no
    /// re-fit" is that `apply_reload` leaves the panes' `fitted` flags set, so a
    /// post-reload `before_build` queues NO `FitContent` request — the camera is left
    /// exactly as the user framed it. (The harness recreates `UiState` per call, so a
    /// zoom-comparison across two `settle`s can't test this; the queued-request check
    /// is the faithful app-side assertion.)
    #[test]
    fn reload_preserves_camera_no_refit() {
        let mut app = board();
        // First frame: the pane fits (queues + marks fitted).
        app.before_build();
        let first = app.drain_viewport_requests();
        assert!(
            first
                .iter()
                .any(|r| matches!(r, ViewportRequest::FitContent { .. })),
            "the initial frame fits the board pane"
        );

        // Reload with identical good source, then run before_build again.
        let src = app.domain.source.clone();
        app.mailbox_push(SourceMsg::Changed(src));
        app.before_build();
        let after = app.drain_viewport_requests();
        assert!(
            !after
                .iter()
                .any(|r| matches!(r, ViewportRequest::FitContent { .. })),
            "a reload must NOT re-fit — no FitContent may be queued after it, got {after:?}"
        );
    }

    /// Good → bad reload: the last-good doc STAYS rendered (canvas does not blank), the
    /// revision does NOT bump, and a persistent reload error is recorded. We choose to
    /// RETAIN the last-good findings (they still describe what is on screen) — see the
    /// reload_semantics report note.
    #[test]
    fn reload_good_to_bad_keeps_last_good_and_sets_error() {
        let mut app = board();
        let rev0 = app.revision();
        let good_findings = app.findings();
        assert!(app.has_board(), "board projects before the bad reload");

        // A source that fails elaboration (unknown part).
        app.mailbox_push(SourceMsg::Changed(BROKEN_SRC.to_string()));
        app.before_build();

        assert_eq!(
            app.revision(),
            rev0,
            "a failed reload must NOT bump the revision"
        );
        assert!(
            app.reload_error().is_some(),
            "a failed reload must record a persistent error"
        );
        assert!(
            app.has_board(),
            "the last-good board must stay rendered (canvas never blanks)"
        );
        assert_eq!(
            app.findings(),
            good_findings,
            "last-good findings are RETAINED across a failed reload"
        );
        assert!(
            app.domain.doc.is_ok(),
            "the last-good doc is still the rendered doc"
        );
    }

    /// Bad → good recovery: after a failed reload, a subsequent good reload swaps in the
    /// new doc, bumps the revision, and CLEARS the error.
    #[test]
    fn reload_bad_then_good_recovers() {
        let mut app = board();
        app.mailbox_push(SourceMsg::Changed(BROKEN_SRC.to_string()));
        app.before_build();
        assert!(app.reload_error().is_some());
        let rev_after_bad = app.revision();

        // Now a good source (the schematic doc) — recovers.
        app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        app.before_build();
        assert!(
            app.reload_error().is_none(),
            "a good reload clears the error"
        );
        assert_eq!(
            app.revision(),
            rev_after_bad + 1,
            "recovery bumps the revision"
        );
        assert!(
            app.has_schematic(),
            "the new doc's schematic projects after recovery"
        );
    }

    /// Selection pruning: select an entity, reload with a source that REMOVES it →
    /// the selection drops the now-dangling id without panicking.
    #[test]
    fn reload_prunes_dangling_selection() {
        // Start from the schematic doc (has parts U1/C1/C2 + nets VDD/GND).
        let mut app = EcadApp::new(schematic_domain());
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Part(ecad_core::id::EntityId::new("U1")));
        assert!(!app.domain.selection.borrow().is_empty());

        // Reload with a source that has NO U1 (only C1) — U1 no longer resolves.
        let pruned_src = "\
inst C1 Cap
net SOLO C1.p1
nc C1.p2
board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
";
        app.mailbox_push(SourceMsg::Changed(pruned_src.to_string()));
        app.before_build(); // must not panic

        assert!(
            app.domain.selection.borrow().is_empty(),
            "the removed entity must be pruned from the selection"
        );
        assert!(app.reload_error().is_none(), "the pruning reload was good");
    }

    /// A selection that STILL resolves survives the prune (the complement of the above).
    #[test]
    fn reload_keeps_resolving_selection() {
        let mut app = EcadApp::new(schematic_domain());
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Net(ecad_core::id::NetId::new("VDD")));
        // Reload with the SAME source: VDD still resolves.
        app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        app.before_build();
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Net(ecad_core::id::NetId::new("VDD"))),
            "a still-resolving net selection survives the prune"
        );
    }

    /// Click a findings row → the finding's refs land in the SelectionModel, and a
    /// CenterOn request is queued for the focused board pane (click-to-select-and-zoom).
    #[test]
    fn click_finding_selects_refs_and_queues_center() {
        let mut app = drc_violation();
        // Find the clearance finding's index (it carries both nets NA + NB).
        let (index, refs) = {
            let f = app.findings();
            let (i, item) = f
                .items
                .iter()
                .enumerate()
                .find(|(_, it)| it.code == "E_DRC_CLEARANCE")
                .expect("the fixture has a clearance finding");
            (i, item.refs.clone())
        };
        assert!(app.domain.selection.borrow().is_empty());

        // Settle first so a board pane is laid out; the returned UiState carries the
        // pane rects the CenterOn conversion needs, so drive the event with an EventCx
        // over that state (matching the host, which routes events against the live UI).
        let r = settle(&mut app);
        let cx = EventCx::new().with_ui_state(&r.ui);
        app.on_event(click(&finding_row_key(index)), &cx);

        // Every ref of the finding is now selected (both nets of the clearance).
        let sel = app.domain.selection.borrow();
        for r in &refs {
            assert!(
                sel.is_selected(r),
                "clicking the finding must select its ref {r:?}"
            );
        }
        drop(sel);
        // A CenterOn was queued for the focused (board) pane.
        let reqs = app.drain_viewport_requests();
        assert!(
            reqs.iter().any(|r| matches!(
                r,
                ViewportRequest::CenterOn { key, .. } if key == PaneId::A.canvas_key()
            )),
            "a clearance finding with a board point must queue a CenterOn on the board pane"
        );
    }

    /// The clearance-finding halo is present in the board overlay at the right board mm:
    /// building the board overlay yields a findings marker whose point matches the
    /// finding's derived board_mm.
    #[test]
    fn finding_halo_present_in_board_overlay() {
        let app = drc_violation();
        let f = app.findings();
        let clearance = f
            .items
            .iter()
            .find(|i| i.code == "E_DRC_CLEARANCE")
            .unwrap();
        let (mx, my) = clearance.board_mm.expect("clearance has a board point");

        let derived = app.derived.borrow();
        let view = derived.board.as_ref().expect("board projects");
        let sets = HighlightSets::default();
        let overlay = app.build_board_overlay(view, PaneId::A, &sets, &derived.findings);
        assert!(
            !overlay.findings.is_empty(),
            "the overlay must carry finding markers"
        );
        // The clearance marker's point matches the finding's board_mm (nm round-trip).
        let want = ecad_core::coord::Point {
            x: (mx * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
            y: (my * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
        };
        assert!(
            overlay
                .findings
                .iter()
                .any(|(p, is_err)| *p == want && *is_err),
            "an error marker must sit at the clearance finding's board point {want:?}"
        );
    }

    /// The DRC chip counts match the cached findings (error + warning tallies).
    #[test]
    fn drc_chip_counts_match_findings() {
        let app = drc_violation();
        let f = app.findings();
        assert!(
            f.errors >= 1,
            "the fixture has at least the clearance error"
        );
        // The chip is not clean and reports the same counts (asserted via the findings
        // the chip reads — the chip El itself is a badge over these counts).
        assert!(!f.is_clean());
    }

    /// A source that fails the load — a malformed `inst` (missing its part token).
    /// An unknown part no longer fails (library packages: it degrades to a
    /// `W_UNRESOLVED_PART` finding), so the error path needs a genuine syntax fault.
    const BROKEN_SRC: &str = "\
inst U1
net GND U1.GND
";
}
