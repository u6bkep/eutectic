use super::*;
use crate::app::open::OPEN_KEY;
use crate::chrome::dialogs::{ChromeDialog, OPEN_CANCEL_KEY, OPEN_DISCARD_KEY, OPEN_SAVE_KEY};
use crate::fixtures::SAMPLE_ECAD;
use crate::open_dialog::{OpenMsg, WakeFn};
use crate::recents::RecentFiles;
use std::sync::{Arc, Mutex};

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
    app.split_weights.set([3.0, 1.0]);
    app.split_drag.borrow_mut().anchor = Some(42.0);
    app.split_drag.borrow_mut().initial = [3.0, 1.0];
    app.split_extent.set(900.0);
    app.focused_pane.set(PaneId::B);
    app.pane_cams.borrow_mut()[0].glide = crate::render::CameraGlide::new(
        crate::render::Camera::new((9_000_000.0, 8_000_000.0), 2e-5),
    );
    app.pane_cams.borrow_mut()[0].request = Some(crate::app::canvas_pane::CamRequest::Fit);
    app.pane_cams.borrow_mut()[1].glide = crate::render::CameraGlide::new(
        crate::render::Camera::new((7_000_000.0, 6_000_000.0), 3e-5),
    );
    app.hidden.borrow_mut().insert("layer:F.Cu".to_string());
    app.set_tool(ViewKind::Board, Tool::Route);
    let mut measure = crate::tool::MeasureState::default();
    measure.click(Point::mm(1, 2));
    app.measure.set(measure);
    app.measure_pane.set(PaneId::B);
    app.set_active_layer("B.Cu");
    app.cursor_board_mm.set(Some((1.0, 2.0)));
    app.cursor_px.set([Some((3.0, 4.0)), Some((5.0, 6.0))]);
    {
        let mut raw = app.raw.borrow_mut();
        raw.cursor = Some((7.0, 8.0));
        raw.primary_down = true;
        raw.hover_ours = true;
    }
    *app.explorer_filter.borrow_mut() = "C1".to_string();
    *app.explorer_filter_selection.borrow_mut() = Selection::caret("explorer:filter", 2);
    {
        let mut inspector = app.inspector_ui.borrow_mut();
        inspector
            .raw
            .insert(crate::panels::properties::POSITION_X_KEY, "12.".to_string());
        inspector.active = Some(crate::panels::properties::POSITION_X_KEY);
        inspector.subject = Some(SemanticId::Part(EntityId::new("C1")));
    }
    app.on_event(hotkey(OPEN_KEY), &EventCx::new());
    assert_ne!(app.domain.source_path.as_deref(), Some(target.as_path()));
    // The native picker is now in flight. Mutations made while it is up must also be
    // cleared when the queued result lands.
    app.set_palette_open(true);
    {
        let mut palette = app.palette_ui.borrow_mut();
        palette.query = "gnd".to_string();
        palette.highlighted = 3;
    }
    app.recent_open.set(true);
    app.before_build();

    assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
    assert_eq!(app.layout.get(), PaneLayout::Dual);
    assert_eq!(app.maximized.get(), None);
    assert_eq!(app.split_weights.get(), [1.0, 1.0]);
    assert_eq!(app.split_drag.borrow().anchor, None);
    assert_eq!(app.split_drag.borrow().initial, [0.0, 0.0]);
    assert_eq!(app.split_extent.get(), 0.0);
    assert_eq!(app.focused_pane.get(), PaneId::A);
    assert_eq!(
        app.panes
            .borrow()
            .iter()
            .map(|pane| (pane.view, pane.fitted))
            .collect::<Vec<_>>(),
        [(ViewKind::Board, false), (ViewKind::Schematic, false)]
    );
    assert_eq!(
        app.pane_camera_target(PaneId::A),
        crate::render::Camera::new((0.0, 0.0), crate::app::canvas_pane::RESET_ZOOM)
    );
    assert_eq!(
        app.pane_camera_target(PaneId::B),
        crate::render::Camera::new((0.0, 0.0), crate::app::canvas_pane::RESET_ZOOM)
    );
    assert!(
        app.pane_cams
            .borrow()
            .iter()
            .all(|camera| camera.request.is_none())
    );
    assert_eq!(app.pane_px.get(), [None, None]);
    assert_eq!(app.strip_px.get(), [None, None]);
    assert!(app.hidden.borrow().is_empty());
    assert!(app.tools.borrow().is_empty());
    assert_eq!(app.measure.get(), Default::default());
    assert_eq!(app.measure_pane.get(), PaneId::A);
    assert!(app.drag.borrow().is_none());
    assert!(!app.suppress_click.get());
    assert!(app.route.borrow().is_none());
    assert!(app.trace_drag.borrow().is_none());
    assert!(app.camera_pan.borrow().is_none());
    assert!(app.active_layer.borrow().is_none());
    assert_eq!(app.cursor_board_mm.get(), None);
    assert_eq!(app.cursor_px.get(), [None, None]);
    assert!(app.open_menu.borrow().is_none());
    assert!(!app.recent_open.get());
    assert!(app.explorer_filter.borrow().is_empty());
    assert_eq!(
        *app.explorer_filter_selection.borrow(),
        Selection::default()
    );
    assert!(app.inspector_ui.borrow().raw.is_empty());
    assert_eq!(app.inspector_ui.borrow().active, None);
    assert_eq!(app.inspector_ui.borrow().subject, None);
    assert!(!app.palette_open.get());
    assert!(app.palette_ui.borrow().query.is_empty());
    assert_eq!(app.palette_ui.borrow().selection, Selection::default());
    assert_eq!(app.palette_ui.borrow().highlighted, 0);
    assert!(app.focus_requests.borrow().is_empty());
    {
        let raw = app.raw.borrow();
        assert_eq!(raw.cursor, None);
        assert!(!raw.primary_down);
        assert!(raw.middle_pan.is_none());
        assert!(!raw.hover_ours);
    }
    assert_eq!(watch_rx.try_recv().expect("watcher switched"), target);
    assert_eq!(app.recent_paths(), vec![target]);
}

