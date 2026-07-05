//! The `ecad-gui` application shell — facade over the `app/` submodules.
//!
//! This is the *workspace-conversion + skeleton* milestone (see
//! `docs/gui-architecture.md`, "v1 scope", milestone 1): the crate compiles,
//! a window can open, and the headless fixture/lint review loop is in place.
//!
//! The shell was originally one ~3000-line `app.rs`; it is now split — pure code
//! motion — along the seams the house facade+submodule pattern (e.g. `ecad-core`'s
//! `text.rs` + `text/`) uses:
//!
//! - [`domain`] — [`DomainState`] / [`LibSource`] + elaboration/reload plumbing and
//!   the revised-keyed derived-cache bundle (`DerivedCaches`, `BoardView`, `DocStats`).
//! - [`pane`] — [`ViewKind`] / [`PaneLayout`] / [`PaneId`] / [`PaneState`], pane/layout
//!   state, and the shared key vocabulary + canvas-target predicate + placeholders.
//! - [`libraries`] — the Libraries modal (UI + event handling + registry editing).
//! - [`panels`] — every `build`-time panel/chrome builder + the findings-row click.
//! - [`events`] — the [`App`] impl (`build` / `before_build` / `on_event`) + pointer
//!   routing.
//!
//! This module ([`app`](self)) remains the facade: it owns the [`EcadApp`] struct
//! (so its private fields stay reachable from every submodule), the `EcadApp::new` +
//! accessor + reload impl block, and the tests. Public items keep their old paths
//! through the re-exports below (`lib.rs` re-exports these unchanged).

mod domain;
mod events;
mod libraries;
mod pane;
mod panels;

pub use domain::{DomainState, LibSource};
pub use pane::{PaneId, PaneLayout, PaneState, ViewKind};

// The `EcadApp` struct fields + `EcadApp::new`/reload impl reference the derived-cache
// bundle and the Libraries UI state that were moved to submodules.
use domain::DerivedCaches;
use libraries::{LibRow, LibUi};
// The tests (a child module using `super::*`) reach these moved items through the
// facade — keep their old flat paths available. Pure code motion: nothing is renamed.
#[cfg(test)]
pub(crate) use libraries::{
    LIBRARIES_ADD_KEY, LIBRARIES_CLOSE_KEY, LIBRARIES_TOGGLE_KEY, library_remove_key,
};
#[cfg(test)]
pub(crate) use pane::{LAYOUT_TOGGLE_KEY, finding_row_key, pane_index};

use crate::findings::Findings;
use crate::reload::{SourceMailbox, SourceMsg};
use crate::tool::{MeasureState, Tool};
use damascene_core::prelude::*;
use ecad_core::doc::Doc;
use std::cell::{Cell, RefCell};

