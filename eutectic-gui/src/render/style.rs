//! Style tables (renderer-spec §8): app-owned per-plane appearance, feeding
//! the composite-pass uniforms. Two color sources, deliberately:
//!
//! - **Chrome-adjacent** entries (canvas background, grid, crosshair, the
//!   emphasis accent) hold **damascene theme tokens** (`Color::srgb_token`)
//!   and resolve through the runner's active
//!   [`Theme`](damascene_core::theme::Theme) palette at **uniform-write
//!   time** ([`StyleTables::resolve`]) — the canvas follows the active
//!   theme, and a theme swap is a uniform rewrite, mirroring damascene's own
//!   token-at-paint-time design.
//! - **Domain colors** (per-copper-slab palette, mask, silk, drills) are
//!   app-owned light/dark defaults — physical-PCB semantics no toolkit theme
//!   can own. They still carry stable `eutectic.*` token names, so a future
//!   user palette can re-skin them through the same resolve path for free.
//!
//! Layer visibility toggles, dimming, and theme swaps are all mutations
//! here + a re-render — **never geometry work** (§4).

use super::scene::{PlaneKey, Scene};
use damascene_core::prelude::Color;
use damascene_core::theme::{Theme, tokens};
use std::collections::BTreeMap;

/// One plane's appearance knobs (the composite-pass uniforms, pre-resolve).
#[derive(Clone, Debug, PartialEq)]
pub struct PlaneStyle {
    pub color: Color,
    /// Base opacity (multiplies coverage).
    pub alpha: f32,
    /// Extra dim factor (inactive-layer treatment; 1.0 = none).
    pub dim: f32,
    pub visible: bool,
    /// Composite with the **background color** instead of `color` — the
    /// drills plane's absence-through-everything paint (§4).
    pub background_paint: bool,
}

/// One dash pattern (nm on / nm off), indexed by
/// [`StyleClass::Dash`](super::scene::StyleClass).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DashPattern {
    pub on_nm: f64,
    pub off_nm: f64,
}

/// The app-owned style tables. Defaults come from
/// [`board_defaults`](StyleTables::board_defaults); per-plane overrides
/// (visibility toggles, user recolors) layer on top. Every mutation bumps
/// [`generation`](StyleTables::generation) — the damage key's theme/style
/// input.
#[derive(Clone, Debug, PartialEq)]
pub struct StyleTables {
    overrides: BTreeMap<PlaneKey, PlaneStyle>,
    /// Canvas background — a real damascene token (`background`), so the
    /// canvas tracks the chrome's light/dark swap.
    pub background: Color,
    pub grid_dot: Color,
    pub grid_dot_major: Color,
    pub grid_axis: Color,
    pub crosshair: Color,
    /// The hover/selection emphasis accent (the G-channel mix target, §4).
    pub emphasis: Color,
    /// Dash patterns; index 0 is the board-edge dash
    /// ([`board::DASH_EDGE`](super::board::DASH_EDGE), 0.8 mm on / 0.5 mm
    /// off — the old canvas's edge treatment); index 1 is the schematic
    /// bin-divider dash ([`schematic::DASH_BIN`](super::schematic::DASH_BIN),
    /// 1 mm on / 1 mm off — the SVG oracle's `stroke-dasharray="1,1"`).
    pub dash: Vec<DashPattern>,
    dark: bool,
    generation: u64,
}

