//! CPU-tier tests for the owned-canvas board panes (WP2): camera gestures,
//! allocation hysteresis, the instrumented damage probe, selection→state
//! mapping, overlay lowering coverage, stale-dim, and the raw-event hover
//! path — all headless, no GPU device anywhere.

use super::*;
use crate::app::EutecticApp;
use crate::app::pane::PaneId;
use crate::pick::SemanticId;
use crate::render::state::{FLAG_HOVERED, FLAG_SELECTED};
use eutectic_core::coord::MM;
use eutectic_core::id::{EntityId, NetId, TraceId, ViaId};

fn mm_pt(x: i64, y: i64) -> Point {
    Point {
        x: x * MM,
        y: y * MM,
    }
}

/// The editing fixture app (pickable pads on pane A) with pane A's rect
/// injected and its camera fitted — the headless stand-in for a laid-out
/// window, so gesture tests can run without any layout pass.
fn fitted_app() -> (EutecticApp, (f32, f32, f32, f32)) {
    let app = EutecticApp::new(crate::fixtures::edit_board_domain());
    let rect = (100.0, 50.0, 800.0, 600.0);
    app.pane_px.set([Some(rect), None]);
    let cam = app.pane_build_camera(PaneId::A, rect);
    assert!(cam.zoom > 0.0);
    (app, rect)
}

// ---------------------------------------------------------------------------
// Camera math.
// ---------------------------------------------------------------------------

/// Unproject/project round-trip through a pane rect, with the y flip: a
/// pointer BELOW the pane center maps to a board point with SMALLER y.
#[test]
fn unproject_round_trips_and_flips_y() {
    let cam = Camera::new((10.0 * MM as f64, 8.0 * MM as f64), 2e-6);
    let rect = (40.0, 30.0, 800.0, 600.0);
    let center_px = (40.0 + 400.0, 30.0 + 300.0);
    let p = pane_unproject(&cam, rect, center_px);
    assert_eq!((p.x, p.y), (10 * MM, 8 * MM), "pane center = camera center");
    let below = pane_unproject(&cam, rect, (center_px.0, center_px.1 + 100.0));
    assert!(below.y < p.y, "screen y down ⇒ board y down: {below:?}");
    let back = pane_project(&cam, rect, below);
    assert!((back.0 - center_px.0).abs() < 0.5 && (back.1 - (center_px.1 + 100.0)).abs() < 0.5);
}

#[test]
fn zoom_clamps_hold() {
    assert_eq!(clamp_zoom(1e-12), MIN_ZOOM);
    assert_eq!(clamp_zoom(1e3), MAX_ZOOM);
    assert_eq!(clamp_zoom(f64::NAN), RESET_ZOOM);
    assert_eq!(clamp_zoom(3e-6), 3e-6);
}

/// The wheel gesture's core invariance: the board point under the cursor is
/// fixed at the tick AND through every glide step until the bit-exact settle
/// (spec: "the board point under the cursor must stay fixed through the
/// whole glide, not just at the tick").
#[test]
fn zoom_at_cursor_holds_point_through_whole_glide() {
    let (app, rect) = fitted_app();
    // An off-center cursor (the interesting case — center-anchored zooms are
    // trivially fixed).
    let pos = (rect.0 + 137.0, rect.1 + 458.0);
    let before = app.pane_camera(PaneId::A);
    let anchor = pane_unproject(&before, rect, pos);

    app.pane_zoom_at(PaneId::A, rect, pos, -50.0); // one tick in
    // Through the glide: step in small increments and re-check the anchor.
    let mut cams = app.pane_cams.borrow_mut();
    let g = &mut cams[0].glide;
    let mut steps = 0;
    while !g.settled() && steps < 1000 {
        let cam = g.step(1.0 / 240.0);
        let now = pane_unproject(&cam, rect, pos);
        let err_px = (((now.x - anchor.x) as f64).hypot((now.y - anchor.y) as f64)) * cam.zoom;
        assert!(
            err_px < 1.0,
            "anchor drifted {err_px:.3} px mid-glide (step {steps})"
        );
        steps += 1;
    }
    assert!(g.settled(), "glide settles");
    let after = g.current();
    assert!(after.zoom > before.zoom, "negative dy zooms in");
    let now = pane_unproject(&after, rect, pos);
    let err_px = (((now.x - anchor.x) as f64).hypot((now.y - anchor.y) as f64)) * after.zoom;
    assert!(err_px < 1.0, "anchor fixed at settle ({err_px:.3} px)");
}

