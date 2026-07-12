//! The MSDF annotation-text tier (renderer-spec §6, WP3).
//!
//! Scene [`PrimShape::TextRun`]s render through a **multi-channel signed
//! distance field glyph atlas** — damascene-core's public MSDF machinery
//! ([`MsdfAtlas`] + [`build_glyph_msdf`]'s metrics via [`glyph_advance`]) over
//! damascene's own Inter face, so annotation text matches the chrome's text
//! quality and stays crisp at every zoom (that is MSDF's point: one raster
//! per glyph at a fixed base em, sampled at any scale — no per-zoom
//! rebuilds).
//!
//! # Split
//!
//! - **CPU layout** (pure, unit-testable, no GPU): [`run_layout`] shapes a
//!   run into per-glyph pen positions (advance-based, no kerning — see the
//!   note below), honoring the **baseline** anchor (`TextRun.pos`, y-up nm)
//!   and Start/End justify; [`glyph_instances`] turns placed glyphs into
//!   anchor-relative quad instances against a (CPU-side) [`MsdfAtlas`].
//! - **GPU** ([`TextGpu`]): the atlas + its page-texture mirrors + bind
//!   groups. Glyph quads render **into the shared coverage target** (R =
//!   coverage, G = state-flagged coverage, max-blended) inside the owning
//!   plane's coverage pass — so the composite pass gives text its plane's
//!   color, alpha, dim (stale-dim included), visibility, and emphasis mix
//!   for free, exactly like every other primitive.
//!
//! # Lifecycle & idle cost
//!
//! Glyphs rasterize once (per `(glyph, weight)`) into the process-lifetime
//! atlas at scene-buffer build time — i.e. **per doc revision**, never per
//! frame or camera change. Page mirrors upload on the next render after a
//! build dirtied them; an idle frame does zero text work (the §7 damage rule
//! is untouched). The page budget can in principle recycle pages, but the
//! annotation glyph population (refdes, pin names, net names) is a handful
//! of ASCII — one page holds thousands of glyphs; instance UVs are rebuilt
//! from live slots at every scene build, so even a recycle only costs a
//! re-rasterize at the next build.
//!
//! # Coverage & fallbacks
//!
//! Arbitrary doc strings resolve per `char` through Inter's cmap; a missing
//! glyph falls back through a small lookalike table (`✕` → `×` → `x`) and
//! then to `.notdef` (a visible tofu box, never silently dropped — the same
//! philosophy as the core stroke font). Shaping is advance-only (no kerning
//! or ligatures): annotation runs are short utilitarian labels, and the
//! consumer contract (Decision 23) deliberately leaves per-consumer
//! realization freedom — the SVG oracle's `<text>` kerns differently too.

use super::instance::TextInstRaw;
use super::scene::{Justify, Prim, PrimShape};
use damascene_core::text::msdf::glyph_advance;
use damascene_core::text::msdf_atlas::{MsdfAtlas, MsdfGlyphKey};
use eutectic_core::coord::Point;
use std::sync::OnceLock;
use ttf_parser::Face;

/// The annotation face: damascene's bundled Inter (variable, default weight).
pub fn face() -> &'static Face<'static> {
    static FACE: OnceLock<Face<'static>> = OnceLock::new();
    FACE.get_or_init(|| {
        Face::parse(damascene_fonts::INTER_VARIABLE, 0).expect("bundled Inter parses")
    })
}

/// The atlas key's font identity for the one annotation face. `fontdb` IDs
/// are runtime-minted; one throwaway database mints ours.
fn font_id() -> cosmic_text::fontdb::ID {
    static ID: OnceLock<cosmic_text::fontdb::ID> = OnceLock::new();
    *ID.get_or_init(|| {
        let mut db = cosmic_text::fontdb::Database::new();
        db.load_font_data(damascene_fonts::INTER_VARIABLE.to_vec());
        db.faces().next().expect("Inter loads into fontdb").id
    })
}

/// Resolve a char to a glyph id, falling back through lookalikes and then
/// `.notdef` (glyph 0 — a visible box, never a silent drop).
fn glyph_for(face: &Face<'_>, ch: char) -> u16 {
    if let Some(g) = face.glyph_index(ch) {
        return g.0;
    }
    let alts: &[char] = match ch {
        '✕' => &['×', 'x'],
        _ => &[],
    };
    for &alt in alts {
        if let Some(g) = face.glyph_index(alt) {
            return g.0;
        }
    }
    0
}

/// One laid-out glyph of a run: its glyph id and the **pen** (baseline
/// origin) in absolute nm, y-up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedGlyph {
    pub glyph_id: u16,
    pub pen: (f64, f64),
}

