//! The `eutectic-gui` application shell — facade over the `app/` submodules.
//!
//! This is the *workspace-conversion + skeleton* milestone (see
//! `docs/gui-architecture.md`, "v1 scope", milestone 1): the crate compiles,
//! a window can open, and the headless fixture/lint review loop is in place.
//!
//! The shell was originally one ~3000-line `app.rs`; it is now split — pure code
//! motion — along the seams the house facade+submodule pattern (e.g. `eutectic-core`'s
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
//! This module ([`app`](self)) remains the facade: it owns the [`EutecticApp`] struct
//! (so its private fields stay reachable from every submodule) and the
//! `EutecticApp::new` + accessor impl block; the tests live in `app/tests/`, split by
//! concern. Public items keep their old paths through the re-exports below
//! (`lib.rs` re-exports these unchanged).

pub(crate) mod autoroute;
pub(crate) mod canvas_pane;
pub(crate) mod domain;
mod editing;
mod events;
pub(crate) mod libraries;
pub(crate) mod open;
pub(crate) mod pane;
#[cfg(test)]
mod tests;

pub use domain::{DomainState, LibSource};
pub use pane::{PaneId, PaneLayout, PaneState, ViewKind};

// The `EutecticApp` struct fields + `EutecticApp::new`/reload impl reference the derived-cache
// bundle and the Libraries UI state that were moved to submodules.
use crate::palette::PaletteUi;
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

use crate::findings::Findings;
use crate::reload::{SourceMailbox, SourceMsg};
use crate::tool::{CameraPanState, DragState, MeasureState, RouteState, Tool, TraceDragState};
use canvas_pane::{GpuState, PaneCam, PaneRect, RawInput};
use damascene_core::prelude::*;
use eutectic_core::coord::Nm;
use std::cell::{Cell, RefCell};

/// App-wide display units for chrome readouts. Document geometry remains in
/// integer nanometres; this setting changes presentation only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayUnits {
    /// Millimetres (the native/default ECAD display unit).
    #[default]
    Millimetres,
    /// Decimal inches.
    Inches,
}

impl DisplayUnits {
    /// Short label used by the toolbar, menus, and status bar.
    pub fn label(self) -> &'static str {
        match self {
            DisplayUnits::Millimetres => "mm",
            DisplayUnits::Inches => "in",
        }
    }

    /// Convert a millimetre value for display in this unit.
    pub fn from_mm(self, value: f64) -> f64 {
        match self {
            DisplayUnits::Millimetres => value,
            DisplayUnits::Inches => value / 25.4,
        }
    }

    fn toggled(self) -> DisplayUnits {
        match self {
            DisplayUnits::Millimetres => DisplayUnits::Inches,
            DisplayUnits::Inches => DisplayUnits::Millimetres,
        }
    }
}

/// Procedural board-grid presentation. Dots remain the default so existing
/// GPU output is byte-for-byte unchanged until the user toggles the setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GridStyle {
    /// Minor/major lattice points.
    #[default]
    Dots,
    /// Hairline minor/major lattice lines.
    Lines,
}

impl GridStyle {
    /// Lowercase value shown at the right edge of the View menu row.
    pub fn label(self) -> &'static str {
        match self {
            GridStyle::Dots => "dots",
            GridStyle::Lines => "lines",
        }
    }

    fn toggled(self) -> GridStyle {
        match self {
            GridStyle::Dots => GridStyle::Lines,
            GridStyle::Lines => GridStyle::Dots,
        }
    }
}

// Test-only symbols the `tests` child module reaches through `super::*`; the
// non-test `EutecticApp` body does not name them (the `#[cfg(test)]` accessors that
// return `Explorer` etc. are themselves test-only).
#[cfg(test)]
use crate::explorer::Explorer;
#[cfg(test)]
use crate::pick::SemanticId;
#[cfg(test)]
use eutectic_core::id::NetId;

