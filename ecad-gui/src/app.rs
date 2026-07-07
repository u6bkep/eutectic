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
//! - [`editing`] — the m6 editing engine: reload / undo / redo source transitions,
//!   the command-commit path, the save model, and the route-commit lowering.
//! - [`events`] — the [`App`] impl (`build` / `before_build` / `on_event`) + pointer
//!   routing.
//!
//! The `build`-time panel/chrome builders that used to live in `app/panels.rs`
//! moved to their own top-level regions (gui-module-split): `crate::chrome`
//! (toolbar + status bar), `crate::panes` (the pane tree + overlays), and
//! `crate::panels` (the right-sidebar panels + the findings-row click).
//!
//! This module ([`app`](self)) remains the facade: it owns the [`EcadApp`] struct
//! (so its private fields stay reachable from every submodule) and the
//! `EcadApp::new` + accessor impl block; the tests live in `app/tests/`, split by
//! concern. Public items keep their old paths through the re-exports below
//! (`lib.rs` re-exports these unchanged).

pub(crate) mod domain;
mod editing;
mod events;
pub(crate) mod libraries;
pub(crate) mod pane;
#[cfg(test)]
mod tests;

pub use domain::{DomainState, LibSource};
pub use pane::{PaneId, PaneLayout, PaneState, ViewKind};

// The `EcadApp` struct fields + `EcadApp::new`/reload impl reference the derived-cache
// bundle and the Libraries UI state that were moved to submodules.
use domain::DerivedCaches;
use libraries::{LibRow, LibUi};
use pane::{SectionOpen, SidebarSection};
// The tests (a child module using `super::*`) reach these moved items through the
// facade — keep their old flat paths available. Pure code motion: nothing is renamed.
#[cfg(test)]
pub(crate) use libraries::{
    LIBRARIES_ADD_KEY, LIBRARIES_CLOSE_KEY, LIBRARIES_TOGGLE_KEY, library_remove_key,
};
#[cfg(test)]
pub(crate) use pane::{
    CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, LAYOUT_TOGGLE_KEY, REDO_KEY, SAVE_KEY, UNDO_KEY,
    finding_row_key, pane_index,
};

