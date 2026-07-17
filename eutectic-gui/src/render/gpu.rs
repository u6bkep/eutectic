//! The GPU half of the renderer (renderer-spec §3/§4): persistent per-plane
//! buffers, the shared coverage target, the coverage → composite pass chain,
//! the procedural grid/crosshair, and the headless-callable
//! [`Renderer::render`] entry.
//!
//! Pass structure per frame (§4): a background pass (clear to the canvas
//! background + procedural dot grid / origin marker), then per visible
//! plane: geometry renders **colorless** into the shared coverage target
//! (`Rg8Unorm`, MSAA 4× where supported, **max-blended**; R = base
//! coverage, G = state-flagged coverage), which a composite pass lays into
//! the pane texture back-to-front with per-plane style uniforms
//! (`color = mix(plane, emphasis, G/R)`, guarded at R≈0). The drills plane
//! composites with the background color (absence-through-everything). The
//! crosshair draws last. Layer visibility/dim/theme are uniform rewrites —
//! never geometry work.
//!
//! Everything here is callable without a window: the golden tests
//! (`tests/render_goldens.rs`) drive [`Renderer::render`] at an owned
//! readback texture; WP2 points the same call at a damascene `AppTexture`
//! view on the runner's device.

use super::camera::Camera;
use super::instance::{self, InstanceRaw, MeshVertex, TextInstRaw};
use super::scene::{PlaneKey, Prim, Scene};
use super::state::SemanticStates;
use super::style::ResolvedStyles;
use super::text::{TextBuf, TextGpu};
use eutectic_core::coord::Point;
use wgpu::util::DeviceExt;

/// Smallest on-screen grid spacing the 1-2-5 pitch ladder targets (px) —
/// the old canvas's `GRID_MIN_PX`, so the dot density band is unchanged.
pub const GRID_MIN_PX: f64 = 8.0;

// ---------------------------------------------------------------------------
// Scene buffers: persistent, keyed by doc revision (spec §3).
// ---------------------------------------------------------------------------

/// One plane's GPU geometry.
pub struct PlaneGpu {
    pub key: PlaneKey,
    instances: Option<(wgpu::Buffer, u32)>,
    mesh: Option<(wgpu::Buffer, wgpu::Buffer, u32)>,
    /// MSDF glyph quads (annotation text, §6) — coverage like everything
    /// else, drawn in this plane's coverage pass.
    text: Option<TextBuf>,
}

/// A scene's persistent GPU buffers: one instance buffer + one triangle
/// mesh per plane, plus the anchor everything is relative to. Built once
/// per doc revision ([`SceneCache`]), shared across panes viewing the doc.
pub struct SceneBuffers {
    pub anchor: Point,
    pub planes: Vec<PlaneGpu>,
}

impl SceneBuffers {
    /// Upload a scene. CPU-side lowering (instances + tessellation) is
    /// [`instance::build_plane_data`]; text runs rasterize any missing
    /// glyphs into `text`'s atlas and build glyph-quad buffers (§6). This
    /// only creates buffers.
    pub fn build(device: &wgpu::Device, scene: &Scene, text: &mut TextGpu) -> SceneBuffers {
        let planes = scene
            .planes
            .iter()
            .map(|plane| {
                let data = instance::build_plane_data(plane, scene.anchor);
                let instances = (!data.instances.is_empty()).then(|| {
                    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("render.plane.instances"),
                        contents: bytemuck::cast_slice(&data.instances),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    (buf, data.instances.len() as u32)
                });
                let mesh = (!data.mesh_indices.is_empty()).then(|| {
                    let vb = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("render.plane.mesh.vb"),
                        contents: bytemuck::cast_slice(&data.mesh_vertices),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    let ib = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("render.plane.mesh.ib"),
                        contents: bytemuck::cast_slice(&data.mesh_indices),
                        usage: wgpu::BufferUsages::INDEX,
                    });
                    (vb, ib, data.mesh_indices.len() as u32)
                });
                PlaneGpu {
                    key: plane.key.clone(),
                    instances,
                    mesh,
                    text: text.build_plane(device, &plane.prims, scene.anchor),
                }
            })
            .collect();
        SceneBuffers {
            anchor: scene.anchor,
            planes,
        }
    }
}

/// Persistent buffer cache keyed by doc revision (§3): rebuilt only when
/// the revision moves, never per frame / camera change / interaction.
#[derive(Default)]
pub struct SceneCache {
    rev: Option<u64>,
    buffers: Option<SceneBuffers>,
}

impl SceneCache {
    pub fn new() -> SceneCache {
        SceneCache::default()
    }