impl StyleTables {
    /// The board defaults for a dark or light canvas. Domain colors follow
    /// the old canvas's dark-ECAD palette (warm top copper, cool bottom,
    /// green inner, off-white silk, green mask film, amber edge/fab) with
    /// light-mode variants; chrome-adjacent entries hold theme tokens.
    pub fn board_defaults(dark: bool) -> StyleTables {
        let t = |name: &'static str, d: [u8; 4], l: [u8; 4]| {
            let c = if dark { d } else { l };
            Color::srgb_token(name, c[0], c[1], c[2], c[3])
        };
        StyleTables {
            overrides: BTreeMap::new(),
            background: tokens::BACKGROUND,
            grid_dot: t(
                "eutectic.grid.dot",
                [0x28, 0x28, 0x2e, 0xff],
                [0xd6, 0xd6, 0xdb, 0xff],
            ),
            grid_dot_major: t(
                "eutectic.grid.dot.major",
                [0x3c, 0x3c, 0x46, 0xff],
                [0xb8, 0xb8, 0xc2, 0xff],
            ),
            grid_axis: t(
                "eutectic.grid.axis",
                [0x3b, 0x82, 0xf6, 0x55],
                [0x25, 0x63, 0xeb, 0x55],
            ),
            crosshair: t(
                "eutectic.crosshair",
                [0xe4, 0xe4, 0xe7, 0xaa],
                [0x3f, 0x3f, 0x46, 0xaa],
            ),
            emphasis: t(
                "eutectic.emphasis",
                [0x22, 0xd3, 0xee, 0xff],
                [0x06, 0x91, 0xa5, 0xff],
            ),
            dash: vec![
                // 0: board-edge (0.8 mm on / 0.5 mm off).
                DashPattern {
                    on_nm: 800_000.0,
                    off_nm: 500_000.0,
                },
                // 1: schematic bin divider (1 mm / 1 mm — the SVG oracle's
                // `stroke-dasharray="1,1"`).
                DashPattern {
                    on_nm: 1_000_000.0,
                    off_nm: 1_000_000.0,
                },
            ],
            dark,
            generation: 0,
        }
    }

    /// Style/theme generation — a damage-key input; every mutation bumps it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The chrome theme swapped (the runner's `Theme` changed): resolution
    /// happens at uniform-write time, so this only needs to damage the pane.
    pub fn theme_changed(&mut self) {
        self.generation += 1;
    }

    /// Override one plane's style (recolor, alpha).
    pub fn set_plane(&mut self, key: PlaneKey, style: PlaneStyle) {
        self.overrides.insert(key, style);
        self.generation += 1;
    }

    /// Toggle one plane's visibility (a composite-uniform change, §4).
    pub fn set_visible(&mut self, key: &PlaneKey, scene: &Scene, on: bool) {
        let mut s = self.plane_style(key, &copper_order(scene));
        s.visible = on;
        self.set_plane(key.clone(), s);
    }

    /// Set one plane's dim factor (inactive-layer treatment).
    pub fn set_dim(&mut self, key: &PlaneKey, scene: &Scene, dim: f32) {
        let mut s = self.plane_style(key, &copper_order(scene));
        s.dim = dim;
        self.set_plane(key.clone(), s);
    }

    /// The effective (default ⊕ override) appearance of one plane — the
    /// layer panel's swatch source (WP3: layer rows derive from the scene's
    /// plane list, not from a dead tessellation pass).
    pub fn plane_appearance(&self, key: &PlaneKey, scene: &Scene) -> PlaneStyle {
        self.plane_style(key, &copper_order(scene))
    }

    /// The effective (default ⊕ override) style of one plane. `copper` is
    /// the scene's copper slab names in paint (ascending-z) order — copper
    /// colors key off stack position (first = bottom/cool, last = top/warm,
    /// middle = inner/green), like the old canvas and `svg.rs`.
    fn plane_style(&self, key: &PlaneKey, copper: &[String]) -> PlaneStyle {
        if let Some(s) = self.overrides.get(key) {
            return s.clone();
        }
        let d = self.dark;
        let t = |name: &'static str, dk: [u8; 4], l: [u8; 4]| {
            let c = if d { dk } else { l };
            Color::srgb_token(name, c[0], c[1], c[2], c[3])
        };
        let copper_color = |name: &String| {
            let n = copper.len();
            match copper.iter().position(|c| c == name) {
                Some(i) if i + 1 == n => t(
                    "eutectic.layer.cu.top",
                    [0xd6, 0x3a, 0x3a, 0xff],
                    [0xb9, 0x1c, 0x1c, 0xff],
                ),
                Some(0) => t(
                    "eutectic.layer.cu.bottom",
                    [0x3a, 0x7a, 0xd6, 0xff],
                    [0x1d, 0x4e, 0xd8, 0xff],
                ),
                Some(_) => t(
                    "eutectic.layer.cu.inner",
                    [0x3a, 0xb0, 0x55, 0xff],
                    [0x15, 0x80, 0x3d, 0xff],
                ),
                None => t(
                    "eutectic.layer.cu.top",
                    [0xd6, 0x3a, 0x3a, 0xff],
                    [0xb9, 0x1c, 0x1c, 0xff],
                ),
            }
        };
        let plain = |color: Color, alpha: f32| PlaneStyle {
            color,
            alpha,
            dim: 1.0,
            visible: true,
            background_paint: false,
        };
        match key {
            PlaneKey::Substrate => plain(
                t(
                    "eutectic.layer.substrate",
                    [0x2a, 0x2a, 0x2a, 0xff],
                    [0xe8, 0xe4, 0xd8, 0xff],
                ),
                1.0,
            ),
            PlaneKey::Outline => plain(
                t(
                    "eutectic.layer.edge",
                    [0xea, 0xb3, 0x08, 0xff],
                    [0xa1, 0x62, 0x07, 0xff],
                ),
                1.0,
            ),
            // Pour fills are translucent so the outline/grid read through —
            // the old canvas's 0.25 `fill-opacity`, now a plane uniform.
            PlaneKey::CopperPour(name) => plain(copper_color(name), 0.25),
            PlaneKey::Copper(name) => plain(copper_color(name), 1.0),
            // The mask film is translucent (the old canvas's 0.3).
            PlaneKey::Mask(_) => plain(
                t(
                    "eutectic.layer.mask",
                    [0x1f, 0x6f, 0x43, 0xff],
                    [0x16, 0xa3, 0x4a, 0xff],
                ),
                0.3,
            ),
            PlaneKey::Silk(_) => plain(
                t(
                    "eutectic.layer.silk",
                    [0xe0, 0xe0, 0xe0, 0xff],
                    [0x37, 0x41, 0x51, 0xff],
                ),
                1.0,
            ),
            PlaneKey::Fab(_) => plain(
                t(
                    "eutectic.layer.fab",
                    [0xc8, 0x8a, 0x2c, 0xff],
                    [0xb4, 0x53, 0x09, 0xff],
                ),
                1.0,
            ),
            PlaneKey::Drills => PlaneStyle {
                // Ignored at resolve: drills paint the background color.
                color: tokens::BACKGROUND,
                alpha: 1.0,
                dim: 1.0,
                visible: true,
                background_paint: true,
            },
            // Schematic tiers (WP3): the old schematic view's palette (its
            // `class_color` table) with light-mode variants.
            PlaneKey::SchematicWire => plain(
                t(
                    "eutectic.schematic.wire",
                    [0x2e, 0xa0, 0x43, 0xff],
                    [0x15, 0x80, 0x3d, 0xff],
                ),
                1.0,
            ),
            PlaneKey::SchematicInk => plain(
                t(
                    "eutectic.schematic.ink",
                    [0xd8, 0xd8, 0xd8, 0xff],
                    [0x37, 0x41, 0x51, 0xff],
                ),
                1.0,
            ),
            PlaneKey::SchematicTag => plain(
                t(
                    "eutectic.schematic.tag",
                    [0x6f, 0xb7, 0xc9, 0xff],
                    [0x0e, 0x74, 0x90, 0xff],
                ),
                1.0,
            ),
            PlaneKey::SchematicChrome => plain(
                t(
                    "eutectic.schematic.chrome",
                    [0x88, 0x88, 0x88, 0xff],
                    [0x6b, 0x72, 0x80, 0xff],
                ),
                1.0,
            ),
            PlaneKey::Overlay => plain(
                t(
                    "eutectic.overlay.preview",
                    [0x53, 0xdd, 0x6c, 0xee],
                    [0x15, 0x80, 0x3d, 0xee],
                ),
                1.0,
            ),
        }
    }

    /// Resolve every color through the active theme palette (token-at-
    /// uniform-write-time) into the flat tables the renderer uploads.
    /// `theme = None` keeps each token's fallback rgba (headless tests).
    pub fn resolve(&self, scene: &Scene, theme: Option<&Theme>) -> ResolvedStyles {
        let copper = copper_order(scene);
        let rc = |c: &Color| resolve_color(c, theme);
        let background = rc(&self.background);
        let emphasis = rc(&self.emphasis);
        let plane = |key: &PlaneKey| {
            let s = self.plane_style(key, &copper);
            ResolvedPlane {
                color: if s.background_paint {
                    background
                } else {
                    rc(&s.color)
                },
                emphasis,
                alpha: s.alpha,
                dim: s.dim,
                visible: s.visible,
            }
        };
        ResolvedStyles {
            planes: scene.planes.iter().map(|p| plane(&p.key)).collect(),
            overlay: plane(&PlaneKey::Overlay),
            background,
            grid_dot: rc(&self.grid_dot),
            grid_dot_major: rc(&self.grid_dot_major),
            grid_axis: rc(&self.grid_axis),
            crosshair: rc(&self.crosshair),
            dash: self.dash.iter().map(|d| [d.on_nm, d.off_nm]).collect(),
        }
    }
}

