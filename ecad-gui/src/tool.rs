//! The tool mode state machine (structural commitment 4, `docs/gui-architecture.md`,
//! as revised 2026-07-07).
//!
//! The active tool is keyed **per view kind** (Blender semantics): every board pane
//! shares one tool slot, every schematic pane another, and the live tool at any
//! moment is the focused pane's kind's entry (`EcadApp::tool_for` /
//! `EcadApp::set_tool` / `EcadApp::live_tool`). A kind with no entry defaults to
//! [`Tool::Select`]. Tools render as per-pane overlay strips inside each canvas
//! pane (see `crate::panes::strip`), never as an app-edge rail. The machine itself
//! is unchanged by the re-keying: the active tool owns its *uncommitted* preview
//! state, which renders **only** to the dynamic overlay (the preview-channel
//! pattern) — nothing is written to the doc. Switching a kind's tool or pressing
//! Esc cancels any in-progress preview cleanly.
//!
//! Milestone 6 slice A adds the first commit-capable interaction: [`DragState`], the
//! Select tool's in-flight component drag. The drag owns a **ghost** preview (the
//! dragged component's pad shapes, translated by the drag delta) and a live
//! **ratsnest** (a straight line from each ghost pad to the nearest other member pad
//! of its net) — both pure vector math over state captured at drag start (cached pick
//! candidates + doc net membership), so no geometry-kernel call runs in the event
//! path and the board asset is never re-tessellated during the drag. Pointer-up
//! commits the move as a `Command::Pin` through the command layer; Esc cancels.

use crate::app::PaneId;
use ecad_core::coord::{MM, Nm, Point};
use ecad_core::geom::kernel::Region;
use ecad_core::geom::{Path, Seg, Shape2D};
use ecad_core::id::{EntityId, NetId, TraceId};

/// The active tool of one view kind's slot. `Select` is the default mode (and the
/// default entry of any view kind without one); `Measure` is the first non-select
/// tool, proving the machine + preview channel; `Route` (m6 slice B) is manual
/// trace drawing at routing-ladder level 1. Which tools a kind offers is
/// structural — `ViewKind::strip_groups` lists them, and Route simply doesn't
/// exist in the schematic kind's strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    /// Pick / select entities (the default). Clicks hit-test into the selection model.
    #[default]
    Select,
    /// Manual point-to-point trace drawing (routing ladder level 1): click a pin or
    /// known-net copper to start, click waypoints, click a pin to commit — permissive,
    /// never legality-gated. Offered by the board kind's strip only.
    Route,
    /// Measure distance: first click anchors, second click (and the live pointer where
    /// events arrive) reports dx / dy / euclidean distance.
    Measure,
}

impl Tool {
    /// This tool's stable route-key suffix — a pane prefix is composed on by
    /// [`PaneId::strip_key`](crate::app::PaneId) for the per-pane strip buttons.
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "tool:select",
            Tool::Route => "tool:route",
            Tool::Measure => "tool:measure",
        }
    }

    /// The human label (strip tooltips, the status-bar tool readout).
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Route => "Route",
            Tool::Measure => "Measure",
        }
    }

    /// Every tool, in a stable order — the key-parse vocabulary. Which tools a
    /// view kind actually offers is `ViewKind::strip_groups`, not this list.
    pub fn all() -> [Tool; 3] {
        [Tool::Select, Tool::Route, Tool::Measure]
    }
}

/// The measure tool's uncommitted state (the preview channel). `None` fields mean the
/// tool is idle; once anchored, the overlay draws the segment and the status bar shows
/// the readout. Lives outside the doc — cancelled by Esc / tool switch with no undo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeasureState {
    /// The first-click anchor in board nm, once placed.
    pub anchor: Option<Point>,
    /// The live / second cursor position in board nm (the moving end while an anchor
    /// is set, or the committed second point). Sparse on 0.4.5 free-hover.
    pub cursor: Option<Point>,
}

impl MeasureState {
    /// Handle a measure click at board point `p`: if no anchor, set it (and seed the
    /// cursor so a segment shows immediately); otherwise set the second point (the
    /// measurement is now complete but stays previewed until reset). Returns nothing —
    /// the app reads [`segment`](Self::segment) / [`readout`](Self::readout) to render.
    pub fn click(&mut self, p: Point) {
        if self.anchor.is_none() {
            self.anchor = Some(p);
            self.cursor = Some(p);
        } else {
            self.cursor = Some(p);
        }
    }

