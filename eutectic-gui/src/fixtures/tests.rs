//! The fixture test suite: every scene renders lint-clean through the settled
//! harness, plus the projection-honesty and coverage assertions. Moved
//! verbatim from `fixtures.rs` (gui-module-split).

use super::*;
use crate::app::{DomainState, PaneId};
use crate::harness::{self, Rendered};
use damascene_core::prelude::*;

/// The fixed review viewport shared by every headless render below.
fn viewport() -> Rect {
    Rect::new(0.0, 0.0, 1280.0, 800.0)
}

/// Drive a fixture through the two-frame host-mirroring harness (so the render
/// reflects the fitted camera, exactly as the `review` binary dumps it) and
/// assert the lint is clean. Takes the app by value because the harness drains
/// its queued viewport requests across frames; returns the settled render so
/// coverage-checking scenes can inspect the post-fit cameras.
fn render_clean(name: &str, mut app: EutecticApp) -> Rendered {
    let r = harness::render_settled(&mut app, viewport());
    assert!(
        r.bundle.lint.findings.is_empty(),
        "fixture `{name}` has lint findings:\n{}",
        r.bundle.lint.text()
    );
    r
}

/// The two canvas viewport keys, for coverage assertions.
fn canvas_keys() -> [&'static str; 2] {
    [PaneId::A.canvas_key(), PaneId::B.canvas_key()]
}

#[test]
fn no_document_is_lint_clean() {
    // No-document / error scenes render no canvas viewport, so there is nothing
    // to fit — lint only.
    render_clean("no_document", no_document());
}

#[test]
fn document_loaded_is_lint_clean() {
    let app = document_loaded();
    // The sample must actually elaborate — otherwise the fixture is
    // silently exercising the error path instead of the loaded path.
    assert!(
        app.domain.doc.is_ok(),
        "sample.eut failed to elaborate: {:?}",
        app.domain.doc.as_ref().err()
    );
    // sample.eut has a board (pane A) but no schematic block, so pane B is a
    // placeholder with no viewport — only the board pane is fit-checked.
    let r = render_clean("document_loaded", app);
    harness::assert_content_coverage("document_loaded", &r, &[PaneId::A.canvas_key()]);
}

/// Library packages, slice 1 + 2: a source whose only fault is an *unknown
/// part* LOADS (permissive degrade) with a `W_UNRESOLVED_PART` finding on the
/// report — the instance is skipped and its netted pins are cascade-suppressed,
/// never a hard error — and (slice 2) the skip renders as an **informational**
/// findings-panel row (warning; no refs, no halo — the entity doesn't exist).
#[test]
fn unresolved_part_loads_with_finding() {
    use crate::findings::Findings;
    let d = DomainState::from_source(
        "inst U1 NotAPart\nnet GND U1.GND\n".to_string(),
        Some("unresolved.eut".to_string()),
    );
    let doc = d
        .doc
        .as_ref()
        .expect("an unknown part must degrade, not fail the load");
    assert!(
        doc.components.is_empty(),
        "the unresolved instance is skipped"
    );
    assert_eq!(
        doc.report.unresolved_parts.len(),
        1,
        "the skip surfaces as a finding: {:?}",
        doc.report.unresolved_parts
    );
    let (id, part, _help) = &doc.report.unresolved_parts[0];
    assert_eq!(id.to_string(), "U1");
    assert_eq!(part, "NotAPart");

    // Slice 2: the skip is a findings-panel row — a warning that counts into
    // the chip, informational (nothing to select or zoom to).
    let f = Findings::compute(doc, &d.lib, &[], &d.lib_notes);
    let row = f
        .items
        .iter()
        .find(|i| i.code == "W_UNRESOLVED_PART")
        .expect("the skip renders as a findings row");
    assert!(
        row.message.contains("NotAPart") && row.message.contains("U1"),
        "the row names the instance and the part: {}",
        row.message
    );
    assert!(row.is_informational(), "no geometry → non-navigating row");
    assert!(f.warnings >= 1, "counts into the chip as a warning");
}

