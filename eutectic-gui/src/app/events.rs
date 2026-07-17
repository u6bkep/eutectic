//! Event routing + the [`App`] impl — the interactive half of the app shell:
//! `build` (root tree + Libraries overlay), `before_build` (mailbox drain + initial
//! fit), and `on_event` (the whole route table: resize handle, Libraries menu,
//! layout / view / maximize toggles, findings, explorer, tools, framing, layer
//! switches, and pane pointer dispatch). Split out of `app.rs` as pure code
//! motion; the per-pane pointer handlers live in the [`pointer`] submodule.

mod pointer;

pub(crate) use pointer::PICK_TOL_PX;

use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::pane::{
    CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, LAYOUT_TOGGLE_KEY, REDO_KEY, SAVE_KEY,
    SPLIT_HANDLE_KEY, SPLIT_ROW_KEY, SidebarSection, UNDO_KEY, active_layer_of_key,
    finding_index_of_key, is_canvas_target, is_findings_chip_key, pane_index, section_of_key,
    strip_target_of_key, switch_key,
};
use crate::app::{EutecticApp, PaneId, PaneLayout, ViewKind};
use crate::chrome::actions::{ZOOM_IN_KEY, ZOOM_OUT_KEY};
use crate::chrome::menubar::{MENUBAR_KEY, REVERT_KEY};
use crate::panels::findings::error_card;
use crate::reload::SourceMsg;
use crate::tool::Tool;
use damascene_core::prelude::*;

