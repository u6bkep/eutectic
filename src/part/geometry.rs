//! World-transform and feature-producing functions for placed parts: the fold from
//! stored component-local [`PadGeo`]/[`FpGraphic`]/[`FpText`] into world-frame
//! [`geom::Feature`]s, plus the courtyard keep-out derivations.

use crate::doc::{Component, Nm, Point};
use crate::geom;
use crate::geom::Shape2D;
use crate::part::{Drill, FpTextKind, PadCopper, PadLayers, PartDef, PinDef};

/// Absolute (world) position of a pin on a placed component instance:
/// `component position + orient.apply(local pin offset)`. Exact for the
/// lattice-symmetry orientations (cardinals + flips), correctly-rounded otherwise
/// (see [`Orient::apply`](crate::doc::Orient::apply)). Returns `None` if the pin is
/// unknown.
pub fn pin_world(comp: &Component, def: &PartDef, pin: &str) -> Option<Point> {
    let off = def.pin_offset(pin)?;
    let r = comp.orient.apply(off);
    Some(Point {
        x: comp.pos.value.x + r.x,
        y: comp.pos.value.y + r.y,
    })
}

/// Lift a component-local point into world space on a placed component: apply the
/// orientation, translate to the component position. Exact for cardinals/flips,
/// correctly-rounded otherwise.
pub fn to_world(comp: &Component, p: Point) -> Point {
    let r = comp.orient.apply(p);
    Point {
        x: comp.pos.value.x + r.x,
        y: comp.pos.value.y + r.y,
    }
}

/// World-frame copper shape of a pad region on a placed component.
pub fn pad_copper_world(comp: &Component, c: &PadCopper) -> Shape2D {
    c.shape.map_points(|p| to_world(comp, p))
}

