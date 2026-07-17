//! Direct-manipulation editing tests: delete/rotate doors and editable
//! Properties fields. All drive the CPU harness / event routes headlessly.

use super::*;
use crate::chrome::menubar::{DELETE_KEY, ROTATE_KEY};
use crate::panels::properties::{
    POSITION_X_KEY, POSITION_Y_KEY, ROTATION_KEY, TRACE_LAYER_KEY, TRACE_WIDTH_KEY,
};
use crate::pick::SemanticId;
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

    app.on_event(hotkey(DELETE_KEY), &EventCx::new());
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

    app.on_event(hotkey(ROTATE_KEY), &EventCx::new());
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
    app.on_event(escape(), &EventCx::new());
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