    /// Update the moving end from a live pointer position (pointer-enter/drag), only
    /// while an anchor is set. A no-op before the first click.
    pub fn hover(&mut self, p: Point) {
        if self.anchor.is_some() {
            self.cursor = Some(p);
        }
    }

    /// Cancel any in-progress measurement (Esc / tool switch).
    pub fn cancel(&mut self) {
        self.anchor = None;
        self.cursor = None;
    }

    /// The current preview segment `(anchor, cursor)` in board nm, if an anchor is set.
    pub fn segment(&self) -> Option<(Point, Point)> {
        Some((self.anchor?, self.cursor?))
    }

    /// The measurement readout `(dx, dy, dist)` in **mm**, if a segment exists.
    pub fn readout(&self) -> Option<(f64, f64, f64)> {
        let (a, b) = self.segment()?;
        let mm = MM as f64;
        let dx = (b.x - a.x) as f64 / mm;
        let dy = (b.y - a.y) as f64 / mm;
        // Euclidean in nm (i128 to avoid overflow on large boards), then to mm.
        let dxn = (b.x - a.x) as i128;
        let dyn_ = (b.y - a.y) as i128;
        let dist_nm = ((dxn * dxn + dyn_ * dyn_) as f64).sqrt();
        Some((dx, dy, dist_nm / mm))
    }
}

/// Format a measure readout for the status bar: `dx / dy / dist` in mm.
pub fn format_readout(dx: f64, dy: f64, dist: f64) -> String {
    format!("dx {dx:.2}  dy {dy:.2}  d {dist:.2} mm")
}

// ----------------------------------------------------------------------------
// The Select tool's component drag (m6 slice A).
// ----------------------------------------------------------------------------

/// An in-flight component drag (Select tool, m6): the uncommitted preview state
/// between pointer-down on a component and pointer-up (commit) / Esc (cancel).
///
/// Everything geometric is captured **once** at drag start from the per-revision
/// derived caches (pad shapes + pad centers from the pick candidates, net
/// membership from the doc), so every per-event update is pure integer vector
/// math: translate the ghost by the delta, re-pick the nearest ratsnest ends.
#[derive(Clone, Debug)]
pub struct DragState {
    /// The component being dragged.
    pub comp: EntityId,
    /// The board pane the drag is happening in (ghost + ratsnest render there).
    pub pane: PaneId,
    /// The pointer-down board point (nm).
    pub start: Point,
    /// The latest pointer board point (nm).
    pub cursor: Point,
    /// The component's doc position at drag start — the commit target is
    /// `orig_pos + (cursor - start)`.
    pub orig_pos: Point,
    /// Whether the drag has exceeded the click slop — only a moved drag shows a
    /// ghost and commits; an un-moved press-release stays a plain click-select.
    pub moved: bool,
    /// The click-slop radius in board nm (the screen-px slop through the zoom at
    /// drag start).
    pub slop: Nm,
    /// The dragged component's pad shapes at their **original** position (cloned
    /// from the pick candidates at drag start); the ghost renders these translated
    /// by the current delta.
    pub shapes: Vec<Shape2D>,
    /// Ratsnest input, one entry per netted pad of the dragged component: the
    /// pad's original center, and the centers of every OTHER member pad of its
    /// net. Per event the ghost pad is `center + delta` and the line runs to the
    /// nearest other member.
    pub pins: Vec<(Point, Vec<Point>)>,
}

impl DragState {
    /// The current drag delta `(cursor - start)` in nm.
    pub fn delta(&self) -> Point {
        Point {
            x: self.cursor.x - self.start.x,
            y: self.cursor.y - self.start.y,
        }
    }

    /// The commit target: the component's original position plus the delta —
    /// "the user dragged it exactly here".
    pub fn target_pos(&self) -> Point {
        translate_point(self.orig_pos, self.delta())
    }

