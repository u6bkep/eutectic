//! Direct-manipulation editing tests: delete/rotate doors and editable
//! Properties fields. All drive the CPU harness / event routes headlessly.

use super::*;
use crate::chrome::menubar::{DELETE_KEY, ROTATE_KEY};
use crate::panels::properties::{
    POSITION_X_KEY, POSITION_Y_KEY, ROTATION_KEY, TRACE_LAYER_KEY, TRACE_WIDTH_KEY,
};
use crate::pick::SemanticId;
use damascene_core::runtime::RunnerCore;
use eutectic_core::doc::Provenance;
use eutectic_core::id::{NetId, TraceId, ViaId};
use eutectic_core::route::{Trace, Via};

const ROUTED_SOURCE: &str = "\
inst C1 Cap
inst C2 Cap
net SIG C1.p2 C2.p1
place C1 (5mm, 5mm)
place C2 (15mm, 5mm)
board (0mm, 0mm) (20mm, 0mm) (20mm, 10mm) (0mm, 10mm)
schematic {
  row {
    sym C1
    sym C2
  }
}
";

/// Two trace runs joined by one through via. Removing either a run or the via
/// restores the engine's existing ratsnest finding for SIG.
fn routed_app() -> EutecticApp {
    let domain = DomainState::from_source_with(
        ROUTED_SOURCE.to_string(),
        Some("routed.eut".to_string()),
        eutectic_core::part::part_library(),
        |_| {
            vec![
                Command::AddTrace(
                    TraceId(1),
                    Trace {
                        net: NetId::new("SIG"),
                        layer: "F.Cu".to_string(),
                        path: vec![Point::mm(6, 5), Point::mm(10, 5)],
                        width: 250_000,
                        prov: Provenance::Pinned,
                    },
                ),
                Command::AddTrace(
                    TraceId(2),
                    Trace {
                        net: NetId::new("SIG"),
                        layer: "B.Cu".to_string(),
                        path: vec![Point::mm(10, 5), Point::mm(14, 5)],
                        width: 250_000,
                        prov: Provenance::Pinned,
                    },
                ),
                Command::AddVia(
                    ViaId(1),
                    Via {
                        net: NetId::new("SIG"),
                        at: Point::mm(10, 5),
                        span: None,
                        drill: 300_000,
                        pad: 600_000,
                        prov: Provenance::Pinned,
                    },
                ),
            ]
        },
    );
    EutecticApp::new(domain)
}

fn has_finding(app: &EutecticApp, code: &str) -> bool {
    app.derived
        .borrow()
        .findings
        .items
        .iter()
        .any(|finding| finding.code == code)
}

fn activate(key: &str) -> UiEvent {
    let mut event = click(key);
    event.kind = UiEventKind::Activate;
    event
}

fn window_key(logical: LogicalKey, physical: PhysicalKey) -> UiEvent {
    let mut event = UiEvent::synthetic_click("");
    event.key = None;
    event.kind = UiEventKind::KeyDown;
    event.key_press = Some(KeyPress::new(
        logical,
        physical,
        KeyModifiers::default(),
        false,
    ));
    event
}

fn routed_key(key: &str, logical: LogicalKey, physical: PhysicalKey) -> UiEvent {
    let mut event = UiEvent::synthetic_click(key);
    event.kind = UiEventKind::KeyDown;
    event.key_press = Some(KeyPress::new(
        logical,
        physical,
        KeyModifiers::default(),
        false,
    ));
    event
}

fn runtime_for(app: &EutecticApp, rendered: crate::harness::Rendered) -> RunnerCore {
    let mut runtime = RunnerCore::new();
    runtime.ui_state = rendered.ui;
    runtime.ui_state.sync_focus_order(&rendered.tree);
    runtime.last_tree = Some(rendered.tree);
    runtime.set_hotkeys(app.hotkeys());
    runtime
}

fn dispatch(app: &mut EutecticApp, events: Vec<UiEvent>) {
    for event in events {
        app.on_event(event, &EventCx::new());
    }
}

fn tree_has_text(el: &El, value: &str) -> bool {
    el.text.as_deref() == Some(value) || el.children.iter().any(|child| tree_has_text(child, value))
}

