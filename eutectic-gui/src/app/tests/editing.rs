//! Editing-foundation tests (m6 slice A): command commits, the save model
//! (dirty / echo / conflict), undo/redo snapshots, drag placement, and the
//! editing hotkeys. Moved verbatim from `app.rs` (gui-module-split).

use super::*;

// -----------------------------------------------------------------------
// Milestone-6 slice A: the editing foundation. Command commits, the save
// model (dirty / explicit save / echo suppression / conflict banner),
// undo/redo via source snapshots, and drag placement end to end through
// synthesized pointer events. All headless.
// -----------------------------------------------------------------------

/// Commit → serialize fixpoint: after a GUI commit the domain source IS the
/// canonical projection (`serialize(doc)`), re-elaborating it reproduces the
/// doc, and re-serializing is byte-identical. The commit dirtied the doc,
/// bumped the revision, and stacked one undo snapshot.
#[test]
fn commit_serialize_fixpoint_and_bookkeeping() {
    let mut app = edit_app();
    let rev0 = app.revision();
    assert!(!app.dirty());
    commit_move(&mut app, 3, 1);

    assert!(app.dirty(), "a commit dirties the doc");
    assert_eq!(app.revision(), rev0 + 1, "a commit bumps the revision");
    assert_eq!(app.undo_depths(), (1, 0));

    let s = app.domain.source.clone();
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(
        eutectic_core::text::serialize(doc),
        s,
        "domain source is the canonical projection after a commit"
    );
    assert!(
        s.contains("pin C1"),
        "the move serialized as a pin override"
    );

    // serialize → parse/elaborate → serialize is a fixpoint.
    let d2 =
        DomainState::from_source_with(s.clone(), None, eutectic_core::part::part_library(), |_| {
            Vec::new()
        });
    let doc2 = d2.doc.as_ref().expect("canonical text elaborates");
    assert_eq!(eutectic_core::text::serialize(doc2), s, "serialize fixpoint");
    assert_eq!(
        doc2.components[&EntityId::new("C1")].pos.value,
        doc.components[&EntityId::new("C1")].pos.value,
        "the pinned position survives the round-trip"
    );
}

/// Undo/redo round-trip with dirty-flag correctness (no save in between):
/// undoing the only edit returns to the loaded state, which equals the
/// load-time saved baseline → clean; redo re-applies and re-dirties.
#[test]
fn undo_redo_roundtrip_dirty_flags() {
    let mut app = edit_app();
    let comp = EntityId::new("C1");
    let pos0 = comp_pos(&app, &comp);
    let rev0 = app.revision();

    commit_move(&mut app, 3, 1);
    let pos1 = comp_pos(&app, &comp);
    assert_ne!(pos0, pos1, "the pin moved the component");
    assert!(app.dirty());

    app.undo();
    assert_eq!(comp_pos(&app, &comp), pos0, "undo restores the position");
    assert!(
        !app.dirty(),
        "undo back to the loaded state clears dirty (snapshot == saved baseline)"
    );
    assert_eq!(app.undo_depths(), (0, 1));
    assert_eq!(app.revision(), rev0 + 2, "undo re-elaborates (a revision)");

    app.redo();
    assert_eq!(comp_pos(&app, &comp), pos1, "redo re-applies the move");
    assert!(app.dirty(), "redo away from the saved state re-dirties");
    assert_eq!(app.undo_depths(), (1, 0));

    // A new commit clears the redo stack.
    app.undo();
    assert_eq!(app.undo_depths(), (0, 1));
    commit_move(&mut app, 1, 0);
    assert_eq!(app.undo_depths(), (1, 0), "a fresh commit clears redo");
}

/// The classic dirty trap, with a save in the middle: edit A, save, edit B —
/// then undo lands on the SAVED state (clean), undo again on the base
/// (dirty), and redo forward re-crosses the same boundary.
#[test]
fn undo_after_save_dirty_string_compare() {
    let scratch = Scratch::new("undo-save");
    let file = scratch.0.join("board.eut");
    let mut app = edit_app();
    app.domain.source_path = Some(file.clone());

    commit_move(&mut app, 3, 0); // state A
    app.save();
    assert!(!app.dirty(), "save clears dirty");
    let saved = std::fs::read_to_string(&file).expect("save wrote the file");
    assert_eq!(
        saved, app.domain.source,
        "save wrote the canonical projection"
    );

    commit_move(&mut app, 0, 2); // state B
    assert!(app.dirty());

    app.undo(); // back to A == last-saved content
    assert!(
        !app.dirty(),
        "undo onto the exactly-saved state must clear dirty (string compare)"
    );
    app.undo(); // base ≠ saved A
    assert!(app.dirty(), "undo past the saved state re-dirties");
    app.redo(); // A again
    assert!(!app.dirty(), "redo onto the saved state clears dirty again");
    app.redo(); // B
    assert!(app.dirty());
}

