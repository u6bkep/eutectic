//! Camera-gesture + canvas-furniture regression tests (the windowed-testing
//! bug pair): Select-tool drag pans the camera from anywhere that is not a
//! component, and the dot grid covers the visible viewport at every camera.
//!
//! These drive the REAL damascene input pass — [`RunnerCore`]'s pointer_down /
//! pointer_moved / pointer_up over the app's real built + laid-out tree — so
//! they exercise the exact native-pan gate the windowed host runs:
//! `runtime.rs` only begins a native viewport pan when the press hits nothing
//! or the viewport's own node. Every keyed canvas child (`layer:*` / `grid:*`
//! / `overlay:*` vector El) spans the full content viewBox rect, so any press
//! inside the content rect suppresses the native pan and the events flow to
//! the app instead — which is why pan appeared dead "on the pcb": zoomed in,
//! the content rect covers the whole pane and the app armed no camera gesture.

use super::*;
use crate::canvas::grid_pitch_mm;
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

    /// Pane A's live camera.
    fn view_a(&self) -> damascene_core::viewport::ViewportView {
        self.rt
            .ui_state
            .viewport_view_by_key(PaneId::A.canvas_key())
            .expect("pane A laid out")
    }

    /// Pane A's laid-out viewport rect.
    fn rect_a(&self) -> Rect {
        self.rt
            .ui_state
            .rect_of_key(PaneId::A.canvas_key())
            .expect("pane A laid out")
    }
}

/// Map a board point to pane-A screen px against the Native harness's live
/// UiState (the twin of `super::px_of_board`, which reads a `Rendered`).
fn px_of_board_ui(app: &EutecticApp, ui: &UiState, p: Point) -> (f32, f32) {
    let canvas = app.board_canvas_clone();
    let rect = ui.rect_of_key(PaneId::A.canvas_key()).expect("pane A");
    let vv = ui
        .viewport_view_by_key(PaneId::A.canvas_key())
        .expect("pane A view");
    let mm = (p.x as f32 / NM_PER_MM as f32, p.y as f32 / NM_PER_MM as f32);
    let content = canvas
        .board_mm_to_content_px(mm, canvas.content_rect((rect.x, rect.y, rect.w, rect.h)))
        .expect("maps");
    vv.project(content, (rect.x, rect.y))
}

/// A board point inside the GND pour, away from both caps' pads (same point
/// `pointer_down_on_empty_board_arms_nothing` uses — resolves to the POUR).
fn pour_point() -> Point {
    Point {
        x: 10 * NM_PER_MM,
        y: 13 * NM_PER_MM,
    }
}

/// A board point in the content-rect margin: inside the shared viewBox
/// (content bounds run to (22, 17) mm on this fixture) but OFF the board
/// (0..20 × 0..15 mm) and off all copper — the "grid region off-board".
/// A press here still hits a keyed canvas child (every layer El spans the
/// full viewBox rect), so the native pan gate is suppressed exactly as it
/// is over the pour.
fn margin_point() -> Point {
    Point {
        x: 21 * NM_PER_MM,
        y: 16 * NM_PER_MM,
    }
}

/// SYMPTOM-2 REPRO (pan-from-pour): with the Select tool, a drag that starts
/// over the pour — bare board copper, not a component — must pan the camera.
/// Pre-fix nothing moved it: damascene's native pan is gated off by the keyed
/// layer El under the press, and the app armed no gesture for a non-component
/// press.
#[test]
fn select_drag_on_pour_pans_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let pan0 = n.view_a().pan;

    let from = px_of_board_ui(&app, &n.rt.ui_state, pour_point());
    // Drag straight down 60 px: the fitted board is width-tight in the pane,
    // so vertical Contain slack is ample and the clamp never bites.
    let to = (from.0, from.1 + 60.0);

    n.press(&mut app, from);
    assert!(!app.drag_active(), "a pour press must not arm a part drag");
    n.move_to(&mut app, to);
    // The app's pan lands as a viewport request; the next frame applies it.
    n.frame(&mut app);

    let pan1 = n.view_a().pan;
    assert!(
        (pan1.1 - pan0.1 - 60.0).abs() < 1.0,
        "a 60 px Select-drag from the pour must pan the camera 60 px \
         (pan {pan0:?} -> {pan1:?})"
    );

    // Release: the drag was a pan, so nothing committed and the trailing
    // Click must not select the copper under the drop point.
    n.release(&mut app, to);
    assert!(!app.dirty(), "a camera pan commits nothing");
    assert!(
        app.domain.selection.borrow().single().is_none(),
        "the trailing Click of a pan must not select the pour"
    );
}

/// SYMPTOM-2 REPRO (pan-from-grid-region): same gesture starting off-board,
/// over the grid furniture inside the content rect — still a keyed-El press,
/// still must pan.
#[test]
fn select_drag_on_grid_region_pans_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let pan0 = n.view_a().pan;

    let from = px_of_board_ui(&app, &n.rt.ui_state, margin_point());
    let to = (from.0, from.1 - 50.0);
    n.press(&mut app, from);
    n.move_to(&mut app, to);
    n.frame(&mut app);

    let pan1 = n.view_a().pan;
    assert!(
        (pan1.1 - pan0.1 + 50.0).abs() < 1.0,
        "a Select-drag from the off-board grid region must pan the camera \
         (pan {pan0:?} -> {pan1:?})"
    );
}