/// Successive wheel ticks compound on the TARGET zoom (continuous steps) and
/// the clamp bounds the target.
#[test]
fn wheel_ticks_compound_and_clamp() {
    let (app, rect) = fitted_app();
    let pos = (rect.0 + 400.0, rect.1 + 300.0);
    let z0 = app.pane_camera_target(PaneId::A).zoom;
    app.pane_zoom_at(PaneId::A, rect, pos, -50.0);
    let z1 = app.pane_camera_target(PaneId::A).zoom;
    app.pane_zoom_at(PaneId::A, rect, pos, -50.0);
    let z2 = app.pane_camera_target(PaneId::A).zoom;
    assert!((z1 / z0 - 1.25).abs() < 1e-9, "one 50 px tick = ×1.25");
    assert!(
        (z2 / z1 - 1.25).abs() < 1e-9,
        "ticks compound on the target"
    );
    // Hammer zoom-in far past the clamp: the target must stop at MAX_ZOOM.
    for _ in 0..200 {
        app.pane_zoom_at(PaneId::A, rect, pos, -50.0);
    }
    assert_eq!(app.pane_camera_target(PaneId::A).zoom, MAX_ZOOM);
    for _ in 0..400 {
        app.pane_zoom_at(PaneId::A, rect, pos, 50.0);
    }
    assert_eq!(app.pane_camera_target(PaneId::A).zoom, MIN_ZOOM);
}

/// Pan math: the Select-tool camera pan moves the center by −Δpx/zoom with
/// the y flip, tracking the pointer exactly.
#[test]
fn camera_pan_center_math() {
    let (app, rect) = fitted_app();
    let cam0 = app.pane_camera(PaneId::A);
    let p0 = pane_unproject(&cam0, rect, (300.0, 300.0));
    // Simulate the pointer.rs pan: start at (300,300), drag to (360, 260).
    let d = (60.0f32, -40.0f32);
    let center = (
        cam0.center.0 - d.0 as f64 / cam0.zoom,
        cam0.center.1 + d.1 as f64 / cam0.zoom,
    );
    app.pane_snap_center(PaneId::A, center);
    let cam1 = app.pane_camera(PaneId::A);
    // The board point that was under the press is now under press+Δ.
    let p1 = pane_unproject(&cam1, rect, (360.0, 260.0));
    assert!(
        (p1.x - p0.x).abs() <= 1 && (p1.y - p0.y).abs() <= 1,
        "pan must track the pointer: {p0:?} vs {p1:?}"
    );
    assert_eq!(cam1.zoom, cam0.zoom, "pan never changes zoom");
}

/// Fit / reset requests queue until a build with a known rect consumes them;
/// fit frames the scene bounds with the padding, reset restores 1 px/mm.
#[test]
fn fit_and_reset_requests_apply_in_build() {
    let (app, rect) = fitted_app();
    let bounds = app.derived.borrow().scene.as_ref().unwrap().bounds;
    let fitted = app.pane_camera(PaneId::A);
    // The initial fit framed the bounds: both extents fit inside the pane.
    let w_px = (bounds.2 - bounds.0) as f64 * fitted.zoom;
    let h_px = (bounds.3 - bounds.1) as f64 * fitted.zoom;
    assert!(w_px <= rect.2 as f64 && h_px <= rect.3 as f64);
    assert!(
        w_px >= rect.2 as f64 - 2.0 * FIT_PADDING_PX - 1.0
            || h_px >= rect.3 as f64 - 2.0 * FIT_PADDING_PX - 1.0
    );

    // Reset: request + build-consume → glide TARGET is the reset camera.
    app.request_pane_cam(PaneId::A, CamRequest::Reset);
    let _ = app.pane_build_camera(PaneId::A, rect);
    let t = app.pane_camera_target(PaneId::A);
    assert_eq!(t.zoom, RESET_ZOOM);
    // Back to fit.
    app.request_pane_cam(PaneId::A, CamRequest::Fit);
    let _ = app.pane_build_camera(PaneId::A, rect);
    let t = app.pane_camera_target(PaneId::A);
    assert!((t.zoom - fitted.zoom).abs() / fitted.zoom < 1e-9);
    // The request is consumed exactly once.
    assert!(app.pane_cams.borrow()[0].request.is_none());
}

