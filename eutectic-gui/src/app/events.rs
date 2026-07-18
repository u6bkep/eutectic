//! Event routing + the [`App`] impl — the interactive half of the app shell:
//! `build` (root tree + Libraries overlay), `before_build` (mailbox drain + initial
//! fit), and `on_event` (the whole route table: resize handle, Libraries menu,
//! split / close / view / maximize actions, findings, explorer, tools, framing, layer
//! switches, and pane pointer dispatch). Split out of `app.rs` as pure code
//! motion; the per-pane pointer handlers live in the [`pointer`] submodule.

mod pointer;

pub(crate) use pointer::PICK_TOL_PX;

use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::open::{OPEN_RECENT_KEY, RECENT_POPOVER_KEY, recent_item_index};
use crate::app::pane::{
    CLOSE_PANE_KEY, CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, REDO_KEY, SAVE_KEY, SPLIT_DOWN_KEY,
    SPLIT_RIGHT_KEY, SidebarSection, UNDO_KEY, active_layer_of_key, finding_index_of_key,
    is_canvas_target, is_findings_chip_key, pane_index, section_of_key, strip_target_of_key,
    switch_key,
};
use crate::app::{EutecticApp, SplitAxis, ViewKind};
use crate::chrome::actions::{ZOOM_IN_KEY, ZOOM_OUT_KEY};
use crate::chrome::menubar::{MENUBAR_KEY, REVERT_KEY, SNAP_TO_GRID_KEY};
use crate::palette::PALETTE_TOGGLE_KEY;
use crate::panels::findings::error_card;
use crate::reload::SourceMsg;
use crate::tool::Tool;
use damascene_core::prelude::*;

