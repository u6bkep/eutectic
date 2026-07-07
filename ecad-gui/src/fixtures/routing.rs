//! The m6 slice-B manual-routing scenes: a route in progress, a committed
//! multi-waypoint trace, a layer-switched route with its via drop, and a
//! trace-vertex refinement drag. Moved verbatim from `fixtures.rs`
//! (gui-module-split).

use super::edit_board_domain;
use crate::app::EcadApp;

// ---------------------------------------------------------------------------
// Milestone-6 slice-B scenes: manual trace drawing (routing ladder level 1).
// A route in progress (pending waypoints + rubber segment), a committed
// multi-waypoint trace, a layer-switched route with its via drop, and a
// trace-vertex refinement drag in progress.
// ---------------------------------------------------------------------------

/// A board point at integer-mm `(x, y)` — shorthand for the m6b scenes.
pub(crate) fn mm_pt(x: i64, y: i64) -> ecad_core::coord::Point {
    use ecad_core::coord::MM;
    ecad_core::coord::Point {
        x: x * MM,
        y: y * MM,
    }
}

/// A route in progress (m6 slice B): the Route tool active over the editing
/// board with a pending route started at C1's `p1` pad (net GND, active layer
/// F.Cu), two waypoints clicked, and the rubber segment tracking the last known
/// pointer position. Nothing committed; the doc is untouched and clean.
pub fn route_in_progress() -> EcadApp {
    use crate::tool::Tool;
    use ecad_core::id::EntityId;
    let app = EcadApp::new(edit_board_domain());
    app.set_tool(crate::app::ViewKind::Board, Tool::Route);
    let armed = app.set_route(
        &EntityId::new("C1"),
        "p1",
        &[mm_pt(10, 5), mm_pt(10, 9)],
        Some(mm_pt(12, 10)),
    );
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app
}

/// A committed multi-waypoint trace (m6 slice B): the pending route above
/// extended to C2's `p1` pad centre and committed through `commit_route` — one
/// GND trace with two interior waypoints, committed via the command layer (the
/// doc is dirty, one undo step, the new trace selected).
pub fn routed_trace() -> EcadApp {
    use ecad_core::id::EntityId;
    let mut app = EcadApp::new(edit_board_domain());
    // (14, 12) is C2.p1's pad centre (C2 sits at (15, 12); p1 offsets -1 mm).
    let armed = app.set_route(
        &EntityId::new("C1"),
        "p1",
        &[mm_pt(10, 5), mm_pt(10, 9), mm_pt(14, 12)],
        None,
    );
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app.commit_route();
    app
}

/// A layer-switched route in progress (m6 slice B, ladder level 1's "via drop
/// on layer switch"): a pending GND route with one F.Cu waypoint, the active
/// layer switched to B.Cu (dropping a through-via at the last waypoint), and a
/// further waypoint on the new layer. Still pending — the via + both runs will
/// commit together as one undo unit.
pub fn route_layer_switch() -> EcadApp {
    use crate::tool::Tool;
    use ecad_core::id::EntityId;
    let app = EcadApp::new(edit_board_domain());
    app.set_tool(crate::app::ViewKind::Board, Tool::Route);
    let armed = app.set_route(&EntityId::new("C1"), "p1", &[mm_pt(10, 5)], None);
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app.set_active_layer("B.Cu");
    if let Some(r) = app.route.borrow_mut().as_mut() {
        r.push_waypoint(mm_pt(10, 9));
        r.hover(mm_pt(12, 10));
    }
    app
}

/// A trace-vertex refinement drag in progress (m6 slice B): the committed
/// multi-waypoint trace with its first interior vertex being dragged (Select
/// tool) — the overlay renders the vertex handles and the working-path preview;
/// nothing further is committed until release.
pub fn trace_vertex_drag() -> EcadApp {
    let app = routed_trace();
    let tid = *app
        .domain
        .doc
        .as_ref()
        .expect("routed board elaborates")
        .traces
        .keys()
        .next()
        .expect("the routed_trace scene committed a trace");
    let armed = app.set_trace_drag(tid, 1, mm_pt(8, 6));
    debug_assert!(armed, "the committed trace has an interior vertex");
    app
}
