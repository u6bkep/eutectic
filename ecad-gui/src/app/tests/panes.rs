//! Pane-tree tests: the view switcher, layout / maximize toggles, deferred
//! fits for hidden panes, and per-pane coordinate composition. Moved verbatim
//! from `app.rs` (gui-module-split).

use super::*;

/// The view switcher flips a pane's view kind.
#[test]
fn view_switcher_flips_pane_view() {
    let mut app = EcadApp::new(schematic_domain());
    assert_eq!(app.panes.borrow()[0].view, ViewKind::Board);
    let cx = EventCx::new();
    app.on_event(click(&PaneId::A.switch_key(ViewKind::Schematic)), &cx);
    assert_eq!(app.panes.borrow()[0].view, ViewKind::Schematic);
}

/// The layout toggle flips dual ↔ stacked; the maximize toggle sets/clears.
#[test]
fn layout_and_maximize_toggles() {
    let mut app = EcadApp::new(schematic_domain());
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

/// A pane hidden by maximize on its first frame must NOT be marked fitted — otherwise
/// its dropped FitContent request (damascene discards requests whose viewport is absent
/// this frame) would strand it at the default camera forever. On restore, the still
/// un-fitted pane must re-arm its fit. Regression for the stuck-`fitted` bug.
#[test]
fn hidden_pane_defers_its_fit_until_visible() {
    let mut app = EcadApp::new(schematic_domain());
    // Maximize B on the very first frame — A is hidden this frame.
    app.maximized.set(Some(PaneId::B));
    app.before_build();

    // Only the visible pane (B) queued a fit; the hidden pane (A) is still un-fitted.
    assert!(app.panes.borrow()[pane_index(PaneId::B)].fitted, "B fits");
    assert!(
        !app.panes.borrow()[pane_index(PaneId::A)].fitted,
        "hidden A must NOT be marked fitted (its request would be dropped)"
    );
    let reqs = app.drain_viewport_requests();
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            ViewportRequest::FitContent { key, .. } if key == PaneId::B.canvas_key()
        )),
        "B's fit was queued"
    );
    assert!(
        !reqs.iter().any(|r| matches!(
            r,
            ViewportRequest::FitContent { key, .. } if key == PaneId::A.canvas_key()
        )),
        "A's fit must NOT be queued while hidden"
    );

    // Restore the split; A is now visible and must fit on this frame.
    app.maximized.set(None);
    app.before_build();
    assert!(
        app.panes.borrow()[pane_index(PaneId::A)].fitted,
        "restored A must now fit"
    );
    let reqs = app.drain_viewport_requests();
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            ViewportRequest::FitContent { key, .. } if key == PaneId::A.canvas_key()
        )),
        "A's fit is queued once it becomes visible"
    );
}

/// Per-pane independence: the SAME screen pixel maps to DIFFERENT board points when the
/// two panes have different cameras — proving the pick composition uses the clicked
/// pane's own viewport view, not a shared one (the m2 bug class). And the same pixel
/// with the same camera but different pane RECTS also maps differently — proving the
/// rect is per-pane too.
#[test]
fn per_pane_composition_uses_the_clicked_panes_view_and_rect() {
    use damascene_core::viewport::ViewportView;
    let app = EcadApp::new(schematic_domain());
    let canvas = app.board_canvas_clone();

    let rect = (0.0f32, 0.0f32, 400.0f32, 300.0f32);
    let px = (100.0f32, 80.0f32);

    // Two different cameras (pane A vs pane B), same rect + pixel.
    let cam_a = ViewportView {
        pan: (0.0, 0.0),
        zoom: 1.0,
    };
    let cam_b = ViewportView {
        pan: (50.0, -30.0),
        zoom: 2.0,
    };
    let pa = pick::pointer_to_board_nm(&canvas, px, rect, cam_a).expect("a maps");
    let pb = pick::pointer_to_board_nm(&canvas, px, rect, cam_b).expect("b maps");
    assert_ne!(
        pa, pb,
        "same pixel under different pane cameras must map to different board points"
    );

    // Same camera, two different pane rects (dual split: A left, B right).
    let rect_a = (0.0f32, 0.0f32, 200.0f32, 300.0f32);
    let rect_b = (210.0f32, 0.0f32, 200.0f32, 300.0f32);
    let ra = pick::pointer_to_board_nm(&canvas, px, rect_a, cam_a).expect("ra maps");
    let rb = pick::pointer_to_board_nm(&canvas, px, rect_b, cam_a).expect("rb maps");
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
    let app = EcadApp::new(schematic_domain());
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
