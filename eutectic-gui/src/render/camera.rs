//! Camera & precision (renderer-spec §7).
//!
//! Integer-nm coordinates exceed the f32 mantissa, so the camera composes
//! view transforms in **f64 on the CPU** (center in nm, zoom in px/nm) and
//! uploads a per-frame f32 transform that folds in the scene's anchor offset
//! ([`Camera::view_transform`] — vertex data is anchor-relative f32).
//! Project/unproject (pointer ↔ board nm) is f64 CPU math on the same state;
//! picking never round-trips through the GPU.
//!
//! [`CameraGlide`] is the motion style: camera *targets* glide through a
//! short interruptible critically-damped ease (~100–150 ms); wheel ticks
//! retarget the same filter so successive steps feel continuous. It is a
//! pure function of `(state, target, dt)` — no clocks, no GPU — so the
//! convergence/interruption tests below run headless.

use eutectic_core::coord::Nm;

/// Screen convention: board y points **up**, screen y points **down**; the
/// projection flips y. Zoom is px per nm (1 px/mm ⇒ `1e-6`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    /// Board-frame center of the view, in nm (f64: sub-nm interpolation
    /// during glides; the f64 mantissa holds any board coordinate exactly).
    pub center: (f64, f64),
    /// Zoom in px per nm.
    pub zoom: f64,
}

impl Camera {
    /// A camera centered on `center_nm` at `zoom` px/nm.
    pub fn new(center_nm: (f64, f64), zoom: f64) -> Camera {
        Camera {
            center: center_nm,
            zoom,
        }
    }

    /// Project a board point (nm) to pane pixels (origin top-left, y down).
    pub fn project(&self, p: (f64, f64), viewport: (f64, f64)) -> (f64, f64) {
        (
            (p.0 - self.center.0) * self.zoom + viewport.0 / 2.0,
            (self.center.1 - p.1) * self.zoom + viewport.1 / 2.0,
        )
    }

    /// Unproject pane pixels back to board nm — the picking transform.
    /// Exact inverse of [`project`](Camera::project) up to f64 rounding.
    pub fn unproject(&self, px: (f64, f64), viewport: (f64, f64)) -> (f64, f64) {
        (
            self.center.0 + (px.0 - viewport.0 / 2.0) / self.zoom,
            self.center.1 - (px.1 - viewport.1 / 2.0) / self.zoom,
        )
    }

    /// The per-frame f32 upload: `(origin_px, scale)` such that an
    /// anchor-relative vertex `q` (f32 nm, y-up) lands at pane pixel
    /// `origin_px + (q.x, -q.y) * scale`. The anchor offset is folded in
    /// **in f64** here; only the small residual reaches f32 (renderer-spec
    /// §7 — this is the "f32 matrix folding the anchor offset", spelled as
    /// the affine pair the shaders consume).
    pub fn view_transform(
        &self,
        anchor: eutectic_core::coord::Point,
        viewport: (f32, f32),
    ) -> ([f32; 2], f32) {
        let o = self.project(
            (anchor.x as f64, anchor.y as f64),
            (viewport.0 as f64, viewport.1 as f64),
        );
        ([o.0 as f32, o.1 as f32], self.zoom as f32)
    }

    /// The camera that frames `bounds` (nm) inside `viewport` px with
    /// `margin_px` on every side — fit-to-content and zoom-to-rect are both
    /// this. Degenerate bounds/viewports get a defensive 1 px/mm zoom.
    pub fn fit(bounds: (Nm, Nm, Nm, Nm), viewport: (f64, f64), margin_px: f64) -> Camera {
        let (x0, y0, x1, y1) = bounds;
        let (w, h) = ((x1 - x0) as f64, (y1 - y0) as f64);
        let avail = (viewport.0 - 2.0 * margin_px, viewport.1 - 2.0 * margin_px);
        let zoom = if w > 0.0 && h > 0.0 && avail.0 > 0.0 && avail.1 > 0.0 {
            (avail.0 / w).min(avail.1 / h)
        } else {
            1e-6
        };
        Camera {
            center: ((x0 + x1) as f64 / 2.0, (y0 + y1) as f64 / 2.0),
            zoom,
        }
    }
}

