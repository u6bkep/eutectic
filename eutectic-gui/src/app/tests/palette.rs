use super::camera::Native;
use super::*;
use crate::app::canvas_pane::CamRequest;
use crate::app::pane::SidebarSection;
use crate::palette::{PALETTE_INPUT_KEY, PALETTE_TOGGLE_KEY};
use crate::panels::explorer::EXPLORER_FILTER_KEY;

fn build_tree(app: &EutecticApp) -> El {
    let theme = app.theme();
    let cx = BuildCx::new(&theme).with_viewport(1280.0, 800.0);
    app.build(&cx)
}

fn contains_text(node: &El, needle: &str) -> bool {
    node.text
        .as_deref()
        .is_some_and(|text| text.contains(needle))
        || node
            .children
            .iter()
            .any(|child| contains_text(child, needle))
}

fn result_key(root: &El, label: &str) -> Option<String> {
    if root
        .key
        .as_deref()
        .is_some_and(|key| key.starts_with("palette:result:"))
        && contains_text(root, label)
    {
        return root.key.clone();
    }
    root.children
        .iter()
        .find_map(|child| result_key(child, label))
}

fn keys(root: &El) -> Vec<String> {
    let mut out = root.key.iter().cloned().collect::<Vec<_>>();
    for child in &root.children {
        out.extend(keys(child));
    }
    out
}

fn named_key(key: NamedKey) -> UiEvent {
    let mut event = click(PALETTE_INPUT_KEY);
    event.kind = UiEventKind::KeyDown;
    event.key_press = Some(KeyPress::new(
        LogicalKey::Named(key),
        PhysicalKey::Unidentified,
        KeyModifiers::default(),
        false,
    ));
    event
}

fn text_input(text: &str, selection: Selection) -> UiEvent {
    let mut event = click("");
    event.key = None;
    event.kind = UiEventKind::TextInput;
    event.text = Some(text.to_string());
    event.selection = Some(selection);
    event
}

fn selection_changed(selection: Selection) -> UiEvent {
    let mut event = click("");
    event.key = None;
    event.kind = UiEventKind::SelectionChanged;
    event.selection = Some(selection);
    event
}

fn jump_fixture() -> EutecticApp {
    let source = "\
inst C1 Cap
inst C2 Cap
net VDD C1.p1 C2.p1
place C1 (10mm, 10mm)
place C2 (20mm, 10mm)
schematic {
  row gap=8mm align=center {
    sym C1
    sym C2
    wire C1.p1 C2.p1
  }
}
";
    EutecticApp::new(DomainState::from_source(
        source.to_string(),
        Some("palette-jump.eut".to_string()),
    ))
}

#[test]
fn explorer_filter_narrows_components_and_nets_and_rows_still_select() {
    let source = "\
inst C1 Cap p:value=100nF
inst C2 Cap p:value=1uF
net VDD C1.p1
net GND C2.p1
";
    let mut app = EutecticApp::new(DomainState::from_source(
        source.to_string(),
        Some("filter.eut".to_string()),
    ));
    app.set_section_open(SidebarSection::Explorer, true);

    *app.explorer_filter.borrow_mut() = "100NF".to_string();
    let filtered = keys(&build_tree(&app));
    assert!(filtered.iter().any(|key| key == "explorer:comp:C1"));
    assert!(!filtered.iter().any(|key| key == "explorer:comp:C2"));
    assert!(!filtered.iter().any(|key| key.starts_with("explorer:net:")));

    *app.explorer_filter.borrow_mut() = "vDd".to_string();
    let filtered = keys(&build_tree(&app));
    assert!(filtered.iter().any(|key| key == "explorer:net:VDD"));
    assert!(!filtered.iter().any(|key| key == "explorer:net:GND"));
    assert!(!filtered.iter().any(|key| key.starts_with("explorer:comp:")));

    app.on_event(click("explorer:net:VDD"), &EventCx::new());
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(NetId::new("VDD"))),
        "a visible filtered row keeps the normal click-to-select route"
    );
}

#[test]
fn explorer_component_row_renders_refdes_and_effective_value() {
    let source = "inst C1 Cap p:value=100nF\n";
    let app = EutecticApp::new(DomainState::from_source(
        source.to_string(),
        Some("component-value.eut".to_string()),
    ));
    app.set_section_open(SidebarSection::Explorer, true);

    assert!(
        contains_text(&build_tree(&app), "Cap1  (100nF)  [2]"),
        "the oracle anatomy is refdes + effective value + pin count"
    );
}

#[test]
fn explorer_filter_text_input_updates_live() {
    let mut app = EutecticApp::new(schematic_domain());
    *app.explorer_filter_selection.borrow_mut() = Selection::caret(EXPLORER_FILTER_KEY, 0);
    let mut input = click(EXPLORER_FILTER_KEY);
    input.kind = UiEventKind::TextInput;
    input.text = Some("vdd".to_string());
    app.on_event(input, &EventCx::new());
    assert_eq!(&*app.explorer_filter.borrow(), "vdd");
}