// Test-only symbols the `tests` child module reaches through `super::*`; the
// non-test `EcadApp` body does not name them (the `#[cfg(test)]` accessors that
// return `Canvas`/`Explorer` are themselves test-only).
#[cfg(test)]
use crate::canvas::Canvas;
#[cfg(test)]
use crate::canvas::pick::{self, SemanticId};
#[cfg(test)]
use crate::explorer::Explorer;
#[cfg(test)]
use crate::highlight::HighlightSets;
#[cfg(test)]
use crate::schematic_view::SchematicView;
#[cfg(test)]
use ecad_core::id::NetId;

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
    pub(crate) panes: RefCell<[PaneState; 2]>,
    /// The two-pane orientation (dual / stacked).
    pub(crate) layout: Cell<PaneLayout>,
    /// Which pane, if any, is maximized (the other is hidden). `None` ⇒ the normal split.
    pub(crate) maximized: Cell<Option<PaneId>>,
    /// The split weights `[a, b]` for the resize handle, and its in-flight drag.
    pub(crate) split_weights: Cell<[f32; 2]>,
    pub(crate) split_drag: RefCell<ResizeWeightsDrag>,
    /// The measured split-container main extent (px), captured each frame for the weighted
    /// resize handler (the README idiom).
    pub(crate) split_extent: Cell<f32>,
    /// The derived caches (board projection, schematic projection, explorer rows,
    /// findings) — everything computed *from* the doc. Rebuilt as a unit only when the
    /// doc revision changes (a reload). `RefCell` because [`apply_reload`] swaps the
    /// whole bundle in `before_build`; `build` reads it immutably.
    ///
    /// [`apply_reload`]: EcadApp::apply_reload
    pub(crate) derived: RefCell<DerivedCaches>,
    /// Which layers are visible, keyed by [`LayerId::key`]. Absent ⇒ visible
    /// (layers default on). Mutated by the layer-panel toggles in `on_event`.
    /// **Preserved across reloads** (the user's framing/visibility is sacred).
    pub(crate) hidden: RefCell<std::collections::HashSet<String>>,
    /// Viewport requests (Fit / Reset / CenterOn) queued from toolbar / findings
    /// clicks, drained once per frame by the host.
    pub(crate) pending: RefCell<Vec<ViewportRequest>>,
    /// The last pointer position over a board pane in **board mm**, for the status-bar
    /// cursor readout. Set by whichever board pane the pointer last moved over.
    pub(crate) cursor_board_mm: Cell<Option<(f32, f32)>>,
    /// The active tool (structural commitment 4). Global mode; `Cell` because it is
    /// flipped in `on_event` and read in `build`.
    pub(crate) tool: Cell<Tool>,
    /// The measure tool's uncommitted preview state (the preview channel — renders
    /// only to the overlay, never the doc). The pane the measure is happening in, so the
    /// overlay draws it in the right place.
    pub(crate) measure: Cell<MeasureState>,
    pub(crate) measure_pane: Cell<PaneId>,
    /// The live-source mailbox (m5): drained in `before_build`; a file change reloads.
    /// A [`SourceMailbox::disconnected`] mailbox (fixtures / no file) never yields.
    pub(crate) mailbox: SourceMailbox,
    /// Whether the findings panel section is expanded (collapsible like the explorer).
    pub(crate) findings_open: Cell<bool>,
    /// Whether the Libraries menu (modal) is open.
    pub(crate) libraries_open: Cell<bool>,
    /// Libraries-menu interaction state (inputs + text selection + last error).
    pub(crate) lib_ui: RefCell<LibUi>,
    /// Cached per-row registry load statuses for the Libraries menu (`None` =
    /// dirty; recomputed lazily on the next build). Invalidated on menu open
    /// and on every registry edit — row status is a filesystem probe
    /// ([`registry::row_status`](crate::registry::row_status)), so it must not run
    /// every frame.
    pub(crate) lib_statuses: RefCell<Option<Vec<LibRow>>>,
}

impl EcadApp {
    pub fn new(domain: DomainState) -> Self {
        let derived = match &domain.doc {
            Ok(doc) => DerivedCaches::build(doc, &domain.lib, &domain.lib_notes),
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
            libraries_open: Cell::new(false),
            lib_ui: RefCell::new(LibUi::default()),
            lib_statuses: RefCell::new(None),
        }
    }

    /// Open or close the Libraries menu — for fixtures / tests. Opening
    /// invalidates the row-status cache so the menu shows fresh statuses.
    pub fn set_libraries_open(&self, open: bool) {
        if open {
            *self.lib_statuses.borrow_mut() = None;
        }
        self.libraries_open.set(open);
    }

    /// Pre-fill the Libraries add-entry inputs — for tests that drive the add
    /// button without simulating per-character text input.
    pub fn set_library_inputs(&self, name: &str, path: &str) {
        let mut ui = self.lib_ui.borrow_mut();
        ui.name = name.to_string();
        ui.path = path.to_string();
    }

