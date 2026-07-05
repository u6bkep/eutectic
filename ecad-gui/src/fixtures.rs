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
// Milestone-6 slice-A scenes: the editing foundation. A drag in progress
// (ghost + live ratsnest in the overlay), a dirty doc (filename bullet + a
// primary Save button), and the disk-conflict banner (external change while
// dirty → explicit Reload / Keep-mine, never silent last-writer).
// ---------------------------------------------------------------------------

/// The m6 editing board: [`BOARD_ECAD`]'s two netted caps + outline + pour,
/// with no command-routed extras — the editing scenes exercise the commit path
/// themselves. The built-in toy library now carries real pad copper (a 0.8 mm
/// top-side square per pin), so the caps are pickable / draggable / routable
/// against the plain `part_library()` — the slice-A `padded_toy_lib` overlay
/// is retired.
pub fn edit_board_domain() -> DomainState {
    DomainState::from_source_with(
        BOARD_ECAD.to_string(),
        Some("board.ecad".to_string()),
        ecad_core::part::part_library(),
        |_| Vec::new(),
    )
}

/// A component drag in progress over the editing board: `C1` grabbed at its own
/// position and dragged +5 mm / +3 mm. The overlay renders the pad-shape GHOST at
/// the uncommitted position and the live RATSNEST (a line from each ghost pad to
/// the nearest other member pad of its net — C2's pads here). Nothing is
/// committed; the doc is untouched and clean.
pub fn drag_in_progress() -> EcadApp {
    use ecad_core::coord::{MM, Point};
    use ecad_core::id::EntityId;
    let app = EcadApp::new(edit_board_domain());
    let comp = EntityId::new("C1");
    let doc = app.domain.doc.as_ref().expect("edit board elaborates");
    let from = doc.components[&comp].pos.value;
    // C1 sits at (15, 3); drag left+up so the ghost stays inside the board.
    let to = Point {
        x: from.x - 5 * MM,
        y: from.y + 3 * MM,
    };
    let armed = app.set_drag(&comp, PaneId::A, to);
    debug_assert!(armed, "C1 has pad candidates");
    app
}

/// A dirty document (m6 save model): the editing board with one committed GUI
/// edit (C1 pinned 2 mm right of its solved position) and a source path wired,
/// so the chrome shows the dirty bullet on the filename badge and a primary Save
/// button. The path is inert fixture data — nothing is ever written by the scene.
pub fn dirty_doc() -> EcadApp {
    use ecad_core::command::{Command, Transaction};
    use ecad_core::coord::{MM, Point};
    use ecad_core::id::EntityId;
    let mut domain = edit_board_domain();
    domain.source_path = Some(std::path::PathBuf::from("/nonexistent/ecad/board.ecad"));
    let mut app = EcadApp::new(domain);
    let comp = EntityId::new("C1");
    let pos = app.domain.doc.as_ref().unwrap().components[&comp].pos.value;
    let target = Point {
        x: pos.x + 2 * MM,
        y: pos.y,
    };
    app.commit_edit(
        Transaction::one(Command::Pin(comp, target)),
        "move component",
    )
    .expect("fixture move commits");
    app
}

/// The conflict banner (m6 save model): the dirty doc above with an external
/// disk change waiting on the mailbox — the harness's first `before_build`
/// routes it into the pending conflict (the doc is dirty, so it is NOT applied)
/// and the persistent banner renders with the two explicit actions.
pub fn conflict_banner() -> EcadApp {
    use crate::reload::SourceMsg;
    let app = dirty_doc();
    // An externally-edited variant of the board source (a comment prepended is
    // enough — any text that differs from our own last save).
    let external = format!("# edited in $EDITOR\n{BOARD_ECAD}");
    app.mailbox_push(SourceMsg::Changed(external));
    app
}

// ---------------------------------------------------------------------------
// Milestone-6 slice-B scenes: manual trace drawing (routing ladder level 1).
// A route in progress (pending waypoints + rubber segment), a committed
// multi-waypoint trace, a layer-switched route with its via drop, and a
// trace-vertex refinement drag in progress.
// ---------------------------------------------------------------------------

/// A board point at integer-mm `(x, y)` — shorthand for the m6b scenes.
fn mm_pt(x: i64, y: i64) -> ecad_core::coord::Point {
    use ecad_core::coord::MM;
    ecad_core::coord::Point {
        x: x * MM,
        y: y * MM,
    }
}