#[test]
fn parse_error_is_lint_clean() {
    let app = parse_error();
    // The broken source must actually fail — otherwise this fixture is not
    // exercising the error path.
    assert!(
        app.domain.doc.is_err(),
        "broken.eut unexpectedly elaborated"
    );
    render_clean("parse_error", app);
}

#[test]
fn board_is_lint_clean() {
    let app = board();
    // The board must elaborate *and* project a canvas — otherwise the viewer
    // is silently falling back to the stats card.
    assert!(
        app.domain.doc.is_ok(),
        "board.eut failed to elaborate: {:?}",
        app.domain.doc.as_ref().err()
    );
    // BOARD_ECAD has no schematic block, so only pane A (board) has content.
    let r = render_clean("board", app);
    harness::assert_content_coverage("board", &r, &[PaneId::A.canvas_key()]);
}

/// Pins the load-bearing layout assumption behind `Canvas::content_rect` (and
/// with it every pointer↔board mapping): a `vector()` child lays out at its
/// NATURAL viewBox size, top-left-anchored at the viewport origin, so the
/// asset stretch rect is `(rect.x, rect.y, vw, vh)`. The m2/m3 composition
/// wrongly used the pane rect here and stayed invisible because both pick
/// directions shared the error — this test fails against the real laid-out
/// UI state if damascene's layout contract ever changes.
#[test]
fn canvas_child_lays_out_at_natural_viewbox_size_at_viewport_origin() {
    let mut app = board();
    let (_, _, vw, vh) = app
        .derived
        .borrow()
        .board
        .as_ref()
        .expect("board scene projects a canvas")
        .canvas
        .content_rect((0.0, 0.0, 0.0, 0.0));
    let r = harness::render_settled(&mut app, viewport());
    let content = harness::content_bounds_of(&r, PaneId::A.canvas_key())
        .expect("canvas viewport has measured content bounds");
    // The bounds carry the viewport's own (untransformed) window origin;
    // "anchored at the viewport origin" pins content.xy == pane.xy.
    let pane =
        r.ui.rect_of_key(PaneId::A.canvas_key())
            .expect("canvas viewport has a laid-out rect");
    assert!(
        (content.x - pane.x).abs() < 0.5 && (content.y - pane.y).abs() < 0.5,
        "canvas content must anchor at the viewport origin ({}, {}), got ({}, {})",
        pane.x,
        pane.y,
        content.x,
        content.y
    );
    assert!(
        (content.w - vw).abs() < 0.5 && (content.h - vh).abs() < 0.5,
        "canvas content must lay out at natural viewBox size {vw}×{vh}, \
         got {}×{}",
        content.w,
        content.h
    );
}

/// The menu-bar scene (chrome region 1): the File menu is expanded, so the menu
/// popover is in the tree — it must render lint-clean over the fitted board.
#[test]
fn menubar_open_is_lint_clean() {
    let app = menubar_open();
    assert_eq!(
        app.open_menu.borrow().as_deref(),
        Some("file"),
        "the scene must have the File menu open"
    );
    let r = render_clean("menubar_open", app);
    harness::assert_content_coverage("menubar_open", &r, &[PaneId::A.canvas_key()]);
}

#[test]
fn board_with_selection_is_lint_clean() {
    let app = board_with_selection();
    // The scene must actually have a selection — otherwise it is silently the
    // empty-inspector board.
    assert!(
        !app.domain.selection.borrow().is_empty(),
        "board_with_selection has no selection"
    );
    let r = render_clean("board_with_selection", app);
    harness::assert_content_coverage("board_with_selection", &r, &[PaneId::A.canvas_key()]);
}

#[test]
fn measure_in_progress_is_lint_clean() {
    let r = render_clean("measure_in_progress", measure_in_progress());
    harness::assert_content_coverage("measure_in_progress", &r, &[PaneId::A.canvas_key()]);
}