impl App for EutecticApp {
    fn build(&self, cx: &BuildCx) -> El {
        // Reset the captured per-pane rects: only panes actually built this
        // frame re-register (a maximized-away or view-switched board pane
        // must not leave a stale rect for the paint pass / raw hover).
        self.pane_px
            .borrow_mut()
            .iter_mut()
            .for_each(|rect| *rect = None);
        self.strip_px
            .borrow_mut()
            .iter_mut()
            .for_each(|rect| *rect = None);
        let main = match &self.domain.doc {
            // A loaded doc renders the pane-tree viewer (at least one pane always shows
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
                self.recent_menu_overlay(),
                self.pane_view_overlay(),
                self.palette_open.get().then(|| self.palette_modal()),
            ],
        )
    }

    fn before_build(&mut self) {
        // (m5/m6) Drain the live-source mailbox first: a disk change is routed by the
        // save model BEFORE this frame builds (echo of our own save → consumed; clean
        // doc → auto-apply reload; dirty doc → conflict banner, never silent
        // last-writer). The drain coalesces a burst to the latest source.
        if let Some(SourceMsg::Changed { path, source }) = self.mailbox.drain()
            && path.as_deref() == self.domain.source_path.as_deref()
        {
            self.handle_disk_change(source);
        }
        self.drain_open_mailbox();

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
        // Ctrl+K / the toolbar button toggles the command palette. While open,
        // it owns all routed input (including Escape) ahead of document tools.
        if self.handle_palette_event(&event) {
            return;
        }

        // The library browser is a palette-like dock, not a modal: it gets
        // first refusal for its filter/rows, while unrelated canvas/chrome
        // events continue through the normal route table.
        if event.kind != UiEventKind::Escape && self.handle_library_browser_event(&event) {
            return;
        }

        // Pane view selects are controlled dropdowns. Their menus live at the
        // root overlay, while the trigger stays inside its leaf header.
        for pane in self.pane_ids() {
            let select_key = pane.view_select_key();
            if event.is_click_or_activate(&select_key) {
                self.pane_view_menu.set(match self.pane_view_menu.get() {
                    Some(open) if open == pane => None,
                    _ => Some(pane),
                });
                return;
            }
            if event.is_click_or_activate(&format!("{select_key}:dismiss")) {
                self.pane_view_menu.set(None);
                return;
            }
            for view in ViewKind::all() {
                if event.is_click_or_activate(&pane.switch_key(view)) {
                    let mut panes = self.panes.borrow_mut();
                    let state = panes[pane_index(pane)].as_mut().expect("live pane");
                    if state.view != view {
                        if self.measure_pane.get() == pane {
                            self.measure.set(Default::default());
                        }
                        state.view = view;
                        state.fitted = false;
                    }
                    self.pane_view_menu.set(None);
                    self.focused_pane.set(pane);
                    return;
                }
            }
        }
        if event.kind == UiEventKind::Escape && self.pane_view_menu.get().is_some() {
            self.pane_view_menu.set(None);
            return;
        }

        // Every internal node owns an independent weighted divider. Its
        // container key supplies the nested extent in the same layout frame.
        let split_ids = self.pane_tree.borrow().split_ids();
        for id in split_ids {
            let mut tree = self.pane_tree.borrow_mut();
            let Some((axis, weights, drag)) = tree.root.split_mut(id) else {
                continue;
            };
            let Some(rect) = cx.rect_of_key(id.container_key()) else {
                continue;
            };
            let extent = match axis {
                SplitAxis::Horizontal => rect.w,
                SplitAxis::Vertical => rect.h,
            };
            if resize_handle::apply_event_weights(
                weights,
                drag,
                &event,
                id.handle_key(),
                axis.damascene(),
                extent,
                0.15,
            ) {
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
                self.recent_open.set(false);
                return;
            }
            if open.as_deref() == Some("file") && event.is_click_or_activate(OPEN_RECENT_KEY) {
                self.recent_open.set(true);
                return;
            }
            if event.is_click_or_activate(&format!("{RECENT_POPOVER_KEY}:dismiss")) {
                self.recent_open.set(false);
                return;
            }
            if event.kind == UiEventKind::Escape && open.is_some() {
                *open = None;
                self.recent_open.set(false);
                return;
            }
            if open.as_deref() == Some("file")
                && matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
                && let Some(index) = event.route().and_then(recent_item_index)
            {
                *open = None;
                self.recent_open.set(false);
                drop(open);
                self.request_recent(index);
                return;
            }
            if open.is_some() && matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
                *open = None;
                self.recent_open.set(false);
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

        if event.is_click_or_activate(SNAP_TO_GRID_KEY) {
            self.toggle_snap_to_grid();
            return;
        }

        // Convention exports, focused zoom, display/grid toggles, Help
        // dialogs, and Quit share one chrome-owned dispatch seam.
        if self.handle_chrome_event(&event) {
            return;
        }

        if self.handle_explorer_filter_event(&event) {
            return;
        }

        // Editable Properties plus the Delete/Rotate actions share one
        // direct-manipulation dispatch hook. A field blur deliberately returns
        // false so the click that caused it continues.
        if !self.libraries_open.get()
            && !self.palette_open.get()
            && self.handle_direct_manip_event(&event)
        {
            return;
        }

        // Editing actions (m6): the Save / Undo / Redo toolbar buttons and their
        // hotkey twins (Ctrl+S / Ctrl+Z / Ctrl+Shift+Z or Ctrl+Y — registered in
        // `hotkeys()`, delivered as `UiEventKind::Hotkey` with the same action
        // names). Suppressed while the Libraries modal is open: its text inputs
        // own the keyboard, and a doc-level undo under a typing user would be a
        // surprise (the buttons sit behind the scrim anyway).
        if !self.libraries_open.get()
            && !self.palette_open.get()
            && self.chrome_dialog.get().is_none()
            && self.open_menu.borrow().is_none()
            && self.pane_view_menu.get().is_none()
        {
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

        // View-menu split/close commands act on the focused leaf.
        if event.is_click_or_activate(SPLIT_RIGHT_KEY) {
            self.split_pane(self.focused_pane.get(), SplitAxis::Horizontal);
            return;
        }
        if event.is_click_or_activate(SPLIT_DOWN_KEY) {
            self.split_pane(self.focused_pane.get(), SplitAxis::Vertical);
            return;
        }
        if event.is_click_or_activate(CLOSE_PANE_KEY) {
            self.close_pane(self.focused_pane.get());
            return;
        }

        // Per-pane header actions.
        for pane in self.pane_ids() {
            if event.is_click_or_activate(&pane.split_right_key()) {
                self.split_pane(pane, SplitAxis::Horizontal);
                return;
            }
            if event.is_click_or_activate(&pane.split_down_key()) {
                self.split_pane(pane, SplitAxis::Vertical);
                return;
            }
            if event.is_click_or_activate(&pane.close_key()) {
                self.close_pane(pane);
                return;
            }
            if event.is_click_or_activate(&pane.maximize_key()) {
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
            let kind = self.pane_view(pane);
            if kind.offers_tool(tool) {
                self.set_tool(kind, tool);
                self.focused_pane.set(pane);
                if kind == ViewKind::Board && tool == Tool::Place {
                    self.open_library_browser();
                }
            }
            return;
        }

        // Place ▸ Part from Library… — the menu door to the same state the
        // strip's Place button sets (the route table closed the menu already).
        if event.is_click_or_activate(crate::chrome::menubar::PLACE_PART_KEY) {
            self.set_tool(ViewKind::Board, Tool::Place);
            self.open_library_browser();
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

        // Escape: cancel an in-flight preview first — camera pan, component
        // drag, trace-vertex drag, then a pending route (preview discarded,
        // nothing committed — m6); with the Route tool idle, Esc exits the
        // board kind's slot back to Select. Next cancel a measurement owned by
        // the focused pane's view kind; otherwise clear the selection. A focused
        // inspector input consumes its routed Escape before this cascade.
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
                self.route_pane.set(None);
                return;
            }
            if self.tool_for(ViewKind::Board) == Tool::Place && self.armed_part.borrow().is_some() {
                self.disarm_part();
                return;
            }
            if self.tool_for(ViewKind::Board) == Tool::Route {
                self.set_tool(ViewKind::Board, Tool::Select);
                return;
            }
            let focused = self.focused_pane.get();
            let focused_kind = self.pane_view(focused);
            let measure_kind = self.pane_view(self.measure_pane.get());
            let mut m = self.measure.get();
            if measure_kind == focused_kind
                && self.tool_for(focused_kind) == Tool::Measure
                && m.segment().is_some()
            {
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
            for id in self.pane_ids() {
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
        let view = self.pane_view(pane);
        if view == ViewKind::Board && self.tool_for(ViewKind::Board) == Tool::Place {
            let Some(rect) = cx.rect_of_key(pane.canvas_key()) else {
                return;
            };
            let point = crate::app::canvas_pane::pane_unproject(
                &self.pane_camera(pane),
                (rect.x, rect.y, rect.w, rect.h),
                pos,
            );
            // Placement respects snap-to-grid like the other commit gestures;
            // ghost and commit share the snapped point (what you see is what
            // commits).
            let point = match self.snap_to_grid().then(|| self.displayed_grid_pitch(pane)) {
                Some(pitch) => crate::app::snap_point(point, pitch),
                None => point,
            };
            self.hover_place_part(pane, point);
            if event.kind == UiEventKind::Click {
                self.commit_armed_part(point);
            }
            return;
        }
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
        // wheel over the Libraries modal, a Help dialog, the command palette,
        // or an open menu must scroll the chrome, never zoom the pane beneath
        // it (the palette stamps the open_menu sentinel, but gate explicitly).
        if self.libraries_open.get()
            || self.chrome_dialog.get().is_some()
            || self.palette_open.get()
            || self.open_menu.borrow().is_some()
            || self.pane_view_menu.get().is_some()
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
    /// and Ctrl+Y conventions. Bare Delete/R stay out of this table so focused
    /// text inputs receive them.
    fn hotkeys(&self) -> Vec<(KeyChord, String)> {
        vec![
            (KeyChord::ctrl('s'), SAVE_KEY.to_string()),
            (KeyChord::ctrl('o'), crate::app::open::OPEN_KEY.to_string()),
            (KeyChord::ctrl('z'), UNDO_KEY.to_string()),
            (KeyChord::ctrl_shift('z'), REDO_KEY.to_string()),
            (KeyChord::ctrl('y'), REDO_KEY.to_string()),
            (KeyChord::ctrl('+'), ZOOM_IN_KEY.to_string()),
            (KeyChord::ctrl('='), ZOOM_IN_KEY.to_string()),
            (KeyChord::ctrl('-'), ZOOM_OUT_KEY.to_string()),
            (KeyChord::ctrl('k'), PALETTE_TOGGLE_KEY.to_string()),
        ]
    }

    /// The app's current text selection, arbitrated by which surface owns the
    /// keyboard: the palette input while the palette is open, the Libraries
    /// inputs while that modal is open, else the Explorer filter's adopted
    /// selection (the host reads this once per frame to paint highlight bands /
    /// resolve clipboard).
    fn selection(&self) -> Selection {
        if self.palette_open.get() {
            return self.palette_ui.borrow().selection.clone();
        }
        if self.libraries_open.get() {
            return self.lib_ui.borrow().selection.clone();
        }
        if self.library_browser_open.get() {
            let browser = self.library_browser_ui.borrow();
            if browser.selection.range.is_some() {
                return browser.selection.clone();
            }
        }
        let explorer = self.explorer_filter_selection.borrow();
        if explorer.range.is_some() {
            explorer.clone()
        } else {
            self.lib_ui.borrow().selection.clone()
        }
    }

    fn drain_focus_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.focus_requests.borrow_mut())
    }
}
