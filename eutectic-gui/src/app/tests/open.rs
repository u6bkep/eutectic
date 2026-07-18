use super::*;
use crate::app::open::OPEN_KEY;
use crate::chrome::dialogs::{ChromeDialog, OPEN_CANCEL_KEY, OPEN_DISCARD_KEY, OPEN_SAVE_KEY};
use crate::fixtures::SAMPLE_ECAD;
use crate::open_dialog::{OpenMsg, WakeFn};
use crate::recents::RecentFiles;
use std::sync::Arc;

#[test]
fn ctrl_o_uses_injected_mailbox_and_fresh_open_resets_workspace() {
    let scratch = Scratch::new("open-dialog");
    let target = scratch.0.join("picked.eut");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");
    let picked = target.clone();
    let launcher = Arc::new(
        move |reply: std::sync::mpsc::Sender<OpenMsg>, wake: WakeFn| {
            reply
                .send(OpenMsg::Picked(Some(picked.clone())))
                .expect("inject pick");
            wake();
        },
    );
    let (watch_tx, watch_rx) = std::sync::mpsc::channel();
    let mut app = EutecticApp::new(edit_board_domain()).with_open_services(
        launcher,
        Arc::new(|| {}),
        watch_tx,
    );
    app.set_layout(PaneLayout::Stacked);
    app.set_maximized(Some(PaneId::B));
    app.pane_cams.borrow_mut()[0].glide = crate::render::CameraGlide::new(
        crate::render::Camera::new((9_000_000.0, 8_000_000.0), 2e-5),
    );

    app.on_event(hotkey(OPEN_KEY), &EventCx::new());
    assert_ne!(app.domain.source_path.as_deref(), Some(target.as_path()));
    app.before_build();

    assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
    assert_eq!(app.layout.get(), PaneLayout::Dual);
    assert_eq!(app.maximized.get(), None);
    assert_eq!(
        app.pane_camera_target(PaneId::A),
        crate::render::Camera::new((0.0, 0.0), crate::app::canvas_pane::RESET_ZOOM)
    );
    assert_eq!(watch_rx.try_recv().expect("watcher switched"), target);
    assert_eq!(app.recent_paths(), vec![target]);
}

#[test]
fn dirty_open_confirm_save_discard_cancel_and_failed_save_abort() {
    let scratch = Scratch::new("open-confirm");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");

    let mut cancel = edit_app();
    let current = scratch.0.join("current.eut");
    std::fs::write(&current, &cancel.domain.source).expect("write current");
    cancel.domain.source_path = Some(current);
    commit_move(&mut cancel, 1, 0);
    let mut recents = RecentFiles::new();
    recents.push(target.clone());
    let mut cancel = cancel.with_recents(recents.clone(), None);
    cancel.request_recent(0);
    assert_eq!(cancel.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    cancel.on_event(click(OPEN_CANCEL_KEY), &EventCx::new());
    assert!(cancel.dirty());
    assert_ne!(cancel.domain.source_path.as_deref(), Some(target.as_path()));

    let mut discard = edit_app();
    commit_move(&mut discard, 1, 0);
    let mut discard = discard.with_recents(recents.clone(), None);
    discard.request_recent(0);
    discard.on_event(click(OPEN_DISCARD_KEY), &EventCx::new());
    assert_eq!(
        discard.domain.source_path.as_deref(),
        Some(target.as_path())
    );
    assert!(!discard.dirty());

    let mut save = edit_app();
    let save_path = scratch.0.join("save-before-open.eut");
    std::fs::write(&save_path, &save.domain.source).expect("write save target");
    save.domain.source_path = Some(save_path.clone());
    commit_move(&mut save, 2, 0);
    let mut save = save.with_recents(recents.clone(), None);
    save.request_recent(0);
    save.on_event(click(OPEN_SAVE_KEY), &EventCx::new());
    assert_eq!(save.domain.source_path.as_deref(), Some(target.as_path()));
    assert!(
        std::fs::read_to_string(save_path)
            .expect("saved current")
            .contains("pin C1")
    );

    let mut failed = edit_app();
    failed.domain.source_path = Some(scratch.0.join("missing/board.eut"));
    commit_move(&mut failed, 1, 0);
    let mut failed = failed.with_recents(recents, None);
    let prior_source = failed.domain.source.clone();
    failed.request_recent(0);
    failed.on_event(click(OPEN_SAVE_KEY), &EventCx::new());
    assert!(failed.dirty(), "failed save keeps current document dirty");
    assert_eq!(failed.domain.source, prior_source, "open was aborted");
    assert!(failed.domain.edit.error.is_some(), "save error surfaced");
}

#[test]
fn dirty_dialog_discard_approval_applies_to_the_picked_path_once() {
    let scratch = Scratch::new("open-dialog-discard");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");
    let picked = target.clone();
    let launcher = Arc::new(
        move |reply: std::sync::mpsc::Sender<OpenMsg>, wake: WakeFn| {
            reply
                .send(OpenMsg::Picked(Some(picked.clone())))
                .expect("inject pick");
            wake();
        },
    );
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut app = edit_app().with_open_services(launcher, Arc::new(|| {}), watch_tx);
    commit_move(&mut app, 1, 0);

    app.on_event(hotkey(OPEN_KEY), &EventCx::new());
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    app.on_event(click(OPEN_DISCARD_KEY), &EventCx::new());
    app.before_build();

    assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
    assert_eq!(app.chrome_dialog.get(), None, "no second dirty prompt");
    assert!(!app.dirty());
}

#[test]
fn vanished_recent_surfaces_load_error_without_pruning_mru() {
    let missing = std::env::temp_dir().join(format!(
        "eutectic-vanished-recent-{}-{}.eut",
        std::process::id(),
        line!()
    ));
    let mut recents = RecentFiles::new();
    recents.push(missing.clone());
    let mut app = edit_app().with_recents(recents, None);

    app.request_recent(0);

    assert!(app.domain.doc.is_err());
    assert!(app.domain.doc.as_ref().unwrap_err().contains("reading"));
    assert_eq!(app.recent_paths(), vec![missing]);
}