/// The DRC findings fixture: it must actually flag the deliberate clearance short
/// (otherwise the scene silently exercises a clean board), render lint-clean, and
/// its board pane must fit — the findings panel + halo + chip are all in the tree.
#[test]
fn drc_violation_is_lint_clean_and_flags() {
    use crate::canvas::pick::candidates;
    use crate::findings::Findings;
    let d = drc_violation_domain();
    let doc = d.doc.as_ref().expect("drc fixture elaborates");
    let su = eutectic_core::elaborate::stackup(&doc.source);
    let cands = candidates(doc, &d.lib, &su);
    let f = Findings::compute(doc, &d.lib, &cands, &d.lib_notes);
    assert!(
        f.items.iter().any(|i| i.code == "E_DRC_CLEARANCE"),
        "the drc_violation fixture must flag a clearance short"
    );
    assert!(f.errors >= 1, "the chip must show at least one error");
    // The scene has a board (pane A) but no schematic block, so only pane A fits.
    let r = render_clean("drc_violation", drc_violation());
    harness::assert_content_coverage("drc_violation", &r, &[PaneId::A.canvas_key()]);
}

#[test]
fn schematic_fixture_elaborates_and_projects() {
    // The schematic source must elaborate AND its schematic must project non-empty —
    // otherwise the schematic pane is silently the empty placeholder.
    let app = dual_cross_highlight();
    assert!(
        app.domain.doc.is_ok(),
        "schematic.eut failed: {:?}",
        app.domain.doc.as_ref().err()
    );
    let doc = app.domain.doc.as_ref().unwrap();
    let view = crate::schematic_view::SchematicView::build(doc, &app.domain.lib)
        .expect("schematic projects");
    assert!(
        !view.candidates().is_empty(),
        "schematic must have pick candidates"
    );
}

#[test]
fn dual_cross_highlight_is_lint_clean() {
    let app = dual_cross_highlight();
    assert!(
        !app.domain.selection.borrow().is_empty(),
        "cross-highlight scene must have a net selected"
    );
    // The dual scene shows a board (pane A) and a schematic (pane B), both with
    // content — the headline artifact, so both panes must fit.
    let r = render_clean("dual_cross_highlight", app);
    harness::assert_content_coverage("dual_cross_highlight", &r, &canvas_keys());
}

#[test]
fn stacked_layout_is_lint_clean() {
    // Same board|schematic content as the dual scene, stacked orientation.
    let r = render_clean("stacked_layout", stacked_layout());
    harness::assert_content_coverage("stacked_layout", &r, &canvas_keys());
}

#[test]
fn maximized_pane_is_lint_clean() {
    // Pane B (schematic) is maximized; pane A is hidden (no viewport), so only
    // the visible schematic pane is fit-checked.
    let r = render_clean("maximized_pane", maximized_pane());
    harness::assert_content_coverage("maximized_pane", &r, &[PaneId::B.canvas_key()]);
}

#[test]
fn dual_boards_is_lint_clean() {
    // Two board panes over the same doc — both show board content, both fit.
    let r = render_clean("dual_boards", dual_boards());
    harness::assert_content_coverage("dual_boards", &r, &canvas_keys());
}

/// The per-kind-tools scene (revised structural commitment 4): the two kinds
/// hold DIFFERENT active tools simultaneously — the board slot Route, the
/// schematic slot Measure — and the scene renders lint-clean with both panes
/// fitted (each pane's strip shows its own kind's active tool).
#[test]
fn per_kind_tools_is_lint_clean_and_holds_two_tools() {
    use crate::app::ViewKind;
    use crate::tool::Tool;
    let app = per_kind_tools();
    assert_eq!(app.tool_for(ViewKind::Board), Tool::Route);
    assert_eq!(app.tool_for(ViewKind::Schematic), Tool::Measure);
    let r = render_clean("per_kind_tools", app);
    harness::assert_content_coverage("per_kind_tools", &r, &canvas_keys());
}

