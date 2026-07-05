//! Event routing + the [`App`] impl — the interactive half of the app shell:
//! `build` (root tree + Libraries overlay), `before_build` (mailbox drain + initial
//! fit), `on_event` (the whole route table: resize handle, Libraries menu, layout /
//! view / maximize toggles, findings, explorer, tools, framing, layer switches, and
//! pane pointer dispatch), and the per-pane pointer handlers. Split out of `app.rs`
//! as pure code motion.

use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::pane::{
    CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, FINDINGS_TOGGLE_KEY, LAYOUT_TOGGLE_KEY, REDO_KEY,
    SAVE_KEY, SPLIT_HANDLE_KEY, SPLIT_ROW_KEY, UNDO_KEY, finding_index_of_key, is_canvas_target,
    is_findings_chip_key, pane_index, switch_key,
};
use crate::app::panels::error_card;
use crate::app::{EcadApp, PaneId, PaneLayout, ViewKind};
use crate::canvas::pick::{self, SemanticId};
use crate::reload::SourceMsg;
use crate::schematic_view::SchematicView;
use crate::tool::{DragState, MeasureState, Tool};
use damascene_core::prelude::*;
use ecad_core::command::{Command, Transaction};
use ecad_core::coord::{Nm, Point};
use ecad_core::id::EntityId;

/// The pick grab radius in screen (logical) px — converted to a board distance
/// through the current zoom by [`pick::tolerance_nm`], so the on-screen radius is
/// zoom-independent.
const PICK_TOL_PX: f32 = 6.0;

