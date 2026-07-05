//! Canned [`EcadApp`] states for the headless review loop.
//!
//! Per `gui-architecture.md` ("Headless review loop"), GUI panels get the same
//! fixture-and-artifact review discipline the engine's fab outputs get: canned
//! scenes here, lint-clean assertions in the tests below, and SVG/tree/lint
//! artifacts dumped by the `review` binary (`src/bin/review.rs`).
//!
//! The three states are the ones milestone 1 can produce: no document, a
//! document loaded (from a tiny inline `.ecad` source), and a parse-error
//! state.

use crate::app::{DomainState, EcadApp, PaneId, PaneLayout, ViewKind};

/// A tiny but complete `.ecad` document: two parts, two nets, and a board
/// outline. With no `slab` directives the elaborator uses the built-in
/// two-layer stackup, so the stats card shows two copper layers and a
/// 20 x 15 mm board.
pub const SAMPLE_ECAD: &str = "\
inst reg LDO
inst C1 Cap

net VBUS reg.VOUT C1.p1
net GND reg.GND C1.p2

board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
";

/// A source that parses structurally but references a part not in the library,
/// so elaboration reports a diagnostic — the parse/elaborate-error state.
pub const BROKEN_ECAD: &str = "\
inst U1 NotAPart
net GND U1.GND
";

/// The no-document state: nothing loaded.
pub fn no_document() -> EcadApp {
    EcadApp::new(DomainState::empty())
}

/// A document loaded from [`SAMPLE_ECAD`].
pub fn document_loaded() -> EcadApp {
    EcadApp::new(DomainState::from_source(
        SAMPLE_ECAD.to_string(),
        Some("sample.ecad".to_string()),
    ))
}

/// A parse/elaborate-error state loaded from [`BROKEN_ECAD`].
pub fn parse_error() -> EcadApp {
    EcadApp::new(DomainState::from_source(
        BROKEN_ECAD.to_string(),
        Some("broken.ecad".to_string()),
    ))
}

// ---------------------------------------------------------------------------
// The milestone-2 board scene: a self-contained board rich enough to exercise
// every visual layer the canvas projects — copper, a pour with a knockout hole,
// silk, mask, outline, and drills.
// ---------------------------------------------------------------------------

/// An inline board authored against the built-in (toy) library plus command-routed
/// copper. It carries:
/// - a 20 × 15 mm board **outline**,
/// - a GND **copper pour** on `F.Cu` covering the board,
/// - a `VBUS` **trace** and a `VBUS` **via** routed *through* the GND pour (so the
///   pour fill knocks out around them — a real pour-with-hole),
/// - two toy parts on two nets (so the pour has foreign copper to knock out),
/// - a **silkscreen** text label on `F.SilkS`,
/// - an authored NPTH **hole** (drill).
///
/// This is the canvas review scene: the source is self-contained (no footprint
/// files) and every feature kind above is present, so the projection tests and the
/// SVG/tree/lint artifacts cover the whole layer set.
pub const BOARD_ECAD: &str = "\
inst C1 Cap
inst C2 Cap
net GND C1.p1 C2.p1
net VBUS C1.p2 C2.p2
board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
region conductor net=GND layer=F.Cu (1mm, 1mm) (19mm, 1mm) (19mm, 14mm) (1mm, 14mm)
text \"BRD\" (4mm, 7mm) h=2mm layer=F.SilkS
hole (10mm, 12mm) dia=1mm
";

/// The board fixture's [`DomainState`]: [`BOARD_ECAD`] loaded against the built-in
/// library, then a `VBUS` trace + via committed through the command API (routed
/// copper is command-authored, not source-authored). The trace runs across the GND
/// pour and the via drops inside it, so the pour knocks out around both.
pub fn board_domain() -> DomainState {
    use ecad_core::command::Command;
    use ecad_core::coord::Point;
    use ecad_core::doc::Provenance;
    use ecad_core::id::{NetId, TraceId, ViaId};
    use ecad_core::route::{Trace, Via};

    DomainState::from_source_with(
        BOARD_ECAD.to_string(),
        Some("board.ecad".to_string()),
        ecad_core::part::part_library(),
        |_doc| {
            vec![
                Command::AddTrace(
                    TraceId(1),
                    Trace {
                        net: NetId::new("VBUS"),
                        layer: "F.Cu".to_string(),
                        path: vec![
                            Point {
                                x: 3_000_000,
                                y: 7_000_000,
                            },
                            Point {
                                x: 17_000_000,
                                y: 7_000_000,
                            },
                        ],
                        width: 500_000,
                        prov: Provenance::Free,
                    },
                ),
                Command::AddVia(
                    ViaId(1),
                    Via {
                        net: NetId::new("VBUS"),
                        at: Point {
                            x: 15_000_000,
                            y: 10_000_000,
                        },
                        span: None,
                        drill: 300_000,
                        pad: 600_000,
                        prov: Provenance::Free,
                    },
                ),
            ]
        },
    )
}

