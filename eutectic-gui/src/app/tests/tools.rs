//! Per-view-kind tool state + per-pane strip tests (revised structural
//! commitment 4): strip clicks land in the clicked pane's KIND's slot, both
//! kinds' tools persist simultaneously, pane focus swaps the live tool, and
//! applicability is structural (schematic offers Select/Pan/Measure;
//! forged board-only tool clicks aimed at a schematic pane are ignored).

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
/// BOARD slot and the schematic slot is untouched by it. The schematic kind
/// offers Measure, so its slot moves independently. A forged board-only tool
/// click aimed at the schematic pane is ignored (structural applicability).
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

    // A forged board-only Delete click is ignored.
    app.on_event(click(&PaneId::B.strip_key(Tool::Delete)), &cx);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Measure);
}

/// The live tool is the FOCUSED pane's kind's slot: pointer focus over the board
/// pane reads Route, over the schematic pane its Select — swapping focus swaps
/// the live tool without touching either kind's memory. A strip click also
/// focuses its pane.
#[test]
fn pane_focus_swaps_the_live_tool() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(click(&PaneId::A.strip_key(Tool::Route)), &cx);
    app.on_event(click(&PaneId::B.strip_key(Tool::Select)), &cx);
    // The last strip click focused pane B (schematic).
    assert_eq!(app.live_tool(), Tool::Select);

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
    assert_eq!(app.live_tool(), Tool::Select);

    // Focus swapping never wrote either slot.
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Select);
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

/// Applicability is structural: board has the shared four-tool head plus Route;
/// schematic has Select/Pan/Measure and excludes Delete/Route.
#[test]
fn strips_match_the_view_ruling_and_ignore_forged_tools() {
    let mut app = split_app();
    let r = settle(&mut app);

    assert_eq!(
        ViewKind::Board.strip_groups(),
        &[
            &[Tool::Select, Tool::Pan, Tool::Measure, Tool::Delete][..],
            &[Tool::Route][..],
        ]
    );
    assert_eq!(
        ViewKind::Schematic.strip_groups(),
        &[&[Tool::Select, Tool::Pan, Tool::Measure][..]]
    );

    for tool in [
        Tool::Select,
        Tool::Pan,
        Tool::Measure,
        Tool::Delete,
        Tool::Route,
    ] {
        assert!(
            tree_has_key(&r.tree, &PaneId::A.strip_key(tool)),
            "board strip renders {tool:?}"
        );
    }
    for tool in [Tool::Select, Tool::Pan, Tool::Measure] {
        assert!(
            tree_has_key(&r.tree, &PaneId::B.strip_key(tool)),
            "schematic strip renders {tool:?}"
        );
    }
    for absent in [Tool::Delete, Tool::Route] {
        assert!(
            !tree_has_key(&r.tree, &PaneId::B.strip_key(absent)),
            "the schematic strip must not render a {absent:?} button"
        );
    }

    // Forged board-only tool clicks on the schematic pane's strip are ignored.
    let cx = EventCx::new().with_ui_state(&r.ui);
    for forged in [Tool::Delete, Tool::Route] {
        app.on_event(click(&PaneId::B.strip_key(forged)), &cx);
        assert_eq!(
            app.tool_for(ViewKind::Schematic),
            Tool::Select,
            "{forged:?} can never enter the schematic kind's slot"
        );
    }
}

/// Pan mode always arms the camera, even when a board drag starts on a pad;
/// Select on that same point arms the component drag instead.
#[test]
fn pan_tool_pans_over_board_objects_without_picking_or_moving() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    let c1 = EntityId::new("C1");
    let pad = app
        .derived
        .borrow()
        .board
        .as_ref()
        .expect("board")
        .candidates
        .iter()
        .find(|candidate| {
            matches!(&candidate.id, crate::pick::SemanticId::Pin { comp, .. } if comp == &c1)
        })
        .expect("C1 pad")
        .aabb;
    let at = px_of_board(
        &app,
        &r,
        Point {
            x: (pad.0.x + pad.1.x) / 2,
            y: (pad.0.y + pad.1.y) / 2,
        },
    );

    app.on_event(pointer(UiEventKind::PointerDown, at), &cx);
    assert!(
        app.drag_active(),
        "Select arms the component drag over a pad"
    );
    app.on_event(escape(), &cx);

    app.on_event(strip_click(Tool::Pan), &cx);
    let before = app.pane_camera(PaneId::A).center;
    app.on_event(pointer(UiEventKind::PointerDown, at), &cx);
    assert!(!app.drag_active(), "Pan never arms a component move");
    assert!(app.camera_pan.borrow().is_some(), "Pan arms the camera");
    app.on_event(pointer(UiEventKind::Drag, (at.0 + 30.0, at.1 + 12.0)), &cx);
    assert_ne!(app.pane_camera(PaneId::A).center, before);
    assert!(app.domain.selection.borrow().is_empty(), "Pan never picks");
    app.on_event(
        pointer(UiEventKind::PointerUp, (at.0 + 30.0, at.1 + 12.0)),
        &cx,
    );
}

