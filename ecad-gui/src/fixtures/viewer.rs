//! The milestone-1/2 viewer scenes: the three m1 states (no document / loaded /
//! parse error) and the m2 review board. Moved verbatim from `fixtures.rs`
//! (gui-module-split).

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

/// A source with a genuinely malformed directive (an `inst` missing its part
/// token), so the load reports a hard diagnostic — the parse/elaborate-error
/// state. NOTE: an *unknown part* no longer fails the load (library packages,
/// slice 1) — it degrades to a `W_UNRESOLVED_PART` finding — so this fixture
/// uses a syntax error to stay on the error path.
pub const BROKEN_ECAD: &str = "\
inst U1
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
place C1 (15mm, 3mm)
place C2 (15mm, 12mm)
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
