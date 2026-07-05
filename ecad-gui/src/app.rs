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
pub(crate) use pane::{
    CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, LAYOUT_TOGGLE_KEY, REDO_KEY, SAVE_KEY, UNDO_KEY,
    finding_row_key, pane_index,
};

use crate::findings::Findings;
use crate::reload::{SourceMailbox, SourceMsg};
use crate::tool::{DragState, MeasureState, Tool};
use damascene_core::prelude::*;
use ecad_core::command::Transaction;
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
            drag: RefCell::new(None),
            suppress_click: Cell::new(false),
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

    /// Start a canned component drag with the ghost at `to` — for fixtures /
    /// tests that render a drag-in-progress scene without driving live pointer
    /// events. Uses the same drag builder as the interactive pointer-down path
    /// (pad shapes + ratsnest pins from the cached candidates), anchored at the
    /// component's current position with zero slop. Returns `false` when the
    /// component doesn't resolve (no doc / no pad candidates).
    pub fn set_drag(
        &self,
        comp: &ecad_core::id::EntityId,
        pane: PaneId,
        to: ecad_core::coord::Point,
    ) -> bool {
        let Ok(doc) = &self.domain.doc else {
            return false;
        };
        let Some(c) = doc.components.get(comp) else {
            return false;
        };
        let start = c.pos.value;
        let Some(mut drag) = self.make_drag(comp.clone(), pane, start, 0) else {
            return false;
        };
        drag.update(to);
        *self.drag.borrow_mut() = Some(drag);
        true
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
    ///
    /// This is the **external-reload** entry (disk content applied): on success it
    /// additionally resets the m6 editing state — the doc now mirrors disk, so the
    /// dirty flag clears, the undo/redo stacks empty (a snapshot from before an
    /// external reload would silently discard the external edit if replayed), any
    /// pending conflict is consumed, and the saved-content baselines re-anchor to
    /// the fresh doc. Registry edits re-elaborate through [`swap_source`]
    /// (`pub(crate)`) instead, which leaves the editing state alone.
    ///
    /// [`swap_source`]: EcadApp::swap_source
    pub fn apply_reload(&mut self, source: String) {
        if self.swap_source(source) {
            let d = &mut self.domain;
            d.edit.dirty = false;
            d.edit.undo.clear();
            d.edit.redo.clear();
            d.edit.conflict = None;
            d.edit.last_saved_write = None;
            d.edit.saved_canon = d.doc.as_ref().ok().map(ecad_core::text::serialize);
        }
    }

    /// The shared re-elaborate-and-swap core of every source transition (external
    /// reload, registry edit, undo/redo): re-resolve libs + re-elaborate `source`
    /// and, on success, swap in the new history/doc/lib, rebuild the derived
    /// caches, prune the selection to ids that still resolve, bump the revision,
    /// and clear any standing reload error — cameras / layer visibility / pane
    /// layout / tool are untouched. On failure the last-good doc stays rendered
    /// (permissive) and the error lands in `domain.reload_error`; returns whether
    /// the swap happened. Never touches the m6 editing state — callers own that.
    pub(crate) fn swap_source(&mut self, source: String) -> bool {
        let (lib, notes, history) = self.domain.elaborate_source(&source);
        match history {
            Ok(history) => {
                let doc = history.doc().clone();
                let derived = DerivedCaches::build(&doc, &lib, &notes);
                // Prune selection + hover to ids that still resolve in the NEW doc,
                // using the freshly-built candidate/schematic ids as the resolvable set.
                self.prune_selection(&doc, &derived);
                *self.derived.borrow_mut() = derived;
                self.domain.lib = lib;
                self.domain.lib_notes = notes;
                self.domain.history = Some(history);
                self.domain.doc = Ok(doc);
                self.domain.source = source;
                self.domain.revision += 1;
                self.domain.reload_error = None;
                // Any in-flight drag preview is anchored to the old candidates; drop it.
                *self.drag.borrow_mut() = None;
                true
            }
            Err(err) => {
                // Permissive: keep the last-good doc + caches + resolved lib on screen;
                // surface the error persistently. Do NOT bump the revision (nothing
                // derived changed).
                self.domain.reload_error = Some(err);
                false
            }
        }
    }

    /// A disk change arrived through the live-source mailbox (m5 watcher → m6 save
    /// model). Three-way routing, per the decided model:
    ///
    /// - **our own save echo** (text equals the last Save write): consumed
    ///   silently — the GUI never reloads its own write back;
    /// - **doc clean**: auto-apply as before (the GUI follows external edits);
    /// - **doc dirty**: never silent last-writer — park the text as the pending
    ///   conflict; the persistent banner offers explicit Reload / Keep-mine. A
    ///   newer delivery replaces the pending text.
    pub(crate) fn handle_disk_change(&mut self, source: String) {
        if self.domain.edit.last_saved_write.as_deref() == Some(source.as_str()) {
            // Watcher echo of our own write — consumed ONCE. Clearing the token
            // means a later byte-identical delivery is the genuine external
            // write it is (and, while dirty, raises the conflict banner rather
            // than being silently swallowed).
            self.domain.edit.last_saved_write = None;
            return;
        }
        if self.domain.edit.dirty {
            self.domain.edit.conflict = Some(source);
        } else {
            self.apply_reload(source);
        }
    }

    /// Commit a GUI-authored transaction — **the command-commit path** (m6). The
    /// pre-commit doc is snapshotted (canonical `serialize`) onto the undo stack
    /// (redo cleared, stack bounded), the engine commit runs against the held
    /// [`History`](ecad_core::history::History), and on success the existing
    /// reload machinery runs: derived caches rebuild as one bundle, the revision
    /// bumps, and the selection is pruned to ids that still resolve (a moved
    /// component stays selected). The doc is now dirty (commits not yet written
    /// to the file). On failure nothing changes (engine atomicity) and the error
    /// is returned (callers surface it in `edit.error`).
    pub(crate) fn commit_edit(&mut self, txn: Transaction, label: &str) -> Result<(), String> {
        let snapshot = match &self.domain.doc {
            Ok(doc) => ecad_core::text::serialize(doc),
            Err(e) => return Err(format!("no document to edit: {e}")),
        };
        self.domain.commit(txn, label)?;
        let edit = &mut self.domain.edit;
        edit.undo.push(snapshot);
        if edit.undo.len() > domain::UNDO_CAP {
            edit.undo.remove(0);
        }
        edit.redo.clear();
        edit.dirty = true;
        edit.error = None;
        // The existing derived machinery: rebuild the whole bundle, prune, bump.
        let doc = self.domain.doc.as_ref().expect("commit succeeded").clone();
        let derived = DerivedCaches::build(&doc, &self.domain.lib, &self.domain.lib_notes);
        self.prune_selection(&doc, &derived);
        *self.derived.borrow_mut() = derived;
        self.domain.revision += 1;
        Ok(())
    }

    /// Undo the newest GUI commit by restoring its pre-commit source snapshot
    /// through the same reload-like path (re-resolve libs — a snapshot may differ
    /// in `use` lines — then re-elaborate): cameras and still-resolving selection
    /// are preserved exactly like a reload. The current state is pushed onto the
    /// redo stack. Dirty is recomputed by the string compare against the
    /// last-saved content — undoing back to the saved state clears the flag;
    /// anything else keeps it. No-op when the undo stack is empty.
    pub fn undo(&mut self) {
        let Some(snapshot) = self.domain.edit.undo.pop() else {
            return;
        };
        let current = match &self.domain.doc {
            Ok(doc) => ecad_core::text::serialize(doc),
            Err(_) => {
                self.domain.edit.undo.push(snapshot);
                return;
            }
        };
        if self.swap_source(snapshot.clone()) {
            let edit = &mut self.domain.edit;
            edit.redo.push(current);
            edit.dirty = edit.saved_canon.as_deref() != Some(snapshot.as_str());
        } else {
            // The snapshot failed to elaborate (a registry edit in between can do
            // that): keep the stack intact; swap_source surfaced the error.
            self.domain.edit.undo.push(snapshot);
        }
    }

    /// Redo the newest undone state (the mirror of [`undo`](Self::undo)); the
    /// current state goes back onto the undo stack. Same dirty string-compare.
    /// No-op when the redo stack is empty.
    pub fn redo(&mut self) {
        let Some(snapshot) = self.domain.edit.redo.pop() else {
            return;
        };
        let current = match &self.domain.doc {
            Ok(doc) => ecad_core::text::serialize(doc),
            Err(_) => {
                self.domain.edit.redo.push(snapshot);
                return;
            }
        };
        if self.swap_source(snapshot.clone()) {
            let edit = &mut self.domain.edit;
            edit.undo.push(current);
            edit.dirty = edit.saved_canon.as_deref() != Some(snapshot.as_str());
        } else {
            self.domain.edit.redo.push(snapshot);
        }
    }

    /// Explicit save (m6 save model): write `text::serialize(doc)` to the source
    /// path atomically (temp + rename, like the registry), clear the dirty flag,
    /// and remember the written text so the watcher echo of our own write is
    /// consumed silently. No-ops: no source path (fixtures — no save affordance),
    /// no loaded doc, or a clean doc (never rewrite the user's hand-authored file
    /// into canonical form unless there is something to save). A write failure
    /// surfaces in `edit.error` and the doc stays dirty.
    pub fn save(&mut self) {
        let Some(path) = self.domain.source_path.clone() else {
            return;
        };
        let Ok(doc) = &self.domain.doc else {
            return;
        };
        if !self.domain.edit.dirty {
            return;
        }
        let text = ecad_core::text::serialize(doc);
        match domain::atomic_write(&path, &text) {
            Ok(()) => {
                let edit = &mut self.domain.edit;
                edit.dirty = false;
                edit.saved_canon = Some(text.clone());
                edit.last_saved_write = Some(text);
                edit.error = None;
                // A save while the conflict banner is up IS the keep-mine
                // resolution made permanent (the disk was just explicitly
                // overwritten), so the banner dismisses with it.
                edit.conflict = None;
            }
            Err(e) => self.domain.edit.error = Some(e),
        }
    }

    /// Resolve the conflict banner with **Reload**: discard my edits, apply the
    /// disk text (through the external-reload path, which clears the editing
    /// state). No-op when no conflict is pending.
    pub fn conflict_reload(&mut self) {
        if let Some(disk) = self.domain.edit.conflict.take() {
            self.apply_reload(disk);
        }
    }

    /// Resolve the conflict banner with **Keep mine**: dismiss; the doc stays
    /// dirty and the next save overwrites disk. No-op when no conflict is pending.
    pub fn conflict_keep(&mut self) {
        self.domain.edit.conflict = None;
    }

    /// Is the doc dirty (commits not yet written to the file)?
    pub fn dirty(&self) -> bool {
        self.domain.edit.dirty
    }

    /// The pending disk-conflict text, if the watcher delivered an external
    /// change while the doc was dirty (the banner is up). For tests / fixtures.
    pub fn conflict(&self) -> Option<String> {
        self.domain.edit.conflict.clone()
    }

    /// The undo / redo stack depths. For tests and the toolbar.
    pub fn undo_depths(&self) -> (usize, usize) {
        (self.domain.edit.undo.len(), self.domain.edit.redo.len())
    }

    /// Is a component drag in flight? For tests.
    pub fn drag_active(&self) -> bool {
        self.drag.borrow().is_some()
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

    /// The per-source findings chips track the cached findings: a doc with
    /// findings renders source chips (no ✓); a clean doc renders exactly the
    /// single neutral ✓ chip (the all-clean branch of `findings_chips`).
    #[test]
    fn findings_chips_match_findings() {
        let app = drc_violation();
        let f = app.findings();
        assert!(
            f.errors >= 1,
            "the fixture has at least the clearance error"
        );
        assert!(!f.is_clean());
        assert!(
            !app.findings_chips().is_empty(),
            "a doc with findings renders at least one source chip"
        );

        // The clean doc from findings/tests.rs: single-pin nets, no routed copper,
        // the cap placed mid-board so its (toy) pad copper clears the board edge.
        let clean = EcadApp::new(DomainState::from_source(
            "inst C1 Cap\nnet SOLO C1.p1\nnc C1.p2\nplace C1 (5mm, 5mm)\n\
             board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)\n"
                .to_string(),
            Some("clean.ecad".to_string()),
        ));
        assert!(clean.findings().is_clean());
        let chips = clean.findings_chips();
        assert_eq!(chips.len(), 1, "all-clean is a single ✓ chip");
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

    // -----------------------------------------------------------------------
    // Milestone-6 slice A: the editing foundation. Command commits, the save
    // model (dirty / explicit save / echo suppression / conflict banner),
    // undo/redo via source snapshots, and drag placement end to end through
    // synthesized pointer events. All headless.
    // -----------------------------------------------------------------------

    use crate::fixtures::edit_board_domain;
    use ecad_core::command::{Command, Transaction};
    use ecad_core::coord::{MM as NM_PER_MM, Point};
    use ecad_core::id::EntityId;

    /// A pointer event of `kind` at `pos`, routed to pane A's canvas — the
    /// headless stand-in for the host's pointer routing (`key` IS the target key
    /// for real pointer events; `UiTarget` is non-exhaustive, so tests carry the
    /// route in `key` and the app's canvas gate accepts either).
    fn pointer(kind: UiEventKind, pos: (f32, f32)) -> UiEvent {
        let mut e = UiEvent::synthetic_click(PaneId::A.canvas_key());
        e.kind = kind;
        e.pointer = Some(pos);
        e
    }

    /// A window-level Escape.
    fn escape() -> UiEvent {
        let mut e = UiEvent::synthetic_click("");
        e.key = None;
        e.kind = UiEventKind::Escape;
        e
    }

    /// A hotkey event for `action` (what damascene emits when a registered
    /// chord matches — Ctrl+S/Z/… — with the action name as the route).
    fn hotkey(action: &str) -> UiEvent {
        let mut e = UiEvent::synthetic_click(action);
        e.kind = UiEventKind::Hotkey;
        e
    }

    /// The editing app: the padded board (pickable pads) as pane A.
    fn edit_app() -> EcadApp {
        EcadApp::new(edit_board_domain())
    }

    /// The doc position of component `id`.
    fn comp_pos(app: &EcadApp, id: &EntityId) -> Point {
        app.domain.doc.as_ref().unwrap().components[id].pos.value
    }

    /// Commit a canned move of `C1` by `(dx, dy)` mm — the test shorthand for "a
    /// GUI edit happened".
    fn commit_move(app: &mut EcadApp, dx: i64, dy: i64) {
        let comp = EntityId::new("C1");
        let p = comp_pos(app, &comp);
        let target = Point {
            x: p.x + dx * NM_PER_MM,
            y: p.y + dy * NM_PER_MM,
        };
        app.commit_edit(Transaction::one(Command::Pin(comp, target)), "move")
            .expect("move commits");
    }

    /// Map a board point to pane-A screen px in a settled render (the exact
    /// inverse composition the pick path applies): board → asset content px
    /// (through the asset's honest natural-size content rect) → screen through
    /// the pane's live camera.
    fn px_of_board(app: &EcadApp, r: &crate::harness::Rendered, p: Point) -> (f32, f32) {
        let canvas = app.board_canvas_clone();
        let rect = r.ui.rect_of_key(PaneId::A.canvas_key()).expect("pane A");
        let vv =
            r.ui.viewport_view_by_key(PaneId::A.canvas_key())
                .expect("pane A view");
        let mm = (p.x as f32 / NM_PER_MM as f32, p.y as f32 / NM_PER_MM as f32);
        let content = canvas
            .board_mm_to_content_px(mm, canvas.content_rect((rect.x, rect.y, rect.w, rect.h)))
            .expect("maps");
        vv.project(content, (rect.x, rect.y))
    }

    /// The screen→board mapping the pointer handler applies, for computing the
    /// exact expected commit target from the synthesized pixel positions.
    fn board_of_px(app: &EcadApp, r: &crate::harness::Rendered, px: (f32, f32)) -> Point {
        let canvas = app.board_canvas_clone();
        let rect = r.ui.rect_of_key(PaneId::A.canvas_key()).unwrap();
        let vv = r.ui.viewport_view_by_key(PaneId::A.canvas_key()).unwrap();
        pick::pointer_to_board_nm(
            &canvas,
            px,
            canvas.content_rect((rect.x, rect.y, rect.w, rect.h)),
            vv,
        )
        .expect("in view")
    }

    /// A pad-candidate center of `comp` (the grab point for drag tests).
    fn pad_center_of(app: &EcadApp, comp: &EntityId) -> Point {
        let derived = app.derived.borrow();
        let view = derived.board.as_ref().expect("board projects");
        let c = view
            .candidates
            .iter()
            .find(|c| matches!(&c.id, SemanticId::Pin { comp: cc, .. } if cc == comp))
            .expect("comp has a pad candidate");
        Point {
            x: (c.aabb.0.x + c.aabb.1.x) / 2,
            y: (c.aabb.0.y + c.aabb.1.y) / 2,
        }
    }

    /// Commit → serialize fixpoint: after a GUI commit the domain source IS the
    /// canonical projection (`serialize(doc)`), re-elaborating it reproduces the
    /// doc, and re-serializing is byte-identical. The commit dirtied the doc,
    /// bumped the revision, and stacked one undo snapshot.
    #[test]
    fn commit_serialize_fixpoint_and_bookkeeping() {
        let mut app = edit_app();
        let rev0 = app.revision();
        assert!(!app.dirty());
        commit_move(&mut app, 3, 1);

        assert!(app.dirty(), "a commit dirties the doc");
        assert_eq!(app.revision(), rev0 + 1, "a commit bumps the revision");
        assert_eq!(app.undo_depths(), (1, 0));

        let s = app.domain.source.clone();
        let doc = app.domain.doc.as_ref().unwrap();
        assert_eq!(
            ecad_core::text::serialize(doc),
            s,
            "domain source is the canonical projection after a commit"
        );
        assert!(
            s.contains("pin C1"),
            "the move serialized as a pin override"
        );

        // serialize → parse/elaborate → serialize is a fixpoint.
        let d2 =
            DomainState::from_source_with(s.clone(), None, ecad_core::part::part_library(), |_| {
                Vec::new()
            });
        let doc2 = d2.doc.as_ref().expect("canonical text elaborates");
        assert_eq!(ecad_core::text::serialize(doc2), s, "serialize fixpoint");
        assert_eq!(
            doc2.components[&EntityId::new("C1")].pos.value,
            doc.components[&EntityId::new("C1")].pos.value,
            "the pinned position survives the round-trip"
        );
    }

    /// Undo/redo round-trip with dirty-flag correctness (no save in between):
    /// undoing the only edit returns to the loaded state, which equals the
    /// load-time saved baseline → clean; redo re-applies and re-dirties.
    #[test]
    fn undo_redo_roundtrip_dirty_flags() {
        let mut app = edit_app();
        let comp = EntityId::new("C1");
        let pos0 = comp_pos(&app, &comp);
        let rev0 = app.revision();

        commit_move(&mut app, 3, 1);
        let pos1 = comp_pos(&app, &comp);
        assert_ne!(pos0, pos1, "the pin moved the component");
        assert!(app.dirty());

        app.undo();
        assert_eq!(comp_pos(&app, &comp), pos0, "undo restores the position");
        assert!(
            !app.dirty(),
            "undo back to the loaded state clears dirty (snapshot == saved baseline)"
        );
        assert_eq!(app.undo_depths(), (0, 1));
        assert_eq!(app.revision(), rev0 + 2, "undo re-elaborates (a revision)");

        app.redo();
        assert_eq!(comp_pos(&app, &comp), pos1, "redo re-applies the move");
        assert!(app.dirty(), "redo away from the saved state re-dirties");
        assert_eq!(app.undo_depths(), (1, 0));

        // A new commit clears the redo stack.
        app.undo();
        assert_eq!(app.undo_depths(), (0, 1));
        commit_move(&mut app, 1, 0);
        assert_eq!(app.undo_depths(), (1, 0), "a fresh commit clears redo");
    }

    /// The classic dirty trap, with a save in the middle: edit A, save, edit B —
    /// then undo lands on the SAVED state (clean), undo again on the base
    /// (dirty), and redo forward re-crosses the same boundary.
    #[test]
    fn undo_after_save_dirty_string_compare() {
        let scratch = Scratch::new("undo-save");
        let file = scratch.0.join("board.ecad");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());

        commit_move(&mut app, 3, 0); // state A
        app.save();
        assert!(!app.dirty(), "save clears dirty");
        let saved = std::fs::read_to_string(&file).expect("save wrote the file");
        assert_eq!(
            saved, app.domain.source,
            "save wrote the canonical projection"
        );

        commit_move(&mut app, 0, 2); // state B
        assert!(app.dirty());

        app.undo(); // back to A == last-saved content
        assert!(
            !app.dirty(),
            "undo onto the exactly-saved state must clear dirty (string compare)"
        );
        app.undo(); // base ≠ saved A
        assert!(app.dirty(), "undo past the saved state re-dirties");
        app.redo(); // A again
        assert!(!app.dirty(), "redo onto the saved state clears dirty again");
        app.redo(); // B
        assert!(app.dirty());
    }

    /// Save-echo suppression: after a save, the watcher's delivery of our own
    /// write is consumed silently — no reload, no revision bump, no conflict.
    #[test]
    fn save_echo_is_suppressed() {
        let scratch = Scratch::new("echo");
        let file = scratch.0.join("board.ecad");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());
        commit_move(&mut app, 2, 0);
        app.save();
        let rev = app.revision();

        // The watcher sees the mtime change and delivers our own write back.
        let echoed = std::fs::read_to_string(&file).unwrap();
        app.mailbox_push(SourceMsg::Changed(echoed));
        app.before_build();

        assert_eq!(app.revision(), rev, "an echo must not reload");
        assert!(app.conflict().is_none(), "an echo is not a conflict");
        assert!(!app.dirty());

        // The echo token is ONE-SHOT: after the echo is consumed, a later
        // byte-identical delivery is a genuine external write. While dirty it
        // must raise the conflict banner, not be silently swallowed.
        let echoed = std::fs::read_to_string(&file).unwrap();
        commit_move(&mut app, 3, 0);
        assert!(app.dirty());
        app.mailbox_push(SourceMsg::Changed(echoed));
        app.before_build();
        assert!(
            app.conflict().is_some(),
            "an identical external write after the echo was consumed is a real conflict"
        );
    }

    /// The conflict flow, Reload branch: a disk change while dirty parks as the
    /// pending conflict (nothing applied); the explicit Reload action applies the
    /// disk text, clears dirty, and empties the undo stack.
    #[test]
    fn conflict_reload_discards_edits_and_follows_disk() {
        let mut app = edit_app();
        commit_move(&mut app, 4, 0);
        let rev_dirty = app.revision();

        app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        app.before_build();
        assert_eq!(
            app.conflict().as_deref(),
            Some(SCHEMATIC_ECAD),
            "the external change is parked, not applied"
        );
        assert_eq!(app.revision(), rev_dirty, "no silent reload while dirty");
        assert!(app.dirty(), "the doc stays dirty under the banner");

        let cx = EventCx::new();
        app.on_event(click(CONFLICT_RELOAD_KEY), &cx);
        assert!(app.conflict().is_none(), "reload consumes the conflict");
        assert!(!app.dirty(), "the doc now mirrors disk");
        assert_eq!(
            app.undo_depths(),
            (0, 0),
            "external reload clears undo/redo"
        );
        assert!(
            app.has_schematic(),
            "the disk text (schematic doc) was applied"
        );
    }

    /// The conflict flow, Keep-mine branch: the banner dismisses, the doc stays
    /// dirty at its revision, and the next save overwrites the disk.
    #[test]
    fn conflict_keep_mine_stays_dirty_and_save_overwrites() {
        let scratch = Scratch::new("keep-mine");
        let file = scratch.0.join("board.ecad");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());
        commit_move(&mut app, 4, 0);
        let my_source = app.domain.source.clone();

        // Someone writes an external version to disk; the watcher delivers it.
        std::fs::write(&file, SCHEMATIC_ECAD).unwrap();
        app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        app.before_build();
        assert!(app.conflict().is_some());

        let cx = EventCx::new();
        app.on_event(click(CONFLICT_KEEP_KEY), &cx);
        assert!(app.conflict().is_none(), "keep-mine dismisses the banner");
        assert!(app.dirty(), "the doc stays dirty");
        assert_eq!(app.domain.source, my_source, "my edits survive");

        app.save();
        assert!(!app.dirty());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            my_source,
            "the next save overwrites the disk (explicit last-writer)"
        );
    }

    /// A newer external delivery replaces the pending conflict text, and a CLEAN
    /// doc still follows disk automatically (the m5 behavior is unchanged).
    #[test]
    fn conflict_updates_and_clean_doc_still_follows() {
        let mut app = edit_app();
        commit_move(&mut app, 1, 0);
        app.mailbox_push(SourceMsg::Changed("v1".to_string()));
        app.before_build();
        app.mailbox_push(SourceMsg::Changed("v2".to_string()));
        app.before_build();
        assert_eq!(
            app.conflict().as_deref(),
            Some("v2"),
            "the newest external text wins the pending slot"
        );
        // Resolve, then verify the clean path still auto-applies.
        let cx = EventCx::new();
        app.on_event(click(CONFLICT_KEEP_KEY), &cx);
        let mut clean = edit_app();
        let rev0 = clean.revision();
        clean.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        clean.before_build();
        assert_eq!(clean.revision(), rev0 + 1, "a clean doc follows disk");
        assert!(!clean.dirty());
    }

    /// Saving while the conflict banner is up is the keep-mine resolution made
    /// permanent: the disk is explicitly overwritten and the banner dismisses.
    #[test]
    fn save_while_conflicted_overwrites_and_dismisses() {
        let scratch = Scratch::new("save-conflict");
        let file = scratch.0.join("board.ecad");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());
        commit_move(&mut app, 3, 0);
        app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
        app.before_build();
        assert!(app.conflict().is_some());

        app.save();
        assert!(!app.dirty());
        assert!(
            app.conflict().is_none(),
            "an explicit save resolves the conflict (last-writer, chosen)"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            app.domain.source,
            "the save overwrote the disk with my edits"
        );
    }

    /// No-path docs have no save: `save()` is a no-op (stays dirty, no error).
    #[test]
    fn save_without_path_is_a_noop() {
        let mut app = edit_app();
        commit_move(&mut app, 1, 1);
        assert!(app.domain.source_path.is_none());
        app.save();
        assert!(app.dirty(), "no path → nothing saved → still dirty");
        assert!(app.domain.edit.error.is_none());
    }

    /// Drag placement end to end through synthesized pointer events: pointer-down
    /// on a C1 pad arms the drag, drag moves the ghost, pointer-up commits a
    /// `Command::Pin` at exactly `orig_pos + (drop − grab)` (hard placement), the
    /// component's provenance is Pinned, the doc is dirty, and the moved part is
    /// selected. The trailing Click is suppressed (no re-select of the drop pad).
    #[test]
    fn drag_commits_pin_at_exact_delta() {
        let mut app = edit_app();
        let r = settle(&mut app);
        let comp = EntityId::new("C1");
        let orig = comp_pos(&app, &comp);
        let grab = pad_center_of(&app, &comp);

        let grab_px = px_of_board(&app, &r, grab);
        let drop_board = Point {
            x: grab.x + 4 * NM_PER_MM,
            y: grab.y + 3 * NM_PER_MM,
        };
        let drop_px = px_of_board(&app, &r, drop_board);
        // The exact board points the handler derives from those pixels (f32
        // round-trip included), so the expected delta is bit-exact.
        let p_grab = board_of_px(&app, &r, grab_px);
        let p_drop = board_of_px(&app, &r, drop_px);
        let expected = Point {
            x: orig.x + (p_drop.x - p_grab.x),
            y: orig.y + (p_drop.y - p_grab.y),
        };

        let cx = EventCx::new().with_ui_state(&r.ui);
        app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
        assert!(app.drag_active(), "pointer-down on a pad arms the drag");
        assert!(!app.dirty(), "arming commits nothing");

        app.on_event(pointer(UiEventKind::Drag, drop_px), &cx);
        {
            let drag = app.drag.borrow();
            let d = drag.as_ref().unwrap();
            assert!(d.moved, "a 4×3 mm drag is way past the slop");
            assert!(!d.ghost_shapes().is_empty(), "the ghost has pad shapes");
            assert!(
                !d.ratsnest().is_empty(),
                "netted pads produce ratsnest lines"
            );
        }
        assert!(!app.dirty(), "still nothing committed during the drag");

        let rev0 = app.revision();
        app.on_event(pointer(UiEventKind::PointerUp, drop_px), &cx);
        assert!(!app.drag_active(), "pointer-up finishes the drag");
        assert!(app.dirty(), "the move committed");
        assert_eq!(app.revision(), rev0 + 1);

        let doc = app.domain.doc.as_ref().unwrap();
        assert_eq!(
            doc.components[&comp].pos.value, expected,
            "a Pin is a fixed solver anchor — the part lands exactly at orig + delta"
        );
        assert_eq!(
            doc.components[&comp].pos.prov,
            ecad_core::doc::Provenance::Pinned,
            "the drag is a hard placement (Pin), per 'user dragged it exactly here'"
        );
        let ov = doc.overrides.get(&comp).expect("a pin override recorded");
        assert_eq!(ov.pos, Some(expected));
        assert_eq!(ov.strength, ecad_core::doc::Strength::Pin);
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Part(comp.clone())),
            "the moved part stays selected"
        );

        // The trailing Click of the same press is eaten exactly once.
        app.on_event(pointer(UiEventKind::Click, drop_px), &cx);
        assert_eq!(
            app.domain.selection.borrow().single(),
            Some(&SemanticId::Part(comp)),
            "the drag's trailing Click must not re-select the drop pad"
        );
    }

    /// Esc during a drag cancels: preview discarded, nothing committed, doc
    /// untouched and clean.
    #[test]
    fn escape_cancels_drag_without_commit() {
        let mut app = edit_app();
        let r = settle(&mut app);
        let comp = EntityId::new("C1");
        let pos0 = comp_pos(&app, &comp);
        let grab_px = px_of_board(&app, &r, pad_center_of(&app, &comp));
        let away_px = (grab_px.0 + 60.0, grab_px.1 + 40.0);

        let cx = EventCx::new().with_ui_state(&r.ui);
        app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
        app.on_event(pointer(UiEventKind::Drag, away_px), &cx);
        assert!(app.drag_active());

        app.on_event(escape(), &cx);
        assert!(!app.drag_active(), "Esc cancels the drag");
        assert!(!app.dirty(), "nothing committed");
        assert_eq!(comp_pos(&app, &comp), pos0, "the doc is untouched");

        // A later pointer-up is inert (no stale drag).
        app.on_event(pointer(UiEventKind::PointerUp, away_px), &cx);
        assert!(!app.dirty());
    }

    /// Click-without-drag stays a plain select: down + up on the same pad within
    /// the slop commits nothing, and the Click selects the pad as before.
    #[test]
    fn click_without_drag_is_plain_select() {
        let mut app = edit_app();
        let r = settle(&mut app);
        let comp = EntityId::new("C1");
        let grab = pad_center_of(&app, &comp);
        let grab_px = px_of_board(&app, &r, grab);

        let cx = EventCx::new().with_ui_state(&r.ui);
        app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
        app.on_event(pointer(UiEventKind::PointerUp, grab_px), &cx);
        assert!(!app.drag_active());
        assert!(!app.dirty(), "an un-moved press commits nothing");

        app.on_event(pointer(UiEventKind::Click, grab_px), &cx);
        match app.domain.selection.borrow().single() {
            Some(SemanticId::Pin { comp: c, .. }) => assert_eq!(c, &comp),
            other => panic!("a plain click selects the pad, got {other:?}"),
        }
    }

    /// Pointer-down on empty board / non-component copper arms no drag.
    #[test]
    fn pointer_down_on_empty_board_arms_nothing() {
        let mut app = edit_app();
        let r = settle(&mut app);
        // (10, 13) mm: inside the board and the pour, away from both caps' pads —
        // resolves to the POUR, which is not a component.
        let px = px_of_board(
            &app,
            &r,
            Point {
                x: 10 * NM_PER_MM,
                y: 13 * NM_PER_MM,
            },
        );
        let cx = EventCx::new().with_ui_state(&r.ui);
        app.on_event(pointer(UiEventKind::PointerDown, px), &cx);
        assert!(!app.drag_active(), "only components are draggable");
    }

    /// The editing hotkeys drive the same actions as the toolbar buttons: Ctrl+Z
    /// undoes the drag commit, Ctrl+Shift+Z redoes it, Ctrl+S saves.
    #[test]
    fn hotkeys_drive_undo_redo_save() {
        let scratch = Scratch::new("hotkeys");
        let file = scratch.0.join("board.ecad");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());
        let comp = EntityId::new("C1");
        let pos0 = comp_pos(&app, &comp);
        commit_move(&mut app, 5, 0);
        let pos1 = comp_pos(&app, &comp);

        let cx = EventCx::new();
        app.on_event(hotkey(UNDO_KEY), &cx);
        assert_eq!(comp_pos(&app, &comp), pos0, "Ctrl+Z undoes");
        app.on_event(hotkey(REDO_KEY), &cx);
        assert_eq!(comp_pos(&app, &comp), pos1, "Ctrl+Shift+Z redoes");
        app.on_event(hotkey(SAVE_KEY), &cx);
        assert!(!app.dirty(), "Ctrl+S saves");
        assert!(file.exists());

        // The registered chord table carries all three actions.
        let chords = app.hotkeys();
        for action in [SAVE_KEY, UNDO_KEY, REDO_KEY] {
            assert!(
                chords.iter().any(|(_, a)| a == action),
                "{action} is registered as a hotkey"
            );
        }
    }

    /// A registry edit while dirty preserves the editing state: the doc
    /// re-elaborates from the serialize-refreshed source (unsaved edits
    /// included), and dirty + undo survive — only an EXTERNAL reload resets them.
    #[test]
    fn registry_edit_preserves_dirty_and_undo() {
        let mut registry = Registry::new();
        registry.set("poc", &poc_parts_dir()).unwrap();
        let mut app = EcadApp::new(DomainState::from_source_registry(
            "inst C1 Cap\ninst C2 Cap\nnet N C1.p1 C2.p1\n\
             board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)\n"
                .to_string(),
            Some("t.ecad".to_string()),
            registry,
            None,
        ));
        let comp = EntityId::new("C1");
        let p = comp_pos(&app, &comp);
        let target = Point {
            x: p.x + 2 * NM_PER_MM,
            y: p.y,
        };
        app.commit_edit(Transaction::one(Command::Pin(comp.clone(), target)), "move")
            .expect("commits");
        assert!(app.dirty());

        let cx = EventCx::new();
        app.set_libraries_open(true);
        app.on_event(click(&library_remove_key("poc")), &cx);

        assert!(app.dirty(), "a registry edit must not clear dirty");
        assert_eq!(app.undo_depths(), (1, 0), "undo survives a registry edit");
        assert_eq!(
            comp_pos(&app, &comp),
            target,
            "the unsaved edit survives the registry-triggered re-elaborate"
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