#[test]
fn stale_reload_from_previous_path_is_ignored_after_open() {
    let scratch = Scratch::new("stale-reload");
    let old = scratch.0.join("old.eut");
    let target = scratch.0.join("target.eut");
    std::fs::write(&old, &edit_app().domain.source).expect("write old");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");
    let mut recents = RecentFiles::new();
    recents.push(target.clone());
    let mut app = edit_app().with_recents(recents, None);
    app.domain.source_path = Some(old.clone());
    app.request_recent(0);
    let source = app.domain.source.clone();
    let dirty = app.dirty();
    let revision = app.revision();

    app.mailbox_push(SourceMsg::Changed {
        path: Some(old),
        source: "inst OLD Cap\nnet OLD OLD.p1\nnc OLD.p2\n".to_string(),
    });
    app.before_build();

    assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
    assert_eq!(app.domain.source, source);
    assert_eq!(app.dirty(), dirty);
    assert_eq!(app.revision(), revision);
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
fn discard_approval_reprompts_if_document_changes_while_picker_is_open() {
    let scratch = Scratch::new("open-approval-revision");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");
    let sender = Arc::new(Mutex::new(None));
    let sender_slot = sender.clone();
    let launcher = Arc::new(move |reply, _wake: WakeFn| {
        *sender_slot.lock().expect("sender slot") = Some(reply);
    });
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut app = edit_app().with_open_services(launcher, Arc::new(|| {}), watch_tx);
    commit_move(&mut app, 1, 0);
    app.request_open_dialog();
    app.confirm_open_discard();
    commit_move(&mut app, 1, 0);
    sender
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send(OpenMsg::Picked(Some(target.clone())))
        .unwrap();

    app.before_build();

    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    assert_ne!(app.domain.source_path.as_deref(), Some(target.as_path()));
    assert!(app.dirty());
}

#[test]
fn discard_approval_is_consumed_by_first_pick() {
    let scratch = Scratch::new("open-approval-once");
    let first = scratch.0.join("first.eut");
    let second = scratch.0.join("second.eut");
    std::fs::write(&first, SAMPLE_ECAD).expect("write first");
    std::fs::write(&second, SAMPLE_ECAD).expect("write second");
    let sender = Arc::new(Mutex::new(None));
    let sender_slot = sender.clone();
    let launcher = Arc::new(move |reply, _wake: WakeFn| {
        *sender_slot.lock().expect("sender slot") = Some(reply);
    });
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut app = edit_app().with_open_services(launcher, Arc::new(|| {}), watch_tx);
    commit_move(&mut app, 1, 0);
    app.request_open_dialog();
    app.confirm_open_discard();
    let tx = sender.lock().unwrap().as_ref().unwrap().clone();
    tx.send(OpenMsg::Picked(Some(first.clone()))).unwrap();
    app.before_build();
    assert_eq!(app.domain.source_path.as_deref(), Some(first.as_path()));

    commit_move(&mut app, 1, 0);
    tx.send(OpenMsg::Picked(Some(second.clone()))).unwrap();
    app.before_build();

    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    assert_eq!(app.domain.source_path.as_deref(), Some(first.as_path()));
    assert!(app.dirty());
}

#[test]
fn picker_result_waits_for_unrelated_chrome_dialog() {
    let scratch = Scratch::new("open-chrome-queue");
    let target = scratch.0.join("target.eut");
    std::fs::write(&target, SAMPLE_ECAD).expect("write target");
    let sender = Arc::new(Mutex::new(None));
    let sender_slot = sender.clone();
    let launcher = Arc::new(move |reply, _wake: WakeFn| {
        *sender_slot.lock().expect("sender slot") = Some(reply);
    });
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut app = edit_app().with_open_services(launcher, Arc::new(|| {}), watch_tx);
    app.request_open_dialog();
    app.chrome_dialog.set(Some(ChromeDialog::Keymap));
    sender
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send(OpenMsg::Picked(Some(target.clone())))
        .unwrap();

    app.before_build();
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::Keymap));
    assert_ne!(app.domain.source_path.as_deref(), Some(target.as_path()));

    app.chrome_dialog.set(None);
    app.before_build();
    assert_eq!(app.domain.source_path.as_deref(), Some(target.as_path()));
}