/// The critically-damped glide stiffness (rad/s). Settling to ~1 % takes
/// ≈ `6.6/ω` ⇒ ~130 ms — inside the spec's 100–150 ms window.
pub const GLIDE_OMEGA: f64 = 50.0;

/// Below these deltas the glide snaps to its target and reports settled: a
/// quarter-pixel of on-screen center error / drift rate, and a log-zoom
/// error of 1e-4 (~0.01 %) — all invisible.
const SETTLE_PX: f64 = 0.25;
const SETTLE_PX_PER_S: f64 = 5.0;
const SETTLE_LOG_ZOOM: f64 = 1e-4;

/// One critically-damped scalar spring: position + velocity.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spring {
    pub x: f64,
    pub v: f64,
}

impl Spring {
    /// Advance toward `target` by `dt` seconds with stiffness `omega`
    /// (closed-form critically-damped step — unconditionally stable for any
    /// `dt`, so a dropped frame cannot overshoot).
    pub fn step(self, target: f64, dt: f64, omega: f64) -> Spring {
        let dx = self.x - target;
        let c2 = self.v + omega * dx;
        let e = (-omega * dt).exp();
        Spring {
            x: target + (dx + c2 * dt) * e,
            v: (self.v - c2 * omega * dt) * e,
        }
    }
}

/// The fixed board point a zoom-at-cursor glide is anchored to (WP2): the
/// board point under the cursor, the cursor's pane px, and the pane size the
/// px are relative to. While anchored, [`CameraGlide::step`] derives the
/// center from the zoom each frame (`center = anchor ∓ offset/zoom`), so the
/// anchored board point stays under the cursor **through the whole glide**,
/// not just at the tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomAnchor {
    /// The anchored board point (nm).
    pub board: (f64, f64),
    /// The cursor position in pane px (origin top-left, y down).
    pub px: (f64, f64),
    /// The pane viewport (px) the cursor px are relative to.
    pub viewport: (f64, f64),
}

impl ZoomAnchor {
    /// The camera center that puts [`board`](Self::board) under
    /// [`px`](Self::px) at `zoom` — the inversion of [`Camera::unproject`].
    fn center_at(&self, zoom: f64) -> (f64, f64) {
        (
            self.board.0 - (self.px.0 - self.viewport.0 / 2.0) / zoom,
            self.board.1 + (self.px.1 - self.viewport.1 / 2.0) / zoom,
        )
    }
}

/// The camera glide filter (renderer-spec §7 motion style): the *current*
/// camera eases toward a *target* camera through critically-damped springs —
/// center in nm, zoom in **log space** (so a 2× zoom-in and a 2× zoom-out
/// feel symmetric). Interruptible: [`retarget`](CameraGlide::retarget) keeps
/// the live position/velocity, so wheel ticks mid-glide chain smoothly.
/// Nothing else animates — selection/hover are instant state changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraGlide {
    cx: Spring,
    cy: Spring,
    lz: Spring,
    target: Camera,
    /// A live zoom-at-cursor constraint (WP2). While set, the center springs
    /// are slaved to the zoom spring through
    /// [`ZoomAnchor::center_at`] — cleared by any plain retarget/snap.
    anchor: Option<ZoomAnchor>,
}

impl CameraGlide {
    /// A settled glide at `cam` (current == target, zero velocity).
    pub fn new(cam: Camera) -> CameraGlide {
        CameraGlide {
            cx: Spring {
                x: cam.center.0,
                v: 0.0,
            },
            cy: Spring {
                x: cam.center.1,
                v: 0.0,
            },
            lz: Spring {
                x: cam.zoom.ln(),
                v: 0.0,
            },
            target: cam,
            anchor: None,
        }
    }

    /// The camera the next frame should render with. A settled glide
    /// returns the target **exactly** (bit-for-bit — no `ln`/`exp`
    /// round-trip), so the damage key goes quiet the moment motion stops.
    pub fn current(&self) -> Camera {
        if self.settled() {
            return self.target;
        }
        Camera {
            center: (self.cx.x, self.cy.x),
            zoom: self.lz.x.exp(),
        }
    }