/// The scene's copper slab names in plane (ascending-z) order — the copper
/// palette's stack-position key.
fn copper_order(scene: &Scene) -> Vec<String> {
    scene
        .planes
        .iter()
        .filter_map(|p| match &p.key {
            PlaneKey::Copper(name) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Token → active palette rgba (or the token's fallback), as straight sRGB
/// f32. The renderer linearizes iff its target format is sRGB.
fn resolve_color(c: &Color, theme: Option<&Theme>) -> [f32; 4] {
    let c = match theme {
        Some(t) => t.resolve(*c),
        None => *c,
    };
    let [r, g, b, a] = c.to_srgb_u8a();
    [
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ]
}

/// One plane's resolved composite uniforms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedPlane {
    /// Straight sRGB (not premultiplied); alpha is the color's own alpha.
    pub color: [f32; 4],
    pub emphasis: [f32; 4],
    pub alpha: f32,
    pub dim: f32,
    pub visible: bool,
}

/// The flat, theme-resolved tables one frame renders with — parallel to
/// `scene.planes` by index.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedStyles {
    pub planes: Vec<ResolvedPlane>,
    pub overlay: ResolvedPlane,
    pub background: [f32; 4],
    pub grid_dot: [f32; 4],
    pub grid_dot_major: [f32; 4],
    pub grid_axis: [f32; 4],
    pub crosshair: [f32; 4],
    /// `[on_nm, off_nm]` per dash pattern id.
    pub dash: Vec<[f64; 2]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::scene::Plane;

    fn scene_with_planes(keys: Vec<PlaneKey>) -> Scene {
        Scene {
            anchor: eutectic_core::coord::Point { x: 0, y: 0 },
            bounds: (0, 0, 1, 1),
            planes: keys
                .into_iter()
                .map(|key| Plane { key, prims: vec![] })
                .collect(),
            semantics: vec![super::super::scene::SemanticKey::Chrome],
        }
    }

    #[test]
    fn copper_palette_keys_off_stack_position() {
        let scene = scene_with_planes(vec![
            PlaneKey::Copper("B.Cu".into()),
            PlaneKey::Copper("In1.Cu".into()),
            PlaneKey::Copper("F.Cu".into()),
        ]);
        let r = StyleTables::board_defaults(true).resolve(&scene, None);
        let (bottom, inner, top) = (r.planes[0], r.planes[1], r.planes[2]);
        assert_ne!(bottom.color, top.color);
        assert_ne!(inner.color, top.color);
        // Top is the warm red default.
        assert!((top.color[0] - 0xd6 as f32 / 255.0).abs() < 1e-6);
        assert!((bottom.color[2] - 0xd6 as f32 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn pour_plane_is_translucent_same_hue() {
        let scene = scene_with_planes(vec![
            PlaneKey::CopperPour("F.Cu".into()),
            PlaneKey::Copper("F.Cu".into()),
        ]);
        let r = StyleTables::board_defaults(true).resolve(&scene, None);
        assert_eq!(r.planes[0].color, r.planes[1].color);
        assert!(r.planes[0].alpha < r.planes[1].alpha);
    }

    #[test]
    fn drills_resolve_to_background_paint() {
        let scene = scene_with_planes(vec![PlaneKey::Drills]);
        let r = StyleTables::board_defaults(true).resolve(&scene, None);
        assert_eq!(r.planes[0].color, r.background);
    }

    #[test]
    fn mutations_bump_generation() {
        let scene = scene_with_planes(vec![PlaneKey::Copper("F.Cu".into())]);
        let mut t = StyleTables::board_defaults(true);
        assert_eq!(t.generation(), 0);
        t.set_visible(&PlaneKey::Copper("F.Cu".into()), &scene, false);
        assert_eq!(t.generation(), 1);
        t.theme_changed();
        assert_eq!(t.generation(), 2);
        let r = t.resolve(&scene, None);
        assert!(!r.planes[0].visible);
    }

    #[test]
    fn light_and_dark_defaults_differ() {
        let scene = scene_with_planes(vec![PlaneKey::Silk("F.SilkS".into())]);
        let d = StyleTables::board_defaults(true).resolve(&scene, None);
        let l = StyleTables::board_defaults(false).resolve(&scene, None);
        assert_ne!(d.planes[0].color, l.planes[0].color);
        assert_ne!(d.grid_dot, l.grid_dot);
    }
}
