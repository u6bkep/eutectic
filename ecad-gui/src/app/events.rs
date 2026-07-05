//! Event routing + the [`App`] impl — the interactive half of the app shell:
//! `build` (root tree + Libraries overlay), `before_build` (mailbox drain + initial
//! fit), `on_event` (the whole route table: resize handle, Libraries menu, layout /
//! view / maximize toggles, findings, explorer, tools, framing, layer switches, and
//! pane pointer dispatch), and the per-pane pointer handlers. Split out of `app.rs`
//! as pure code motion.

use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::pane::{
    FINDINGS_TOGGLE_KEY, LAYOUT_TOGGLE_KEY, SPLIT_HANDLE_KEY, SPLIT_ROW_KEY, finding_index_of_key,
    is_canvas_target, is_findings_chip_key, pane_index, switch_key,
};
use crate::app::panels::error_card;
use crate::app::{EcadApp, PaneId, PaneLayout, ViewKind};
use crate::canvas::pick;
use crate::reload::SourceMsg;
use crate::schematic_view::SchematicView;
use crate::tool::{MeasureState, Tool};
use damascene_core::prelude::*;

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
                let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
                let mut sel = self.domain.selection.borrow_mut();
                match hit {
                    Some(pick) => sel.select_only(pick.id),
                    None => sel.clear(),
                }
            }
            (Tool::Select, UiEventKind::PointerEnter | UiEventKind::Drag) => {
                let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
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