/// The board viewer fixture: the milestone-2 read-only board scene.
pub fn board() -> EcadApp {
    EcadApp::new(board_domain())
}

// ---------------------------------------------------------------------------
// Milestone-3 scenes: a board with a trace selected (overlay + populated
// inspector), and a measure-in-progress overlay. Both are the board fixture
// with canned interaction state applied, so the SVG/tree/lint artifacts cover
// the selection overlay + inspector projection + measure preview headlessly.
// ---------------------------------------------------------------------------

/// The board fixture with its `VBUS` trace (`TraceId(1)`) pre-selected: the overlay
/// draws the selection halo and the inspector shows the trace's net / layer / width /
/// length. The selected id is looked up from the doc so the scene stays honest if the
/// fixture route changes.
pub fn board_with_selection() -> EcadApp {
    use crate::canvas::pick::SemanticId;
    let app = EcadApp::new(board_domain());
    if let Ok(doc) = &app.domain.doc {
        // Select the first routed trace by its real id (not a hardcoded number).
        if let Some(tid) = doc.traces.keys().next().copied() {
            app.domain
                .selection
                .borrow_mut()
                .select_only(SemanticId::Trace(tid));
        }
    }
    app
}

/// The board fixture in Measure mode with a measurement in progress: an anchor at
/// (3, 3) mm and the moving end at (15, 10) mm, so the overlay draws the measure line
/// and the status bar shows the dx / dy / distance readout.
pub fn measure_in_progress() -> EcadApp {
    use crate::tool::{MeasureState, Tool};
    use ecad_core::coord::{MM, Point};
    let app = EcadApp::new(board_domain());
    app.set_tool(Tool::Measure);
    let mut m = MeasureState::default();
    m.click(Point {
        x: 3 * MM,
        y: 3 * MM,
    });
    m.click(Point {
        x: 15 * MM,
        y: 10 * MM,
    });
    app.set_measure(m);
    app
}

// ---------------------------------------------------------------------------
// Milestone-4 scenes: split panes + a read-only schematic view + cross-view
// highlighting + the explorer. The schematic source authors a small `schematic`
// block inline (per the parser in `text/schematic.rs`), so the schematic pane has
// real placed symbols, and a NET is pre-selected so the cross-highlight is visible
// in BOTH panes' overlays headlessly.
// ---------------------------------------------------------------------------

/// A tiny self-contained document with a **schematic block**: three toy parts wired on
/// two nets, laid out in a `row` of symbols, plus a board outline so the board pane also
/// renders. The `MCU` toy part has named pins so the schematic draws stubs + pin names +
/// net tags. A `wire` draws a presentational connection (so the schematic pane exercises
/// wire rendering + wire-pick → net).
pub const SCHEMATIC_ECAD: &str = "\
inst U1 MCU
inst C1 Cap
inst C2 Cap
net VDD U1.VDD C1.p1
net GND U1.GND C2.p1
board (0mm, 0mm) (30mm, 0mm) (30mm, 20mm) (0mm, 20mm)
region conductor net=VDD layer=F.Cu (2mm, 2mm) (28mm, 2mm) (28mm, 10mm) (2mm, 10mm)
region conductor net=GND layer=F.Cu (2mm, 11mm) (28mm, 11mm) (28mm, 18mm) (2mm, 18mm)
schematic {
  row gap=8mm align=center {
    sym C1
    sym U1
    sym C2
    wire C1.p1 U1.VDD
  }
}
";

/// The schematic fixture's [`DomainState`]: [`SCHEMATIC_ECAD`] against the built-in lib.
pub fn schematic_domain() -> DomainState {
    DomainState::from_source(
        SCHEMATIC_ECAD.to_string(),
        Some("schematic.ecad".to_string()),
    )
}

/// Which net to pre-select in the cross-highlight scenes — a net with members on both a
/// board pad and a schematic pin, so the highlight lights up in both panes.
const CROSS_NET: &str = "VDD";

/// Pre-select the `VDD` net so the cross-highlight is visible in every pane.
fn select_cross_net(app: &EcadApp) {
    use crate::canvas::pick::SemanticId;
    use ecad_core::id::NetId;
    if let Ok(doc) = &app.domain.doc
        && doc.nets.contains_key(&NetId::new(CROSS_NET))
    {
        app.domain
            .selection
            .borrow_mut()
            .select_only(SemanticId::Net(NetId::new(CROSS_NET)));
    }
}

