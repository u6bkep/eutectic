//! Menu-bar event-dispatch tests (UI oracle region 1): top-level trigger
//! open/close/switch, and the wired rows routing to the *same* actions the
//! retired toolbar buttons dispatched to (Save / Revert / Libraries / Undo /
//! Redo / Fit) while closing the menu. The menu *model* (which rows are wired vs
//! disabled) is asserted in `chrome::menubar`'s own unit tests.

use super::*;
use crate::app::open::recent_item_key;
use crate::chrome::dialogs::ChromeDialog;
use crate::chrome::menubar::{FIT_KEY, MENUBAR_KEY, REVERT_KEY};
use crate::fixtures::dirty_doc;
use crate::recents::RecentFiles;

/// The routed key a top-level trigger for `value` emits.
fn trigger(value: &str) -> String {
    menubar_trigger_key(MENUBAR_KEY, &value)
}

/// A trigger click opens its menu; a different trigger switches; re-clicking the
/// open one closes it.
#[test]
fn trigger_click_toggles_and_switches_the_open_menu() {
    let mut app = board();
    let cx = EventCx::new();
    assert!(app.open_menu.borrow().is_none());

    app.on_event(click(&trigger("file")), &cx);
    assert_eq!(app.open_menu.borrow().as_deref(), Some("file"));

    app.on_event(click(&trigger("edit")), &cx);
    assert_eq!(app.open_menu.borrow().as_deref(), Some("edit"));

    app.on_event(click(&trigger("edit")), &cx);
    assert!(app.open_menu.borrow().is_none());
}

/// The Edit ▸ Undo row (keyed [`UNDO_KEY`]) runs undo AND closes the open menu —
/// same route the retired toolbar Undo button used.
#[test]
fn undo_row_dispatches_and_closes_the_menu() {
    let mut app = dirty_doc(); // one committed edit → one undo unit.
    assert_eq!(app.undo_depths(), (1, 0));
    let cx = EventCx::new();
    app.set_open_menu(Some("edit"));

    app.on_event(click(UNDO_KEY), &cx);
    assert_eq!(app.undo_depths(), (0, 1), "the Undo row ran undo");
    assert!(
        app.open_menu.borrow().is_none(),
        "invoking a row closes the menu"
    );
}

/// The View ▸ Fit row (keyed [`FIT_KEY`]) queues a pane-camera Fit request
/// (fit every pane) — same route the retired toolbar Fit button used, on the
/// owned cameras (WP3: the viewport-request queue is gone).
#[test]
fn fit_row_dispatches_to_the_fit_action() {
    let mut app = board();
    let cx = EventCx::new();
    app.set_open_menu(Some("view"));

    app.on_event(click(FIT_KEY), &cx);
    assert!(
        app.pane_cams.borrow()[0].as_ref().unwrap().request
            == Some(crate::app::canvas_pane::CamRequest::Fit),
        "the Fit row queued a pane-camera Fit request"
    );
    assert!(app.open_menu.borrow().is_none());
}

/// The File ▸ Libraries row (keyed [`LIBRARIES_TOGGLE_KEY`]) opens the Libraries
/// modal and closes the menu — same route the retired toolbar Libraries button
/// used.
#[test]
fn libraries_row_opens_the_modal_and_closes_the_menu() {
    let mut app = board();
    let cx = EventCx::new();
    app.set_open_menu(Some("file"));
    assert!(!app.libraries_open.get());

    app.on_event(click(LIBRARIES_TOGGLE_KEY), &cx);
    assert!(
        app.libraries_open.get(),
        "the Libraries row opened the modal"
    );
    assert!(app.open_menu.borrow().is_none());
}

fn app_with_recent(path: std::path::PathBuf) -> EutecticApp {
    let mut recents = RecentFiles::new();
    recents.push(path);
    let app = board().with_recents(recents, None);
    app.set_open_menu(Some("file"));
    app.recent_open.set(true);
    app
}

