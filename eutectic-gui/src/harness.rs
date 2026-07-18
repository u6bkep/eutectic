//! Multi-frame headless render harness — the faithful stand-in for the winit
//! host.
//!
//! # Why a harness at all
//!
//! The app's pane cameras settle across frames: `build` captures each pane's
//! laid-out rect (via `cx.rect_of_key`, one frame stale by construction) and
//! applies the pending camera work — the initial fit-on-first-show and any
//! queued Fit/Reset request — against it (`pane_build_camera`). A single
//! fresh-state frame therefore renders the *unfitted* reset framing (content
//! as a ~30 px speck): frame 1 lays the panes out, frame 2's build fits
//! against the now-known rects. The live winit host gets this for free by
//! carrying one persistent `UiState` across frames; this harness reproduces
//! that loop exactly, CPU-only, so the dumped fixture artifacts show the same
//! fitted canvas a user sees.
//!
//! # The host loop we mirror
//!
//! From the in-tree host copy at `src/host.rs` (`RunnerCore`'s redraw path),
//! each frame runs, in order:
//!
//! 1. `WinitWgpuApp::before_build(&mut app)` — per-frame app state (mailbox
//!    drain: reloads / conflicts).
//! 2. `let cx = BuildCx::new(&theme).with_ui_state(…)` — **build reads the
//!    persistent `UiState`**, so `cx.rect_of_key(key)` returns each pane's
//!    rect *as of the last layout*; `pane_build_camera` consumes it.
//! 3. `let tree = app.build(&cx)`.
//! 4. layout runs (inside `render_bundle_with_theme` here; `prepare` in the
//!    host), writing this frame's rects into the `UiState` for the next
//!    build.
//!
//! # Frame count — the settle signal
//!
//! We loop until [`EutecticApp::cameras_pending`] reports every visible
//! pane's camera settled (initial fit applied, no queued Fit/Reset), capped
//! at [`MAX_FRAMES`] and always running at least [`MIN_FRAMES`] (= 2). This
//! is the owned-camera replacement for the old "viewport-request queue
//! drained empty" signal (the queue died with the viewport path, WP3): the
//! camera requests now live in app state, and `cameras_pending` is their
//! visibility. For the current fixtures it settles in exactly two frames —
//! frame 1 lays the panes out (cameras still un-fitted), frame 2 builds
//! against the known rects and fits — and the *final* frame's bundle is the
//! one returned, so the artifact shows the fitted canvas.

use damascene_core::prelude::*;
use damascene_core::state::UiState;

use crate::app::{EutecticApp, PaneId};

/// Always run at least this many frames: the first lays out, the second
/// builds against the laid-out rects (fit applied). A single frame would
/// dump the unfitted camera.
pub const MIN_FRAMES: usize = 2;

/// Safety cap on the settle loop, so a fixture that never settles can't
/// spin forever. Generous relative to the two frames the real fixtures need.
pub const MAX_FRAMES: usize = 8;

/// The result of driving a scene to its settled frame: the final frame's
/// [`Bundle`] plus the persistent [`UiState`] and built tree that produced
/// it, so callers (the coverage assertion) can read post-fit rects.
pub struct Rendered {
    /// The final frame's render bundle (svg / tree dump / draw ops / lint).
    pub bundle: Bundle,
    /// The persistent UI state after the final layout — holds the laid-out
    /// pane rects the app cameras fitted against.
    pub ui: UiState,
    /// The final frame's built + laid-out tree.
    pub tree: El,
    /// How many frames actually ran (for tests / diagnostics).
    pub frames: usize,
}

