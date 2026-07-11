//! Selection-interaction tests: explorer row clicks, findings-row
//! click-to-select-and-zoom, the overlay halo, and the findings chips. Moved
//! verbatim from `app.rs` (gui-module-split).

use super::*;

/// Clicking an explorer net row selects that net (cross-highlights everywhere). Drives
/// the real `on_event` explorer path.
#[test]
fn explorer_click_selects_net() {
    let mut app = EutecticApp::new(schematic_domain());
    // The VDD net row's key, from the projection.
    let explorer = app.explorer_snapshot();
    let net_row = explorer
        .nets
        .iter()
        .find(|r| r.label == "VDD")
        .expect("VDD net row")
        .clone();
    assert!(app.domain.selection.borrow().is_empty());
    let cx = EventCx::new();
    app.on_event(click(&net_row.key), &cx);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Net(NetId::new("VDD"))),
        "explorer click must select the net"
    );
}

/// Clicking an explorer component row selects that part.
#[test]
fn explorer_click_selects_part() {
    let mut app = EutecticApp::new(schematic_domain());
    // Find the row by its semantic id (the label is the *annotated* refdes, not the
    // instance path — e.g. `U1 MCU` annotates to `MCU1`).
    let explorer = app.explorer_snapshot();
    let row = explorer
        .components
        .iter()
        .find(|r| r.id == SemanticId::Part(eutectic_core::id::EntityId::new("U1")))
        .expect("U1 component row")
        .clone();
    let cx = EventCx::new();
    app.on_event(click(&row.key), &cx);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Part(eutectic_core::id::EntityId::new("U1")))
    );
}

/// Click a findings row → the finding's refs land in the SelectionModel, and the
/// focused board pane's camera glides onto the finding's board point
/// (click-to-select-and-zoom — WP2: camera-target math, not a viewport request).
#[test]
fn click_finding_selects_refs_and_queues_center() {
    let mut app = drc_violation();
    // Find the clearance finding's index (it carries both nets NA + NB).
    let (index, refs, board_mm) = {
        let f = app.findings();
        let (i, item) = f
            .items
            .iter()
            .enumerate()
            .find(|(_, it)| it.code == "E_DRC_CLEARANCE")
            .expect("the fixture has a clearance finding");
        (i, item.refs.clone(), item.board_mm)
    };
    assert!(app.domain.selection.borrow().is_empty());

    // Settle first so a board pane is laid out + fitted, then drive the event
    // with an EventCx over that state (matching the host, which routes events
    // against the live UI).
    let r = settle(&mut app);
    let zoom_before = app.board_camera_target(PaneId::A).zoom;
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(click(&finding_row_key(index)), &cx);

    // Every ref of the finding is now selected (both nets of the clearance).
    let sel = app.domain.selection.borrow();
    for r in &refs {
        assert!(
            sel.is_selected(r),
            "clicking the finding must select its ref {r:?}"
        );
    }
    drop(sel);
    // The focused (board) pane's camera glide now targets the finding's
    // board point, at the unchanged zoom.
    let (mx, my) = board_mm.expect("clearance has a board point");
    let target = app.board_camera_target(PaneId::A);
    let mm = NM_PER_MM as f64;
    assert!(
        (target.center.0 - mx as f64 * mm).abs() < mm / 2.0
            && (target.center.1 - my as f64 * mm).abs() < mm / 2.0,
        "the camera must glide onto the finding ({mx}, {my}) mm, got {:?}",
        target.center
    );
    assert_eq!(target.zoom, zoom_before, "center-on keeps the user's zoom");
}

/// The clearance-finding halo is present in the board overlay at the right board mm:
/// building the board overlay yields a findings marker whose point matches the
/// finding's derived board_mm.
#[test]
fn finding_halo_present_in_board_overlay() {
    let app = drc_violation();
    let f = app.findings();
    let clearance = f
        .items
        .iter()
        .find(|i| i.code == "E_DRC_CLEARANCE")
        .unwrap();
    let (mx, my) = clearance.board_mm.expect("clearance has a board point");

    let derived = app.derived.borrow();
    let view = derived.board.as_ref().expect("board projects");
    let sets = HighlightSets::default();
    let overlay = app.build_board_overlay(view, PaneId::A, &sets, &derived.findings);
    assert!(
        !overlay.findings.is_empty(),
        "the overlay must carry finding markers"
    );
    // The clearance marker's point matches the finding's board_mm (nm round-trip).
    let want = eutectic_core::coord::Point {
        x: (mx * eutectic_core::coord::MM as f32).round() as eutectic_core::coord::Nm,
        y: (my * eutectic_core::coord::MM as f32).round() as eutectic_core::coord::Nm,
    };
    assert!(
        overlay
            .findings
            .iter()
            .any(|(p, is_err)| *p == want && *is_err),
        "an error marker must sit at the clearance finding's board point {want:?}"
    );
}

/// The per-source findings chips track the cached findings: a doc with
/// findings renders source chips (no ✓); a clean doc renders exactly the
/// single neutral ✓ chip (the all-clean branch of `findings_chips`).
#[test]
fn findings_chips_match_findings() {
    let app = drc_violation();
    let f = app.findings();
    assert!(
        f.errors >= 1,
        "the fixture has at least the clearance error"
    );
    assert!(!f.is_clean());
    assert!(
        !app.findings_chips().is_empty(),
        "a doc with findings renders at least one source chip"
    );

    // The clean doc from findings/tests.rs: single-pin nets, no routed copper,
    // the cap placed mid-board so its (toy) pad copper clears the board edge.
    let clean = EutecticApp::new(DomainState::from_source(
        "inst C1 Cap\nnet SOLO C1.p1\nnc C1.p2\nplace C1 (5mm, 5mm)\n\
         board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)\n"
            .to_string(),
        Some("clean.eut".to_string()),
    ));
    assert!(clean.findings().is_clean());
    let chips = clean.findings_chips();
    assert_eq!(chips.len(), 1, "all-clean is a single ✓ chip");
}