/// The application shell: a [`DomainState`], the pane/layout state, and the
/// board-view state (per-pane owned-canvas cameras + per-layer visibility +
/// live interaction state).
///
/// Implements [`App`] as a pure projection from state to a widget tree — the
/// shape `gui-architecture.md` calls out as matching the engine's source →
/// derived-views model. The derived render inputs are the structural
/// commitment: built **once** per doc revision (the renderer scene + pick
/// candidates in [`DerivedCaches`]) and held here, so `build` never
/// re-tessellates and the per-frame GPU work is governed by the damage
/// contract ([`canvas_pane`]). Interaction state (`RefCell`/`Cell` per the
/// damascene interior-mutability pattern) is written in `on_event` /
/// `before_build` and read in `build`.
///
/// [`new`]: EutecticApp::new
pub struct EutecticApp {
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
    /// [`apply_reload`]: EutecticApp::apply_reload
    pub(crate) derived: RefCell<DerivedCaches>,
    /// Which layers are visible, keyed by [`LayerId::key`]. Absent ⇒ visible
    /// (layers default on). Mutated by the layer-panel toggles in `on_event`.
    /// **Preserved across reloads** (the user's framing/visibility is sacred).
    pub(crate) hidden: RefCell<std::collections::HashSet<String>>,
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
    /// Native-open results arrive here from a background dialog thread. Tests
    /// inject picks directly and never launch native chrome.
    pub(crate) open_mailbox: crate::open_dialog::OpenMailbox,
    pub(crate) open_dialog_launcher: std::sync::Arc<dyn crate::open_dialog::OpenDialogLauncher>,
    pub(crate) background_wakeup: crate::open_dialog::WakeFn,
    pub(crate) open_dialog_busy: Cell<bool>,
    pub(crate) open_discard_approval: RefCell<Option<open::DiscardApproval>>,
    pub(crate) next_open_request_id: Cell<u64>,
    pub(crate) active_dialog_request_id: Cell<Option<u64>>,
    pub(crate) pending_open: RefCell<Option<open::PendingOpen>>,
    /// File ▸ Open Recent state and its independently persisted XDG path.
    pub(crate) recents: RefCell<crate::recents::RecentFiles>,
    pub(crate) recents_path: Option<std::path::PathBuf>,
    pub(crate) recent_open: Cell<bool>,
    /// Switches the live-source watcher to a newly opened document.
    pub(crate) watch_path_tx: Option<std::sync::mpsc::Sender<std::path::PathBuf>>,
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
    /// (pour / trace / empty board / bare canvas). Per drag event the pane's
    /// app-owned camera snaps to `start_center − Δpx/zoom`; pointer-up disarms
    /// (eating the trailing Click iff the gesture moved).
    pub(crate) camera_pan: RefCell<Option<CameraPanState>>,
    /// The per-pane app-owned cameras (`[A, B]`) — the owned-canvas camera
    /// state (glide filter + pending Fit/Reset request), one per pane and
    /// shared across view kinds (a view switch resets the pane's `fitted`
    /// flag, so the incoming view re-frames).
    pub(crate) pane_cams: RefCell<[PaneCam; 2]>,
    /// The GPU bundle (renderer + pane textures + scene buffers), created by
    /// the host's `gpu_setup` seam. `None` on the CPU harness path — the
    /// board pane's `build` never requires a device.
    pub(crate) gpu: RefCell<Option<GpuState>>,
    /// Per-pane canvas rects (window-logical px), captured each `build` from
    /// the last layout for the paint pass + raw-event pane resolution.
    pub(crate) pane_px: Cell<[Option<PaneRect>; 2]>,
    /// Per-pane floating tool-strip rects, captured each `build` — free
    /// hover treats a pointer over the strip as chrome, not canvas.
    pub(crate) strip_px: Cell<[Option<PaneRect>; 2]>,
    /// Per-pane crosshair cursor (pane-local logical px), written by the raw
    /// pointer tap; the renderer draws the §4 crosshair from it.
    pub(crate) cursor_px: Cell<[Option<(f32, f32)>; 2]>,
    /// The window's scale factor (physical px per logical px; fractional
    /// DPI), from the host's raw events / build diagnostics.
    pub(crate) scale_factor: Cell<f32>,
    /// Style/visibility revision — bumped by layer toggles (and any future
    /// theme swap); a damage-key input (`style_gen`).
    pub(crate) style_rev: Cell<u64>,
    /// Raw-pointer bookkeeping (free hover, middle-drag pan) fed by the
    /// host's `raw_window_event` seam.
    pub(crate) raw: RefCell<RawInput>,
    /// The active routing layer, as a copper slab name. `None` = the default
    /// (top copper). Set from the layer panel's set-active affordance; switching
    /// it while a route is pending drops a via (ladder level 1).
    pub(crate) active_layer: RefCell<Option<String>>,
    /// The open top-level menu-bar menu, by its lowercase value token (`"file"`,
    /// `"edit"`, …), or `None` when every menu is closed. The app-owned open-menu
    /// slot damascene's [`menubar`](damascene_core::menubar) folds trigger clicks
    /// into (`RefCell` for the interior-mutability pattern: flipped in `on_event`,
    /// read in `build`). Clicking outside (the popover scrim) or invoking any row
    /// closes it. The command palette borrows this slot with a non-menu sentinel
    /// (`palette::PALETTE_MENU_GATE`) to inherit the raw-input gates keyed off an
    /// open menu — see `set_palette_open`.
    pub(crate) open_menu: RefCell<Option<String>>,
    /// App-wide chrome display units (session-only; persistence is out of scope).
    pub(crate) display_units: Cell<DisplayUnits>,
    /// Board procedural-grid style (dots by default, or hairline lines).
    pub(crate) grid_style: Cell<GridStyle>,
    /// Whether board editing gestures snap to the displayed pane grid.
    pub(crate) snap_to_grid: Cell<bool>,
    /// Most recent export result, rendered as a persistent menu-bar chip.
    pub(crate) chrome_notice: RefCell<Option<crate::chrome::actions::ChromeNotice>>,
    /// The small Help modal currently open, if any.
    pub(crate) chrome_dialog: Cell<Option<crate::chrome::dialogs::ChromeDialog>>,
    /// File ▸ Quit's host-observed exit request (headless tests inspect it).
    pub(crate) quit_requested: Cell<bool>,
    /// Live Explorer substring filter and its text-input selection.
    pub(crate) explorer_filter: RefCell<String>,
    pub(crate) explorer_filter_selection: RefCell<Selection>,
    /// Command-palette modal state, appended to keep the shell change minimal.
    pub(crate) palette_open: Cell<bool>,
    pub(crate) palette_ui: RefCell<PaletteUi>,
    /// One-shot programmatic focus requests drained by damascene after layout.
    pub(crate) focus_requests: RefCell<Vec<String>>,
    /// Editable Properties-panel raw text + caret ownership. Appended state for
    /// the oracle's fieldRaw commit/revert behavior.
    pub(crate) inspector_ui: RefCell<crate::panels::properties::InspectorUi>,
}