/// Del removes the selected trace, the ratsnest reappears, and undo restores
/// both the stable trace id and routed connectivity.
#[test]
fn delete_key_removes_trace_and_undo_restores_connectivity() {
    let mut app = routed_app();
    assert!(!has_finding(&app, "E_DRC_UNROUTED"));
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Trace(TraceId(1)));

    app.on_event(
        window_key(LogicalKey::Named(NamedKey::Delete), PhysicalKey::Delete),
        &EventCx::new(),
    );
    assert!(
        !app.domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .contains_key(&TraceId(1))
    );
    assert!(has_finding(&app, "E_DRC_UNROUTED"));
    assert_eq!(app.undo_depths(), (1, 0));

    app.undo();
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .contains_key(&TraceId(1))
    );
    assert!(!has_finding(&app, "E_DRC_UNROUTED"));
}

/// Edit ▸ Delete uses the same path for a selected via; deleting the via breaks
/// the cross-layer connection, and undo restores it.
#[test]
fn delete_menu_row_removes_via_and_undo_restores_it() {
    let mut app = routed_app();
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Via(ViaId(1)));
    app.set_open_menu(Some("edit"));

    app.on_event(click(DELETE_KEY), &EventCx::new());
    assert!(
        !app.domain
            .doc
            .as_ref()
            .unwrap()
            .vias
            .contains_key(&ViaId(1))
    );
    assert!(has_finding(&app, "E_DRC_UNROUTED"));
    assert!(app.open_menu.borrow().is_none());

    app.undo();
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .vias
            .contains_key(&ViaId(1))
    );
    assert!(!has_finding(&app, "E_DRC_UNROUTED"));
}

/// Delete tool picking uses the Select pick kernel/tolerance. Clicking a pad
/// deletes its owning plain-authored part immediately; undo restores it.
#[test]
fn delete_tool_clicks_a_part_through_its_pad_and_undo_restores_it() {
    let mut app = routed_app();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let c1 = EntityId::new("C1");
    let candidate = app
        .derived
        .borrow()
        .board
        .as_ref()
        .unwrap()
        .candidates
        .iter()
        .find(|candidate| matches!(&candidate.id, SemanticId::Pin { comp, .. } if comp == &c1))
        .unwrap()
        .aabb;
    let point = Point {
        x: (candidate.0.x + candidate.1.x) / 2,
        y: (candidate.0.y + candidate.1.y) / 2,
    };

    app.on_event(strip_click(Tool::Delete), &cx);
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, point)),
        &cx,
    );
    assert!(
        !app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1)
    );
    // Routes are net-owned materialized facts, not component-owned. Existing
    // engine semantics retain the copper; with only one pin left on SIG the net
    // is trivially connected and therefore has no ratsnest finding.
    assert_eq!(app.domain.doc.as_ref().unwrap().traces.len(), 2);
    assert_eq!(app.domain.doc.as_ref().unwrap().vias.len(), 1);
    assert!(!has_finding(&app, "E_DRC_UNROUTED"));
    assert!(!app.domain.source.contains("sym C1"));
    assert!(app.domain.source.contains("sym C2"));

    app.undo();
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1)
    );
}

/// Net and pour selections have no independently deletable source identity, so
/// bare Delete is a pinned no-op rather than falling into another edit path.
#[test]
fn delete_key_noops_for_net_and_pour_selections() {
    for selection in [
        SemanticId::Net(NetId::new("SIG")),
        SemanticId::Pour {
            net: NetId::new("SIG"),
            layer: "F.Cu".to_string(),
        },
    ] {
        let mut app = routed_app();
        let before = app.domain.source.clone();
        app.domain.selection.borrow_mut().select_only(selection);
        app.on_event(
            window_key(LogicalKey::Named(NamedKey::Delete), PhysicalKey::Delete),
            &EventCx::new(),
        );
        assert_eq!(app.domain.source, before);
        assert_eq!(app.undo_depths(), (0, 0));
        assert!(app.domain.edit.error.is_none());
    }
}