/// The unresolved-libraries scene (slice 2): the doc LOADS degraded (the ghost
/// instance skipped, the caps + outline render), and the findings carry BOTH
/// library rows — the GUI-side `W_LIB_UNREGISTERED` resolution note and the
/// engine `W_UNRESOLVED_PART` — as chip-counted warnings.
#[test]
fn unresolved_libs_is_lint_clean_and_carries_library_findings() {
    let app = unresolved_libs();
    let doc = app
        .domain
        .doc
        .as_ref()
        .expect("a missing library degrades, never fails the load");
    assert_eq!(
        doc.components.len(),
        2,
        "the ghost instance is skipped; the two caps remain"
    );
    let f = app.findings();
    assert!(
        f.items.iter().any(|i| i.code == "W_LIB_UNREGISTERED"),
        "the unregistered `use nolib` renders as a findings row: {:?}",
        f.items
    );
    assert!(
        f.items
            .iter()
            .any(|i| i.code == "W_UNRESOLVED_PART" && i.message.contains("Ghost")),
        "the skipped instance renders as a findings row naming the part"
    );
    assert!(f.warnings >= 2, "both count into the chip as warnings");

    let r = render_clean("unresolved_libs", app);
    harness::assert_content_coverage("unresolved_libs", &r, &[PaneId::A.canvas_key()]);
}

/// The Libraries-menu scene (slice 2): the modal renders lint-clean over the
/// board, with the stale registry row showing its "path missing" status. The
/// board pane behind the scrim still fits.
#[test]
fn libraries_menu_is_lint_clean() {
    let app = libraries_menu();
    let r = render_clean("libraries_menu", app);
    harness::assert_content_coverage("libraries_menu", &r, &[PaneId::A.canvas_key()]);
}

/// The drag-in-progress scene (m6): the drag is actually armed and moved (so
/// the overlay carries the ghost + a non-empty ratsnest), the doc is
/// untouched (nothing committed, not dirty), and the scene renders lint-clean
/// with a fitted board pane.
#[test]
fn drag_in_progress_is_lint_clean_and_previews() {
    let app = drag_in_progress();
    assert!(app.drag_active(), "the scene must have a drag armed");
    assert!(!app.dirty(), "a drag preview commits nothing");
    assert_eq!(app.revision(), 0, "no commit → no revision bump");
    let r = render_clean("drag_in_progress", app);
    harness::assert_content_coverage("drag_in_progress", &r, &[PaneId::A.canvas_key()]);
}

/// The dirty-doc scene (m6): the committed move left the doc dirty with one
/// undo step, and the scene renders lint-clean (the chrome carries the dirty
/// bullet + Save button — presence is asserted structurally via `dirty()` +
/// `source_path`, the render via the lint gate).
#[test]
fn dirty_doc_is_lint_clean_and_dirty() {
    let app = dirty_doc();
    assert!(app.dirty(), "the scene must be dirty");
    assert_eq!(app.undo_depths(), (1, 0), "one committed edit → one undo");
    assert!(
        app.domain.source_path.is_some(),
        "the save affordance needs a source path"
    );
    let r = render_clean("dirty_doc", app);
    harness::assert_content_coverage("dirty_doc", &r, &[PaneId::A.canvas_key()]);
}

/// The conflict-banner scene (m6): after the harness's first frame the
/// external change is parked as the pending conflict (NOT applied — the doc
/// stays dirty at its pre-conflict revision), and the banner scene renders
/// lint-clean.
#[test]
fn conflict_banner_is_lint_clean_and_pending() {
    let mut app = conflict_banner();
    let rev0 = app.revision();
    let r = harness::render_settled(&mut app, viewport());
    assert!(
        r.bundle.lint.findings.is_empty(),
        "fixture `conflict_banner` has lint findings:\n{}",
        r.bundle.lint.text()
    );
    assert!(app.conflict().is_some(), "the external change is parked");
    assert!(app.dirty(), "the doc stays dirty");
    assert_eq!(app.revision(), rev0, "the external change was NOT applied");
    harness::assert_content_coverage("conflict_banner", &r, &[PaneId::A.canvas_key()]);
}

/// The route-in-progress scene (m6 slice B): a pending route with waypoints
/// and a live rubber segment, nothing committed (doc clean), lint-clean with
/// a fitted board pane.
#[test]
fn route_in_progress_is_lint_clean_and_pending() {
    let app = route_in_progress();
    assert!(app.route_active(), "the scene must have a pending route");
    assert!(!app.dirty(), "a pending route commits nothing");
    let r = app.pending_route().unwrap();
    assert_eq!(r.net.to_string(), "GND", "started from C1.p1 → net GND");
    assert_eq!(r.runs.len(), 1, "single-layer so far");
    assert_eq!(r.runs[0].points.len(), 3, "anchor + two waypoints");
    assert!(r.rubber().is_some(), "the rubber segment tracks the cursor");
    let rendered = render_clean("route_in_progress", app);
    harness::assert_content_coverage("route_in_progress", &rendered, &[PaneId::A.canvas_key()]);
}