    /// Fold a live pointer position in: move the ghost, and latch `moved` once
    /// the drag exceeds the slop radius (it never unlatches — wobbling back over
    /// the start point mid-drag is still a drag).
    pub fn update(&mut self, p: Point) {
        self.cursor = p;
        if !self.moved {
            let dx = (p.x - self.start.x) as i128;
            let dy = (p.y - self.start.y) as i128;
            let slop = self.slop.max(0) as i128;
            if dx * dx + dy * dy > slop * slop {
                self.moved = true;
            }
        }
    }

    /// The ghost preview: the dragged component's pad shapes translated by the
    /// current delta. Pure point arithmetic (no kernel).
    pub fn ghost_shapes(&self) -> Vec<Shape2D> {
        let d = self.delta();
        self.shapes.iter().map(|s| translate_shape(s, d)).collect()
    }

    /// The live ratsnest: for each netted pad of the dragged component, a straight
    /// segment from the pad's **ghost** position to the nearest other member pad
    /// of its net (doc positions). Cheap vector math per event.
    pub fn ratsnest(&self) -> Vec<(Point, Point)> {
        let d = self.delta();
        let mut out = Vec::with_capacity(self.pins.len());
        for (center, others) in &self.pins {
            let ghost = translate_point(*center, d);
            let nearest = others.iter().min_by_key(|o| {
                let dx = (o.x - ghost.x) as i128;
                let dy = (o.y - ghost.y) as i128;
                dx * dx + dy * dy
            });
            if let Some(n) = nearest {
                out.push((ghost, *n));
            }
        }
        out
    }
}

/// Translate a point by a delta (both nm).
fn translate_point(p: Point, d: Point) -> Point {
    Point {
        x: p.x + d.x,
        y: p.y + d.y,
    }
}

/// Translate a [`Shape2D`] by a delta — the ghost transform. Pure per-point
/// arithmetic across every variant (`Stroke`/`Polygon` paths incl. curve control
/// points, `Area` ring vertices); no kernel call, no tessellation.
pub(crate) fn translate_shape(shape: &Shape2D, d: Point) -> Shape2D {
    let tpath = |path: &Path| Path {
        start: translate_point(path.start, d),
        segs: path
            .segs
            .iter()
            .map(|s| match s {
                Seg::Line { end } => Seg::Line {
                    end: translate_point(*end, d),
                },
                Seg::Arc { mid, end } => Seg::Arc {
                    mid: translate_point(*mid, d),
                    end: translate_point(*end, d),
                },
                Seg::Quadratic { ctrl, end } => Seg::Quadratic {
                    ctrl: translate_point(*ctrl, d),
                    end: translate_point(*end, d),
                },
                Seg::Cubic { c1, c2, end } => Seg::Cubic {
                    c1: translate_point(*c1, d),
                    c2: translate_point(*c2, d),
                    end: translate_point(*end, d),
                },
            })
            .collect(),
    };
    match shape {
        Shape2D::Stroke { path, radius } => Shape2D::Stroke {
            path: tpath(path),
            radius: *radius,
        },
        Shape2D::Polygon { path, radius } => Shape2D::Polygon {
            path: tpath(path),
            radius: *radius,
        },
        Shape2D::Area { region } => Shape2D::Area {
            region: Region {
                rings: region
                    .rings
                    .iter()
                    .map(|ring| ring.iter().map(|p| translate_point(*p, d)).collect())
                    .collect(),
            },
        },
    }
}

// ----------------------------------------------------------------------------
// The Select tool's camera pan (the non-component drag gesture).
// ----------------------------------------------------------------------------

/// The screen-px click slop for the camera pan: a press-release that never
/// moves past this stays a plain click (select / deselect); crossing it
/// latches the gesture as a pan and eats the trailing Click. Screen-space
/// (not board-space) because the gesture is a camera move, not a board edit.
pub const CAMERA_PAN_SLOP_PX: f32 = 4.0;

