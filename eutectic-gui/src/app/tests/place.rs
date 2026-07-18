use super::*;
use crate::panels::library_browser::{LIBRARY_FILTER_KEY, library_part_key};
use crate::pick::SemanticId;
use damascene_core::runtime::RunnerCore;

fn text_input(text: &str, selection: Selection) -> UiEvent {
    let mut event = click("");
    event.key = None;
    event.kind = UiEventKind::TextInput;
    event.text = Some(text.to_string());
    event.selection = Some(selection);
    event
}

#[test]
fn place_strip_opens_palette_and_filter_accepts_text_input() {
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

#[test]
fn runtime_delete_and_r_are_inert_with_library_filter_focused_then_work_on_board() {
    let mut app = edit_app();
    let c1 = EntityId::new("C1");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    app.set_tool(ViewKind::Board, Tool::Place);
    app.open_library_browser();
    let rendered = settle(&mut app);
    let field = rendered
        .ui
        .rect_of_key(LIBRARY_FILTER_KEY)
        .expect("library filter");
    let mut runtime = runtime_for(&app, rendered);
    let filter_pointer = Pointer::mouse(
        field.x + 4.0,
        field.y + field.h / 2.0,
        PointerButton::Primary,
    );
    dispatch(&mut app, runtime.pointer_down(filter_pointer));
    dispatch(&mut app, runtime.pointer_up(filter_pointer));
    assert!(runtime.focused_captures_keys());

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
    assert_eq!(app.undo_depths(), (0, 0));

    let canvas = runtime
        .ui_state
        .rect_of_key(PaneId::A.canvas_key())
        .expect("board pane");
    let board_pointer = Pointer::mouse(
        canvas.x + canvas.w * 0.75,
        canvas.y + canvas.h * 0.75,
        PointerButton::Primary,
    );
    dispatch(&mut app, runtime.pointer_down(board_pointer));
    dispatch(&mut app, runtime.pointer_up(board_pointer));
    assert!(!runtime.focused_captures_keys());
    app.set_tool(ViewKind::Board, Tool::Select);
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));

    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Character("r".to_string()),
            PhysicalKey::KeyR,
            KeyModifiers::default(),
            false,
        ),
    );
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&c1]
            .orient
            .to_deg(),
        90
    );
    app.undo();
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(c1.clone()));
    dispatch(
        &mut app,
        runtime.key_down(
            LogicalKey::Named(NamedKey::Delete),
            PhysicalKey::Delete,
            KeyModifiers::default(),
            false,
        ),
    );
    assert!(
        !app.domain
            .doc
            .as_ref()
            .unwrap()
            .components
            .contains_key(&c1)
    );
}

fn write_package(root: &std::path::Path, part: &str, footprint: &str) {
    std::fs::create_dir_all(root).unwrap();
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../poc/parts")
        .join(footprint);
    std::fs::copy(&source, root.join(footprint)).unwrap();
    std::fs::write(
        root.join(eutectic_core::library::MANIFEST_NAME),
        format!("part {part} footprint={footprint}\n"),
    )
    .unwrap();
}