impl PinDef {
    /// World-frame physical features for this pin's pad: each copper region as a
    /// [`Role::Conductor`](geom::Role) prism on its layer's z; a solder-mask opening
    /// per copper region as a [`Role::Void`](geom::Role) prism (the copper expanded by
    /// [`geom::MASK_EXPANSION`]) at its side's mask slab z; plus the drill (if any) as a
    /// [`Role::Void`](geom::Role) prism spanning the *full* stackup. Empty if the pin
    /// has no pad.
    ///
    /// The mask opening deletes mask material where the pad is exposed (Decision 13 — an
    /// opening is a `Void` at mask z, not a negative layer): a surface pad opens its
    /// resolved side's mask, a through pad opens both. The mask slab is found by
    /// **role and z-position** ([`Stackup::top_mask`]/[`Stackup::bottom_mask`] — the
    /// `Role::Mask` slab immediately outboard of the outer copper), respecting the flip;
    /// a custom-named mask slab is opened just the same, and a side with no mask slab
    /// opens nothing. These `Void`s are not copper, so the DRC copper producer / the
    /// Gerber copper path drop them exactly as they drop the drill `Void`.
    ///
    /// The component's position + cardinal [`Orient`](crate::doc::Orient) place the
    /// geometry — copper via [`pad_copper_world`] (the pad's local offset is already
    /// baked into the copper [`Shape2D`]); the drill is built in component-local
    /// coords centred on the pad centre ([`PinDef::offset`] for a round drill, the
    /// stored slot endpoints for a slot — both in `offset`'s frame) and mapped with
    /// the same [`to_world`] transform. The [`Stackup`](geom::Stackup) resolves the
    /// layer-relative [`PadLayers`] to absolute z: `Top`/`Bottom` to the outer copper
    /// z, `Through` **fanned out** to one conductor feature per copper slab (the
    /// "annulus on every copper layer" semantics). Features whose z is degenerate in
    /// the stackup (a missing accessor) are skipped.
    ///
    /// This is the [`PadGeo`](crate::part::PadGeo)-derives-`Feature`s fold of the
    /// geometry-model convergence (docs/geometry-model-convergence.md, Decision 12): the
    /// compact `PadGeo` stays stored on the pin; the features are the derived view. Purely
    /// additive — it does not alter or replace any existing geometry.
    pub fn pad_features(&self, comp: &Component, stackup: &geom::Stackup) -> Vec<geom::Feature> {
        let Some(pad) = &self.pad else {
            return Vec::new();
        };
        // A flipped (bottom-side) component swaps its outer-layer copper: a `Top` pad
        // lands on the board bottom and vice-versa. Derived from the orientation — no
        // side flag. (The copper *shape* is already flipped by `pad_copper_world`'s
        // `apply`; only the layer assignment needs swapping. `Through` is unaffected.)
        let flipped = comp.orient.is_bottom();
        let mut features = Vec::new();
        for cu in &pad.copper {
            let world = pad_copper_world(comp, cu);
            // Solder-mask opening: the pad copper, expanded by the mask margin, deletes
            // mask material on the side(s) it is exposed (Decision 13 — an opening is a
            // `Void` at mask z, not a negative layer). The mask slab is resolved by
            // **role + z-position** (the `Role::Mask` slab immediately outboard of the
            // outer copper on the pad's resolved side), *not* by a hardcoded name, so a
            // custom-named mask slab is opened exactly like the default F.Mask/B.Mask —
            // symmetric with the by-role mask solid in `elaborate::features`. A side with
            // no mask slab opens nothing (a `Void` is a no-op where no mask exists).
            let opening = world.inflated(geom::MASK_EXPANSION);
            let mask_zs: [Option<geom::ZRange>; 2] = match cu.layers {
                PadLayers::Through => [stackup.top_mask(), stackup.bottom_mask()],
                PadLayers::Top | PadLayers::Bottom => {
                    // XOR with the flip: a Top pad on a flipped part is on the bottom,
                    // so its exposed side (and thus its mask slab) is the bottom mask.
                    if matches!(cu.layers, PadLayers::Top) != flipped {
                        [stackup.top_mask(), None]
                    } else {
                        [stackup.bottom_mask(), None]
                    }
                }
            };
            match cu.layers {
                PadLayers::Top | PadLayers::Bottom => {
                    let is_top_local = matches!(cu.layers, PadLayers::Top);
                    // XOR with the flip: a Top pad on a flipped part is on the bottom.
                    let z = if is_top_local != flipped {
                        stackup.top_copper()
                    } else {
                        stackup.bottom_copper()
                    };
                    if let Some(z) = z {
                        features.push(geom::Feature::prism(geom::Role::Conductor, world, z));
                    }
                }
                PadLayers::Through => {
                    // Fan out: one conductor feature per copper slab, same world shape.
                    for slab in stackup.copper_slabs() {
                        features.push(geom::Feature::prism(
                            geom::Role::Conductor,
                            world.clone(),
                            slab.z,
                        ));
                    }
                }
            }
            for z in mask_zs.into_iter().flatten() {
                features.push(geom::Feature::prism(geom::Role::Void, opening.clone(), z));
            }
        }
        if let Some(drill) = &pad.drill {
            // The drill is a Void that pierces the whole stackup (mask + silk included),
            // centred on the pad centre. A round drill carries no centre, so it sits at
            // the pin offset; a slot's endpoints are already stored in the pin's local
            // frame.
            let local = match *drill {
                Drill::Round { d } => Shape2D::disc(self.offset, d / 2),
                Drill::Slot { a, b, d } => Shape2D::capsule(a, b, d / 2),
            };
            let world = local.map_points(|p| to_world(comp, p));
            if let Some(z) = stackup.full_z() {
                // A pad drill is a *plated* through-hole (its barrel connects the copper
                // it fans out to), so the Void carries a copper material — the
                // plated/non-plated bit the Excellon PTH/NPTH split reads (Decision 16b).
                // Mask-opening Voids stay material-less; a standalone authored `Void`
                // region defaults material-less too, so it drills NPTH.
                features.push(
                    geom::Feature::prism(geom::Role::Void, world, z)
                        .with_material(geom::Material::named("copper")),
                );
            }
        }
        features
    }
}

/// Swap a slab name's leading side prefix `F.`↔`B.` — the side-relative resolution a
/// footprint's own layer references need (its geometry is authored in its top-side
/// frame; a bottom-side placement mirrors every layer to the other side). Names with
/// no `F.`/`B.` prefix (`core`, `In1.Cu`) pass through unchanged. This is the graphic
/// twin of the copper-side XOR in [`PinDef::pad_features`], factored so both stay in
/// step.
pub fn swap_side(layer: &str) -> String {
    if let Some(rest) = layer.strip_prefix("F.") {
        format!("B.{rest}")
    } else if let Some(rest) = layer.strip_prefix("B.") {
        format!("F.{rest}")
    } else {
        layer.to_string()
    }
}

