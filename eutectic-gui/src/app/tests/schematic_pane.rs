//! WP3 schematic-pane tests: the owned-canvas gestures + pick over the REAL
//! damascene input pass, the issue-0035 dissolution proofs, cross-view
//! highlighting through the semantic state buffers (both directions), the
//! findings-halo state flags, and the schematic pane's CPU damage probe.
//!
//! Issue 0035's two residuals were structural to the viewport path:
//! 1. schematic panes could pan only in the gutter (the keyed content El
//!    suppressed the native gesture over content, and there was no app-side
//!    camera pan);
//! 2. a click in the gutter never reached the app (native pan captured the
//!    press), so empty-space deselect silently did nothing.
//!
//! On the owned canvas both dissolve by construction — the pane is one keyed
//! container, every press reaches the app, and the Select-tool camera pan is
//! the app's own gesture. The tests below prove it with synthesized input.

use super::camera::Native;
use super::*;
use crate::app::canvas_pane::{PaneDamage, pane_project, pane_unproject};
use crate::render::DamageKey;
use crate::render::scene::SemanticKey;
use crate::render::state::{FLAG_EMPHASIS, FLAG_SELECTED};
use eutectic_core::id::NetId;

/// The schematic-first app: pane A shows the schematic (so the Native
/// harness's pane-A helpers drive it), pane B the board — same doc as the
/// dual cross-highlight fixture.
fn schematic_app() -> EutecticApp {
    let app = EutecticApp::new(schematic_domain());
    app.set_pane_views(ViewKind::Schematic, ViewKind::Board);
    app
}

/// A schematic-space point on U1's body center (clear of its pins).
fn u1_center(app: &EutecticApp) -> Point {
    let doc = app.domain.doc.as_ref().unwrap();
    let placements = doc.reflow_schematic(&app.domain.lib);
    placements[&EntityId::new("U1")].center
}

/// Map a schematic point to pane-A screen px through the pane camera.
fn px_of_schematic(app: &EutecticApp, n: &Native, p: Point) -> (f32, f32) {
    let rect = n.rect_a();
    let cam = app.pane_camera(PaneId::A);
    pane_project(&cam, (rect.x, rect.y, rect.w, rect.h), p)
}

/// ISSUE 0035 RESIDUAL 1 DISSOLVED: with the Select tool, a drag that starts
/// over schematic CONTENT (a symbol body — the exact spot the old viewport
/// path could never pan from) pans the camera, tracking the pointer; the
/// release commits nothing and the trailing Click is eaten.
#[test]
fn select_drag_over_schematic_content_pans() {
    let mut app = schematic_app();
    let mut n = Native::settled(&mut app);
    let cam0 = app.pane_camera(PaneId::A);
    let from = px_of_schematic(&app, &n, u1_center(&app));
    let to = (from.0 + 60.0, from.1 + 40.0);

    n.press(&mut app, from);
    n.move_to(&mut app, to);
    let cam1 = app.pane_camera(PaneId::A);
    assert!(
        (cam1.center.0 - (cam0.center.0 - 60.0 / cam0.zoom)).abs() * cam0.zoom < 1.0
            && (cam1.center.1 - (cam0.center.1 + 40.0 / cam0.zoom)).abs() * cam0.zoom < 1.0,
        "a drag from a SYMBOL BODY must pan the schematic camera \
         (center {:?} -> {:?})",
        cam0.center,
        cam1.center
    );
    assert_eq!(cam1.zoom, cam0.zoom, "a pan never changes zoom");

    n.release(&mut app, to);
    assert!(
        app.domain.selection.borrow().single().is_none(),
        "the trailing Click of a pan must not select the symbol under the drop"
    );
    assert!(!app.dirty(), "a camera pan commits nothing");
}

/// ISSUE 0035 RESIDUAL 2 DISSOLVED (+ click-select preserved): an un-moved
/// click on a symbol selects it; an un-moved click on empty schematic space
/// (the gutter the old native pan used to swallow) CLEARS the selection.
#[test]
fn schematic_click_selects_and_empty_click_deselects() {
    let mut app = schematic_app();
    let mut n = Native::settled(&mut app);

    // Click U1's body center: selects the part (pin ▸ wire ▸ symbol
    // priority — the center is clear of pins and wires).
    let body_px = px_of_schematic(&app, &n, u1_center(&app));
    n.press(&mut app, body_px);
    n.release(&mut app, body_px);
    assert_eq!(
        app.domain.selection.borrow().single(),
        Some(&SemanticId::Part(EntityId::new("U1"))),
        "an un-moved click on a symbol body selects the part"
    );

    // Click the pane's bottom-left corner — inside the pane, outside every
    // candidate (verified against the pick itself, not assumed).
    let rect = n.rect_a();
    let empty_px = (rect.x + 8.0, rect.y + rect.h - 8.0);
    {
        let cam = app.pane_camera(PaneId::A);
        let p = pane_unproject(&cam, (rect.x, rect.y, rect.w, rect.h), empty_px);
        let derived = app.derived.borrow();
        assert!(
            crate::schematic_pick::resolve(&derived.schematic_picks, p, 0).is_none(),
            "test point must be empty schematic space"
        );
    }
    n.press(&mut app, empty_px);
    n.release(&mut app, empty_px);
    assert!(
        app.domain.selection.borrow().single().is_none(),
        "an empty-space click DESELECTS (0035 residual 2: the click reaches \
         the app — no native pan swallows it)"
    );
}