/// Save-echo suppression: after a save, the watcher's delivery of our own
/// write is consumed silently — no reload, no revision bump, no conflict.
#[test]
fn save_echo_is_suppressed() {
    let scratch = Scratch::new("echo");
    let file = scratch.0.join("board.eut");
    let mut app = edit_app();
    app.domain.source_path = Some(file.clone());
    commit_move(&mut app, 2, 0);
    app.save();
    let rev = app.revision();

    // The watcher sees the mtime change and delivers our own write back.
    let echoed = std::fs::read_to_string(&file).unwrap();
    app.mailbox_push(SourceMsg::Changed(echoed));
    app.before_build();

    assert_eq!(app.revision(), rev, "an echo must not reload");
    assert!(app.conflict().is_none(), "an echo is not a conflict");
    assert!(!app.dirty());

    // The echo token is ONE-SHOT: after the echo is consumed, a later
    // byte-identical delivery is a genuine external write. While dirty it
    // must raise the conflict banner, not be silently swallowed.
    let echoed = std::fs::read_to_string(&file).unwrap();
    commit_move(&mut app, 3, 0);
    assert!(app.dirty());
    app.mailbox_push(SourceMsg::Changed(echoed));
    app.before_build();
    assert!(
        app.conflict().is_some(),
        "an identical external write after the echo was consumed is a real conflict"
    );
}

/// The conflict flow, Reload branch: a disk change while dirty parks as the
/// pending conflict (nothing applied); the explicit Reload action applies the
/// disk text, clears dirty, and empties the undo stack.
#[test]
fn conflict_reload_discards_edits_and_follows_disk() {
    let mut app = edit_app();
    commit_move(&mut app, 4, 0);
    let rev_dirty = app.revision();

    app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
    app.before_build();
    assert_eq!(
        app.conflict().as_deref(),
        Some(SCHEMATIC_ECAD),
        "the external change is parked, not applied"
    );
    assert_eq!(app.revision(), rev_dirty, "no silent reload while dirty");
    assert!(app.dirty(), "the doc stays dirty under the banner");

    let cx = EventCx::new();
    app.on_event(click(CONFLICT_RELOAD_KEY), &cx);
    assert!(app.conflict().is_none(), "reload consumes the conflict");
    assert!(!app.dirty(), "the doc now mirrors disk");
    assert_eq!(
        app.undo_depths(),
        (0, 0),
        "external reload clears undo/redo"
    );
    assert!(
        app.has_schematic(),
        "the disk text (schematic doc) was applied"
    );
}

/// The conflict flow, Keep-mine branch: the banner dismisses, the doc stays
/// dirty at its revision, and the next save overwrites the disk.
#[test]
fn conflict_keep_mine_stays_dirty_and_save_overwrites() {
    let scratch = Scratch::new("keep-mine");
    let file = scratch.0.join("board.eut");
    let mut app = edit_app();
    app.domain.source_path = Some(file.clone());
    commit_move(&mut app, 4, 0);
    let my_source = app.domain.source.clone();

    // Someone writes an external version to disk; the watcher delivers it.
    std::fs::write(&file, SCHEMATIC_ECAD).unwrap();
    app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
    app.before_build();
    assert!(app.conflict().is_some());

    let cx = EventCx::new();
    app.on_event(click(CONFLICT_KEEP_KEY), &cx);
    assert!(app.conflict().is_none(), "keep-mine dismisses the banner");
    assert!(app.dirty(), "the doc stays dirty");
    assert_eq!(app.domain.source, my_source, "my edits survive");

    app.save();
    assert!(!app.dirty());
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        my_source,
        "the next save overwrites the disk (explicit last-writer)"
    );
}

