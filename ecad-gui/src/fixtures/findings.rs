//! The findings scenes: the m5 deliberate-DRC-violation board and the
//! library-packages slice-2 unresolved-libraries / Libraries-menu scenes.
//! Moved verbatim from `fixtures.rs` (gui-module-split).

use crate::app::{DomainState, EcadApp};

// ---------------------------------------------------------------------------
// Milestone-5 findings fixture: a board with a DELIBERATE clearance violation —
// two traces on different nets routed 0.05 mm apart on F.Cu, well inside the
// 0.15 mm default clearance. DRC (`route/drc.rs`) flags `E_DRC_CLEARANCE`; the
// findings panel populates, the halo lands at the derived board-mm point, and the
// chip shows the error count.
// ---------------------------------------------------------------------------

/// The findings fixture source: four toy caps on two nets (`NA`, `NB`) plus a board
/// outline. No pours (so the only findings come from the routed copper below, keeping
/// the violation set predictable). The traces are command-authored in
/// [`drc_violation_domain`].
pub const DRC_ECAD: &str = "\
inst A1 Cap
inst A2 Cap
inst B1 Cap
inst B2 Cap
net NA A1.p1 A2.p1
net NB B1.p1 B2.p1
place A1 (5mm, 5mm)
place A2 (15mm, 5mm)
place B1 (5mm, 10mm)
place B2 (15mm, 10mm)
board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
";

/// The findings fixture's [`DomainState`]: [`DRC_ECAD`] plus two parallel traces on
/// **different** nets 0.05 mm apart (centre-to-centre 0.3 mm, each 0.25 mm wide → 0.05
/// mm edge gap), which is inside the 0.15 mm default clearance → a guaranteed
/// `E_DRC_CLEARANCE` violation on `F.Cu`. Both nets are also 2-pin and not fully
/// routed, so the set additionally carries `E_DRC_UNROUTED` for each — the fixture
/// exercises multiple simultaneous findings.
pub fn drc_violation_domain() -> DomainState {
    use ecad_core::command::Command;
    use ecad_core::coord::Point;
    use ecad_core::doc::Provenance;
    use ecad_core::id::{NetId, TraceId};
    use ecad_core::route::Trace;

    DomainState::from_source_with(
        DRC_ECAD.to_string(),
        Some("drc.ecad".to_string()),
        ecad_core::part::part_library(),
        |_doc| {
            let trace = |id: u64, net: &str, y: i64| {
                Command::AddTrace(
                    TraceId(id),
                    Trace {
                        net: NetId::new(net),
                        layer: "F.Cu".to_string(),
                        path: vec![Point { x: 4_000_000, y }, Point { x: 16_000_000, y }],
                        width: 250_000,
                        prov: Provenance::Free,
                    },
                )
            };
            // Two 0.25 mm traces on different nets, centres 0.3 mm apart at y=7.0 and
            // y=7.3 mm → 0.05 mm edge gap ≪ 0.15 mm clearance.
            vec![trace(1, "NA", 7_000_000), trace(2, "NB", 7_300_000)]
        },
    )
}

/// The findings fixture as an app (m5): a board with a deliberate clearance short, so
/// the findings panel + halo + chip render populated.
pub fn drc_violation() -> EcadApp {
    EcadApp::new(drc_violation_domain())
}

/// The right-sidebar accordion with **Findings expanded** and **Layers collapsed**
/// (over the DRC-violation board, so the Findings body has real rows and the header
/// carries populated err/warn chips). Proves the accordion's headline invariant: the
/// collapsed Layers section still shows its header — nothing lives below an invisible
/// fold — while a section that is closed by default (Findings) can be opened. The
/// other two sections keep their defaults (Properties open, Explorer collapsed).
pub fn sidebar_findings_expanded() -> EcadApp {
    use crate::app::pane::SidebarSection;
    let app = EcadApp::new(drc_violation_domain());
    app.set_section_open(SidebarSection::Findings, true);
    app.set_section_open(SidebarSection::Layers, false);
    app
}

// ---------------------------------------------------------------------------
// Library-packages slice-2 scenes: a doc whose `use` name resolves to nothing
// (registry-driven, permissive degrade → findings rows + a loaded-but-degraded
// board), and the Libraries menu open over it.
// ---------------------------------------------------------------------------

/// A doc that `use`s an unregistered library and instantiates a part only that
/// library could provide: `nolib` misses the registry (a `W_LIB_UNREGISTERED`
/// note), `Ghost` misses the resolved lib (an engine `W_UNRESOLVED_PART`
/// finding — U9 is skipped), and the two toy caps + outline still elaborate —
/// the loaded-but-degraded board the findings panel annotates.
pub const UNRESOLVED_LIBS_ECAD: &str = "\
use nolib
inst U9 Ghost
inst C1 Cap
inst C2 Cap
net GND C1.p1 C2.p1
net VBUS C1.p2 C2.p2
board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
";

/// The unresolved-libraries [`DomainState`]: [`UNRESOLVED_LIBS_ECAD`] resolved
/// through an **empty test registry** (never the per-user config; no save
/// path), so resolution degrades exactly as a fresh machine would: the doc
/// loads, the findings carry the unregistered-library note and the skipped
/// instance.
pub fn unresolved_libs_domain() -> DomainState {
    DomainState::from_source_registry(
        UNRESOLVED_LIBS_ECAD.to_string(),
        Some("unresolved.ecad".to_string()),
        crate::registry::Registry::new(),
        None,
    )
}

/// The unresolved-libraries scene: findings rows (`W_LIB_UNREGISTERED` +
/// `W_UNRESOLVED_PART`) over the degraded-but-rendered board.
pub fn unresolved_libs() -> EcadApp {
    EcadApp::new(unresolved_libs_domain())
}

/// The Libraries menu open over the unresolved-libraries doc, with a test
/// registry containing one entry whose path does not exist — the menu's rows
/// show the per-row load status (here: "path missing"), the add-entry inputs,
/// and the close affordance. The path is deterministic and never touched
/// (nothing is created there), so the scene needs no file IO to render.
pub fn libraries_menu() -> EcadApp {
    let mut registry = crate::registry::Registry::new();
    registry
        .set("stale", std::path::Path::new("/nonexistent/ecad-lib"))
        .expect("absolute path registers");
    let domain = DomainState::from_source_registry(
        UNRESOLVED_LIBS_ECAD.to_string(),
        Some("unresolved.ecad".to_string()),
        registry,
        None,
    );
    let app = EcadApp::new(domain);
    app.set_libraries_open(true);
    app
}
