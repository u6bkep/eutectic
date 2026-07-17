//! Headless chrome behavior: convention exports, focused camera zoom,
//! app-level units/grid state, Help dialogs, findings, quit, and inert rows.

use super::*;
use crate::app::canvas_pane::{MAX_ZOOM, MIN_ZOOM};
use crate::app::pane::{REDO_KEY, SAVE_KEY, UNDO_KEY};
use crate::chrome::actions::*;
use crate::chrome::dialogs::{ABOUT_KEY, ChromeDialog, KEYMAP_KEY, WIRED_CHORDS};
use crate::chrome::menubar::{MenuRow, menu_defs};
use crate::render::camera::Camera;
use damascene_core::runtime::RunnerCore;

fn all_texts(el: &El) -> Vec<String> {
    fn walk(el: &El, out: &mut Vec<String>) {
        if let Some(text) = &el.text {
            out.push(text.clone());
        }
        for child in &el.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    walk(el, &mut out);
    out
}

fn find_text_el<'a>(el: &'a El, needle: &str) -> Option<&'a El> {
    if el.text.as_deref() == Some(needle) {
        return Some(el);
    }
    el.children
        .iter()
        .find_map(|child| find_text_el(child, needle))
}

fn source_backed_app(tag: &str) -> (Scratch, EutecticApp, std::path::PathBuf) {
    let scratch = Scratch::new(tag);
    let mut app = board();
    let path = scratch.0.join("demo-board.eut");
    std::fs::write(&path, &app.domain.source).expect("copy fixture source");
    app.domain.source_path = Some(path.clone());
    (scratch, app, path)
}

/// Spell the public `KeyChord` fields exactly as the Keymap renders them.
/// Keeping this on the test side makes the dialog's literal inventory fail
/// when registration changes without a matching content update.
fn chord_label(chord: &KeyChord) -> String {
    let ChordTrigger::Logical(LogicalKey::Character(key)) = &chord.trigger else {
        panic!("the app currently registers only logical character chords")
    };
    let mut parts: Vec<String> = Vec::new();
    if chord.modifiers.ctrl {
        parts.push("Ctrl".to_string());
    }
    if chord.modifiers.shift {
        parts.push("Shift".to_string());
    }
    if chord.modifiers.alt {
        parts.push("Alt".to_string());
    }
    if chord.modifiers.logo {
        parts.push("Meta".to_string());
    }
    parts.push(
        if key.is_ascii() && key.chars().all(|c| c.is_ascii_alphabetic()) {
            key.to_ascii_uppercase()
        } else {
            key.clone()
        },
    );
    parts.join("+")
}

fn hotkey_action_label(action: &str) -> &'static str {
    match action {
        SAVE_KEY => "Save",
        UNDO_KEY => "Undo",
        REDO_KEY => "Redo",
        ZOOM_IN_KEY => "Zoom in",
        ZOOM_OUT_KEY => "Zoom out",
        other => panic!("unlisted registered hotkey action: {other}"),
    }
}

#[test]
fn export_actions_write_full_fab_set_and_both_svgs() {
    let (_scratch, mut app, source) = source_backed_app("chrome-export");
    let expected_fab =
        eutectic_core::export::gerber_set(app.domain.doc.as_ref().unwrap(), &app.domain.lib)
            .unwrap();
    let cx = EventCx::new();

    app.on_event(click(EXPORT_GERBERS_KEY), &cx);
    let fab = source.parent().unwrap().join("fab");
    for (name, content) in &expected_fab {
        assert_eq!(
            std::fs::read_to_string(fab.join(name)).unwrap(),
            *content,
            "fab export writes engine output verbatim for {name}"
        );
    }
    let notice = app.chrome_notice.borrow().clone().expect("success notice");
    assert!(!notice.error);
    assert!(
        notice
            .message
            .contains(&format!("{} fab files", expected_fab.len()))
    );
    assert!(notice.message.contains(&fab.display().to_string()));

    app.on_event(click(EXPORT_SVG_KEY), &cx);
    let board_svg = source.parent().unwrap().join("demo-board.svg");
    let schematic_svg = source.parent().unwrap().join("demo-board-schematic.svg");
    assert!(std::fs::read_to_string(board_svg).unwrap().contains("<svg"));
    assert!(
        std::fs::read_to_string(schematic_svg)
            .unwrap()
            .contains("<svg")
    );
    let notice = app.chrome_notice.borrow().clone().expect("success notice");
    assert!(!notice.error);
    assert!(notice.message.contains("2 SVG files"));
}

