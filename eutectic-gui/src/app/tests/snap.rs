//! App-event coverage for the app-wide displayed-grid snapping contract.

use super::*;
use crate::chrome::menubar::SNAP_TO_GRID_KEY;
use crate::render::Camera;

fn pin_center(app: &EutecticApp, comp: &str, pin: &str) -> Point {
    let want = SemanticId::Pin {
        comp: EntityId::new(comp),
        pin: pin.to_string(),
    };
    let derived = app.derived.borrow();
    let candidate = derived
        .board
        .as_ref()
        .expect("board")
        .candidates
        .iter()
        .find(|candidate| candidate.id == want)
        .expect("pin candidate");
    Point {
        x: (candidate.aabb.0.x + candidate.aabb.1.x) / 2,
        y: (candidate.aabb.0.y + candidate.aabb.1.y) / 2,
    }
}

fn set_zoom(app: &EutecticApp, zoom: f64) {
    let mut cameras = app.pane_cams.borrow_mut();
    let glide = &mut cameras[pane_index(PaneId::A)].glide;
    glide.snap(Camera::new(glide.current().center, zoom));
}

fn drag_c1(app: &mut EutecticApp, rendered: &crate::harness::Rendered) -> (Point, Point) {
    let comp = EntityId::new("C1");
    let original = comp_pos(app, &comp);
    let grab = pad_center_of(app, &comp);
    let grab_px = px_of_board(app, rendered, grab);
    let drop_px = px_of_board(
        app,
        rendered,
        Point {
            x: grab.x + 3_370_000,
            y: grab.y + 2_210_000,
        },
    );
    let raw_grab = board_of_px(app, rendered, grab_px);
    let raw_drop = board_of_px(app, rendered, drop_px);
    let raw_target = Point {
        x: original.x + raw_drop.x - raw_grab.x,
        y: original.y + raw_drop.y - raw_grab.y,
    };
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
    app.on_event(pointer(UiEventKind::Drag, drop_px), &cx);
    let preview_target = app
        .drag
        .borrow()
        .as_ref()
        .expect("drag preview")
        .target_pos();
    app.on_event(pointer(UiEventKind::PointerUp, drop_px), &cx);
    (raw_target, preview_target)
}

#[test]
fn part_drag_preview_and_commit_snap_and_toggle_off_is_raw() {
    let mut snapped = edit_app();
    let rendered = settle(&mut snapped);
    set_zoom(&snapped, 1e-5);
    let pitch = snapped.displayed_grid_pitch(PaneId::A);
    let (raw_target, preview_target) = drag_c1(&mut snapped, &rendered);
    let expected = snap_point(raw_target, pitch);
    assert_eq!(
        preview_target, expected,
        "ghost preview uses the lattice target"
    );
    assert_eq!(comp_pos(&snapped, &EntityId::new("C1")), expected);
    assert_eq!((expected.x % pitch, expected.y % pitch), (0, 0));

    let mut raw = edit_app();
    let rendered = settle(&mut raw);
    set_zoom(&raw, 1e-5);
    raw.on_event(click(SNAP_TO_GRID_KEY), &EventCx::new());
    assert!(!raw.snap_to_grid());
    let (raw_target, preview_target) = drag_c1(&mut raw, &rendered);
    assert_eq!(preview_target, raw_target);
    assert_eq!(comp_pos(&raw, &EntityId::new("C1")), raw_target);
}

#[test]
fn route_waypoint_snaps_but_pin_anchors_remain_exact() {
    let mut app = edit_app();
    let rendered = settle(&mut app);
    set_zoom(&app, 1e-6);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let pitch = app.displayed_grid_pitch(PaneId::A);
    app.on_event(strip_click(Tool::Route), &cx);

    let start = pin_center(&app, "C1", "p1");
    let end = pin_center(&app, "C2", "p1");
    let waypoint_px = px_of_board(
        &app,
        &rendered,
        Point {
            x: 7_400_000,
            y: 8_400_000,
        },
    );
    let raw_waypoint = board_of_px(&app, &rendered, waypoint_px);
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, start)),
        &cx,
    );
    app.on_event(pointer(UiEventKind::Click, waypoint_px), &cx);
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, end)),
        &cx,
    );

    let trace = app
        .domain
        .doc
        .as_ref()
        .expect("doc")
        .traces
        .values()
        .next()
        .expect("committed trace");
    assert_eq!(
        trace.path,
        vec![start, snap_point(raw_waypoint, pitch), end]
    );
    assert_ne!(
        start,
        snap_point(start, pitch),
        "start pin is off this coarse grid"
    );
    assert_ne!(
        end,
        snap_point(end, pitch),
        "end pin is off this coarse grid"
    );
}

