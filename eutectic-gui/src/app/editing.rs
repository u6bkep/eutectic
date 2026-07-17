//! The m6 editing engine of the app shell — the source-transition core
//! (external reload / registry-edit swap / undo / redo snapshots), the
//! command-commit path, the save model (dirty / explicit save / echo
//! suppression / conflict resolution), the Route-tool commit lowering, the
//! canned interaction-state arming helpers for fixtures, and the selection
//! prune every transition runs. Split out of `app.rs` as pure code motion
//! (gui-module-split).

use crate::app::domain::{self, DerivedCaches};
use crate::app::{EutecticApp, PaneId, route_defaults};
use crate::pick::SemanticId;
use crate::tool::{RouteState, TraceDragState};
use eutectic_core::command::{Command, Transaction};
use eutectic_core::doc::{Doc, Orient, Provenance};
use eutectic_core::id::{EntityId, TraceId};
use eutectic_core::ir::GenDirective;

impl EutecticApp {
    /// Fold editable-inspector events and the board-only Delete/Rotate routes.
    /// Kept beside their mutation implementations so the central app dispatcher
    /// only needs one direct-manipulation hook.
    pub(crate) fn handle_direct_manip_event(
        &mut self,
        event: &damascene_core::prelude::UiEvent,
    ) -> bool {
        if self.handle_inspector_event(event) {
            return true;
        }
        if event.kind == damascene_core::prelude::UiEventKind::Escape {
            let measure_kind =
                self.panes.borrow()[crate::app::pane::pane_index(self.measure_pane.get())].view;
            if measure_kind == crate::app::ViewKind::Schematic
                && self.tool_for(measure_kind) == crate::tool::Tool::Measure
                && self.measure.get().segment().is_some()
            {
                self.measure.set(Default::default());
                return true;
            }
        }
        let board_focused =
            self.panes.borrow()[crate::app::pane::pane_index(self.focused_pane.get())].view
                == crate::app::ViewKind::Board;
        if !board_focused {
            return false;
        }
        if event.is_click_or_activate(crate::chrome::menubar::DELETE_KEY)
            || event.is_hotkey(crate::chrome::menubar::DELETE_KEY)
        {
            self.delete_selection();
            return true;
        }
        if event.is_click_or_activate(crate::chrome::menubar::ROTATE_KEY)
            || event.is_hotkey(crate::chrome::menubar::ROTATE_KEY)
        {
            self.rotate_selection_ccw();
            return true;
        }
        false
    }

    /// Whether the current single selection can be deleted by the board editing
    /// surface. A selected pad denotes its owning component. Pours deliberately
    /// return false: their semantic id lacks a stable source-region identity.
    pub(crate) fn can_delete_selection(&self) -> bool {
        matches!(
            self.domain.selection.borrow().single(),
            Some(
                SemanticId::Part(_)
                    | SemanticId::Pin { .. }
                    | SemanticId::Trace(_)
                    | SemanticId::Via(_)
            )
        )
    }

    /// Whether the current single selection is a board component (a whole-part
    /// selection or one of its pads) and can therefore be rotated.
    pub(crate) fn can_rotate_selection(&self) -> bool {
        matches!(
            self.domain.selection.borrow().single(),
            Some(SemanticId::Part(_) | SemanticId::Pin { .. })
        )
    }

    /// Delete the current board selection through the one semantic-id mutation
    /// path shared by the Del chord and Edit ▸ Delete.
    pub(crate) fn delete_selection(&mut self) {
        let id = self.domain.selection.borrow().single().cloned();
        if let Some(id) = id {
            self.delete_id(id);
        }
    }