#[test]
fn export_uses_unsaved_live_document_without_rewriting_source() {
    let (_scratch, mut app, source) = source_backed_app("chrome-live-export");
    let disk_before = std::fs::read_to_string(&source).unwrap();
    let svg_before =
        eutectic_core::export::svg(app.domain.doc.as_ref().unwrap(), &app.domain.lib).unwrap();

    commit_move(&mut app, -2, 0);
    assert!(app.dirty(), "the export happens before an explicit save");
    let live_svg =
        eutectic_core::export::svg(app.domain.doc.as_ref().unwrap(), &app.domain.lib).unwrap();
    assert_ne!(
        live_svg, svg_before,
        "the in-memory edit changes SVG output"
    );

    app.on_event(click(EXPORT_SVG_KEY), &EventCx::new());
    let exported = std::fs::read_to_string(source.with_extension("svg")).unwrap();
    assert_eq!(exported, live_svg, "export snapshots the live edited doc");
    assert_ne!(exported, svg_before, "the export reflects the unsaved move");
    assert_eq!(
        std::fs::read_to_string(&source).unwrap(),
        disk_before,
        "export never saves or rewrites the source document"
    );
}

#[test]
fn export_write_failure_surfaces_a_destructive_notice_without_panicking() {
    let (_scratch, mut app, source) = source_backed_app("chrome-export-failure");
    let output = source.with_extension("svg");
    std::fs::create_dir(&output).expect("directory blocks the SVG output file");

    app.on_event(click(EXPORT_SVG_KEY), &EventCx::new());
    let notice = app.chrome_notice.borrow().clone().expect("failure notice");
    assert!(notice.error);
    assert!(notice.message.contains("SVG export failed"));
    assert!(notice.message.contains("writing"));

    let bar = app.menubar_bar();
    let chip = find_text_el(&bar, &notice.message).expect("export failure chip is rendered");
    assert_eq!(
        chip.text_color,
        Some(damascene_core::tokens::DESTRUCTIVE),
        "an export failure is rendered as a destructive chip"
    );
}

#[test]
fn export_notice_clears_after_reload_revert_and_successful_save() {
    let (_scratch, mut app, _source) = source_backed_app("chrome-export-lifecycle");
    let cx = EventCx::new();

    app.on_event(click(EXPORT_SVG_KEY), &cx);
    assert!(app.chrome_notice.borrow().is_some());
    app.apply_reload(app.domain.source.clone());
    assert!(
        app.chrome_notice.borrow().is_none(),
        "a successful document reload clears the prior export result"
    );

    commit_move(&mut app, -1, 0);
    app.on_event(click(EXPORT_SVG_KEY), &cx);
    assert!(app.chrome_notice.borrow().is_some());
    app.save();
    assert!(
        app.chrome_notice.borrow().is_none(),
        "a successful save clears the prior export result"
    );

    app.on_event(click(EXPORT_SVG_KEY), &cx);
    assert!(app.chrome_notice.borrow().is_some());
    app.revert_to_saved();
    assert!(
        app.chrome_notice.borrow().is_none(),
        "a successful Revert to Saved clears the prior export result"
    );
}

#[test]
fn export_rows_are_disabled_without_a_source_path() {
    let mut app = board();
    app.set_open_menu(Some("file"));
    let menu = app.menu_overlay().unwrap();
    fn keyed_label(el: &El, label: &str) -> Option<Option<String>> {
        let hit = el.text.as_deref() == Some(label)
            || el.children.iter().any(|c| c.text.as_deref() == Some(label));
        if hit && matches!(&el.kind, Kind::Custom(name) if *name == "menubar_item") {
            return Some(el.key.clone());
        }
        el.children.iter().find_map(|c| keyed_label(c, label))
    }
    assert_eq!(keyed_label(&menu, "Export Gerbers…"), Some(None));
    assert_eq!(keyed_label(&menu, "Export SVG…"), Some(None));
    app.on_event(click(EXPORT_GERBERS_KEY), &EventCx::new());
    let notice = app.chrome_notice.borrow().clone().expect("failure notice");
    assert!(notice.error);
    assert!(notice.message.contains("no source path"));
}

