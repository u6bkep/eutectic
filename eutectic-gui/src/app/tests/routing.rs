//! Manual trace drawing + refinement tests (m6 slice B, routing ladder level
//! 1): route start / waypoint / commit, Esc layering, via drop on layer
//! switch, and trace-vertex drags. Moved verbatim from `app.rs`
//! (gui-module-split).

use super::*;

// -----------------------------------------------------------------------
// Milestone-6 slice B: manual trace drawing (routing ladder level 1) +
// trace-vertex refinement, end to end through synthesized pointer events.
// -----------------------------------------------------------------------

/// The pad centre of a SPECIFIC pin of `comp` (the Route tool's snap point).
fn pin_center_of(app: &EutecticApp, comp: &EntityId, pin: &str) -> Point {
    let derived = app.derived.borrow();
    let view = derived.board.as_ref().expect("board projects");
    let want = SemanticId::Pin {
        comp: comp.clone(),
        pin: pin.to_string(),
    };
    let c = view
        .candidates
        .iter()
        .find(|c| c.id == want)
        .expect("pin has a pad candidate");
    Point {
        x: (c.aabb.0.x + c.aabb.1.x) / 2,
        y: (c.aabb.0.y + c.aabb.1.y) / 2,
    }
}

/// The full manual-routing contract through synthesized pointer events:
/// Route-tool click on a pin STARTS (net = the pin's net, anchor = the pad
/// centre snap), a click on non-pin board adds a WAYPOINT at the raw board
/// position, and a click on another pin COMMITS — one AddTrace through
/// commit_edit (dirty, one undo unit, the new trace selected), at the
/// engine-default width, `Pinned`.
#[test]
fn route_draw_start_waypoint_commit() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);

    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    let c2 = pin_center_of(&app, &EntityId::new("C2"), "p1");
    let wp_px = px_of_board(
        &app,
        &r,
        Point {
            x: 7 * NM_PER_MM,
            y: 7 * NM_PER_MM,
        },
    );
    // The exact board point the handler derives from the waypoint pixel
    // (f32 round-trip included).
    let wp_board = board_of_px(&app, &r, wp_px);

    // Start on C1.p1.
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c1)), &cx);
    assert!(app.route_active(), "a pin click starts the route");
    assert!(!app.dirty(), "starting commits nothing");
    {
        let pending = app.pending_route().unwrap();
        assert_eq!(pending.net.to_string(), "GND");
        assert_eq!(pending.last_point(), c1, "anchored at the pad centre");
    }

    // A waypoint on non-pin board (the GND pour is fine — permissive).
    app.on_event(pointer(UiEventKind::Click, wp_px), &cx);
    assert_eq!(app.pending_route().unwrap().last_point(), wp_board);
    assert!(!app.dirty(), "waypoints commit nothing");

    // Commit on C2.p1.
    let rev0 = app.revision();
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c2)), &cx);
    assert!(!app.route_active(), "the pin click committed the route");
    assert!(app.dirty());
    assert_eq!(app.revision(), rev0 + 1);
    assert_eq!(app.undo_depths(), (1, 0), "one commit → one undo unit");

    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.traces.len(), 1);
    let (tid, t) = doc.traces.iter().next().unwrap();
    assert_eq!(t.net.to_string(), "GND");
    assert_eq!(t.layer, "F.Cu", "default active layer = top copper");
    assert_eq!(t.path, vec![c1, wp_board, c2], "pin-snap + raw waypoint");
    let (width, ..) = crate::app::route_defaults();
    assert_eq!(t.width, width, "engine default width (0.15 mm)");
    assert_eq!(t.prov, eutectic_core::doc::Provenance::Pinned);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(*tid)),
        "the committed trace is selected, ready for refinement"
    );
    // The routes serialize into the source's `# routes` section.
    assert!(app.domain.source.contains("# routes"));

    // Undo removes the whole route.
    app.undo();
    assert!(app.domain.doc.as_ref().unwrap().traces.is_empty());
}

/// Permissiveness: committing on a pin of a DIFFERENT net commits fine —
/// the trace keeps its source net; any overlap is a findings matter, never
/// a block.
#[test]
fn route_commit_on_foreign_pin_is_permissive() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);

    let c1_gnd = pin_center_of(&app, &EntityId::new("C1"), "p1"); // net GND
    let c2_vbus = pin_center_of(&app, &EntityId::new("C2"), "p2"); // net VBUS
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &r, c1_gnd)),
        &cx,
    );
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &r, c2_vbus)),
        &cx,
    );
    assert!(!app.route_active(), "the foreign pin still commits");
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.traces.len(), 1, "naive source→dest line committed");
    let t = doc.traces.values().next().unwrap();
    assert_eq!(t.net.to_string(), "GND", "the trace keeps its SOURCE net");
    assert_eq!(t.path, vec![c1_gnd, c2_vbus]);
}