/// Delete tool on empty board space consumes the pick action without creating a
/// command or disturbing the existing document.
#[test]
fn delete_tool_empty_space_is_a_noop() {
    let mut app = routed_app();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let before = app.domain.source.clone();
    app.on_event(strip_click(Tool::Delete), &cx);
    app.on_event(
        pointer(
            UiEventKind::Click,
            px_of_board(&app, &rendered, Point::mm(1, 1)),
        ),
        &cx,
    );
    assert_eq!(app.domain.source, before);
    assert_eq!(app.undo_depths(), (0, 0));
    assert!(app.domain.edit.error.is_none());
}

/// Removing the final member of a named net keeps the empty net and its
/// net-owned routes, so route validation and later reconnects remain valid.
#[test]
fn deleting_last_net_member_keeps_empty_named_net_and_routes() {
    let mut app = routed_app();
    app.delete_id(SemanticId::Part(EntityId::new("C1")));
    app.delete_id(SemanticId::Part(EntityId::new("C2")));

    let doc = app.domain.doc.as_ref().unwrap();
    let sig = &doc.nets[&NetId::new("SIG")];
    assert!(sig.members.is_empty());
    assert_eq!(doc.traces.len(), 2);
    assert_eq!(doc.vias.len(), 1);
    assert!(app.domain.edit.error.is_none());
}

const RANGE_GENERATED_SOURCE: &str = "\
inst R[0..2] Cap
board (0mm, 0mm) (20mm, 0mm) (20mm, 10mm) (0mm, 10mm)
";

const DEF_GENERATED_SOURCE: &str = "\
def Cell {
  inst C Cap
}
inst X Cell
board (0mm, 0mm) (20mm, 0mm) (20mm, 10mm) (0mm, 10mm)
";

/// A generated leaf cannot be independently removed without rewriting its
/// range or def authoring construct, so deletion surfaces an explicit edit
/// error and leaves the elaborated component intact.
#[test]
fn delete_generated_range_and_def_parts_surfaces_edit_error() {
    for (source, id) in [
        (RANGE_GENERATED_SOURCE, EntityId::new("R[0]")),
        (DEF_GENERATED_SOURCE, EntityId::new("X.C")),
    ] {
        let mut app = EutecticApp::new(DomainState::from_source(source.to_string(), None));
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Part(id.clone()));
        app.on_event(
            window_key(LogicalKey::Named(NamedKey::Delete), PhysicalKey::Delete),
            &EventCx::new(),
        );

        assert!(
            app.domain
                .doc
                .as_ref()
                .unwrap()
                .components
                .contains_key(&id)
        );
        let error = app.domain.edit.error.as_deref().expect("edit error");
        assert!(error.contains("generated by a range/def"), "{error}");
        assert_eq!(app.undo_depths(), (0, 0));
    }
}

/// Stable-ID pin and rotate directives genuinely edit range- and def-expanded
/// components: both values survive canonical serialization and re-elaboration.
#[test]
fn generated_part_position_and_rotation_roundtrip() {
    for (source, id) in [
        (RANGE_GENERATED_SOURCE, EntityId::new("R[0]")),
        (DEF_GENERATED_SOURCE, EntityId::new("X.C")),
    ] {
        let mut app = EutecticApp::new(DomainState::from_source(source.to_string(), None));
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Part(id.clone()));
        app.set_inspector_raw(POSITION_X_KEY, "7.5");
        app.on_event(activate(POSITION_X_KEY), &EventCx::new());
        app.on_event(
            window_key(LogicalKey::Character("r".to_string()), PhysicalKey::KeyR),
            &EventCx::new(),
        );
        assert_eq!(
            app.domain.doc.as_ref().unwrap().components[&id].pos.value.x,
            7_500_000
        );
        assert_eq!(
            app.domain.doc.as_ref().unwrap().components[&id]
                .orient
                .to_deg(),
            90
        );

        let canonical = app.domain.source.clone();
        app.apply_reload(canonical);
        let component = &app.domain.doc.as_ref().unwrap().components[&id];
        assert_eq!(component.pos.value.x, 7_500_000);
        assert_eq!(component.orient.to_deg(), 90);
        assert!(app.domain.edit.error.is_none());
    }
}