/// Reload never re-fits: the `fitted` flag survives, so a later build leaves
/// the user's camera alone.
#[test]
fn build_camera_is_stable_once_fitted() {
    let (app, rect) = fitted_app();
    let cam0 = app.pane_camera(PaneId::A);
    let cam1 = app.pane_build_camera(PaneId::A, rect);
    assert_eq!(cam0, cam1, "no request, no re-fit — camera untouched");
}

// ---------------------------------------------------------------------------
// Texture allocation hysteresis.
// ---------------------------------------------------------------------------

/// Grow snaps to a step boundary; small oscillations inside the allocation
/// are free; shrink waits for two whole steps of slack, then snaps down.
#[test]
fn tex_alloc_hysteresis_does_not_thrash() {
    // First allocation: step-rounded.
    let a0 = tex_alloc((801, 600), None);
    assert_eq!(a0, (1024, 768));
    // Growing/shrinking inside the allocation: no change.
    assert_eq!(tex_alloc((900, 700), Some(a0)), a0);
    assert_eq!(tex_alloc((801, 600), Some(a0)), a0);
    assert_eq!(tex_alloc((600, 400), Some(a0)), a0, "shrink is lazy");
    // A resize wiggle across a step boundary and back: grows once, then
    // holds (no shrink until 2 steps of slack).
    let a1 = tex_alloc((1025, 700), Some(a0));
    assert_eq!(a1, (1280, 768));
    assert_eq!(tex_alloc((1020, 700), Some(a1)), a1, "wiggle back is free");
    assert_eq!(tex_alloc((1030, 700), Some(a1)), a1, "wiggle forth is free");
    // A genuine large shrink: ≥ 2 steps of slack snaps down.
    let a2 = tex_alloc((300, 200), Some(a1));
    assert_eq!(a2, (512, 256));
    // Degenerate sizes never produce zero.
    assert_eq!(tex_alloc((0, 0), None), (TEX_STEP, TEX_STEP));
}

// ---------------------------------------------------------------------------
// Damage (instrumented probe — the §7 contract with numbers).
// ---------------------------------------------------------------------------

/// The render-count probe: the first frame renders, each changed input
/// renders exactly once, and idle frames render **zero** — the §7 "idle
/// cost: zero GPU work" rule proven by count, not claim.
#[test]
fn damage_probe_renders_once_per_input_and_zero_idle() {
    let mut d = PaneDamage::default();
    let cam = Camera::new((1e6, 2e6), 2e-6);
    let key = |rev: u64, cam: &Camera, state: u64, overlay: u64, style: u64| {
        DamageKey::new(rev, cam, (800, 600), state, overlay, style)
    };
    assert!(d.observe(key(0, &cam, 0, 0, 0)), "first frame renders");
    assert_eq!(d.renders, 1);
    // 100 idle frames: zero further renders.
    for _ in 0..100 {
        assert!(!d.observe(key(0, &cam, 0, 0, 0)));
    }
    assert_eq!(d.renders, 1, "idle frames render ZERO");
    // Each input triggers exactly once, then goes quiet again.
    let moved = Camera::new((1e6 + 1.0, 2e6), 2e-6);
    for (i, k) in [
        key(1, &cam, 0, 0, 0),                                 // doc revision
        key(1, &moved, 0, 0, 0),                               // camera
        key(1, &moved, 1, 0, 0),                               // state generation
        key(1, &moved, 1, 1, 0),                               // overlay generation
        key(1, &moved, 1, 1, 1),                               // style/theme generation
        key(1, &moved, 1, 1, 1).with_cursor(Some([5.0, 5.0])), // cursor
    ]
    .into_iter()
    .enumerate()
    {
        assert!(d.observe(k), "input {i} renders");
        assert!(!d.observe(k), "input {i} then goes quiet");
    }
    assert_eq!(d.renders, 7);
    // Texture realloc invalidates: same key renders once more.
    let k = key(1, &moved, 1, 1, 1).with_cursor(Some([5.0, 5.0]));
    d.invalidate();
    assert!(d.observe(k));
    assert_eq!(d.renders, 8);
}