use crate::canvas::GridCache;
use crate::findings::Findings;
use crate::reload::{SourceMailbox, SourceMsg};
use crate::tool::{CameraPanState, DragState, MeasureState, RouteState, Tool, TraceDragState};
use damascene_core::prelude::*;
use ecad_core::coord::Nm;
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
    /// The active tool per **view kind** (structural commitment 4, revised: Blender
    /// semantics — all board panes share one slot, all schematic panes another; the
    /// live tool is the focused pane's kind's entry). A kind with no entry defaults
    /// to [`Tool::Select`], so future view kinds need no registration. View-state
    /// territory (not [`DomainState`]): a future popped-out window carries it.
    pub(crate) tools: RefCell<std::collections::BTreeMap<ViewKind, Tool>>,
    /// The focused pane — the pane the pointer last touched (Blender hover-focus)
    /// or whose strip was last clicked. Its view kind's tool slot is the *live*
    /// tool ([`live_tool`](Self::live_tool)); the status bar reads it out.
    pub(crate) focused_pane: Cell<PaneId>,
    /// The measure tool's uncommitted preview state (the preview channel — renders
    /// only to the overlay, never the doc). The pane the measure is happening in, so the
    /// overlay draws it in the right place.
    pub(crate) measure: Cell<MeasureState>,
    pub(crate) measure_pane: Cell<PaneId>,
    /// The live-source mailbox (m5): drained in `before_build`; a file change reloads.
    /// A [`SourceMailbox::disconnected`] mailbox (fixtures / no file) never yields.
    pub(crate) mailbox: SourceMailbox,
    /// The right-sidebar accordion's per-section expanded state (all four headers
    /// always render; this governs which bodies are open). Default: Properties +
    /// Layers open.
    pub(crate) sections: Cell<SectionOpen>,
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
    /// The Select tool's in-flight component drag (m6) — the uncommitted preview
    /// state between pointer-down on a component and pointer-up (commit) / Esc
    /// (cancel). `RefCell` per the interior-mutability pattern: updated in
    /// `on_event`, read by the overlay builder in `build`.
    pub(crate) drag: RefCell<Option<DragState>>,
    /// Set when a moved drag just committed on `PointerUp`, so the trailing
    /// `Click` (damascene fires PointerUp then Click for an up on the pressed
    /// node) does not re-run click-select over the drop point. Cleared by the
    /// next `PointerDown` (so an eaten Click can never go stale) and consumed by
    /// the Click handler.
    pub(crate) suppress_click: Cell<bool>,
    /// The Route tool's pending route (m6 slice B) — the uncommitted preview
    /// state between the start click and the commit-on-pin click / Esc. Renders
    /// only to the overlay; commit lowers to AddTrace/AddVia through
    /// [`commit_route`](Self::commit_route).
    pub(crate) route: RefCell<Option<RouteState>>,
    /// The Select tool's in-flight trace-vertex refinement drag (m6 slice B):
    /// between pointer-down on a selected trace's vertex/segment and pointer-up
    /// (commit as Remove+Add under the same id) / Esc (cancel).
    pub(crate) trace_drag: RefCell<Option<TraceDragState>>,
    /// The Select tool's in-flight **camera pan**: armed on pointer-down over a
    /// board pane when neither a component drag nor a trace-vertex drag armed
    /// (pour / trace / empty board / grid furniture). Per drag event the camera
    /// pan is realised as a `ViewportRequest::CenterOn`; pointer-up disarms
    /// (eating the trailing Click iff the gesture moved). See
    /// [`CameraPanState`] for why the app owns this gesture at all.
    pub(crate) camera_pan: RefCell<Option<CameraPanState>>,
    /// The per-pane viewport-anchored grid-window caches (`[A, B]`): the built
    /// dot-lattice asset plus the (pitch, viewBox, index-window) key it was
    /// built from. A build is a cache hit — an asset clone — while the pane's
    /// visible window stays inside the built window at the same pitch; only
    /// escaping it (a > half-viewport pan, a pitch-bucket change, a reload)
    /// re-tessellates. See [`crate::canvas::Canvas::grid_el`].
    pub(crate) grid_caches: RefCell<[Option<GridCache>; 2]>,
    /// The active routing layer, as a copper slab name. `None` = the default
    /// (top copper). Set from the layer panel's set-active affordance; switching
    /// it while a route is pending drops a via (ladder level 1).
    pub(crate) active_layer: RefCell<Option<String>>,
    /// The open top-level menu-bar menu, by its lowercase value token (`"file"`,
    /// `"edit"`, …), or `None` when every menu is closed. The app-owned open-menu
    /// slot damascene's [`menubar`](damascene_core::menubar) folds trigger clicks
    /// into (`RefCell` for the interior-mutability pattern: flipped in `on_event`,
    /// read in `build`). Clicking outside (the popover scrim) or invoking any row
    /// closes it.
    pub(crate) open_menu: RefCell<Option<String>>,
}