/// Wheel over a schematic pane is the owned camera's zoom-at-cursor — the
/// WP3 twin of the board wheel test (pre-WP3 the schematic wheel fell
/// through to the damascene viewport; now the app consumes it and holds the
/// content point under the cursor through the whole glide).
#[test]
fn wheel_over_schematic_zooms_at_cursor() {
    let mut app = schematic_app();
    let n = Native::settled(&mut app);
    let rect = n.rect_a();
    let pos = (rect.x + rect.w * 0.7, rect.y + rect.h * 0.3);
    let cam0 = app.pane_camera(PaneId::A);
    let anchor = pane_unproject(&cam0, (rect.x, rect.y, rect.w, rect.h), pos);

    let mut e = UiEvent::synthetic_click(PaneId::A.canvas_key());
    e.kind = UiEventKind::PointerWheel;
    e.pointer = Some(pos);
    e.wheel_delta = Some((0.0, -50.0));
    let cx = EventCx::new()
        .with_ui_state(&n.rt.ui_state)
        .with_viewport(1280.0, 800.0);
    assert!(
        app.on_wheel_event(e, &cx),
        "wheel over a schematic pane is consumed by the owned camera (WP3)"
    );
    {
        let mut cams = app.pane_cams.borrow_mut();
        while !cams[0].glide.settled() {
            cams[0].glide.step(1.0 / 120.0);
        }
    }
    let cam1 = app.pane_camera(PaneId::A);
    assert!(cam1.zoom > cam0.zoom, "scroll up zooms in");
    let now = pane_unproject(&cam1, (rect.x, rect.y, rect.w, rect.h), pos);
    let err_px = (((now.x - anchor.x) as f64).hypot((now.y - anchor.y) as f64)) * cam1.zoom;
    assert!(
        err_px < 1.0,
        "the schematic point under the cursor survives the whole zoom \
         ({err_px:.2} px off)"
    );
}

/// Middle-drag pan works over a schematic pane through the raw seam (the
/// view-generic gesture): press arms, motion pans by Δpx/zoom, release
/// disarms — and no crosshair appears (schematic furniture parity).
#[test]
fn middle_drag_pans_schematic_pane() {
    let mut app = schematic_app();
    let rect = (100.0, 50.0, 800.0, 600.0);
    app.pane_px.set([Some(rect), None]);
    let _ = app.pane_build_camera(PaneId::A, rect);

    let start = (rect.0 + 400.0, rect.1 + 300.0);
    app.raw_cursor_moved(start);
    assert_eq!(
        app.cursor_px.get(),
        [None, None],
        "no crosshair over a schematic pane"
    );
    assert!(app.raw_middle(true), "middle press arms over the pane");
    let cam0 = app.pane_camera(PaneId::A);
    let to = (start.0 + 120.0, start.1 - 80.0);
    assert!(app.raw_cursor_moved(to), "pan motion needs a redraw");
    let cam1 = app.pane_camera(PaneId::A);
    assert!((cam1.center.0 - (cam0.center.0 - 120.0 / cam0.zoom)).abs() < 1.0);
    assert!((cam1.center.1 - (cam0.center.1 - 80.0 / cam0.zoom)).abs() < 1.0);
    assert!(app.raw_middle(false), "release disarms");
}

/// Free hover over the schematic pane picks through the pane camera: hovering
/// a symbol body sets the hover flag; moving to empty space clears it.
#[test]
fn schematic_free_hover_picks_symbols() {
    let mut app = schematic_app();
    let rect = (100.0, 50.0, 800.0, 600.0);
    app.pane_px.set([Some(rect), None]);
    let _ = app.pane_build_camera(PaneId::A, rect);

    let cam = app.pane_camera(PaneId::A);
    let body_px = pane_project(&cam, rect, u1_center(&app));
    assert!(app.raw_cursor_moved(body_px));
    assert_eq!(
        app.domain.selection.borrow().hovered().next(),
        Some(&SemanticId::Part(EntityId::new("U1"))),
        "free hover picked the symbol under the cursor"
    );
    // Empty space clears the hover.
    app.raw_cursor_moved((rect.0 + 8.0, rect.1 + rect.3 - 8.0));
    assert!(app.domain.selection.borrow().hovered().next().is_none());
}