/// A glide produces damage while live and goes bit-exact quiet at settle:
/// the settled camera yields an identical key every subsequent frame.
#[test]
fn glide_settle_is_damage_quiet() {
    let mut g = CameraGlide::new(Camera::new((0.0, 0.0), 1e-6));
    g.retarget(Camera::new((5.0 * MM as f64, 3.0 * MM as f64), 2e-6));
    let mut d = PaneDamage::default();
    let mut live_renders = 0;
    for _ in 0..500 {
        let cam = g.step(1.0 / 120.0);
        if d.observe(DamageKey::new(0, &cam, (800, 600), 0, 0, 0)) {
            live_renders += 1;
        }
        if g.settled() {
            break;
        }
    }
    assert!(g.settled());
    assert!(live_renders > 1, "the glide re-rendered while live");
    // Settled: every further frame is damage-quiet.
    let renders_at_settle = d.renders;
    for _ in 0..50 {
        let cam = g.step(1.0 / 120.0);
        d.observe(DamageKey::new(0, &cam, (800, 600), 0, 0, 0));
    }
    assert_eq!(d.renders, renders_at_settle, "bit-exact settle ⇒ zero idle");
}

// ---------------------------------------------------------------------------
// Selection → semantic state words.
// ---------------------------------------------------------------------------

/// The HighlightSets → state-word mapping: net-keyed scene ids light when
/// their net is in the expanded set (net expansion = the old overlay's
/// `board_matches`), netless copper by its own id, chrome/board never; the
/// selected and hovered sets flag independent bits.
#[test]
fn state_words_map_highlight_sets() {
    let comp = EntityId::new("C1");
    let semantics = vec![
        SemanticKey::Chrome,
        SemanticKey::Net(NetId::new("GND")),
        SemanticKey::Net(NetId::new("VBUS")),
        SemanticKey::Trace(TraceId(7)),
        SemanticKey::Via(ViaId(3)),
        SemanticKey::Pin {
            comp: comp.clone(),
            pad: "p1".into(),
        },
        SemanticKey::Part(comp.clone()),
        SemanticKey::Board,
    ];
    let mut sel = HighlightSets::default();
    sel.nets.insert(NetId::new("GND"));
    sel.board.insert(SemanticId::Trace(TraceId(7)));
    sel.board.insert(SemanticId::Part(comp.clone()));
    let mut hov = HighlightSets::default();
    hov.nets.insert(NetId::new("VBUS"));
    hov.board.insert(SemanticId::Pin {
        comp: comp.clone(),
        pin: "p1".into(),
    });

    let words = board_state_words(&semantics, &sel, &hov);
    assert_eq!(words[0], 0, "chrome never flags");
    assert_eq!(words[1], FLAG_SELECTED, "selected net lights its scene key");
    assert_eq!(words[2], FLAG_HOVERED, "hovered net lights hovered only");
    assert_eq!(words[3], FLAG_SELECTED, "netless trace by its own id");
    assert_eq!(words[4], 0, "unselected via stays dark");
    assert_eq!(words[5], FLAG_HOVERED, "pin from the hover set");
    assert_eq!(words[6], FLAG_SELECTED, "part body from the selected set");
    assert_eq!(words[7], 0, "the board itself never flags");
}