    /// Delete one board semantic id. This is the single mutation path used by all
    /// three doors: current-selection chord, menu row, and Delete-tool pick.
    pub(crate) fn delete_id(&mut self, id: SemanticId) {
        let result = match id {
            SemanticId::Part(id) | SemanticId::Pin { comp: id, .. } => self.delete_component(id),
            SemanticId::Trace(id) => {
                self.commit_edit(Transaction::one(Command::RemoveTrace(id)), "delete trace")
            }
            SemanticId::Via(id) => {
                self.commit_edit(Transaction::one(Command::RemoveVia(id)), "delete via")
            }
            // Pour deletion is intentionally deferred: net+layer does not identify
            // one authored Region when multiple pours share that pair.
            SemanticId::Pour { .. } | SemanticId::Net(_) => return,
        };
        if let Err(e) = result {
            self.domain.edit.error = Some(e);
        }
    }

    /// Rotate the selected board component 90° counterclockwise. A selected pad
    /// denotes its owner. Bottom-side placement is preserved.
    pub(crate) fn rotate_selection_ccw(&mut self) {
        let id = match self.domain.selection.borrow().single() {
            Some(SemanticId::Part(id)) | Some(SemanticId::Pin { comp: id, .. }) => id.clone(),
            _ => return,
        };
        let Some(component) = self
            .domain
            .doc
            .as_ref()
            .ok()
            .and_then(|doc| doc.components.get(&id))
        else {
            return;
        };
        let degrees = crate::inspector::rotation_degrees(component.orient) + 90.0;
        let bottom = component.orient.is_bottom();
        self.set_component_rotation(&id, degrees, bottom, "rotate component");
    }

    /// Commit one component coordinate in mm, preserving the other coordinate.
    pub(crate) fn set_component_position_mm(
        &mut self,
        id: &EntityId,
        x_mm: Option<f64>,
        y_mm: Option<f64>,
    ) {
        let Some(component) = self
            .domain
            .doc
            .as_ref()
            .ok()
            .and_then(|doc| doc.components.get(id))
        else {
            return;
        };
        let mm = eutectic_core::coord::MM as f64;
        let target = eutectic_core::coord::Point {
            x: x_mm
                .map(|v| (v * mm).round() as eutectic_core::coord::Nm)
                .unwrap_or(component.pos.value.x),
            y: y_mm
                .map(|v| (v * mm).round() as eutectic_core::coord::Nm)
                .unwrap_or(component.pos.value.y),
        };
        if let Err(e) = self.commit_edit(
            Transaction::one(Command::Pin(id.clone(), target)),
            "edit component position",
        ) {
            self.domain.edit.error = Some(e);
        }
    }

    /// Commit an inspector-authored planar rotation in degrees, preserving the
    /// component's current board side.
    pub(crate) fn set_component_rotation_deg(&mut self, id: &EntityId, degrees: f64) {
        let bottom = self
            .domain
            .doc
            .as_ref()
            .ok()
            .and_then(|doc| doc.components.get(id))
            .is_some_and(|component| component.orient.is_bottom());
        self.set_component_rotation(id, degrees, bottom, "edit component rotation");
    }

    /// Commit a trace width edit in mm by replacing the trace under its stable id.
    pub(crate) fn set_trace_width_mm(&mut self, id: TraceId, width_mm: f64) {
        let width =
            (width_mm * eutectic_core::coord::MM as f64).round() as eutectic_core::coord::Nm;
        self.replace_trace(id, "edit trace width", |trace| trace.width = width);
    }

    /// Cycle a trace through the document's copper slabs, preserving every other
    /// authored trace field. The common two-layer case is F.Cu ↔ B.Cu.
    pub(crate) fn cycle_trace_layer(&mut self, id: TraceId) {
        let copper = self.copper_layer_names();
        let Some(trace) = self
            .domain
            .doc
            .as_ref()
            .ok()
            .and_then(|doc| doc.traces.get(&id))
        else {
            return;
        };
        let next = copper
            .iter()
            .position(|name| name == &trace.layer)
            .map(|i| copper[(i + 1) % copper.len()].clone())
            .or_else(|| copper.first().cloned());
        if let Some(next) = next {
            self.replace_trace(id, "edit trace layer", |trace| trace.layer = next);
        }
    }