/// An in-flight Select-tool **camera pan** (the counterpart of [`DragState`]
/// for everything that is *not* a component): armed on pointer-down over a
/// board pane when the press picks no draggable component and no selected
/// trace vertex — pour, trace, empty board, grid furniture alike.
///
/// Why the app owns this at all: damascene's native viewport pan (default
/// primary-button trigger) only engages when the press hits **nothing or the
/// viewport's own node** (`runtime.rs`, "Viewport pan"). Every canvas child —
/// the layer / grid / overlay vector Els — is a keyed hit-test target whose
/// rect spans the full content viewBox, so any press inside the content rect
/// suppresses the native gesture and the pointer events flow to the app
/// instead. This state turns those events back into a pan: per drag event the
/// desired pan is `start_pan + (pointer − start_px)`, realised as a
/// `ViewportRequest::CenterOn` (the one request that can place the pan
/// anywhere at the current zoom); damascene's layout applies it next frame
/// under the same `PanBounds` clamp the native pan gets. Pure per-event
/// arithmetic — no kernel, no tessellation (event-path discipline).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraPanState {
    /// The board pane whose camera is being panned.
    pub pane: PaneId,
    /// The pointer-down position in screen (logical) px.
    pub start_px: (f32, f32),
    /// The pane camera's pan at pointer-down (`ViewportView::pan`).
    pub start_pan: (f32, f32),
    /// Latched once the drag exceeds [`CAMERA_PAN_SLOP_PX`] — only a moved
    /// gesture pans and eats the trailing Click; an un-moved press-release
    /// stays a plain click-select (pours stay selectable).
    pub moved: bool,
}

impl CameraPanState {
    /// Fold a live pointer position in: the screen-px delta since the press,
    /// latching `moved` past the slop (never unlatches — wobbling back over
    /// the start point mid-pan is still a pan).
    pub fn update(&mut self, px: (f32, f32)) -> (f32, f32) {
        let d = (px.0 - self.start_px.0, px.1 - self.start_px.1);
        if !self.moved && d.0 * d.0 + d.1 * d.1 > CAMERA_PAN_SLOP_PX * CAMERA_PAN_SLOP_PX {
            self.moved = true;
        }
        d
    }
}

// ----------------------------------------------------------------------------
// The Route tool's pending route (m6 slice B, routing ladder level 1).
// ----------------------------------------------------------------------------

/// One single-layer run of a pending route: the copper slab name and the points
/// clicked onto it so far. A layer switch closes the current run (dropping a via
/// at its last point) and opens a new one starting at that same point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRun {
    /// The copper slab this run's trace will land on (`Trace::layer`).
    pub layer: String,
    /// The polyline so far: the anchor, then each waypoint (consecutive
    /// duplicates deduped). A run commits as a trace only with ≥ 2 points.
    pub points: Vec<Point>,
}

/// The Route tool's uncommitted pending route (the preview channel): the net it
/// belongs to, the per-layer runs, the vias dropped by layer switches, and the
/// last known pointer position (the rubber-segment end — sparse on damascene
/// 0.4.5, which delivers no free-hover pointer-move; the rubber updates on the
/// events that do arrive: enter / down / drag / up). Lives outside the doc;
/// cancelled by Esc / tool switch with no undo. Commit turns the runs + vias
/// into one `commit_edit` transaction (one undo unit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteState {
    /// The trace's net — fixed by the start pick (a pin's net, or the net of the
    /// known-net copper clicked). Permissive: committing onto a *different* net's
    /// pin keeps THIS net; the overlap surfaces as DRC findings, never a block.
    pub net: NetId,
    /// The per-layer runs, oldest first; the LAST run is the live one.
    pub runs: Vec<RouteRun>,
    /// Via positions dropped by layer switches (through vias, span `None`).
    pub vias: Vec<Point>,
    /// The last known pointer board position — the rubber segment's moving end.
    pub cursor: Option<Point>,
}

impl RouteState {
    /// Start a pending route on `net`, anchored at `anchor` on copper slab `layer`.
    pub fn start(net: NetId, layer: String, anchor: Point) -> RouteState {
        RouteState {
            net,
            runs: vec![RouteRun {
                layer,
                points: vec![anchor],
            }],
            vias: Vec::new(),
            cursor: Some(anchor),
        }
    }

    /// The live run's layer.
    pub fn current_layer(&self) -> &str {
        &self.runs.last().expect("a route always has a run").layer
    }