/// Cross-view highlight flows BOTH directions through the semantic state
/// buffers: a schematic-side selection (a pin, via the schematic pick)
/// flags the board scene's matching keys, and a board-side selection (a
/// pour) flags the schematic scene's net key — the same one-word writes both
/// panes' renders observe (spec §5).
#[test]
fn cross_highlight_flows_both_ways_through_state_buffers() {
    let app = schematic_app();

    // Schematic → board: select the VDD net by picking the schematic wire
    // (a wire's selectable identity IS its net — the cross-view currency).
    let wire_net = NetId::new("VDD");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(wire_net.clone()));
    app.sync_board_states();
    {
        let derived = app.derived.borrow();
        let scene = derived.scene.as_ref().expect("board scene");
        let id = scene
            .semantics
            .iter()
            .position(|k| *k == SemanticKey::Net(wire_net.clone()))
            .expect("VDD keys board copper") as u32;
        assert_eq!(
            derived.states.borrow().word(id) & FLAG_SELECTED,
            FLAG_SELECTED,
            "a schematic-side net selection lights the board scene's net key"
        );
    }

    // Board → schematic: select the GND pour (board-only geometry); its NET
    // expands into the schematic set, flagging the schematic scene's key.
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Pour {
            net: NetId::new("GND"),
            layer: "F.Cu".into(),
        });
    app.sync_schematic_states();
    {
        let derived = app.derived.borrow();
        let scene = derived.schematic_scene.as_ref().expect("schematic scene");
        let id = scene
            .semantics
            .iter()
            .position(|k| *k == SemanticKey::Net(NetId::new("GND")))
            .expect("GND keys schematic wires/tags") as u32;
        assert_eq!(
            derived.schematic_states.borrow().word(id) & FLAG_SELECTED,
            FLAG_SELECTED,
            "a board-side pour selection lights the schematic scene's net key"
        );
    }
}

/// Findings halo the schematic through the STATE BUFFER (the old overlay
/// rings' replacement): a deliberate clearance violation between two nets on
/// a doc with a schematic block flags FLAG_EMPHASIS on those nets' schematic
/// keys after a sync — no selection involved.
#[test]
fn findings_refs_emphasize_schematic_nets() {
    use eutectic_core::command::Command;
    use eutectic_core::doc::Provenance;
    use eutectic_core::id::TraceId;
    use eutectic_core::route::Trace;

    // The schematic fixture's doc plus two clashing traces on its two nets
    // (the DRC fixture's recipe: 0.25 mm traces, 0.3 mm apart → 0.05 mm gap).
    let src = crate::fixtures::SCHEMATIC_ECAD.to_string();
    let d = DomainState::from_source_with(src, None, eutectic_core::part::part_library(), |_| {
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
        vec![trace(1, "VDD", 7_000_000), trace(2, "GND", 7_300_000)]
    });
    let app = EutecticApp::new(d);
    let f = app.findings();
    let clearance = f
        .items
        .iter()
        .find(|i| i.code == "E_DRC_CLEARANCE")
        .expect("the clash flags a clearance finding");
    assert!(
        clearance.refs.contains(&SemanticId::Net(NetId::new("VDD"))),
        "the finding refs the nets: {:?}",
        clearance.refs
    );

    app.sync_schematic_states();
    let derived = app.derived.borrow();
    let scene = derived.schematic_scene.as_ref().expect("schematic scene");
    let id = scene
        .semantics
        .iter()
        .position(|k| *k == SemanticKey::Net(NetId::new("VDD")))
        .expect("VDD keys schematic wires/tags") as u32;
    assert_eq!(
        derived.schematic_states.borrow().word(id) & FLAG_EMPHASIS,
        FLAG_EMPHASIS,
        "finding refs light the schematic net through the state buffer"
    );
}

/// The schematic pane's CPU damage probe: with the schematic scene's state
/// generation as the damage input, idle syncs are generation-quiet and idle
/// frames render ZERO; a selection change renders exactly once, then goes
/// quiet again — the same §7 numbers-not-claims proof the board pane has.
#[test]
fn schematic_pane_damage_probe_zero_idle() {
    let app = schematic_app();
    let cam = crate::render::Camera::new((0.0, 0.0), 1e-6);
    let mut d = PaneDamage::default();
    let key = |app: &EutecticApp| {
        let derived = app.derived.borrow();
        let g = derived.schematic_states.borrow().generation();
        DamageKey::new(app.domain.revision, &cam, (800, 600), g, 0, app.style_gen())
    };

    app.sync_schematic_states();
    assert!(d.observe(key(&app)), "first frame renders");
    // 50 idle frames: sync is idempotent (no generation churn), damage quiet.
    for _ in 0..50 {
        app.sync_schematic_states();
        assert!(!d.observe(key(&app)), "idle frames render ZERO");
    }
    assert_eq!(d.renders, 1);

    // A selection write: exactly one re-render, then quiet.
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("VDD")));
    app.sync_schematic_states();
    assert!(d.observe(key(&app)), "the selection change renders once");
    for _ in 0..10 {
        app.sync_schematic_states();
        assert!(!d.observe(key(&app)));
    }
    assert_eq!(d.renders, 2);
}
