//! The m6 editing engine of the app shell — the source-transition core
//! (external reload / registry-edit swap / undo / redo snapshots), the
//! command-commit path, the save model (dirty / explicit save / echo
//! suppression / conflict resolution), the Route-tool commit lowering, the
//! canned interaction-state arming helpers for fixtures, and the selection
//! prune every transition runs. Split out of `app.rs` as pure code motion
//! (gui-module-split).

use crate::app::domain::{self, DerivedCaches};
use crate::app::{EutecticApp, PaneId, route_defaults};
use crate::tool::{RouteState, TraceDragState};
use eutectic_core::command::Transaction;
use eutectic_core::doc::Doc;

impl EutecticApp {
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
