//! Recursive pane-tree tests: stable leaf identity, split/close actions,
//! maximize, nested dividers, and per-pane coordinate composition.

use super::*;

/// The pane-header dropdown opens at the root and flips its leaf's view kind.
#[test]
fn view_dropdown_opens_and_flips_pane_view() {
    let mut app = EutecticApp::new(schematic_domain());
    assert_eq!(app.pane_view(PaneId::A), ViewKind::Board);
    let cx = EventCx::new();
    app.on_event(click(&PaneId::A.view_select_key()), &cx);
    assert_eq!(app.pane_view_menu.get(), Some(PaneId::A));
    let overlay = app.pane_view_overlay().expect("dropdown is open");
    assert!(tree_has_key(
        &overlay,
        &PaneId::A.switch_key(ViewKind::Schematic)
    ));
    app.on_event(click(&PaneId::A.switch_key(ViewKind::Schematic)), &cx);
    assert_eq!(app.pane_view(PaneId::A), ViewKind::Schematic);
    assert_eq!(app.pane_view_menu.get(), None);
}

fn node_shape(node: &crate::app::pane::PaneNode) -> String {
    use crate::app::pane::PaneNode;
    match node {
        PaneNode::Leaf(id) => format!("p{}", pane_index(*id)),
        PaneNode::Split {
            axis,
            first,
            second,
            ..
        } => format!(
            "{}({},{})",
            match axis {
                SplitAxis::Horizontal => "h",
                SplitAxis::Vertical => "v",
            },
            node_shape(first),
            node_shape(second)
        ),
    }
}

fn tree_has_key(el: &El, key: &str) -> bool {
    el.key.as_deref() == Some(key) || el.children.iter().any(|child| tree_has_key(child, key))
}

fn split_weights(app: &EutecticApp, id: crate::app::pane::SplitId) -> [f32; 2] {
    let mut tree = app.pane_tree.borrow_mut();
    let (_, weights, _) = tree.root.split_mut(id).unwrap();
    *weights
}

/// Startup pins the original board | schematic geometry and camera defaults.
#[test]
fn default_startup_arrangement_is_pinned() {
    let app = EutecticApp::new(schematic_domain());
    assert_eq!(app.pane_ids(), vec![PaneId::A, PaneId::B]);
    assert_eq!(app.pane_view(PaneId::A), ViewKind::Board);
    assert_eq!(app.pane_view(PaneId::B), ViewKind::Schematic);
    assert_eq!(node_shape(&app.pane_tree.borrow().root), "h(p0,p1)");
    assert_eq!(app.pane_camera(PaneId::A), app.pane_camera(PaneId::B));
    let crate::app::pane::PaneNode::Split { weights, id, .. } = &app.pane_tree.borrow().root else {
        panic!("default root must be a split");
    };
    assert_eq!(*weights, [1.0, 1.0]);
    assert_eq!(id.handle_key(), "pane:split");
    assert_eq!(id.container_key(), "pane:split-row");
}