#[test]
fn picker_result_does_not_hijack_unrelated_open_confirmation() {
    let scratch = Scratch::new("open-confirm-queue");
    let picked = scratch.0.join("picked.eut");
    let recent = scratch.0.join("recent.eut");
    std::fs::write(&picked, SAMPLE_ECAD).expect("write picked");
    std::fs::write(&recent, SAMPLE_ECAD).expect("write recent");
    let sender = Arc::new(Mutex::new(None));
    let sender_slot = sender.clone();
    let launcher = Arc::new(move |reply, _wake: WakeFn| {
        *sender_slot.lock().expect("sender slot") = Some(reply);
    });
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut recents = RecentFiles::new();
    recents.push(recent.clone());
    let mut app = edit_app()
        .with_open_services(launcher, Arc::new(|| {}), watch_tx)
        .with_recents(recents, None);
    app.request_open_dialog();
    commit_move(&mut app, 1, 0);
    app.request_recent(0);
    sender
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send(OpenMsg::Picked(Some(picked.clone())))
        .unwrap();

    app.before_build();
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    assert_eq!(
        app.pending_open.borrow().as_ref(),
        Some(&crate::app::open::PendingOpen::Path(recent))
    );

    app.cancel_pending_open();
    app.before_build();
    assert_eq!(app.chrome_dialog.get(), Some(ChromeDialog::ConfirmOpen));
    assert_eq!(
        app.pending_open.borrow().as_ref(),
        Some(&crate::app::open::PendingOpen::Path(picked))
    );
}

#[test]
fn dialog_sender_disconnect_clears_busy_for_next_open() {
    let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let launch_count = launches.clone();
    let launcher = Arc::new(move |reply, _wake: WakeFn| {
        launch_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(reply);
    });
    let (watch_tx, _watch_rx) = std::sync::mpsc::channel();
    let mut app = edit_app().with_open_services(launcher, Arc::new(|| {}), watch_tx);

    app.request_open_dialog();
    assert!(app.open_dialog_busy.get());
    app.before_build();
    assert!(!app.open_dialog_busy.get());
    app.request_open_dialog();

    assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 2);
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
