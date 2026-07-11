//! Owned-canvas board panes (renderer-spec §12 WP2) — the app-side plumbing
//! that puts the WP1 renderer behind the existing pane interface.
//!
//! Everything the spec's §7/§9 contracts need lives here:
//!
//! - **Per-pane cameras** ([`BoardCam`]): a [`CameraGlide`] per board pane in
//!   app state (f64 center-nm / zoom-px-per-nm), with the min/max zoom clamp
//!   ([`clamp_zoom`]) and the pending camera requests (Fit / Reset) consumed
//!   in `build` where the pane rect is known.
//! - **Texture lifecycle**: one app-owned `wgpu::Texture` per board pane,
//!   wrapped as a damascene [`AppTexture`] and composited by one keyed
//!   `surface()` El (`SurfaceAlpha::Opaque`, sRGB8 matching the swapchain,
//!   constructed on the runner's device). Allocation runs through the
//!   [`tex_alloc`] hysteresis (grow to a [`TEX_STEP`] boundary, shrink
//!   lazily); the renderer draws the pane's exact pixel size into the
//!   texture's top-left sub-viewport.
//! - **Damage contract** ([`PaneDamage`]): a pane texture re-renders iff its
//!   [`DamageKey`] changed; the probe counts actual renders so tests can
//!   prove the idle-frames-render-zero rule with numbers, not claims.
//! - **Semantic states**: the selection/hover model maps onto the scene's
//!   [`SemanticKey`] table ([`board_state_words`]) — one-word writes into the
//!   shared [`SemanticStates`](crate::render::SemanticStates) buffer, so both
//!   board panes (and the schematic's cross-highlight source) observe the
//!   same selection.
//! - **Overlay lowering** ([`overlay_prims`]): the per-frame
//!   [`Overlay`](crate::canvas::Overlay) (measure segment, finding markers,
//!   drag ghost + ratsnest, route runs/rubber/vias, edit path, vertex
//!   handles) lowers to renderer overlay primitives; highlight geometry is
//!   deliberately NOT lowered — selection/hover emphasis goes through the
//!   state buffer instead (spec §5).
//! - **Raw-event input** (the host's ECAD seams): free hover, the crosshair
//!   cursor, and middle-drag pan ride `raw_window_event`; the per-frame
//!   texture renders ride `before_paint`.
//!
//! # Frame order & the one-frame rect lag
//!
//! The host calls `before_paint` (renders pane textures) *before* `build`
//! (which reads the previous frame's laid-out rects via `cx.rect_of_key`).
//! Pane rects and the scale factor are therefore captured during `build`
//! into [`EutecticApp::pane_px`] / `scale_factor` and consumed one frame
//! later — during a live resize the texture trails the rect by one frame
//! (the resize itself keeps frames flowing), and at rest they agree exactly.

use crate::app::pane::{PaneId, pane_index};
use crate::app::{EutecticApp, ViewKind};
use crate::canvas::Overlay;
use crate::canvas::pick;
use crate::highlight::HighlightSets;
use crate::render::camera::ZoomAnchor;
use crate::render::scene::{PlaneKey, Prim, PrimShape, SEM_CHROME, SemanticKey};
use crate::render::state::{FLAG_HOVERED, FLAG_SELECTED};
use crate::render::{
    Camera, CameraGlide, DamageKey, OverlayGpu, RenderArgs, Renderer, ResolvedStyles, SceneCache,
    StyleTables, needs_render,
};
use eutectic_core::coord::{Nm, Point};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Camera: clamps, per-pane state, gestures.
// ---------------------------------------------------------------------------

/// A pane/strip rect in window-logical px, `(x, y, w, h)` — the shape
/// `cx.rect_of_key` yields, captured per build for the paint + raw-event
/// paths.
pub(crate) type PaneRect = (f32, f32, f32, f32);

/// Minimum zoom, px/nm: 0.1 px/mm — the old viewport's `min_zoom(0.1)`
/// (whole-building-scale zoom-out; the fit clamp keeps boards visible).
pub(crate) const MIN_ZOOM: f64 = 1e-7;

/// Maximum zoom, px/nm: 0.01 px/nm = 10 000 px/mm (1 px = 100 nm).
///
/// Justification (spec deliverable: the max clamp must account for the f32
/// anchor-relative quantization): vertex data uploads as anchor-relative f32,
/// so a feature 100 mm (1e8 nm) from the scene anchor — a large board's far
/// corner — quantizes to the f32 lattice, whose ULP at 1e8 is 2³ = 8 nm
/// (≈ 4 nm max rounding error). At 0.01 px/nm that error is ≤ 0.08 px on
/// screen — invisible. One decade deeper (0.1 px/nm) it would reach 0.8 px
/// and visibly wobble geometry against the exact-f64 grid/crosshair, so
/// 0.01 px/nm is the last safe decade. (The old viewport's 64 px/mm max is
/// 156× shallower; the owned camera makes deep inspection zoom real.)
pub(crate) const MAX_ZOOM: f64 = 1e-2;

/// The initial / reset zoom: 1 px per mm (the old viewport's `zoom = 1.0`).
pub(crate) const RESET_ZOOM: f64 = 1e-6;

/// Fit-to-content margin in px (the old `FitContent { padding: 24.0 }`).
pub(crate) const FIT_PADDING_PX: f64 = 24.0;