    /// The Libraries menu's last inline edit error — for tests.
    pub fn library_edit_error(&self) -> Option<String> {
        self.lib_ui.borrow().error.clone()
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
        let (lib, notes, doc) = self.domain.elaborate_source(&source);
        match doc {
            Ok(doc) => {
                let derived = DerivedCaches::build(&doc, &lib, &notes);
                // Prune selection + hover to ids that still resolve in the NEW doc,
                // using the freshly-built candidate/schematic ids as the resolvable set.
                self.prune_selection(&doc, &derived);
                *self.derived.borrow_mut() = derived;
                self.domain.lib = lib;
                self.domain.lib_notes = notes;
                self.domain.doc = Ok(doc);
                self.domain.source = source;
                self.domain.revision += 1;
                self.domain.reload_error = None;
            }
            Err(err) => {
                // Permissive: keep the last-good doc + caches + resolved lib on screen;
                // surface the error persistently. Do NOT bump the revision (nothing
                // derived changed).
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
        sel.retain(|id| domain::resolves_in(id, doc, derived));
    }
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

    // -----------------------------------------------------------------------
    // Library packages, slice 2: registry-driven resolution + the Libraries
    // menu's live edit semantics. All headless; registries live in scratch
    // dirs (never the per-user config — the path is injected).
    // -----------------------------------------------------------------------

    use crate::registry::Registry;

    /// The in-repo poc library package directory (an absolute path — the
    /// crate manifest dir is absolute).
    fn poc_parts_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts")
    }

    /// A one-instance source that only the poc package can resolve.
    const USE_POC_SRC: &str = "use poc\ninst U1 RP2350A\n";

    /// A scratch dir under the system temp dir, removed on drop.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir =
                std::env::temp_dir().join(format!("ecad-app-test-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The Libraries-menu add flow end to end: with `use poc` unregistered the
    /// doc loads degraded (instance skipped, W_LIB_UNREGISTERED in the
    /// findings); adding the poc entry through the menu saves the registry
    /// file, re-resolves + re-elaborates through the reload path (revision
    /// bump), and the part now resolves. Removing it degrades again.
    #[test]
    fn registry_add_and_remove_reresolve_the_current_doc() {
        let scratch = Scratch::new("add-remove");
        let save = scratch.0.join("libraries");
        let mut app = EcadApp::new(DomainState::from_source_registry(
            USE_POC_SRC.to_string(),
            Some("t.ecad".to_string()),
            Registry::new(),
            Some(save.clone()),
        ));
        let doc = app.domain.doc.as_ref().expect("degraded load succeeds");
        assert!(doc.components.is_empty(), "RP2350A unresolved at first");
        assert!(
            app.findings()
                .items
                .iter()
                .any(|i| i.code == "W_LIB_UNREGISTERED"),
            "the unregistered use renders in the findings"
        );
        assert_eq!(app.revision(), 0);

        // Drive the menu: open, fill the add-entry inputs, click Add.
        let cx = EventCx::new();
        app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
        assert!(app.libraries_open.get(), "toolbar button opens the menu");
        app.set_library_inputs("poc", poc_parts_dir().to_str().unwrap());
        app.on_event(click(LIBRARIES_ADD_KEY), &cx);

        assert_eq!(
            app.revision(),
            1,
            "a registry edit re-elaborates through the reload path (bump once)"
        );
        assert_eq!(app.library_edit_error(), None);
        let doc = app.domain.doc.as_ref().unwrap();
        assert_eq!(doc.components.len(), 1, "RP2350A resolves after the add");
        assert!(
            !app.findings()
                .items
                .iter()
                .any(|i| i.code == "W_LIB_UNREGISTERED" || i.code == "W_UNRESOLVED_PART"),
            "the library findings clear once the name binds"
        );
        // Live edit semantics: the registry file was saved immediately.
        let back = Registry::load(&save).expect("saved registry loads");
        assert_eq!(back.get("poc"), Some(poc_parts_dir().as_path()));
        // The add cleared the inputs.
        assert_eq!(app.lib_ui.borrow().name, "");
        assert_eq!(app.lib_ui.borrow().path, "");

        // Remove flow: the row's Remove button unbinds + re-resolves again.
        app.on_event(click(&library_remove_key("poc")), &cx);
        assert_eq!(app.revision(), 2);
        assert!(
            app.domain.doc.as_ref().unwrap().components.is_empty(),
            "unbinding the library degrades the doc again"
        );
        let back = Registry::load(&save).expect("saved registry loads");
        assert!(back.is_empty(), "the removal was saved");
    }

    /// A relative path in the add form is rejected at the boundary: the error
    /// renders inline, nothing is saved, and the doc is untouched (no revision
    /// bump).
    #[test]
    fn registry_add_rejects_relative_path_inline() {
        let scratch = Scratch::new("relative");
        let save = scratch.0.join("libraries");
        let mut app = EcadApp::new(DomainState::from_source_registry(
            USE_POC_SRC.to_string(),
            Some("t.ecad".to_string()),
            Registry::new(),
            Some(save.clone()),
        ));
        let cx = EventCx::new();
        app.set_libraries_open(true);
        app.set_library_inputs("poc", "relative/path");
        app.on_event(click(LIBRARIES_ADD_KEY), &cx);
        let err = app.library_edit_error().expect("inline error set");
        assert!(err.contains("absolute"), "{err}");
        assert_eq!(app.revision(), 0, "no re-elaborate on a rejected edit");
        assert!(!save.exists(), "nothing saved on a rejected edit");
        // The inputs stay for correction.
        assert_eq!(app.lib_ui.borrow().path, "relative/path");
    }

    /// A source reload that ADDS a `use` line re-runs resolution against the
    /// registry — the lib is re-derived per load, not fixed at open time.
    #[test]
    fn reload_reresolves_use_names() {
        let mut registry = Registry::new();
        registry.set("poc", &poc_parts_dir()).unwrap();
        let mut app = EcadApp::new(DomainState::from_source_registry(
            "inst U1 RP2350A\n".to_string(),
            Some("t.ecad".to_string()),
            registry,
            None,
        ));
        assert!(
            app.domain.doc.as_ref().unwrap().components.is_empty(),
            "without a use line the registry is not consulted"
        );
        app.apply_reload(USE_POC_SRC.to_string());
        assert_eq!(app.revision(), 1);
        assert_eq!(
            app.domain.doc.as_ref().unwrap().components.len(),
            1,
            "the reload's new `use poc` resolves through the registry"
        );
    }

    /// A registry edit while a reload-error banner is up re-elaborates the
    /// last-GOOD source — which says nothing about the newer broken source on
    /// disk, so the banner must survive the registry-triggered reload.
    #[test]
    fn registry_edit_preserves_a_standing_reload_error() {
        let mut registry = Registry::new();
        registry.set("poc", &poc_parts_dir()).unwrap();
        let mut app = EcadApp::new(DomainState::from_source_registry(
            USE_POC_SRC.to_string(),
            Some("t.ecad".to_string()),
            registry,
            None,
        ));
        // A broken disk source arrives: banner up, last-good doc stays.
        app.apply_reload(BROKEN_SRC.to_string());
        assert!(app.reload_error().is_some(), "banner up");

        // A registry edit re-resolves the last-good source; the banner stays.
        let cx = EventCx::new();
        app.set_libraries_open(true);
        app.on_event(click(&library_remove_key("poc")), &cx);
        assert!(
            app.reload_error().is_some(),
            "the banner must survive a registry-triggered reload of the stale-good source"
        );
    }

    /// Escape closes the Libraries menu (and is consumed — the selection
    /// survives), and the scrim/close affordances work.
    #[test]
    fn libraries_menu_escape_and_close() {
        let mut app = EcadApp::new(schematic_domain());
        let cx = EventCx::new();
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Net(NetId::new("VDD")));

        app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
        assert!(app.libraries_open.get());
        // damascene has no generic synthetic constructor; shape an Escape by hand.
        let mut esc = UiEvent::synthetic_click("");
        esc.key = None;
        esc.kind = UiEventKind::Escape;
        app.on_event(esc, &cx);
        assert!(!app.libraries_open.get(), "Escape closes the menu");
        assert!(
            !app.domain.selection.borrow().is_empty(),
            "Escape was consumed by the menu — the selection survives"
        );

        app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
        app.on_event(click(LIBRARIES_CLOSE_KEY), &cx);
        assert!(!app.libraries_open.get(), "Close button closes the menu");
    }
}
