//! GPU golden-image tests for the owned-canvas renderer (renderer-spec §10
//! tier 2, ruling gw-25): small hand-built scenes, one per shader feature,
//! rendered headless and compared against committed PNGs with tolerance
//! (drivers differ on AA edges — the goldens guard *how* scenes rasterize;
//! the SVG + lint bundle stays the semantic authority).
//!
//! - **No adapter ⇒ loud skip** (not a failure, not a silent pass): this
//!   machine class has hardware Vulkan ICDs and no lavapipe; a CI box
//!   without any adapter prints a SKIPPED banner per test.
//! - **Regeneration**: `EUTECTIC_GOLDEN_REGEN=1 cargo test -p eutectic-gui
//!   --test render_goldens` rewrites the PNGs under `tests/goldens/`.
//! - The adapter name/backend and the negotiated coverage config are
//!   printed per run (`--nocapture` shows them) and embedded in failure
//!   messages.

use eutectic_core::coord::{MM, Point};
use eutectic_gui::render::Camera;
use eutectic_gui::render::gpu::{RenderArgs, Renderer, SceneBuffers};
use eutectic_gui::render::scene::{Plane, PlaneKey, Prim, PrimShape, Scene, SemanticKey};
use eutectic_gui::render::state::{FLAG_SELECTED, SemanticStates};
use eutectic_gui::render::style::StyleTables;

const W: u32 = 256;
const H: u32 = 192;

struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    label: String,
}

/// Acquire a headless device, or `None` after a LOUD skip banner.
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
                 SKIPPED (no GPU adapter): golden test '{test}' DID NOT RUN: {e}\n\
                 The renderer goldens need a Vulkan/Metal/DX12 adapter (hardware\n\
                 or lavapipe). This is a skip, not a pass.\n\
                 =================================================================="
            );
            return None;
        }
    };
    let info = adapter.get_info();
    let label = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
    println!("golden '{test}': adapter {label}");
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("render_goldens"),
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
        label,
    })
}

fn mm_pt(x: f64, y: f64) -> Point {
    Point {
        x: (x * MM as f64) as i64,
        y: (y * MM as f64) as i64,
    }
}

/// A minimal scene: 20 × 15 mm bounds, the given planes, `nets` extra
/// semantic ids after the chrome sentinel.
fn scene(planes: Vec<Plane>, nets: usize) -> Scene {
    let mut semantics = vec![SemanticKey::Chrome];
    for i in 0..nets {
        semantics.push(SemanticKey::Net(eutectic_core::id::NetId::new(format!(
            "N{i}"
        ))));
    }
    Scene {
        anchor: mm_pt(10.0, 7.5),
        bounds: (0, 0, 20 * MM, 15 * MM),
        planes,
        semantics,
    }
}

fn fit_camera(s: &Scene) -> Camera {
    Camera::fit(s.bounds, (W as f64, H as f64), 10.0)
}

/// Render `scene` and read the pixels back (RGBA8, row-major).
fn render_scene(
    gpu: &Gpu,
    s: &Scene,
    state: &SemanticStates,
    styles: &eutectic_gui::render::style::ResolvedStyles,
    camera: &Camera,
    cursor: Option<[f32; 2]>,
) -> Vec<u8> {
    let mut renderer = Renderer::new(&gpu.device, &gpu.adapter, wgpu::TextureFormat::Rgba8Unorm);
    println!("  coverage: {:?}", renderer.coverage());
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("golden.target"),
        size: wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let buffers = SceneBuffers::build(&gpu.device, s);
    renderer.render(
        &gpu.device,
        &gpu.queue,
        &RenderArgs {
            scene: &buffers,
            overlay: None,
            camera,
            styles,
            state,
            target: &view,
            size: (W, H),
            cursor_px: cursor,
        },
    );
    // Read back.
    let bytes_per_row = W * 4; // 1024, already 256-aligned
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("golden.readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        tx.send(r).ok();
    });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll");
    rx.recv().expect("map callback").expect("map readback");
    let data = slice.get_mapped_range().to_vec();
    drop(readback);
    data
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/goldens")
        .join(format!("{name}.png"))
}