    /// The buffers for `rev`, rebuilding from `scene` iff the revision
    /// changed. `text` receives any glyph rasterization the rebuild needs
    /// (per doc revision — never per frame, §6).
    pub fn get_or_build(
        &mut self,
        device: &wgpu::Device,
        rev: u64,
        scene: &Scene,
        text: &mut TextGpu,
    ) -> &SceneBuffers {
        if self.rev != Some(rev) || self.buffers.is_none() {
            self.buffers = Some(SceneBuffers::build(device, scene, text));
            self.rev = Some(rev);
        }
        self.buffers.as_ref().expect("filled above")
    }
}

// ---------------------------------------------------------------------------
// Dynamic overlay buffer (spec §3/§5): same instance schema, rebuilt only
// while a preview is live.
// ---------------------------------------------------------------------------

/// The small dynamic overlay buffer (rubber-band trace, DRC halo, drag
/// ghost). Same instance/mesh schema as the static planes by design;
/// reserved for genuinely dynamic *geometry* — state tinting goes through
/// the semantic state buffer instead.
#[derive(Default)]
pub struct OverlayGpu {
    inst: Option<wgpu::Buffer>,
    inst_cap: u64,
    inst_count: u32,
    mesh: Option<(wgpu::Buffer, wgpu::Buffer)>,
    mesh_caps: (u64, u64),
    mesh_index_count: u32,
}

impl OverlayGpu {
    pub fn new() -> OverlayGpu {
        OverlayGpu::default()
    }

    /// Replace the overlay contents. Buffers grow geometrically and are
    /// reused between updates (a live preview updates every pointer move).
    pub fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        prims: &[Prim],
        anchor: Point,
    ) {
        let data = instance::build_prim_data(prims, anchor);
        self.inst_count = data.instances.len() as u32;
        if !data.instances.is_empty() {
            let bytes: &[u8] = bytemuck::cast_slice(&data.instances);
            if self.inst.is_none() || (bytes.len() as u64) > self.inst_cap {
                let cap = (bytes.len() as u64).next_power_of_two().max(1024);
                self.inst = Some(device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("render.overlay.instances"),
                    size: cap,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                self.inst_cap = cap;
            }
            queue.write_buffer(self.inst.as_ref().expect("created above"), 0, bytes);
        }
        self.mesh_index_count = data.mesh_indices.len() as u32;
        if !data.mesh_indices.is_empty() {
            let vb_bytes: &[u8] = bytemuck::cast_slice(&data.mesh_vertices);
            let ib_bytes: &[u8] = bytemuck::cast_slice(&data.mesh_indices);
            let need = (vb_bytes.len() as u64, ib_bytes.len() as u64);
            if self.mesh.is_none() || need.0 > self.mesh_caps.0 || need.1 > self.mesh_caps.1 {
                let caps = (
                    need.0.next_power_of_two().max(1024),
                    need.1.next_power_of_two().max(1024),
                );
                let vb = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("render.overlay.mesh.vb"),
                    size: caps.0,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let ib = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("render.overlay.mesh.ib"),
                    size: caps.1,
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.mesh = Some((vb, ib));
                self.mesh_caps = caps;
            }
            let (vb, ib) = self.mesh.as_ref().expect("created above");
            queue.write_buffer(vb, 0, vb_bytes);
            queue.write_buffer(ib, 0, ib_bytes);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inst_count == 0 && self.mesh_index_count == 0
    }
}

// ---------------------------------------------------------------------------
// Uniform ABI (must mirror the WGSL `Frame` / `PlaneU` structs).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniform {
    origin_px: [f32; 2],
    scale: f32,
    _p0: f32,
    viewport: [f32; 2],
    cursor_px: [f32; 2],
    grid: [f32; 4],
    grid_offset: [f32; 2],
    origin_marker: [f32; 2],
    flags: u32,
    _p1: [u32; 3],
    bg: [f32; 4],
    dot: [f32; 4],
    dot_major: [f32; 4],
    axis: [f32; 4],
    crosshair: [f32; 4],
    dash: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct PlaneUniform {
    color: [f32; 4],
    emphasis: [f32; 4],
    params: [f32; 4],
}

/// Dynamic-offset stride for [`PlaneUniform`] slots (the spec-mandated
/// 256-byte uniform alignment covers every real adapter).
const PLANE_STRIDE: u64 = 256;

/// Frame flag bits (mirrored in WGSL).
const FLAG_CURSOR: u32 = 1;
const FLAG_X_AXIS: u32 = 2;
const FLAG_Y_AXIS: u32 = 4;
const FLAG_GRID_LINES: u32 = 8;

fn frame_flags(axis_flags: u32, cursor: bool, style: crate::app::GridStyle) -> u32 {
    let mut flags = axis_flags;
    if cursor {
        flags |= FLAG_CURSOR;
    }
    if style == crate::app::GridStyle::Lines {
        flags |= FLAG_GRID_LINES;
    }
    flags
}

// ---------------------------------------------------------------------------
// Grid parameters (CPU side, pure — unit-tested).
// ---------------------------------------------------------------------------

/// The procedural grid's per-frame numbers, all reduced to screen-px
/// magnitudes in f64 before the f32 cast (a deep zoom far from the origin
/// produces astronomically large origin offsets; only their phase mod the
/// 10× lattice reaches the shader).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridParams {
    /// Minor dot pitch in px — in `[GRID_MIN_PX, 2.5 · GRID_MIN_PX)`.
    pub pitch_px: f64,
    /// The chosen ladder pitch in nm (1/2/5 × 10ⁿ mm).
    pub pitch_nm: f64,
    /// Screen-px phase of the 10×-pitch lattice (both lattices share it —
    /// a major dot sits on the origin).
    pub offset_px: [f64; 2],
    /// Screen px of the board origin (meaningful only when an axis flag is
    /// set; clamped magnitudes otherwise).
    pub origin_px: [f64; 2],
    /// [`FLAG_X_AXIS`] / [`FLAG_Y_AXIS`] visibility bits.
    pub axis_flags: u32,
}

