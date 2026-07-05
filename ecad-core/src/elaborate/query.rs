//! Read-only projections over a [`Source`]: the shared board/region/stackup readers and
//! the derived [`NetFeature`] materialization gate (Decision 13). The font/ttf lowering
//! for board text concentrates here.

use crate::doc::*;
use crate::geom::{Feature, NetFeature, Role, Shape2D, Slab, Stackup, ZRange};
use crate::id::NetId;
use crate::ir::{GenDirective, RegionDecl, Source};

/// The board as a filled [`Region`](crate::geom::kernel::Region): the last `Board` directive's
/// outline **minus** every `Cutout` (Decision 16c). `None` if there is no `Board` (the
/// solver then leaves placement unbounded). This is the single shared board-geometry
/// reader — elaboration (the substrate/mask `Area` features), the solver (containment),
/// the autorouter (grid bbox), and export (Edge.Cuts, SVG) all fold through it, so the
/// board's truth lives in one place instead of a bespoke `outline`/`cutouts` struct.
///
/// The outline and cutouts are polygonized here (the region kernel flattens arcs at
/// construction, Decision 16b): a curved board edge or round cutout becomes a fine
/// polyline. The authored arcs survive in the `Board`/`Cutout` directives; this derived
/// region does not carry them.
pub fn board_region(source: &Source) -> Option<crate::geom::kernel::Region> {
    use crate::geom::kernel::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region, union_all};
    let outline = source.iter().rev().find_map(|d| match d {
        GenDirective::Board { outline } => Some(outline),
        _ => None,
    })?;
    let mut region = shape_to_region(outline, DEFAULT_CIRCLE_SEGS);
    let cutouts: Vec<crate::geom::kernel::Region> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Cutout { shape } => Some(shape_to_region(shape, DEFAULT_CIRCLE_SEGS)),
            _ => None,
        })
        .collect();
    if !cutouts.is_empty() {
        region = difference(&region, &union_all(cutouts));
    }
    Some(region)
}

