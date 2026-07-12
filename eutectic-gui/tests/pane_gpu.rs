//! GPU-tier integration tests for the owned-canvas panes (WP2 board + WP3
//! schematic): drive the REAL windowed seams — `WinitWgpuApp::gpu_setup` +
//! `before_paint` — on a headless device and prove the §7 damage contract
//! with the paint path's own render counter: the first frame renders, idle
//! frames render **zero**, each damage input (selection/state, camera
//! request + glide) re-renders, and the settled glide goes quiet again.
//!
//! Skips loudly (not fail, not silent pass) when no adapter enumerates —
//! same policy as `render_goldens.rs`.

use damascene_core::prelude::Rect;
use eutectic_gui::host::WinitWgpuApp;
use eutectic_gui::{PaneId, fixtures, harness};

struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu(test: &str) -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    })) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "==================================================================\n\
                 SKIPPED (no GPU adapter): pane GPU test '{test}' DID NOT RUN: {e}\n\
                 This is a skip, not a pass.\n\
                 =================================================================="
            );
            return None;
        }
    };
    let info = adapter.get_info();
    println!(
        "pane_gpu '{test}': adapter {} ({:?}, {:?})",
        info.name, info.backend, info.device_type
    );
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("pane_gpu"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .expect("device on enumerated adapter");
    Some(Gpu {
        adapter,
        device,
        queue,
    })
}

/// The end-to-end damage proof on the real paint path: render counts move
/// exactly when a damage input moves, and idle frames cost zero renders.
#[test]
fn board_pane_paint_renders_once_per_damage_and_zero_idle() {
    let Some(g) = gpu("board_pane_paint") else {
        return;
    };
    let mut app = fixtures::board();
    // The host's gpu_setup seam (runner device + adapter + swapchain format).
    app.gpu_setup(
        &g.device,
        &g.queue,
        &g.adapter,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    // Settle the CPU frame loop so the board pane has a laid-out rect and a
    // fitted camera (frame 2's build captures both).
    let _ = harness::render_settled(&mut app, Rect::new(0.0, 0.0, 1280.0, 800.0));

    // First paint: the pane texture renders once.
    app.before_paint(&g.device, &g.queue);
    assert_eq!(
        app.pane_render_count(PaneId::A),
        1,
        "first paint renders the pane texture"
    );

    // Idle frames: ZERO further renders (the §7 contract, counted).
    for _ in 0..10 {
        app.before_paint(&g.device, &g.queue);
    }
    assert_eq!(
        app.pane_render_count(PaneId::A),
        1,
        "idle frames must render ZERO"
    );

    // A selection write is a one-word state change → exactly one re-render.
    app.domain
        .selection
        .borrow_mut()
        .select_only(eutectic_gui::pick::SemanticId::Net(
            eutectic_core::id::NetId::new("GND"),
        ));
    app.before_paint(&g.device, &g.queue);
    assert_eq!(
        app.pane_render_count(PaneId::A),
        2,
        "a selection change re-renders once"
    );
    for _ in 0..5 {
        app.before_paint(&g.device, &g.queue);
    }
    assert_eq!(app.pane_render_count(PaneId::A), 2, "…and goes quiet again");

    // A camera request (toolbar Reset) starts a glide: frames render while
    // the glide is live and stop at the bit-exact settle.
    {
        let cx = damascene_core::EventCx::new();
        damascene_core::App::on_event(
            &mut app,
            damascene_core::UiEvent::synthetic_click("reset"),
            &cx,
        );
    }
    // One CPU frame to consume the request in build (rect is known).
    let _ = harness::render_settled(&mut app, Rect::new(0.0, 0.0, 1280.0, 800.0));
    let before_glide = app.pane_render_count(PaneId::A);
    let mut spins = 0;
    while app.glide_active() && spins < 400 {
        std::thread::sleep(std::time::Duration::from_millis(4));
        app.before_paint(&g.device, &g.queue);
        spins += 1;
    }
    assert!(!app.glide_active(), "the glide settles");
    let settled = app.pane_render_count(PaneId::A);
    assert!(
        settled > before_glide,
        "the glide re-rendered while live ({before_glide} -> {settled})"
    );
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.before_paint(&g.device, &g.queue);
    }
    assert_eq!(
        app.pane_render_count(PaneId::A),
        settled,
        "bit-exact settle ⇒ idle renders zero again"
    );
}

/// The SCHEMATIC pane's §7 proof on the real paint path (WP3): pane B of the
/// dual fixture (schematic — with symbols, wires, net tags, i.e. real MSDF
/// text in the scene) renders once, idles at zero, re-renders exactly once
/// per selection change, and goes quiet again.
#[test]
fn schematic_pane_paint_renders_once_per_damage_and_zero_idle() {
    let Some(g) = gpu("schematic_pane_paint") else {
        return;
    };
    let mut app = fixtures::dual_cross_highlight();
    app.gpu_setup(
        &g.device,
        &g.queue,
        &g.adapter,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    let _ = harness::render_settled(&mut app, Rect::new(0.0, 0.0, 1280.0, 800.0));

    // First paint: both panes render once (board A, schematic B).
    app.before_paint(&g.device, &g.queue);
    assert_eq!(
        app.pane_render_count(PaneId::B),
        1,
        "first paint renders the schematic pane texture"
    );

    // Idle frames: ZERO further renders (the §7 contract, counted).
    for _ in 0..10 {
        app.before_paint(&g.device, &g.queue);
    }
    assert_eq!(
        app.pane_render_count(PaneId::B),
        1,
        "idle schematic frames must render ZERO"
    );

    // A selection change (hover a symbol) is a one-word state write →
    // exactly one re-render, then quiet.
    app.domain
        .selection
        .borrow_mut()
        .select_only(eutectic_gui::pick::SemanticId::Part(
            eutectic_core::id::EntityId::new("U1"),
        ));
    app.before_paint(&g.device, &g.queue);
    assert_eq!(
        app.pane_render_count(PaneId::B),
        2,
        "a selection change re-renders the schematic pane once"
    );
    for _ in 0..5 {
        app.before_paint(&g.device, &g.queue);
    }
    assert_eq!(app.pane_render_count(PaneId::B), 2, "…and goes quiet again");
}