/// Lay out one text run: chars → glyphs → advance-accumulated pens on the
/// baseline at `pos` (y-up nm), justified about the anchor (`Left` reads
/// rightward from it, `Right` ends at it, `Center` straddles it). `base_em`
/// is the atlas's em size in atlas px; advances scale by `height / base_em`.
/// Pure — the pick/goldens tier tests this with no GPU.
pub fn run_layout(
    pos: Point,
    height: i64,
    justify: Justify,
    content: &str,
    base_em: u32,
) -> Vec<PlacedGlyph> {
    let f = face();
    let s = height as f64 / base_em as f64; // nm per base-em px
    let glyphs: Vec<u16> = content.chars().map(|c| glyph_for(f, c)).collect();
    let advances: Vec<f64> = glyphs
        .iter()
        .map(|&g| glyph_advance(f, g, base_em) as f64 * s)
        .collect();
    let total: f64 = advances.iter().sum();
    let mut x = pos.x as f64
        - match justify {
            Justify::Left => 0.0,
            Justify::Center => total / 2.0,
            Justify::Right => total,
        };
    let y = pos.y as f64;
    glyphs
        .into_iter()
        .zip(advances)
        .map(|(glyph_id, adv)| {
            let g = PlacedGlyph {
                glyph_id,
                pen: (x, y),
            };
            x += adv;
            g
        })
        .collect()
}

/// Lower every [`PrimShape::TextRun`] in `prims` to glyph quad instances
/// against `atlas` (rasterizing misses), grouped by atlas page:
/// `(page, instance)` pairs in deterministic order. Positions are
/// **anchor-relative f32 nm** (§7 precision rule; the y-up→screen flip
/// happens in the shader's `nm_to_px`, like every other primitive).
pub fn glyph_instances(
    atlas: &mut MsdfAtlas,
    prims: &[Prim],
    anchor: Point,
) -> Vec<(u32, TextInstRaw)> {
    let f = face();
    let font = font_id();
    let base_em = atlas.base_em();
    let mut out: Vec<(u32, TextInstRaw)> = Vec::new();
    for prim in prims {
        let PrimShape::TextRun {
            pos,
            height,
            justify,
            content,
        } = &prim.shape
        else {
            continue;
        };
        let s = *height as f64 / base_em as f64;
        for g in run_layout(*pos, *height, *justify, content, base_em) {
            let key = MsdfGlyphKey {
                font,
                glyph_id: g.glyph_id,
                weight: 0,
            };
            let Some(slot) = atlas.ensure(key, f) else {
                continue; // whitespace / outline-free glyph: advance only
            };
            let page = atlas.page(slot.page).expect("slot page exists");
            // Quad in absolute nm, y-up. The slot's bearings are y-down
            // base-em px: bearing_y is baseline→bitmap-top (negative above
            // the baseline), so the y-up top edge is `pen.y − bearing_y·s`.
            let x0 = g.pen.0 + slot.bearing_x as f64 * s;
            let y_top = g.pen.1 - slot.bearing_y as f64 * s;
            let w = slot.rect.w as f64 * s;
            let h = slot.rect.h as f64 * s;
            let (pw, ph) = (page.width as f64, page.height as f64);
            out.push((
                slot.page,
                TextInstRaw {
                    rect: [
                        (x0 - anchor.x as f64) as f32,
                        (y_top - anchor.y as f64) as f32,
                        w as f32,
                        h as f32,
                    ],
                    uv: [
                        slot.rect.x as f32 / pw as f32,
                        slot.rect.y as f32 / ph as f32,
                        slot.rect.w as f32 / pw as f32,
                        slot.rect.h as f32 / ph as f32,
                    ],
                    sem: prim.sem,
                    spread: slot.spread,
                },
            ));
        }
    }
    // Group by page for per-page draw ranges; stable, so within a page the
    // stream order is preserved (deterministic scenes).
    out.sort_by_key(|(page, _)| *page);
    out
}

// ---------------------------------------------------------------------------
// GPU side: the atlas's page-texture mirror + per-plane instance buffers.
// ---------------------------------------------------------------------------

/// One plane's text draw data: the glyph instance buffer plus per-page draw
/// ranges (the pipeline rebinds the page texture between ranges).
pub struct TextBuf {
    pub(crate) buf: wgpu::Buffer,
    pub(crate) ranges: Vec<(u32, std::ops::Range<u32>)>,
}

struct PageTex {
    /// Kept alive for the bind group; re-created when the page resizes.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    bg: wgpu::BindGroup,
    size: (u32, u32),
}