#[test]
fn explorer_filter_only_accepts_typing_while_its_selection_is_current() {
    let mut app = EutecticApp::new(schematic_domain());
    *app.explorer_filter.borrow_mut() = "kept".to_string();

    app.on_event(
        selection_changed(Selection::caret(PALETTE_TOGGLE_KEY, 0)),
        &EventCx::new(),
    );
    app.on_event(
        text_input("x", Selection::caret(PALETTE_TOGGLE_KEY, 0)),
        &EventCx::new(),
    );
    assert_eq!(
        &*app.explorer_filter.borrow(),
        "kept",
        "typing with a button focused must not leak into the Explorer filter"
    );

    app.on_event(
        selection_changed(Selection::caret(EXPLORER_FILTER_KEY, 4)),
        &EventCx::new(),
    );
    app.on_event(
        text_input("x", Selection::caret(EXPLORER_FILTER_KEY, 4)),
        &EventCx::new(),
    );
    assert_eq!(&*app.explorer_filter.borrow(), "keptx");
}

#[test]
fn explorer_filter_selection_releases_on_route_less_focus_change() {
    let mut app = EutecticApp::new(schematic_domain());
    app.on_event(click(EXPLORER_FILTER_KEY), &EventCx::new());
    app.on_event(
        selection_changed(Selection::caret(EXPLORER_FILTER_KEY, 0)),
        &EventCx::new(),
    );
    assert!(app.selection().is_within(EXPLORER_FILTER_KEY));

    app.on_event(click(PaneId::A.canvas_key()), &EventCx::new());
    app.on_event(selection_changed(Selection::default()), &EventCx::new());
    assert!(
        !app.selection().is_within(EXPLORER_FILTER_KEY),
        "the route-less runtime selection update releases the filter range"
    );
}

#[test]
fn ctrl_k_opens_autofocuses_and_escape_closes_before_selection_clear() {
    let mut app = EutecticApp::new(schematic_domain());
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("VDD")));

    app.on_event(hotkey(PALETTE_TOGGLE_KEY), &EventCx::new());
    assert!(app.palette_open.get());
    assert_eq!(
        app.drain_focus_requests(),
        vec![PALETTE_INPUT_KEY.to_string()],
        "opening requests focus for the query field"
    );
    app.on_event(escape(), &EventCx::new());
    assert!(!app.palette_open.get());
    assert!(
        !app.domain.selection.borrow().is_empty(),
        "palette Escape is consumed before the existing selection-clear tail"
    );
}

#[test]
fn ctrl_k_is_inert_while_modal_chrome_owns_the_keyboard() {
    let mut app = EutecticApp::new(schematic_domain());
    app.set_libraries_open(true);
    app.on_event(hotkey(PALETTE_TOGGLE_KEY), &EventCx::new());
    assert!(!app.palette_open.get());
    assert!(app.libraries_open.get(), "Libraries remains the owner");

    app.set_libraries_open(false);
    app.set_open_menu(Some("file"));
    app.on_event(hotkey(PALETTE_TOGGLE_KEY), &EventCx::new());
    assert!(!app.palette_open.get());
    assert_eq!(app.open_menu.borrow().as_deref(), Some("file"));
}

#[test]
fn jump_to_net_targets_the_known_board_feature_center() {
    let mut app = jump_fixture();
    let _ = settle(&mut app);
    app.pane_center_on(PaneId::A, (0.0, 0.0));
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "net VDD".to_string();
    let key = result_key(&build_tree(&app), "net VDD").expect("VDD result");
    let before = app.pane_camera_target(PaneId::A);

    app.on_event(click(&key), &EventCx::new());

    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(NetId::new("VDD")))
    );
    let target = app.pane_camera_target(PaneId::A);
    assert_eq!(target.zoom, before.zoom, "jump keeps the user's zoom");
    assert_ne!(
        target.center, before.center,
        "jump must retarget the camera"
    );
    let expected = (14.0 * NM_PER_MM as f64, 10.0 * NM_PER_MM as f64);
    assert!(
        (target.center.0 - expected.0).abs() < 1.0 && (target.center.1 - expected.1).abs() < 1.0,
        "two identical 0.8 mm p1 pads at 9/19 mm center at {expected:?}, got {:?}",
        target.center
    );
    assert!(!app.palette_open.get());
}