    /// Where the glide is heading.
    pub fn target(&self) -> Camera {
        self.target
    }

    /// Retarget mid-flight (pan, fit request): position and velocity carry
    /// over, so consecutive steps feel continuous. Clears any zoom anchor —
    /// the new target names its own center.
    pub fn retarget(&mut self, target: Camera) {
        self.target = target;
        self.anchor = None;
    }

    /// Retarget a **zoom-at-cursor** glide (WP2 wheel gesture): glide the
    /// zoom to `zoom` while keeping `anchor`'s board point fixed under its
    /// cursor px through every intermediate frame. The target center is
    /// derived from the anchor at the target zoom; successive ticks
    /// re-anchor at the live cursor and chain continuously (the zoom
    /// spring's position/velocity carry over).
    pub fn retarget_zoom_about(&mut self, zoom: f64, anchor: ZoomAnchor) {
        self.target = Camera {
            center: anchor.center_at(zoom),
            zoom,
        };
        self.anchor = Some(anchor);
        // Pin the center springs to the constraint at the CURRENT zoom so the
        // anchored point is exact from the first frame (a prior pan glide's
        // center velocity would otherwise fight the constraint).
        let c = anchor.center_at(self.lz.x.exp());
        self.cx = Spring { x: c.0, v: 0.0 };
        self.cy = Spring { x: c.1, v: 0.0 };
    }

    /// Jump instantly (initial placement, doc reload): no animation.
    pub fn snap(&mut self, cam: Camera) {
        *self = CameraGlide::new(cam);
    }

    /// Advance by `dt` seconds; returns the camera to render this frame.
    /// Snaps (and zeroes velocity) once within the settle epsilons, so
    /// [`settled`](CameraGlide::settled) goes true and continuous redraw
    /// requests can stop (the §7 damage rule's "glide live" condition).
    pub fn step(&mut self, dt: f64) -> Camera {
        self.lz = self.lz.step(self.target.zoom.ln(), dt, GLIDE_OMEGA);
        if let Some(anchor) = self.anchor {
            // Anchored zoom: the center is a pure function of the zoom, so
            // the anchored board point never leaves the cursor px.
            let c = anchor.center_at(self.lz.x.exp());
            self.cx = Spring { x: c.0, v: 0.0 };
            self.cy = Spring { x: c.1, v: 0.0 };
        } else {
            self.cx = self.cx.step(self.target.center.0, dt, GLIDE_OMEGA);
            self.cy = self.cy.step(self.target.center.1, dt, GLIDE_OMEGA);
        }
        if self.near_target() {
            self.snap(self.target);
        }
        self.current()
    }

    /// Has the glide reached its target (no more redraws needed)?
    pub fn settled(&self) -> bool {
        self.cx.v == 0.0 && self.cy.v == 0.0 && self.lz.v == 0.0 && self.near_target()
    }