    /// The last committed-to point (the rubber segment's fixed end).
    pub fn last_point(&self) -> Point {
        *self
            .runs
            .last()
            .and_then(|r| r.points.last())
            .expect("a route always has an anchor")
    }

    /// Append a waypoint to the live run (raw board position — no grid snap in
    /// v1). A click on the exact last point is a no-op (no zero-length segment).
    pub fn push_waypoint(&mut self, p: Point) {
        if self.last_point() != p {
            self.runs.last_mut().expect("run").points.push(p);
        }
        self.cursor = Some(p);
    }

    /// Switch the live run to `layer` (the active-layer switch while pending):
    /// closes the current run and opens a new one on `layer` starting at the
    /// last point, recording a through-via drop there. A switch to the same
    /// layer is a no-op.
    pub fn switch_layer(&mut self, layer: &str) {
        if self.current_layer() == layer {
            return;
        }
        let at = self.last_point();
        self.vias.push(at);
        self.runs.push(RouteRun {
            layer: layer.to_string(),
            points: vec![at],
        });
    }

    /// Update the rubber segment's moving end from a live pointer position.
    pub fn hover(&mut self, p: Point) {
        self.cursor = Some(p);
    }

    /// The rubber segment `(last point, cursor)`, if the cursor has moved off
    /// the last point.
    pub fn rubber(&self) -> Option<(Point, Point)> {
        let last = self.last_point();
        let cur = self.cursor?;
        (cur != last).then_some((last, cur))
    }

    /// Is there anything to commit? At least one run with ≥ 2 points, or a via.
    /// (A start-click followed immediately by a commit-click on the same point
    /// has neither — the commit is ignored and the route stays pending.)
    pub fn has_committable(&self) -> bool {
        !self.vias.is_empty() || self.runs.iter().any(|r| r.points.len() >= 2)
    }
}

// ----------------------------------------------------------------------------
// The Select tool's trace-vertex refinement drag (m6 slice B).
// ----------------------------------------------------------------------------

/// An in-flight vertex drag on a *selected* trace: the uncommitted preview state
/// between pointer-down on a vertex handle (or on a segment, which inserts a new
/// vertex there) and pointer-up (commit) / Esc (cancel). `path` is the working
/// copy — `path[index]` tracks the pointer; release commits the whole path via
/// Remove+Add under the SAME `TraceId` in one transaction (the engine has no
/// update-path command).
#[derive(Clone, Debug)]
pub struct TraceDragState {
    /// The trace being refined.
    pub trace: TraceId,
    /// The working path (doc path with the dragged/inserted vertex updated live).
    pub path: Vec<Point>,
    /// The index of the vertex tracking the pointer.
    pub index: usize,
    /// The trace's width (for the preview stroke).
    pub width: Nm,
    /// The pointer-down board point (nm).
    pub start: Point,
    /// Latches once the drag exceeds the slop — only a moved drag commits; an
    /// un-moved press-release stays a plain click (and discards an inserted
    /// vertex without committing it).
    pub moved: bool,
    /// The click-slop radius in board nm.
    pub slop: Nm,
}

impl TraceDragState {
    /// Fold a live pointer position in: move the dragged vertex, latch `moved`
    /// past the slop (never unlatches).
    pub fn update(&mut self, p: Point) {
        self.path[self.index] = p;
        if !self.moved {
            let dx = (p.x - self.start.x) as i128;
            let dy = (p.y - self.start.y) as i128;
            let slop = self.slop.max(0) as i128;
            if dx * dx + dy * dy > slop * slop {
                self.moved = true;
            }
        }
    }
}

/// The index of the path vertex within `tol` of `p` (squared-integer test),
/// nearest first on ties. Cheap per-event math over a handful of points — the
/// handle hit-test (event-path discipline: no kernel).
pub fn hit_vertex(path: &[Point], p: Point, tol: Nm) -> Option<usize> {
    let tol2 = (tol.max(0) as i128) * (tol.max(0) as i128);
    let mut best: Option<(usize, i128)> = None;
    for (i, v) in path.iter().enumerate() {
        let dx = (v.x - p.x) as i128;
        let dy = (v.y - p.y) as i128;
        let d2 = dx * dx + dy * dy;
        if d2 <= tol2 && best.map(|(_, b)| d2 < b).unwrap_or(true) {
            best = Some((i, d2));
        }
    }
    best.map(|(i, _)| i)
}

