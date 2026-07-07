//! The per-pane pointer handlers — the pane-under-pointer resolution and the
//! board / schematic pointer dispatch (pick / hover / measure, the component
//! and trace-vertex drags, and the Route tool's click handling). Split out of
//! `app/events.rs` as pure code motion (gui-module-split); `on_event`'s route
//! table stays in [`events`](crate::app::events).

use crate::app::{EcadApp, PaneId};
use crate::canvas::pick::{self, SemanticId};
use crate::schematic_view::SchematicView;
use crate::tool::{self, CameraPanState, DragState, RouteState, Tool, TraceDragState};
use damascene_core::prelude::*;
use ecad_core::command::{Command, Transaction};
use ecad_core::coord::{Nm, Point};
use ecad_core::id::{EntityId, NetId};

/// The pick grab radius in screen (logical) px — converted to a board distance
/// through the current zoom by [`pick::tolerance_nm`], so the on-screen radius is
/// zoom-independent.
const PICK_TOL_PX: f32 = 6.0;

impl EcadApp {
    /// Which pane's canvas the pointer at `pos` (logical px) is inside, by testing each
    /// visible pane's laid-out canvas rect. A maximized pane is the only candidate. `None`
    /// when the pointer is over no pane canvas (chrome / gutter).
    pub(crate) fn pane_under_pointer(&self, cx: &EventCx, pos: (f32, f32)) -> Option<PaneId> {
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
    pub(crate) fn handle_board_pointer(
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

        // The tool in force over a board pane is the BOARD kind's slot (per-view-
        // kind tool memory) — the pane being handled here is a board pane.
        match (self.tool_for(crate::app::ViewKind::Board), event.kind) {
            (Tool::Select, UiEventKind::PointerDown) => {
                // A fresh press can never inherit a stale eaten-click flag —
                // nor a stale camera pan (a press whose PointerUp never arrived
                // must not hijack this press's drag through the Drag fast path).
                self.suppress_click.set(false);
                *self.camera_pan.borrow_mut() = None;
                // A press on the SELECTED trace's vertex / segment arms the
                // vertex-refinement drag (m6 slice B) — it wins over a component
                // drag (handles render on top of everything).
                if self.begin_trace_drag(p, tol) {
                    return;
                }
                self.begin_drag(pane, p, tol);
                // Anything undraggable under the press — pour, unselected trace,
                // empty board, grid furniture — arms the CAMERA PAN instead: the
                // Select-tool drag gesture always does something, and damascene's
                // native pan cannot engage here (the press hit a keyed canvas
                // child, which gates the default trigger off — see
                // `CameraPanState`). An un-moved press-release stays a click.
                if self.drag.borrow().is_none()
                    && let Some(vv) = cx.viewport_view(pane.canvas_key())
                {
                    *self.camera_pan.borrow_mut() = Some(CameraPanState {
                        pane,
                        start_px: pos,
                        start_pan: vv.pan,
                        moved: false,
                    });
                }
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
                // An in-flight trace-vertex or component drag consumes pointer
                // movement (the preview tracks it); otherwise drag-over is a
                // hover cue, as before.
                if let Some(d) = self.trace_drag.borrow_mut().as_mut() {
                    d.update(p);
                    return;
                }
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
            (Tool::Route, UiEventKind::Click) => {
                self.route_click(p, tol);
            }
            (
                Tool::Route,
                UiEventKind::PointerEnter
                | UiEventKind::PointerDown
                | UiEventKind::Drag
                | UiEventKind::PointerUp,
            ) => {
                // Rubber-segment update on every pointer event 0.4.5 delivers
                // (no free-hover pointer-move — the documented toolkit limit).
                if let Some(r) = self.route.borrow_mut().as_mut() {
                    r.hover(p);
                }
            }
            _ => {}
        }
    }

    /// A Route-tool click at board point `p` (m6 slice B, ladder level 1).
    ///
    /// No pending route: a PIN with a net starts one (anchor = the pad's centre
    /// — the engine stores trace paths as free points, so the snap is realised
    /// as the candidate AABB centre); known-net copper (trace / via / pour)
    /// starts one anchored at the snapped click point (trace → nearest point on
    /// its centreline, via → its centre, pour → the raw point). Empty space /
    /// netless copper does nothing (a trace needs a net — a data requirement,
    /// not a legality refusal).
    ///
    /// Pending: a PIN click — any pin, even one on a DIFFERENT net (permissive;
    /// overlap surfaces as DRC findings, never a block) — snaps to that pad's
    /// centre and COMMITS. Anything else appends a waypoint at the raw board
    /// position (no grid snap in v1).
    fn route_click(&mut self, p: Point, tol: Nm) {
        if self.route.borrow().is_none() {
            let Some((net, anchor)) = self.route_start_at(p, tol) else {
                return;
            };
            let Some(layer) = self.active_layer_name() else {
                return;
            };
            *self.route.borrow_mut() = Some(RouteState::start(net, layer, anchor));
            return;
        }
        match self.pin_snap_at(p, tol) {
            Some(end) => {
                let committable = {
                    let mut r = self.route.borrow_mut();
                    let r = r.as_mut().expect("pending checked above");
                    r.push_waypoint(end);
                    r.has_committable()
                };
                // A commit-click on the start pin with no waypoints has nothing
                // to commit — the route stays pending (commit_route also guards).
                if committable {
                    self.commit_route();
                }
            }
            None => {
                let mut r = self.route.borrow_mut();
                r.as_mut().expect("pending checked above").push_waypoint(p);
            }
        }
    }

    /// The pad-centre snap for a pin pick at `p`, if the winning pick is a pin:
    /// the candidate's AABB centre (the same centre the drag ghost / ratsnest
    /// use). Pure per-event lookup over the cached candidates.
    fn pin_snap_at(&self, p: Point, tol: Nm) -> Option<Point> {
        let derived = self.derived.borrow();
        let view = derived.board.as_ref()?;
        let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id))?;
        if !matches!(hit.id, SemanticId::Pin { .. }) {
            return None;
        }
        let c = view.candidates.iter().find(|c| c.id == hit.id)?;
        Some(Point {
            x: (c.aabb.0.x + c.aabb.1.x) / 2,
            y: (c.aabb.0.y + c.aabb.1.y) / 2,
        })
    }