#[test]
fn colliding_doc_resolution_wins_catalog_preview_and_placement_without_rebinding() {
    let scratch = Scratch::new("place-collision");
    let alpha = scratch.0.join("alpha");
    let beta = scratch.0.join("beta");
    write_package(&alpha, "C_0402", "Inductor_2020.kicad_mod");
    write_package(&beta, "C_0402", "C_0402.kicad_mod");
    let mut registry = crate::registry::Registry::new();
    registry.set("alpha", &alpha).unwrap();
    registry.set("beta", &beta).unwrap();
    let mut app = EutecticApp::new(DomainState::from_source_registry(
        "use beta\ninst C1 C_0402\npin C1 (1mm, 2mm)\n".to_string(),
        Some("collision.eut".to_string()),
        registry,
        None,
    ));
    let before_source = app.domain.source.clone();
    let before_def = app.domain.lib["C_0402"].clone();
    let before_component =
        app.domain.doc.as_ref().unwrap().components[&EntityId::new("C1")].clone();
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "C_0402")
        .cloned()
        .expect("collision winner is catalogued");
    assert_eq!(row.library, "beta", "document use order owns the row");
    assert_eq!(app.domain.catalog_lib["C_0402"], before_def);
    app.arm_library_part(&row);
    let preview_shapes = app.place_shapes.borrow().clone();
    assert!(!preview_shapes.is_empty());

    app.commit_armed_part(Point::mm(8, 9));

    assert_eq!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .source
            .iter()
            .filter(|directive| matches!(directive, eutectic_core::ir::GenDirective::Use { .. }))
            .count(),
        1,
        "an already-resolved part authors no use"
    );
    assert!(app.domain.source.starts_with("use beta\n"));
    assert_ne!(app.domain.source, before_source);
    assert_eq!(app.domain.lib["C_0402"], before_def);
    assert_eq!(app.domain.catalog_lib["C_0402"], app.domain.lib["C_0402"]);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components[&EntityId::new("C1")],
        before_component,
        "the existing instance retains its resolved definition and placement"
    );
    assert_eq!(
        app.library_preview_data(&row).unwrap().1,
        preview_shapes,
        "arm and thumbnail reuse the same isolated preview"
    );
}

#[test]
fn new_package_use_is_inserted_after_the_existing_use_block() {
    let scratch = Scratch::new("place-use-order");
    let alpha = scratch.0.join("alpha");
    let beta = scratch.0.join("beta");
    write_package(&alpha, "NEW_PART", "Inductor_2020.kicad_mod");
    write_package(&beta, "C_0402", "C_0402.kicad_mod");
    let mut registry = crate::registry::Registry::new();
    registry.set("alpha", &alpha).unwrap();
    registry.set("beta", &beta).unwrap();
    let app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 C_0402\nuse beta\npin C1 (1mm, 2mm)\n".to_string(),
        Some("use-order.eut".to_string()),
        registry,
        None,
    ));
    let text = app
        .placement_text_for_test("alpha", "NEW_PART", Point::mm(3, 4))
        .unwrap();
    let parsed = eutectic_core::text::parse(&text).unwrap();
    let uses: Vec<_> = parsed
        .source
        .iter()
        .enumerate()
        .filter_map(|(index, directive)| match directive {
            eutectic_core::ir::GenDirective::Use { name } => Some((index, name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(uses, vec![(1, "beta"), (2, "alpha")]);
}

#[test]
fn builtin_doc_resolution_owns_a_collision_with_an_unused_package() {
    let scratch = Scratch::new("place-builtin-collision");
    let alpha = scratch.0.join("alpha");
    write_package(&alpha, "Cap", "C_0402.kicad_mod");
    let mut registry = crate::registry::Registry::new();
    registry.set("alpha", &alpha).unwrap();
    let app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 Cap\n".to_string(),
        Some("builtin-collision.eut".to_string()),
        registry,
        None,
    ));
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .unwrap();
    assert_eq!(row.library, "builtin");
    assert_eq!(app.domain.catalog_lib["Cap"], app.domain.lib["Cap"]);
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
fn second_placement_does_not_renumber_the_first_refdes() {
    let mut app = edit_app();
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    app.commit_armed_part(Point::mm(4, 5));
    let first = EntityId::new("Cap3");
    assert_eq!(app.domain.doc.as_ref().unwrap().refdes_pins[&first], "Cap3");
    app.commit_armed_part(Point::mm(7, 8));
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.refdes_pins[&first], "Cap3");
    assert_eq!(doc.refdes_pins[&EntityId::new("Cap4")], "Cap4");
}

#[test]
fn placement_allocation_skips_auto_and_refdes_override_names() {
    let app = EutecticApp::new(DomainState::from_source(
        "inst first Cap\nrefdes first Cap2\ninst Cap1 Cap\n".to_string(),
        Some("pinned-refdes.eut".to_string()),
    ));
    let text = app
        .placement_text_for_test("builtin", "Cap", Point::mm(4, 5))
        .unwrap();
    assert!(text.contains("inst Cap3 Cap\n"), "{text}");
    assert!(text.contains("refdes Cap3 Cap3\n"), "{text}");
}

#[test]
fn digit_leading_part_commits_the_actual_u_family_refdes() {
    let mut lib = eutectic_core::part::part_library();
    let mut digit = lib["Cap"].clone();
    digit.name = "0402C".to_string();
    lib.insert("0402C".to_string(), digit);
    let mut app = EutecticApp::new(DomainState::from_source_with(
        String::new(),
        Some("digit.eut".to_string()),
        lib,
        |_| Vec::new(),
    ));
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "0402C")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    app.commit_armed_part(Point::mm(2, 3));
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.components[&EntityId::new("U1")].part, "0402C");
    assert_eq!(doc.refdes_pins[&EntityId::new("U1")], "U1");
}

