//! Two-frame headless render harness — the faithful stand-in for the winit host.
//!
//! # Why a harness at all
//!
//! [`render_bundle_themed`](damascene_core::render_bundle_themed) builds a bundle
//! from a *fresh* [`UiState`], so any pan/zoom the app queues through
//! [`App::drain_viewport_requests`] is dropped on the floor: the app's initial
//! [`FitContent`](damascene_core::viewport::ViewportRequest::FitContent) never
//! reaches layout, and the canvas renders at the unfitted reset framing (`zoom =
//! 1.0`) — content draws as a ~30 px speck in the pane corner. The live winit host
//! avoids this by carrying **one persistent `UiState` across frames** and applying
//! each frame's requests during the *next* layout; the fit only becomes visible on
//! the frame after it was queued.
//!
//! This harness reproduces that loop exactly, CPU-only, so the dumped fixture
//! artifacts show the same fitted canvas a user sees.
//!
//! # The host loop we mirror
//!
//! From `damascene-winit-wgpu-0.4.5/src/lib.rs` (`RunnerCore`'s redraw path, the
//! `frame::build` / `frame::prepare` spans), each frame runs, in order:
//!
//! 1. `WinitWgpuApp::before_build(&mut app)` — the app queues per-frame state
//!    (our fixtures queue the initial `FitContent` here).
//! 2. `let cx = BuildCx::new(&theme).with_ui_state(gfx.renderer.ui_state())…
//!    .with_viewport(w, h)` — **build reads the persistent `UiState`**, so
//!    `cx.viewport_view(key)` returns the camera *as of the last layout*.
//! 3. `let tree = app.build(&cx)`.
//! 4. `gfx.renderer.push_viewport_requests(app.drain_viewport_requests())` — the
//!    frame's requests are handed to the same `UiState`, still **pending**.
//! 5. `gfx.renderer.prepare(…, &mut tree, viewport, scale)` — layout runs and
//!    **consumes** the pending requests against the live per-pane rect + content
//!    extents, writing the resulting pan/zoom into `UiState`.
//!
//! [`render_bundle_with_theme`](damascene_core::render_bundle_with_theme) is the
//! CPU analogue of step 5: it calls `layout(root, ui_state, viewport)`, which is
//! where pending viewport requests are applied. So per frame the harness does:
//! `before_build` → build with `with_ui_state(&ui)` → `push_viewport_requests(
//! drain)` → `render_bundle_with_theme(&mut tree, &mut ui, …)`. Pushing *before*
//! the render matches the host's push-before-prepare ordering.
//!
//! # Frame count
//!
//! We loop until [`App::drain_viewport_requests`] comes back empty, capped at
//! [`MAX_FRAMES`], and always run at least [`MIN_FRAMES`] (= 2). This is more
//! faithful than a hard-coded two frames: the host runs frames until the app
//! stops requesting redraws, and a scene that queues a follow-up request (a pane
//! that becomes visible only after a first-frame layout, say) needs the extra
//! pass. For the current fixtures it settles in exactly two frames — frame 1
//! queues + applies the fit (build still saw `zoom = 1.0`), frame 2 builds against
//! the fitted camera and queues nothing — and the *final* frame's bundle is the
//! one returned, so the artifact shows the fitted canvas and the toolbar zoom
//! readout is the fitted zoom, not `100%`.

use damascene_core::prelude::*;
use damascene_core::state::UiState;

use crate::app::EcadApp;

/// Always run at least this many frames: the first queues the fit, the second
/// renders against it. A single frame would dump the unfitted camera.
pub const MIN_FRAMES: usize = 2;

/// Safety cap on the settle loop, so a fixture that never stops requesting can't
/// spin forever. Generous relative to the two frames the real fixtures need.
pub const MAX_FRAMES: usize = 8;

/// The result of driving a scene to its settled frame: the final frame's
/// [`Bundle`] plus the persistent [`UiState`] and built tree that produced it, so
/// callers (the coverage assertion) can read post-fit camera + geometry.
pub struct Rendered {
    /// The final frame's render bundle (svg / tree dump / draw ops / lint).
    pub bundle: Bundle,
    /// The persistent UI state after the final layout — holds the fitted per-pane
    /// cameras and viewport content metrics.
    pub ui: UiState,
    /// The final frame's built + laid-out tree.
    pub tree: El,
    /// How many frames actually ran (for tests / diagnostics).
    pub frames: usize,
}