/// Drive `app` through the host-mirroring frame loop at `viewport` and return
/// the settled final frame. See the module docs for the per-frame ordering
/// and its correspondence to the in-tree host's redraw path.
pub fn render_settled(app: &mut EutecticApp, viewport: Rect) -> Rendered {
    let mut ui = UiState::new();
    let mut last;
    let mut frames = 0;

    loop {
        // (1) Per-frame app state (mailbox drain — reloads / conflicts).
        app.before_build();

        // (2)+(3) Build against the PERSISTENT UiState, so cx.rect_of_key
        // reads the rects the previous frame's layout wrote — that is where
        // pane_build_camera applies the fit-on-first-show + Fit/Reset
        // requests. The immutable borrow of `ui` ends when `build` returns.
        let theme = app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(&ui)
                .with_viewport(viewport.w, viewport.h);
            app.build(&cx)
        };

        // (4) Layout runs inside the bundle render, writing this frame's
        // rects into `ui` for the next build.
        let bundle = render_bundle_with_theme(&mut tree, &mut ui, viewport, &theme);

        frames += 1;
        last = (bundle, tree);

        // Settle once we've met the minimum AND no visible pane owes camera
        // work (initial fit or a queued Fit/Reset) — the app-camera settle
        // signal — or the safety cap is hit. One before_build per frame.
        if (frames >= MIN_FRAMES && !app.cameras_pending()) || frames >= MAX_FRAMES {
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

/// Minimum fraction of a canvas pane's extent (in at least one axis) the
/// fitted content bounding box must occupy. Fit frames content into the pane
/// with a small padding, so on a healthy fit the content fills most of the
/// shorter axis; 30 % is a loose floor that a broken fit (content left at
/// the unfitted `zoom = 1.0` speck) fails by a wide margin.
pub const MIN_COVERAGE: f32 = 0.30;

/// The reset/unfitted zoom (px/mm) a pane camera renders at before any fit
/// applies. A fitted view's zoom is nowhere near this; the coverage
/// assertion treats a pane still sitting at this zoom as an outright fit
/// failure.
const UNFITTED_ZOOM: f32 = 1.0;

/// Assert that every visible canvas pane in a settled render shows fitted
/// content — the "did the fit machinery actually run" gate on top of
/// damascene's own lint. For each `key` (a pane's `canvas_key`) we require
/// BOTH:
///
/// - the fitted `zoom` differs from the unfitted default ([`UNFITTED_ZOOM`]),
///   and
/// - the content bounding box, projected through that zoom, spans at least
///   [`MIN_COVERAGE`] of the pane in one axis.
///
/// Either signal alone would catch the common failure (fit never applied ⇒
/// `zoom == 1.0` and a ~30 px speck), but requiring both makes the assertion
/// fail loudly if *either* the request-application path breaks (zoom frozen)
/// or the content bounds collapse — the two ways the fit pipeline can rot
/// independently.
///
/// WP3: BOTH view kinds run on app-owned cameras, so both halves read the
/// pane camera + its view kind's renderer-scene bounds — the same behavioral
/// meaning (fitted zoom + real coverage) the viewport-era assertion had.
pub fn assert_content_coverage(scene: &str, app: &EutecticApp, r: &Rendered, keys: &[&str]) {
    for &key in keys {
        let pane_id = PaneId::all_slots()
            .find(|pane| pane.canvas_key() == key)
            .unwrap_or_else(|| panic!("scene `{scene}`: unknown canvas key `{key}`"));
        let pane =
            r.ui.rect_of_key(key)
                .unwrap_or_else(|| panic!("scene `{scene}`: canvas `{key}` has no laid-out rect"));

        let cam = app.pane_camera(pane_id);
        let view = app.pane_view(pane_id);
        let bounds = app.pane_scene_bounds(pane_id).unwrap_or_else(|| {
            panic!("scene `{scene}`: pane `{key}` ({view:?}) has no renderer scene")
        });
        let zoom = crate::app::canvas_pane::zoom_px_per_mm(&cam);
        let mm = eutectic_core::coord::MM as f32;
        let (content_w, content_h) = (
            (bounds.2 - bounds.0) as f32 / mm,
            (bounds.3 - bounds.1) as f32 / mm,
        );

        assert!(
            (zoom - UNFITTED_ZOOM).abs() > 1e-3,
            "scene `{scene}`: canvas `{key}` is still at the unfitted zoom \
             {UNFITTED_ZOOM} — the fit never applied",
        );
        // Content extent on screen = zoom * content-space extent. Coverage is
        // the larger of the two per-axis fractions of the pane's inner extent.
        let cov_x = (content_w * zoom) / pane.w.max(f32::EPSILON);
        let cov_y = (content_h * zoom) / pane.h.max(f32::EPSILON);
        let coverage = cov_x.max(cov_y);
        assert!(
            coverage >= MIN_COVERAGE,
            "scene `{scene}`: canvas `{key}` fitted content covers only {:.1}% of \
             the pane (need ≥ {:.0}%) — fit produced a speck (zoom {:.3}, content \
             {:.1}×{:.1}, pane {:.1}×{:.1})",
            coverage * 100.0,
            MIN_COVERAGE * 100.0,
            zoom,
            content_w,
            content_h,
            pane.w,
            pane.h,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ViewKind;
    use crate::fixtures;

    fn viewport() -> Rect {
        Rect::new(0.0, 0.0, 1280.0, 800.0)
    }

    /// The board fixture settles in exactly two frames and its canvas ends up
    /// fitted — the app-owned camera moved off the unfitted reset zoom
    /// (frame 1 lays the pane out, frame 2's build snaps the fit against the
    /// known rect — the owned-canvas twin of the persistent-UiState loop).
    #[test]
    fn board_settles_fitted_in_two_frames() {
        let mut app = fixtures::board();
        let r = render_settled(&mut app, viewport());
        assert_eq!(
            r.frames, MIN_FRAMES,
            "the board fixture settles in two frames"
        );
        let zoom = crate::app::canvas_pane::zoom_px_per_mm(&app.pane_camera(PaneId::A));
        assert!(
            (zoom - 1.0).abs() > 1e-3,
            "the board camera must be fitted (zoom off the unfitted 1.0 px/mm), got {zoom}",
        );
        // The coverage gate agrees on the fitted render.
        assert_content_coverage("board", &app, &r, &[PaneId::A.canvas_key()]);
    }

    /// The SCHEMATIC pane settles through the same app-camera loop: the dual
    /// fixture's pane B (schematic) fits within the settle window, off the
    /// reset zoom, with real coverage (WP3: no viewport requests anywhere).
    #[test]
    fn schematic_settles_fitted() {
        let mut app = fixtures::dual_cross_highlight();
        let r = render_settled(&mut app, viewport());
        assert!(!app.cameras_pending(), "every visible pane settled");
        assert_eq!(
            app.pane_view(PaneId::B),
            ViewKind::Schematic,
            "pane B is the schematic pane"
        );
        let zoom = crate::app::canvas_pane::zoom_px_per_mm(&app.pane_camera(PaneId::B));
        assert!(
            (zoom - 1.0).abs() > 1e-3,
            "the schematic camera must be fitted, got {zoom}"
        );
        assert_content_coverage(
            "dual",
            &app,
            &r,
            &[PaneId::A.canvas_key(), PaneId::B.canvas_key()],
        );
    }

    /// The coverage assertion FAILS LOUDLY on an unfitted render: a single
    /// raw frame (the exact pre-harness bug) has no laid-out pane rect yet,
    /// so the camera never fits and stays at the reset zoom — the assertion
    /// must panic rather than pass.
    #[test]
    #[should_panic(expected = "unfitted zoom")]
    fn coverage_rejects_an_unfitted_render() {
        let mut app = fixtures::board();
        let vp = viewport();
        // One frame only, with a FRESH UiState per the old broken path: no
        // rect from a prior layout, so the camera fit cannot apply.
        app.before_build();
        let theme = app.theme();
        let mut ui = UiState::new();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(&ui)
                .with_viewport(vp.w, vp.h);
            app.build(&cx)
        };
        let bundle = render_bundle_with_theme(&mut tree, &mut ui, vp, &theme);
        let r = Rendered {
            bundle,
            ui,
            tree,
            frames: 1,
        };
        assert_content_coverage("board-unfitted", &app, &r, &[PaneId::A.canvas_key()]);
    }
}