/// Control (documents the pre-existing mechanism, must keep working): a press
/// in the pane gutter BEYOND the content rect hits no keyed child, so
/// damascene's own default-trigger pan captures the drag before the app sees
/// any event — the one place pan already worked pre-fix.
#[test]
fn gutter_press_pans_natively() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let rect = n.rect_a();
    let vv = n.view_a();
    let pan0 = vv.pan;

    // Bottom-left corner of the pane: verify it is genuinely outside the
    // projected content rect (the fit centers the board with padding).
    let canvas = app.board_canvas_clone();
    let (cx0, cy0, cw, ch) = canvas.content_rect((rect.x, rect.y, rect.w, rect.h));
    let tl = vv.project((cx0, cy0), (rect.x, rect.y));
    let content_on_screen = (tl.0, tl.1, cw * vv.zoom, ch * vv.zoom);
    let px = (rect.x + 8.0, rect.y + rect.h - 8.0);
    assert!(
        px.0 < content_on_screen.0 || px.1 > content_on_screen.1 + content_on_screen.3,
        "test point {px:?} must lie in the gutter outside the content rect \
         {content_on_screen:?}"
    );

    let evs =
        n.rt.pointer_down(Pointer::mouse(px.0, px.1, PointerButton::Primary));
    assert!(
        evs.is_empty(),
        "a gutter press is captured by the native pan (no app events), got {evs:?}"
    );
    n.move_to(&mut app, (px.0 + 40.0, px.1 - 30.0));
    let pan1 = n.view_a().pan;
    assert!(
        pan1 != pan0,
        "the native pan drives the camera directly (pan {pan0:?} -> {pan1:?})"
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
    let pan0 = n.view_a().pan;
    let comp = EntityId::new("C1");
    let grab = px_of_board_ui(&app, &n.rt.ui_state, pad_center_of(&app, &comp));

    n.press(&mut app, grab);
    assert!(app.drag_active(), "a pad press arms the component drag");
    n.move_to(&mut app, (grab.0 + 40.0, grab.1 + 25.0));
    n.frame(&mut app);
    assert_eq!(
        n.view_a().pan,
        pan0,
        "a component drag must not pan the camera"
    );
    n.release(&mut app, (grab.0 + 40.0, grab.1 + 25.0));
    assert!(!app.drag_active());
    assert!(app.dirty(), "the moved part committed");
    assert_eq!(n.view_a().pan, pan0, "still no camera movement");
}

/// Selection still works as a plain click everywhere, including pours: a
/// press-release with no movement selects the pour and moves nothing.
#[test]
fn click_selects_pour_without_panning() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);
    let pan0 = n.view_a().pan;
    let px = px_of_board_ui(&app, &n.rt.ui_state, pour_point());

    n.press(&mut app, px);
    n.release(&mut app, px);
    n.frame(&mut app);

    match app.domain.selection.borrow().single() {
        Some(SemanticId::Pour { .. }) => {}
        other => panic!("an un-moved press-release on the pour selects it, got {other:?}"),
    }
    assert_eq!(n.view_a().pan, pan0, "a plain click must not pan");
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
    let pan0 = n.view_a().pan;

    let comp = EntityId::new("C1");
    let pin_px = px_of_board_ui(&app, &n.rt.ui_state, pad_center_of(&app, &comp));
    n.press(&mut app, pin_px);
    n.move_to(&mut app, (pin_px.0 + 30.0, pin_px.1 + 30.0));
    n.release(&mut app, (pin_px.0 + 30.0, pin_px.1 + 30.0));
    n.frame(&mut app);
    assert_eq!(
        n.view_a().pan,
        pan0,
        "Route-tool pointer work must not arm the Select camera pan"
    );
}

// ---------------------------------------------------------------------------
// SYMPTOM 1: grid coverage.
// ---------------------------------------------------------------------------

/// The dot-grid asset of pane A in the last laid-out tree.
fn grid_asset_of(tree: &El) -> std::sync::Arc<damascene_core::vector::VectorAsset> {
    fn walk(el: &El) -> Option<std::sync::Arc<damascene_core::vector::VectorAsset>> {
        if el.key.as_deref() == Some("grid:canvas:a") {
            return el.vector_source.clone();
        }
        el.children.iter().find_map(walk)
    }
    walk(tree).expect("pane A renders a grid El")
}

/// The bbox of every path point in a vector path, in viewBox (view-mm) space.
fn path_bbox(path: &damascene_core::vector::VectorPath) -> (f32, f32, f32, f32) {
    use damascene_core::vector::VectorSegment;
    let mut bb = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    let mut fold = |p: &[f32; 2]| {
        bb.0 = bb.0.min(p[0]);
        bb.1 = bb.1.min(p[1]);
        bb.2 = bb.2.max(p[0]);
        bb.3 = bb.3.max(p[1]);
    };
    for s in &path.segments {
        match s {
            VectorSegment::MoveTo(p) | VectorSegment::LineTo(p) => fold(p),
            VectorSegment::QuadTo(c, p) => {
                fold(c);
                fold(p);
            }
            VectorSegment::CubicTo(c1, c2, p) => {
                fold(c1);
                fold(c2);
                fold(p);
            }
            VectorSegment::Close => {}
        }
    }
    bb
}