/// The dedicated Pan branch is also live in schematic panes and does not run
/// the schematic pick path when the press begins on a symbol body.
#[test]
fn pan_tool_pans_over_schematic_objects_without_picking() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    let rect = r.ui.rect_of_key(PaneId::B.canvas_key()).expect("pane B");
    let center = app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .reflow_schematic(&app.domain.lib)[&EntityId::new("C1")]
        .center;
    let before = app.pane_camera(PaneId::B).center;
    let at = crate::app::canvas_pane::pane_project(
        &app.pane_camera(PaneId::B),
        (rect.x, rect.y, rect.w, rect.h),
        center,
    );

    app.on_event(click(&PaneId::B.strip_key(Tool::Pan)), &cx);
    app.on_event(pointer_in(PaneId::B, UiEventKind::PointerDown, at), &cx);
    app.on_event(
        pointer_in(PaneId::B, UiEventKind::Drag, (at.0 + 24.0, at.1 - 16.0)),
        &cx,
    );

    assert_ne!(app.pane_camera(PaneId::B).center, before);
    assert!(app.domain.selection.borrow().is_empty());
    assert!(!app.drag_active());
}

/// Schematic Measure unprojects through the schematic pane's own camera and
/// stores those schematic-space points (not board-pane geometry).
#[test]
fn schematic_measure_uses_schematic_space() {
    let mut app = split_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);

    let rect = r.ui.rect_of_key(PaneId::B.canvas_key()).expect("pane B");
    let cam = app.pane_camera(PaneId::B);
    let a = Point::mm(2, 3);
    let b = Point::mm(7, 9);
    let project =
        |p| crate::app::canvas_pane::pane_project(&cam, (rect.x, rect.y, rect.w, rect.h), p);
    app.on_event(pointer_in(PaneId::B, UiEventKind::Click, project(a)), &cx);
    app.on_event(
        pointer_in(PaneId::B, UiEventKind::PointerEnter, project(b)),
        &cx,
    );
    let (_, hover) = app.measure.get().segment().expect("rubber band");
    assert!((hover.x - b.x).abs() < 20);
    assert!((hover.y - b.y).abs() < 20);
    app.on_event(pointer_in(PaneId::B, UiEventKind::Click, project(b)), &cx);

    assert_eq!(app.measure_pane.get(), PaneId::B);
    let (got_a, got_b) = app.measure.get().segment().expect("measurement");
    for (got, want) in [(got_a, a), (got_b, b)] {
        assert!((got.x - want.x).abs() < 20);
        assert!((got.y - want.y).abs() < 20);
    }
    app.on_event(escape(), &cx);
    assert!(app.measure.get().segment().is_none());
}

/// Switching the BOARD kind's tool through a strip cancels the board previews
/// (a measure in progress here); a SCHEMATIC-strip click leaves board previews
/// alone — cancellation follows the kind whose slot changed.
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

    // A real SCHEMATIC tool change leaves the board preview alone.
    app.on_event(click(&PaneId::B.strip_key(Tool::Pan)), &cx);
    assert!(
        app.measure.get().segment().is_some(),
        "a schematic-strip click cancels nothing on the board"
    );

    // A BOARD-kind switch cancels it.
    app.on_event(click(&PaneId::A.strip_key(Tool::Select)), &cx);
    assert!(
        app.measure.get().segment().is_none(),
        "changing the board slot cancels the board measure preview"
    );
}

/// Moving a measurement between Board and Schematic panes drops the old anchor
/// before either event-driven or free-hover cursor coordinates can mix with it.
#[test]
fn measure_resets_across_view_kinds_in_both_directions() {
    let mut app = split_app();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let board_rect = rendered
        .ui
        .rect_of_key(PaneId::A.canvas_key())
        .expect("board pane");
    let schematic_rect = rendered
        .ui
        .rect_of_key(PaneId::B.canvas_key())
        .expect("schematic pane");
    let board_at = (
        board_rect.x + board_rect.w / 2.0,
        board_rect.y + board_rect.h / 2.0,
    );
    let schematic_at = (
        schematic_rect.x + schematic_rect.w / 2.0,
        schematic_rect.y + schematic_rect.h / 2.0,
    );

    app.on_event(click(&PaneId::A.strip_key(Tool::Measure)), &cx);
    app.on_event(pointer_in(PaneId::A, UiEventKind::Click, board_at), &cx);
    assert!(app.measure.get().segment().is_some());
    app.on_event(click(&PaneId::B.strip_key(Tool::Measure)), &cx);
    app.on_event(
        pointer_in(PaneId::B, UiEventKind::PointerEnter, schematic_at),
        &cx,
    );
    assert!(
        app.measure.get().segment().is_none(),
        "schematic hover discarded the board-space anchor"
    );

    app.on_event(pointer_in(PaneId::B, UiEventKind::Click, schematic_at), &cx);
    assert!(app.measure.get().segment().is_some());
    assert!(app.raw_cursor_moved(board_at));
    assert_eq!(app.measure_pane.get(), PaneId::A);
    assert!(
        app.measure.get().segment().is_none(),
        "board free-hover discarded the schematic-space anchor"
    );
}