/// Drive `app` through the host-mirroring frame loop at `viewport` and return the
/// settled final frame. See the module docs for the per-frame ordering and its
/// correspondence to `damascene-winit-wgpu`'s redraw path.
pub fn render_settled(app: &mut EcadApp, viewport: Rect) -> Rendered {
    let mut ui = UiState::new();
    let mut last;
    let mut frames = 0;

    loop {
        // (1) Per-frame app state: our fixtures queue the initial FitContent here.
        app.before_build();

        // (2)+(3) Build against the PERSISTENT UiState, so cx.viewport_view(key)
        // reads the camera the previous frame's layout wrote (the zoom readout in
        // build consumes it). The immutable borrow of `ui` ends when `build`
        // returns, freeing `ui` for the mutable steps below.
        let theme = app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(&ui)
                .with_viewport(viewport.w, viewport.h);
            app.build(&cx)
        };

        // (4) Hand this frame's requests to the same UiState — still pending until
        // layout consumes them, exactly as the host pushes before prepare. Whether
        // this frame emitted any is the settle signal: a frame that queued a fit has
        // more work for the *next* layout to reflect.
        let requests = app.drain_viewport_requests();
        let queued_this_frame = !requests.is_empty();
        ui.push_viewport_requests(requests);

        // (5) render_bundle_with_theme runs layout(root, &mut ui, viewport), which
        // applies the pending requests against the live rects + content extents and
        // writes the fitted cameras back into `ui`.
        let bundle = render_bundle_with_theme(&mut tree, &mut ui, viewport, &theme);

        frames += 1;
        last = (bundle, tree);

        // Settle once we've met the minimum AND the frame just rendered queued no
        // new requests (so a further frame would build + lay out identically), or the
        // safety cap is hit. One before_build per frame — no double-firing of the fit.
        if (frames >= MIN_FRAMES && !queued_this_frame) || frames >= MAX_FRAMES {
            break;
        }
    }

    let (bundle, tree) = last;
    Rendered {
        bundle,
        ui,
        tree,
        frames,
    }
}

/// Minimum fraction of a canvas pane's extent (in at least one axis) the fitted
/// content bounding box must occupy. FitContent frames content into the pane with
/// a small padding, so on a healthy fit the content fills most of the shorter axis;
/// 30 % is a loose floor that a broken fit (content left at the unfitted `zoom =
/// 1.0` speck) fails by a wide margin.
pub const MIN_COVERAGE: f32 = 0.30;

/// The reset/unfitted zoom the canvas renders at before any FitContent is applied.
/// A fitted board's zoom is nowhere near this; the coverage assertion treats a
/// canvas still sitting at this zoom as an outright fit failure.
const UNFITTED_ZOOM: f32 = 1.0;

/// Assert that every visible canvas viewport in a settled render shows fitted
/// content — the "did the fit machinery actually run" gate on top of damascene's
/// own lint. For each `key` (a pane's `canvas_key`) we require BOTH:
///
/// - the fitted `zoom` differs from the unfitted default ([`UNFITTED_ZOOM`]), and
/// - the content bounding box, projected through that zoom, spans at least
///   [`MIN_COVERAGE`] of the pane in one axis.
///
/// Either signal alone would catch the common failure (fit never applied ⇒ `zoom
/// == 1.0` and a ~30 px speck), but requiring both makes the assertion fail loudly
/// if *either* the request-application path breaks (zoom frozen) or the
/// content-extent measurement breaks (bounds collapse) — the two ways the fit
/// pipeline can rot independently.
///
/// `scene` names the fixture for the panic message. `keys` are the visible canvas
/// viewport keys to check (skip hidden panes and no-document scenes).
pub fn assert_content_coverage(scene: &str, r: &Rendered, keys: &[&str]) {
    for &key in keys {
        let view = r
            .ui
            .viewport_view_by_key(key)
            .unwrap_or_else(|| panic!("scene `{scene}`: canvas viewport `{key}` was not laid out"));
        assert!(
            (view.zoom - UNFITTED_ZOOM).abs() > 1e-3,
            "scene `{scene}`: canvas `{key}` is still at the unfitted zoom \
             {UNFITTED_ZOOM} — the FitContent request never reached layout",
        );

        // Content bounds are keyed by computed_id, not the app key; map key →
        // computed_id by walking the built tree for the viewport node.
        let id = computed_id_for_key(&r.tree, key).unwrap_or_else(|| {
            panic!("scene `{scene}`: no node carries key `{key}` in the built tree")
        });
        let content = r.ui.viewport_content_bounds(&id).unwrap_or_else(|| {
            panic!(
                "scene `{scene}`: canvas `{key}` has no measured content bounds \
                 (empty viewport?)"
            )
        });
        let pane =
            r.ui.rect_of_key(key)
                .unwrap_or_else(|| panic!("scene `{scene}`: canvas `{key}` has no laid-out rect"));

        // Content extent on screen = zoom * content-space extent (the transform is
        // origin-anchored uniform scale). Coverage is the larger of the two per-axis
        // fractions of the pane's inner extent.
        let cov_x = (content.w * view.zoom) / pane.w.max(f32::EPSILON);
        let cov_y = (content.h * view.zoom) / pane.h.max(f32::EPSILON);
        let coverage = cov_x.max(cov_y);
        assert!(
            coverage >= MIN_COVERAGE,
            "scene `{scene}`: canvas `{key}` fitted content covers only {:.1}% of \
             the pane (need ≥ {:.0}%) — fit produced a speck (zoom {:.3}, content \
             {:.1}×{:.1}, pane {:.1}×{:.1})",
            coverage * 100.0,
            MIN_COVERAGE * 100.0,
            view.zoom,
            content.w,
            content.h,
            pane.w,
            pane.h,
        );
    }
}