/// Compare against (or regenerate) the committed golden with tolerance:
/// AA/rasterization differences between drivers show up as a small
/// population of edge pixels with moderate deltas — allow ≤ 1 % of pixels
/// past a per-channel delta of 12, none past 64, mean delta ≤ 2.
fn check_golden(name: &str, data: &[u8], adapter: &str) {
    let path = golden_path(name);
    if std::env::var_os("EUTECTIC_GOLDEN_REGEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        println!("golden '{name}': REGENERATED on {adapter}");
        return;
    }
    let file = std::fs::File::open(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {path:?} ({e}); regenerate with \
             EUTECTIC_GOLDEN_REGEN=1 (adapter: {adapter})"
        )
    });
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .expect("decode golden");
    let mut want = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut want).expect("read golden frame");
    want.truncate(info.buffer_size());
    assert_eq!(
        (info.width, info.height),
        (W, H),
        "golden {name} has stale dimensions; regenerate"
    );
    assert_eq!(want.len(), data.len());
    let mut over12 = 0usize;
    let mut max_d = 0u8;
    let mut sum = 0u64;
    for (a, b) in data.iter().zip(&want) {
        let d = a.abs_diff(*b);
        sum += d as u64;
        max_d = max_d.max(d);
        if d > 12 {
            over12 += 1;
        }
    }
    let total = data.len();
    let frac = over12 as f64 / total as f64;
    let mean = sum as f64 / total as f64;
    assert!(
        max_d <= 64 && frac <= 0.01 && mean <= 2.0,
        "golden '{name}' mismatch on {adapter}: max Δ{max_d}, {:.3}% channels past Δ12, mean Δ{mean:.3}\n\
         (regenerate with EUTECTIC_GOLDEN_REGEN=1 if the change is intended)",
        frac * 100.0
    );
}

fn px(data: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [data[i], data[i + 1], data[i + 2], data[i + 3]]
}

fn styles_for(s: &Scene) -> eutectic_gui::render::style::ResolvedStyles {
    StyleTables::board_defaults(true).resolve(s, None)
}

// ---------------------------------------------------------------------------
// The goldens.
// ---------------------------------------------------------------------------

#[test]
fn capsule_aa_cross_section() {
    let Some(gpu) = gpu("capsule_aa") else { return };
    let s = scene(
        vec![Plane {
            key: PlaneKey::Copper("F.Cu".into()),
            prims: vec![Prim::fill(
                1,
                PrimShape::Capsule {
                    a: mm_pt(3.0, 3.0),
                    b: mm_pt(17.0, 12.0),
                    r: 800_000,
                },
            )],
        }],
        1,
    );
    let cam = fit_camera(&s);
    let data = render_scene(
        &gpu,
        &s,
        &SemanticStates::for_scene(&s),
        &styles_for(&s),
        &cam,
        None,
    );
    // The capsule center is solid copper; a point far off it is background.
    let center = px(&data, W / 2, H / 2);
    assert!(center[0] > 120, "copper red at capsule center: {center:?}");
    check_golden("capsule_aa", &data, &gpu.label);
}

#[test]
fn max_blend_saturates_overlapping_translucent_copper() {
    let Some(gpu) = gpu("max_blend") else { return };
    // Two overlapping wide capsules on a translucent pour plane: where they
    // overlap, coverage max-blends to the same value as a single capsule —
    // no double-darkening.
    let cross = |a, b| Prim::fill(1, PrimShape::Capsule { a, b, r: 1_500_000 });
    let s = scene(
        vec![Plane {
            key: PlaneKey::CopperPour("F.Cu".into()),
            prims: vec![
                cross(mm_pt(4.0, 7.5), mm_pt(16.0, 7.5)),
                cross(mm_pt(10.0, 2.0), mm_pt(10.0, 13.0)),
            ],
        }],
        1,
    );
    let cam = fit_camera(&s);
    let data = render_scene(
        &gpu,
        &s,
        &SemanticStates::for_scene(&s),
        &styles_for(&s),
        &cam,
        None,
    );
    // Center of the cross (overlap) vs a point on one arm only.
    let overlap = px(&data, W / 2, H / 2);
    let arm = px(&data, W / 2 - 40, H / 2);
    for c in 0..3 {
        assert!(
            overlap[c].abs_diff(arm[c]) <= 2,
            "overlap must not double-blend: {overlap:?} vs {arm:?}"
        );
    }
    check_golden("max_blend", &data, &gpu.label);
}