/// Header actions make the requested recursive shapes and inherit the source
/// view/camera exactly; maximize still hides/restores the rest of the tree.
#[test]
fn split_right_and_down_inherit_view_camera_and_maximize_restores() {
    let mut app = EutecticApp::new(schematic_domain());
    let cx = EventCx::new();
    app.pane_snap_center(PaneId::A, (12_000_000.0, 34_000_000.0));
    let inherited = app.pane_camera(PaneId::A);
    app.on_event(click(&PaneId::A.split_right_key()), &cx);
    let c = app.focused_pane.get();
    assert_eq!(node_shape(&app.pane_tree.borrow().root), "h(h(p0,p2),p1)");
    assert_eq!(app.pane_view(c), ViewKind::Board);
    assert_eq!(app.pane_camera(c), inherited);
    app.on_event(click(&c.split_down_key()), &cx);
    let d = app.focused_pane.get();
    assert_eq!(
        node_shape(&app.pane_tree.borrow().root),
        "h(h(p0,v(p2,p3)),p1)"
    );
    assert_eq!(app.pane_camera(d), inherited);

    assert_eq!(app.maximized.get(), None);
    app.on_event(click(&PaneId::B.maximize_key()), &cx);
    assert_eq!(app.maximized.get(), Some(PaneId::B));
    let theme = app.theme();
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    assert!(tree_has_key(&root, PaneId::B.canvas_key()));
    for hidden in [PaneId::A, c, d] {
        assert!(!tree_has_key(&root, hidden.canvas_key()));
    }
    app.on_event(click(&PaneId::B.maximize_key()), &cx);
    assert_eq!(app.maximized.get(), None, "toggling again restores");
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    for visible in [PaneId::A, PaneId::B, c, d] {
        assert!(tree_has_key(&root, visible.canvas_key()));
    }
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
    assert!(
        app.panes.borrow()[pane_index(PaneId::B)]
            .as_ref()
            .unwrap()
            .fitted,
        "B fits"
    );
    assert!(
        !app.panes.borrow()[pane_index(PaneId::A)]
            .as_ref()
            .unwrap()
            .fitted,
        "hidden A must NOT be marked fitted (it has no rect to fit against)"
    );
    let unfitted = crate::app::canvas_pane::zoom_px_per_mm(&app.pane_camera(PaneId::A));
    assert!(
        (unfitted - 1.0).abs() < 1e-3,
        "hidden A's camera stays at the reset zoom, got {unfitted}"
    );

    // Restore the split; A is now visible and must fit on its first visible
    // build (frame 2 of the settle — the rect exists from frame 1's layout).
    app.maximized.set(None);
    let _ = settle(&mut app);
    assert!(
        app.panes.borrow()[pane_index(PaneId::A)]
            .as_ref()
            .unwrap()
            .fitted,
        "restored A must now fit"
    );
    let fitted = crate::app::canvas_pane::zoom_px_per_mm(&app.pane_camera(PaneId::A));
    assert!(
        (fitted - 1.0).abs() > 1e-3,
        "restored A's camera actually fitted (zoom {fitted})"
    );
}

#[test]
fn close_collapses_and_focuses_sibling_while_preserving_survivor_keys() {
    let mut app = EutecticApp::new(schematic_domain());
    let cx = EventCx::new();
    let b_camera = app.pane_camera(PaneId::B);
    let b_key = PaneId::B.canvas_key();
    app.on_event(click(&PaneId::A.split_right_key()), &cx);
    let c = app.focused_pane.get();
    assert_ne!(c, PaneId::A);
    assert_eq!(app.pane_camera(PaneId::B), b_camera);
    assert_eq!(PaneId::B.canvas_key(), b_key);

    app.on_event(click(&c.close_key()), &cx);
    assert_eq!(app.focused_pane.get(), PaneId::A);
    assert_eq!(node_shape(&app.pane_tree.borrow().root), "h(p0,p1)");
    assert!(app.panes.borrow()[pane_index(c)].is_none());
    assert_eq!(app.pane_camera(PaneId::B), b_camera);

    app.on_event(click(&PaneId::A.split_down_key()), &cx);
    assert_eq!(app.focused_pane.get(), c, "the freed stable slot is reused");
    assert_eq!(PaneId::B.canvas_key(), b_key, "survivor key never moved");
}

#[test]
fn raw_pointer_focus_follows_a_new_leaf() {
    let mut app = board();
    let pane = app.split_pane(PaneId::A, SplitAxis::Horizontal).unwrap();
    app.focused_pane.set(PaneId::B);
    app.pane_px.borrow_mut()[pane_index(pane)] = Some((20.0, 30.0, 200.0, 100.0));

    assert!(app.raw_cursor_moved((80.0, 70.0)));
    assert_eq!(app.focused_pane.get(), pane);
}

#[test]
fn pane_cap_and_last_close_disable_affordances_and_reject_events() {
    let mut app = EutecticApp::new(schematic_domain());
    let cx = EventCx::new();
    while app.pane_count() < crate::app::pane::MAX_PANES {
        app.on_event(click(SPLIT_RIGHT_KEY), &cx);
    }
    assert_eq!(app.pane_count(), crate::app::pane::MAX_PANES);
    app.on_event(click(SPLIT_DOWN_KEY), &cx);
    assert_eq!(app.pane_count(), crate::app::pane::MAX_PANES);
    let theme = app.theme();
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    for pane in app.pane_ids() {
        assert!(!tree_has_key(&root, &pane.split_right_key()));
        assert!(!tree_has_key(&root, &pane.split_down_key()));
    }
    app.set_open_menu(Some("view"));
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    assert!(!tree_has_key(&root, SPLIT_RIGHT_KEY));
    assert!(!tree_has_key(&root, SPLIT_DOWN_KEY));
    app.set_open_menu(None);

    while app.pane_count() > 1 {
        app.on_event(click(CLOSE_PANE_KEY), &cx);
    }
    let last = app.focused_pane.get();
    app.on_event(click(CLOSE_PANE_KEY), &cx);
    assert_eq!(app.pane_ids(), vec![last]);
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    assert!(!tree_has_key(&root, &last.close_key()));
    app.set_open_menu(Some("view"));
    let root = app.build(&BuildCx::new(&theme).with_viewport(1280.0, 800.0));
    assert!(!tree_has_key(&root, CLOSE_PANE_KEY));
}