/// The trace / via defaults the Route tool commits with, sourced from the same
/// [`DesignRules::default()`](eutectic_core::route::DesignRules) the DRC query and the
/// autorouter consume: trace width = `min_trace_width` (0.15 mm), via drill =
/// `min_trace_width`, via pad = `2 * min_trace_width` — the exact `width` /
/// `via_drill` / `via_pad` derivation `eutectic_core::autoroute` applies. Returned as
/// `(width, via_drill, via_pad)`.
pub(crate) fn route_defaults() -> (Nm, Nm, Nm) {
    let rules = eutectic_core::route::DesignRules::default();
    (
        rules.min_trace_width,
        rules.min_trace_width,
        2 * rules.min_trace_width,
    )
}

impl EutecticApp {
    pub fn new(domain: DomainState) -> Self {
        let derived = match &domain.doc {
            Ok(doc) => DerivedCaches::build(doc, &domain.lib, &domain.lib_notes),
            Err(_) => DerivedCaches::empty(),
        };
        EutecticApp {
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
            cursor_board_mm: Cell::new(None),
            tools: RefCell::new(std::collections::BTreeMap::new()),
            focused_pane: Cell::new(PaneId::A),
            measure: Cell::new(MeasureState::default()),
            measure_pane: Cell::new(PaneId::A),
            mailbox: SourceMailbox::disconnected(),
            open_mailbox: crate::open_dialog::OpenMailbox::new(),
            open_dialog_launcher: std::sync::Arc::new(crate::open_dialog::NativeOpenDialog),
            background_wakeup: std::sync::Arc::new(|| {}),
            open_dialog_busy: Cell::new(false),
            open_discard_approval: RefCell::new(None),
            next_open_request_id: Cell::new(0),
            active_dialog_request_id: Cell::new(None),
            pending_open: RefCell::new(None),
            recents: RefCell::new(crate::recents::RecentFiles::new()),
            recents_path: None,
            recent_open: Cell::new(false),
            watch_path_tx: None,
            sections: Cell::new(SectionOpen::default()),
            libraries_open: Cell::new(false),
            lib_ui: RefCell::new(LibUi::default()),
            lib_statuses: RefCell::new(None),
            drag: RefCell::new(None),
            suppress_click: Cell::new(false),
            route: RefCell::new(None),
            trace_drag: RefCell::new(None),
            camera_pan: RefCell::new(None),
            pane_cams: RefCell::new([PaneCam::default(), PaneCam::default()]),
            gpu: RefCell::new(None),
            pane_px: Cell::new([None, None]),
            strip_px: Cell::new([None, None]),
            cursor_px: Cell::new([None, None]),
            scale_factor: Cell::new(1.0),
            style_rev: Cell::new(0),
            raw: RefCell::new(RawInput::default()),
            active_layer: RefCell::new(None),
            open_menu: RefCell::new(None),
            display_units: Cell::new(DisplayUnits::default()),
            grid_style: Cell::new(GridStyle::default()),
            snap_to_grid: Cell::new(true),
            chrome_notice: RefCell::new(None),
            chrome_dialog: Cell::new(None),
            quit_requested: Cell::new(false),
            explorer_filter: RefCell::new(String::new()),
            explorer_filter_selection: RefCell::new(Selection::default()),
            palette_open: Cell::new(false),
            palette_ui: RefCell::new(PaletteUi::default()),
            focus_requests: RefCell::new(Vec::new()),
            inspector_ui: RefCell::new(crate::panels::properties::InspectorUi::default()),
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
    /// point. A pathless fixture message is stamped with this app's current source path;
    /// an explicitly tagged message is preserved for stale-path regression tests. The
    /// next `before_build` drains and applies it.
    pub fn mailbox_push(&self, mut msg: SourceMsg) {
        let SourceMsg::Changed { path, .. } = &mut msg;
        if path.is_none() {
            *path = self.domain.source_path.clone();
        }
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
        self.derived.borrow().schematic_scene.is_some()
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

    /// Set the measure preview state — for fixtures / tests that render a
    /// measure-in-progress scene without driving live pointer events.
    pub fn set_measure(&self, m: MeasureState) {
        self.measure.set(m);
    }

    /// The single source of truth for chrome display units.
    pub fn display_units(&self) -> DisplayUnits {
        self.display_units.get()
    }

    /// Toggle the app-wide display unit between millimetres and inches.
    pub(crate) fn toggle_display_units(&self) {
        self.display_units.set(self.display_units.get().toggled());
    }

    /// The current board-grid presentation.
    pub fn grid_style(&self) -> GridStyle {
        self.grid_style.get()
    }

    /// Toggle dots/lines and damage the canvas so the uniform is rewritten.
    pub(crate) fn toggle_grid_style(&self) {
        self.grid_style.set(self.grid_style.get().toggled());
        self.style_rev.set(self.style_rev.get() + 1);
    }

    /// Whether board editing gestures snap to the displayed pane grid.
    pub fn snap_to_grid(&self) -> bool {
        self.snap_to_grid.get()
    }

    /// Toggle app-wide snap-to-grid behavior.
    pub(crate) fn toggle_snap_to_grid(&self) {
        self.snap_to_grid.set(!self.snap_to_grid.get());
    }

    /// The exact integer-nm pitch currently displayed in `pane`.
    pub(crate) fn displayed_grid_pitch(&self, pane: PaneId) -> Nm {
        let zoom = physical_zoom(self.pane_camera(pane).zoom, self.scale_factor.get());
        crate::render::grid_pitch_nm(zoom)
    }

    /// Whether File ▸ Quit has requested a clean host exit.
    pub fn quit_requested(&self) -> bool {
        self.quit_requested.get()
    }
}

/// Fold device scale into a logical pane zoom. Both grid rendering and editing
/// snapping use this value so the snap pitch equals the displayed grid pitch.
pub(crate) fn physical_zoom(logical_zoom: f64, scale_factor: f32) -> f64 {
    logical_zoom * (scale_factor as f64).max(0.1)
}

/// Round one integer-nm coordinate to the nearest `pitch` multiple. Exact
/// half-pitch ties round away from zero on both sides of the origin.
pub(crate) fn snap_nm(value: Nm, pitch: Nm) -> Nm {
    assert!(pitch > 0, "grid pitch must be positive");
    let value = value as i128;
    let pitch = pitch as i128;
    let quotient = value / pitch;
    let remainder = value % pitch;
    let rounded = if remainder.abs() * 2 >= pitch {
        quotient + remainder.signum()
    } else {
        quotient
    } * pitch;
    rounded.clamp(Nm::MIN as i128, Nm::MAX as i128) as Nm
}

/// Snap both coordinates of a board point in exact integer-nm arithmetic.
pub(crate) fn snap_point(
    point: eutectic_core::coord::Point,
    pitch: Nm,
) -> eutectic_core::coord::Point {
    eutectic_core::coord::Point {
        x: snap_nm(point.x, pitch),
        y: snap_nm(point.y, pitch),
    }
}