    fn replace_trace(
        &mut self,
        id: TraceId,
        label: &str,
        edit: impl FnOnce(&mut eutectic_core::route::Trace),
    ) {
        let Some(mut trace) = self
            .domain
            .doc
            .as_ref()
            .ok()
            .and_then(|doc| doc.traces.get(&id))
            .cloned()
        else {
            return;
        };
        edit(&mut trace);
        trace.prov = Provenance::Pinned;
        if let Err(e) = self.commit_edit(
            Transaction(vec![Command::RemoveTrace(id), Command::AddTrace(id, trace)]),
            label,
        ) {
            self.domain.edit.error = Some(e);
        }
    }

    fn set_component_rotation(&mut self, id: &EntityId, degrees: f64, bottom: bool, label: &str) {
        if !degrees.is_finite() {
            return;
        }
        let rounded = degrees.round();
        let mut orient = if (degrees - rounded).abs() < f64::EPSILON
            && rounded >= i32::MIN as f64
            && rounded <= i32::MAX as f64
        {
            Orient::from_deg(rounded as i32).unwrap_or_else(|| Orient::from_angle_deg(degrees))
        } else {
            Orient::from_angle_deg(degrees)
        };
        if bottom {
            orient = orient.flipped();
        }
        let mut source = match &self.domain.doc {
            Ok(doc) if doc.components.contains_key(id) => doc.source.clone(),
            _ => return,
        };
        source.retain(|d| !matches!(d, GenDirective::Rotate { path, .. } if path == id.as_str()));
        source.push(GenDirective::Rotate {
            path: id.as_str().to_string(),
            orient,
        });
        if let Err(e) = self.commit_edit(Transaction::one(Command::SetSource(source)), label) {
            self.domain.edit.error = Some(e);
        }
    }

    fn delete_component(&mut self, id: EntityId) -> Result<(), String> {
        let text = match &self.domain.doc {
            Ok(doc) => {
                let source = delete_component_from_source(&doc.source, &id).ok_or_else(|| {
                    format!(
                        "component `{id}` is generated by a range/def and cannot be deleted independently"
                    )
                })?;
                // LoadText is the command algebra's whole-tier-1 ingest path. Build
                // its canonical payload from a clone so schematic references and
                // ID-keyed exceptions disappear atomically with the instance while
                // materialized net-owned routing is preserved.
                let mut staged = doc.clone();
                staged.source = source;
                staged.overrides.remove(&id);
                staged.refdes_pins.remove(&id);
                if let Some(layout) = &mut staged.schematic {
                    prune_schematic_component(&mut layout.roots, id.as_str());
                }
                eutectic_core::text::serialize(&staged)
            }
            Err(e) => return Err(format!("no document to edit: {e}")),
        };
        self.commit_edit(
            Transaction::one(Command::LoadText(text)),
            "delete component",
        )
    }