#[test]
fn zoom_targets_only_the_focused_pane_and_clamps() {
    let mut app = board();
    let _ = settle(&mut app);
    let cx = EventCx::new();
    app.focused_pane.set(PaneId::B);
    let a0 = app.pane_camera_target(PaneId::A);
    let b0 = app.pane_camera_target(PaneId::B);
    app.on_event(click(ZOOM_IN_KEY), &cx);
    assert_eq!(app.pane_camera_target(PaneId::A), a0);
    let b1 = app.pane_camera_target(PaneId::B);
    assert_eq!(b1.center, b0.center, "pane-centre zoom preserves centre");
    assert!((b1.zoom - b0.zoom * 1.25).abs() < 1e-15);
    app.on_event(click(ZOOM_OUT_KEY), &cx);
    assert!((app.pane_camera_target(PaneId::B).zoom - b0.zoom).abs() < 1e-15);

    app.pane_cams.borrow_mut()[1]
        .glide
        .snap(Camera::new((7.0, 9.0), MAX_ZOOM));
    app.on_event(click(ZOOM_IN_KEY), &cx);
    assert_eq!(app.pane_camera_target(PaneId::B).zoom, MAX_ZOOM);
    app.pane_cams.borrow_mut()[1]
        .glide
        .snap(Camera::new((7.0, 9.0), MIN_ZOOM));
    app.on_event(click(ZOOM_OUT_KEY), &cx);
    assert_eq!(app.pane_camera_target(PaneId::B).zoom, MIN_ZOOM);
}

#[test]
fn runtime_zoom_hotkey_does_not_steal_library_path_hyphen() {
    let mut app = board();
    app.focused_pane.set(PaneId::B);
    let before = app.pane_camera_target(PaneId::B);

    // Exercise actual damascene hotkey matching, not a fabricated Hotkey event.
    let mut runtime = RunnerCore::new();
    runtime.set_hotkeys(app.hotkeys());
    let ctrl = KeyModifiers {
        ctrl: true,
        ..Default::default()
    };
    let events = runtime.key_down(
        LogicalKey::Character("-".into()),
        PhysicalKey::Minus,
        ctrl,
        false,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, UiEventKind::Hotkey);
    for event in events {
        app.on_event(event, &EventCx::new());
    }
    assert_eq!(
        app.pane_camera_target(PaneId::B).zoom,
        before.zoom / 1.25,
        "Ctrl+- zooms the focused pane through the runtime"
    );

    // Build and lay out the real Libraries modal, then focus its path input
    // with a runtime pointer press exactly as the host does.
    app.set_libraries_open(true);
    let rendered = settle(&mut app);
    let path = rendered
        .ui
        .rect_of_key("libraries:input:path")
        .expect("Libraries path input is laid out");
    let mut runtime = RunnerCore::new();
    runtime.ui_state = rendered.ui;
    runtime.ui_state.sync_focus_order(&rendered.tree);
    runtime.last_tree = Some(rendered.tree);
    runtime.set_hotkeys(app.hotkeys());
    let pointer = Pointer::mouse(
        path.x + path.w / 2.0,
        path.y + path.h / 2.0,
        PointerButton::Primary,
    );
    assert!(
        runtime.would_press_focus_text_input(pointer.x, pointer.y),
        "the runtime hit-test sees the Libraries path input"
    );
    for event in runtime.pointer_down(pointer) {
        app.on_event(event, &EventCx::new());
    }
    assert!(
        runtime.focused_captures_keys(),
        "pointer-down focuses the path input"
    );
    for event in runtime.pointer_up(pointer) {
        app.on_event(event, &EventCx::new());
    }

    let camera_before_typing = app.pane_camera_target(PaneId::B);
    let events = runtime.key_down(
        LogicalKey::Character("-".into()),
        PhysicalKey::Minus,
        KeyModifiers::default(),
        false,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].kind,
        UiEventKind::KeyDown,
        "bare - must reach the focused input rather than match a hotkey"
    );
    for event in events {
        app.on_event(event, &EventCx::new());
    }
    let text = runtime
        .text_input("-".to_string())
        .expect("focused input receives composed text from the same key press");
    app.on_event(text, &EventCx::new());
    assert!(
        app.lib_ui.borrow().path.contains('-'),
        "the focused path input receives the typed hyphen"
    );
    assert_eq!(
        app.pane_camera_target(PaneId::B),
        camera_before_typing,
        "typing into the modal must not move the pane behind its scrim"
    );
}

#[test]
fn chrome_hotkeys_are_suppressed_while_modal_chrome_owns_input() {
    for owner in ["libraries", "menu", "dialog"] {
        let mut app = board();
        let before = app.pane_camera_target(PaneId::A);
        match owner {
            "libraries" => app.set_libraries_open(true),
            "menu" => app.set_open_menu(Some("view")),
            "dialog" => app.chrome_dialog.set(Some(ChromeDialog::Keymap)),
            _ => unreachable!(),
        }
        app.on_event(hotkey(ZOOM_OUT_KEY), &EventCx::new());
        assert_eq!(
            app.pane_camera_target(PaneId::A),
            before,
            "{owner} must own chrome hotkeys"
        );
    }
}