    /// Resolve a route START click: the net the new trace belongs to and the
    /// snapped anchor point. `None` when the click hits nothing routable (empty
    /// space, or a netless pin — a trace needs a net).
    fn route_start_at(&self, p: Point, tol: Nm) -> Option<(NetId, Point)> {
        let derived = self.derived.borrow();
        let view = derived.board.as_ref()?;
        let hit = pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id))?;
        let doc = self.domain.doc.as_ref().ok()?;
        match &hit.id {
            SemanticId::Pin { .. } => {
                let net = self.candidate_net(&hit.id)?;
                let c = view.candidates.iter().find(|c| c.id == hit.id)?;
                Some((
                    net,
                    Point {
                        x: (c.aabb.0.x + c.aabb.1.x) / 2,
                        y: (c.aabb.0.y + c.aabb.1.y) / 2,
                    },
                ))
            }
            SemanticId::Trace(tid) => {
                let t = doc.traces.get(tid)?;
                Some((t.net.clone(), tool::closest_on_path(&t.path, p)?))
            }
            SemanticId::Via(vid) => {
                let v = doc.vias.get(vid)?;
                Some((v.net.clone(), v.at))
            }
            SemanticId::Pour { net, .. } => Some((net.clone(), p)),
            _ => None,
        }
    }

    /// Pointer-down with the Select tool over the single-selected trace: arm the
    /// vertex-refinement drag (m6 slice B). A press within the pick tolerance of
    /// a VERTEX drags that vertex; a press on a SEGMENT (within tolerance of the
    /// copper, i.e. tol + half width) INSERTS a new vertex at the projected
    /// point and drags it. Cheap per-event vector math against the selected
    /// trace's few points — no kernel call (event-path discipline). Returns
    /// whether a drag was armed.
    fn begin_trace_drag(&self, p: Point, tol: Nm) -> bool {
        let Ok(doc) = &self.domain.doc else {
            return false;
        };
        let tid = match self.domain.selection.borrow().single() {
            Some(SemanticId::Trace(t)) => *t,
            _ => return false,
        };
        let Some(trace) = doc.traces.get(&tid) else {
            return false;
        };
        let (index, path) = if let Some(i) = tool::hit_vertex(&trace.path, p, tol) {
            (i, trace.path.clone())
        } else if let Some((i, q)) = tool::hit_segment(&trace.path, p, tol + trace.width / 2) {
            let mut path = trace.path.clone();
            path.insert(i, q);
            (i, path)
        } else {
            return false;
        };
        *self.trace_drag.borrow_mut() = Some(TraceDragState {
            trace: tid,
            path,
            index,
            width: trace.width,
            start: p,
            moved: false,
            slop: tol,
        });
        true
    }

    /// Pointer-up with a trace-vertex drag in flight: a **moved** drag commits
    /// the updated path as `RemoveTrace + AddTrace` under the SAME `TraceId` in
    /// one transaction — the engine has no update-trace-path command, so the
    /// replace is the disclosed workaround; the stable id keeps the selection
    /// alive through the commit. `Pinned` provenance (the refined path is
    /// user-authored). An un-moved press-release just disarms — a plain click
    /// (and an inserted-but-never-dragged vertex is discarded uncommitted).
    pub(crate) fn finish_trace_drag(&mut self) {
        let Some(d) = self.trace_drag.borrow_mut().take() else {
            return;
        };
        if !d.moved {
            return;
        }
        // Eat the trailing Click of this press (PointerUp fires first).
        self.suppress_click.set(true);
        let new_trace = {
            let Ok(doc) = &self.domain.doc else {
                return;
            };
            let Some(orig) = doc.traces.get(&d.trace) else {
                return;
            };
            ecad_core::route::Trace {
                net: orig.net.clone(),
                layer: orig.layer.clone(),
                path: d.path.clone(),
                width: orig.width,
                prov: ecad_core::doc::Provenance::Pinned,
            }
        };
        let txn = Transaction(vec![
            Command::RemoveTrace(d.trace),
            Command::AddTrace(d.trace, new_trace),
        ]);
        if let Err(e) = self.commit_edit(txn, "edit trace path") {
            self.domain.edit.error = Some(e);
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
    pub(crate) fn finish_drag(&mut self) {
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

    /// Drive an in-flight camera pan from a live pointer position: fold the
    /// screen delta in and, once past the click slop, queue the pan as a
    /// `ViewportRequest::CenterOn` for the pane's viewport. The desired pan is
    /// `start_pan + delta`; `CenterOn` places content-space `point` at the
    /// viewport center under the current zoom
    /// (`pan = center − origin − zoom·(point − origin)`, damascene
    /// `viewport_center_on`), so inverting for `point` realises exactly that
    /// pan — before the viewport's own `PanBounds` clamp, which applies to
    /// this request the same way it applies to the native gesture. Pure
    /// per-event arithmetic; layout applies the request next frame.
    pub(crate) fn update_camera_pan(&self, cx: &EventCx, pos: (f32, f32)) {
        let request = {
            let mut pan = self.camera_pan.borrow_mut();
            let Some(cp) = pan.as_mut() else {
                return;
            };
            let d = cp.update(pos);
            if !cp.moved {
                return; // still within the click slop — stay a plain click.
            }
            let key = cp.pane.canvas_key();
            let (Some(rect), Some(vv)) = (cx.rect_of_key(key), cx.viewport_view(key)) else {
                return;
            };
            if !(vv.zoom.is_finite() && vv.zoom > 0.0) {
                return;
            }
            let desired = (cp.start_pan.0 + d.0, cp.start_pan.1 + d.1);
            // Invert viewport_center_on for the content point whose centering
            // yields `desired` (origin = the viewport's inner top-left; our
            // canvas viewports have no padding, so that is the keyed rect).
            let (cx_, cy_) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            let point = (
                rect.x + (cx_ - rect.x - desired.0) / vv.zoom,
                rect.y + (cy_ - rect.y - desired.1) / vv.zoom,
            );
            ViewportRequest::CenterOn {
                key: key.to_string(),
                point,
            }
        };
        self.pending.borrow_mut().push(request);
    }

    /// Pointer-up with a camera pan in flight: disarm, and eat the trailing
    /// Click iff the gesture moved (a pan is not a select); an un-moved
    /// press-release leaves the Click alone so it stays a plain click-select.
    pub(crate) fn finish_camera_pan(&self) {
        if let Some(cp) = self.camera_pan.borrow_mut().take()
            && cp.moved
        {
            self.suppress_click.set(true);
        }
    }

    /// Handle a pointer event over a schematic pane: pick symbol/pin/wire → the schematic
    /// selection (pin > wire > symbol). Uses THE CLICKED PANE's canvas key + rect + view.
    pub(crate) fn handle_schematic_pointer(
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
