//! Headless chrome behavior: convention exports, focused camera zoom,
//! app-level units/grid state, Help dialogs, findings, quit, and inert rows.

use super::*;
use crate::app::canvas_pane::{MAX_ZOOM, MIN_ZOOM};
use crate::chrome::actions::*;
use crate::chrome::dialogs::{ABOUT_KEY, ChromeDialog, KEYMAP_KEY, WIRED_CHORDS};
use crate::chrome::menubar::{MenuRow, menu_defs};
use crate::render::camera::Camera;

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

fn source_backed_app(tag: &str) -> (Scratch, EutecticApp, std::path::PathBuf) {
    let scratch = Scratch::new(tag);
    let mut app = board();
    let path = scratch.0.join("demo-board.eut");
    std::fs::write(&path, &app.domain.source).expect("copy fixture source");
    app.domain.source_path = Some(path.clone());
    (scratch, app, path)
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
    app.on_event(click(KEYMAP_KEY), &EventCx::new());
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::Keymap));
    let texts = all_texts(&app.chrome_dialog_overlay().unwrap());
    for (chord, action) in WIRED_CHORDS {
        assert!(texts.iter().any(|t| t == chord));
        assert!(texts.iter().any(|t| t == action));
    }
    assert_eq!(
        app.hotkeys().len() + 1,
        WIRED_CHORDS.len(),
        "keymap rows are the registered hotkeys plus the directly handled Escape key"
    );

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
