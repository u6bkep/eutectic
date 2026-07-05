//! The tool mode state machine (structural commitment 4, `docs/gui-architecture.md`).
//!
//! One active tool app-wide (a global mode, not per-pane). Milestone 3 stubs the
//! machine with two tools — [`Tool::Select`] (the default; picks entities) and
//! [`Tool::Measure`] (a two-click distance readout). The active tool owns its
//! *uncommitted* preview state, which renders **only** to the dynamic overlay (the
//! preview-channel pattern) — nothing is written to the doc. Switching tools or
//! pressing Esc cancels any in-progress preview cleanly.
//!
//! This is deliberately minimal: it establishes the enum + preview-channel shape that
//! interactive routing (ladder levels 1–4) and the other board tools grow into,
//! without any command-layer wiring (there is no commit path in m3).

use ecad_core::coord::{MM, Point};

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