/// The trace / via defaults the Route tool commits with, sourced from the same
/// [`DesignRules::default()`](ecad_core::route::DesignRules) the DRC query and the
/// autorouter consume: trace width = `min_trace_width` (0.15 mm), via drill =
/// `min_trace_width`, via pad = `2 * min_trace_width` — the exact `width` /
/// `via_drill` / `via_pad` derivation `ecad_core::autoroute` applies. Returned as
/// `(width, via_drill, via_pad)`.
pub(crate) fn route_defaults() -> (Nm, Nm, Nm) {
    let rules = ecad_core::route::DesignRules::default();
    (
        rules.min_trace_width,
        rules.min_trace_width,
        2 * rules.min_trace_width,
    )
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
            tools: RefCell::new(std::collections::BTreeMap::new()),
            focused_pane: Cell::new(PaneId::A),
            measure: Cell::new(MeasureState::default()),
            measure_pane: Cell::new(PaneId::A),
            mailbox: SourceMailbox::disconnected(),
            sections: Cell::new(SectionOpen::default()),
            libraries_open: Cell::new(false),
            lib_ui: RefCell::new(LibUi::default()),
            lib_statuses: RefCell::new(None),
            drag: RefCell::new(None),
            suppress_click: Cell::new(false),
            route: RefCell::new(None),
            trace_drag: RefCell::new(None),
            camera_pan: RefCell::new(None),
            grid_caches: RefCell::new([None, None]),
            active_layer: RefCell::new(None),
            open_menu: RefCell::new(None),
        }
    }

    /// Open a top-level menu-bar menu by its lowercase value token (`"file"`,
    /// `"edit"`, …), or close all with `None` — for fixtures / tests that render a
    /// menu-expanded scene without driving the trigger click. The interactive path
    /// folds trigger clicks into this slot in `on_event`.
    pub fn set_open_menu(&self, menu: Option<&str>) {
        *self.open_menu.borrow_mut() = menu.map(str::to_string);
    }

    /// Open or close the Libraries menu — for fixtures / tests. Opening
    /// invalidates the row-status cache so the menu shows fresh statuses.
    pub fn set_libraries_open(&self, open: bool) {
        if open {
            *self.lib_statuses.borrow_mut() = None;
        }
        self.libraries_open.set(open);
    }

    /// Is the given sidebar accordion section currently expanded?
    pub(crate) fn section_open(&self, section: SidebarSection) -> bool {
        self.sections.get().is_open(section)
    }

    /// Toggle a sidebar accordion section (a header click, or a findings chip).
    pub(crate) fn toggle_section(&self, section: SidebarSection) {
        self.sections.set(self.sections.get().toggled(section));
    }

    /// Set a sidebar accordion section's expanded state — for fixtures / tests.
    pub(crate) fn set_section_open(&self, section: SidebarSection, open: bool) {
        self.sections.set(self.sections.get().with(section, open));
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

    /// The active tool of view kind `kind` (per-view-kind tool memory, revised
    /// structural commitment 4). A kind with no entry yet defaults to
    /// [`Tool::Select`].
    pub fn tool_for(&self, kind: ViewKind) -> Tool {
        self.tools.borrow().get(&kind).copied().unwrap_or_default()
    }

    /// Set view kind `kind`'s active tool — the strip clicks land here, and
    /// fixtures / tests use it directly. Changing the **board** kind's tool
    /// cancels every in-flight board preview (measure / pending route / vertex
    /// drag) — a preview never outlives its tool. (All previews today are board
    /// previews; a schematic-kind switch has nothing to cancel.)
    pub fn set_tool(&self, kind: ViewKind, tool: Tool) {
        if kind == ViewKind::Board && self.tool_for(kind) != tool {
            self.measure.set(MeasureState::default());
            *self.route.borrow_mut() = None;
            *self.trace_drag.borrow_mut() = None;
            *self.camera_pan.borrow_mut() = None;
        }
        self.tools.borrow_mut().insert(kind, tool);
    }

    /// The **live** tool: the focused pane's view kind's tool slot (the tool the
    /// status bar reads out; moving focus between panes of different kinds swaps
    /// it without touching either kind's memory).
    pub fn live_tool(&self) -> Tool {
        let kind = self.panes.borrow()[pane::pane_index(self.focused_pane.get())].view;
        self.tool_for(kind)
    }

    /// Set the measure preview state — for fixtures / tests that render a
    /// measure-in-progress scene without driving live pointer events.
    pub fn set_measure(&self, m: MeasureState) {
        self.measure.set(m);
    }
}