/// Esc layering (m6 slice B): the first Esc cancels the pending route
/// (nothing committed), the next exits the Route tool back to Select.
#[test]
fn route_esc_cancels_pending_then_exits_tool() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c1)), &cx);
    assert!(app.route_active());

    app.on_event(escape(), &cx);
    assert!(!app.route_active(), "Esc cancels the pending route first");
    assert_eq!(
        app.tool_for(ViewKind::Board),
        Tool::Route,
        "…but stays in the tool"
    );
    assert!(!app.dirty(), "nothing committed");

    app.on_event(escape(), &cx);
    assert_eq!(
        app.tool_for(ViewKind::Board),
        Tool::Select,
        "the second Esc exits the tool"
    );
}

/// A lingering schematic measurement cannot jump ahead of the focused board
/// cancellation cascade: the pending route consumes Escape first.
#[test]
fn board_route_escape_precedes_schematic_measure_cancel() {
    let mut app = edit_app();
    let rendered = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&rendered.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    app.on_event(
        pointer(UiEventKind::Click, px_of_board(&app, &rendered, c1)),
        &cx,
    );
    assert!(app.route_active());

    app.set_tool(ViewKind::Schematic, Tool::Measure);
    app.claim_measure_pane(PaneId::B);
    let mut measure = crate::tool::MeasureState::default();
    measure.click(Point::mm(2, 3));
    measure.hover(Point::mm(4, 7));
    app.set_measure(measure);
    app.focused_pane.set(PaneId::A);

    app.on_event(escape(), &cx);
    assert!(!app.route_active(), "focused board route cancelled first");
    assert!(
        app.measure.get().segment().is_some(),
        "unfocused schematic measure was left untouched"
    );
}

/// A Route-tool start click on empty space (or netless copper) does nothing
/// — a trace needs a net (a data requirement, not a legality refusal).
#[test]
fn route_start_on_empty_space_does_nothing() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    // (0.5, 0.5) mm: on the board but outside the pour outline (1,1) and
    // away from every pad.
    let px = px_of_board(
        &app,
        &r,
        Point {
            x: NM_PER_MM / 2,
            y: NM_PER_MM / 2,
        },
    );
    app.on_event(pointer(UiEventKind::Click, px), &cx);
    assert!(!app.route_active(), "empty space starts nothing");
    assert!(!app.dirty());
}

/// Ladder level 1's "via drop on layer switch", end to end: switching the
/// active layer mid-route drops a through-via at the last waypoint and the
/// commit lands BOTH per-layer traces + the via in ONE transaction — a
/// single undo removes the whole route atomically.
#[test]
fn layer_switch_drops_via_and_undo_removes_whole_route() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    assert_eq!(
        app.active_layer_name().as_deref(),
        Some("F.Cu"),
        "default active layer is top copper"
    );

    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    let c2 = pin_center_of(&app, &EntityId::new("C2"), "p1");
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c1)), &cx);
    let wp_px = px_of_board(
        &app,
        &r,
        Point {
            x: 7 * NM_PER_MM,
            y: 7 * NM_PER_MM,
        },
    );
    let wp_board = board_of_px(&app, &r, wp_px);
    app.on_event(pointer(UiEventKind::Click, wp_px), &cx);

    // The layer panel's set-active affordance for B.Cu (a chrome click).
    app.on_event(click(&crate::app::pane::active_layer_key("B.Cu")), &cx);
    assert_eq!(app.active_layer_name().as_deref(), Some("B.Cu"));
    {
        let pending = app.pending_route().unwrap();
        assert_eq!(pending.vias, vec![wp_board], "via at the last waypoint");
        assert_eq!(pending.current_layer(), "B.Cu");
    }

    // Commit on C2.p1: two traces + one via, one transaction.
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c2)), &cx);
    assert!(!app.route_active());
    assert_eq!(app.undo_depths(), (1, 0), "ONE undo unit");
    {
        let doc = app.domain.doc.as_ref().unwrap();
        assert_eq!(doc.traces.len(), 2, "one trace per layer run");
        let layers: Vec<&str> = doc.traces.values().map(|t| t.layer.as_str()).collect();
        assert_eq!(layers, vec!["F.Cu", "B.Cu"]);
        let paths: Vec<&[Point]> = doc.traces.values().map(|t| t.path.as_slice()).collect();
        assert_eq!(paths[0], &[c1, wp_board][..]);
        assert_eq!(paths[1], &[wp_board, c2][..]);
        assert_eq!(doc.vias.len(), 1);
        let v = doc.vias.values().next().unwrap();
        assert_eq!(v.at, wp_board);
        assert_eq!(v.span, None, "layer-switch vias are through vias");
        let (_, drill, pad) = crate::app::route_defaults();
        assert_eq!((v.drill, v.pad), (drill, pad));
        assert_eq!(v.net.to_string(), "GND");
        // The whole route serializes into the `# routes` section.
        assert!(app.domain.source.contains("# routes"));
    }

    // One undo removes trace runs AND via together (atomic undo unit).
    app.undo();
    let doc = app.domain.doc.as_ref().unwrap();
    assert!(doc.traces.is_empty(), "undo removed both trace runs");
    assert!(doc.vias.is_empty(), "…and the via, atomically");
}