/// A route in progress (m6 slice B): the Route tool active over the editing
/// board with a pending route started at C1's `p1` pad (net GND, active layer
/// F.Cu), two waypoints clicked, and the rubber segment tracking the last known
/// pointer position. Nothing committed; the doc is untouched and clean.
pub fn route_in_progress() -> EcadApp {
    use crate::tool::Tool;
    use ecad_core::id::EntityId;
    let app = EcadApp::new(edit_board_domain());
    app.set_tool(Tool::Route);
    let armed = app.set_route(
        &EntityId::new("C1"),
        "p1",
        &[mm_pt(10, 5), mm_pt(10, 9)],
        Some(mm_pt(12, 10)),
    );
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app
}

/// A committed multi-waypoint trace (m6 slice B): the pending route above
/// extended to C2's `p1` pad centre and committed through `commit_route` — one
/// GND trace with two interior waypoints, committed via the command layer (the
/// doc is dirty, one undo step, the new trace selected).
pub fn routed_trace() -> EcadApp {
    use ecad_core::id::EntityId;
    let mut app = EcadApp::new(edit_board_domain());
    // (14, 12) is C2.p1's pad centre (C2 sits at (15, 12); p1 offsets -1 mm).
    let armed = app.set_route(
        &EntityId::new("C1"),
        "p1",
        &[mm_pt(10, 5), mm_pt(10, 9), mm_pt(14, 12)],
        None,
    );
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app.commit_route();
    app
}

/// A layer-switched route in progress (m6 slice B, ladder level 1's "via drop
/// on layer switch"): a pending GND route with one F.Cu waypoint, the active
/// layer switched to B.Cu (dropping a through-via at the last waypoint), and a
/// further waypoint on the new layer. Still pending — the via + both runs will
/// commit together as one undo unit.
pub fn route_layer_switch() -> EcadApp {
    use crate::tool::Tool;
    use ecad_core::id::EntityId;
    let app = EcadApp::new(edit_board_domain());
    app.set_tool(Tool::Route);
    let armed = app.set_route(&EntityId::new("C1"), "p1", &[mm_pt(10, 5)], None);
    debug_assert!(armed, "C1.p1 has a pad candidate on net GND");
    app.set_active_layer("B.Cu");
    if let Some(r) = app.route.borrow_mut().as_mut() {
        r.push_waypoint(mm_pt(10, 9));
        r.hover(mm_pt(12, 10));
    }
    app
}

/// A trace-vertex refinement drag in progress (m6 slice B): the committed
/// multi-waypoint trace with its first interior vertex being dragged (Select
/// tool) — the overlay renders the vertex handles and the working-path preview;
/// nothing further is committed until release.
pub fn trace_vertex_drag() -> EcadApp {
    let app = routed_trace();
    let tid = *app
        .domain
        .doc
        .as_ref()
        .expect("routed board elaborates")
        .traces
        .keys()
        .next()
        .expect("the routed_trace scene committed a trace");
    let armed = app.set_trace_drag(tid, 1, mm_pt(8, 6));
    debug_assert!(armed, "the committed trace has an interior vertex");
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

/// The real multiprobe board loaded from `poc/out/board.ecad` with the `poc`
/// library package loaded from `poc/parts` (the same manifest-driven
/// [`ecad_core::library::load_library`] path the `poc_multiprobe` example uses —
/// the board's `use poc` directive names it). Panics on a missing/broken package —
/// this is test scaffolding, not the app path. Used by the end-to-end canvas
/// smoke test.
pub fn poc_board_domain() -> DomainState {
    let parts = poc_dir().join("parts");
    let lib = ecad_core::library::load_library(&parts)
        .unwrap_or_else(|e| panic!("load library package from {}: {e}", parts.display()));
    let path = poc_dir().join("out/board.ecad");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    DomainState::from_source_with(source, Some("board.ecad".to_string()), lib, |_| Vec::new())
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
        ("drc_violation", drc_violation()),
        ("measure_in_progress", measure_in_progress()),
        ("dual_cross_highlight", dual_cross_highlight()),
        ("stacked_layout", stacked_layout()),
        ("maximized_pane", maximized_pane()),
        ("dual_boards", dual_boards()),
        ("unresolved_libs", unresolved_libs()),
        ("libraries_menu", libraries_menu()),
        ("drag_in_progress", drag_in_progress()),
        ("dirty_doc", dirty_doc()),
        ("conflict_banner", conflict_banner()),
        ("route_in_progress", route_in_progress()),
        ("routed_trace", routed_trace()),
        ("route_layer_switch", route_layer_switch()),
        ("trace_vertex_drag", trace_vertex_drag()),
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
            Some("unresolved.ecad".to_string()),
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
        let su = ecad_core::elaborate::stackup(&doc.source);
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
        assert_eq!(t.prov, ecad_core::doc::Provenance::Pinned, "hand-routed");
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
        harness::assert_content_coverage(
            "route_layer_switch",
            &rendered,
            &[PaneId::A.canvas_key()],
        );
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