/// A newer external delivery replaces the pending conflict text, and a CLEAN
/// doc still follows disk automatically (the m5 behavior is unchanged).
#[test]
fn conflict_updates_and_clean_doc_still_follows() {
    let mut app = edit_app();
    commit_move(&mut app, 1, 0);
    app.mailbox_push(SourceMsg::Changed("v1".to_string()));
    app.before_build();
    app.mailbox_push(SourceMsg::Changed("v2".to_string()));
    app.before_build();
    assert_eq!(
        app.conflict().as_deref(),
        Some("v2"),
        "the newest external text wins the pending slot"
    );
    // Resolve, then verify the clean path still auto-applies.
    let cx = EventCx::new();
    app.on_event(click(CONFLICT_KEEP_KEY), &cx);
    let mut clean = edit_app();
    let rev0 = clean.revision();
    clean.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
    clean.before_build();
    assert_eq!(clean.revision(), rev0 + 1, "a clean doc follows disk");
    assert!(!clean.dirty());
}

/// Saving while the conflict banner is up is the keep-mine resolution made
/// permanent: the disk is explicitly overwritten and the banner dismisses.
#[test]
fn save_while_conflicted_overwrites_and_dismisses() {
    let scratch = Scratch::new("save-conflict");
    let file = scratch.0.join("board.eut");
    let mut app = edit_app();
    app.domain.source_path = Some(file.clone());
    commit_move(&mut app, 3, 0);
    app.mailbox_push(SourceMsg::Changed(SCHEMATIC_ECAD.to_string()));
    app.before_build();
    assert!(app.conflict().is_some());

    app.save();
    assert!(!app.dirty());
    assert!(
        app.conflict().is_none(),
        "an explicit save resolves the conflict (last-writer, chosen)"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        app.domain.source,
        "the save overwrote the disk with my edits"
    );
}

/// No-path docs have no save: `save()` is a no-op (stays dirty, no error).
#[test]
fn save_without_path_is_a_noop() {
    let mut app = edit_app();
    commit_move(&mut app, 1, 1);
    assert!(app.domain.source_path.is_none());
    app.save();
    assert!(app.dirty(), "no path → nothing saved → still dirty");
    assert!(app.domain.edit.error.is_none());
}

/// Drag placement end to end through synthesized pointer events: pointer-down
/// on a C1 pad arms the drag, drag moves the ghost, pointer-up commits a
/// `Command::Pin` at exactly `orig_pos + (drop − grab)` (hard placement), the
/// component's provenance is Pinned, the doc is dirty, and the moved part is
/// selected. The trailing Click is suppressed (no re-select of the drop pad).
#[test]
fn drag_commits_pin_at_exact_delta() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let comp = EntityId::new("C1");
    let orig = comp_pos(&app, &comp);
    let grab = pad_center_of(&app, &comp);

    let grab_px = px_of_board(&app, &r, grab);
    let drop_board = Point {
        x: grab.x + 4 * NM_PER_MM,
        y: grab.y + 3 * NM_PER_MM,
    };
    let drop_px = px_of_board(&app, &r, drop_board);
    // The exact board points the handler derives from those pixels (f32
    // round-trip included), so the expected delta is bit-exact.
    let p_grab = board_of_px(&app, &r, grab_px);
    let p_drop = board_of_px(&app, &r, drop_px);
    let expected = Point {
        x: orig.x + (p_drop.x - p_grab.x),
        y: orig.y + (p_drop.y - p_grab.y),
    };

    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
    assert!(app.drag_active(), "pointer-down on a pad arms the drag");
    assert!(!app.dirty(), "arming commits nothing");

    app.on_event(pointer(UiEventKind::Drag, drop_px), &cx);
    {
        let drag = app.drag.borrow();
        let d = drag.as_ref().unwrap();
        assert!(d.moved, "a 4×3 mm drag is way past the slop");
        assert!(!d.ghost_shapes().is_empty(), "the ghost has pad shapes");
        assert!(
            !d.ratsnest().is_empty(),
            "netted pads produce ratsnest lines"
        );
    }
    assert!(!app.dirty(), "still nothing committed during the drag");

    let rev0 = app.revision();
    app.on_event(pointer(UiEventKind::PointerUp, drop_px), &cx);
    assert!(!app.drag_active(), "pointer-up finishes the drag");
    assert!(app.dirty(), "the move committed");
    assert_eq!(app.revision(), rev0 + 1);

    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(
        doc.components[&comp].pos.value, expected,
        "a Pin is a fixed solver anchor — the part lands exactly at orig + delta"
    );
    assert_eq!(
        doc.components[&comp].pos.prov,
        eutectic_core::doc::Provenance::Pinned,
        "the drag is a hard placement (Pin), per 'user dragged it exactly here'"
    );
    let ov = doc.overrides.get(&comp).expect("a pin override recorded");
    assert_eq!(ov.pos, Some(expected));
    assert_eq!(ov.strength, eutectic_core::doc::Strength::Pin);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Part(comp.clone())),
        "the moved part stays selected"
    );

    // The trailing Click of the same press is eaten exactly once.
    app.on_event(pointer(UiEventKind::Click, drop_px), &cx);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Part(comp)),
        "the drag's trailing Click must not re-select the drop pad"
    );
}

