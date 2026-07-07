//! Library-packages tests (slice 2): registry-driven resolution, the Libraries
//! menu's live edit semantics, and their interaction with the reload / editing
//! state. Moved verbatim from `app.rs` (gui-module-split).

use super::*;
use crate::registry::Registry;

// -----------------------------------------------------------------------
// Library packages, slice 2: registry-driven resolution + the Libraries
// menu's live edit semantics. All headless; registries live in scratch
// dirs (never the per-user config — the path is injected).
// -----------------------------------------------------------------------

/// The in-repo poc library package directory (an absolute path — the
/// crate manifest dir is absolute).
fn poc_parts_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts")
}

/// A one-instance source that only the poc package can resolve.
const USE_POC_SRC: &str = "use poc\ninst U1 RP2350A\n";

/// The Libraries-menu add flow end to end: with `use poc` unregistered the
/// doc loads degraded (instance skipped, W_LIB_UNREGISTERED in the
/// findings); adding the poc entry through the menu saves the registry
/// file, re-resolves + re-elaborates through the reload path (revision
/// bump), and the part now resolves. Removing it degrades again.
#[test]
fn registry_add_and_remove_reresolve_the_current_doc() {
    let scratch = Scratch::new("add-remove");
    let save = scratch.0.join("libraries");
    let mut app = EcadApp::new(DomainState::from_source_registry(
        USE_POC_SRC.to_string(),
        Some("t.ecad".to_string()),
        Registry::new(),
        Some(save.clone()),
    ));
    let doc = app.domain.doc.as_ref().expect("degraded load succeeds");
    assert!(doc.components.is_empty(), "RP2350A unresolved at first");
    assert!(
        app.findings()
            .items
            .iter()
            .any(|i| i.code == "W_LIB_UNREGISTERED"),
        "the unregistered use renders in the findings"
    );
    assert_eq!(app.revision(), 0);

    // Drive the menu: open, fill the add-entry inputs, click Add.
    let cx = EventCx::new();
    app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
    assert!(app.libraries_open.get(), "toolbar button opens the menu");
    app.set_library_inputs("poc", poc_parts_dir().to_str().unwrap());
    app.on_event(click(LIBRARIES_ADD_KEY), &cx);

    assert_eq!(
        app.revision(),
        1,
        "a registry edit re-elaborates through the reload path (bump once)"
    );
    assert_eq!(app.library_edit_error(), None);
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.components.len(), 1, "RP2350A resolves after the add");
    assert!(
        !app.findings()
            .items
            .iter()
            .any(|i| i.code == "W_LIB_UNREGISTERED" || i.code == "W_UNRESOLVED_PART"),
        "the library findings clear once the name binds"
    );
    // Live edit semantics: the registry file was saved immediately.
    let back = Registry::load(&save).expect("saved registry loads");
    assert_eq!(back.get("poc"), Some(poc_parts_dir().as_path()));
    // The add cleared the inputs.
    assert_eq!(app.lib_ui.borrow().name, "");
    assert_eq!(app.lib_ui.borrow().path, "");

    // Remove flow: the row's Remove button unbinds + re-resolves again.
    app.on_event(click(&library_remove_key("poc")), &cx);
    assert_eq!(app.revision(), 2);
    assert!(
        app.domain.doc.as_ref().unwrap().components.is_empty(),
        "unbinding the library degrades the doc again"
    );
    let back = Registry::load(&save).expect("saved registry loads");
    assert!(back.is_empty(), "the removal was saved");
}

/// A relative path in the add form is rejected at the boundary: the error
/// renders inline, nothing is saved, and the doc is untouched (no revision
/// bump).
#[test]
fn registry_add_rejects_relative_path_inline() {
    let scratch = Scratch::new("relative");
    let save = scratch.0.join("libraries");
    let mut app = EcadApp::new(DomainState::from_source_registry(
        USE_POC_SRC.to_string(),
        Some("t.ecad".to_string()),
        Registry::new(),
        Some(save.clone()),
    ));
    let cx = EventCx::new();
    app.set_libraries_open(true);
    app.set_library_inputs("poc", "relative/path");
    app.on_event(click(LIBRARIES_ADD_KEY), &cx);
    let err = app.library_edit_error().expect("inline error set");
    assert!(err.contains("absolute"), "{err}");
    assert_eq!(app.revision(), 0, "no re-elaborate on a rejected edit");
    assert!(!save.exists(), "nothing saved on a rejected edit");
    // The inputs stay for correction.
    assert_eq!(app.lib_ui.borrow().path, "relative/path");
}

