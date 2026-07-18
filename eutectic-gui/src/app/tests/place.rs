use super::*;
use crate::panels::library_browser::{LIBRARY_FILTER_KEY, library_part_key};

fn text_input(text: &str, selection: Selection) -> UiEvent {
    let mut event = click("");
    event.key = None;
    event.kind = UiEventKind::TextInput;
    event.text = Some(text.to_string());
    event.selection = Some(selection);
    event
}

#[test]
fn place_strip_opens_palette_and_filter_captures_typing() {
    let mut app = edit_app();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);

    app.on_event(click(&PaneId::A.strip_key(Tool::Place)), &cx);
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Place);
    assert!(app.library_browser_open.get());
    assert_eq!(
        app.drain_focus_requests(),
        vec![LIBRARY_FILTER_KEY.to_string()]
    );

    app.on_event(
        text_input("-", Selection::caret(LIBRARY_FILTER_KEY, 0)),
        &EventCx::new(),
    );
    assert_eq!(app.library_browser_ui.borrow().query, "-");
    assert!(
        app.domain.selection.borrow().is_empty(),
        "typing in the palette never leaks to board editing"
    );
}

#[test]
fn choosing_a_row_arms_place_and_keeps_the_oracle_flyout_open() {
    let mut app = edit_app();
    app.open_library_browser();
    let cap = app
        .domain
        .library_parts
        .iter()
        .position(|row| row.part == "Cap")
        .expect("builtin Cap row");

    app.on_event(click(&library_part_key(cap)), &EventCx::new());

    assert_eq!(app.armed_part_name().as_deref(), Some("Cap"));
    assert!(app.library_browser_open.get(), "the dock stays visible");
    assert!(!app.place_shapes.borrow().is_empty());
}

#[test]
fn resolved_index_retains_package_ownership_and_builtin_last() {
    let mut registry = crate::registry::Registry::new();
    let poc = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
    registry.set("poc", &poc).unwrap();
    let app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 Cap\n".to_string(),
        Some("index.eut".to_string()),
        registry,
        None,
    ));
    let rows = &app.domain.library_parts;
    assert_eq!(
        rows.iter()
            .find(|row| row.part == "RP2350A")
            .map(|row| row.library.as_str()),
        Some("poc")
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.part == "Cap")
            .map(|row| row.library.as_str()),
        Some("builtin")
    );
    let last_real = rows.iter().rposition(|row| row.library == "poc").unwrap();
    let first_builtin = rows
        .iter()
        .position(|row| row.library == "builtin")
        .unwrap();
    assert!(last_real < first_builtin, "builtin is the final group");
}

#[test]
fn placing_from_an_unused_registered_package_authors_its_use_declaration() {
    let mut registry = crate::registry::Registry::new();
    let poc = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
    registry.set("poc", &poc).unwrap();
    let mut app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 Cap\n".to_string(),
        Some("catalog.eut".to_string()),
        registry,
        None,
    ));
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.library == "poc" && row.part == "RP2350A")
        .cloned()
        .expect("unused registered package is catalogued");
    app.arm_library_part(&row);
    app.commit_armed_part(Point {
        x: 4 * MM,
        y: 5 * MM,
    });

    let canonical = eutectic_core::text::serialize(app.domain.doc.as_ref().unwrap());
    assert!(canonical.starts_with("use poc\n"), "{canonical}");
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .values()
            .any(|component| component.part == "RP2350A")
    );
    assert_eq!(app.undo_depths(), (1, 0));
}

#[test]
fn isolated_preview_uses_board_scene_and_world_features() {
    let lib = eutectic_core::part::part_library();
    let (scene, shapes) =
        crate::app::place::isolated_part_preview("Cap", &lib).expect("Cap previews");
    assert!(scene.prim_count() > 0);
    assert!(!shapes.is_empty());
    assert!(
        scene.planes.iter().any(|plane| !plane.prims.is_empty()),
        "the owned-renderer scene carries the footprint"
    );
}