/// The renderer's text state: the process-lifetime MSDF atlas plus GPU
/// mirrors of its pages. Owned by [`Renderer`](super::gpu::Renderer); scene
/// builds rasterize glyphs into it, [`sync`](TextGpu::sync) uploads dirty
/// pages before the next draw.
pub struct TextGpu {
    atlas: MsdfAtlas,
    pages: Vec<PageTex>,
}

impl TextGpu {
    pub(crate) fn new() -> TextGpu {
        TextGpu {
            atlas: MsdfAtlas::default(),
            pages: Vec::new(),
        }
    }

    /// The CPU atlas (tests / diagnostics).
    pub fn atlas_mut(&mut self) -> &mut MsdfAtlas {
        &mut self.atlas
    }

    /// Build one plane's text instance buffer (rasterizing any missing
    /// glyphs into the atlas). `None` when the plane has no text ink.
    pub(crate) fn build_plane(
        &mut self,
        device: &wgpu::Device,
        prims: &[Prim],
        anchor: Point,
    ) -> Option<TextBuf> {
        let placed = glyph_instances(&mut self.atlas, prims, anchor);
        if placed.is_empty() {
            return None;
        }
        let instances: Vec<TextInstRaw> = placed.iter().map(|(_, i)| *i).collect();
        let mut ranges: Vec<(u32, std::ops::Range<u32>)> = Vec::new();
        for (i, (page, _)) in placed.iter().enumerate() {
            match ranges.last_mut() {
                Some((p, r)) if *p == *page => r.end = i as u32 + 1,
                _ => ranges.push((*page, i as u32..i as u32 + 1)),
            }
        }
        use wgpu::util::DeviceExt;
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("render.text.instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
        });
        Some(TextBuf { buf, ranges })
    }

    /// Mirror the atlas pages onto GPU textures: (re)create any missing or
    /// resized page texture and upload pages dirtied since the last sync.
    /// Dirty pages re-upload whole (a page is 4 MB and dirties only at scene
    /// builds — per doc revision, never per frame).
    pub(crate) fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bgl: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
    ) {
        let dirty: Vec<usize> = self
            .atlas
            .take_dirty()
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        for (i, page) in self.atlas.pages().iter().enumerate() {
            let size = (page.width, page.height);
            let recreate = self.pages.get(i).is_none_or(|p| p.size != size);
            if recreate {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("render.text.atlas"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    // Linear (non-sRGB): the texels are encoded distances,
                    // not color.
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("render.text.page.bg"),
                    layout: bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(sampler),
                        },
                    ],
                });
                let tex = PageTex { texture, bg, size };
                if i < self.pages.len() {
                    self.pages[i] = tex;
                } else {
                    self.pages.push(tex);
                }
            }
            if recreate || dirty.contains(&i) {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.pages[i].texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &page.pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(size.0 * 4),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
    }

    /// A page's bind group for the text pipeline's `@group(1)`.
    pub(crate) fn page_bg(&self, page: u32) -> Option<&wgpu::BindGroup> {
        self.pages.get(page as usize).map(|p| &p.bg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eutectic_core::coord::MM;

    fn pt(x: i64, y: i64) -> Point {
        Point { x, y }
    }

    /// Justify math: a Left run reads rightward from the anchor, a Right run
    /// ends at it, and both share the baseline y (the anchor IS the
    /// baseline — no vertical convention applied here).
    #[test]
    fn layout_justifies_about_the_anchor() {
        let base_em = 48;
        let pos = pt(10 * MM, 5 * MM);
        let left = run_layout(pos, MM, Justify::Left, "VDD", base_em);
        let right = run_layout(pos, MM, Justify::Right, "VDD", base_em);
        assert_eq!(left.len(), 3);
        assert_eq!(left[0].pen.0, pos.x as f64, "Left starts at the anchor");
        assert!(left[2].pen.0 > left[0].pen.0, "advances accumulate");
        // Right: the run's total advance ends at the anchor.
        let f = face();
        let s = MM as f64 / base_em as f64;
        let total: f64 = "VDD"
            .chars()
            .map(|c| glyph_advance(f, f.glyph_index(c).unwrap().0, base_em) as f64 * s)
            .sum();
        assert!((right[0].pen.0 - (pos.x as f64 - total)).abs() < 1e-6);
        for g in left.iter().chain(&right) {
            assert_eq!(g.pen.1, pos.y as f64, "baseline anchor, y-up");
        }
        // Center straddles: first pen is half the total left of the anchor.
        let center = run_layout(pos, MM, Justify::Center, "VDD", base_em);
        assert!((center[0].pen.0 - (pos.x as f64 - total / 2.0)).abs() < 1e-6);
    }

    /// Glyph quads place off the baseline with the MSDF bearings: an 'A'
    /// quad's top sits above the baseline, its bottom at/below it, its ink
    /// height scales with the run height, and the instance is
    /// anchor-relative (§7).
    #[test]
    fn glyph_quads_place_on_the_baseline_anchor_relative() {
        let mut atlas = MsdfAtlas::default();
        let anchor = pt(10 * MM, 5 * MM);
        let prim = Prim::fill(
            3,
            PrimShape::TextRun {
                pos: pt(12 * MM, 6 * MM),
                height: 2 * MM,
                justify: Justify::Left,
                content: "A".into(),
            },
        );
        let inst = glyph_instances(&mut atlas, std::slice::from_ref(&prim), anchor);
        assert_eq!(inst.len(), 1);
        let (page, i) = &inst[0];
        assert_eq!(*page, 0);
        assert_eq!(i.sem, 3);
        // Anchor-relative: the quad sits ~2 mm right / ~1 mm up of the anchor.
        let baseline_rel_y = MM; // (6 − 5) mm above the anchor
        assert!(
            i.rect[1] as f64 > baseline_rel_y as f64,
            "the cap's top edge is above the baseline (y-up): {}",
            i.rect[1]
        );
        let bottom = i.rect[1] - i.rect[3];
        assert!(
            (bottom as f64) < baseline_rel_y as f64 + 0.2 * MM as f64,
            "the quad's bottom reaches (about) the baseline: {bottom}"
        );
        // Ink height ~ cap height of a 2 mm em: between half and the full em
        // (plus spread margins).
        assert!(
            i.rect[3] as f64 > 0.5 * 2.0 * MM as f64 && (i.rect[3] as f64) < 1.2 * 2.0 * MM as f64,
            "ink height scales with the run height: {}",
            i.rect[3]
        );
        // The uv rect is inside the page.
        assert!(i.uv[0] >= 0.0 && i.uv[0] + i.uv[2] <= 1.0);
        assert!(i.uv[1] >= 0.0 && i.uv[1] + i.uv[3] <= 1.0);
        assert!(i.spread > 0.0);

        // Doubling the height doubles the quad (no per-zoom rebuild — the
        // same slot serves every size).
        let mut big = prim;
        if let PrimShape::TextRun { height, .. } = &mut big.shape {
            *height = 4 * MM;
        }
        let inst2 = glyph_instances(&mut atlas, &[big], anchor);
        assert!((inst2[0].1.rect[3] / i.rect[3] - 2.0).abs() < 1e-3);
    }

    /// Whitespace contributes advance but no quad; the nc-mark ✕ resolves to
    /// a real glyph (Inter's own, or the × / x lookalike fallback — never a
    /// silent drop).
    #[test]
    fn whitespace_and_fallbacks() {
        let mut atlas = MsdfAtlas::default();
        let run = |content: &str| {
            Prim::fill(
                1,
                PrimShape::TextRun {
                    pos: pt(0, 0),
                    height: MM,
                    justify: Justify::Left,
                    content: content.into(),
                },
            )
        };
        let spaced = glyph_instances(&mut atlas, &[run("a b")], pt(0, 0));
        assert_eq!(spaced.len(), 2, "the space produces no quad");
        assert!(
            spaced[1].1.rect[0] > spaced[0].1.rect[0],
            "…but its advance separates the letters"
        );
        let nc = glyph_instances(&mut atlas, &[run("✕")], pt(0, 0));
        assert_eq!(nc.len(), 1, "the nc mark renders a visible glyph");
        assert!(glyph_for(face(), '✕') != 0 || glyph_for(face(), '×') != 0);
    }

    /// Instance building is deterministic (equal inputs, equal instances) —
    /// the scene-buffer determinism contract extends to text.
    #[test]
    fn instances_are_deterministic() {
        let prims = vec![Prim::fill(
            2,
            PrimShape::TextRun {
                pos: pt(MM, MM),
                height: MM,
                justify: Justify::Right,
                content: "C3 (Cap)".into(),
            },
        )];
        let mut a1 = MsdfAtlas::default();
        let mut a2 = MsdfAtlas::default();
        assert_eq!(
            glyph_instances(&mut a1, &prims, pt(0, 0)),
            glyph_instances(&mut a2, &prims, pt(0, 0))
        );
        // And idempotent against a warm atlas.
        assert_eq!(
            glyph_instances(&mut a1, &prims, pt(0, 0)),
            glyph_instances(&mut a1, &prims, pt(0, 0))
        );
    }
}