/// The measured **content-space** bounds of the viewport carrying `key` — the
/// laid-out extent of its child, pre-transform (damascene's
/// `viewport_content_bounds`). This is the ground truth the pointer↔board
/// composition's content-rect assumption is pinned against (see
/// `Canvas::content_rect`).
pub fn content_bounds_of(r: &Rendered, key: &str) -> Option<damascene_core::prelude::Rect> {
    let id = computed_id_for_key(&r.tree, key)?;
    r.ui.viewport_content_bounds(&id)
}

/// Depth-first search for the `computed_id` of the node carrying `key`.
fn computed_id_for_key(root: &El, key: &str) -> Option<String> {
    if root.key.as_deref() == Some(key) {
        return Some(root.computed_id.to_string());
    }
    for child in &root.children {
        if let Some(id) = computed_id_for_key(child, key) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PaneId;
    use crate::fixtures;

    fn viewport() -> Rect {
        Rect::new(0.0, 0.0, 1280.0, 800.0)
    }

    /// The board fixture settles in exactly two frames and its canvas ends up
    /// fitted — zoom moved off the unfitted default, which is the whole point of
    /// the persistent-UiState loop.
    #[test]
    fn board_settles_fitted_in_two_frames() {
        let mut app = fixtures::board();
        let r = render_settled(&mut app, viewport());
        assert_eq!(
            r.frames, MIN_FRAMES,
            "the board fixture settles in two frames"
        );
        let view =
            r.ui.viewport_view_by_key(PaneId::A.canvas_key())
                .expect("board pane A laid out");
        assert!(
            (view.zoom - 1.0).abs() > 1e-3,
            "the board canvas must be fitted (zoom off the unfitted 1.0), got {}",
            view.zoom
        );
        // The coverage gate agrees on the fitted render.
        assert_content_coverage("board", &r, &[PaneId::A.canvas_key()]);
    }

    /// The coverage assertion FAILS LOUDLY on an unfitted render: rendering a single
    /// raw frame (no persistent UiState carry-over, the exact pre-harness bug) leaves
    /// the canvas at zoom 1.0, and the assertion must panic rather than pass.
    #[test]
    #[should_panic(expected = "unfitted zoom")]
    fn coverage_rejects_an_unfitted_render() {
        let mut app = fixtures::board();
        let vp = viewport();
        // One frame only, with a FRESH UiState per the old broken path: build queues
        // the fit but nothing carries it into a second layout, so zoom stays 1.0.
        app.before_build();
        let theme = app.theme();
        let mut ui = UiState::new();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(&ui)
                .with_viewport(vp.w, vp.h);
            app.build(&cx)
        };
        // Deliberately DROP the drained requests (never push them) — the speck bug.
        let _ = app.drain_viewport_requests();
        let bundle = render_bundle_with_theme(&mut tree, &mut ui, vp, &theme);
        let r = Rendered {
            bundle,
            ui,
            tree,
            frames: 1,
        };
        assert_content_coverage("board-unfitted", &r, &[PaneId::A.canvas_key()]);
    }
}