#[test]
fn canonical_placement_allocates_around_generated_refdes() {
    let source = "\
def bank {
  inst K Cap
}
inst bank1 bank
inst generated[0..2] Cap
place generated[0] (1mm, 1mm)
place generated[1] (2mm, 1mm)
";
    let app = EutecticApp::new(DomainState::from_source(
        source.to_string(),
        Some("generated.eut".to_string()),
    ));

    let text = app
        .placement_text_for_test(
            "builtin",
            "Cap",
            Point {
                x: 4 * MM,
                y: 5 * MM,
            },
        )
        .expect("placement stages");
    assert!(text.contains("inst Cap4 Cap\n"), "{text}");
    assert!(text.contains("pin Cap4 (4mm, 5mm)\n"), "{text}");
    assert!(text.contains("refdes Cap4 Cap4\n"), "{text}");

    let parsed = eutectic_core::text::parse(&text).expect("canonical payload parses");
    let staged = eutectic_core::doc::Doc {
        source: parsed.source,
        overrides: parsed.overrides,
        refdes_pins: parsed.refdes_pins,
        ..eutectic_core::doc::Doc::default()
    };
    assert_eq!(
        eutectic_core::text::serialize(&staged),
        text,
        "the LoadText payload is already canonical"
    );
}

#[test]
fn repeated_place_commits_one_undo_unit_each_and_stays_armed() {
    let mut app = edit_app();
    app.set_tool(ViewKind::Board, Tool::Place);
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .expect("Cap row");
    app.arm_library_part(&row);

    app.commit_armed_part(Point {
        x: 4 * MM,
        y: 5 * MM,
    });
    let first = app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .components
        .values()
        .find(|component| {
            component.pos.value
                == Point {
                    x: 4 * MM,
                    y: 5 * MM,
                }
        })
        .expect("first placed component");
    assert_eq!(first.pos.prov, eutectic_core::doc::Provenance::Pinned);
    assert_eq!(app.undo_depths(), (1, 0));
    assert_eq!(app.armed_part_name().as_deref(), Some("Cap"));

    app.commit_armed_part(Point {
        x: 7 * MM,
        y: 8 * MM,
    });
    assert_eq!(app.undo_depths(), (2, 0));
    assert_eq!(app.armed_part_name().as_deref(), Some("Cap"));
    assert!(app.dirty());
}

#[test]
fn board_click_commits_armed_part_and_unarmed_click_is_a_no_op() {
    let mut app = edit_app();
    app.set_tool(ViewKind::Board, Tool::Place);
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    let target = Point {
        x: 4 * MM,
        y: 5 * MM,
    };
    let px = crate::app::canvas_pane::pane_project(
        &app.pane_camera(PaneId::A),
        app.pane_px.get()[0].unwrap(),
        target,
    );
    let before = app.domain.doc.as_ref().unwrap().components.len();

    app.on_event(pointer(UiEventKind::Click, px), &cx);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components.len(),
        before,
        "Place without an armed row consumes the click without editing"
    );

    app.arm_library_part(&row);
    app.on_event(pointer(UiEventKind::Click, px), &cx);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components.len(),
        before + 1
    );
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .values()
            .any(|component| component.pos.value == target)
    );
}

#[test]
fn escape_disarms_but_keeps_place_mode_and_tool_switch_cancels_ghost() {
    let mut app = edit_app();
    app.set_tool(ViewKind::Board, Tool::Place);
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    assert!(app.hover_place_part(PaneId::A, Point { x: MM, y: MM }));
    assert!(!app.place_ghost_shapes(PaneId::A).is_empty());

    app.on_event(escape(), &EventCx::new());
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Place);
    assert!(app.armed_part_name().is_none());
    assert!(app.place_ghost_shapes(PaneId::A).is_empty());

    app.arm_library_part(&row);
    app.hover_place_part(PaneId::A, Point { x: MM, y: MM });
    app.set_tool(ViewKind::Board, Tool::Select);
    assert!(app.place_ghost_shapes(PaneId::A).is_empty());
    assert_eq!(
        app.armed_part_name().as_deref(),
        Some("Cap"),
        "tool switching cancels the ghost without rewriting the palette choice"
    );
}
