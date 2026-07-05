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

use crate::app::{DomainState, EcadApp};

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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::prelude::*;

    /// Render one fixture through the headless bundle pipeline and assert the
    /// lint is clean. Mirrors the pattern in the damascene-core README
    /// ("Testing without a window").
    fn assert_lint_clean(name: &str, app: &EcadApp) {
        let theme = app.theme();
        let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
        let cx = BuildCx::new(&theme).with_viewport(viewport.w, viewport.h);
        let mut root = app.build(&cx);
        let bundle = render_bundle_themed(&mut root, viewport, &theme);
        assert!(
            bundle.lint.findings.is_empty(),
            "fixture `{name}` has lint findings:\n{}",
            bundle.lint.text()
        );
    }

    #[test]
    fn no_document_is_lint_clean() {
        assert_lint_clean("no_document", &no_document());
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
        assert_lint_clean("document_loaded", &app);
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
        assert_lint_clean("parse_error", &app);
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
        assert_lint_clean("board", &app);
    }
}