/// Wheel zoom rate: `exp(-dy · K)` per event, tuned so one 50 px line tick
/// (a notched mouse wheel step) is a ×1.25 zoom.
pub(crate) const WHEEL_ZOOM_K: f64 = 0.004462871026284195; // ln(1.25) / 50

/// Clamp a target zoom into the [`MIN_ZOOM`], [`MAX_ZOOM`] band (guarding
/// non-finite input to the reset zoom).
pub(crate) fn clamp_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        RESET_ZOOM
    }
}

/// A pending camera operation for a board pane, consumed in `build` where
/// the pane's laid-out rect is known (so Fit/Reset work for panes that are
/// currently hidden — they apply on first show, like the old queued
/// `ViewportRequest`s).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CamRequest {
    /// Frame the scene bounds with [`FIT_PADDING_PX`].
    Fit,
    /// The reset view: [`RESET_ZOOM`], scene top-left at the pane top-left
    /// (the old `ResetView`'s zoom-1/pan-0 framing).
    Reset,
}

/// One board pane's camera state: the glide filter plus any pending request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BoardCam {
    pub(crate) glide: CameraGlide,
    pub(crate) request: Option<CamRequest>,
}

impl Default for BoardCam {
    fn default() -> Self {
        BoardCam {
            glide: CameraGlide::new(Camera::new((0.0, 0.0), RESET_ZOOM)),
            request: None,
        }
    }
}

/// Unproject a window-logical pointer position to board nm through a pane
/// camera: pointer px → pane-local px → f64 unproject → rounded nm. The
/// y flip (screen y down, board y up) lives inside [`Camera::unproject`].
pub(crate) fn board_unproject(cam: &Camera, rect: (f32, f32, f32, f32), pos: (f32, f32)) -> Point {
    let p = cam.unproject(
        ((pos.0 - rect.0) as f64, (pos.1 - rect.1) as f64),
        (rect.2 as f64, rect.3 as f64),
    );
    Point {
        x: p.0.round() as Nm,
        y: p.1.round() as Nm,
    }
}

/// Project a board point to window-logical px through a pane camera — the
/// exact inverse of [`board_unproject`]. Consumed by the test tier (the
/// board→screen helper every synthesized-pointer test maps through).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn board_project(cam: &Camera, rect: (f32, f32, f32, f32), p: Point) -> (f32, f32) {
    let px = cam.project((p.x as f64, p.y as f64), (rect.2 as f64, rect.3 as f64));
    (rect.0 + px.0 as f32, rect.1 + px.1 as f32)
}

/// A pane camera's zoom in the legacy "px per mm" scale (`1.0` = 1 logical
/// px per board mm — the viewport-era readout the zoom chip, status bar, and
/// pick tolerance keep using).
pub(crate) fn zoom_px_per_mm(cam: &Camera) -> f32 {
    (cam.zoom * 1e6) as f32
}

/// The reset camera for `bounds` in a `viewport`-px pane: [`RESET_ZOOM`]
/// with the scene bounds' top-left (x0, y1 — board y is up) at the pane's
/// top-left, mirroring the old viewport `ResetView` (zoom 1, pan 0: content
/// top-left at the viewport origin).
pub(crate) fn reset_camera(bounds: (Nm, Nm, Nm, Nm), viewport: (f64, f64)) -> Camera {
    let z = RESET_ZOOM;
    Camera::new(
        (
            bounds.0 as f64 + viewport.0 / 2.0 / z,
            bounds.3 as f64 - viewport.1 / 2.0 / z,
        ),
        z,
    )
}

// ---------------------------------------------------------------------------
// Texture allocation hysteresis (spec §9 sizing).
// ---------------------------------------------------------------------------

/// Texture allocation step (px). Growing snaps up to the next multiple;
/// shrinking waits until the allocation is ≥ 2 steps above the needed step.
pub(crate) const TEX_STEP: u32 = 256;

