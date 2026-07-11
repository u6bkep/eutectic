//! Camera-gesture regression tests over the REAL damascene input pass —
//! [`RunnerCore`]'s pointer_down / pointer_moved / pointer_up over the app's
//! real built + laid-out tree. WP2: the board pane is one keyed owned-canvas
//! container (no viewport El, no native pan gate), so every press inside the
//! pane routes to the app and the Select-tool camera pan drives the pane's
//! app-owned camera directly. These tests prove the whole chain: synthetic
//! winit-level input → damascene capture → app gesture → camera math.

use super::*;
use damascene_core::event::{Pointer, PointerButton};
use damascene_core::runtime::RunnerCore;
use damascene_core::state::UiState;

/// The native-input harness: the winit host's per-frame loop (build → push
/// requests → layout) against a persistent [`RunnerCore`], whose pointer
/// entry points synthesize the same capture decisions + `UiEvent`s the
/// windowed host dispatches. `render_settled`'s twin, plus real input.
struct Native {
    rt: RunnerCore,
    vp: Rect,
}

impl Native {
    /// Build + settle `app` exactly like [`crate::harness::render_settled`],
    /// but against the runtime's own persistent `UiState` so pointer input
    /// can be delivered afterwards.
    fn settled(app: &mut EutecticApp) -> Native {
        let mut n = Native {
            rt: RunnerCore::new(),
            vp: Rect::new(0.0, 0.0, 1280.0, 800.0),
        };
        let mut frames = 0;
        loop {
            let queued = n.frame(app);
            frames += 1;
            if (frames >= crate::harness::MIN_FRAMES && !queued)
                || frames >= crate::harness::MAX_FRAMES
            {
                break;
            }
        }
        n
    }

    /// One host frame: before_build → build (against the persistent UiState)
    /// → push the app's viewport requests → layout (which applies them) →
    /// snapshot the laid-out tree for hit-testing. Returns whether the app
    /// queued any viewport requests this frame (the settle signal).
    fn frame(&mut self, app: &mut EutecticApp) -> bool {
        app.before_build();
        let theme = app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(&self.rt.ui_state)
                .with_viewport(self.vp.w, self.vp.h);
            app.build(&cx)
        };
        let requests = app.drain_viewport_requests();
        let queued = !requests.is_empty();
        self.rt.ui_state.push_viewport_requests(requests);
        let _ = render_bundle_with_theme(&mut tree, &mut self.rt.ui_state, self.vp, &theme);
        self.rt.last_tree = Some(tree);
        queued
    }

    /// Dispatch runtime-synthesized events through the app, with the same
    /// `EventCx` the windowed host builds.
    fn dispatch(&self, app: &mut EutecticApp, events: Vec<UiEvent>) {
        for e in events {
            let cx = EventCx::new()
                .with_ui_state(&self.rt.ui_state)
                .with_viewport(self.vp.w, self.vp.h);
            app.on_event(e, &cx);
        }
    }

    fn press(&mut self, app: &mut EutecticApp, px: (f32, f32)) {
        let evs = self
            .rt
            .pointer_down(Pointer::mouse(px.0, px.1, PointerButton::Primary));
        self.dispatch(app, evs);
    }

    fn move_to(&mut self, app: &mut EutecticApp, px: (f32, f32)) {
        let m = self.rt.pointer_moved(Pointer::moving(px.0, px.1));
        self.dispatch(app, m.events);
    }

    fn release(&mut self, app: &mut EutecticApp, px: (f32, f32)) {
        let evs = self
            .rt
            .pointer_up(Pointer::mouse(px.0, px.1, PointerButton::Primary));
        self.dispatch(app, evs);
    }

    /// Pane A's laid-out canvas rect.
    fn rect_a(&self) -> Rect {
        self.rt
            .ui_state
            .rect_of_key(PaneId::A.canvas_key())
            .expect("pane A laid out")
    }
}

/// Map a board point to pane-A screen px against the Native harness's live
/// UiState, through the pane's app-owned camera (the twin of
/// `super::px_of_board`, which reads a `Rendered`).
fn px_of_board_ui(app: &EutecticApp, ui: &UiState, p: Point) -> (f32, f32) {
    let rect = ui.rect_of_key(PaneId::A.canvas_key()).expect("pane A");
    let cam = app.board_camera(PaneId::A);
    crate::app::board_pane::board_project(&cam, (rect.x, rect.y, rect.w, rect.h), p)
}

/// A board point inside the GND pour, away from both caps' pads (resolves to
/// the POUR — the "undraggable copper" press case).
fn pour_point() -> Point {
    Point {
        x: 10 * NM_PER_MM,
        y: 13 * NM_PER_MM,
    }
}

