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
fn jump_to_net_selects_and_center_glides_the_focused_view() {
    let mut app = EutecticApp::new(schematic_domain());
    let _ = settle(&mut app);
    app.set_palette_open(true);
    app.palette_ui.borrow_mut().query = "net VDD".to_string();
    let key = result_key(&build_tree(&app), "net VDD").expect("VDD result");
    let zoom = app.pane_camera_target(PaneId::A).zoom;

    app.on_event(click(&key), &EventCx::new());

    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(NetId::new("VDD")))
    );
    let target = app.pane_camera_target(PaneId::A);
    assert_eq!(target.zoom, zoom, "jump keeps the user's zoom");
    assert!(
        target.center.0.is_finite() && target.center.1.is_finite(),
        "jump queues a finite semantic center"
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

    assert_eq!(app.pane_cams.borrow()[0].request, Some(CamRequest::Fit));
    assert_eq!(app.pane_cams.borrow()[1].request, Some(CamRequest::Fit));
    assert!(!app.palette_open.get());
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
fn wheel_and_free_hover_are_gated_while_palette_is_open() {
    let mut app = edit_app();
    let native = Native::settled(&mut app);
    let rect = native.rect_a();
    let pos = (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5);
    let cx = EventCx::new()
        .with_ui_state(&native.rt.ui_state)
        .with_viewport(native.vp.w, native.vp.h);
    let cam = app.pane_camera(PaneId::A);
    let mut wheel = pointer(UiEventKind::PointerWheel, pos);
    wheel.wheel_delta = Some((0.0, -50.0));

    app.set_palette_open(true);
    assert!(!app.on_wheel_event(wheel, &cx));
    app.raw_cursor_moved(pos);
    assert_eq!(app.cursor_px.get(), [None, None]);
    assert!(app.domain.selection.borrow().hovered().next().is_none());
    assert_eq!(app.pane_camera(PaneId::A), cam);
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