/// R and Edit ▸ Rotate both apply +90° CCW through source, bump geom_rev so
/// derived geometry rebuilds (issue 0013), serialize the orientation, and undo.
#[test]
fn rotate_key_and_menu_rebuild_geometry_and_undo() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    let before = app.domain.doc.as_ref().unwrap().clone();
    let before_source = app.domain.source.clone();

    app.on_event(
        window_key(LogicalKey::Character("r".to_string()), PhysicalKey::KeyR),
        &EventCx::new(),
    );
    let rotated = app.domain.doc.as_ref().unwrap();
    assert_eq!(rotated.components[&c1].orient.to_deg(), 90);
    assert!(rotated.geom_rev > before.geom_rev, "issue 0013 regression");
    assert!(app.domain.source.contains("rotate C1 90"));
    let _rendered = settle(&mut app);

    app.undo();
    assert_eq!(app.domain.source, before_source);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        0
    );

    app.set_open_menu(Some("edit"));
    app.on_event(click(ROTATE_KEY), &EventCx::new());
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        90
    );
    app.undo();
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        0
    );
}

/// Real runtime matching leaves bare Delete/R as raw input events. With an
/// inspector field focused they edit its text and never mutate the selected
/// component; without a focused input they remain board editor commands.
#[test]
fn runtime_delete_and_rotate_keys_respect_inspector_focus() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    let rendered = settle(&mut app);
    let field = rendered
        .ui
        .rect_of_key(POSITION_X_KEY)
        .expect("position field");
    let pointer = Pointer::mouse(
        field.x + 2.0,
        field.y + field.h / 2.0,
        PointerButton::Primary,
    );
    let mut runtime = runtime_for(&app, rendered);
    dispatch(&mut app, runtime.pointer_down(pointer));
    dispatch(&mut app, runtime.pointer_up(pointer));
    assert!(runtime.focused_captures_keys());

    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Named(NamedKey::Home),
            PhysicalKey::Home,
            KeyModifiers::default(),
            false,
        ),
    );
    let before = app.inspector_ui.borrow().raw[POSITION_X_KEY].clone();
    let delete_events = runtime.key_down(
        LogicalKey::Named(NamedKey::Delete),
        PhysicalKey::Delete,
        KeyModifiers::default(),
        false,
    );
    assert_eq!(delete_events[0].kind, UiEventKind::KeyDown);
    assert_eq!(delete_events[0].target_key(), Some(POSITION_X_KEY));
    dispatch(&mut app, delete_events);
    let after = app.inspector_ui.borrow().raw[POSITION_X_KEY].clone();
    assert_eq!(after.len() + 1, before.len(), "forward-delete edited text");
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1)
    );
    assert_eq!(
        app.undo_depths(),
        (0, 0),
        "Delete did not edit the document"
    );

    let rotate_events = runtime.key_down(
        LogicalKey::Character("r".to_string()),
        PhysicalKey::KeyR,
        KeyModifiers::default(),
        false,
    );
    assert_eq!(rotate_events[0].kind, UiEventKind::KeyDown);
    dispatch(&mut app, rotate_events);
    let text = runtime.text_input("r".to_string()).expect("focused input");
    app.on_event(text, &EventCx::new());
    assert!(app.inspector_ui.borrow().raw[POSITION_X_KEY].contains('r'));
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        0,
        "typing r did not rotate"
    );

    let mut board_app = edit_app();
    board_app
        .domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    let mut board_runtime = RunnerCore::new();
    board_runtime.set_hotkeys(board_app.hotkeys());
    let events = board_runtime.key_down(
        LogicalKey::Named(NamedKey::Delete),
        PhysicalKey::Delete,
        KeyModifiers::default(),
        false,
    );
    assert_eq!(events[0].kind, UiEventKind::KeyDown);
    assert!(events[0].target_key().is_none());
    dispatch(&mut board_app, events);
    assert!(
        !board_app
            .domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1),
        "window-level Delete removes the board selection"
    );

    let mut rotate_app = edit_app();
    rotate_app
        .domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    let mut rotate_runtime = RunnerCore::new();
    rotate_runtime.set_hotkeys(rotate_app.hotkeys());
    dispatch(
        &mut rotate_app,
        rotate_runtime.key_down(
            LogicalKey::Character("r".to_string()),
            PhysicalKey::KeyR,
            KeyModifiers::default(),
            false,
        ),
    );
    assert_eq!(
        rotate_app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        90,
        "window-level r rotates the board selection"
    );
}