/// End-to-end through the app: selecting a net in the shared selection model
/// flags the scene's net id in the state buffer via `sync_board_states`, and
/// clearing goes quiet (generation only moves on real change).
#[test]
fn sync_board_states_writes_selection_flags() {
    let (app, _rect) = fitted_app();
    let net = NetId::new("GND");
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(net.clone()));
    app.sync_board_states();
    let derived = app.derived.borrow();
    let scene = derived.scene.as_ref().unwrap();
    let id = scene
        .semantics
        .iter()
        .position(|k| *k == SemanticKey::Net(net.clone()))
        .expect("GND is in the scene semantics") as u32;
    let states = derived.states.borrow();
    assert_eq!(states.word(id), FLAG_SELECTED);
    let gen0 = states.generation();
    drop(states);
    drop(derived);
    // Idempotent sync: no generation churn.
    app.sync_board_states();
    assert_eq!(app.derived.borrow().states.borrow().generation(), gen0);
    // Clear: the flag drops, one generation step.
    app.domain.selection.borrow_mut().clear();
    app.sync_board_states();
    let derived = app.derived.borrow();
    let states = derived.states.borrow();
    assert_eq!(states.word(id), 0);
    assert!(states.generation() > gen0);
}

// ---------------------------------------------------------------------------
// Overlay lowering.
// ---------------------------------------------------------------------------

/// Every Overlay field with geometry produces primitives (and the ignored
/// `highlights` field produces none — it rides the state buffer instead).
#[test]
fn overlay_lowering_covers_every_field() {
    let zoom = 2e-6;
    let count = |o: &Overlay| overlay_prims(o, zoom).len();
    let empty = Overlay::default();
    assert_eq!(count(&empty), 0, "empty overlay lowers to nothing");

    let one = |f: &dyn Fn(&mut Overlay)| {
        let mut o = empty.clone();
        f(&mut o);
        count(&o)
    };
    assert!(one(&|o| o.measure = Some((mm_pt(1, 1), mm_pt(5, 4)))) >= 2);
    assert!(one(&|o| o.findings = vec![(mm_pt(3, 3), true)]) >= 1);
    assert!(one(&|o| o.ghost = vec![eutectic_core::geom::Shape2D::disc(mm_pt(2, 2), MM / 2)]) >= 1);
    assert!(one(&|o| o.ratsnest = vec![(mm_pt(0, 0), mm_pt(4, 4))]) >= 1);
    assert!(
        one(&|o| o.route_runs = vec![(vec![mm_pt(0, 0), mm_pt(3, 0), mm_pt(3, 3)], 150_000)]) >= 2
    );
    assert!(one(&|o| o.route_rubber = Some((mm_pt(3, 3), mm_pt(6, 3)))) >= 1);
    assert!(one(&|o| o.route_vias = vec![(mm_pt(3, 0), 300_000)]) >= 1);
    assert!(one(&|o| o.edit_path = Some((vec![mm_pt(0, 0), mm_pt(2, 2)], 150_000))) >= 1);
    assert!(one(&|o| o.handles = vec![mm_pt(0, 0), mm_pt(2, 2)]) >= 2);
    // (Selection/hover highlight geometry no longer exists as an overlay
    // channel at all — emphasis rides the semantic state buffer, spec §5.)
    // Content equality gates the GPU upload: identical overlays lower to
    // identical prims (the overlay generation stays put).
    let mut a = empty.clone();
    a.measure = Some((mm_pt(1, 1), mm_pt(5, 4)));
    assert_eq!(overlay_prims(&a, zoom), overlay_prims(&a.clone(), zoom));
}

// ---------------------------------------------------------------------------
// Stale dim + style generation.
// ---------------------------------------------------------------------------