/// The committed multi-waypoint trace scene (m6 slice B): one GND trace with
/// the pin-snapped endpoints and both waypoints, committed through the
/// command layer (dirty, one undo, trace selected), lint-clean.
#[test]
fn routed_trace_is_lint_clean_and_committed() {
    let app = routed_trace();
    assert!(!app.route_active(), "the route committed");
    assert!(app.dirty(), "the commit dirtied the doc");
    assert_eq!(app.undo_depths(), (1, 0), "one commit → one undo unit");
    let doc = app.domain.doc.as_ref().unwrap();
    assert_eq!(doc.traces.len(), 1, "one committed trace");
    let t = doc.traces.values().next().unwrap();
    assert_eq!(t.net.to_string(), "GND");
    assert_eq!(t.layer, "F.Cu");
    assert_eq!(
        t.path,
        vec![mm_pt(14, 3), mm_pt(10, 5), mm_pt(10, 9), mm_pt(14, 12)],
        "pad-centre anchor + waypoints + pad-centre end"
    );
    let (width, ..) = crate::app::route_defaults();
    assert_eq!(t.width, width, "the DRC/router default width (0.15 mm)");
    assert_eq!(
        t.prov,
        eutectic_core::doc::Provenance::Pinned,
        "hand-routed"
    );
    // The commit left the new trace selected, ready for refinement.
    let tid = *doc.traces.keys().next().unwrap();
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&crate::canvas::pick::SemanticId::Trace(tid))
    );
    let rendered = render_clean("routed_trace", app);
    harness::assert_content_coverage("routed_trace", &rendered, &[PaneId::A.canvas_key()]);
}

/// The layer-switch scene (m6 slice B): the pending route carries the via
/// drop at the switch point and continues on B.Cu; still uncommitted.
#[test]
fn route_layer_switch_is_lint_clean_and_has_via() {
    let app = route_layer_switch();
    assert!(app.route_active());
    assert!(!app.dirty(), "still pending — nothing committed");
    assert_eq!(app.active_layer_name().as_deref(), Some("B.Cu"));
    let r = app.pending_route().unwrap();
    assert_eq!(
        r.vias,
        vec![mm_pt(10, 5)],
        "via dropped at the last waypoint"
    );
    assert_eq!(r.runs.len(), 2, "one run per layer");
    assert_eq!(r.runs[0].layer, "F.Cu");
    assert_eq!(r.runs[1].layer, "B.Cu");
    assert_eq!(
        r.runs[1].points.first(),
        Some(&mm_pt(10, 5)),
        "the new run continues from the via point"
    );
    let rendered = render_clean("route_layer_switch", app);
    harness::assert_content_coverage("route_layer_switch", &rendered, &[PaneId::A.canvas_key()]);
}

/// The vertex-refinement scene (m6 slice B): a trace-vertex drag in flight
/// over the committed trace — handles + working-path preview render, the
/// doc still holds the pre-drag path.
#[test]
fn trace_vertex_drag_is_lint_clean_and_previews() {
    let app = trace_vertex_drag();
    assert!(app.trace_drag_active(), "the scene must have a drag armed");
    let doc = app.domain.doc.as_ref().unwrap();
    let t = doc.traces.values().next().unwrap();
    assert_eq!(
        t.path[1],
        mm_pt(10, 5),
        "the doc path is untouched until release"
    );
    let rendered = render_clean("trace_vertex_drag", app);
    harness::assert_content_coverage("trace_vertex_drag", &rendered, &[PaneId::A.canvas_key()]);
}