/// Dual layout (board | schematic) with a NET selected — the cross-highlight is visible in
/// both panes' overlays. The headline milestone-4 scene.
pub fn dual_cross_highlight() -> EcadApp {
    let app = EcadApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_layout(PaneLayout::Dual);
    select_cross_net(&app);
    app
}

/// Stacked layout (board over schematic), net selected — the stacked-orientation scene.
pub fn stacked_layout() -> EcadApp {
    let app = EcadApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_layout(PaneLayout::Stacked);
    select_cross_net(&app);
    app
}

/// A maximized pane (the schematic pane full, the board hidden) — the maximize scene.
pub fn maximized_pane() -> EcadApp {
    let app = EcadApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Schematic);
    app.set_maximized(Some(PaneId::B));
    select_cross_net(&app);
    app
}

/// Two board panes over the same doc (the per-pane-independence scene): both show the
/// board, so their cameras must be independent by El key.
pub fn dual_boards() -> EcadApp {
    let app = EcadApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Board, ViewKind::Board);
    app
}

// ---------------------------------------------------------------------------
// The real 4-layer multiprobe board, loaded from `poc/out/board.ecad` with the
// same KiCad-imported library the `poc_multiprobe` example builds. Reads files at
// call time (path relative to the crate manifest) — used by the end-to-end smoke
// test, not the inline-preferred review scene above.
// ---------------------------------------------------------------------------

/// The `poc/` directory, relative to this crate's manifest.
fn poc_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc")
}

/// Build the multiprobe part library exactly as `examples/poc_multiprobe.rs` does:
/// KiCad footprints imported from `poc/parts`, with functional pad-role overlays.
/// Panics on a missing/broken part file — this is test scaffolding, not the app
/// path.
fn poc_lib() -> ecad_core::part::PartLib {
    use ecad_core::kicad::{
        apply_role_map, import_footprint_file, import_symbol_named, join_symbol_footprint,
    };
    use ecad_core::part::{PartDef, PartLib, PinRole::*};

    let parts = poc_dir().join("parts");
    let fp = |file: &str| -> PartDef {
        import_footprint_file(parts.join(file).to_str().unwrap())
            .unwrap_or_else(|e| panic!("import {file}: {e}"))
    };
    let relabel = |part: PartDef, map: &[(&str, &str, ecad_core::part::PinRole)]| -> PartDef {
        apply_role_map(part, map).expect("role map references a missing pad")
    };

    let mut lib = PartLib::new();

    let sym = import_symbol_named(
        &std::fs::read_to_string(parts.join("MCU_RaspberryPi.kicad_sym")).unwrap(),
        "RP2350A",
    )
    .expect("RP2350A symbol");
    let mcu_fp = fp("RP2350A_QFN-60.kicad_mod");
    lib.insert("RP2350A".into(), join_symbol_footprint(&sym, &mcu_fp).part);

    lib.insert("JST_SH".into(), fp("JST_SH_3pin_Horizontal.kicad_mod"));
    lib.insert(
        "W25Q".into(),
        relabel(
            fp("Flash_SOIC-8.kicad_mod"),
            &[
                ("1", "CS_N", Input),
                ("2", "IO1", Bidir),
                ("3", "IO2", Bidir),
                ("4", "GND", Passive),
                ("5", "IO0", Bidir),
                ("6", "CLK", Input),
                ("7", "IO3", Bidir),
                ("8", "VCC", PowerIn),
            ],
        ),
    );
    lib.insert(
        "XTAL".into(),
        relabel(
            fp("Crystal_3225.kicad_mod"),
            &[
                ("1", "X1", Passive),
                ("2", "GNDa", Passive),
                ("3", "X2", Passive),
                ("4", "GNDb", Passive),
            ],
        ),
    );
    lib.insert(
        "REG".into(),
        relabel(
            fp("Regulator_SOT-23-5.kicad_mod"),
            &[
                ("1", "VIN", PowerIn),
                ("2", "GND", Passive),
                ("3", "EN", Input),
                ("4", "NC", Passive),
                ("5", "VOUT", PowerOut),
            ],
        ),
    );
    lib.insert(
        "USBC".into(),
        relabel(
            fp("USB_C_Receptacle.kicad_mod"),
            &[
                ("A1", "GND", Passive),
                ("A4", "VBUS", PowerIn),
                ("A5", "CC1", Passive),
                ("A6", "DP", Bidir),
                ("A7", "DM", Bidir),
                ("A8", "SBU1", Passive),
                ("A9", "VBUS", PowerIn),
                ("A12", "GND", Passive),
                ("B1", "GND", Passive),
                ("B4", "VBUS", PowerIn),
                ("B5", "CC2", Passive),
                ("B6", "DP", Bidir),
                ("B7", "DM", Bidir),
                ("B8", "SBU2", Passive),
                ("B9", "VBUS", PowerIn),
                ("B12", "GND", Passive),
                ("SH", "SHIELD", Passive),
            ],
        ),
    );
    lib.insert("IND".into(), fp("Inductor_2020.kicad_mod"));
    lib.insert("R".into(), fp("R_0402.kicad_mod"));
    lib.insert("C".into(), fp("C_0402.kicad_mod"));
    lib.insert("BTN".into(), fp("Button_EVQP7A.kicad_mod"));
    lib.insert(
        "LED".into(),
        relabel(
            fp("LED_WS2812B.kicad_mod"),
            &[
                ("1", "VDD", PowerIn),
                ("2", "DOUT", Output),
                ("3", "GND", Passive),
                ("4", "DIN", Input),
            ],
        ),
    );
    lib
}