#[test]
fn vertex_refinement_drag_snaps() {
    let mut app = edit_app();
    let rendered = settle(&mut app);
    set_zoom(&app, 1e-5);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let start = pin_center(&app, "C1", "p1");
    let end = pin_center(&app, "C2", "p1");
    app.on_event(strip_click(Tool::Route), &cx);
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, start)),
        &cx,
    );
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, end)),
        &cx,
    );
    app.on_event(strip_click(Tool::Select), &cx);

    let midpoint = Point {
        x: (start.x + end.x) / 2,
        y: (start.y + end.y) / 2,
    };
    let destination_px = px_of_board(
        &app,
        &rendered,
        Point {
            x: 12_400_000,
            y: 7_600_000,
        },
    );
    let raw_destination = board_of_px(&app, &rendered, destination_px);
    app.on_event(
        pointer(
            UiEventKind::PointerDown,
            px_of_board(&app, &rendered, midpoint),
        ),
        &cx,
    );
    app.on_event(pointer(UiEventKind::Drag, destination_px), &cx);
    app.on_event(pointer(UiEventKind::PointerUp, destination_px), &cx);

    let pitch = app.displayed_grid_pitch(PaneId::A);
    let trace = app
        .domain
        .doc
        .as_ref()
        .expect("doc")
        .traces
        .values()
        .next()
        .expect("trace");
    assert_eq!(trace.path[1], snap_point(raw_destination, pitch));
}

#[test]
fn snap_pitch_follows_the_interaction_pane_zoom() {
    fn waypoint_at(zoom: f64) -> (Nm, Point) {
        let mut app = edit_app();
        let rendered = settle(&mut app);
        set_zoom(&app, zoom);
        let cx = EventCx::new().with_ui_state(&rendered.ui);
        app.on_event(strip_click(Tool::Route), &cx);
        let start = pin_center(&app, "C1", "p1");
        app.on_event(
            pointer(UiEventKind::Click, px_of_board(&app, &rendered, start)),
            &cx,
        );
        let waypoint_px = px_of_board(
            &app,
            &rendered,
            Point {
                x: 7_400_000,
                y: 8_400_000,
            },
        );
        app.on_event(pointer(UiEventKind::Click, waypoint_px), &cx);
        (
            app.displayed_grid_pitch(PaneId::A),
            app.pending_route().expect("route").last_point(),
        )
    }

    let fine = waypoint_at(1e-5);
    let coarse = waypoint_at(1e-6);
    assert_eq!(fine.0, 1_000_000);
    assert_eq!(coarse.0, 10_000_000);
    assert_eq!((fine.1.x % fine.0, fine.1.y % fine.0), (0, 0));
    assert_eq!((coarse.1.x % coarse.0, coarse.1.y % coarse.0), (0, 0));
    assert_ne!(fine.1, coarse.1);
}

#[test]
fn nm_rounding_is_exact_across_zero_and_negative_coordinates() {
    let pitch = 10;
    assert_eq!(snap_nm(4, pitch), 0);
    assert_eq!(snap_nm(5, pitch), 10);
    assert_eq!(snap_nm(16, pitch), 20);
    assert_eq!(snap_nm(-4, pitch), 0);
    assert_eq!(snap_nm(-5, pitch), -10);
    assert_eq!(snap_nm(-16, pitch), -20);
    assert_eq!(
        snap_point(Point { x: -25, y: 25 }, pitch),
        Point { x: -30, y: 30 }
    );
}

#[test]
fn snap_menu_row_toggles_the_app_flag_and_checkmark() {
    fn contains_text(element: &El, text: &str) -> bool {
        element.text.as_deref() == Some(text)
            || element
                .children
                .iter()
                .any(|child| contains_text(child, text))
    }

    let mut app = edit_app();
    assert!(app.snap_to_grid(), "snap defaults on");
    app.set_open_menu(Some("view"));
    assert!(contains_text(&app.menu_overlay().expect("View menu"), "✓"));

    app.on_event(click(SNAP_TO_GRID_KEY), &EventCx::new());
    assert!(!app.snap_to_grid());
    app.set_open_menu(Some("view"));
    assert!(!contains_text(&app.menu_overlay().expect("View menu"), "✓"));
}