#[test]
fn closing_owner_cancels_measure_and_route_previews() {
    let mut app = board();
    let cx = EventCx::new();
    app.on_event(click(&PaneId::A.split_right_key()), &cx);
    let owner = app.focused_pane.get();
    let mut measure = crate::tool::MeasureState::default();
    measure.click(Point::mm(1, 1));
    app.measure.set(measure);
    app.measure_pane.set(owner);
    assert!(app.set_route(&EntityId::new("C1"), "p1", &[], None));
    assert_eq!(app.route_pane.get(), Some(owner));

    app.on_event(click(&owner.close_key()), &cx);
    assert_eq!(app.measure.get(), crate::tool::MeasureState::default());
    assert!(app.route.borrow().is_none());
    assert_eq!(app.route_pane.get(), None);
}

#[test]
fn nested_divider_drag_changes_only_target_split() {
    use damascene_core::layout::layout;
    use damascene_core::prelude::Rect;
    use damascene_core::state::UiState;

    let mut app = EutecticApp::new(schematic_domain());
    app.split_pane(PaneId::B, SplitAxis::Vertical).unwrap();
    let theme = app.theme();
    let build = BuildCx::new(&theme).with_viewport(1280.0, 800.0);
    let mut root = app.build(&build);
    let mut ui = UiState::new();
    layout(&mut root, &mut ui, Rect::new(0.0, 0.0, 1280.0, 800.0));
    let ids = app.pane_tree.borrow().split_ids();
    let root_id = ids[0];
    let nested_id = ids[1];
    let rect = ui.rect_of_key(nested_id.container_key()).unwrap();
    let root_before = split_weights(&app, root_id);
    let nested_before = split_weights(&app, nested_id);
    let event_cx = EventCx::new().with_ui_state(&ui);
    let mut down = click(nested_id.handle_key());
    down.kind = UiEventKind::PointerDown;
    down.pointer = Some((rect.x + rect.w * 0.5, rect.y + rect.h * 0.5));
    app.on_event(down, &event_cx);
    let mut drag = click(nested_id.handle_key());
    drag.kind = UiEventKind::Drag;
    drag.pointer = Some((rect.x + rect.w * 0.5, rect.y + rect.h * 0.65));
    app.on_event(drag, &event_cx);
    assert_eq!(split_weights(&app, root_id), root_before);
    assert_ne!(split_weights(&app, nested_id), nested_before);
}

/// Per-pane independence: the SAME screen pixel maps to DIFFERENT board points when the
/// two panes have different cameras — proving the pick composition uses the clicked
/// pane's own app camera, not a shared one (the m2 bug class). And the same pixel
/// with the same camera but different pane RECTS also maps differently — proving the
/// rect is per-pane too.
#[test]
fn per_pane_composition_uses_the_clicked_panes_camera_and_rect() {
    use crate::app::canvas_pane::pane_unproject;
    use crate::render::Camera;

    let rect = (0.0f32, 0.0f32, 400.0f32, 300.0f32);
    let px = (100.0f32, 80.0f32);

    // Two different cameras (pane A vs pane B), same rect + pixel.
    let cam_a = Camera::new((5e6, 5e6), 1e-6);
    let cam_b = Camera::new((9e6, 2e6), 2e-6);
    let pa = pane_unproject(&cam_a, rect, px);
    let pb = pane_unproject(&cam_b, rect, px);
    assert_ne!(
        pa, pb,
        "same pixel under different pane cameras must map to different board points"
    );

    // Same camera, two different pane rects (dual split: A left, B right).
    let rect_a = (0.0f32, 0.0f32, 200.0f32, 300.0f32);
    let rect_b = (210.0f32, 0.0f32, 200.0f32, 300.0f32);
    let ra = pane_unproject(&cam_a, rect_a, px);
    let rb = pane_unproject(&cam_a, rect_b, px);
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
    // The schematic projection has a renderer scene + pick candidates
    // (built once per load — WP3 owned canvas).
    assert!(app.derived.borrow().schematic_scene.is_some());
    assert!(!app.derived.borrow().schematic_picks.is_empty());
    let _ = MM; // (kept for symmetry with other tests' unit imports)
}
