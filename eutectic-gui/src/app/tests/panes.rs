//! Pane-tree tests: the view switcher, layout / maximize toggles, deferred
//! fits for hidden panes, and per-pane coordinate composition. Moved verbatim
//! from `app.rs` (gui-module-split).

use super::*;

/// The view switcher flips a pane's view kind.
#[test]
fn view_switcher_flips_pane_view() {
    let mut app = EutecticApp::new(schematic_domain());
    assert_eq!(app.panes.borrow()[0].view, ViewKind::Board);
    let cx = EventCx::new();
    app.on_event(click(&PaneId::A.switch_key(ViewKind::Schematic)), &cx);
    assert_eq!(app.panes.borrow()[0].view, ViewKind::Schematic);
}

/// The layout toggle flips dual ↔ stacked; the maximize toggle sets/clears.
#[test]
fn layout_and_maximize_toggles() {
    let mut app = EutecticApp::new(schematic_domain());
    let cx = EventCx::new();
    assert_eq!(app.layout.get(), PaneLayout::Dual);
    app.on_event(click(LAYOUT_TOGGLE_KEY), &cx);
    assert_eq!(app.layout.get(), PaneLayout::Stacked);

    assert_eq!(app.maximized.get(), None);
    app.on_event(click(PaneId::B.maximize_key()), &cx);
    assert_eq!(app.maximized.get(), Some(PaneId::B));
    app.on_event(click(PaneId::B.maximize_key()), &cx);
    assert_eq!(app.maximized.get(), None, "toggling again restores");
}

/// A pane hidden by maximize on its first frame must NOT be marked fitted —
/// otherwise on restore it would render with the default camera and never
/// re-fit. On restore, the still un-fitted pane must fit on its first
/// visible frame. Regression for the stuck-`fitted` bug — WP2 shape: the
/// board pane (A)'s fit is app-camera math applied in `build` against its
/// laid-out rect; the schematic pane (B) still rides the FitContent queue.
#[test]
fn hidden_pane_defers_its_fit_until_visible() {
    let mut app = EutecticApp::new(schematic_domain());
    // Maximize B on the very first frame — A (board) is hidden.
    app.maximized.set(Some(PaneId::B));
    let _ = settle(&mut app);
    assert!(app.panes.borrow()[pane_index(PaneId::B)].fitted, "B fits");
    assert!(
        !app.panes.borrow()[pane_index(PaneId::A)].fitted,
        "hidden A must NOT be marked fitted (it has no rect to fit against)"
    );
    let unfitted = crate::app::board_pane::zoom_px_per_mm(&app.board_camera(PaneId::A));
    assert!(
        (unfitted - 1.0).abs() < 1e-3,
        "hidden A's camera stays at the reset zoom, got {unfitted}"
    );

    // Restore the split; A is now visible and must fit on its first visible
    // build (frame 2 of the settle — the rect exists from frame 1's layout).
    app.maximized.set(None);
    let _ = settle(&mut app);
    assert!(
        app.panes.borrow()[pane_index(PaneId::A)].fitted,
        "restored A must now fit"
    );
    let fitted = crate::app::board_pane::zoom_px_per_mm(&app.board_camera(PaneId::A));
    assert!(
        (fitted - 1.0).abs() > 1e-3,
        "restored A's camera actually fitted (zoom {fitted})"
    );
}

/// Per-pane independence: the SAME screen pixel maps to DIFFERENT board points when the
/// two panes have different cameras — proving the pick composition uses the clicked
/// pane's own app camera, not a shared one (the m2 bug class). And the same pixel
/// with the same camera but different pane RECTS also maps differently — proving the
/// rect is per-pane too.
#[test]
fn per_pane_composition_uses_the_clicked_panes_camera_and_rect() {
    use crate::app::board_pane::board_unproject;
    use crate::render::Camera;

    let rect = (0.0f32, 0.0f32, 400.0f32, 300.0f32);
    let px = (100.0f32, 80.0f32);

    // Two different cameras (pane A vs pane B), same rect + pixel.
    let cam_a = Camera::new((5e6, 5e6), 1e-6);
    let cam_b = Camera::new((9e6, 2e6), 2e-6);
    let pa = board_unproject(&cam_a, rect, px);
    let pb = board_unproject(&cam_b, rect, px);
    assert_ne!(
        pa, pb,
        "same pixel under different pane cameras must map to different board points"
    );

    // Same camera, two different pane rects (dual split: A left, B right).
    let rect_a = (0.0f32, 0.0f32, 200.0f32, 300.0f32);
    let rect_b = (210.0f32, 0.0f32, 200.0f32, 300.0f32);
    let ra = board_unproject(&cam_a, rect_a, px);
    let rb = board_unproject(&cam_a, rect_b, px);
    assert_ne!(
        ra, rb,
        "same pixel under different pane rects must map to different board points"
    );
}

/// Two board panes over the same doc lay out with DISTINCT, non-overlapping rects and
/// distinct viewport keys — the structural prerequisite for independent cameras.
#[test]
fn dual_boards_lay_out_as_two_independent_panes() {
    use damascene_core::layout::layout;
    use damascene_core::prelude::Rect;
    use damascene_core::state::UiState;

    let app = dual_boards();
    let theme = app.theme();
    let cx = BuildCx::new(&theme).with_viewport(1280.0, 800.0);
    let mut root = app.build(&cx);
    let mut ui = UiState::new();
    layout(&mut root, &mut ui, Rect::new(0.0, 0.0, 1280.0, 800.0));

    let ra = ui
        .rect_of_key(PaneId::A.canvas_key())
        .expect("pane A canvas laid out");
    let rb = ui
        .rect_of_key(PaneId::B.canvas_key())
        .expect("pane B canvas laid out");
    // Distinct rects, side by side (dual = row): A's right edge is left of B's left.
    assert!(
        ra.x + ra.w <= rb.x + 1.0,
        "dual board panes must be side by side, got A={ra:?} B={rb:?}"
    );
    assert!(ra.w > 0.0 && rb.w > 0.0);
}

/// A schematic-only pane over a schematic-block doc renders its viewport (not a
/// placeholder), and the poc board's schematic pane builds without panic.
#[test]
fn schematic_pane_renders_for_a_schematic_doc() {
    let app = EutecticApp::new(schematic_domain());
    assert!(
        app.has_schematic(),
        "a doc with components must project a schematic"
    );
    // The schematic projection has pick candidates (built once per load).
    let doc = app.domain.doc.as_ref().unwrap();
    let view = SchematicView::build(doc, &app.domain.lib).expect("schematic projects");
    assert!(!view.candidates().is_empty());
    let _ = MM; // (kept for symmetry with other tests' unit imports)
}