/// Post-commit refinement, end to end: with the Select tool, pressing on a
/// selected trace's SEGMENT inserts a vertex there and drags it; release
/// commits the updated path under the SAME TraceId (Remove+Add in one
/// transaction); undo restores the pre-drag path.
#[test]
fn vertex_drag_inserts_moves_and_commits_same_id() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);

    // Draw a naive straight GND trace C1.p1 → C2.p1 with the Route tool.
    app.on_event(strip_click(Tool::Route), &cx);
    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    let c2 = pin_center_of(&app, &EntityId::new("C2"), "p1");
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c1)), &cx);
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c2)), &cx);
    let tid = *app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .traces
        .keys()
        .next()
        .unwrap();
    let orig_path = vec![c1, c2];

    // Back to Select (the commit left the trace selected).
    app.on_event(strip_click(Tool::Select), &cx);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(tid))
    );

    // Press on the segment midpoint → arms a drag with an INSERTED vertex.
    let mid = Point {
        x: (c1.x + c2.x) / 2,
        y: (c1.y + c2.y) / 2,
    };
    let mid_px = px_of_board(&app, &r, mid);
    app.on_event(pointer(UiEventKind::PointerDown, mid_px), &cx);
    assert!(app.trace_drag_active(), "a segment press arms the drag");
    assert!(!app.drag_active(), "…and wins over a component drag");

    // Drag the new vertex aside and release.
    let to_px = px_of_board(
        &app,
        &r,
        Point {
            x: 9 * NM_PER_MM,
            y: 7 * NM_PER_MM,
        },
    );
    let to_board = board_of_px(&app, &r, to_px);
    app.on_event(pointer(UiEventKind::Drag, to_px), &cx);
    assert_eq!(
        app.undo_depths(),
        (1, 0),
        "nothing further committed during the drag (only the draw commit)"
    );
    app.on_event(pointer(UiEventKind::PointerUp, to_px), &cx);
    assert!(!app.trace_drag_active());
    assert_eq!(
        app.undo_depths(),
        (2, 0),
        "release committed the path edit as its own undo unit"
    );
    assert!(app.dirty());

    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.traces.len(), 1, "still one trace");
    let t = doc.traces.get(&tid).expect("SAME TraceId after the edit");
    assert_eq!(
        t.path,
        vec![c1, to_board, c2],
        "the inserted vertex landed at the drop point"
    );
    assert_eq!(t.net.to_string(), "GND", "net preserved");
    // The trailing Click of the release is eaten — the trace stays selected.
    app.on_event(pointer(UiEventKind::Click, to_px), &cx);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(tid))
    );

    // Undo restores the pre-drag path AT THE SAME id: snapshot round-trips now carry
    // identity (Decision 22), so the trace stays `tid` across the reload rather than
    // being re-minted.
    app.undo();
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.traces.len(), 1);
    assert_eq!(
        doc.traces.get(&tid).expect("SAME id after undo").path,
        orig_path
    );
}

