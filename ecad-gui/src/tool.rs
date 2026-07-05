//! The tool mode state machine (structural commitment 4, `docs/gui-architecture.md`).
//!
//! One active tool app-wide (a global mode, not per-pane). Milestone 3 stubs the
//! machine with two tools — [`Tool::Select`] (the default; picks entities) and
//! [`Tool::Measure`] (a two-click distance readout). The active tool owns its
//! *uncommitted* preview state, which renders **only** to the dynamic overlay (the
//! preview-channel pattern) — nothing is written to the doc. Switching tools or
//! pressing Esc cancels any in-progress preview cleanly.
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
use ecad_core::id::EntityId;

/// The global active tool. `Select` is the default mode; `Measure` is the first
/// non-select tool, proving the machine + preview channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    /// Pick / select entities (the default). Clicks hit-test into the selection model.
    #[default]
    Select,
    /// Measure distance: first click anchors, second click (and the live pointer where
    /// events arrive) reports dx / dy / euclidean distance.
    Measure,
}

impl Tool {
    /// The route key of this tool's toolbar toggle button.
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "tool:select",
            Tool::Measure => "tool:measure",
        }
    }

    /// The button label.
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Measure => "Measure",
        }
    }

    /// Every tool, in palette order — for building the toolbar toggle strip.
    pub fn all() -> [Tool; 2] {
        [Tool::Select, Tool::Measure]
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