/// The Libraries modal's capture-key path owns Delete/R too; the component
/// behind the scrim survives and forward-delete remains available in its input.
#[test]
fn runtime_delete_and_r_edit_libraries_input_without_board_actions() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    app.set_libraries_open(true);
    app.set_library_inputs("lib", "/abc");
    let rendered = settle(&mut app);
    let field = rendered
        .ui
        .rect_of_key("libraries:input:path")
        .expect("path field");
    let pointer = Pointer::mouse(
        field.x + 2.0,
        field.y + field.h / 2.0,
        PointerButton::Primary,
    );
    let mut runtime = runtime_for(&app, rendered);
    let down = runtime.pointer_down(pointer);
    assert!(
        down.iter()
            .any(|event| event.target_key() == Some("libraries:input:path")),
        "runtime focused the path field: {down:?}"
    );
    dispatch(&mut app, down);
    dispatch(&mut app, runtime.pointer_up(pointer));
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Named(NamedKey::Home),
            PhysicalKey::Home,
            KeyModifiers::default(),
            false,
        ),
    );
    let selection = app.lib_ui.borrow().selection.clone();
    assert_eq!(
        selection
            .within("libraries:input:path")
            .expect("path owns selection")
            .head,
        0,
        "Home moved the path caret: {selection:?}"
    );
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Named(NamedKey::Delete),
            PhysicalKey::Delete,
            KeyModifiers::default(),
            false,
        ),
    );
    assert_eq!(app.lib_ui.borrow().path, "abc");
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Character("r".to_string()),
            PhysicalKey::KeyR,
            KeyModifiers::default(),
            false,
        ),
    );
    let text = runtime
        .text_input("r".to_string())
        .expect("focused path input");
    app.on_event(text, &EventCx::new());
    assert!(app.lib_ui.borrow().path.starts_with('r'));
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1)
    );
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        0
    );
}

/// An open menu is modal chrome for bare editor keys; its visible Delete/Rotate
/// rows remain usable by click, but a raw key press cannot edit behind it.
#[test]
fn runtime_delete_and_r_are_suppressed_while_menu_is_open() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    app.set_open_menu(Some("edit"));
    let mut runtime = RunnerCore::new();
    runtime.set_hotkeys(app.hotkeys());
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Named(NamedKey::Delete),
            PhysicalKey::Delete,
            KeyModifiers::default(),
            false,
        ),
    );
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Character("r".to_string()),
            PhysicalKey::KeyR,
            KeyModifiers::default(),
            false,
        ),
    );
    let component = &app.domain.doc.as_ref().unwrap().components[&c1];
    assert_eq!(component.orient.to_deg(), 0);
    assert!(app.open_menu.borrow().is_some());
    assert_eq!(app.undo_depths(), (0, 0));
}