/// SYMPTOM-2 REPRO (pan-from-pour), preserved through WP2: with the Select
/// tool, a drag that starts over the pour — bare board copper, not a
/// component — must pan the camera. The pan now drives the pane's app-owned
/// camera: a 60 px drag moves the camera center by exactly 60 px / zoom
/// (the board tracks the pointer).
#[test]
fn select_drag_on_pour_pans_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let cam0 = app.board_camera(PaneId::A);

    let from = px_of_board_ui(&app, &n.rt.ui_state, pour_point());
    // Drag straight down 60 px.
    let to = (from.0, from.1 + 60.0);

    n.press(&mut app, from);
    assert!(!app.drag_active(), "a pour press must not arm a part drag");
    n.move_to(&mut app, to);

    let cam1 = app.board_camera(PaneId::A);
    // Screen y down + board y up: dragging DOWN moves the camera center UP
    // in board space by 60 px / zoom.
    let want_dy = 60.0 / cam0.zoom;
    assert!(
        (cam1.center.1 - cam0.center.1 - want_dy).abs() * cam0.zoom < 1.0,
        "a 60 px Select-drag from the pour must pan the camera 60 px \
         (center {:?} -> {:?}, want +{want_dy:.0} nm in y)",
        cam0.center,
        cam1.center
    );
    assert_eq!(cam1.zoom, cam0.zoom, "a pan never changes zoom");

    // Release: the drag was a pan, so nothing committed and the trailing
    // Click must not select the copper under the drop point.
    n.release(&mut app, to);
    assert!(!app.dirty(), "a camera pan commits nothing");
    assert!(
        app.domain.selection.borrow().single().is_none(),
        "the trailing Click of a pan must not select the pour"
    );
}

/// Pan-from-anywhere, including the pane gutter beyond the board: with the
/// owned canvas there is no native viewport pan and no content-rect seam —
/// the whole pane is one keyed surface container, so a press on bare canvas
/// arms the same app camera pan the pour press arms. (Pre-WP2 the gutter was
/// the one place damascene's native pan engaged; the app camera now owns the
/// gesture everywhere.)
#[test]
fn select_drag_in_gutter_pans_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let rect = n.rect_a();
    let cam0 = app.board_camera(PaneId::A);

    // Bottom-left corner of the pane: outside the fitted board's footprint
    // (the fit centers the board with padding), i.e. empty canvas.
    let px = (rect.x + 8.0, rect.y + rect.h - 8.0);
    let p = crate::app::board_pane::board_unproject(&cam0, (rect.x, rect.y, rect.w, rect.h), px);
    {
        let derived = app.derived.borrow();
        let view = derived.board.as_ref().unwrap();
        assert!(
            crate::canvas::pick::resolve(&view.candidates, p, 0, |_| true).is_none(),
            "test point must be empty canvas, hit something at {p:?}"
        );
    }

    n.press(&mut app, px);
    n.move_to(&mut app, (px.0 + 40.0, px.1 - 30.0));
    let cam1 = app.board_camera(PaneId::A);
    assert!(
        (cam1.center.0 - (cam0.center.0 - 40.0 / cam0.zoom)).abs() * cam0.zoom < 1.0
            && (cam1.center.1 - (cam0.center.1 - 30.0 / cam0.zoom)).abs() * cam0.zoom < 1.0,
        "a gutter drag pans the app camera (center {:?} -> {:?})",
        cam0.center,
        cam1.center
    );
    n.release(&mut app, (px.0 + 40.0, px.1 - 30.0));
}

/// A drag starting on a PART still drags the part (m6a behavior, unchanged):
/// the pick wins over the camera pan, the move commits on release, and the
/// camera never moves.
#[test]
fn part_drag_still_drags_and_never_pans() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let cam0 = app.board_camera(PaneId::A);
    let comp = EntityId::new("C1");
    let grab = px_of_board_ui(&app, &n.rt.ui_state, pad_center_of(&app, &comp));

    n.press(&mut app, grab);
    assert!(app.drag_active(), "a pad press arms the component drag");
    n.move_to(&mut app, (grab.0 + 40.0, grab.1 + 25.0));
    n.frame(&mut app);
    assert_eq!(
        app.board_camera(PaneId::A),
        cam0,
        "a component drag must not pan the camera"
    );
    n.release(&mut app, (grab.0 + 40.0, grab.1 + 25.0));
    assert!(!app.drag_active());
    assert!(app.dirty(), "the moved part committed");
    assert_eq!(
        app.board_camera(PaneId::A),
        cam0,
        "still no camera movement"
    );
}