/// Compute the grid ladder + phases for a camera (mm-native 1-2-5 ladder
/// keyed to zoom; renderer-spec §4).
pub fn grid_params(cam: &Camera, viewport: (f32, f32)) -> GridParams {
    let z = if cam.zoom.is_finite() && cam.zoom > 0.0 {
        cam.zoom
    } else {
        1e-6
    };
    // Smallest 1/2/5 × 10ⁿ mm whose screen spacing is ≥ GRID_MIN_PX.
    let ideal_mm = GRID_MIN_PX / (z * 1e6);
    let decade = 10f64.powf(ideal_mm.log10().floor());
    let n = ideal_mm / decade;
    let step = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    let pitch_nm = step * decade * 1e6;
    let pitch_px = pitch_nm * z;
    let (w, h) = (viewport.0 as f64, viewport.1 as f64);
    // Screen position of board (0,0) in f64 — exact camera math.
    let ox = w / 2.0 - cam.center.0 * z;
    let oy = h / 2.0 + cam.center.1 * z;
    let p10 = pitch_px * 10.0;
    let mut flags = 0;
    if (-4.0..=w + 4.0).contains(&ox) {
        flags |= FLAG_X_AXIS;
    }
    if (-4.0..=h + 4.0).contains(&oy) {
        flags |= FLAG_Y_AXIS;
    }
    GridParams {
        pitch_px,
        pitch_nm,
        offset_px: [ox.rem_euclid(p10), oy.rem_euclid(p10)],
        origin_px: [ox.clamp(-1e6, 1e6), oy.clamp(-1e6, 1e6)],
        axis_flags: flags,
    }
}

// ---------------------------------------------------------------------------
// Coverage-format negotiation (spec WP1 "verify early").
// ---------------------------------------------------------------------------

/// The negotiated coverage target: `Rg8Unorm` MSAA 4× where the adapter
/// supports rendering + 4× + resolve on it; `Rgba8Unorm` (spare channels
/// reserved — a future pass-group packing) otherwise; sample count falls to
/// 1 only if even that lacks MSAA (no known adapter). The decision is
/// logged so golden runs record it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageConfig {
    pub format: wgpu::TextureFormat,
    pub samples: u32,
}

/// Negotiate the coverage config against the adapter's format features.
pub fn pick_coverage(adapter: &wgpu::Adapter) -> CoverageConfig {
    use wgpu::TextureFormatFeatureFlags as Flags;
    let usable = |format: wgpu::TextureFormat| {
        let f = adapter.get_texture_format_features(format);
        let renderable = f
            .allowed_usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT);
        let msaa4 =
            f.flags.contains(Flags::MULTISAMPLE_X4) && f.flags.contains(Flags::MULTISAMPLE_RESOLVE);
        (renderable, msaa4)
    };
    for format in [
        wgpu::TextureFormat::Rg8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ] {
        let (renderable, msaa4) = usable(format);
        if renderable && msaa4 {
            return CoverageConfig { format, samples: 4 };
        }
    }
    // Fall back to single-sample Rgba8Unorm (guaranteed renderable).
    CoverageConfig {
        format: wgpu::TextureFormat::Rgba8Unorm,
        samples: 1,
    }
}

// ---------------------------------------------------------------------------
// The renderer.
// ---------------------------------------------------------------------------

/// Everything one frame needs (headless-callable; spec deliverable 9).
pub struct RenderArgs<'a> {
    pub scene: &'a SceneBuffers,
    /// Live preview geometry, composited above every plane under the
    /// [`PlaneKey::Overlay`] style. `None`/empty draws nothing.
    pub overlay: Option<&'a OverlayGpu>,
    pub camera: &'a Camera,
    pub styles: &'a ResolvedStyles,
    pub state: &'a SemanticStates,
    /// The pane texture view to draw into (an `AppTexture` view in WP2, an
    /// owned readback texture in tests). Must match the `target_format`
    /// the renderer was built with and be **at least** `size` texels (WP2's
    /// allocation hysteresis over-allocates; rendering is clipped to the
    /// top-left `size` sub-viewport, and the clear covers the rest).
    pub target: &'a wgpu::TextureView,
    /// The rendered viewport in texels — the pane's pixel size.
    pub size: (u32, u32),
    /// Pane-local cursor position for the crosshair; `None` hides it.
    pub cursor_px: Option<[f32; 2]>,
    /// Draw the procedural dot grid + origin axes (§4 canvas furniture).
    /// Board panes pass `true`; schematic panes pass `false` (the old
    /// schematic pane had no grid — a per-view config seam, not a fork).
    pub grid: bool,
    /// Dot or hairline-line procedural branch. Ignored when `grid` is false.
    pub grid_style: crate::app::GridStyle,
}

