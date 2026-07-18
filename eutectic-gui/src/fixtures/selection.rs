//! The m3/m4 interaction-and-views scenes: a selected trace, a measure in
//! progress, and the split-pane / schematic / cross-highlight arrangements.
//! Moved verbatim from `fixtures.rs` (gui-module-split).

use super::board_domain;
use crate::app::{DomainState, EutecticApp, PaneId, SplitAxis, ViewKind};

// ---------------------------------------------------------------------------
// Milestone-3 scenes: a board with a trace selected (overlay + populated
// inspector), and a measure-in-progress overlay. Both are the board fixture
// with canned interaction state applied, so the SVG/tree/lint artifacts cover
// the selection overlay + inspector projection + measure preview headlessly.
// ---------------------------------------------------------------------------

/// The board fixture with its `VBUS` trace (`TraceId(1)`) pre-selected: the overlay
/// draws the selection halo and the inspector shows the trace's net / layer / width /
/// length. The selected id is looked up from the doc so the scene stays honest if the
/// fixture route changes.
pub fn board_with_selection() -> EutecticApp {
    use crate::pick::SemanticId;
    let app = EutecticApp::new(board_domain());
    if let Ok(doc) = &app.domain.doc {
        // Select the first routed trace by its real id (not a hardcoded number).
        if let Some(tid) = doc.traces.keys().next().copied() {
            app.domain
                .selection
                .borrow_mut()
                .select_only(SemanticId::Trace(tid));
        }
    }
    app
}

/// The board fixture in Measure mode with a measurement in progress: an anchor at
/// (3, 3) mm and the moving end at (15, 10) mm, so the overlay draws the measure line
/// and the status bar shows the dx / dy / distance readout.
pub fn measure_in_progress() -> EutecticApp {
    use crate::tool::{MeasureState, Tool};
    use eutectic_core::coord::{MM, Point};
    let app = EutecticApp::new(board_domain());
    app.set_tool(ViewKind::Board, Tool::Measure);
    let mut m = MeasureState::default();
    m.click(Point {
        x: 3 * MM,
        y: 3 * MM,
    });
    m.click(Point {
        x: 15 * MM,
        y: 10 * MM,
    });
    app.set_measure(m);
    app
}

// ---------------------------------------------------------------------------
// Milestone-4 scenes: split panes + a read-only schematic view + cross-view
// highlighting + the explorer. The schematic source authors a small `schematic`
// block inline (per the parser in `text/schematic.rs`), so the schematic pane has
// real placed symbols, and a NET is pre-selected so the cross-highlight is visible
// in BOTH panes' overlays headlessly.
// ---------------------------------------------------------------------------

/// A tiny self-contained document with a **schematic block**: three toy parts wired on
/// two nets, laid out in a `row` of symbols, plus a board outline so the board pane also
/// renders. The `MCU` toy part has named pins so the schematic draws stubs + pin names +
/// net tags. A `wire` draws a presentational connection (so the schematic pane exercises
/// wire rendering + wire-pick → net).
pub const SCHEMATIC_ECAD: &str = "\
inst U1 MCU
inst C1 Cap
inst C2 Cap
net VDD U1.VDD C1.p1
net GND U1.GND C2.p1
board (0mm, 0mm) (30mm, 0mm) (30mm, 20mm) (0mm, 20mm)
region conductor net=VDD layer=F.Cu (2mm, 2mm) (28mm, 2mm) (28mm, 10mm) (2mm, 10mm)
region conductor net=GND layer=F.Cu (2mm, 11mm) (28mm, 11mm) (28mm, 18mm) (2mm, 18mm)
schematic {
  row gap=8mm align=center {
    sym C1
    sym U1
    sym C2
    wire C1.p1 U1.VDD
  }
}
";

/// The schematic fixture's [`DomainState`]: [`SCHEMATIC_ECAD`] against the built-in lib.
pub fn schematic_domain() -> DomainState {
    DomainState::from_source(
        SCHEMATIC_ECAD.to_string(),
        Some("schematic.eut".to_string()),
    )
}

/// Which net to pre-select in the cross-highlight scenes — a net with members on both a
/// board pad and a schematic pin, so the highlight lights up in both panes.
const CROSS_NET: &str = "VDD";

/// Pre-select the `VDD` net so the cross-highlight is visible in every pane.
fn select_cross_net(app: &EutecticApp) {
    use crate::pick::SemanticId;
    use eutectic_core::id::NetId;
    if let Ok(doc) = &app.domain.doc
        && doc.nets.contains_key(&NetId::new(CROSS_NET))
    {
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Net(NetId::new(CROSS_NET)));
    }
}

/// Default layout (board | schematic) with a NET selected — the cross-highlight is visible in
/// both panes' overlays. The headline milestone-4 scene.
pub fn dual_cross_highlight() -> EutecticApp {
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    select_cross_net(&app);
    app
}

/// A nested down-split on the schematic leaf, with the inherited kind visible.
pub fn split_down_layout() -> EutecticApp {
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.split_pane(PaneId::B, SplitAxis::Vertical);
    select_cross_net(&app);
    app
}

/// A maximized pane (the schematic pane full, the board hidden) — the maximize scene.
pub fn maximized_pane() -> EutecticApp {
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_maximized(Some(PaneId::B));
    select_cross_net(&app);
    app
}

/// Two board panes over the same doc (the per-pane-independence scene): both show the
/// board, so their cameras must be independent by El key.
pub fn dual_boards() -> EutecticApp {
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Board);
    app
}

/// Per-view-kind tool memory made visible (revised structural commitment 4): a
/// board pane and a schematic pane whose overlay strips show DIFFERENT active
/// tools simultaneously — the board kind holds Route while the schematic kind
/// holds its Select. Board-only Delete/Route remain structurally absent there.
pub fn per_kind_tools() -> EutecticApp {
    use crate::tool::Tool;
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_tool(ViewKind::Board, Tool::Route);
    app
}