#[test]
fn recent_pointer_down_does_not_open_document() {
    let scratch = Scratch::new("recent-pointer-down");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, crate::fixtures::SAMPLE_ECAD).expect("write target");
    let mut app = app_with_recent(target.clone());
    let source_before = app.domain.source.clone();
    let mut event = click(&recent_item_key(0));
    event.kind = UiEventKind::PointerDown;

    app.on_event(event, &EventCx::new());

    assert_eq!(app.domain.source, source_before);
    assert_ne!(app.domain.source_path.as_deref(), Some(target.as_path()));
}

#[test]
fn recent_click_and_activate_open_document() {
    for kind in [UiEventKind::Click, UiEventKind::Activate] {
        let scratch = Scratch::new("recent-activate");
        let target = scratch.0.join("target.eut");
        std::fs::write(&target, crate::fixtures::SAMPLE_ECAD).expect("write target");
        let mut app = app_with_recent(target.clone());
        let mut event = click(&recent_item_key(0));
        event.kind = kind;

        app.on_event(event, &EventCx::new());

        assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
    }
}

#[test]
fn recent_escape_closes_menu_without_opening_document() {
    let scratch = Scratch::new("recent-escape");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, crate::fixtures::SAMPLE_ECAD).expect("write target");
    let mut app = app_with_recent(target.clone());
    let source_before = app.domain.source.clone();
    let mut event = click(&recent_item_key(0));
    event.kind = UiEventKind::Escape;

    app.on_event(event, &EventCx::new());

    assert_eq!(app.domain.source, source_before);
    assert!(app.open_menu.borrow().is_none());
    assert!(!app.recent_open.get());
    assert_ne!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
}

#[test]
fn save_undo_redo_hotkeys_are_blocked_while_a_menu_is_open() {
    for action in [SAVE_KEY, UNDO_KEY, REDO_KEY] {
        let scratch = Scratch::new("menu-edit-hotkey");
        let file = scratch.0.join("board.eut");
        let mut app = edit_app();
        app.domain.source_path = Some(file.clone());
        commit_move(&mut app, 1, 0);
        if action == REDO_KEY {
            app.undo();
        }
        let before = (
            app.domain.source.clone(),
            app.undo_depths(),
            app.dirty(),
            file.exists(),
        );
        app.set_open_menu(Some("edit"));

        app.on_event(hotkey(action), &EventCx::new());

        assert_eq!(
            (
                app.domain.source.clone(),
                app.undo_depths(),
                app.dirty(),
                file.exists(),
            ),
            before,
            "{action} must not reach the document behind an open menu"
        );
    }
}

/// The File ▸ Revert to Saved row (keyed [`REVERT_KEY`]) re-reads the document
/// from disk and applies it, discarding the in-memory edit (dirty + undo cleared,
/// the disk position restored) — the new wired action this slice adds.
#[test]
fn revert_row_rereads_disk_and_discards_edits() {
    let scratch = Scratch::new("revert");
    let mut app = edit_app();
    let file = scratch.0.join("board.eut");
    // Persist the pristine (pre-edit) source, then point the doc at that file.
    std::fs::write(&file, &app.domain.source).expect("write pristine source");
    app.domain.source_path = Some(file.clone());

    let c1 = EntityId::new("C1");
    let before = comp_pos(&app, &c1);
    commit_move(&mut app, 3, 1);
    assert!(app.dirty(), "the edit dirtied the doc");
    assert_ne!(comp_pos(&app, &c1), before, "the edit moved C1");

    let cx = EventCx::new();
    app.set_open_menu(Some("file"));
    app.on_event(click(REVERT_KEY), &cx);

    assert!(!app.dirty(), "revert cleared dirty");
    assert_eq!(
        app.undo_depths(),
        (0, 0),
        "revert cleared the undo/redo stacks"
    );
    assert_eq!(
        comp_pos(&app, &c1),
        before,
        "revert restored the on-disk position"
    );
    assert!(app.open_menu.borrow().is_none(), "the row closed the menu");
}