struct CoverageTargets {
    /// Allocated texel size — **grow-only** (two panes of different sizes
    /// share these targets per frame; each render clips to its own
    /// sub-viewport instead of reallocating).
    size: (u32, u32),
    /// MSAA render target (`samples` > 1) — resolved into `resolved`.
    msaa: Option<wgpu::TextureView>,
    /// Single-sample texture the composite pass reads.
    resolved_view: wgpu::TextureView,
}

/// The owned-canvas renderer: pipelines + shared targets. One per device;
/// scenes/cameras/styles/state arrive per call, so any number of panes
/// share it.
pub struct Renderer {
    coverage: CoverageConfig,
    target_format: wgpu::TextureFormat,
    srgb_target: bool,

    cover_inst: wgpu::RenderPipeline,
    cover_mesh: wgpu::RenderPipeline,
    cover_text: wgpu::RenderPipeline,
    composite: wgpu::RenderPipeline,
    chrome_bg: wgpu::RenderPipeline,
    chrome_cross: wgpu::RenderPipeline,

    cover_bgl: wgpu::BindGroupLayout,
    comp_bgl: wgpu::BindGroupLayout,
    text_bgl: wgpu::BindGroupLayout,
    text_sampler: wgpu::Sampler,
    /// The MSDF atlas + its GPU page mirrors (§6) — one per renderer, shared
    /// by every scene this device renders.
    text: TextGpu,

    frame_buf: wgpu::Buffer,
    plane_buf: wgpu::Buffer,
    plane_slots: u32,
    state_buf: wgpu::Buffer,
    state_capacity_words: usize,
    /// `(SemanticStates::id, generation)` of the last upload — the pair
    /// prevents cross-instance aliasing when the caller swaps flag buffers
    /// (doc switch) whose independent generation counters happen to match.
    state_uploaded_gen: Option<(u64, u64)>,

    cover_bg: wgpu::BindGroup,
    comp_bg: Option<wgpu::BindGroup>,
    targets: Option<CoverageTargets>,
}