/// The stale composite treatment dims every plane (and the overlay), and the
/// stale bit moves the style damage input so the swap re-renders exactly once.
#[test]
fn stale_dim_state_machine() {
    let (app, _rect) = fitted_app();
    let g0 = app.style_gen();
    assert_eq!(g0 % 2, 0, "fresh doc: stale bit clear");
    // A failed reload keeps the last-good doc + sets reload_error (a
    // malformed `inst` missing its part token is a genuine syntax fault).
    app.mailbox_push(crate::reload::SourceMsg::Changed(
        "inst U1\nnet GND U1.GND\n".to_string(),
    ));
    let mut app = app;
    damascene_core::App::before_build(&mut app);
    assert!(
        app.domain.reload_error.is_some(),
        "reload failed as intended"
    );
    let g1 = app.style_gen();
    assert_ne!(g0, g1, "going stale damages the pane");
    assert_eq!(g1 % 2, 1, "stale bit set");

    // The dim itself: every plane's dim shrinks.
    let derived = app.derived.borrow();
    let scene = derived.scene.as_ref().unwrap();
    let tables = StyleTables::board_defaults(true);
    let mut styles = tables.resolve(scene, None);
    let before: Vec<f32> = styles.planes.iter().map(|p| p.dim).collect();
    stale_dim(&mut styles);
    for (p, b) in styles.planes.iter().zip(before) {
        assert!((p.dim - b * STALE_DIM).abs() < 1e-6);
    }
    assert!((styles.overlay.dim - STALE_DIM).abs() < 1e-6);
}

/// Layer visibility maps scene planes onto the layer panel's keys (substrate
/// and outline share the outline toggle; drills/overlay have none).
#[test]
fn plane_layer_keys_map_to_panel_toggles() {
    assert_eq!(
        plane_layer_key(&PlaneKey::Copper("F.Cu".into())).as_deref(),
        Some("layer:F.Cu")
    );
    assert_eq!(
        plane_layer_key(&PlaneKey::CopperPour("F.Cu".into())).as_deref(),
        Some("layer:F.Cu")
    );
    assert_eq!(
        plane_layer_key(&PlaneKey::Silk("F.SilkS".into())).as_deref(),
        Some("layer:F.SilkS")
    );
    assert_eq!(
        plane_layer_key(&PlaneKey::Substrate).as_deref(),
        Some("layer:outline")
    );
    assert_eq!(
        plane_layer_key(&PlaneKey::Outline).as_deref(),
        Some("layer:outline")
    );
    assert_eq!(plane_layer_key(&PlaneKey::Drills), None);
    assert_eq!(plane_layer_key(&PlaneKey::Overlay), None);
}

// ---------------------------------------------------------------------------
// Raw-event hover / pane resolution / middle pan.
// ---------------------------------------------------------------------------

/// The board pane resolves by rect (maximize honored, strip excluded), and
/// free hover picks through the camera with the correct y orientation:
/// pointing at a pad hovers exactly that pad's id.
#[test]
fn free_hover_resolves_pane_and_picks_with_y_flip() {
    let (mut app, rect) = fitted_app();
    // A pad candidate's center, mapped board → screen through the camera.
    let comp = EntityId::new("C1");
    let (pad_id, center) = {
        let derived = app.derived.borrow();
        let view = derived.board.as_ref().unwrap();
        let c = view
            .candidates
            .iter()
            .find(|c| matches!(&c.id, SemanticId::Pin { comp: cc, .. } if *cc == comp))
            .expect("C1 has a pad candidate");
        (
            c.id.clone(),
            Point {
                x: (c.aabb.0.x + c.aabb.1.x) / 2,
                y: (c.aabb.0.y + c.aabb.1.y) / 2,
            },
        )
    };
    let cam = app.pane_camera(PaneId::A);
    let px = pane_project(&cam, rect, center);
    assert!(
        app.raw_cursor_moved(px),
        "hover over the pane needs a redraw"
    );
    assert_eq!(
        app.domain.selection.borrow().hovered().next(),
        Some(&pad_id),
        "free hover picked the pad under the cursor"
    );
    // The crosshair tracked (pane-local px).
    let cross = app.cursor_px.get()[0].expect("crosshair set");
    assert!((cross.0 - (px.0 - rect.0)).abs() < 1e-3);

    // Moving to empty board clears the pick but keeps the crosshair.
    let off = pane_project(&cam, rect, mm_pt(0, 0));
    app.raw_cursor_moved((off.0.max(rect.0 + 1.0), off.1.min(rect.1 + rect.3 - 1.0)));
    assert!(app.domain.selection.borrow().hovered().next().is_none());
    assert!(app.cursor_px.get()[0].is_some());

    // Hover again, then leave the pane (chrome): hover clears, crosshair off.
    app.raw_cursor_moved(px);
    assert!(app.domain.selection.borrow().hovered().next().is_some());
    assert!(app.raw_cursor_moved((rect.0 - 20.0, rect.1 - 20.0)));
    assert!(
        app.domain.selection.borrow().hovered().next().is_none(),
        "hover clears on leaving the pane"
    );
    assert_eq!(app.cursor_px.get(), [None, None]);

    // CursorLeft (window) also clears.
    app.raw_cursor_moved(px);
    app.raw_cursor_left();
    assert!(app.domain.selection.borrow().hovered().next().is_none());
}