impl App for EutecticApp {
    fn build(&self, cx: &BuildCx) -> El {
        // Reset the captured per-pane rects: only panes actually built this
        // frame re-register (a maximized-away or view-switched board pane
        // must not leave a stale rect for the paint pass / raw hover).
        self.pane_px.set([None, None]);
        self.strip_px.set([None, None]);
        let main = match &self.domain.doc {
            // A loaded doc renders the two-pane viewer (at least one pane always shows
            // something — board or schematic). Even a board-only or schematic-only doc
            // gets panes; the empty side shows a placeholder.
            Ok(_) => page([self.viewer_body(cx)]),
            Err(message) => {
                let chrome = toolbar([
                    toolbar_title("eutectic"),
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
        // reachable even with no document — that is when you register libs). The
        // open menu-bar menu (if any) stacks as its own anchored overlay.
        overlays(
            main,
            [
                self.libraries_open.get().then(|| self.libraries_modal()),
                self.chrome_dialog_overlay(),
                self.menu_overlay(),
            ],
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

        // Camera settlement note (WP3, owned canvas everywhere): the initial
        // fit-to-content for BOTH view kinds is app-camera math applied in
        // `build` where each pane's laid-out rect is known
        // (`pane_build_camera` consumes the per-pane `fitted` flag; hidden
        // panes stay un-fitted and fit on their first visible frame). The
        // old damascene viewport-request queue is gone. Reload NEVER re-fits
        // (the user's framing is sacred): `apply_reload` leaves the `fitted`
        // flags alone.
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        // Help dialogs are modal and own all input until dismissed.
        if self.handle_chrome_dialog_event(&event) {
            return;
        }
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

        // Menu bar (oracle region 1): fold top-level trigger clicks + the
        // click-outside scrim into the open-menu slot. A click on an open menu's
        // ROW is not a trigger, so `apply_event` returns false and leaves the slot
        // alone — we close the menu here and fall through so the row's wired action
        // (Save / Undo / Fit / Libraries / …, keyed with its existing route)
        // dispatches through its handler below, exactly like the retired button.
        {
            let mut open = self.open_menu.borrow_mut();
            if menubar::apply_event(&mut open, &event, MENUBAR_KEY) {
                return;
            }
            if open.is_some() && matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                *open = None;
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

        // Convention exports, focused zoom, display/grid toggles, Help
        // dialogs, and Quit share one chrome-owned dispatch seam.
        if self.handle_chrome_event(&event) {
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

        // File ▸ Revert to Saved (menu bar): reload the document from disk,
        // discarding in-memory edits (through the external-reload path).
        if event.is_click_or_activate(REVERT_KEY) {
            self.revert_to_saved();
            return;
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

        // Sidebar accordion: a header click toggles that section's body. All four
        // headers stay visible; only the open set changes.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(section) = event.route().and_then(section_of_key)
        {
            self.toggle_section(section);
            return;
        }
        // A toolbar findings chip (any per-source chip, or the ✓ chip) toggles the
        // Findings section exactly like clicking its header (preserved wiring).
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && event.route().is_some_and(is_findings_chip_key)
        {
            self.toggle_section(SidebarSection::Findings);
            return;
        }
        // A findings row click → select the finding's refs + centre the focused
        // board pane on its board point (click-to-select-and-zoom).
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

        // Per-pane tool-strip buttons (structural commitment 4, revised): a strip
        // click sets THE CLICKED PANE'S VIEW KIND's tool slot (all panes of that
        // kind follow — Blender semantics) and focuses that pane. A tool the kind
        // doesn't offer is ignored (applicability is structural — the button isn't
        // rendered, so only a synthesized event can get here). `set_tool` cancels
        // the kind's in-flight previews on a change — a preview never outlives
        // its tool.
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
            && let Some((pane, tool)) = strip_target_of_key(route)
        {
            let kind = self.panes.borrow()[pane_index(pane)].view;
            if kind.offers_tool(tool) {
                self.set_tool(kind, tool);
                self.focused_pane.set(pane);
            }
            return;
        }

        // The layer panel's set-active affordance (m6 slice B): make that copper
        // slab the active routing layer. While a route is pending this drops a
        // via at the last waypoint and continues on the new layer (the app-side
        // `set_active_layer` owns that logic).
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(route) = event.route()
            && let Some(name) = active_layer_of_key(route)
        {
            let name = name.to_string();
            self.set_active_layer(&name);
            return;
        }

        // Escape: cancel an in-flight preview first — component drag, then a
        // trace-vertex drag, then a pending route (preview discarded, nothing
        // committed — m6); with the Route tool idle, Esc exits the board kind's
        // slot back to Select; then a measure in progress; else clear the
        // selection. The Route/Measure checks key off the BOARD kind's slot —
        // every preview today is a board-pane preview, so this is the same
        // layering as before the per-kind re-keying.
        if event.kind == UiEventKind::Escape {
            if self.camera_pan.borrow().is_some() {
                // Cancel the pan gesture (the camera stays where it is — a pan
                // has no uncommitted preview to roll back). The in-flight press
                // ends as a non-select: eat its trailing Click.
                *self.camera_pan.borrow_mut() = None;
                self.suppress_click.set(true);
                return;
            }
            if self.drag.borrow().is_some() {
                *self.drag.borrow_mut() = None;
                return;
            }
            if self.trace_drag.borrow().is_some() {
                *self.trace_drag.borrow_mut() = None;
                return;
            }
            if self.route.borrow().is_some() {
                *self.route.borrow_mut() = None;
                return;
            }
            if self.tool_for(ViewKind::Board) == Tool::Route {
                self.set_tool(ViewKind::Board, Tool::Select);
                return;
            }
            let mut m = self.measure.get();
            if self.tool_for(ViewKind::Board) == Tool::Measure && m.segment().is_some() {
                m.cancel();
                self.measure.set(m);
            } else {
                self.domain.selection.borrow_mut().clear();
            }
            return;
        }

        // Toolbar framing buttons act on the pane(s) — Fit/Reset every pane's
        // camera so a single button reframes whatever the user sees. Both
        // view kinds get an app-camera request (consumed in `build` where the
        // rect is known — hidden panes apply on first show).
        if event.is_click_or_activate("fit") || event.is_click_or_activate("reset") {
            let fit = event.is_click_or_activate("fit");
            for id in [PaneId::A, PaneId::B] {
                self.request_pane_cam(
                    id,
                    if fit {
                        crate::app::canvas_pane::CamRequest::Fit
                    } else {
                        crate::app::canvas_pane::CamRequest::Reset
                    },
                );
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
                    // Layer visibility is a composite-uniform change on the
                    // owned canvas — bump the style revision so the board
                    // panes' damage keys move (spec §4: never geometry work).
                    self.style_rev.set(self.style_rev.get() + 1);
                    return;
                }
            }
        }

        // A pointer-up with a drag in flight ALWAYS finishes the drag (commit if
        // moved), even when the release lands outside the pane rect — otherwise a
        // release over the chrome would strand the ghost. The drag's own last
        // in-pane cursor is the drop point in that case. Same rule for a
        // trace-vertex drag (m6 slice B).
        if event.kind == UiEventKind::PointerUp && self.drag.borrow().is_some() {
            self.finish_drag();
            return;
        }
        if event.kind == UiEventKind::PointerUp && self.trace_drag.borrow().is_some() {
            self.finish_trace_drag();
            return;
        }
        // The Select-tool camera pan is likewise global once armed: Drag events
        // keep panning even when the pointer leaves the pane rect (matching the
        // native pan, which is "global once captured"), and PointerUp always
        // disarms — a release over the chrome must not strand the gesture.
        if self.camera_pan.borrow().is_some() {
            if event.kind == UiEventKind::Drag
                && let Some(pos) = event.pointer_pos()
            {
                self.update_camera_pan(pos);
                return;
            }
            if event.kind == UiEventKind::PointerUp {
                self.finish_camera_pan();
                return;
            }
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
        // A pointer touching a pane focuses it (Blender hover-focus): the focused
        // pane's kind's tool slot is the live tool the status bar reads out.
        self.focused_pane.set(pane);
        let view = self.panes.borrow()[pane_index(pane)].view;
        match view {
            ViewKind::Board => self.handle_board_pointer(event, cx, pane, pos),
            ViewKind::Schematic => self.handle_schematic_pointer(event, cx, pane, pos),
        }
    }

    /// Wheel over ANY canvas pane is the owned camera's zoom-at-cursor
    /// (spec §7): consume it and retarget the pane's glide so the content
    /// point under the cursor stays fixed through the whole glide. Anywhere
    /// else (scrollable chrome) returns `false` so damascene's native wheel
    /// handling proceeds unchanged.
    fn on_wheel_event(&mut self, event: UiEvent, cx: &EventCx) -> bool {
        // Modal chrome owns the pointer (the same gate free hover applies):
        // wheel over the Libraries modal or an open menu must scroll the
        // chrome, never zoom the pane beneath it.
        if self.libraries_open.get()
            || self.chrome_dialog.get().is_some()
            || self.open_menu.borrow().is_some()
        {
            return false;
        }
        let Some(pos) = event.pointer_pos() else {
            return false;
        };
        let Some(dy) = event.wheel_dy() else {
            return false;
        };
        let Some(pane) = self.pane_under_pointer(cx, pos) else {
            return false;
        };
        let Some(r) = cx.rect_of_key(pane.canvas_key()) else {
            return false;
        };
        self.focused_pane.set(pane);
        self.pane_zoom_at(pane, (r.x, r.y, r.w, r.h), pos, dy);
        true
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
            (
                KeyChord::vim('+').with_modifiers(KeyModifiers {
                    shift: true,
                    ..Default::default()
                }),
                ZOOM_IN_KEY.to_string(),
            ),
            (KeyChord::vim('-'), ZOOM_OUT_KEY.to_string()),
        ]
    }

    /// The app's current text selection — the Libraries menu's inputs are the
    /// only text fields, so their shared [`Selection`] is the app's (the host
    /// reads this once per frame to paint highlight bands / resolve clipboard).
    fn selection(&self) -> Selection {
        self.lib_ui.borrow().selection.clone()
    }
}