/// Raw numeric text commits on Enter, Escape reverts without a command, and a
/// click elsewhere commits on blur. Each successful edit is ordinary undoable
/// source-first history.
#[test]
fn inspector_component_numeric_commit_escape_and_blur() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));

    app.set_inspector_raw(POSITION_X_KEY, "17.");
    let editing = settle(&mut app);
    assert!(
        tree_has_text(&editing.tree, "17."),
        "raw text survives rebuild"
    );

    app.set_inspector_raw(POSITION_X_KEY, "17.5");
    app.on_event(activate(POSITION_X_KEY), &EventCx::new());
    assert_eq!(comp_pos(&app, &c1).x, 17_500_000);
    assert_eq!(app.undo_depths(), (1, 0));

    let y = comp_pos(&app, &c1).y;
    app.set_inspector_raw(POSITION_Y_KEY, "not-a-number");
    app.on_event(
        routed_key(
            POSITION_Y_KEY,
            LogicalKey::Named(NamedKey::Escape),
            PhysicalKey::Escape,
        ),
        &EventCx::new(),
    );
    assert_eq!(comp_pos(&app, &c1).y, y);
    assert_eq!(app.undo_depths(), (1, 0), "Escape created no command");

    app.set_inspector_raw(POSITION_Y_KEY, "still-not-a-number");
    app.on_event(activate(POSITION_Y_KEY), &EventCx::new());
    assert_eq!(comp_pos(&app, &c1).y, y, "invalid Enter reverted");
    assert_eq!(
        app.undo_depths(),
        (1, 0),
        "invalid Enter created no command"
    );

    app.set_inspector_raw(POSITION_Y_KEY, "4.25");
    app.on_event(click("outside-properties"), &EventCx::new());
    assert_eq!(comp_pos(&app, &c1).y, 4_250_000, "blur committed");

    app.set_inspector_raw(ROTATION_KEY, "22.5");
    app.on_event(activate(ROTATION_KEY), &EventCx::new());
    let degrees =
        crate::inspector::rotation_degrees(app.domain.doc.as_ref().unwrap().components[&c1].orient);
    assert!((degrees - 22.5).abs() < 0.001);
    app.undo();
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        0
    );
}

/// Hover/drag never impersonate field focus, unrelated activation cannot commit
/// an armed field, and Tab/Enter only affect the field that owns the key event.
#[test]
fn inspector_focus_activation_and_tab_are_target_strict() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));

    for kind in [UiEventKind::PointerEnter, UiEventKind::Drag] {
        let mut hover = click(POSITION_X_KEY);
        hover.kind = kind;
        app.on_event(hover, &EventCx::new());
        assert_eq!(
            app.inspector_ui.borrow().active,
            None,
            "{kind:?} did not arm fieldRaw"
        );
    }
    app.on_event(escape(), &EventCx::new());
    assert!(
        app.domain.selection.borrow().is_empty(),
        "Escape after mere hover fell through to the normal cascade"
    );

    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    let original_x = comp_pos(&app, &c1).x;
    app.set_inspector_raw(POSITION_X_KEY, "12.5");
    app.on_event(activate("unrelated-button"), &EventCx::new());
    assert_eq!(comp_pos(&app, &c1).x, original_x);
    assert_eq!(app.inspector_ui.borrow().active, Some(POSITION_X_KEY));
    app.on_event(activate(POSITION_X_KEY), &EventCx::new());
    assert_eq!(comp_pos(&app, &c1).x, 12_500_000);

    app.set_inspector_raw(POSITION_Y_KEY, "6.25");
    app.on_event(
        routed_key(
            POSITION_Y_KEY,
            LogicalKey::Named(NamedKey::Tab),
            PhysicalKey::Tab,
        ),
        &EventCx::new(),
    );
    assert_eq!(comp_pos(&app, &c1).y, 6_250_000);
    assert_eq!(
        app.inspector_ui.borrow().active,
        None,
        "Tab blurred fieldRaw"
    );
}

/// Trace width and layer are editable through the same command layer. A positive
/// but rule-violating width commits and produces a finding instead of blocking.
#[test]
fn inspector_trace_width_layer_and_permissive_finding() {
    let mut app = routed_app();
    let tid = TraceId(1);
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Trace(tid));

    app.set_inspector_raw(TRACE_WIDTH_KEY, "0.05");
    app.on_event(activate(TRACE_WIDTH_KEY), &EventCx::new());
    let trace = &app.domain.doc.as_ref().unwrap().traces[&tid];
    assert_eq!(trace.width, 50_000, "violating edit stands");
    assert!(has_finding(&app, "E_DRC_MIN_WIDTH"));

    let layer = trace.layer.clone();
    app.on_event(click(TRACE_LAYER_KEY), &EventCx::new());
    assert_ne!(app.domain.doc.as_ref().unwrap().traces[&tid].layer, layer);
    assert_eq!(app.undo_depths(), (2, 0));

    app.undo();
    assert_eq!(app.domain.doc.as_ref().unwrap().traces[&tid].layer, layer);
    app.undo();
    assert_eq!(app.domain.doc.as_ref().unwrap().traces[&tid].width, 250_000);
}