impl Renderer {
    /// Build pipelines for a pane-texture `target_format`. `adapter`
    /// negotiates the coverage format (logged, with the adapter name — the
    /// golden tests record it).
    pub fn new(
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        target_format: wgpu::TextureFormat,
    ) -> Renderer {
        let coverage = pick_coverage(adapter);
        let info = adapter.get_info();
        log::info!(
            "render: adapter '{}' ({:?}); coverage {:?} @ {}x MSAA; target {:?}",
            info.name,
            info.backend,
            coverage.format,
            coverage.samples,
            target_format,
        );

        let cover_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render.cover"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/cover.wgsl").into()),
        });
        let comp_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render.composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/composite.wgsl").into()),
        });
        let chrome_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render.chrome"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/chrome.wgsl").into()),
        });
        let text_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("render.text"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/text.wgsl").into()),
        });

        let cover_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render.cover.bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let comp_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render.comp.bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        // Text (§6): group 0 is the cover pass's frame+state bindings, group
        // 1 the MSDF atlas page (texture + sampler), rebound between per-page
        // draw ranges.
        let text_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("render.text.bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let text_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("render.text.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let cover_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render.cover.pl"),
            bind_group_layouts: &[Some(&cover_bgl)],
            immediate_size: 0,
        });
        let text_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render.text.pl"),
            bind_group_layouts: &[Some(&cover_bgl), Some(&text_bgl)],
            immediate_size: 0,
        });
        let comp_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("render.comp.pl"),
            bind_group_layouts: &[Some(&comp_bgl)],
            immediate_size: 0,
        });

        // Coverage max-blend: overlapping same-plane primitives saturate.
        let max_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Max,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Max,
            },
        };
        let cover_target = [Some(wgpu::ColorTargetState {
            format: coverage.format,
            blend: Some(max_blend),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let inst_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<InstanceRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Uint32, 4 => Uint32,
            ],
        };
        let mesh_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<MeshVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Uint32],
        };
        let multisample = wgpu::MultisampleState {
            count: coverage.samples,
            mask: !0,
            alpha_to_coverage_enabled: false,
        };
        let cover_inst = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render.cover.inst"),
            layout: Some(&cover_pl),
            vertex: wgpu::VertexState {
                module: &cover_mod,
                entry_point: Some("vs_inst"),
                compilation_options: Default::default(),
                buffers: std::slice::from_ref(&inst_layout),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &cover_mod,
                entry_point: Some("fs_inst"),
                compilation_options: Default::default(),
                targets: &cover_target,
            }),
            multiview_mask: None,
            cache: None,
        });
        let cover_mesh = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render.cover.mesh"),
            layout: Some(&cover_pl),
            vertex: wgpu::VertexState {
                module: &cover_mod,
                entry_point: Some("vs_mesh"),
                compilation_options: Default::default(),
                buffers: std::slice::from_ref(&mesh_layout),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &cover_mod,
                entry_point: Some("fs_mesh"),
                compilation_options: Default::default(),
                targets: &cover_target,
            }),
            multiview_mask: None,
            cache: None,
        });

        // Text coverage: instanced glyph quads, same max-blended coverage
        // target as the analytic instances (§6 renders text as coverage).
        let text_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextInstRaw>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x4, 1 => Float32x4, 2 => Uint32, 3 => Float32,
            ],
        };
        let cover_text = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render.cover.text"),
            layout: Some(&text_pl),
            vertex: wgpu::VertexState {
                module: &text_mod,
                entry_point: Some("vs_text"),
                compilation_options: Default::default(),
                buffers: std::slice::from_ref(&text_layout),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample,
            fragment: Some(wgpu::FragmentState {
                module: &text_mod,
                entry_point: Some("fs_text"),
                compilation_options: Default::default(),
                targets: &cover_target,
            }),
            multiview_mask: None,
            cache: None,
        });

        let over_blend = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
        let pane_target = [Some(wgpu::ColorTargetState {
            format: target_format,
            blend: Some(over_blend),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let composite = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render.composite"),
            layout: Some(&comp_pl),
            vertex: wgpu::VertexState {
                module: &comp_mod,
                entry_point: Some("vs_fullscreen"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &comp_mod,
                entry_point: Some("fs_composite"),
                compilation_options: Default::default(),
                targets: &pane_target,
            }),
            multiview_mask: None,
            cache: None,
        });
        let chrome_pipeline = |entry: &str, label: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&cover_pl),
                vertex: wgpu::VertexState {
                    module: &chrome_mod,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &chrome_mod,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &pane_target,
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let chrome_bg = chrome_pipeline("fs_background", "render.chrome.bg");
        let chrome_cross = chrome_pipeline("fs_crosshair", "render.chrome.cross");

        let frame_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("render.frame.ubo"),
            size: std::mem::size_of::<FrameUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let plane_slots = 32u32;
        let plane_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("render.plane.ubo"),
            size: PLANE_STRIDE * plane_slots as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let state_capacity_words = 256;
        let state_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("render.state.ssbo"),
            size: (state_capacity_words * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let cover_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render.cover.bg"),
            layout: &cover_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: frame_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: state_buf.as_entire_binding(),
                },
            ],
        });

        Renderer {
            coverage,
            target_format,
            srgb_target: target_format.is_srgb(),
            cover_inst,
            cover_mesh,
            cover_text,
            composite,
            chrome_bg,
            chrome_cross,
            cover_bgl,
            comp_bgl,
            text_bgl,
            text_sampler,
            text: TextGpu::new(),
            frame_buf,
            plane_buf,
            plane_slots,
            state_buf,
            state_capacity_words,
            state_uploaded_gen: None,
            cover_bg,
            comp_bg: None,
            targets: None,
        }
    }

    /// The negotiated coverage target (tests assert / logs record it).
    pub fn coverage(&self) -> CoverageConfig {
        self.coverage
    }

    /// The pane-texture format the pipelines were built for.
    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// The text tier's atlas + GPU mirrors — scene builds rasterize glyphs
    /// through this ([`SceneCache::get_or_build`] takes it).
    pub fn text_mut(&mut self) -> &mut TextGpu {
        &mut self.text
    }

    /// A color component for the shader: linearized iff the target format
    /// is sRGB (blending then happens in linear light, like the rest of
    /// damascene's pipeline); raw otherwise (the golden tests' Rgba8Unorm).
    fn shader_color(&self, c: [f32; 4]) -> [f32; 4] {
        if !self.srgb_target {
            return c;
        }
        let lin = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        [lin(c[0]), lin(c[1]), lin(c[2]), c[3]]
    }

    fn ensure_targets(&mut self, device: &wgpu::Device, size: (u32, u32)) {
        // Grow-only: a request smaller than the allocation renders into its
        // own sub-viewport of the shared targets (see `render`), so two
        // panes of different sizes never thrash reallocations per frame.
        let size = match self.targets.as_ref() {
            Some(t) if t.size.0 >= size.0 && t.size.1 >= size.1 => return,
            Some(t) => (t.size.0.max(size.0), t.size.1.max(size.1)),
            None => size,
        };
        let extent = wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        };
        let msaa = (self.coverage.samples > 1).then(|| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("render.coverage.msaa"),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: self.coverage.samples,
                    dimension: wgpu::TextureDimension::D2,
                    format: self.coverage.format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        });
        let resolved = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("render.coverage.resolved"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.coverage.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let resolved_view = resolved.create_view(&Default::default());
        self.targets = Some(CoverageTargets {
            size,
            msaa,
            resolved_view,
        });
        self.comp_bg = None; // references the old resolve view
    }

    fn ensure_state(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, state: &SemanticStates) {
        let words = state.words();
        if words.len() > self.state_capacity_words {
            self.state_capacity_words = words.len().next_power_of_two();
            self.state_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("render.state.ssbo"),
                size: (self.state_capacity_words * 4) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.cover_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("render.cover.bg"),
                layout: &self.cover_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.frame_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.state_buf.as_entire_binding(),
                    },
                ],
            });
            self.state_uploaded_gen = None;
        }
        if self.state_uploaded_gen != Some((state.id(), state.generation())) {
            queue.write_buffer(&self.state_buf, 0, bytemuck::cast_slice(words));
            self.state_uploaded_gen = Some((state.id(), state.generation()));
        }
    }

    fn ensure_plane_slots(&mut self, device: &wgpu::Device, needed: u32) {
        if needed <= self.plane_slots {
            return;
        }
        self.plane_slots = needed.next_power_of_two();
        self.plane_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("render.plane.ubo"),
            size: PLANE_STRIDE * self.plane_slots as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.comp_bg = None;
    }

    fn ensure_comp_bg(&mut self, device: &wgpu::Device) {
        if self.comp_bg.is_some() {
            return;
        }
        let targets = self.targets.as_ref().expect("ensure_targets ran");
        self.comp_bg = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("render.comp.bg"),
            layout: &self.comp_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.plane_buf,
                        offset: 0,
                        size: wgpu::BufferSize::new(std::mem::size_of::<PlaneUniform>() as u64),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&targets.resolved_view),
                },
            ],
        }));
    }

    /// Render one frame into `args.target` (spec deliverable 9): background
    /// and grid, per-plane coverage/composite, overlay, crosshair. Submits
    /// its own command buffer.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, args: &RenderArgs<'_>) {
        let size = (args.size.0.max(1), args.size.1.max(1));
        self.ensure_targets(device, size);
        self.ensure_state(device, queue, args.state);

        // The draw list: visible planes with geometry, in scene order, plus
        // the overlay on top. Styles are parallel to scene planes by index.
        struct Draw<'a> {
            instances: Option<(&'a wgpu::Buffer, u32)>,
            mesh: Option<(&'a wgpu::Buffer, &'a wgpu::Buffer, u32)>,
            text: Option<&'a TextBuf>,
            uniform: PlaneUniform,
        }
        let mut draws: Vec<Draw> = Vec::new();
        for (i, plane) in args.scene.planes.iter().enumerate() {
            let Some(style) = args.styles.planes.get(i) else {
                continue; // styles were resolved for a different scene
            };
            if !style.visible
                || (plane.instances.is_none() && plane.mesh.is_none() && plane.text.is_none())
            {
                continue;
            }
            draws.push(Draw {
                instances: plane.instances.as_ref().map(|(b, n)| (b, *n)),
                mesh: plane.mesh.as_ref().map(|(v, i, n)| (v, i, *n)),
                text: plane.text.as_ref(),
                uniform: PlaneUniform {
                    color: self.shader_color(style.color),
                    emphasis: self.shader_color(style.emphasis),
                    params: [style.alpha, style.dim, 0.0, 0.0],
                },
            });
        }
        if let Some(overlay) = args.overlay
            && !overlay.is_empty()
        {
            let style = &args.styles.overlay;
            draws.push(Draw {
                instances: overlay
                    .inst
                    .as_ref()
                    .filter(|_| overlay.inst_count > 0)
                    .map(|b| (b, overlay.inst_count)),
                mesh: overlay
                    .mesh
                    .as_ref()
                    .filter(|_| overlay.mesh_index_count > 0)
                    .map(|(v, i)| (v, i, overlay.mesh_index_count)),
                text: None,
                uniform: PlaneUniform {
                    color: self.shader_color(style.color),
                    emphasis: self.shader_color(style.emphasis),
                    params: [style.alpha, style.dim, 0.0, 0.0],
                },
            });
        }
        self.ensure_plane_slots(device, draws.len() as u32);
        self.ensure_comp_bg(device);
        for (slot, d) in draws.iter().enumerate() {
            queue.write_buffer(
                &self.plane_buf,
                slot as u64 * PLANE_STRIDE,
                bytemuck::bytes_of(&d.uniform),
            );
        }

        // Frame uniform: camera transform + grid/crosshair parameters. With
        // `args.grid` off (schematic panes — no grid, no axes, matching the
        // old pane's furniture) the pitch uploads as 0, which the background
        // shader's existing `pitch >= 2` gate already draws as nothing — a
        // pure uniform config seam, no shader change.
        let vp = (size.0 as f32, size.1 as f32);
        let (origin_px, scale) = args.camera.view_transform(args.scene.anchor, vp);
        let grid = if args.grid {
            grid_params(args.camera, vp)
        } else {
            GridParams {
                pitch_px: 0.0,
                pitch_nm: 0.0,
                offset_px: [0.0, 0.0],
                origin_px: [0.0, 0.0],
                axis_flags: 0,
            }
        };
        let dot_r_minor = (grid.pitch_px * 0.09).clamp(0.75, 2.5) as f32;
        let dot_r_major = (dot_r_minor * 1.8).min(4.0);
        let flags = frame_flags(grid.axis_flags, args.cursor_px.is_some(), args.grid_style);
        let mut dash = [[0f32; 4]; 4];
        for (i, d) in args.styles.dash.iter().take(4).enumerate() {
            let on_px = (d[0] * args.camera.zoom) as f32;
            let off_px = (d[1] * args.camera.zoom) as f32;
            dash[i] = [on_px, on_px + off_px, 0.0, 0.0];
        }
        let frame = FrameUniform {
            origin_px,
            scale,
            _p0: 0.0,
            viewport: [vp.0, vp.1],
            cursor_px: args.cursor_px.unwrap_or([-1e6, -1e6]),
            grid: [grid.pitch_px as f32, dot_r_minor, dot_r_major, 0.0],
            grid_offset: [grid.offset_px[0] as f32, grid.offset_px[1] as f32],
            origin_marker: [grid.origin_px[0] as f32, grid.origin_px[1] as f32],
            flags,
            _p1: [0; 3],
            bg: self.shader_color(args.styles.background),
            dot: self.shader_color(args.styles.grid_dot),
            dot_major: self.shader_color(args.styles.grid_dot_major),
            axis: self.shader_color(args.styles.grid_axis),
            crosshair: self.shader_color(args.styles.crosshair),
            dash,
        };
        queue.write_buffer(&self.frame_buf, 0, bytemuck::bytes_of(&frame));

        let bg = self.shader_color(args.styles.background);
        let clear = wgpu::Color {
            r: bg[0] as f64,
            g: bg[1] as f64,
            b: bg[2] as f64,
            a: 1.0,
        };

        // Text: mirror any atlas pages a scene build dirtied (per doc
        // revision — idle frames upload nothing).
        if draws.iter().any(|d| d.text.is_some()) {
            self.text
                .sync(device, queue, &self.text_bgl, &self.text_sampler);
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("render.frame"),
        });

        // Every pass draws into the top-left `size` sub-viewport: the pane
        // texture (allocation hysteresis) and the shared coverage targets
        // (grow-only) may both be larger than the rendered pane.
        let vp_px = (size.0 as f32, size.1 as f32);

        // Background + grid (clears the pane).
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render.pass.background"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: args.target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_viewport(0.0, 0.0, vp_px.0, vp_px.1, 0.0, 1.0);
            pass.set_pipeline(&self.chrome_bg);
            pass.set_bind_group(0, &self.cover_bg, &[]);
            pass.draw(0..3, 0..1);
        }

        // Per plane: coverage then composite.
        let targets = self.targets.as_ref().expect("ensured");
        let comp_bg = self.comp_bg.as_ref().expect("ensured");
        for (slot, d) in draws.iter().enumerate() {
            {
                let (view, resolve_target) = match &targets.msaa {
                    Some(msaa) => (msaa, Some(&targets.resolved_view)),
                    None => (&targets.resolved_view, None),
                };
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("render.pass.coverage"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: if targets.msaa.is_some() {
                                wgpu::StoreOp::Discard
                            } else {
                                wgpu::StoreOp::Store
                            },
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_viewport(0.0, 0.0, vp_px.0, vp_px.1, 0.0, 1.0);
                pass.set_bind_group(0, &self.cover_bg, &[]);
                if let Some((vb, ib, n)) = d.mesh {
                    pass.set_pipeline(&self.cover_mesh);
                    pass.set_vertex_buffer(0, vb.slice(..));
                    pass.set_index_buffer(ib.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..n, 0, 0..1);
                }
                if let Some((buf, n)) = d.instances {
                    pass.set_pipeline(&self.cover_inst);
                    pass.set_vertex_buffer(0, buf.slice(..));
                    pass.draw(0..4, 0..n);
                }
                if let Some(text) = d.text {
                    pass.set_pipeline(&self.cover_text);
                    pass.set_vertex_buffer(0, text.buf.slice(..));
                    for (page, range) in &text.ranges {
                        let Some(bg) = self.text.page_bg(*page) else {
                            continue; // page mirror missing (unreachable after sync)
                        };
                        pass.set_bind_group(1, bg, &[]);
                        pass.draw(0..4, range.clone());
                    }
                }
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("render.pass.composite"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: args.target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_viewport(0.0, 0.0, vp_px.0, vp_px.1, 0.0, 1.0);
                pass.set_pipeline(&self.composite);
                pass.set_bind_group(0, comp_bg, &[slot as u32 * PLANE_STRIDE as u32]);
                pass.draw(0..3, 0..1);
            }
        }

        // Crosshair, over everything.
        if args.cursor_px.is_some() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render.pass.crosshair"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: args.target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_viewport(0.0, 0.0, vp_px.0, vp_px.1, 0.0, 1.0);
            pass.set_pipeline(&self.chrome_cross);
            pass.set_bind_group(0, &self.cover_bg, &[]);
            pass.draw(0..3, 0..1);
        }

        queue.submit([encoder.finish()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_ladder_is_1_2_5_and_spacing_band_holds() {
        // Sweep zooms across 12 decades; the chosen pitch must be a 1/2/5
        // decade multiple in mm and the screen spacing in [8, 20) px.
        let mut z = 1e-9; // px per nm
        while z < 1e3 {
            let cam = Camera::new((0.0, 0.0), z);
            let g = grid_params(&cam, (800.0, 600.0));
            assert!(
                g.pitch_px >= GRID_MIN_PX && g.pitch_px < GRID_MIN_PX * 2.5 + 1e-9,
                "zoom {z}: pitch_px {}",
                g.pitch_px
            );
            let mm = g.pitch_nm / 1e6;
            let decade = 10f64.powf(mm.log10().floor());
            let step = mm / decade;
            let ok = [1.0, 2.0, 5.0, 10.0]
                .iter()
                .any(|s| (step - s).abs() / s < 1e-9);
            assert!(ok, "zoom {z}: step {step} (pitch {mm} mm)");
            z *= 3.7;
        }
    }

    #[test]
    fn grid_phase_is_screen_scale_even_far_from_origin() {
        // Center ~1 m from the origin at deep zoom: the raw origin offset
        // is ~1e11 px, but the emitted phase stays inside one 10x cell.
        let cam = Camera::new((999_999_999.0, -999_999_999.0), 100.0);
        let g = grid_params(&cam, (800.0, 600.0));
        let p10 = g.pitch_px * 10.0;
        assert!(g.offset_px[0] >= 0.0 && g.offset_px[0] < p10);
        assert!(g.offset_px[1] >= 0.0 && g.offset_px[1] < p10);
        assert_eq!(g.axis_flags, 0, "origin is far off-screen");
    }

    #[test]
    fn origin_axes_flag_when_visible() {
        let cam = Camera::new((0.0, 0.0), 1e-5);
        let g = grid_params(&cam, (800.0, 600.0));
        assert_eq!(g.axis_flags, FLAG_X_AXIS | FLAG_Y_AXIS);
        assert!((g.origin_px[0] - 400.0).abs() < 1e-6);
        assert!((g.origin_px[1] - 300.0).abs() < 1e-6);
    }

    #[test]
    fn grid_dot_on_origin() {
        // A lattice point must sit exactly on the board origin whenever the
        // origin is on-screen: offset ≡ origin (mod 10·pitch).
        let cam = Camera::new((3_141_592.0, -2_718_281.0), 2e-5);
        let g = grid_params(&cam, (800.0, 600.0));
        let p10 = g.pitch_px * 10.0;
        let dx = (g.origin_px[0] - g.offset_px[0]).rem_euclid(p10);
        assert!(dx < 1e-6 || (p10 - dx) < 1e-6, "dx {dx} of {p10}");
    }

    #[test]
    fn uniform_layouts_match_wgsl() {
        // Struct sizes are load-bearing for the WGSL ABI; a drive-by field
        // edit must fail loudly here rather than misrender.
        assert_eq!(std::mem::size_of::<FrameUniform>(), 224);
        assert_eq!(std::mem::size_of::<PlaneUniform>(), 48);
        assert_eq!(std::mem::size_of::<InstanceRaw>(), 40);
        assert_eq!(std::mem::size_of::<MeshVertex>(), 12);
        assert_eq!(std::mem::size_of::<TextInstRaw>(), 40);
    }

    #[test]
    fn grid_style_is_encoded_in_the_frame_uniform_flags() {
        let dots = frame_flags(FLAG_X_AXIS, false, crate::app::GridStyle::Dots);
        let lines = frame_flags(FLAG_X_AXIS, false, crate::app::GridStyle::Lines);
        assert_eq!(dots & FLAG_GRID_LINES, 0, "dots are the default branch");
        assert_ne!(lines & FLAG_GRID_LINES, 0, "lines set the uniform branch");
        assert_eq!(dots & FLAG_X_AXIS, lines & FLAG_X_AXIS);
    }
}