/// The real multiprobe board loaded from `poc/out/board.ecad` with [`poc_lib`].
/// Used by the end-to-end canvas smoke test.
pub fn poc_board_domain() -> DomainState {
    let path = poc_dir().join("out/board.ecad");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    DomainState::from_source_with(source, Some("board.ecad".to_string()), poc_lib(), |_| {
        Vec::new()
    })
}

/// Every fixture, paired with a stable scene name for artifact filenames. The
/// `poc_board` scene is deliberately excluded — it reads files, so it belongs to
/// the smoke test, not the always-on lint bundle.
pub fn all() -> Vec<(&'static str, EcadApp)> {
    vec![
        ("no_document", no_document()),
        ("document_loaded", document_loaded()),
        ("parse_error", parse_error()),
        ("board", board()),
        ("board_with_selection", board_with_selection()),
        ("measure_in_progress", measure_in_progress()),
        ("dual_cross_highlight", dual_cross_highlight()),
        ("stacked_layout", stacked_layout()),
        ("maximized_pane", maximized_pane()),
        ("dual_boards", dual_boards()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn render_clean(name: &str, mut app: EcadApp) -> Rendered {
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
            "sample.ecad failed to elaborate: {:?}",
            app.domain.doc.as_ref().err()
        );
        // sample.ecad has a board (pane A) but no schematic block, so pane B is a
        // placeholder with no viewport — only the board pane is fit-checked.
        let r = render_clean("document_loaded", app);
        harness::assert_content_coverage("document_loaded", &r, &[PaneId::A.canvas_key()]);
    }

    #[test]
    fn parse_error_is_lint_clean() {
        let app = parse_error();
        // The broken source must actually fail — otherwise this fixture is not
        // exercising the error path.
        assert!(
            app.domain.doc.is_err(),
            "broken.ecad unexpectedly elaborated"
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
            "board.ecad failed to elaborate: {:?}",
            app.domain.doc.as_ref().err()
        );
        // BOARD_ECAD has no schematic block, so only pane A (board) has content.
        let r = render_clean("board", app);
        harness::assert_content_coverage("board", &r, &[PaneId::A.canvas_key()]);
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

    #[test]
    fn schematic_fixture_elaborates_and_projects() {
        // The schematic source must elaborate AND its schematic must project non-empty —
        // otherwise the schematic pane is silently the empty placeholder.
        let app = dual_cross_highlight();
        assert!(
            app.domain.doc.is_ok(),
            "schematic.ecad failed: {:?}",
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

    /// Inspector value honesty: the selected part's inspector shows the position
    /// authored in the fixture source — no hardcoded values. Selects a component by
    /// its real entity id and asserts the projected `Position` row matches the doc's
    /// stored position.
    #[test]
    fn inspector_shows_authored_part_position() {
        use crate::canvas::pick::SemanticId;
        use crate::inspector::InspectorData;
        use ecad_core::doc::MM;

        let d = board_domain();
        let doc = d.doc.as_ref().expect("board fixture elaborates");
        let (eid, comp) = doc
            .components
            .iter()
            .next()
            .expect("board fixture has a component");
        let data = InspectorData::project(&SemanticId::Part(eid.clone()), doc, &d.lib)
            .expect("part projects");

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

        use ecad_core::doc::PinRef;

        let d = poc_board_domain();
        let doc = d.doc.as_ref().expect("poc board elaborates");
        let su = ecad_core::elaborate::stackup(&doc.source);
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
}