impl App for EcadApp {
    fn build(&self, cx: &BuildCx) -> El {
        let main = match &self.domain.doc {
            // A loaded doc renders the two-pane viewer (at least one pane always shows
            // something — board or schematic). Even a board-only or schematic-only doc
            // gets panes; the empty side shows a placeholder.
            Ok(_) => page([self.viewer_body(cx)]),
            Err(message) => {
                let chrome = toolbar([
                    toolbar_title("ecad"),
                    badge("no document").muted(),
                    spacer(),
                    button("Libraries").key(LIBRARIES_TOGGLE_KEY),
                ])
                .gap(tokens::SPACE_2)
                .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2));
                page([column([chrome, error_card(message)])
                    .gap(tokens::SPACE_4)
                    .height(Size::Fill(1.0))])
            }
        };
        // The Libraries menu floats over whichever body rendered (it must be
        // reachable even with no document — that is when you register libs).
        overlays(
            main,
            [self.libraries_open.get().then(|| self.libraries_modal())],
        )
    }

    fn before_build(&mut self) {
        // (m5/m6) Drain the live-source mailbox first: a disk change is routed by the
        // save model BEFORE this frame builds (echo of our own save → consumed; clean
        // doc → auto-apply reload; dirty doc → conflict banner, never silent
        // last-writer). The drain coalesces a burst to the latest source.
        if let Some(SourceMsg::Changed(source)) = self.mailbox.drain() {
            self.handle_disk_change(source);
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

        // Libraries menu (slice 2): the toolbar toggle opens/closes; while open,
        // the modal's own controls (inputs / add / remove / close / scrim) are
        // handled first — everything else sits behind the scrim.
        if event.is_click_or_activate(LIBRARIES_TOGGLE_KEY) {
            self.set_libraries_open(!self.libraries_open.get());
            return;
        }
        if self.libraries_open.get() && self.handle_libraries_event(&event) {
            return;
        }

        // Editing actions (m6): the Save / Undo / Redo toolbar buttons and their
        // hotkey twins (Ctrl+S / Ctrl+Z / Ctrl+Shift+Z or Ctrl+Y — registered in
        // `hotkeys()`, delivered as `UiEventKind::Hotkey` with the same action
        // names). Suppressed while the Libraries modal is open: its text inputs
        // own the keyboard, and a doc-level undo under a typing user would be a
        // surprise (the buttons sit behind the scrim anyway).
        if !self.libraries_open.get() {
            if event.is_click_or_activate(SAVE_KEY) || event.is_hotkey(SAVE_KEY) {
                self.save();
                return;
            }
            if event.is_click_or_activate(UNDO_KEY) || event.is_hotkey(UNDO_KEY) {
                self.undo();
                return;
            }
            if event.is_click_or_activate(REDO_KEY) || event.is_hotkey(REDO_KEY) {
                self.redo();
                return;
            }
        }

        // The conflict banner's two explicit actions (m6 save model): Reload
        // (discard my edits, apply disk) and Keep mine (dismiss; stay dirty).
        if event.is_click_or_activate(CONFLICT_RELOAD_KEY) {
            self.conflict_reload();
            return;
        }
        if event.is_click_or_activate(CONFLICT_KEEP_KEY) {
            self.conflict_keep();
            return;
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
        // A toolbar findings chip (any per-source chip, or the ✓ chip) toggles the panel
        // exactly like the collapse toggle.
        if event.is_click_or_activate(FINDINGS_TOGGLE_KEY)
            || (matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
                && event.route().is_some_and(is_findings_chip_key))
        {
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

        // Escape: cancel an in-flight drag first (preview discarded, nothing
        // committed — m6), then a measure in progress, else clear the selection.
        if event.kind == UiEventKind::Escape {
            if self.drag.borrow().is_some() {
                *self.drag.borrow_mut() = None;
                return;
            }
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

        // A pointer-up with a drag in flight ALWAYS finishes the drag (commit if
        // moved), even when the release lands outside the pane rect — otherwise a
        // release over the chrome would strand the ghost. The drag's own last
        // in-pane cursor is the drop point in that case.
        if event.kind == UiEventKind::PointerUp && self.drag.borrow().is_some() {
            self.finish_drag();
            return;
        }

        // Canvas pointer interaction. A pointer event over a pane's canvas routes to the
        // pane's viewport / a stacked layer / overlay El — all canvas targets. THE CLICKED
        // PANE is resolved by testing the pointer against each pane's laid-out rect (NOT a
        // global key — this is where m2's coordinate-composition bug class returns; every
        // unproject / rect uses the clicked pane's own key). The target key falls back to
        // the route (identical for real pointer events, where `key` IS the target key) so
        // headless tests can synthesize pointer events without a `UiTarget` (the struct is
        // `#[non_exhaustive]` and cannot be built outside damascene).
        if !is_canvas_target(event.target_key().or_else(|| event.route())) {
            return;
        }
        let Some(pos) = event.pointer_pos() else {
            return;
        };
        let Some(pane) = self.pane_under_pointer(cx, pos) else {
            return;
        };
        let view = self.panes.borrow()[pane_index(pane)].view;
        match view {
            ViewKind::Board => self.handle_board_pointer(event, cx, pane, pos),
            ViewKind::Schematic => self.handle_schematic_pointer(event, cx, pane, pos),
        }
    }

    fn drain_viewport_requests(&mut self) -> Vec<ViewportRequest> {
        std::mem::take(&mut self.pending.borrow_mut())
    }

    /// The app-wide keyboard chords (m6): Save / Undo / Redo, delivered by
    /// damascene 0.4.5 as [`UiEventKind::Hotkey`] events whose route is the
    /// registered action name — the same names as the toolbar button keys, so
    /// `on_event` handles both through one arm. Redo binds both the Ctrl+Shift+Z
    /// and Ctrl+Y conventions.
    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        vec![
            (KeyChord::ctrl('s'), SAVE_KEY.to_string()),
            (KeyChord::ctrl('z'), UNDO_KEY.to_string()),
            (KeyChord::ctrl_shift('z'), REDO_KEY.to_string()),
            (KeyChord::ctrl('y'), REDO_KEY.to_string()),
        ]
    }

    /// The app's current text selection — the Libraries menu's inputs are the
    /// only text fields, so their shared [`Selection`] is the app's (the host
    /// reads this once per frame to paint highlight bands / resolve clipboard).
    fn selection(&self) -> Selection {
        self.lib_ui.borrow().selection.clone()
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

    /// Handle a pointer event over a board pane: cursor readout, pick / hover /
    /// measure / component drag (m6) — all through THE CLICKED PANE's canvas key +
    /// rect + viewport view. `&mut self` because a drag commit mutates domain +
    /// derived state; the `derived` borrow is scoped so the commit path can
    /// re-borrow.
    fn handle_board_pointer(
        &mut self,
        event: UiEvent,
        cx: &EventCx,
        pane: PaneId,
        pos: (f32, f32),
    ) {
        // Scope the derived borrow: map the pointer into board space and pre-resolve
        // what the drag-capable arms need, then drop the borrow before any commit.
        let (p, tol) = {
            let derived = self.derived.borrow();
            let Some(view) = &derived.board else {
                return;
            };
            let key = pane.canvas_key();
            let (Some(rect), Some(vv)) = (cx.rect_of_key(key), cx.viewport_view(key)) else {
                return;
            };
            // The asset's honest stretch rect: the vector child is laid out at
            // natural (viewBox) size in the viewport, NOT stretched to the pane.
            let el_rect = view.canvas.content_rect((rect.x, rect.y, rect.w, rect.h));

            let content_px = vv.unproject(pos, (rect.x, rect.y));
            if let Some(mm) = view.canvas.content_px_to_board_mm(content_px, el_rect) {
                self.cursor_board_mm.set(Some(mm));
            }

            let Some(p) = pick::pointer_to_board_nm(&view.canvas, pos, el_rect, vv) else {
                return;
            };
            (p, pick::tolerance_nm(PICK_TOL_PX, vv.zoom))
        };

        match (self.tool.get(), event.kind) {
            (Tool::Select, UiEventKind::PointerDown) => {
                // A fresh press can never inherit a stale eaten-click flag.
                self.suppress_click.set(false);
                self.begin_drag(pane, p, tol);
            }
            (Tool::Select, UiEventKind::Click) => {
                // The trailing Click of a just-committed drag: consumed (the drag
                // was the interaction; re-selecting whatever sits under the drop
                // point would fight it).
                if self.suppress_click.replace(false) {
                    return;
                }
                let derived = self.derived.borrow();
                let Some(view) = &derived.board else {
                    return;
                };
                let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.select_only(pick.id),
                    None => sel.clear(),
                }
            }
            (Tool::Select, UiEventKind::Drag) => {
                // An in-flight component drag consumes pointer movement (the ghost
                // tracks it); otherwise drag-over is a hover cue, as before.
                let mut drag = self.drag.borrow_mut();
                if let Some(d) = drag.as_mut() {
                    d.update(p);
                    return;
                }
                drop(drag);
                let derived = self.derived.borrow();
                let Some(view) = &derived.board else {
                    return;
                };
                let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.hover_only(pick.id),
                    None => sel.clear_hover(),
                }
            }
            (Tool::Select, UiEventKind::PointerEnter) => {
                let derived = self.derived.borrow();
                let Some(view) = &derived.board else {
                    return;
                };
                let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.hover_only(pick.id),
                    None => sel.clear_hover(),
                }
            }
            (Tool::Select, UiEventKind::PointerUp) => {
                // Reached only when no drag is in flight (the on_event fast path
                // finishes an active drag before pane resolution); nothing to do.
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

    /// Pointer-down on the board with the Select tool (m6): if the pick resolves
    /// to a component's pad (or the component itself), arm a [`DragState`] for
    /// that component. Anything else (trace / via / pour / empty board) arms
    /// nothing — the interaction stays a plain click-select.
    fn begin_drag(&self, pane: PaneId, p: Point, tol: Nm) {
        let comp = {
            let derived = self.derived.borrow();
            let Some(view) = &derived.board else {
                return;
            };
            match pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id)) {
                Some(pick::Pick {
                    id: SemanticId::Pin { comp, .. },
                    ..
                }) => comp,
                Some(pick::Pick {
                    id: SemanticId::Part(comp),
                    ..
                }) => comp,
                _ => return,
            }
        };
        if let Some(drag) = self.make_drag(comp, pane, p, tol) {
            *self.drag.borrow_mut() = Some(drag);
        }
    }

    /// Build a [`DragState`] for `comp`: capture the component's doc position, its
    /// pad shapes, and the ratsnest input (own pad centers + the other member pad
    /// centers of each net) — all from the **cached** pick candidates + doc maps,
    /// so nothing here (or in any later per-event update) calls the geometry
    /// kernel. `None` when the component has no pad candidates (nothing to ghost).
    pub(crate) fn make_drag(
        &self,
        comp: EntityId,
        pane: PaneId,
        start: Point,
        slop: Nm,
    ) -> Option<DragState> {
        let doc = self.domain.doc.as_ref().ok()?;
        let orig_pos = doc.components.get(&comp)?.pos.value;
        let derived = self.derived.borrow();
        let view = derived.board.as_ref()?;

        // Pad centers for every candidate pad on the board, keyed by (comp, pad
        // number) — the AABB midpoint is the honest cheap center (the AABB was
        // derived from the pad's tessellated region at candidate build time). A
        // multi-layer pad yields one candidate per layer; first wins (same center).
        let mut centers: std::collections::BTreeMap<(&EntityId, &str), Point> =
            std::collections::BTreeMap::new();
        let mut shapes: Vec<ecad_core::geom::Shape2D> = Vec::new();
        for c in &view.candidates {
            if let SemanticId::Pin { comp: cc, pin } = &c.id {
                let center = Point {
                    x: (c.aabb.0.x + c.aabb.1.x) / 2,
                    y: (c.aabb.0.y + c.aabb.1.y) / 2,
                };
                if *cc == comp {
                    shapes.push(c.shape.clone());
                }
                centers.entry((cc, pin.as_str())).or_insert(center);
            }
        }
        if shapes.is_empty() {
            return None;
        }

        // Ratsnest input: for each net, the dragged component's member pad centers
        // vs every OTHER member pad center. Netless pads contribute nothing.
        let mut pins: Vec<(Point, Vec<Point>)> = Vec::new();
        for net in doc.nets.values() {
            let mut mine: Vec<Point> = Vec::new();
            let mut others: Vec<Point> = Vec::new();
            for m in &net.members {
                let Some(center) = centers.get(&(&m.comp, m.pin.as_str())) else {
                    continue; // an unplaced / suppressed member has no candidate
                };
                if m.comp == comp {
                    mine.push(*center);
                } else {
                    others.push(*center);
                }
            }
            if !others.is_empty() {
                for c in mine {
                    pins.push((c, others.clone()));
                }
            }
        }

        Some(DragState {
            comp,
            pane,
            start,
            cursor: start,
            orig_pos,
            moved: false,
            slop,
            shapes,
            pins,
        })
    }

    /// Pointer-up with a drag in flight: a **moved** drag commits the component
    /// move as `Command::Pin(comp, orig_pos + delta)` — a hard placement, "the
    /// user dragged it exactly here" — through the command-commit path (derived
    /// caches rebuild, revision bumps, dirty set). Per the permissive philosophy
    /// there is NO rejection path: a DRC-violating drop commits fine and the
    /// violations surface as findings. An un-moved press-release just disarms
    /// (the trailing Click stays a plain select). The moved component is left
    /// selected.
    fn finish_drag(&mut self) {
        let Some(drag) = self.drag.borrow_mut().take() else {
            return;
        };
        if !drag.moved {
            return;
        }
        // Eat the trailing Click of this press (PointerUp fires first).
        self.suppress_click.set(true);
        let target = drag.target_pos();
        let comp = drag.comp.clone();
        match self.commit_edit(
            Transaction::one(Command::Pin(comp.clone(), target)),
            "move component",
        ) {
            Ok(()) => {
                self.domain
                    .selection
                    .borrow_mut()
                    .select_only(SemanticId::Part(comp));
            }
            Err(e) => self.domain.edit.error = Some(e),
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
        // Same natural-size layout fact as the board path: map through the
        // asset's honest content rect, not the pane's viewport rect.
        let el_rect = view.content_rect((rect.x, rect.y, rect.w, rect.h));
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