#[test]
fn jump_to_part_targets_visible_schematic_when_focused_pane_is_hidden() {
    let mut app = jump_fixture();
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_maximized(Some(PaneId::B));
    let _ = settle(&mut app);
    let hidden_before = app.pane_camera_target(PaneId::A);
    let before = app.pane_camera_target(PaneId::B);
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "part Cap2".to_string();
    let key = result_key(&build_tree(&app), "part Cap2").expect("Cap2 result");

    app.on_event(click(&key), &EventCx::new());

    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Part(EntityId::new("C2")))
    );
    assert_eq!(
        app.pane_camera_target(PaneId::A),
        hidden_before,
        "the hidden focused pane is not targeted"
    );
    let target = app.pane_camera_target(PaneId::B);
    assert_eq!(target.zoom, before.zoom, "jump keeps the user's zoom");
    assert_ne!(
        target.center, before.center,
        "jump must retarget the camera"
    );
    let expected = (23.67 * NM_PER_MM as f64, -3.81 * NM_PER_MM as f64);
    assert!(
        (target.center.0 - expected.0).abs() < 1.0 && (target.center.1 - expected.1).abs() < 1.0,
        "Cap2's known row-reflow center is {expected:?}, got {:?}",
        target.center
    );
    assert!(!app.palette_open.get());
}

#[test]
fn fit_view_command_executes() {
    let mut app = EutecticApp::new(schematic_domain());
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "fv".to_string();
    let key = result_key(&build_tree(&app), "Fit view").expect("fuzzy Fit result");

    app.on_event(click(&key), &EventCx::new());

    assert_eq!(
        app.pane_cams.borrow()[0].as_ref().unwrap().request,
        Some(CamRequest::Fit)
    );
    assert_eq!(
        app.pane_cams.borrow()[1].as_ref().unwrap().request,
        Some(CamRequest::Fit)
    );
    assert!(!app.palette_open.get());
}

#[test]
fn fit_view_has_no_unbound_shortcut_hint() {
    let app = EutecticApp::new(schematic_domain());
    app.set_palette_open(true);
    let tree = build_tree(&app);
    assert!(result_key(&tree, "Fit view").is_some());
    assert!(!contains_text(&tree, "Fit view    F"));
}

#[test]
fn palette_arrow_keys_move_highlight_and_enter_executes() {
    let mut app = EutecticApp::new(schematic_domain());
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "net".to_string();
    assert_eq!(app.palette_ui.borrow().highlighted, 0);

    app.on_event(named_key(NamedKey::ArrowDown), &EventCx::new());
    assert_eq!(app.palette_ui.borrow().highlighted, 1);
    app.on_event(named_key(NamedKey::Enter), &EventCx::new());

    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(NetId::new("VDD"))),
        "the second stable net row executes on Enter"
    );
    assert!(!app.palette_open.get());
}

#[test]
fn palette_renders_no_matches_empty_state() {
    let app = EutecticApp::new(schematic_domain());
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "zzzzzz".to_string();
    assert!(contains_text(&build_tree(&app), "No matches"));
}

#[test]
fn palette_menu_token_gates_canvas_input_until_close() {
    let mut app = edit_app();
    let native = Native::settled(&mut app);
    let rect = native.rect_a();
    let pad = pad_center_of(&app, &EntityId::new("C1"));
    let pos = crate::app::canvas_pane::pane_project(
        &app.pane_camera(PaneId::A),
        (rect.x, rect.y, rect.w, rect.h),
        pad,
    );
    let cx = EventCx::new()
        .with_ui_state(&native.rt.ui_state)
        .with_viewport(native.vp.w, native.vp.h);
    let cam = app.pane_camera(PaneId::A);
    let wheel = || {
        let mut event = pointer(UiEventKind::PointerWheel, pos);
        event.wheel_delta = Some((0.0, -50.0));
        event
    };

    app.raw_cursor_moved(pos);
    assert!(app.cursor_px.borrow()[0].is_some());
    assert!(app.domain.selection.borrow().hovered().next().is_some());

    app.set_palette_open(true);
    assert_eq!(
        app.open_menu.borrow().as_deref(),
        Some("__palette_modal_gate")
    );
    assert!(!app.on_wheel_event(wheel(), &cx));
    app.raw_cursor_moved(pos);
    assert_eq!(*app.cursor_px.borrow(), vec![None, None]);
    assert!(app.domain.selection.borrow().hovered().next().is_none());
    assert!(!app.raw_middle(true), "middle-drag cannot arm");
    assert_eq!(app.pane_camera(PaneId::A), cam);

    app.set_palette_open(false);
    assert!(app.open_menu.borrow().is_none(), "the gate token clears");
    assert!(app.on_wheel_event(wheel(), &cx), "wheel gate lifts");
    app.raw_cursor_moved(pos);
    assert!(app.cursor_px.borrow()[0].is_some(), "crosshair gate lifts");
    assert!(
        app.domain.selection.borrow().hovered().next().is_some(),
        "free-hover gate lifts"
    );
    assert!(app.raw_middle(true), "middle-drag gate lifts");
    assert!(app.raw_middle(false));
}

#[test]
fn toolbar_palette_button_is_enabled_and_opens_the_modal() {
    let mut app = EutecticApp::new(schematic_domain());
    let tree = build_tree(&app);
    assert!(
        keys(&tree).iter().any(|key| key == PALETTE_TOGGLE_KEY),
        "toolbar exposes a routed palette icon"
    );

    app.on_event(click(PALETTE_TOGGLE_KEY), &EventCx::new());
    assert!(app.palette_open.get());
}