/// Assemble every authored [`RegionDecl`] from the source, in declaration order. The
/// single shared reader for pours / keep-outs / filled voids — the derived fill query
/// (0004 stage 3), DRC, and export all call this, exactly as [`board_region`] is the
/// shared reader for the outline.
pub fn regions(source: &Source) -> Vec<RegionDecl> {
    source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Region(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// The board [`Stackup`] for a source — the single shared reader that every consumer
/// lowering an abstract layer to a real `ZRange` must go through (sibling to
/// [`board_region`] / [`regions`]).
///
/// Collects every [`Slab`](GenDirective::Slab) directive, in **declaration order**, into
/// `Stackup { slabs }` — exactly as [`regions`] collects [`RegionDecl`]s. Declaration
/// order is preserved (not sorted): [`Stackup`]'s own accessors order by z where they
/// need to ([`Stackup::copper_slabs`] sorts by z, [`Stackup::board_z`] takes min/max,
/// [`Stackup::slab_z`] looks up by name), so order is functionally irrelevant — and
/// preserving it keeps `parse(serialize(doc)) == doc` trivially. No overlap/gap
/// validation is performed here (`ZRange::new` already normalises `lo ≤ hi`); a future
/// validation pass can layer on top without changing this reader's contract.
///
/// If the source authors **no** slabs, falls back to [`Stackup::default_2layer`] — the
/// unchanged familiar 2-layer default, so existing sources behave exactly as before.
pub fn stackup(source: &Source) -> Stackup {
    let slabs: Vec<Slab> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Slab(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    if slabs.is_empty() {
        Stackup::default_2layer()
    } else {
        Stackup { slabs }
    }
}

/// Lower the authored board/region geometry of a `Source` into the converged
/// [`NetFeature`] model — a [`Feature`] (pure physical geometry) paired with the
/// optional net it carries. This is the additive producer the convergence's Phase 2
/// will wire DRC/export onto; for now it has no callers besides tests. It is the
/// role-filtered union of what [`board_region`] and [`regions`] read today
/// (Decision 12.4), kept as one derived view, threading z through [`stackup`].
///
/// Emitted per directive (net stays an *annotation* alongside the feature, never a
/// field on `Feature` — connectivity is authoritative, Decision 12.1):
///   - the **last** `Board` directive minus every `Cutout` → one [`Role::Substrate`]
///     netless feature carrying a [`Shape2D::Area`] (the [`board_region`], Decision 16c).
///     Cutouts are holes in that Area, not separate `Void` features (Decision 16b).
///     (Unioning several `Board` directives into one multi-substrate body is deferred.)
///   - every `Region` → a feature carrying the authored role + net, at its slab's z
///     (mirrors [`regions`]).
///
/// This is the single **materialization gate** that resolves slab names against the
/// [`Stackup`] (Decision 13), so it is **fallible**: an unknown slab name — on a region
/// or a text label — is a hard error, and a `Conductor` region whose slab is not a
/// copper slab (a net-bound pour on silk) is likewise rejected here.
pub fn features(source: &Source) -> Result<Vec<crate::geom::NetFeature>, String> {
    let su = stackup(source);
    // The physical board *body* extent (the Substrate solid spans it). An empty stackup
    // has no extent — fall back to a zero range so the feature is still emitted.
    let board_z = su.board_z().unwrap_or(ZRange::new(0, 0));

    let mut out: Vec<NetFeature> = Vec::new();

    // Board: the single `Role::Substrate` feature is the board region — the last
    // `Board`'s outline minus every `Cutout` — carried as a `Shape2D::Area` (Decision
    // 16c). Board-level cutouts are *holes* in this Area (routed contours, Decision 16b),
    // not separate `Void` features. The same region (holes included) is the mask area.
    if let Some(region) = board_region(source) {
        let area = Shape2D::Area { region };
        out.push(NetFeature::netless(Feature::prism(
            Role::Substrate,
            area.clone(),
            board_z,
        )));

        // Solder mask: one board-area solid per `Role::Mask` slab in the stackup, at the
        // slab's honest z, carrying the slab's material (Decision 13 — mask is a positive
        // generated solid, and its openings are `Void` deletion volumes; there are no
        // negative layers). The mask area is the *same* board region **including the
        // cutout holes**, so a cutout reads through the mask (its opening) exactly as
        // before — now via the Area's holes rather than a separate cutout Void.
        for slab in su.slabs.iter().filter(|s| s.role == Role::Mask) {
            let mut mask = Feature::prism(Role::Mask, area.clone(), slab.z);
            mask.material = slab.material.clone();
            out.push(NetFeature::netless(mask));
        }
    }

    // Regions: every one, carrying the authored role + net (mirrors `regions`). The
    // slab name resolves to z; an unknown name is a hard error, and a `Conductor`
    // region on a non-copper slab (a net-bound pour on silk) is nonsense.
    for d in source {
        if let GenDirective::Region(RegionDecl {
            shape,
            role,
            net,
            layer,
        }) = d
        {
            let slab = su.slabs.iter().find(|s| &s.name == layer).ok_or_else(|| {
                let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
                format!("unknown slab `{layer}` (available: {})", names.join(", "))
            })?;
            if *role == Role::Conductor && slab.role != Role::Conductor {
                return Err(format!(
                    "Conductor region on non-copper slab `{layer}` (its role is {:?}) \
                     — a net-bound pour must target a copper slab",
                    slab.role
                ));
            }
            // A `Conductor` region is a **copper pour**: its materialised feature is a
            // *filled* `Shape2D::Area` (outline ∖ foreign-copper knockouts), which needs
            // the placed copper to derive — so it is lowered by the unified world-frame
            // producer ([`crate::route::world_features`]), not here. This source-only
            // query still validates the pour's slab above (the materialization gate,
            // Decision 13); it just does not emit the raw outline as geometry.
            if *role == Role::Conductor {
                continue;
            }
            let net_opt = net.as_ref().map(|n| NetId::new(n.clone()));
            out.push(NetFeature::new(
                net_opt,
                Feature::prism(role.clone(), shape.clone(), slab.z),
            ));
        }
    }

    // Holes: every authored NPTH `hole` lowers to a full-stackup `Role::Void` disc with
    // **no material** (Decision 16b — a mounting hole is an authored non-plated `Void`).
    // Full-z so `excellon_drill`'s through-cut query picks it up; material-less so its
    // plating classification is NPTH. The `Some(full)` guard matches the via-drill
    // sibling above: `full_z()` is `None` only for a slab-less stackup, which `stackup()`
    // never yields (it falls back to `default_2layer`), so the drop is unreachable via the
    // normal reader — a hole with no board to drill through contributes no geometry.
    if let Some(full) = su.full_z() {
        for d in source {
            if let GenDirective::Hole { center, dia } = d {
                out.push(NetFeature::netless(Feature::prism(
                    Role::Void,
                    Shape2D::disc(*center, dia / 2),
                    full,
                )));
            }
        }
    }

    // Text: every authored string lowers to `Marking` features (Decision 9). The
    // geometry is derived here, never stored, so a renamed label re-derives. An
    // outline `font` directive (Decision 17), if present and loadable, swaps the stroke
    // font for filled glyph outlines; otherwise the built-in stroke font is used.
    let font = resolve_font(source);
    for d in source {
        if let GenDirective::Text {
            string,
            at,
            height,
            layer,
            orient,
        } = d
        {
            out.extend(text_features(
                string,
                *at,
                *height,
                layer,
                *orient,
                &su,
                font.as_ref(),
            )?);
        }
    }

    Ok(out)
}

/// The doc-wide outline font (Decision 17): the **last** [`GenDirective::Font`]'s file
/// parsed as a [`TtfFont`](crate::font::TtfFont), or `None` when there is no directive
/// **or the file fails to load**. Load failure degrades silently here (rendering must
/// never fail); [`font_diagnostics`] is the channel that surfaces the failure to the user.
pub fn resolve_font(source: &Source) -> Option<crate::font::TtfFont> {
    let path = source.iter().rev().find_map(|d| match d {
        GenDirective::Font { path } => Some(path),
        _ => None,
    })?;
    crate::font::TtfFont::from_path(std::path::Path::new(path)).ok()
}

/// The doc-wide [`GenDirective::Font`] failure, if any: `(path, reason)` when the last
/// `Font` directive's file cannot be read or parsed. `None` when there is no directive or
/// it loads cleanly. Distinct from [`resolve_font`] because feature lowering has no
/// diagnostic channel and must never fail; this feeds the [`ReconReport`]'s
/// `font_load_failure` field, which the `Diagnose` impl renders as a `W_FONT_LOAD`
/// warning — the path that surfaces a silently-ignored directive to the user.
pub fn font_load_failure(source: &Source) -> Option<(String, String)> {
    let path = source.iter().rev().find_map(|d| match d {
        GenDirective::Font { path } => Some(path),
        _ => None,
    })?;
    match crate::font::TtfFont::from_path(std::path::Path::new(path)) {
        Ok(_) => None,
        Err(reason) => Some((path.clone(), reason)),
    }
}

/// Lower one authored [`GenDirective::Text`] into stroke-font features on its named slab
/// (Decision 9). The shared [`crate::font::text_strokes`] produces the glyph centreline
/// polylines in a local frame (left-origin — board text's authored `at` *is* the origin,
/// so it stays [`Justify::Left`](crate::font::Justify::Left)); each is then rotated by
/// `orient` about that origin (exact for [`Orient::IDENTITY`]), translated to `at`, and
/// traced at a pen width of `height / 8` on the named slab's z (an unknown name is a hard
/// error). The feature's [`Role`] is **forward-queried from the resolved slab** — silk
/// slabs are [`Role::Marking`], a fab slab is [`Role::Datum`] (Decision 15) — exactly as
/// [`crate::part::graphic_features`] takes a graphic's role from its slab, rather than
/// hardcoding `Marking` (which silently shipped fab-slab text onto silk). The features are
/// **netless** — marking/fab surface geometry carries no electrical identity.
fn text_features(
    string: &str,
    at: Point,
    height: Nm,
    layer: &str,
    orient: Orient,
    su: &Stackup,
    font: Option<&crate::font::TtfFont>,
) -> Result<Vec<NetFeature>, String> {
    let slab = su.slab(layer).ok_or_else(|| {
        let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
        format!("unknown slab `{layer}` (available: {})", names.join(", "))
    })?;
    let (role, z) = (slab.role.clone(), slab.z);
    // rotate about the text origin, then place at `at`.
    let place = |local: Point| {
        let r = orient.apply(local);
        Point {
            x: r.x + at.x,
            y: r.y + at.y,
        }
    };
    let mut out = Vec::new();
    if let Some(font) = font {
        // Outline font: each glyph is a filled `Area` already — place it (no pen trace).
        for shape in crate::font::text_regions(string, height, crate::font::Justify::Left, font) {
            out.push(NetFeature::netless(Feature::prism(
                role.clone(),
                shape.map_points(place),
                z,
            )));
        }
    } else {
        // Stroke font: trace each centreline polyline at a visible pen width.
        let pen = (height / 8).max(1);
        for stroke in crate::font::text_strokes(string, height, crate::font::Justify::Left) {
            let pts: Vec<Point> = stroke.into_iter().map(place).collect();
            out.push(NetFeature::netless(Feature::prism(
                role.clone(),
                Shape2D::trace(pts, pen),
                z,
            )));
        }
    }
    Ok(out)
}