/// Selection still works as a plain click everywhere, including pours: a
/// press-release with no movement selects the pour and moves nothing.
#[test]
fn click_selects_pour_without_panning() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let cam0 = app.board_camera(PaneId::A);
    let px = px_of_board_ui(&app, &n.rt.ui_state, pour_point());

    n.press(&mut app, px);
    n.release(&mut app, px);
    n.frame(&mut app);

    match app.domain.selection.borrow().single() {
        Some(SemanticId::Pour { .. }) => {}
        other => panic!("an un-moved press-release on the pour selects it, got {other:?}"),
    }
    assert_eq!(
        app.board_camera(PaneId::A),
        cam0,
        "a plain click must not pan"
    );
    assert!(!app.dirty());
}

/// With the Route tool active, the canvas gesture stays the tool's: a click
/// on a pin starts a route and a drag pans nothing (tool gestures keep
/// priority over the Select-tool camera pan).
#[test]
fn route_tool_gestures_keep_canvas_priority() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let cx = EventCx::new().with_ui_state(&n.rt.ui_state);
    app.on_event(click(&PaneId::A.strip_key(Tool::Route)), &cx);
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);
    let cam0 = app.board_camera(PaneId::A);

    let comp = EntityId::new("C1");
    let pin_px = px_of_board_ui(&app, &n.rt.ui_state, pad_center_of(&app, &comp));
    n.press(&mut app, pin_px);
    n.move_to(&mut app, (pin_px.0 + 30.0, pin_px.1 + 30.0));
    n.release(&mut app, (pin_px.0 + 30.0, pin_px.1 + 30.0));
    n.frame(&mut app);
    assert_eq!(
        app.board_camera(PaneId::A),
        cam0,
        "Route-tool pointer work must not arm the Select camera pan"
    );
}

/// The toolbar Fit / Reset buttons drive the board camera requests, applied
/// on the next build: Reset restores the 1 px/mm framing, Fit re-frames the
/// scene bounds. (The grid furniture itself is procedural in the renderer —
/// `render::gpu::grid_params` pins its ladder/coverage; the old
/// grid-asset-window tests died with the grid cache.)
#[test]
fn toolbar_fit_and_reset_drive_board_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let fitted = app.board_camera(PaneId::A);

    {
        let cx = EventCx::new().with_ui_state(&n.rt.ui_state);
        app.on_event(click("reset"), &cx);
    }
    n.frame(&mut app); // build consumes the request (glide retargets)
    let t = app.board_camera_target(PaneId::A);
    assert_eq!(
        t.zoom,
        crate::app::board_pane::RESET_ZOOM,
        "reset targets 1 px/mm"
    );

    {
        let cx = EventCx::new().with_ui_state(&n.rt.ui_state);
        app.on_event(click("fit"), &cx);
    }
    n.frame(&mut app);
    let t = app.board_camera_target(PaneId::A);
    assert!(
        (t.zoom - fitted.zoom).abs() / fitted.zoom < 1e-9,
        "fit re-frames the scene bounds (zoom {} vs fitted {})",
        t.zoom,
        fitted.zoom
    );
}

/// Wheel zoom-at-cursor through the real wheel-event path (`on_wheel_event`
/// consumes wheel over a board pane): the board point under the cursor is
/// unchanged from tick through settle, and the schematic pane's wheel stays
/// unconsumed (native viewport zoom).
#[test]
fn wheel_over_board_zooms_at_cursor_and_is_consumed() {
    let mut app = edit_app();
    let n = Native::settled(&mut app);
    let rect = n.rect_a();
    let pos = (rect.x + rect.w * 0.7, rect.y + rect.h * 0.3);
    let cam0 = app.board_camera(PaneId::A);
    let anchor =
        crate::app::board_pane::board_unproject(&cam0, (rect.x, rect.y, rect.w, rect.h), pos);

    let mut e = UiEvent::synthetic_click(PaneId::A.canvas_key());
    e.kind = UiEventKind::PointerWheel;
    e.pointer = Some(pos);
    e.wheel_delta = Some((0.0, -50.0));
    let cx = EventCx::new()
        .with_ui_state(&n.rt.ui_state)
        .with_viewport(n.vp.w, n.vp.h);
    assert!(
        app.on_wheel_event(e, &cx),
        "wheel over a board pane is consumed by the owned camera"
    );
    // Settle the glide and re-check the anchor.
    {
        let mut cams = app.board_cams.borrow_mut();
        while !cams[0].glide.settled() {
            cams[0].glide.step(1.0 / 120.0);
        }
    }
    let cam1 = app.board_camera(PaneId::A);
    assert!(cam1.zoom > cam0.zoom, "scroll up zooms in");
    let now = crate::app::board_pane::board_unproject(&cam1, (rect.x, rect.y, rect.w, rect.h), pos);
    let err_px = (((now.x - anchor.x) as f64).hypot((now.y - anchor.y) as f64)) * cam1.zoom;
    assert!(
        err_px < 1.0,
        "the board point under the cursor must survive the whole zoom ({err_px:.2} px off)"
    );
}