/// Inspector value honesty: the selected part's inspector shows the position
/// authored in the fixture source — no hardcoded values. Selects a component by
/// its real entity id and asserts the projected `Position` row matches the doc's
/// stored position.
#[test]
fn inspector_shows_authored_part_position() {
    use crate::canvas::pick::SemanticId;
    use crate::inspector::InspectorData;
    use eutectic_core::doc::MM;

    let d = board_domain();
    let doc = d.doc.as_ref().expect("board fixture elaborates");
    let (eid, comp) = doc
        .components
        .iter()
        .next()
        .expect("board fixture has a component");
    let data =
        InspectorData::project(&SemanticId::Part(eid.clone()), doc, &d.lib).expect("part projects");

    // The identity card shows the refdes; a Position row shows the authored mm.
    assert_eq!(data.kind, "Part");
    let pos_row = data
        .rows
        .iter()
        .find(|r| r.key == "Position")
        .expect("inspector has a Position row");
    let expect = format!(
        "{:.3}, {:.3} mm",
        comp.pos.value.x as f64 / MM as f64,
        comp.pos.value.y as f64 / MM as f64
    );
    assert_eq!(
        pos_row.value, expect,
        "inspector Position must be the doc's authored position, not a hardcoded value"
    );
}

/// Pin-pick identity round-trip (regression for the m3 pin blocker). A picked
/// `SemanticId::Pin` must carry the pad *number* (the `PinRef` / net-membership
/// join key), NOT the functional pin name — otherwise the inspector's net lookup
/// misses and every netted, renamed pad reads "(unconnected)". Drives the full
/// pick → inspector chain over the real poc board (real KiCad footprints, where
/// pad name != number), asserting that a pad which IS on a net projects that net.
#[test]
fn picked_pin_projects_its_net() {
    use crate::canvas::pick::{SemanticId, candidates};
    use crate::inspector::InspectorData;

    use eutectic_core::doc::PinRef;

    let d = poc_board_domain();
    let doc = d.doc.as_ref().expect("poc board elaborates");
    let su = eutectic_core::elaborate::stackup(&doc.source);
    let cands = candidates(doc, &d.lib, &su);
    let pin_cands: Vec<&SemanticId> = cands
        .iter()
        .filter(|c| matches!(c.id, SemanticId::Pin { .. }))
        .map(|c| &c.id)
        .collect();
    assert!(
        !pin_cands.is_empty(),
        "poc board has real footprints, so the picker must emit pin candidates"
    );

    // Ground truth is keyed by pad NUMBER (the `PinRef` contract), established
    // independently of what the picker put in the candidate. For every pin whose
    // pad number is netted AND whose functional name differs from its number (the
    // exact case the blocker hit), require the picker to emit a matching candidate
    // whose inspector projection reports that same net. If the picker stored the
    // NAME instead of the NUMBER, either no candidate carries the number (miss) or
    // the projected id keys the wrong node (unconnected) — both fail here.
    let mut checked = 0usize;
    for (eid, comp) in &doc.components {
        let Some(def) = d.lib.get(&comp.part) else {
            continue;
        };
        for p in &def.pins {
            if p.name == p.number {
                continue; // toy-style pins can't distinguish the bug
            }
            let pr = PinRef::new(eid, &p.number);
            let net = doc
                .nets
                .iter()
                .find(|(_, n)| n.members.contains(&pr))
                .map(|(nid, _)| nid.to_string());
            let Some(net) = net else { continue };

            // The picker must have emitted a candidate identifying this exact pad
            // by NUMBER (not name) — otherwise a directly-picked pad is unreachable.
            let want = SemanticId::Pin {
                comp: eid.clone(),
                pin: p.number.clone(),
            };
            assert!(
                pin_cands.iter().any(|id| **id == want),
                "picker must emit a Pin candidate keyed by pad number for \
                 {eid:?}.{} (name {}); the candidate set is name-keyed",
                p.number,
                p.name
            );

            let data = InspectorData::project(&want, doc, &d.lib).expect("pin projects");
            let net_row = data.rows.iter().find(|r| r.key == "Net").unwrap();
            assert_eq!(
                net_row.value, net,
                "picked pad {eid:?}.{} (name {}) is on net {net} but the inspector \
                 reports {:?} — pin identity must be the pad number, not the name",
                p.number, p.name, net_row.value
            );
            assert_eq!(data.net.as_deref(), Some(net.as_str()));
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "poc board must have at least one netted pad whose name differs from its number"
    );
}