/// World-frame physical features for a placed component's footprint
/// [`graphics`](PartDef::graphics): each [`FpGraphic`](crate::part::FpGraphic) as a prism
/// on its side-resolved slab z, taking its [`Role`](geom::Role) from that slab (silk slabs
/// are [`Role::Marking`](geom::Role), so silk is unchanged; an authored fab slab is
/// [`Role::Datum`](geom::Role), Decision 15). The `graphic_features` sibling to
/// [`PinDef::pad_features`] — the geometry takes the *same* placement path (mapped through
/// [`to_world`], so it rotates/flips with the component), and a bottom-side component swaps
/// each graphic's leading `F.`↔`B.` slab prefix ([`swap_side`], derived from
/// `orient.is_bottom()` exactly as `pad_features` derives the copper side — no side flag).
///
/// A graphic whose (resolved) slab name is absent from the stackup is **skipped**,
/// matching how `pad_features` drops a pad whose copper slab the stackup lacks
/// (`top_copper()`/`bottom_copper()` returning `None`). The default stackup always
/// carries `F/B.SilkS`, so this only bites a custom stackup that omits a silk slab.
/// Markings are netless — silk carries no electrical identity.
pub fn graphic_features(
    def: &PartDef,
    comp: &Component,
    stackup: &geom::Stackup,
) -> Vec<geom::Feature> {
    let flipped = comp.orient.is_bottom();
    let mut features = Vec::new();
    for g in &def.graphics {
        let layer = if flipped {
            swap_side(&g.layer)
        } else {
            g.layer.clone()
        };
        let Some(slab) = stackup.slab(&layer) else {
            continue;
        };
        let world = g.shape.map_points(|p| to_world(comp, p));
        features.push(geom::Feature::prism(slab.role.clone(), world, slab.z));
    }
    features
}

/// World-frame physical features for a placed component's footprint [`texts`](PartDef::texts):
/// each [`FpText`](crate::part::FpText) anchor's resolved string, lowered to stroke geometry
/// (Decision 14). The `text_features` sibling to [`graphic_features`]: the geometry takes the
/// *same* placement path (rotated by the anchor's own `orient` about its `at`, then mapped
/// through [`to_world`], so it rotates/flips with the component and bottom-side text
/// mirrors with zero special-case code), side-swaps its `F.`↔`B.` slab prefix on a
/// bottom placement ([`swap_side`]), and takes its [`Role`](geom::Role) from the resolved
/// slab (so silk text is [`Role::Marking`](geom::Role) and text on a fab/Datum slab
/// renders as `Datum` — nowhere in the Marking-filtered silk outputs, matching graphics).
///
/// The anchor kind resolves live: [`FpTextKind::Reference`] → `refdes`,
/// [`FpTextKind::Label`] → `label`, [`FpTextKind::Literal`] → its own string. The two
/// derived strings are passed in because `refdes` is a whole-document annotation query
/// (see [`annotate::refdes`](crate::annotate::refdes)) that the caller computes once for
/// all components; `label` is per-component ([`annotate::label`](crate::annotate::label)).
///
/// A `hide` anchor produces no features; a text whose (resolved) slab name is absent from
/// the stackup is **skipped** — both exactly like `graphic_features`' skip. Footprint
/// text is centre-anchored ([`Justify::Center`](crate::font::Justify)), unlike board text.
pub fn text_features(
    def: &PartDef,
    comp: &Component,
    stackup: &geom::Stackup,
    refdes: &str,
    label: &str,
    font: Option<&crate::font::TtfFont>,
) -> Vec<geom::Feature> {
    let flipped = comp.orient.is_bottom();
    let mut features = Vec::new();
    for t in &def.texts {
        if t.hide {
            continue;
        }
        let string = match &t.kind {
            FpTextKind::Reference => refdes,
            FpTextKind::Label => label,
            FpTextKind::Literal(s) => s.as_str(),
        };
        let layer = if flipped {
            swap_side(&t.layer)
        } else {
            t.layer.clone()
        };
        let Some(slab) = stackup.slab(&layer) else {
            continue;
        };
        // Local frame: rotate by the anchor's own orient about `at`, offset to `at`, then
        // the SAME `to_world` as graphics — bottom-side mirroring is the component
        // quaternion's, no special case. For an `Area` glyph, `map_points` also
        // renormalizes ring winding under that reflection.
        let place = |p: Point| {
            let r = t.orient.apply(p);
            to_world(
                comp,
                Point {
                    x: r.x + t.at.x,
                    y: r.y + t.at.y,
                },
            )
        };
        if let Some(font) = font {
            // Outline font: filled-`Area` glyphs, centre-anchored like the stroke path.
            for shape in
                crate::font::text_regions(string, t.height, crate::font::Justify::Center, font)
            {
                features.push(geom::Feature::prism(
                    slab.role.clone(),
                    shape.map_points(place),
                    slab.z,
                ));
            }
        } else {
            let pen = (t.height / 8).max(1);
            for stroke in crate::font::text_strokes(string, t.height, crate::font::Justify::Center)
            {
                let world: Vec<Point> = stroke.into_iter().map(place).collect();
                features.push(geom::Feature::prism(
                    slab.role.clone(),
                    Shape2D::trace(world, pen),
                    slab.z,
                ));
            }
        }
    }
    features
}