/// A source reload that ADDS a `use` line re-runs resolution against the
/// registry — the lib is re-derived per load, not fixed at open time.
#[test]
fn reload_reresolves_use_names() {
    let mut registry = Registry::new();
    registry.set("poc", &poc_parts_dir()).unwrap();
    let mut app = EcadApp::new(DomainState::from_source_registry(
        "inst U1 RP2350A\n".to_string(),
        Some("t.ecad".to_string()),
        registry,
        None,
    ));
    assert!(
        app.domain.doc.as_ref().unwrap().components.is_empty(),
        "without a use line the registry is not consulted"
    );
    app.apply_reload(USE_POC_SRC.to_string());
    assert_eq!(app.revision(), 1);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().components.len(),
        1,
        "the reload's new `use poc` resolves through the registry"
    );
}

/// A registry edit while a reload-error banner is up re-elaborates the
/// last-GOOD source — which says nothing about the newer broken source on
/// disk, so the banner must survive the registry-triggered reload.
#[test]
fn registry_edit_preserves_a_standing_reload_error() {
    let mut registry = Registry::new();
    registry.set("poc", &poc_parts_dir()).unwrap();
    let mut app = EcadApp::new(DomainState::from_source_registry(
        USE_POC_SRC.to_string(),
        Some("t.ecad".to_string()),
        registry,
        None,
    ));
    // A broken disk source arrives: banner up, last-good doc stays.
    app.apply_reload(BROKEN_SRC.to_string());
    assert!(app.reload_error().is_some(), "banner up");

    // A registry edit re-resolves the last-good source; the banner stays.
    let cx = EventCx::new();
    app.set_libraries_open(true);
    app.on_event(click(&library_remove_key("poc")), &cx);
    assert!(
        app.reload_error().is_some(),
        "the banner must survive a registry-triggered reload of the stale-good source"
    );
}

/// A registry edit while dirty preserves the editing state: the doc
/// re-elaborates from the serialize-refreshed source (unsaved edits
/// included), and dirty + undo survive — only an EXTERNAL reload resets them.
#[test]
fn registry_edit_preserves_dirty_and_undo() {
    let mut registry = Registry::new();
    registry.set("poc", &poc_parts_dir()).unwrap();
    let mut app = EcadApp::new(DomainState::from_source_registry(
        "inst C1 Cap\ninst C2 Cap\nnet N C1.p1 C2.p1\n\
         board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)\n"
            .to_string(),
        Some("t.ecad".to_string()),
        registry,
        None,
    ));
    let comp = EntityId::new("C1");
    let p = comp_pos(&app, &comp);
    let target = Point {
        x: p.x + 2 * NM_PER_MM,
        y: p.y,
    };
    app.commit_edit(Transaction::one(Command::Pin(comp.clone(), target)), "move")
        .expect("commits");
    assert!(app.dirty());

    let cx = EventCx::new();
    app.set_libraries_open(true);
    app.on_event(click(&library_remove_key("poc")), &cx);

    assert!(app.dirty(), "a registry edit must not clear dirty");
    assert_eq!(app.undo_depths(), (1, 0), "undo survives a registry edit");
    assert_eq!(
        comp_pos(&app, &comp),
        target,
        "the unsaved edit survives the registry-triggered re-elaborate"
    );
}

/// Escape closes the Libraries menu (and is consumed — the selection
/// survives), and the scrim/close affordances work.
#[test]
fn libraries_menu_escape_and_close() {
    let mut app = EcadApp::new(schematic_domain());
    let cx = EventCx::new();
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("VDD")));

    app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
    assert!(app.libraries_open.get());
    // damascene has no generic synthetic constructor; shape an Escape by hand.
    let mut esc = UiEvent::synthetic_click("");
    esc.key = None;
    esc.kind = UiEventKind::Escape;
    app.on_event(esc, &cx);
    assert!(!app.libraries_open.get(), "Escape closes the menu");
    assert!(
        !app.domain.selection.borrow().is_empty(),
        "Escape was consumed by the menu — the selection survives"
    );

    app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
    app.on_event(click(LIBRARIES_CLOSE_KEY), &cx);
    assert!(!app.libraries_open.get(), "Close button closes the menu");
}