/// SYMPTOM-1 REPRO: at a camera where the old content-anchored overscan runs
/// out — the reset view, board a small box in a huge pane — the dot field
/// must still cover the entire visible viewport (to within one pitch), and
/// the origin axes must span at least the visible rect. The oracle's grid
/// fills the whole canvas at every pan/zoom.
#[test]
fn grid_covers_visible_viewport_at_reset_camera() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);

    // Reset pane cameras (zoom 1, pan 0 — the board is a ~24×19 px box in a
    // ~600×650 px pane, far beyond any content-anchored overscan).
    let cx = EventCx::new().with_ui_state(&n.rt.ui_state);
    app.on_event(click("reset"), &cx);
    n.frame(&mut app); // applies the reset
    n.frame(&mut app); // rebuilds the grid against the reset camera

    let rect = n.rect_a();
    let vv = n.view_a();
    let canvas = app.board_canvas_clone();
    let el_rect = canvas.content_rect((rect.x, rect.y, rect.w, rect.h));

    // The visible viewport in BOARD mm: unproject the pane corners.
    let corner = |px: (f32, f32)| {
        let content = vv.unproject(px, (rect.x, rect.y));
        canvas
            .content_px_to_board_mm(content, el_rect)
            .expect("maps")
    };
    let a = corner((rect.x, rect.y));
    let b = corner((rect.x + rect.w, rect.y + rect.h));
    let visible = (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1));

    // The dot field's bbox in BOARD mm (path points are view-mm; the y-flip
    // maps them back through the canvas's own inverse).
    let asset = grid_asset_of(n.rt.last_tree.as_ref().unwrap());
    let dots = &asset.paths[0];
    let vb = path_bbox(dots);
    let d0 = canvas.view_to_board_mm((vb.0, vb.1));
    let d1 = canvas.view_to_board_mm((vb.2, vb.3));
    let dots_mm = (
        d0.0.min(d1.0),
        d0.1.min(d1.1),
        d0.0.max(d1.0),
        d0.1.max(d1.1),
    );

    let pitch = grid_pitch_mm(vv.zoom);
    assert!(
        dots_mm.0 <= visible.0 + pitch
            && dots_mm.1 <= visible.1 + pitch
            && dots_mm.2 >= visible.2 - pitch
            && dots_mm.3 >= visible.3 - pitch,
        "the dot field {dots_mm:?} mm must cover the visible viewport \
         {visible:?} mm to within one pitch ({pitch} mm) — the grid ran out"
    );

    // The origin axes span at least the visible rect: the vertical axis
    // (board x = 0) and horizontal axis (board y = 0) are in view here, and
    // each stroked axis path must span the visible extent on its axis.
    let axes: Vec<_> = asset.paths[1..].iter().collect();
    assert!(!axes.is_empty(), "origin axes render at the reset camera");
    let spans_y = axes.iter().any(|p| {
        let bb = path_bbox(p);
        let (y0, y1) = {
            let p0 = canvas.view_to_board_mm((bb.0, bb.1)).1;
            let p1 = canvas.view_to_board_mm((bb.2, bb.3)).1;
            (p0.min(p1), p0.max(p1))
        };
        y0 <= visible.1 + pitch && y1 >= visible.3 - pitch
    });
    let spans_x = axes.iter().any(|p| {
        let bb = path_bbox(p);
        bb.0 <= visible.0 + pitch && bb.2 >= visible.2 - pitch
    });
    assert!(
        spans_x && spans_y,
        "each origin axis must span at least the visible rect"
    );
}

/// The derived-state discipline: panning by less than the built window's
/// margin re-emits a byte-identical grid asset (a cache hit — `content_hash`
/// dedupes the GPU upload), so the per-frame cost is a clone, not a
/// re-tessellation. Only escaping the window (or a pitch change) rebuilds.
#[test]
fn grid_asset_is_cache_stable_across_small_pans() {
    let mut app = edit_app();
    let mut n = Native::settled(&mut app);

    let hash0 = grid_asset_of(n.rt.last_tree.as_ref().unwrap()).content_hash();

    // A small pan: drag the pour down 30 px (well inside the half-viewport
    // window margin) and re-render.
    let from = px_of_board_ui(&app, &n.rt.ui_state, pour_point());
    n.press(&mut app, from);
    n.move_to(&mut app, (from.0, from.1 + 30.0));
    n.release(&mut app, (from.0, from.1 + 30.0));
    n.frame(&mut app);
    n.frame(&mut app);

    let hash1 = grid_asset_of(n.rt.last_tree.as_ref().unwrap()).content_hash();
    assert_eq!(
        hash0, hash1,
        "a small pan must be a grid-cache hit (identical asset, deduped upload)"
    );
}