#[test]
fn units_toggle_converts_cursor_and_measure_readouts() {
    let mut app = board();
    app.cursor_board_mm.set(Some((25.4, 50.8)));
    app.set_tool(ViewKind::Board, Tool::Measure);
    let mut measure = crate::tool::MeasureState::default();
    measure.click(Point { x: 0, y: 0 });
    measure.hover(Point {
        x: 25_400_000,
        y: 50_800_000,
    });
    app.set_measure(measure);
    let mm = all_texts(&app.status_bar(1.0));
    assert!(mm.iter().any(|t| t == "X 25.40  Y 50.80 mm"));
    assert!(mm.iter().any(|t| t.contains("dx 25.40  dy 50.80")));

    app.on_event(click(UNITS_TOGGLE_KEY), &EventCx::new());
    let inches = all_texts(&app.status_bar(1.0));
    assert!(inches.iter().any(|t| t == "X 1.00  Y 2.00 in"));
    assert!(
        inches
            .iter()
            .any(|t| { t.contains("dx 1.00  dy 2.00") && t.ends_with(" in") })
    );
    assert!(
        all_texts(&app.viewer_toolbar())
            .iter()
            .any(|t| t == "Units: in")
    );
}

#[test]
fn grid_toggle_changes_state_and_damage_generation() {
    let mut app = board();
    assert_eq!(app.grid_style(), GridStyle::Dots);
    let generation = app.style_rev.get();
    app.on_event(click(GRID_TOGGLE_KEY), &EventCx::new());
    assert_eq!(app.grid_style(), GridStyle::Lines);
    assert_eq!(app.style_rev.get(), generation + 1);
    app.on_event(click(GRID_TOGGLE_KEY), &EventCx::new());
    assert_eq!(app.grid_style(), GridStyle::Dots);
}

#[test]
fn help_dialogs_list_only_wired_chords_and_real_version() {
    let mut app = board();
    let mut registered: Vec<(String, String)> = app
        .hotkeys()
        .iter()
        .map(|(chord, action)| (chord_label(chord), hotkey_action_label(action).to_string()))
        .collect();
    registered.push((
        "Esc".to_string(),
        "Cancel gesture/tool or clear selection".to_string(),
    ));
    let documented: Vec<(String, String)> = WIRED_CHORDS
        .iter()
        .map(|(chord, action)| ((*chord).to_string(), (*action).to_string()))
        .collect();
    assert_eq!(
        documented, registered,
        "Keymap rows must exactly match registered chords plus raw Escape"
    );

    app.on_event(click(KEYMAP_KEY), &EventCx::new());
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::Keymap));
    let texts = all_texts(&app.chrome_dialog_overlay().unwrap());
    for (chord, action) in WIRED_CHORDS {
        assert!(texts.iter().any(|t| t == chord));
        assert!(texts.iter().any(|t| t == action));
    }
    app.chrome_dialog.set(None);
    app.on_event(click(ABOUT_KEY), &EventCx::new());
    let texts = all_texts(&app.chrome_dialog_overlay().unwrap());
    assert!(
        texts
            .iter()
            .any(|t| t == &format!("eutectic {}", env!("CARGO_PKG_VERSION")))
    );
}

#[test]
fn findings_and_quit_rows_dispatch_and_unimplemented_rows_stay_inert() {
    let mut app = board();
    assert!(!app.section_open(crate::app::pane::SidebarSection::Findings));
    app.on_event(click(FINDINGS_PANEL_KEY), &EventCx::new());
    assert!(app.section_open(crate::app::pane::SidebarSection::Findings));
    app.on_event(click(QUIT_KEY), &EventCx::new());
    assert!(app.quit_requested());

    for label in [
        "Open…",
        "Copy",
        "Paste",
        "Command Palette…",
        "Flip Board (bottom view)",
        "Autoroute Net",
        "Autoroute Board",
        "Preferences…",
    ] {
        let row = menu_defs()
            .into_iter()
            .flat_map(|menu| menu.rows)
            .find(|row| match row {
                MenuRow::Disabled { label: found, .. } => *found == label,
                _ => false,
            });
        assert!(row.is_some(), "{label} must remain visible and inert");
    }
    assert!(
        menu_defs()
            .into_iter()
            .flat_map(|menu| menu.rows)
            .all(|row| !matches!(row, MenuRow::Wired { label: "Snap", .. })),
        "no snap action is advertised without snapping semantics"
    );
}