    /// Start a canned component drag with the ghost at `to` — for fixtures /
    /// tests that render a drag-in-progress scene without driving live pointer
    /// events. Uses the same drag builder as the interactive pointer-down path
    /// (pad shapes + ratsnest pins from the cached candidates), anchored at the
    /// component's current position with zero slop. Returns `false` when the
    /// component doesn't resolve (no doc / no pad candidates).
    pub fn set_drag(
        &self,
        comp: &eutectic_core::id::EntityId,
        pane: PaneId,
        to: eutectic_core::coord::Point,
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
    /// [`swap_source`]: EutecticApp::swap_source
    pub fn apply_reload(&mut self, source: String) {
        if self.swap_source(source) {
            // An export result describes the prior live document; once a disk
            // reload/revert lands, do not leave that stale result advertised.
            *self.chrome_notice.borrow_mut() = None;
            let d = &mut self.domain;
            d.edit.dirty = false;
            d.edit.undo.clear();
            d.edit.redo.clear();
            d.edit.conflict = None;
            d.edit.last_saved_write = None;
            d.edit.saved_canon = d.doc.as_ref().ok().map(eutectic_core::text::serialize);
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
                // Any in-flight previews are anchored to the old pick
                // candidates, which the reload re-derives; drop them all.
                // (Trace ids themselves survive the reload since Decision 22
                // — ids round-trip — but candidate indices do not.)
                *self.drag.borrow_mut() = None;
                *self.route.borrow_mut() = None;
                *self.trace_drag.borrow_mut() = None;
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
    /// [`History`](eutectic_core::history::History), and on success the existing
    /// reload machinery runs: derived caches rebuild as one bundle, the revision
    /// bumps, and the selection is pruned to ids that still resolve (a moved
    /// component stays selected). The doc is now dirty (commits not yet written
    /// to the file). On failure nothing changes (engine atomicity) and the error
    /// is returned (callers surface it in `edit.error`).
    pub(crate) fn commit_edit(&mut self, txn: Transaction, label: &str) -> Result<(), String> {
        let snapshot = match &self.domain.doc {
            Ok(doc) => eutectic_core::text::serialize(doc),
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
            Ok(doc) => eutectic_core::text::serialize(doc),
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
            Ok(doc) => eutectic_core::text::serialize(doc),
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
        let text = eutectic_core::text::serialize(doc);
        match domain::atomic_write(&path, &text) {
            Ok(()) => {
                // A successful save supersedes any earlier export result (or
                // failure); the next export will publish a fresh notice.
                *self.chrome_notice.borrow_mut() = None;
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

    /// **Revert to Saved** (File menu): re-read the document from its source path
    /// and apply it through the external-reload path — discarding in-memory edits,
    /// clearing dirty / undo / redo, exactly as an on-disk change would when the
    /// doc is clean. No-op without a source path (an in-memory doc has nothing to
    /// revert to). A read failure surfaces as the persistent reload-error chip
    /// (permissive: the last-good doc stays on screen), never a crash.
    pub fn revert_to_saved(&mut self) {
        let Some(path) = self.domain.source_path.clone() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => self.apply_reload(text),
            Err(e) => {
                self.domain.reload_error = Some(format!("revert failed: {e}"));
            }
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

    /// Is a route pending (Route tool, started but not committed)? For tests.
    pub fn route_active(&self) -> bool {
        self.route.borrow().is_some()
    }

    /// A clone of the pending route state, if any — for tests / fixtures that
    /// assert on runs / vias / rubber.
    pub fn pending_route(&self) -> Option<RouteState> {
        self.route.borrow().clone()
    }

    /// Is a trace-vertex drag in flight? For tests.
    pub fn trace_drag_active(&self) -> bool {
        self.trace_drag.borrow().is_some()
    }

    /// The copper slab names of the current doc's stackup, top-down — the
    /// candidates for the active routing layer.
    pub fn copper_layer_names(&self) -> Vec<String> {
        let Ok(doc) = &self.domain.doc else {
            return Vec::new();
        };
        let su = eutectic_core::elaborate::stackup(&doc.source);
        su.copper_slabs().iter().map(|s| s.name.clone()).collect()
    }

    /// The active routing layer's copper slab name: the user's pick while it
    /// still resolves to a copper slab, else the default — the TOP copper slab
    /// (`copper_slabs()` is ordered top-down). `None` only without a doc / with
    /// a copper-less stackup.
    pub fn active_layer_name(&self) -> Option<String> {
        let copper = self.copper_layer_names();
        if let Some(name) = self.active_layer.borrow().as_ref()
            && copper.iter().any(|n| n == name)
        {
            return Some(name.clone());
        }
        copper.first().cloned()
    }

    /// Set the active routing layer (the layer panel's set-active affordance).
    /// Only copper slab names are accepted. If a route is PENDING and the layer
    /// actually changes, this drops a through-via at the route's last point and
    /// continues the pending route on the new layer (ladder level 1's "via drop
    /// on layer switch") — the via + all trace runs commit together as one undo
    /// unit when the route commits.
    pub fn set_active_layer(&self, name: &str) {
        if !self.copper_layer_names().iter().any(|n| n == name) {
            return;
        }
        // `switch_layer` compares against the ROUTE's own live-run layer, so a
        // redundant set (same layer) never drops a spurious via.
        if let Some(r) = self.route.borrow_mut().as_mut() {
            r.switch_layer(name);
        }
        *self.active_layer.borrow_mut() = Some(name.to_string());
    }

    /// Commit the pending route (the commit-on-pin click): each run with ≥ 2
    /// points lowers to an `AddTrace` (fresh ids from the doc's shared route-id
    /// allocator — [`Doc::route_id_alloc`], the same one `eutectic_core::autoroute`
    /// uses, Decision 22), each layer-switch via to an `AddVia` (through, span
    /// `None`), all in ONE `commit_edit` transaction — one undo unit. Width /
    /// drill / pad come from [`route_defaults`]. `Pinned` provenance (a hand
    /// edit). The first committed trace is selected, ready for refinement. A
    /// route with nothing committable is left pending (the click is ignored).
    pub(crate) fn commit_route(&mut self) {
        use eutectic_core::command::Command;

        let committable = self
            .route
            .borrow()
            .as_ref()
            .is_some_and(|r| r.has_committable());
        if !committable {
            return;
        }
        let route = self.route.borrow_mut().take().expect("checked above");
        let (width, drill, pad) = route_defaults();
        let (cmds, first_tid) = {
            let Ok(doc) = &self.domain.doc else {
                return;
            };
            let mut alloc = doc.route_id_alloc();
            let mut cmds = Vec::new();
            let mut first_tid = None;
            for run in &route.runs {
                if run.points.len() < 2 {
                    continue;
                }
                let tid = alloc.mint_trace();
                first_tid.get_or_insert(tid);
                cmds.push(Command::AddTrace(
                    tid,
                    eutectic_core::route::Trace {
                        net: route.net.clone(),
                        layer: run.layer.clone(),
                        path: run.points.clone(),
                        width,
                        prov: eutectic_core::doc::Provenance::Pinned,
                    },
                ));
            }
            for at in &route.vias {
                cmds.push(Command::AddVia(
                    alloc.mint_via(),
                    eutectic_core::route::Via {
                        net: route.net.clone(),
                        at: *at,
                        span: None,
                        drill,
                        pad,
                        prov: eutectic_core::doc::Provenance::Pinned,
                    },
                ));
            }
            (cmds, first_tid)
        };
        match self.commit_edit(Transaction(cmds), "draw trace") {
            Ok(()) => {
                if let Some(tid) = first_tid {
                    self.domain
                        .selection
                        .borrow_mut()
                        .select_only(crate::pick::SemanticId::Trace(tid));
                }
            }
            Err(e) => self.domain.edit.error = Some(e),
        }
    }

    /// Arm a canned pending route — for fixtures / tests that render a
    /// route-in-progress scene without driving live pointer events. Starts at
    /// the pad centre of `comp`.`pin` (the same snap the interactive start
    /// click applies) on that pin's net and the current active layer, then
    /// appends `waypoints` and sets the rubber cursor. Returns `false` when the
    /// pin has no candidate or no net.
    pub fn set_route(
        &self,
        comp: &eutectic_core::id::EntityId,
        pin: &str,
        waypoints: &[eutectic_core::coord::Point],
        cursor: Option<eutectic_core::coord::Point>,
    ) -> bool {
        use crate::pick::SemanticId;
        let id = SemanticId::Pin {
            comp: comp.clone(),
            pin: pin.to_string(),
        };
        let anchor = {
            let derived = self.derived.borrow();
            let Some(view) = &derived.board else {
                return false;
            };
            let Some(c) = view.candidates.iter().find(|c| c.id == id) else {
                return false;
            };
            eutectic_core::coord::Point {
                x: (c.aabb.0.x + c.aabb.1.x) / 2,
                y: (c.aabb.0.y + c.aabb.1.y) / 2,
            }
        };
        let Some(net) = self.candidate_net(&id) else {
            return false;
        };
        let Some(layer) = self.active_layer_name() else {
            return false;
        };
        let mut r = RouteState::start(net, layer, anchor);
        for w in waypoints {
            r.push_waypoint(*w);
        }
        if let Some(c) = cursor {
            r.hover(c);
        }
        *self.route.borrow_mut() = Some(r);
        true
    }

    /// Arm a canned trace-vertex drag on `trace`'s vertex `index`, moved to
    /// `to` — for fixtures / tests that render a refinement-in-progress scene.
    /// Returns `false` when the trace / index doesn't resolve.
    pub fn set_trace_drag(
        &self,
        trace: eutectic_core::id::TraceId,
        index: usize,
        to: eutectic_core::coord::Point,
    ) -> bool {
        let Ok(doc) = &self.domain.doc else {
            return false;
        };
        let Some(t) = doc.traces.get(&trace) else {
            return false;
        };
        if index >= t.path.len() {
            return false;
        }
        let mut d = TraceDragState {
            trace,
            path: t.path.clone(),
            index,
            width: t.width,
            start: t.path[index],
            moved: false,
            slop: 0,
        };
        d.update(to);
        *self.trace_drag.borrow_mut() = Some(d);
        true
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

/// Remove one plain authored instance and every directive that would otherwise
/// retain a dangling reference to it. Named nets are kept even when their member
/// list becomes empty so net-owned materialized routes remain valid under the
/// engine's existing semantics. Returns `None` for generated/def-expanded parts:
/// deleting one expansion would require rewriting its generator or definition.
fn delete_component_from_source(
    source: &[GenDirective],
    id: &EntityId,
) -> Option<Vec<GenDirective>> {
    let target = id.as_str();
    let mut found = false;
    let mut out = Vec::with_capacity(source.len());
    for directive in source.iter().cloned() {
        match directive {
            GenDirective::Instance { path, .. } if path == target => found = true,
            GenDirective::Place { ref path, .. }
            | GenDirective::Fix { ref path, .. }
            | GenDirective::Rotate { ref path, .. }
                if path == target => {}
            GenDirective::Near { ref a, ref b, .. } | GenDirective::MinSep { ref a, ref b, .. }
                if a == target || b == target => {}
            GenDirective::NearPin {
                ref a, ref b_comp, ..
            } if a == target || b_comp == target => {}
            GenDirective::ConnectInterface { ref a, ref b } if a.0 == target || b.0 == target => {}
            GenDirective::ConnectPins { net, mut pins } => {
                pins.retain(|(comp, _)| comp != target);
                out.push(GenDirective::ConnectPins { net, pins });
            }
            GenDirective::NoConnect { mut pins } => {
                pins.retain(|(comp, _)| comp != target);
                if !pins.is_empty() {
                    out.push(GenDirective::NoConnect { pins });
                }
            }
            GenDirective::AlignX { mut nodes } => {
                nodes.retain(|node| node != target);
                if nodes.len() >= 2 {
                    out.push(GenDirective::AlignX { nodes });
                }
            }
            GenDirective::AlignY { mut nodes } => {
                nodes.retain(|node| node != target);
                if nodes.len() >= 2 {
                    out.push(GenDirective::AlignY { nodes });
                }
            }
            other => out.push(other),
        }
    }
    found.then_some(out)
}

/// Remove a component's symbol and presentational wires from the authored
/// schematic tree when the same plain instance is deleted on the board.
fn prune_schematic_component(nodes: &mut Vec<eutectic_core::schematic::LayoutNode>, target: &str) {
    use eutectic_core::schematic::LayoutNode;
    nodes.retain_mut(|node| match node {
        LayoutNode::Container(container) => {
            prune_schematic_component(&mut container.children, target);
            true
        }
        LayoutNode::Symbol(symbol) => symbol.path != target,
        LayoutNode::Wire(wire) => wire.a.comp != target && wire.b.comp != target,
        LayoutNode::Comment(_) | LayoutNode::Blank => true,
    });
}