/// Default extra clearance added around a part's copper extent to form its
/// courtyard keep-out, in nm (~0.25 mm, the KiCad-ish default).
pub const COURTYARD_MARGIN: Nm = 250_000;

/// A part's **courtyard** as origin-centred axis-aligned half-extents `(hw, hh)` in
/// component-local nm: the bounding box of its **pad copper**, made symmetric about
/// the origin and grown by [`COURTYARD_MARGIN`]. This is the keep-out the placement
/// solver uses for overlap-avoidance (issue 0005).
///
/// Derived from real copper extent only, so a footprint-less part (the toy
/// `part_library`, `pad: None`) returns `(0, 0)` — it has no defined physical
/// courtyard, so it is exempt from overlap-avoidance (it is an abstract fixture, not
/// a placeable body). Origin-centred (rather than a true offset bbox) keeps it a
/// single half-extent pair that rotates by swapping `hw`/`hh` on a cardinal turn;
/// real footprints are centred on their origin, so this is tight in practice and
/// conservative otherwise.
pub fn courtyard_half_extents(def: &PartDef) -> (Nm, Nm) {
    // An imported courtyard (Decision 10) is authoritative — proxy its bbox directly so
    // the solver's overlap-avoidance respects the real outline, not the pad hull. The
    // imported outline already carries its own clearance, so no COURTYARD_MARGIN is added.
    if let Some((lo, hi)) = def.courtyard.as_ref().and_then(Shape2D::bbox) {
        let mx = lo.x.abs().max(hi.x.abs());
        let my = lo.y.abs().max(hi.y.abs());
        return (mx, my);
    }
    let (mut mx, mut my) = (0, 0); // max |coordinate| on each axis
    let mut any = false;
    for pin in &def.pins {
        let Some(pad) = &pin.pad else { continue };
        for cu in &pad.copper {
            if let Some((lo, hi)) = cu.shape.bbox() {
                mx = mx.max(lo.x.abs()).max(hi.x.abs());
                my = my.max(lo.y.abs()).max(hi.y.abs());
                any = true;
            }
        }
    }
    if !any {
        return (0, 0);
    }
    (mx + COURTYARD_MARGIN, my + COURTYARD_MARGIN)
}

/// A part's **courtyard** as a real polygon (Decision 10): the convex hull of every
/// pad-copper skeleton vertex, inflated by [`COURTYARD_MARGIN`] (carried as the
/// polygon's Minkowski radius). In **component-local** coordinates, the same frame as
/// the pad copper.
///
/// This is the honest polygonal keep-out — available now for DRC / 3D / render. The
/// placement solver still pushes the cheap axis-aligned [`courtyard_half_extents`]
/// proxy: because this hull is always ⊆ that AABB, a *separate* polygon verify after a
/// converged AABB push can never find an overlap the push left behind, so realising
/// Decision 10's tighter-packing value requires the solver's push itself to consume
/// this polygon — a deferred solver enhancement (issue 0019), not a verify bolt-on.
///
/// Footprint-less parts (the toy `part_library`, every `pad: None`) have no copper, so
/// they return `None` and are exempt from overlap verification — exactly as they are
/// exempt from the proxy push. A degenerate footprint whose copper vertices are
/// collinear (no 2-D hull, e.g. a single round pad) also returns `None`.
///
/// The hull is taken over the skeleton corner vertices ([`Shape2D::points`]); the pad
/// copper's own inflation radius is *not* added, so for round/oval pads the margin is
/// measured from the pad centre-line rather than its copper edge. `COURTYARD_MARGIN`
/// (~0.25 mm) dominates at typical pad scale; the axis-aligned proxy
/// ([`courtyard_half_extents`], which *does* include the radius via `bbox`) stays the
/// conservative pusher.
pub fn courtyard_shape(def: &PartDef) -> Option<Shape2D> {
    // Decision 10: an imported courtyard polygon IS the authoritative courtyard — it
    // wins over the derived pad-hull below.
    if let Some(court) = &def.courtyard {
        return Some(court.clone());
    }
    let mut pts = Vec::new();
    for pin in &def.pins {
        let Some(pad) = &pin.pad else { continue };
        for cu in &pad.copper {
            pts.extend(cu.shape.points());
        }
    }
    if pts.is_empty() {
        return None;
    }
    let hull = geom::convex_hull(&pts);
    if hull.len() < 3 {
        return None; // no 2-D hull (a lone pad / collinear pads): no polygon courtyard
    }
    Some(Shape2D::polygon_path(
        geom::Path::polyline(hull),
        COURTYARD_MARGIN,
    ))
}