/// The segment of `path` within `tol` of `p`, as `(insert_index, projected point)`:
/// the point on the nearest segment closest to `p`, and the vertex index at which
/// inserting it keeps the path order (segment `i` → insert at `i + 1`). Exact
/// integer projection (i128), clamped to the segment. `None` when nothing is
/// within `tol`. Cheap per-event vector math (event-path discipline).
pub fn hit_segment(path: &[Point], p: Point, tol: Nm) -> Option<(usize, Point)> {
    let tol2 = (tol.max(0) as i128) * (tol.max(0) as i128);
    let mut best: Option<(usize, Point, i128)> = None;
    for i in 0..path.len().saturating_sub(1) {
        let (a, b) = (path[i], path[i + 1]);
        let abx = (b.x - a.x) as i128;
        let aby = (b.y - a.y) as i128;
        let apx = (p.x - a.x) as i128;
        let apy = (p.y - a.y) as i128;
        let len2 = abx * abx + aby * aby;
        let proj = if len2 == 0 {
            a
        } else {
            // t = clamp(dot(ap, ab) / |ab|², 0..1), applied in integer space with
            // rounding: q = a + ab * dot / len2.
            let dot = (apx * abx + apy * aby).clamp(0, len2);
            Point {
                x: a.x + ((abx * dot + len2 / 2) / len2) as Nm,
                y: a.y + ((aby * dot + len2 / 2) / len2) as Nm,
            }
        };
        let dx = (p.x - proj.x) as i128;
        let dy = (p.y - proj.y) as i128;
        let d2 = dx * dx + dy * dy;
        if d2 <= tol2 && best.as_ref().map(|(_, _, b)| d2 < *b).unwrap_or(true) {
            best = Some((i + 1, proj, d2));
        }
    }
    best.map(|(i, q, _)| (i, q))
}

