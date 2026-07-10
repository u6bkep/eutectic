//! Damage keys — the §7 re-render contract, as a pure function.
//!
//! A pane's texture re-renders **iff** one of (doc revision, camera, texture
//! size, state-buffer generation, overlay generation, theme generation)
//! changed; otherwise damascene composites the cached texture. This module
//! is the whole rule: build a [`DamageKey`] per frame from those six inputs
//! and ask [`needs_render`]. WP1 builds it; WP2 wires it into the pane loop.
//! Idle cost: zero GPU work, by construction.

use super::camera::Camera;

/// The six damage inputs, hashed by value. Camera components compare by
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
}

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
        }
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
        ]
        .iter()
        .enumerate()
        {
            assert!(needs_render(Some(&base), next), "input {i} must damage");
        }
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