/// Issue 0034, end to end: with a **gap** in the trace ids, undo/redo (which snapshot
/// through serialize→LoadText) preserves both the surviving ids and a selection pinned to
/// one of them. Before Decision 22 the id-free format re-minted `1..N` on every reload, so
/// redoing a deletion renumbered the surviving traces and silently dropped the selection.
#[test]
fn undo_redo_across_deletion_gap_preserves_trace_id_and_selection() {
    use eutectic_core::id::TraceId;
    let mut app = edit_app();
    let _ = settle(&mut app);

    // Three GND traces, ids 1/2/3, in ONE commit (one undo unit).
    let mk = |n: u64| {
        Command::AddTrace(
            TraceId(n),
            eutectic_core::route::Trace {
                net: eutectic_core::id::NetId::new("GND"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(1, n as i64), Point::mm(2, n as i64)],
                width: 150_000,
                prov: eutectic_core::doc::Provenance::Pinned,
            },
        )
    };
    app.commit_edit(Transaction(vec![mk(1), mk(2), mk(3)]), "add three traces")
        .expect("adds commit");
    let path3 = app.domain.doc.as_ref().unwrap().traces[&TraceId(3)]
        .path
        .clone();
    let ids = |app: &EutecticApp| {
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .keys()
            .copied()
            .collect::<Vec<_>>()
    };

    // Select trace 3, then delete trace 2 → a gap {1, 3}.
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Trace(TraceId(3)));
    app.commit_edit(
        Transaction::one(Command::RemoveTrace(TraceId(2))),
        "delete trace 2",
    )
    .expect("delete commits");
    assert_eq!(ids(&app), vec![TraceId(1), TraceId(3)], "gap after delete");
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(TraceId(3))),
        "selection survives the delete"
    );

    // Undo → {1, 2, 3} restored, id 3 still the same trace, selection intact.
    app.undo();
    assert_eq!(ids(&app), vec![TraceId(1), TraceId(2), TraceId(3)]);
    assert_eq!(
        app.domain.doc.as_ref().unwrap().traces[&TraceId(3)].path,
        path3,
        "id 3 maps to the same trace after undo"
    );
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(TraceId(3)))
    );

    // Redo → gap {1, 3} again. The 0034 repro: id 3 is NOT renumbered to 2, and the
    // selection pinned to trace 3 survives the reload.
    app.redo();
    assert_eq!(
        ids(&app),
        vec![TraceId(1), TraceId(3)],
        "redo keeps the gap, not a densified 1..N"
    );
    assert_eq!(
        app.domain.doc.as_ref().unwrap().traces[&TraceId(3)].path,
        path3,
        "id 3 is still the same trace after redo"
    );
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Trace(TraceId(3))),
        "selection of trace 3 survives undo/redo across the gap (0034)"
    );
}

/// Pressing a selected trace's VERTEX (not segment) drags that vertex
/// without inserting; Esc cancels the drag with nothing committed.
#[test]
fn vertex_drag_esc_cancels_without_commit() {
    let mut app = edit_app();
    let r = settle(&mut app);
    let cx = EventCx::new().with_ui_state(&r.ui);
    app.on_event(strip_click(Tool::Route), &cx);
    let c1 = pin_center_of(&app, &EntityId::new("C1"), "p1");
    let c2 = pin_center_of(&app, &EntityId::new("C2"), "p1");
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c1)), &cx);
    app.on_event(pointer(UiEventKind::Click, px_of_board(&app, &r, c2)), &cx);
    let path0 = app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .traces
        .values()
        .next()
        .unwrap()
        .path
        .clone();
    let dirty_after_draw = app.dirty();
    app.on_event(strip_click(Tool::Select), &cx);

    // Press ON the endpoint vertex, drag, Esc.
    app.on_event(
        pointer(UiEventKind::PointerDown, px_of_board(&app, &r, c2)),
        &cx,
    );
    assert!(app.trace_drag_active(), "a vertex press arms the drag");
    let away = px_of_board(
        &app,
        &r,
        Point {
            x: 5 * NM_PER_MM,
            y: 5 * NM_PER_MM,
        },
    );
    app.on_event(pointer(UiEventKind::Drag, away), &cx);
    app.on_event(escape(), &cx);
    assert!(!app.trace_drag_active(), "Esc cancels the vertex drag");
    assert_eq!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .values()
            .next()
            .unwrap()
            .path,
        path0,
        "the doc path is untouched"
    );
    assert_eq!(app.dirty(), dirty_after_draw, "no further commit happened");
    // A later pointer-up is inert (no stale drag).
    app.on_event(pointer(UiEventKind::PointerUp, away), &cx);
    assert_eq!(app.undo_depths(), (1, 0), "still only the draw commit");
}