/// The point on `path` nearest to `p` (unclamped tolerance) — the snap point for
/// starting a route on a clicked trace. `None` for a degenerate path.
pub fn closest_on_path(path: &[Point], p: Point) -> Option<Point> {
    hit_segment(path, p, Nm::MAX / 4).map(|(_, q)| q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    /// A drag below the slop stays a click (`moved == false`); crossing the slop
    /// latches `moved` and it stays latched even back at the start point.
    #[test]
    fn drag_slop_latches() {
        let mut d = DragState {
            comp: EntityId::new("C1"),
            pane: PaneId::A,
            start: pt(0, 0),
            cursor: pt(0, 0),
            orig_pos: pt(10, 10),
            moved: false,
            slop: 100,
            shapes: vec![],
            pins: vec![],
        };
        d.update(pt(50, 50)); // inside the slop circle (dist ~70 < 100)
        assert!(!d.moved, "within slop is not a move");
        d.update(pt(200, 0)); // outside
        assert!(d.moved);
        d.update(pt(0, 0)); // back to start — stays a drag
        assert!(d.moved, "moved latches");
        assert_eq!(d.target_pos(), pt(10, 10), "delta 0 → original position");
    }

    /// The ratsnest connects each ghost pad to the NEAREST other member, and the
    /// ghost end tracks the delta.
    #[test]
    fn ratsnest_picks_nearest_member() {
        let mut d = DragState {
            comp: EntityId::new("C1"),
            pane: PaneId::A,
            start: pt(0, 0),
            cursor: pt(0, 0),
            orig_pos: pt(0, 0),
            moved: true,
            slop: 0,
            shapes: vec![],
            pins: vec![(pt(0, 0), vec![pt(1000, 0), pt(-200, 0)])],
        };
        // At the start, the nearest other member is (-200, 0).
        assert_eq!(d.ratsnest(), vec![(pt(0, 0), pt(-200, 0))]);
        // Drag +600 in x: ghost pad at (600, 0); nearest flips to (1000, 0).
        d.update(pt(600, 0));
        assert_eq!(d.ratsnest(), vec![(pt(600, 0), pt(1000, 0))]);
    }

    /// The pending-route state machine: waypoints dedupe, a layer switch drops a
    /// via at the last point and continues on the new layer, and the rubber
    /// segment tracks the cursor.
    #[test]
    fn route_state_waypoints_layer_switch_and_rubber() {
        let mut r = RouteState::start(NetId::new("GND"), "F.Cu".into(), pt(0, 0));
        assert!(!r.has_committable(), "an anchor alone commits nothing");
        assert_eq!(r.rubber(), None, "cursor starts on the anchor");

        r.push_waypoint(pt(0, 0)); // exact re-click: deduped
        assert_eq!(r.runs[0].points.len(), 1);
        r.push_waypoint(pt(1000, 0));
        assert!(r.has_committable());

        r.hover(pt(1500, 500));
        assert_eq!(r.rubber(), Some((pt(1000, 0), pt(1500, 500))));

        // Layer switch: via at the last waypoint, new run opens there on B.Cu.
        r.switch_layer("B.Cu");
        assert_eq!(r.vias, vec![pt(1000, 0)]);
        assert_eq!(r.current_layer(), "B.Cu");
        assert_eq!(r.last_point(), pt(1000, 0));
        // Same-layer switch is a no-op.
        r.switch_layer("B.Cu");
        assert_eq!(r.vias.len(), 1);
        assert_eq!(r.runs.len(), 2);
    }

    /// Vertex and segment hit-testing: a vertex within tolerance wins its index;
    /// a mid-segment point projects onto the segment with the right insert index;
    /// out-of-tolerance points miss.
    #[test]
    fn vertex_and_segment_hits() {
        let path = [pt(0, 0), pt(1000, 0), pt(1000, 1000)];
        assert_eq!(hit_vertex(&path, pt(10, -10), 50), Some(0));
        assert_eq!(hit_vertex(&path, pt(990, 990), 50), Some(2));
        assert_eq!(hit_vertex(&path, pt(500, 300), 50), None);

        // On the first segment, 30 off-axis: projects to (500, 0), insert at 1.
        assert_eq!(hit_segment(&path, pt(500, 30), 50), Some((1, pt(500, 0))));
        // On the second segment: projects to (1000, 500), insert at 2.
        assert_eq!(
            hit_segment(&path, pt(970, 500), 50),
            Some((2, pt(1000, 500)))
        );
        // Too far from any segment.
        assert_eq!(hit_segment(&path, pt(500, 300), 50), None);
        // The unclamped nearest-point helper (route-start snap onto a trace).
        assert_eq!(closest_on_path(&path, pt(500, 300)), Some(pt(500, 0)));
    }

    /// The trace-vertex drag latches `moved` past the slop and tracks the vertex.
    #[test]
    fn trace_drag_moves_vertex_and_latches() {
        let mut d = TraceDragState {
            trace: TraceId(1),
            path: vec![pt(0, 0), pt(1000, 0)],
            index: 1,
            width: 100,
            start: pt(1000, 0),
            moved: false,
            slop: 100,
        };
        d.update(pt(1030, 0)); // inside the slop
        assert!(!d.moved);
        assert_eq!(d.path[1], pt(1030, 0), "the vertex tracks even inside slop");
        d.update(pt(1500, 200));
        assert!(d.moved);
        assert_eq!(d.path, vec![pt(0, 0), pt(1500, 200)]);
    }

    /// `translate_shape` moves every variant's points and preserves radii.
    #[test]
    fn translate_shape_moves_points() {
        let d = pt(5, -3);
        let disc = Shape2D::disc(pt(10, 10), 7);
        match translate_shape(&disc, d) {
            Shape2D::Stroke { path, radius } => {
                assert_eq!(path.start, pt(15, 7));
                assert_eq!(radius, 7);
            }
            other => panic!("disc stays a stroke, got {other:?}"),
        }
        let area = Shape2D::Area {
            region: Region {
                rings: vec![vec![pt(0, 0), pt(10, 0), pt(10, 10)]],
            },
        };
        match translate_shape(&area, d) {
            Shape2D::Area { region } => {
                assert_eq!(region.rings, vec![vec![pt(5, -3), pt(15, -3), pt(15, 7)]]);
            }
            other => panic!("area stays an area, got {other:?}"),
        }
    }
}
