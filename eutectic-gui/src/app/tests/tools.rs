//! Per-view-kind tool state + per-pane strip tests (revised structural
//! commitment 4): strip clicks land in the clicked pane's KIND's slot, both
//! kinds' tools persist simultaneously, pane focus swaps the live tool, and
//! applicability is structural (no Route button in a schematic strip; a
//! synthesized Route click on a schematic pane is ignored).

use super::*;

/// A board|schematic app over the schematic fixture doc (pane A = board,
/// pane B = schematic — the `EutecticApp::new` default arrangement).
fn split_app() -> EutecticApp {
    EutecticApp::new(schematic_domain())
}

/// A pointer event of `kind` at `pos` targeting `pane`'s canvas (the shared
/// `pointer` helper is pane-A only).
fn pointer_in(pane: PaneId, kind: UiEventKind, pos: (f32, f32)) -> UiEvent {
    let mut e = UiEvent::synthetic_click(pane.canvas_key());
    e.kind = kind;
    e.pointer = Some(pos);
    e
}

/// The center of `pane`'s laid-out canvas rect.
fn pane_center(r: &crate::harness::Rendered, pane: PaneId) -> (f32, f32) {
    let rect = r.ui.rect_of_key(pane.canvas_key()).expect("pane laid out");
    (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
}

/// Whether any node in the built tree carries `key`.
fn tree_has_key(root: &El, key: &str) -> bool {
    root.key.as_deref() == Some(key) || root.children.iter().any(|c| tree_has_key(c, key))
}

/// Per-kind tool memory through the strips: pane A's (board) strip sets the
/// BOARD slot, pane B's (schematic) strip sets the SCHEMATIC slot, and both
/// persist simultaneously — setting one never touches the other.
#[test]
fn strip_clicks_set_per_kind_slots_that_persist() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);

    // Defaults: every kind starts on Select.
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Select);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Select);

    // Route via the BOARD pane's strip.
    app.on_event(click(&PaneId::A.strip_key(Tool::Route)), &cx);
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);
    assert_eq!(
        app.tool_for(ViewKind::Schematic),
        Tool::Select,
        "the schematic slot is untouched by a board-strip click"
    );

    // Measure via the SCHEMATIC pane's strip.
    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Measure);
    assert_eq!(
        app.tool_for(ViewKind::Board),
        Tool::Route,
        "the board slot persists — per-kind memory"
    );
}

/// The live tool is the FOCUSED pane's kind's slot: pointer focus over the board
/// pane reads Route, over the schematic pane reads Measure — swapping focus
/// swaps the live tool without touching either kind's memory. A strip click
/// also focuses its pane.
#[test]
fn pane_focus_swaps_the_live_tool() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(click(&PaneId::A.strip_key(Tool::Route)), &cx);
    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);
    // The last strip click focused pane B (schematic).
    assert_eq!(app.live_tool(), Tool::Measure);

    // Pointer over the board pane focuses it: live tool = the board slot.
    app.on_event(
        pointer_in(
            PaneId::A,
            UiEventKind::PointerEnter,
            pane_center(&r, PaneId::A),
        ),
        &cx,
    );
    assert_eq!(app.live_tool(), Tool::Route);

    // Back over the schematic pane: live tool = the schematic slot.
    app.on_event(
        pointer_in(
            PaneId::B,
            UiEventKind::PointerEnter,
            pane_center(&r, PaneId::B),
        ),
        &cx,
    );
    assert_eq!(app.live_tool(), Tool::Measure);

    // Focus swapping never wrote either slot.
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Measure);
}

/// A strip click routes to the clicked pane's KIND, not the pane itself: with
/// two BOARD panes, pane B's strip writes the one shared board slot (Blender
/// semantics — all panes of a kind follow).
#[test]
fn strip_click_routes_to_the_kind_not_the_pane() {
    let mut app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Board);
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);

    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);
    assert_eq!(
        app.tool_for(ViewKind::Board),
        Tool::Measure,
        "both board panes share the one board slot"
    );
    assert_eq!(
        app.live_tool(),
        Tool::Measure,
        "the strip click focused pane B"
    );
}

/// Applicability is structural: the schematic pane's strip renders NO Route
/// button (while the board pane's strip has one), and a synthesized Route click
/// aimed at the schematic pane is ignored — Route can never enter the schematic
/// slot.
#[test]
fn schematic_strip_has_no_route_and_ignores_a_forged_one() {
    let mut app = split_app();
    let r = settle(&mut app);

    // Board strip: all three tools. Schematic strip: Select + Measure only.
    for tool in [Tool::Select, Tool::Measure, Tool::Route] {
        assert!(
            tree_has_key(&r.tree, &PaneId::A.strip_key(tool)),
            "board strip renders {tool:?}"
        );
    }
    for tool in [Tool::Select, Tool::Measure] {
        assert!(
            tree_has_key(&r.tree, &PaneId::B.strip_key(tool)),
            "schematic strip renders {tool:?}"
        );
    }
    assert!(
        !tree_has_key(&r.tree, &PaneId::B.strip_key(Tool::Route)),
        "the schematic strip must not render a Route button"
    );

    // A forged Route click on the schematic pane's strip slot is ignored.
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(click(&PaneId::B.strip_key(Tool::Route)), &cx);
    assert_eq!(
        app.tool_for(ViewKind::Schematic),
        Tool::Select,
        "Route can never enter the schematic kind's slot"
    );
}

/// Switching the BOARD kind's tool through a strip cancels the board previews
/// (a measure in progress here); switching the SCHEMATIC kind's tool leaves
/// board previews alone — cancellation follows the kind whose slot changed.
#[test]
fn board_tool_switch_cancels_board_previews_schematic_switch_does_not() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);

    // Arm a measure preview on the board kind.
    app.on_event(click(&PaneId::A.strip_key(Tool::Measure)), &cx);
    let mut m = crate::tool::MeasureState::default();
    m.click(eutectic_core::coord::Point {
        x: 3 * NM_PER_MM,
        y: 3 * NM_PER_MM,
    });
    app.set_measure(m);
    assert!(app.measure.get().segment().is_some());

    // A SCHEMATIC-kind switch leaves the board preview alone.
    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);
    assert!(
        app.measure.get().segment().is_some(),
        "a schematic-slot change cancels nothing on the board"
    );

    // A BOARD-kind switch cancels it.
    app.on_event(click(&PaneId::A.strip_key(Tool::Select)), &cx);
    assert!(
        app.measure.get().segment().is_none(),
        "changing the board slot cancels the board measure preview"
    );
}