/// Esc during a drag cancels: preview discarded, nothing committed, doc
/// untouched and clean.
#[test]
fn escape_cancels_drag_without_commit() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let comp = EntityId::new("C1");
    let pos0 = comp_pos(&app, &comp);
    let grab_px = px_of_board(&app, &r, pad_center_of(&app, &comp));
    let away_px = (grab_px.0 + 60.0, grab_px.1 + 40.0);

    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
    app.on_event(pointer(UiEventKind::Drag, away_px), &cx);
    assert!(app.drag_active());

    app.on_event(escape(), &cx);
    assert!(!app.drag_active(), "Esc cancels the drag");
    assert!(!app.dirty(), "nothing committed");
    assert_eq!(comp_pos(&app, &comp), pos0, "the doc is untouched");

    // A later pointer-up is inert (no stale drag).
    app.on_event(pointer(UiEventKind::PointerUp, away_px), &cx);
    assert!(!app.dirty());
}

/// Click-without-drag stays a plain select: down + up on the same pad within
/// the slop commits nothing, and the Click selects the pad as before.
#[test]
fn click_without_drag_is_plain_select() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let comp = EntityId::new("C1");
    let grab = pad_center_of(&app, &comp);
    let grab_px = px_of_board(&app, &r, grab);

    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(pointer(UiEventKind::PointerDown, grab_px), &cx);
    app.on_event(pointer(UiEventKind::PointerUp, grab_px), &cx);
    assert!(!app.drag_active());
    assert!(!app.dirty(), "an un-moved press commits nothing");

    app.on_event(pointer(UiEventKind::Click, grab_px), &cx);
    match app.domain.selection.borrow().single() {
        Some(SemanticId::Pin { comp: c, .. }) => assert_eq!(c, &comp),
        other => panic!("a plain click selects the pad, got {other:?}"),
    }
}

/// Pointer-down on empty board / non-component copper arms no drag.
#[test]
fn pointer_down_on_empty_board_arms_nothing() {
    let mut app = edit_app();
    let r = settle(&mut app);
    // (10, 13) mm: inside the board and the pour, away from both caps' pads —
    // resolves to the POUR, which is not a component.
    let px = px_of_board(
        &app,
        &r,
        Point {
            x: 10 * NM_PER_MM,
            y: 13 * NM_PER_MM,
        },
    );
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(pointer(UiEventKind::PointerDown, px), &cx);
    assert!(!app.drag_active(), "only components are draggable");
}

/// The editing hotkeys drive the same actions as the toolbar buttons: Ctrl+Z
/// undoes the drag commit, Ctrl+Shift+Z redoes it, Ctrl+S saves.
#[test]
fn hotkeys_drive_undo_redo_save() {
    let scratch = Scratch::new("hotkeys");
    let file = scratch.0.join("board.eut");
    let mut app = edit_app();
    app.domain.source_path = Some(file.clone());
    let comp = EntityId::new("C1");
    let pos0 = comp_pos(&app, &comp);
    commit_move(&mut app, 5, 0);
    let pos1 = comp_pos(&app, &comp);

    let cx = EventCx::new();
    app.on_event(hotkey(UNDO_KEY), &cx);
    assert_eq!(comp_pos(&app, &comp), pos0, "Ctrl+Z undoes");
    app.on_event(hotkey(REDO_KEY), &cx);
    assert_eq!(comp_pos(&app, &comp), pos1, "Ctrl+Shift+Z redoes");
    app.on_event(hotkey(SAVE_KEY), &cx);
    assert!(!app.dirty(), "Ctrl+S saves");
    assert!(file.exists());

    // The registered chord table carries all three actions.
    let chords = app.hotkeys();
    for action in [SAVE_KEY, UNDO_KEY, REDO_KEY] {
        assert!(
            chords.iter().any(|(_, a)| a == action),
            "{action} is registered as a hotkey"
        );
    }
}