#[test]
fn rg_emphasis_mix_flags_selected_geometry() {
    let Some(gpu) = gpu("emphasis_mix") else {
        return;
    };
    let disc = |x, sem| {
        Prim::fill(
            sem,
            PrimShape::Disc {
                c: mm_pt(x, 7.5),
                r: 2_500_000,
            },
        )
    };
    let s = scene(
        vec![Plane {
            key: PlaneKey::Copper("F.Cu".into()),
            prims: vec![disc(6.0, 1), disc(14.0, 2)],
        }],
        2,
    );
    let mut state = SemanticStates::for_scene(&s);
    state.set_flags(2, FLAG_SELECTED, true);
    let cam = fit_camera(&s);
    let data = render_scene(&gpu, &s, &state, &styles_for(&s), &cam, None);
    let plain = px(&data, (W as f64 * 6.0 / 20.0) as u32, H / 2);
    let flagged = px(&data, (W as f64 * 14.0 / 20.0) as u32, H / 2);
    // The flagged disc renders in the emphasis accent (cyan: G/B high),
    // the plain one in copper red.
    assert!(plain[0] > plain[2], "plain disc stays copper: {plain:?}");
    assert!(
        flagged[2] > flagged[0],
        "selected disc takes the emphasis accent: {flagged:?}"
    );
    check_golden("emphasis_mix", &data, &gpu.label);
}

#[test]
fn drill_paints_background_over_dimmed_copper() {
    let Some(gpu) = gpu("drill_over_copper") else {
        return;
    };
    let s = scene(
        vec![
            Plane {
                key: PlaneKey::Copper("F.Cu".into()),
                prims: vec![Prim::fill(
                    1,
                    PrimShape::Disc {
                        c: mm_pt(10.0, 7.5),
                        r: 4_000_000,
                    },
                )],
            },
            Plane {
                key: PlaneKey::Drills,
                prims: vec![Prim::fill(
                    0,
                    PrimShape::Disc {
                        c: mm_pt(10.0, 7.5),
                        r: 1_500_000,
                    },
                )],
            },
        ],
        1,
    );
    // Dim the copper to half — the drill must still punch to full
    // background, not to dimmed-copper-over-background.
    let mut tables = StyleTables::board_defaults(true);
    tables.set_dim(&PlaneKey::Copper("F.Cu".into()), &s, 0.5);
    let styles = tables.resolve(&s, None);
    let cam = fit_camera(&s);
    let data = render_scene(
        &gpu,
        &s,
        &SemanticStates::for_scene(&s),
        &styles,
        &cam,
        None,
    );
    let hole = px(&data, W / 2, H / 2);
    let bg = px(&data, 4, 4);
    for c in 0..3 {
        assert!(
            hole[c].abs_diff(bg[c]) <= 2,
            "drill interior must be background: {hole:?} vs {bg:?}"
        );
    }
    // The copper ring around the drill is present but dimmed.
    let ring = px(&data, W / 2 + 30, H / 2);
    assert!(ring[0] > bg[0], "dimmed copper ring visible: {ring:?}");
    check_golden("drill_over_copper", &data, &gpu.label);
}

#[test]
fn grid_ladder_at_three_zooms() {
    let Some(gpu) = gpu("grid_ladder") else {
        return;
    };
    let s = scene(vec![], 0);
    let styles = styles_for(&s);
    let state = SemanticStates::for_scene(&s);
    for (i, zoom) in [8e-7, 8e-6, 8e-5].into_iter().enumerate() {
        let cam = Camera::new((10_000_000.0, 7_500_000.0), zoom);
        let data = render_scene(&gpu, &s, &state, &styles, &cam, None);
        // Dots exist: some pixel differs from the background.
        let bg = px(&data, 1, 1);
        assert!(
            (0..H).any(|y| (0..W).any(|x| px(&data, x, y) != bg)),
            "grid dots must render at zoom {zoom}"
        );
        check_golden(&format!("grid_zoom_{i}"), &data, &gpu.label);
    }
}

#[test]
fn crosshair_hairlines_at_cursor() {
    let Some(gpu) = gpu("crosshair") else { return };
    let s = scene(vec![], 0);
    let cam = fit_camera(&s);
    let cursor = [100.5, 80.5];
    let data = render_scene(
        &gpu,
        &s,
        &SemanticStates::for_scene(&s),
        &styles_for(&s),
        &cam,
        Some(cursor),
    );
    // The hairline rows/columns are brighter than the neighbouring bg.
    let on_v = px(&data, 100, 40);
    let off = px(&data, 120, 40);
    assert!(
        on_v[0] > off[0] && on_v[1] > off[1],
        "vertical hairline: {on_v:?} vs {off:?}"
    );
    let on_h = px(&data, 30, 80);
    assert!(on_h[0] > off[0], "horizontal hairline: {on_h:?}");
    check_golden("crosshair", &data, &gpu.label);
}