/// The pane-texture allocation for a needed pixel size, with hysteresis so a
/// live pane resize doesn't thrash allocations: grow to a step boundary the
/// moment `needed` exceeds the current allocation; shrink only once the
/// current allocation is at least two whole steps above the needed step
/// boundary (then snap down to it).
pub(crate) fn tex_alloc(needed: (u32, u32), current: Option<(u32, u32)>) -> (u32, u32) {
    let step_up = |v: u32| v.max(1).div_ceil(TEX_STEP) * TEX_STEP;
    let want = (step_up(needed.0), step_up(needed.1));
    match current {
        None => want,
        Some(cur) => {
            if needed.0 > cur.0 || needed.1 > cur.1 {
                (want.0.max(cur.0), want.1.max(cur.1))
            } else if cur.0 >= want.0 + 2 * TEX_STEP || cur.1 >= want.1 + 2 * TEX_STEP {
                want
            } else {
                cur
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Damage probe (spec §7 — a contract, instrumented).
// ---------------------------------------------------------------------------

/// Per-pane damage state + render counter. [`observe`](Self::observe) is the
/// single render/skip decision point, and [`renders`](Self::renders) counts
/// how many frames actually re-rendered — the instrumented proof that idle
/// frames cost zero GPU work.
#[derive(Debug, Default)]
pub(crate) struct PaneDamage {
    last: Option<DamageKey>,
    pub(crate) renders: u64,
}

impl PaneDamage {
    /// Forget the last rendered key (texture reallocated / GPU rebuilt —
    /// the cached pixels are gone, so the next frame must render).
    pub(crate) fn invalidate(&mut self) {
        self.last = None;
    }

    /// Should this frame render? Records the key and counts iff yes.
    pub(crate) fn observe(&mut self, key: DamageKey) -> bool {
        if needs_render(self.last.as_ref(), &key) {
            self.last = Some(key);
            self.renders += 1;
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Selection → semantic state words (spec §5; deliverable 6's state half).
// ---------------------------------------------------------------------------

/// Map the cross-view highlight sets onto a scene's semantic-key table: one
/// flag word per compact id. Net-keyed geometry lights when its net is in
/// the expanded set (`HighlightSets::nets` — the same net expansion the old
/// overlay's `board_matches` applied); netless copper lights when its own
/// pick id is in the board set. `selected` and `hovered` are projected
/// separately so the flags stay distinct words.
pub(crate) fn board_state_words(
    semantics: &[SemanticKey],
    selected: &HighlightSets,
    hovered: &HighlightSets,
) -> Vec<u32> {
    fn matches(sets: &HighlightSets, key: &SemanticKey) -> bool {
        match key {
            SemanticKey::Chrome | SemanticKey::Board | SemanticKey::BoardText => false,
            SemanticKey::Net(n) => sets.nets.contains(n),
            SemanticKey::Trace(t) => sets.board.contains(&pick::SemanticId::Trace(*t)),
            SemanticKey::Via(v) => sets.board.contains(&pick::SemanticId::Via(*v)),
            SemanticKey::Pin { comp, pad } => sets.board.contains(&pick::SemanticId::Pin {
                comp: comp.clone(),
                pin: pad.clone(),
            }),
            SemanticKey::Part(e) => sets.board.contains(&pick::SemanticId::Part(e.clone())),
        }
    }
    semantics
        .iter()
        .map(|key| {
            let mut w = 0;
            if matches(selected, key) {
                w |= FLAG_SELECTED;
            }
            if matches(hovered, key) {
                w |= FLAG_HOVERED;
            }
            w
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Overlay lowering (deliverable 6's geometry half).
// ---------------------------------------------------------------------------

/// Lower a board pane's per-frame [`Overlay`] to renderer overlay primitives.
///
/// Screen-constant stroke widths convert through `zoom` (px/nm — the
/// *physical* zoom, so 1 px is one device pixel). The `highlights` field is
/// deliberately ignored: selection/hover emphasis on scene geometry goes
/// through the semantic state buffer ([`board_state_words`]), never through
/// overlay geometry. Everything else — geometry with no scene primitive —
/// lowers here.
pub(crate) fn overlay_prims(o: &Overlay, zoom: f64) -> Vec<Prim> {
    let z = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        RESET_ZOOM
    };
    let px = |v: f64| -> Nm { ((v / z).round() as Nm).max(1) };
    fn capsule(out: &mut Vec<Prim>, a: Point, b: Point, r: Nm) {
        out.push(Prim::fill(SEM_CHROME, PrimShape::Capsule { a, b, r }));
    }
    fn chain(out: &mut Vec<Prim>, pts: &[Point], r: Nm) {
        if pts.len() == 1 {
            out.push(Prim::fill(SEM_CHROME, PrimShape::Disc { c: pts[0], r }));
        }
        for w in pts.windows(2) {
            if w[0] != w[1] {
                capsule(out, w[0], w[1], r);
            }
        }
    }
    fn ring(out: &mut Vec<Prim>, c: Point, radius: f64, hw: Nm) {
        out.push(Prim::fill(
            SEM_CHROME,
            PrimShape::ArcStroke {
                center: [c.x as f64, c.y as f64],
                radius,
                a0: 0.0,
                a1: std::f64::consts::TAU,
                half_width: hw,
            },
        ));
    }

    let mut out = Vec::new();
    // Measure preview: the segment plus small endpoint dots.
    if let Some((a, b)) = o.measure {
        capsule(&mut out, a, b, px(0.75));
        out.push(Prim::fill(SEM_CHROME, PrimShape::Disc { c: a, r: px(2.0) }));
        out.push(Prim::fill(SEM_CHROME, PrimShape::Disc { c: b, r: px(2.0) }));
    }
    // Finding markers: a ring per located finding (screen-constant size).
    for (p, _is_error) in &o.findings {
        ring(&mut out, *p, 6.0 / z, px(1.25));
    }
    // Drag ghost: the dragged component's pad shapes, filled.
    for s in &o.ghost {
        crate::render::board::fill_prims(&mut out, s, SEM_CHROME, 0);
    }
    // Live ratsnest: hairlines from ghost pads to their nets.
    for (a, b) in &o.ratsnest {
        if a != b {
            capsule(&mut out, *a, *b, px(0.5));
        }
    }
    // Pending route: runs at commit width, rubber at the same width, vias as
    // pad-sized rings.
    for (pts, width) in &o.route_runs {
        chain(&mut out, pts, (*width / 2).max(1));
    }
    if let Some((a, b)) = o.route_rubber {
        let (w, ..) = crate::app::route_defaults();
        if a != b {
            capsule(&mut out, a, b, (w / 2).max(1));
        }
    }
    for (p, pad) in &o.route_vias {
        ring(&mut out, *p, (*pad as f64) / 2.0, px(1.0));
    }
    // Trace-vertex refinement: the working path preview + vertex handles.
    if let Some((pts, width)) = &o.edit_path {
        chain(&mut out, pts, (*width / 2).max(1));
    }
    for p in &o.handles {
        out.push(Prim::fill(
            SEM_CHROME,
            PrimShape::Disc { c: *p, r: px(2.5) },
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Stale-dim (spec §9 elaboration failure).
// ---------------------------------------------------------------------------

/// The stale-revision dim factor: while the freshest source fails to
/// elaborate, the last-good revision keeps rendering with every plane dimmed
/// by this (the findings/chrome carry the error text — the existing alert).
pub(crate) const STALE_DIM: f32 = 0.55;

/// Apply the stale composite treatment to a frame's resolved styles.
pub(crate) fn stale_dim(styles: &mut ResolvedStyles) {
    for p in styles.planes.iter_mut() {
        p.dim *= STALE_DIM;
    }
    styles.overlay.dim *= STALE_DIM;
}

/// The layer-panel visibility key governing a scene plane, `None` for planes
/// with no toggle (drills, the overlay). Substrate follows the outline's
/// toggle — both are "the board body" in the panel's vocabulary.
pub(crate) fn plane_layer_key(key: &PlaneKey) -> Option<String> {
    match key {
        PlaneKey::Substrate | PlaneKey::Outline => Some("layer:outline".to_string()),
        PlaneKey::Copper(n)
        | PlaneKey::CopperPour(n)
        | PlaneKey::Mask(n)
        | PlaneKey::Silk(n)
        | PlaneKey::Fab(n) => Some(format!("layer:{n}")),
        PlaneKey::Drills | PlaneKey::Overlay => None,
    }
}

// ---------------------------------------------------------------------------
// GPU state (windowed path only; the CPU harness never constructs this).
// ---------------------------------------------------------------------------

/// One board pane's GPU-side state: its texture (+ the damascene handle the
/// `surface()` El composites), damage record, and overlay buffer.
#[derive(Default)]
pub(crate) struct PaneGpu {
    tex: Option<PaneTexture>,
    pub(crate) damage: PaneDamage,
    overlay: OverlayGpu,
    /// The prims currently uploaded to `overlay` — the equality gate that
    /// keeps `overlay_gen` (a damage input) quiet when nothing moved.
    overlay_prims: Vec<Prim>,
    overlay_gen: u64,
}

struct PaneTexture {
    /// Kept alive for view creation; the damascene handle holds its own Arc.
    texture: wgpu::Texture,
    handle: damascene_core::surface::AppTexture,
    alloc: (u32, u32),
}

/// The app's GPU bundle, created by the host's `gpu_setup` seam on the
/// runner's device (same-device zero-copy compositing) and **rebuilt from
/// CPU caches** whenever the host hands us a fresh device (Android
/// suspend/resume recreates the GPU context; a lost device takes the same
/// path) — scenes, cameras, and style state all live outside this struct,
/// so a rebuild is just re-uploading.
pub(crate) struct GpuState {
    renderer: Renderer,
    scenes: SceneCache,
    styles: StyleTables,
    panes: [PaneGpu; 2],
    last_frame: Option<Instant>,
}

impl EutecticApp {
    /// Build (or rebuild — device loss / Android resume) the GPU bundle on
    /// the runner's device. The pane texture format is sRGB8 matching the
    /// swapchain family (spec §9): BGRA swapchains get `Bgra8UnormSrgb`,
    /// everything else (including HDR float swapchains) `Rgba8UnormSrgb`.
    pub(crate) fn setup_gpu(
        &mut self,
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        surface_format: wgpu::TextureFormat,
    ) {
        let format = match surface_format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                wgpu::TextureFormat::Bgra8UnormSrgb
            }
            _ => wgpu::TextureFormat::Rgba8UnormSrgb,
        };
        *self.gpu.borrow_mut() = Some(GpuState {
            renderer: Renderer::new(device, adapter, format),
            scenes: SceneCache::new(),
            styles: StyleTables::board_defaults(true),
            panes: [PaneGpu::default(), PaneGpu::default()],
            last_frame: None,
        });
    }

    /// A board pane's `AppTexture` handle for the `surface()` El, if its
    /// texture exists (windowed path, after the first paint), plus the
    /// allocated texel size.
    pub(crate) fn board_pane_texture(
        &self,
        pane: PaneId,
    ) -> Option<(damascene_core::surface::AppTexture, (u32, u32))> {
        let gpu = self.gpu.borrow();
        let t = gpu.as_ref()?.panes[pane_index(pane)].tex.as_ref()?;
        Some((t.handle.clone(), t.alloc))
    }

    /// The style/theme damage-key input: the layer-visibility revision plus
    /// the stale bit (reload-error dim in force).
    pub(crate) fn board_style_gen(&self) -> u64 {
        self.style_rev.get() * 2 + self.domain.reload_error.is_some() as u64
    }

    /// Recompute the semantic state buffer from the shared selection model —
    /// per-frame, one-word diffs only (`set_word` bumps the generation only
    /// on real changes, so an idle selection is damage-quiet).
    pub(crate) fn sync_board_states(&self) {
        let derived = self.derived.borrow();
        let Some(scene) = &derived.scene else {
            return;
        };
        let (sel_sets, hov_sets) = match &self.domain.doc {
            Ok(doc) => {
                let sel = self.domain.selection.borrow();
                (
                    HighlightSets::project(sel.selected(), doc, &self.domain.lib),
                    HighlightSets::project(sel.hovered(), doc, &self.domain.lib),
                )
            }
            Err(_) => (HighlightSets::default(), HighlightSets::default()),
        };
        let words = board_state_words(&scene.semantics, &sel_sets, &hov_sets);
        let mut states = derived.states.borrow_mut();
        for (i, w) in words.iter().enumerate() {
            states.set_word(i as u32, *w);
        }
    }

    /// Render every visible board pane's texture for this frame — the
    /// `before_paint` seam body. Steps live glides, syncs the state buffer,
    /// sizes/reallocs textures (hysteresis), rebuilds the overlay buffer on
    /// content change, and re-renders **iff** the pane's damage key moved.
    pub(crate) fn paint_board_panes(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        // Advance live glides by the wall-clock dt (clamped so a stall never
        // teleports past the ease). Settle is bit-exact, so a settled glide
        // stops producing new damage keys.
        {
            let mut gpu = self.gpu.borrow_mut();
            let Some(gpu) = gpu.as_mut() else {
                return;
            };
            let now = Instant::now();
            let dt = gpu
                .last_frame
                .map(|t| (now - t).as_secs_f64().clamp(0.0, 0.1))
                .unwrap_or(0.0);
            gpu.last_frame = Some(now);
            if dt > 0.0 {
                let mut cams = self.board_cams.borrow_mut();
                for c in cams.iter_mut() {
                    if !c.glide.settled() {
                        c.glide.step(dt);
                    }
                }
            }
        }

        self.sync_board_states();

        let mut gpu_slot = self.gpu.borrow_mut();
        let gpu = gpu_slot.as_mut().expect("checked above");
        let derived = self.derived.borrow();
        let (Some(view), Some(scene)) = (&derived.board, &derived.scene) else {
            return;
        };
        let doc_rev = self.domain.revision;
        let sets = self.highlight_sets();
        let scale = (self.scale_factor.get() as f64).max(0.1);
        let theme = damascene_core::App::theme(self);
        let maximized = self.maximized.get();
        let style_gen = self.board_style_gen();

        for pane in [PaneId::A, PaneId::B] {
            let i = pane_index(pane);
            if maximized.is_some_and(|m| m != pane) {
                continue;
            }
            if self.panes.borrow()[i].view != ViewKind::Board {
                continue;
            }
            let Some(rect) = self.pane_px.get()[i] else {
                continue;
            };
            let needed = (
                ((rect.2 as f64) * scale).round().max(1.0) as u32,
                ((rect.3 as f64) * scale).round().max(1.0) as u32,
            );

            let pg = &mut gpu.panes[i];
            // Texture lifecycle (hysteresis): reallocation invalidates the
            // damage record — the cached pixels are gone.
            let alloc = tex_alloc(needed, pg.tex.as_ref().map(|t| t.alloc));
            if pg.tex.as_ref().map(|t| t.alloc) != Some(alloc) {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("eutectic.board_pane"),
                    size: wgpu::Extent3d {
                        width: alloc.0,
                        height: alloc.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: gpu.renderer.target_format(),
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let handle = damascene_wgpu::app_texture(std::sync::Arc::new(texture.clone()));
                pg.tex = Some(PaneTexture {
                    texture,
                    handle,
                    alloc,
                });
                pg.damage.invalidate();
            }

            // The physical camera: pane cameras hold logical px/nm; the
            // texture renders in device px, so fold the scale factor in.
            let cam = self.board_cams.borrow()[i].glide.current();
            let phys = Camera {
                center: cam.center,
                zoom: cam.zoom * scale,
            };

            // Overlay: rebuild the GPU buffer only when the lowered prims
            // changed (the generation is a damage input).
            let overlay = self.build_board_overlay(view, pane, &sets, &derived.findings);
            let prims = overlay_prims(&overlay, phys.zoom);
            if prims != pg.overlay_prims {
                pg.overlay.update(device, queue, &prims, scene.anchor);
                pg.overlay_prims = prims;
                pg.overlay_gen += 1;
            }

            // Damage: render iff any input moved.
            let states = derived.states.borrow();
            let cursor = self.cursor_px.get()[i]
                .map(|(x, y)| [(x as f64 * scale) as f32, (y as f64 * scale) as f32]);
            let key = DamageKey::new(
                doc_rev,
                &phys,
                needed,
                states.generation(),
                pg.overlay_gen,
                style_gen,
            )
            .with_cursor(cursor);
            if !pg.damage.observe(key) {
                continue;
            }

            // Styles: resolve through the live theme, then apply the layer
            // panel's visibility + the stale dim (uniform writes, spec §4).
            let mut styles = gpu.styles.resolve(scene, Some(&theme));
            for (idx, p) in scene.planes.iter().enumerate() {
                if let Some(k) = plane_layer_key(&p.key)
                    && !self.layer_visible(&k)
                {
                    styles.planes[idx].visible = false;
                }
            }
            if self.domain.reload_error.is_some() {
                stale_dim(&mut styles);
            }

            // Encoder decision (WP2-should, recorded): each pane render
            // keeps `Renderer::render`'s own encoder + submit rather than
            // batching both panes into one frame encoder. The renderer's
            // frame/plane uniform buffers are shared across calls and staged
            // with `queue.write_buffer`, whose copies execute before the
            // NEXT submit — per-pane submits give write-A/submit-A/
            // write-B/submit-B ordering for free, while one batched encoder
            // would see pane B's uniforms in both passes (or force per-pane
            // uniform rings). At ≤ 2 panes, damage-gated, the extra submit
            // is noise; revisit only if a profiler ever disagrees.
            let buffers = gpu.scenes.get_or_build(device, doc_rev, scene);
            let target = pg
                .tex
                .as_ref()
                .expect("allocated above")
                .texture
                .create_view(&Default::default());
            gpu.renderer.render(
                device,
                queue,
                &RenderArgs {
                    scene: buffers,
                    overlay: (!pg.overlay.is_empty()).then_some(&pg.overlay),
                    camera: &phys,
                    styles: &styles,
                    state: &states,
                    target: &target,
                    size: needed,
                    cursor_px: cursor,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Camera accessors + gestures on EutecticApp.
// ---------------------------------------------------------------------------

impl EutecticApp {
    /// A board pane's current (possibly mid-glide) camera.
    pub fn board_camera(&self, pane: PaneId) -> Camera {
        self.board_cams.borrow()[pane_index(pane)].glide.current()
    }

    /// A board pane's glide target (tests: where a queued glide is heading).
    pub fn board_camera_target(&self, pane: PaneId) -> Camera {
        self.board_cams.borrow()[pane_index(pane)].glide.target()
    }

    /// How many times this board pane's texture has actually re-rendered —
    /// the damage probe's counter, exposed for the GPU-tier idle test (the
    /// §7 "idle = zero GPU work" proof runs against the real paint path).
    pub fn board_pane_render_count(&self, pane: PaneId) -> u64 {
        self.gpu
            .borrow()
            .as_ref()
            .map_or(0, |g| g.panes[pane_index(pane)].damage.renders)
    }

    /// Is any board pane's glide mid-flight (continuous redraw needed)?
    /// `pub` for the GPU-tier settle test.
    pub fn board_glide_active(&self) -> bool {
        self.board_cams.borrow().iter().any(|c| !c.glide.settled())
    }

    /// Queue a Fit/Reset for a board pane, consumed in `build` where the
    /// pane rect is known (hidden panes apply it on first show).
    pub(crate) fn request_board_cam(&self, pane: PaneId, req: CamRequest) {
        self.board_cams.borrow_mut()[pane_index(pane)].request = Some(req);
    }

    /// Glide a board pane's camera center to a board point at the current
    /// target zoom (findings click-to-zoom, the old `CenterOn`).
    pub(crate) fn board_center_on(&self, pane: PaneId, center: (f64, f64)) {
        let mut cams = self.board_cams.borrow_mut();
        let g = &mut cams[pane_index(pane)].glide;
        let zoom = g.target().zoom;
        g.retarget(Camera::new(center, zoom));
    }

    /// Wheel zoom-at-cursor (spec §7): retarget the pane's glide so the
    /// board point under the cursor stays fixed through the whole glide;
    /// successive ticks compound on the *target* zoom so steps chain
    /// continuously. `rect` is the pane's laid-out rect, `pos` the pointer
    /// in window-logical px, `dy` the wheel delta in the host's px
    /// convention (negative = zoom in).
    pub(crate) fn board_zoom_at(
        &self,
        pane: PaneId,
        rect: (f32, f32, f32, f32),
        pos: (f32, f32),
        dy: f32,
    ) {
        let i = pane_index(pane);
        let mut cams = self.board_cams.borrow_mut();
        let g = &mut cams[i].glide;
        let cur = g.current();
        let px = ((pos.0 - rect.0) as f64, (pos.1 - rect.1) as f64);
        let vp = (rect.2 as f64, rect.3 as f64);
        let board = cur.unproject(px, vp);
        let zoom = clamp_zoom(g.target().zoom * (-(dy as f64) * WHEEL_ZOOM_K).exp());
        g.retarget_zoom_about(
            zoom,
            ZoomAnchor {
                board,
                px,
                viewport: vp,
            },
        );
    }

    /// Snap a board pane's camera (no glide) — the pan gestures' per-event
    /// write (direct manipulation tracks the pointer exactly).
    pub(crate) fn board_snap_center(&self, pane: PaneId, center: (f64, f64)) {
        let mut cams = self.board_cams.borrow_mut();
        let g = &mut cams[pane_index(pane)].glide;
        let zoom = g.current().zoom;
        g.snap(Camera::new(center, zoom));
    }

    /// `build`-time camera settlement for a board pane with a known rect:
    /// apply the initial fit (once per pane, `fitted` flag) and any pending
    /// Fit/Reset request, then return the camera to draw with this frame.
    pub(crate) fn board_build_camera(&self, pane: PaneId, rect: (f32, f32, f32, f32)) -> Camera {
        let i = pane_index(pane);
        let bounds = self.derived.borrow().scene.as_ref().map(|s| s.bounds);
        let mut cams = self.board_cams.borrow_mut();
        let cam = &mut cams[i];
        if let Some(bounds) = bounds
            && rect.2 > 0.0
            && rect.3 > 0.0
        {
            let vp = (rect.2 as f64, rect.3 as f64);
            let fit = || {
                let mut c = Camera::fit(bounds, vp, FIT_PADDING_PX);
                c.zoom = clamp_zoom(c.zoom);
                c
            };
            let mut panes = self.panes.borrow_mut();
            if !panes[i].fitted {
                // Fit-on-first-show is a snap, not a glide (initial placement).
                cam.glide.snap(fit());
                panes[i].fitted = true;
                cam.request = None;
            } else if let Some(req) = cam.request.take() {
                match req {
                    CamRequest::Fit => cam.glide.retarget(fit()),
                    CamRequest::Reset => cam.glide.retarget(reset_camera(bounds, vp)),
                }
            }
        }
        cam.glide.current()
    }
}

// ---------------------------------------------------------------------------
// Raw winit input: free hover, crosshair, middle-drag pan (host ECAD seams).
// ---------------------------------------------------------------------------

/// An in-flight middle-drag camera pan (spec §7's pan gesture; left stays
/// select). Driven from raw `CursorMoved` deltas, so it works regardless of
/// damascene's event routing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MiddlePan {
    pane: PaneId,
    start_px: (f32, f32),
    start_center: (f64, f64),
}

/// Raw-pointer bookkeeping fed by the host's `raw_window_event` seam.
#[derive(Default)]
pub(crate) struct RawInput {
    /// Last pointer position in window-logical px.
    pub(crate) cursor: Option<(f32, f32)>,
    /// Primary button held (suppresses free-hover churn during drags).
    pub(crate) primary_down: bool,
    pub(crate) middle_pan: Option<MiddlePan>,
    /// Whether the current hover flags were written by the free-hover path
    /// (so leaving the panes clears exactly what we set).
    pub(crate) hover_ours: bool,
}

impl EutecticApp {
    /// The host's raw event tap. Returns whether the event changed app state
    /// that needs a redraw. `scale` is the window's current scale factor
    /// (physical px per logical px).
    pub(crate) fn handle_raw_event(
        &mut self,
        event: &winit::event::WindowEvent,
        scale: f64,
    ) -> bool {
        use winit::event::{ElementState, MouseButton, WindowEvent};
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self.scale_factor.set(s as f32);
        match event {
            WindowEvent::ScaleFactorChanged { .. } => true,
            WindowEvent::CursorMoved { position, .. } => {
                let pos = ((position.x / s) as f32, (position.y / s) as f32);
                self.raw_cursor_moved(pos)
            }
            WindowEvent::CursorLeft { .. } => self.raw_cursor_left(),
            WindowEvent::MouseInput { state, button, .. } => match button {
                MouseButton::Middle => self.raw_middle(*state == ElementState::Pressed),
                MouseButton::Left => {
                    self.raw.borrow_mut().primary_down = *state == ElementState::Pressed;
                    false
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// The visible pane (+ kind and rect) under a window-logical point,
    /// resolved against the rects captured at the last build, honoring the
    /// maximize rule and excluding each pane's floating tool strip (a
    /// pointer over the strip is chrome, not canvas — matching the old
    /// keyed-El hit-test).
    pub(crate) fn raw_pane_at(&self, pos: (f32, f32)) -> Option<(PaneId, ViewKind, PaneRect)> {
        let inside = |r: (f32, f32, f32, f32)| {
            pos.0 >= r.0 && pos.0 <= r.0 + r.2 && pos.1 >= r.1 && pos.1 <= r.1 + r.3
        };
        let candidates: [PaneId; 2] = [PaneId::A, PaneId::B];
        for pane in candidates {
            let i = pane_index(pane);
            if self.maximized.get().is_some_and(|m| m != pane) {
                continue;
            }
            let Some(rect) = self.pane_px.get()[i] else {
                continue;
            };
            if !inside(rect) {
                continue;
            }
            if let Some(strip) = self.strip_px.get()[i]
                && inside(strip)
            {
                return None;
            }
            let view = self.panes.borrow()[i].view;
            return Some((pane, view, rect));
        }
        None
    }

    /// Update the per-pane crosshair cursor (pane-local logical px; `None`
    /// clears every pane). Returns whether anything changed.
    fn set_crosshair(&self, at: Option<(PaneId, (f32, f32))>) -> bool {
        let mut cur = self.cursor_px.get();
        let next = match at {
            Some((pane, local)) => {
                let mut n = [None, None];
                n[pane_index(pane)] = Some(local);
                n
            }
            None => [None, None],
        };
        if cur != next {
            cur = next;
            self.cursor_px.set(cur);
            true
        } else {
            false
        }
    }

    fn raw_cursor_moved(&mut self, pos: (f32, f32)) -> bool {
        self.raw.borrow_mut().cursor = Some(pos);

        // Middle-drag pan: direct manipulation, snap per event.
        let mp = self.raw.borrow().middle_pan;
        if let Some(mp) = mp {
            let zoom = self.board_camera(mp.pane).zoom;
            if zoom > 0.0 {
                let center = (
                    mp.start_center.0 - ((pos.0 - mp.start_px.0) as f64) / zoom,
                    mp.start_center.1 + ((pos.1 - mp.start_px.1) as f64) / zoom,
                );
                self.board_snap_center(mp.pane, center);
            }
            if let Some(rect) = self.pane_px.get()[pane_index(mp.pane)] {
                self.set_crosshair(Some((mp.pane, (pos.0 - rect.0, pos.1 - rect.1))));
            }
            return true;
        }

        // Modal chrome owns the pointer: no hover, no crosshair.
        if self.libraries_open.get() || self.open_menu.borrow().is_some() {
            let mut changed = self.set_crosshair(None);
            changed |= self.clear_free_hover();
            return changed;
        }

        match self.raw_pane_at(pos) {
            Some((pane, ViewKind::Board, rect)) => {
                let local = (pos.0 - rect.0, pos.1 - rect.1);
                self.set_crosshair(Some((pane, local)));
                let cam = self.board_camera(pane);
                let p = board_unproject(&cam, rect, pos);
                self.cursor_board_mm
                    .set(Some((p.x as f32 / 1e6, p.y as f32 / 1e6)));

                // No hover churn during an active drag gesture.
                let busy = {
                    let raw = self.raw.borrow();
                    raw.primary_down
                        || self.drag.borrow().is_some()
                        || self.trace_drag.borrow().is_some()
                        || self.camera_pan.borrow().is_some()
                };
                if !busy {
                    // Live previews that track the pointer (free hover makes
                    // these smooth; the event path still updates them too).
                    if self.tool_for(ViewKind::Board) == crate::tool::Tool::Measure {
                        self.measure_pane.set(pane);
                        let mut m = self.measure.get();
                        m.hover(p);
                        self.measure.set(m);
                    }
                    if let Some(r) = self.route.borrow_mut().as_mut() {
                        r.hover(p);
                    }
                    // The pick: same candidates, same kernel as the click path.
                    let derived = self.derived.borrow();
                    if let Some(view) = &derived.board {
                        let tol = pick::tolerance_nm(
                            crate::app::events::PICK_TOL_PX,
                            zoom_px_per_mm(&cam),
                        );
                        let hit =
                            pick::resolve(&view.candidates, p, tol, |id| self.layer_id_visible(id));
                        let mut sel = self.domain.selection.borrow_mut();
                        match hit {
                            Some(pk) => sel.hover_only(pk.id),
                            None => sel.clear_hover(),
                        }
                        drop(sel);
                        self.raw.borrow_mut().hover_ours = true;
                    }
                }
                true
            }
            // The schematic pane keeps its El-event hover path (WP3 territory).
            Some((_, ViewKind::Schematic, _)) => {
                let mut changed = self.set_crosshair(None);
                changed |= self.clear_free_hover();
                changed
            }
            None => {
                let mut changed = self.set_crosshair(None);
                changed |= self.clear_free_hover();
                changed
            }
        }
    }

    /// Clear a free-hover flag we set (leaving the pane / entering chrome).
    fn clear_free_hover(&self) -> bool {
        let mut raw = self.raw.borrow_mut();
        if raw.hover_ours {
            raw.hover_ours = false;
            drop(raw);
            self.domain.selection.borrow_mut().clear_hover();
            true
        } else {
            false
        }
    }

    fn raw_cursor_left(&mut self) -> bool {
        let mut raw = self.raw.borrow_mut();
        raw.cursor = None;
        raw.middle_pan = None;
        raw.primary_down = false;
        drop(raw);
        let mut changed = self.set_crosshair(None);
        changed |= self.clear_free_hover();
        changed
    }

    fn raw_middle(&mut self, pressed: bool) -> bool {
        if !pressed {
            return self.raw.borrow_mut().middle_pan.take().is_some();
        }
        if self.libraries_open.get() || self.open_menu.borrow().is_some() {
            return false;
        }
        let Some(pos) = self.raw.borrow().cursor else {
            return false;
        };
        let Some((pane, ViewKind::Board, _)) = self.raw_pane_at(pos) else {
            return false;
        };
        // Interrupt any live glide: the pan is direct manipulation.
        let cam = self.board_camera(pane);
        self.board_cams.borrow_mut()[pane_index(pane)]
            .glide
            .snap(cam);
        self.raw.borrow_mut().middle_pan = Some(MiddlePan {
            pane,
            start_px: pos,
            start_center: cam.center,
        });
        true
    }
}

// ---------------------------------------------------------------------------
// The host trait wiring.
// ---------------------------------------------------------------------------

impl crate::host::WinitWgpuApp for EutecticApp {
    fn gpu_setup(
        &mut self,
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        adapter: &wgpu::Adapter,
        surface_format: wgpu::TextureFormat,
    ) {
        self.setup_gpu(device, adapter, surface_format);
    }

    fn before_paint(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        self.paint_board_panes(device, queue);
    }

    fn raw_window_event(&mut self, event: &winit::event::WindowEvent, scale_factor: f64) -> bool {
        self.handle_raw_event(event, scale_factor)
    }
}

#[cfg(test)]
mod tests;