#[test]
fn undo_plain_placement_restores_source_byte_for_byte() {
    let mut app = edit_app();
    let prior = eutectic_core::text::serialize(app.domain.doc.as_ref().unwrap());
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    app.commit_armed_part(Point::mm(4, 5));
    app.undo();
    assert_eq!(app.domain.source, prior);
}

#[test]
fn undo_use_adding_placement_restores_source_lib_and_armed_part() {
    let mut registry = crate::registry::Registry::new();
    let poc = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
    registry.set("poc", &poc).unwrap();
    let mut app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 Cap\n".to_string(),
        Some("undo-use.eut".to_string()),
        registry,
        None,
    ));
    let prior = app.domain.source.clone();
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.library == "poc" && row.part == "RP2350A")
        .cloned()
        .unwrap();
    let payload = app
        .placement_text_for_test("poc", "RP2350A", Point::mm(4, 5))
        .unwrap();
    app.arm_library_part(&row);
    app.commit_armed_part(Point::mm(4, 5));
    assert_eq!(
        eutectic_core::text::serialize(app.domain.doc.as_ref().unwrap()),
        payload,
        "the committed non-builtin document equals its LoadText payload"
    );
    assert!(app.domain.lib.contains_key("RP2350A"));

    app.undo();

    assert_eq!(app.domain.source, prior);
    assert!(!app.domain.lib.contains_key("RP2350A"));
    assert_eq!(app.armed_part_name().as_deref(), Some("RP2350A"));
    assert!(app.domain.catalog_lib.contains_key("RP2350A"));
    assert!(!app.place_shapes.borrow().is_empty());
}

#[test]
fn registered_load_failure_remains_a_finding_during_placement() {
    let scratch = Scratch::new("place-load-failure");
    let bad = scratch.0.join("bad");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join(eutectic_core::library::MANIFEST_NAME),
        "part Broken footprint=missing.kicad_mod\n",
    )
    .unwrap();
    let mut registry = crate::registry::Registry::new();
    registry.set("bad", &bad).unwrap();
    let mut app = EutecticApp::new(DomainState::from_source_registry(
        "inst C1 Cap\n".to_string(),
        Some("load-failure.eut".to_string()),
        registry,
        None,
    ));
    assert!(
        app.domain
            .lib_notes
            .iter()
            .any(|note| note.code == crate::registry::W_LIB_LOAD && note.message.contains("bad"))
    );
    assert!(
        app.derived
            .borrow()
            .findings
            .items
            .iter()
            .any(|finding| finding.code == crate::registry::W_LIB_LOAD)
    );
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    app.commit_armed_part(Point::mm(3, 4));
    assert!(
        app.domain
            .lib_notes
            .iter()
            .any(|note| note.code == crate::registry::W_LIB_LOAD && note.message.contains("bad"))
    );
    assert!(
        app.derived
            .borrow()
            .findings
            .items
            .iter()
            .any(|finding| finding.code == crate::registry::W_LIB_LOAD)
    );
}

