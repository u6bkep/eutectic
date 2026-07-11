//! Damage keys — the §7 re-render contract, as a pure function.
//!
//! A pane's texture re-renders **iff** one of (doc revision, camera, texture
//! size, state-buffer generation, overlay generation, theme generation)
//! changed; otherwise damascene composites the cached texture. This module
//! is the whole rule: build a [`DamageKey`] per frame from those inputs and
//! ask [`needs_render`]. WP1 built it; WP2 wires it into the pane loop.
//! Idle cost: zero GPU work, by construction.
//!
//! WP2 added a **seventh input**: the crosshair cursor position (§4 draws the
//! crosshair into the pane texture, so a pointer move over the pane must
//! re-render it; quantized to the physical pixel it lands on). A moving
//! pointer is not "idle" — pointer-still frames compare equal, so the zero-
//! idle-GPU contract is unchanged.

use super::camera::Camera;

/// The damage inputs, compared by value. Camera components compare by
/// **bit pattern** — a glide emits a stream of distinct f64s (each frame
/// re-renders while motion is live), and a settled camera is bit-stable so
/// idle frames compare equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DamageKey {
    pub doc_rev: u64,
    camera: [u64; 3],
    pub size: (u32, u32),
    pub state_gen: u64,
    pub overlay_gen: u64,
    pub theme_gen: u64,
    /// The crosshair cursor, quantized to the pane-texture pixel (or a
    /// sentinel when the pointer is off the pane). WP2's seventh input.
    cursor: (i32, i32),
}

/// The [`DamageKey::cursor`] sentinel for "no crosshair" (pointer off-pane).
const NO_CURSOR: (i32, i32) = (i32::MIN, i32::MIN);

impl DamageKey {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        doc_rev: u64,
        camera: &Camera,
        size: (u32, u32),
        state_gen: u64,
        overlay_gen: u64,
        theme_gen: u64,
    ) -> DamageKey {
        DamageKey {
            doc_rev,
            camera: [
                camera.center.0.to_bits(),
                camera.center.1.to_bits(),
                camera.zoom.to_bits(),
            ],
            size,
            state_gen,
            overlay_gen,
            theme_gen,
            cursor: NO_CURSOR,
        }
    }

    /// The key with the crosshair cursor at `px` (pane-texture px, `None` =
    /// pointer off the pane). Quantized to whole pixels so sub-pixel pointer
    /// jitter does not thrash renders.
    pub fn with_cursor(mut self, px: Option<[f32; 2]>) -> DamageKey {
        self.cursor = match px {
            Some([x, y]) => (x.round() as i32, y.round() as i32),
            None => NO_CURSOR,
        };
        self
    }
}

/// Should this frame re-render the pane texture? `last` is the key of the
/// last *rendered* frame (`None` ⇒ never rendered ⇒ render).
pub fn needs_render(last: Option<&DamageKey>, next: &DamageKey) -> bool {
    last != Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(z: f64) -> Camera {
        Camera::new((1_000_000.0, 2_000_000.0), z)
    }

    #[test]
    fn damage_truth_table() {
        let base = DamageKey::new(7, &cam(1e-6), (800, 600), 3, 1, 0);
        // Never rendered ⇒ render.
        assert!(needs_render(None, &base));
        // Nothing changed ⇒ cached texture, zero GPU work.
        let same = DamageKey::new(7, &cam(1e-6), (800, 600), 3, 1, 0);
        assert!(!needs_render(Some(&base), &same));
        // Each input alone forces a render.
        for (i, next) in [
            DamageKey::new(8, &cam(1e-6), (800, 600), 3, 1, 0), // doc revision
            DamageKey::new(7, &cam(2e-6), (800, 600), 3, 1, 0), // camera
            DamageKey::new(7, &cam(1e-6), (801, 600), 3, 1, 0), // size
            DamageKey::new(7, &cam(1e-6), (800, 600), 4, 1, 0), // state gen
            DamageKey::new(7, &cam(1e-6), (800, 600), 3, 2, 0), // overlay gen
            DamageKey::new(7, &cam(1e-6), (800, 600), 3, 1, 1), // theme gen
            DamageKey::new(7, &cam(1e-6), (800, 600), 3, 1, 0).with_cursor(Some([10.0, 10.0])), // cursor
        ]
        .iter()
        .enumerate()
        {
            assert!(needs_render(Some(&base), next), "input {i} must damage");
        }
    }

    /// The cursor input is quantized to whole pixels: sub-pixel jitter within
    /// one pixel compares equal (no render thrash), crossing a pixel damages,
    /// and off-pane (`None`) is a distinct stable state.
    #[test]
    fn cursor_quantizes_to_pixels() {
        let base = DamageKey::new(0, &cam(1e-6), (8, 8), 0, 0, 0).with_cursor(Some([10.2, 10.2]));
        let jitter = DamageKey::new(0, &cam(1e-6), (8, 8), 0, 0, 0).with_cursor(Some([10.4, 9.8]));
        assert!(
            !needs_render(Some(&base), &jitter),
            "sub-pixel jitter is free"
        );
        let moved = DamageKey::new(0, &cam(1e-6), (8, 8), 0, 0, 0).with_cursor(Some([11.6, 10.2]));
        assert!(needs_render(Some(&base), &moved));
        let off = DamageKey::new(0, &cam(1e-6), (8, 8), 0, 0, 0).with_cursor(None);
        assert!(
            needs_render(Some(&base), &off),
            "leaving the pane damages once"
        );
        let off2 = DamageKey::new(0, &cam(1e-6), (8, 8), 0, 0, 0).with_cursor(None);
        assert!(!needs_render(Some(&off), &off2), "and stays quiet after");
    }

    #[test]
    fn camera_compares_by_bits_not_epsilon() {
        let a = DamageKey::new(0, &cam(1e-6), (1, 1), 0, 0, 0);
        let b = DamageKey::new(0, &cam(1e-6 + f64::EPSILON * 1e-6), (1, 1), 0, 0, 0);
        assert!(needs_render(Some(&a), &b), "any camera motion re-renders");
        // A camera moved and moved back is bit-identical ⇒ no render.
        let c = DamageKey::new(0, &cam(1e-6), (1, 1), 0, 0, 0);
        assert!(!needs_render(Some(&a), &c));
    }
}