    fn near_target(&self) -> bool {
        // Center epsilons scale with zoom so the test is "sub-pixel on
        // screen", not an absolute nm figure (deep zoom needs tighter nm).
        let z = self.target.zoom;
        let px_err = ((self.cx.x - self.target.center.0).abs()
            + (self.cy.x - self.target.center.1).abs())
            * z;
        let px_vel = (self.cx.v.abs() + self.cy.v.abs()) * z;
        px_err < SETTLE_PX
            && px_vel < SETTLE_PX_PER_S
            && (self.lz.x - self.target.zoom.ln()).abs() < SETTLE_LOG_ZOOM
            && self.lz.v.abs() < 0.01
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: (f64, f64) = (800.0, 600.0);

    #[test]
    fn project_unproject_round_trip() {
        let cam = Camera::new((10_000_000.0, 7_500_000.0), 25.0 / 1e6); // 25 px/mm
        for p in [
            (0.0, 0.0),
            (10_000_000.0, 7_500_000.0),
            (-3_000_000.0, 22_000_000.0),
        ] {
            let px = cam.project(p, VP);
            let back = cam.unproject(px, VP);
            assert!((back.0 - p.0).abs() < 1e-3, "{back:?} vs {p:?}");
            assert!((back.1 - p.1).abs() < 1e-3);
        }
    }

    #[test]
    fn round_trip_far_from_origin_and_deep_zoom() {
        // Board center ~0.9 m from the origin (near MAX_COORD), zoom deep
        // enough that one pixel is 0.05 nm — the f32 path would be tens of
        // µm off; the f64 camera must stay sub-0.01 nm.
        let cam = Camera::new((900_000_000.0, -900_000_000.0), 20.0);
        let p = (900_000_123.25, -899_999_876.5);
        let px = cam.project(p, VP);
        let back = cam.unproject(px, VP);
        assert!((back.0 - p.0).abs() < 1e-2, "{:?}", back.0 - p.0);
        assert!((back.1 - p.1).abs() < 1e-2);
        // And a shallow-zoom far-origin case (whole-board view of a distant
        // board): still exact to well under a hundredth of a pixel.
        let cam = Camera::new((900_000_000.0, 900_000_000.0), 25.0 / 1e6);
        let p = (890_000_000.0, 905_000_000.0);
        let back = cam.unproject(cam.project(p, VP), VP);
        assert!((back.0 - p.0).abs() * cam.zoom < 1e-2);
        assert!((back.1 - p.1).abs() * cam.zoom < 1e-2);
    }

    #[test]
    fn y_axis_flips() {
        let cam = Camera::new((0.0, 0.0), 1e-6);
        let up = cam.project((0.0, 1_000_000.0), VP); // 1 mm up in board space
        assert!(up.1 < 300.0, "board +y must be screen -y: {up:?}");
    }

    #[test]
    fn fit_frames_bounds_with_margin() {
        let bounds = (0, 0, 20_000_000, 10_000_000); // 20 × 10 mm
        let cam = Camera::fit(bounds, VP, 20.0);
        // Width-limited: (800 − 40) px over 20 mm ⇒ 38 px/mm.
        assert!((cam.zoom - 38.0e-6).abs() < 1e-9, "{}", cam.zoom);
        assert_eq!(cam.center, (10_000_000.0, 5_000_000.0));
        // The bounds corners land inside the viewport with the margin.
        let (x0, y0) = cam.project((0.0, 0.0), VP);
        assert!(x0 >= 19.9 && y0 <= VP.1 - 19.9, "({x0}, {y0})");
    }

    #[test]
    fn view_transform_matches_project() {
        let cam = Camera::new((5_000_000.0, 5_000_000.0), 3e-5);
        let anchor = eutectic_core::coord::Point {
            x: 4_000_000,
            y: 6_000_000,
        };
        let (origin, scale) = cam.view_transform(anchor, (VP.0 as f32, VP.1 as f32));
        // A point 1 mm right / 2 mm down of the anchor.
        let q = (1_000_000.0f32, -2_000_000.0f32);
        let px = (origin[0] + q.0 * scale, origin[1] - q.1 * scale);
        let want = cam.project(
            (anchor.x as f64 + q.0 as f64, anchor.y as f64 + q.1 as f64),
            VP,
        );
        assert!((px.0 as f64 - want.0).abs() < 1e-2, "{px:?} vs {want:?}");
        assert!((px.1 as f64 - want.1).abs() < 1e-2);
    }

    #[test]
    fn glide_converges_within_spec_window() {
        let from = Camera::new((0.0, 0.0), 1e-6);
        let to = Camera::new((5_000_000.0, -2_000_000.0), 4e-6);
        let mut g = CameraGlide::new(from);
        g.retarget(to);
        let dt = 1.0 / 240.0; // fine steps to observe the trajectory
        let mut t = 0.0;
        while !g.settled() && t < 1.0 {
            g.step(dt);
            t += dt;
        }
        assert!(g.settled(), "glide never settled");
        assert!(
            (0.05..=0.30).contains(&t),
            "settle time {t:.3}s outside the ~100–150 ms class"
        );
        let c = g.current();
        assert_eq!(c, to);
    }

    #[test]
    fn glide_is_interruptible_and_stays_continuous() {
        let mut g = CameraGlide::new(Camera::new((0.0, 0.0), 1e-6));
        g.retarget(Camera::new((10_000_000.0, 0.0), 1e-6));
        let dt = 1.0 / 120.0;
        for _ in 0..6 {
            g.step(dt);
        }
        let mid = g.current();
        let vel_before = g.cx.v;
        assert!(mid.center.0 > 0.0 && mid.center.0 < 10_000_000.0);
        assert!(vel_before > 0.0, "must be moving when interrupted");
        // Wheel retargets mid-flight: position and velocity carry over.
        g.retarget(Camera::new((20_000_000.0, 0.0), 2e-6));
        assert_eq!(g.current(), mid, "retarget must not jump the camera");
        assert_eq!(g.cx.v, vel_before, "retarget must keep velocity");
        let after = g.step(dt);
        assert!(
            after.center.0 > mid.center.0,
            "continues moving toward the new target"
        );
        let mut t = 2.0 * dt + 6.0 * dt;
        while !g.settled() && t < 2.0 {
            g.step(dt);
            t += dt;
        }
        assert_eq!(g.current(), Camera::new((20_000_000.0, 0.0), 2e-6));
    }

    #[test]
    fn glide_big_dt_does_not_overshoot() {
        let mut g = CameraGlide::new(Camera::new((0.0, 0.0), 1e-6));
        g.retarget(Camera::new((1_000_000.0, 0.0), 1e-6));
        // A single quarter-second "frame" (a stall): the closed form must
        // land at/before the target, never past it.
        let c = g.step(0.25);
        assert!(c.center.0 <= 1_000_000.0 + 1e-6);
    }

    #[test]
    fn spring_step_is_deterministic() {
        let s = Spring { x: 3.0, v: -1.0 };
        assert_eq!(
            s.step(10.0, 0.016, GLIDE_OMEGA),
            s.step(10.0, 0.016, GLIDE_OMEGA)
        );
    }

    /// An anchored zoom glide (WP2 wheel gesture) keeps the anchored board
    /// point exactly under its cursor px through EVERY step, and lands
    /// bit-exactly on its derived target.
    #[test]
    fn anchored_zoom_glide_pins_the_cursor_point() {
        let cam = Camera::new((10_000_000.0, 7_500_000.0), 2e-6);
        let mut g = CameraGlide::new(cam);
        let px = (123.0, 517.0); // deliberately off-center
        let board = cam.unproject(px, VP);
        g.retarget_zoom_about(
            cam.zoom * 4.0,
            ZoomAnchor {
                board,
                px,
                viewport: VP,
            },
        );
        let mut t = 0.0;
        while !g.settled() && t < 2.0 {
            let c = g.step(1.0 / 240.0);
            let now = c.unproject(px, VP);
            let err_px = ((now.0 - board.0).abs() + (now.1 - board.1).abs()) * c.zoom;
            assert!(err_px < 1e-6, "anchor drifted {err_px} px at t={t:.3}");
            t += 1.0 / 240.0;
        }
        assert!(g.settled());
        let end = g.current();
        assert_eq!(end, g.target(), "bit-exact settle");
        assert!((end.zoom - cam.zoom * 4.0).abs() < 1e-18);
        let now = end.unproject(px, VP);
        assert!(((now.0 - board.0).abs() + (now.1 - board.1).abs()) * end.zoom < 1e-6);
    }

    /// A plain retarget (pan/fit) clears the anchor: the follow-up glide
    /// converges on the new center instead of being slaved to the old
    /// cursor constraint.
    #[test]
    fn plain_retarget_clears_the_zoom_anchor() {
        let cam = Camera::new((0.0, 0.0), 1e-6);
        let mut g = CameraGlide::new(cam);
        g.retarget_zoom_about(
            2e-6,
            ZoomAnchor {
                board: cam.unproject((100.0, 100.0), VP),
                px: (100.0, 100.0),
                viewport: VP,
            },
        );
        g.step(1.0 / 120.0);
        let want = Camera::new((9_000_000.0, -4_000_000.0), 2e-6);
        g.retarget(want);
        let mut t = 0.0;
        while !g.settled() && t < 2.0 {
            g.step(1.0 / 120.0);
            t += 1.0 / 120.0;
        }
        assert_eq!(g.current(), want, "the pan target wins after retarget");
    }
}
