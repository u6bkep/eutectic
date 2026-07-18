//! Live-source reload tests (m5): revision bumps, camera / state preservation,
//! the permissive bad-source path, and selection pruning. Moved verbatim from
//! `app.rs` (gui-module-split).

use super::*;

// -----------------------------------------------------------------------
// Milestone-5: live source loop (reload) + findings interaction tests.
// All headless: inject SourceMsg onto the mailbox, run before_build.
// -----------------------------------------------------------------------

/// Good → good reload: the doc revision bumps EXACTLY once, and the preserved
/// state (layer visibility, pane layout, a still-resolving selection) survives.
#[test]
fn reload_good_to_good_bumps_revision_once_and_preserves_state() {
    let mut app = board();
    // Preserve targets: hide a layer, flip the layout, select the routed trace.
    app.hidden.borrow_mut().insert("layer:F.Cu".to_string());
    app.layout.set(PaneLayout::Stacked);
    let tid = app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .traces
        .keys()
        .next()
        .copied()
        .unwrap();
    // Trace ids are command-authored (not in source), so a source-only reload drops
    // them; select a NET instead, which survives a same-source reload.
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(eutectic_core::id::NetId::new("GND")));
    let _ = tid;
    let rev0 = app.revision();

    // Reload with the SAME source (a good doc). The board fixture's source has no
    // routed copper (that was command-authored), so GND is still a net in the doc.
    let src = app.domain.source.clone();
    app.mailbox_push(SourceMsg::pathless(src));
    app.before_build();

    assert_eq!(app.revision(), rev0 + 1, "one good reload bumps once");
    assert!(
        app.reload_error().is_none(),
        "a good reload clears any error"
    );
    assert!(
        app.hidden.borrow().contains("layer:F.Cu"),
        "layer visibility must be preserved across reload"
    );
    assert_eq!(
        app.layout.get(),
        PaneLayout::Stacked,
        "pane layout must be preserved across reload"
    );
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(eutectic_core::id::NetId::new("GND"))),
        "a still-resolving selection must survive reload"
    );

    // A second identical reload bumps again (each applied Changed is one revision).
    let src = app.domain.source.clone();
    app.mailbox_push(SourceMsg::pathless(src));
    app.before_build();
    assert_eq!(app.revision(), rev0 + 2);
}

/// Reload preserves cameras: the board pane's camera is app-owned (WP2) and
/// `apply_reload` leaves both the camera and the `fitted` flag alone, so a
/// reload leaves the framing exactly as the user set it — even a framing the
/// user changed away from the fit.
#[test]
fn reload_preserves_camera_no_refit() {
    let mut app = board();
    // Settle: the board pane fits (camera snapped, `fitted` set).
    let _ = settle(&mut app);
    assert!(app.panes.borrow()[pane_index(PaneId::A)].fitted);
    // The user reframes: nudge the camera off the fit.
    let fit = app.pane_camera(PaneId::A);
    let framed = crate::render::Camera::new(
        (fit.center.0 + 3.0 * NM_PER_MM as f64, fit.center.1),
        fit.zoom * 2.0,
    );
    app.pane_cams.borrow_mut()[pane_index(PaneId::A)]
        .glide
        .snap(framed);

    // Reload with identical good source, then run the frame loop again.
    let src = app.domain.source.clone();
    app.mailbox_push(SourceMsg::pathless(src));
    let _ = settle(&mut app);
    assert_eq!(
        app.pane_camera(PaneId::A),
        framed,
        "a reload must NOT re-fit — the user's framing is sacred"
    );
    assert!(app.panes.borrow()[pane_index(PaneId::A)].fitted);
}

/// Good → bad reload: the last-good doc STAYS rendered (canvas does not blank), the
/// revision does NOT bump, and a persistent reload error is recorded. We choose to
/// RETAIN the last-good findings (they still describe what is on screen) — see the
/// reload_semantics report note.
#[test]
fn reload_good_to_bad_keeps_last_good_and_sets_error() {
    let mut app = board();
    let rev0 = app.revision();
    let good_findings = app.findings();
    assert!(app.has_board(), "board projects before the bad reload");

    // A source that fails elaboration (unknown part).
    app.mailbox_push(SourceMsg::pathless(BROKEN_SRC));
    app.before_build();

    assert_eq!(
        app.revision(),
        rev0,
        "a failed reload must NOT bump the revision"
    );
    assert!(
        app.reload_error().is_some(),
        "a failed reload must record a persistent error"
    );
    assert!(
        app.has_board(),
        "the last-good board must stay rendered (canvas never blanks)"
    );
    assert_eq!(
        app.findings(),
        good_findings,
        "last-good findings are RETAINED across a failed reload"
    );
    assert!(
        app.domain.doc.is_ok(),
        "the last-good doc is still the rendered doc"
    );
}

/// Bad → good recovery: after a failed reload, a subsequent good reload swaps in the
/// new doc, bumps the revision, and CLEARS the error.
#[test]
fn reload_bad_then_good_recovers() {
    let mut app = board();
    app.mailbox_push(SourceMsg::pathless(BROKEN_SRC));
    app.before_build();
    assert!(app.reload_error().is_some());
    let rev_after_bad = app.revision();

    // Now a good source (the schematic doc) — recovers.
    app.mailbox_push(SourceMsg::pathless(SCHEMATIC_ECAD));
    app.before_build();
    assert!(
        app.reload_error().is_none(),
        "a good reload clears the error"
    );
    assert_eq!(
        app.revision(),
        rev_after_bad + 1,
        "recovery bumps the revision"
    );
    assert!(
        app.has_schematic(),
        "the new doc's schematic projects after recovery"
    );
}

/// Selection pruning: select an entity, reload with a source that REMOVES it →
/// the selection drops the now-dangling id without panicking.
#[test]
fn reload_prunes_dangling_selection() {
    // Start from the schematic doc (has parts U1/C1/C2 + nets VDD/GND).
    let mut app = EutecticApp::new(schematic_domain());
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Part(eutectic_core::id::EntityId::new("U1")));
    assert!(!app.domain.selection.borrow().is_empty());

    // Reload with a source that has NO U1 (only C1) — U1 no longer resolves.
    let pruned_src = "\
inst C1 Cap
net SOLO C1.p1
nc C1.p2
board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
";
    app.mailbox_push(SourceMsg::pathless(pruned_src));
    app.before_build(); // must not panic

    assert!(
        app.domain.selection.borrow().is_empty(),
        "the removed entity must be pruned from the selection"
    );
    assert!(app.reload_error().is_none(), "the pruning reload was good");
}

/// A selection that STILL resolves survives the prune (the complement of the above).
#[test]
fn reload_keeps_resolving_selection() {
    let mut app = EutecticApp::new(schematic_domain());
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(eutectic_core::id::NetId::new("VDD")));
    // Reload with the SAME source: VDD still resolves.
    app.mailbox_push(SourceMsg::pathless(SCHEMATIC_ECAD));
    app.before_build();
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(eutectic_core::id::NetId::new("VDD"))),
        "a still-resolving net selection survives the prune"
    );
}