/// No hover churn during an active drag: with the primary button down (or an
/// armed gesture), pointer motion never rewrites hover flags.
#[test]
fn free_hover_is_suppressed_during_drags() {
    let (mut app, rect) = fitted_app();
    let comp = EntityId::new("C1");
    let center = {
        let derived = app.derived.borrow();
        let view = derived.board.as_ref().unwrap();
        let c = view
            .candidates
            .iter()
            .find(|c| matches!(&c.id, SemanticId::Pin { comp: cc, .. } if *cc == comp))
            .unwrap();
        Point {
            x: (c.aabb.0.x + c.aabb.1.x) / 2,
            y: (c.aabb.0.y + c.aabb.1.y) / 2,
        }
    };
    let px = pane_project(&app.pane_camera(PaneId::A), rect, center);
    app.raw.borrow_mut().primary_down = true;
    app.raw_cursor_moved(px);
    assert!(
        app.domain.selection.borrow().hovered().next().is_none(),
        "no hover writes while the primary button is down"
    );
}

/// Hover respects modal chrome (Libraries modal / open menu own the pointer)
/// and the strip rect (a pointer over the floating strip is chrome).
#[test]
fn free_hover_respects_modals_and_strip() {
    let (mut app, rect) = fitted_app();
    let center = (rect.0 + rect.2 / 2.0, rect.1 + rect.3 / 2.0);
    // The strip occupies the pane's top-left corner.
    app.strip_px
        .set([Some((rect.0, rect.1, 60.0, 200.0)), None]);
    assert!(
        app.raw_pane_at((rect.0 + 10.0, rect.1 + 10.0)).is_none(),
        "the strip is chrome, not canvas"
    );
    assert!(app.raw_pane_at(center).is_some());
    // Modal open: no hover, no crosshair.
    app.set_libraries_open(true);
    app.raw_cursor_moved(center);
    assert_eq!(app.cursor_px.get(), [None, None]);
    app.set_libraries_open(false);
    // Maximizing pane B hides pane A from resolution.
    app.set_maximized(Some(PaneId::B));
    assert!(app.raw_pane_at(center).is_none());
}

/// Middle-drag pan: press arms on the board pane, motion pans the camera by
/// exactly Δpx/zoom (y flipped), release disarms. Left stays select — the
/// selection model is untouched throughout.
#[test]
fn middle_drag_pans_camera() {
    let (mut app, rect) = fitted_app();
    let start = (rect.0 + 400.0, rect.1 + 300.0);
    app.raw_cursor_moved(start);
    assert!(app.raw_middle(true), "middle press arms over a board pane");
    let cam0 = app.pane_camera(PaneId::A);
    let to = (start.0 + 120.0, start.1 - 80.0);
    assert!(app.raw_cursor_moved(to), "pan motion needs a redraw");
    let cam1 = app.pane_camera(PaneId::A);
    assert!((cam1.center.0 - (cam0.center.0 - 120.0 / cam0.zoom)).abs() < 1.0);
    assert!((cam1.center.1 - (cam0.center.1 - 80.0 / cam0.zoom)).abs() < 1.0);
    assert_eq!(cam1.zoom, cam0.zoom);
    assert!(app.raw_middle(false), "release disarms");
    assert!(app.raw.borrow().middle_pan.is_none());
    assert!(
        app.domain.selection.borrow().selected().next().is_none(),
        "middle pan never selects"
    );
}