#[test]
fn failing_isolated_preview_is_negative_cached_by_catalog_generation() {
    let app = edit_app();
    let row = crate::registry::LibraryPart {
        library: "builtin".to_string(),
        part: "MissingPreview".to_string(),
    };
    assert!(app.library_preview_data(&row).is_err());
    // The Err must actually be STORED — a regression that only caches Ok
    // results leaves the map empty and both calls re-elaborate every frame.
    assert_eq!(
        app.library_preview_data.borrow().len(),
        1,
        "failing elaboration is negative-cached"
    );
    assert!(app.library_preview_data(&row).is_err());
    assert_eq!(app.library_preview_data.borrow().len(), 1);
    let key = app
        .library_preview_data
        .borrow()
        .keys()
        .next()
        .cloned()
        .expect("one cached entry");
    assert_eq!(
        key.2, app.domain.catalog_generation,
        "cache keys by catalog generation, not doc revision"
    );
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
        app.pane_px.borrow()[0].unwrap(),
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

#[test]
fn escape_while_library_filter_is_active_disarms_place() {
    let mut app = edit_app();
    app.set_tool(ViewKind::Board, Tool::Place);
    app.open_library_browser();
    let row = app
        .domain
        .library_parts
        .iter()
        .find(|row| row.part == "Cap")
        .cloned()
        .unwrap();
    app.arm_library_part(&row);
    app.library_browser_ui.borrow_mut().query = "cap".to_string();
    let mut event = escape();
    event.selection = Some(Selection::caret(LIBRARY_FILTER_KEY, 3));
    app.on_event(event, &EventCx::new());
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Place);
    assert!(app.armed_part_name().is_none());
    assert!(app.library_browser_open.get());
}

#[test]
fn placement_click_snaps_to_the_displayed_grid_and_raw_when_toggled_off() {
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
    let pitch = app.displayed_grid_pitch(PaneId::A);
    // An off-lattice target: snapped and raw commits must land differently.
    let target = Point {
        x: 4 * MM + pitch / 3,
        y: 5 * MM + pitch / 3,
    };
    let px = crate::app::canvas_pane::pane_project(
        &app.pane_camera(PaneId::A),
        app.pane_px.borrow()[0].unwrap(),
        target,
    );
    // The commit sees the px round-trip of `target` (sub-pixel nm error), so
    // derive expectations from that exact point.
    let roundtrip = crate::app::canvas_pane::pane_unproject(
        &app.pane_camera(PaneId::A),
        app.pane_px.borrow()[0].unwrap(),
        px,
    );
    let snapped = crate::app::snap_point(roundtrip, pitch);
    assert_ne!(
        snapped, roundtrip,
        "target must be off-lattice at this pitch"
    );

    assert!(app.snap_to_grid(), "snap defaults on");
    app.arm_library_part(&row);
    app.on_event(pointer(UiEventKind::Click, px), &cx);
    let doc = app.domain.doc.as_ref().unwrap();
    assert!(
        doc.components
            .values()
            .any(|component| component.pos.value == snapped),
        "snapped placement commits on the displayed grid lattice"
    );

    app.on_event(
        click(crate::chrome::menubar::SNAP_TO_GRID_KEY),
        &EventCx::new(),
    );
    assert!(!app.snap_to_grid());
    app.arm_library_part(&row);
    app.on_event(pointer(UiEventKind::Click, px), &cx);
    let doc = app.domain.doc.as_ref().unwrap();
    assert!(
        doc.components
            .values()
            .any(|component| component.pos.value == roundtrip),
        "raw placement commits the unprojected point with snap off"
    );
}

#[test]
fn place_menu_row_selects_the_tool_and_opens_the_browser() {
    let mut app = edit_app();
    assert_ne!(app.tool_for(ViewKind::Board), Tool::Place);
    app.on_event(
        click(crate::chrome::menubar::PLACE_PART_KEY),
        &EventCx::new(),
    );
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Place);
    assert!(app.library_browser_open.get());
}
