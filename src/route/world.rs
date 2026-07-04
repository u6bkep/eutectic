//! The world-frame derivation producer cluster (Decision 16c): the single
//! [`world_features`] query and its supporting layer/slab bridges, plus the [`Pour`]
//! view. The autorouter consumes several of these `pub(crate)` items, so their
//! `crate::route::` reachability is preserved via the facade.

use crate::doc::{Doc, PinRef};
use crate::geom::{Extent, Feature, Material, NetFeature, Role, Shape2D, Stackup, ZRange};
use crate::id::NetId;
use crate::part::{PartLib, PinRole};
use crate::region::{DEFAULT_CIRCLE_SEGS, Region, difference, shape_to_region, union_all};
use std::collections::BTreeMap;

use super::model::Layer;

/// The copper layers of a stackup with their slab z, top-down, as `(Layer, ZRange)`.
/// `Top` is the highest-z copper, `Bottom` the lowest, `Inner(k)` those between —
/// consistent with [`Layer::depth`]. A **router-internal** ordinal bridge (Decision 13
/// rule 2): the autorouter's grid is positional, so it maps slab z ↔ ordinal here at its
/// own boundary; nothing persisted uses it.
pub(crate) fn copper_layers_z(stackup: &Stackup) -> Vec<(Layer, ZRange)> {
    let slabs = stackup.copper_slabs();
    let n = slabs.len();
    // This mapping assigns Top to index 0 and Bottom to the last, trusting `copper_slabs()`
    // to return the copper top-first (it sorts by `Reverse(z.hi)`). Pin that invariant: the
    // slabs must be in non-increasing z order, else the ordinal↔slab bridge (and every
    // consumer keyed on it — the autorouter grid, DRC/export forward queries) silently
    // mislabels layers.
    debug_assert!(
        slabs.windows(2).all(|w| w[0].z.hi >= w[1].z.hi),
        "copper_slabs() must be ordered top-first (non-increasing z); copper_layers_z relies on it"
    );
    slabs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let layer = if i == 0 {
                Layer::Top
            } else if i + 1 == n {
                Layer::Bottom
            } else {
                Layer::Inner((i - 1) as u8)
            };
            (layer, s.z)
        })
        .collect()
}

/// The slab **name** of a router ordinal [`Layer`] (the outward half of the router's
/// ordinal↔name bridge): `Top`→top copper slab, `Bottom`→bottom copper, `Inner(n)`→the
/// `1+n`-th from top. `None` if that copper layer is absent. Router-internal
/// (Decision 13 rule 2).
pub(crate) fn layer_slab_name(stackup: &Stackup, l: Layer) -> Option<String> {
    let cu = stackup.copper_slabs();
    let idx = match l {
        Layer::Top => 0,
        Layer::Bottom => cu.len().checked_sub(1)?,
        // Inner copper is strictly *between* the outer layers; guard against `Inner(n)`
        // aliasing onto `Bottom` (or past it) on a stackup with too few inner layers.
        Layer::Inner(n) => {
            let idx = 1 + n as usize;
            if idx + 1 >= cu.len() {
                return None;
            }
            idx
        }
    };
    cu.get(idx).map(|s| s.name.clone())
}

/// The router ordinal [`Layer`] a copper slab **name** maps to (the inward half of the
/// router's bridge): `None` for an unknown or non-copper name. Router-internal. The
/// autorouter now works in ordinals derived once from [`copper_layers_z`], so this
/// inward half currently has only the round-trip test as a caller; it is retained as the
/// documented sibling of [`layer_slab_name`] (Decision 13 rule 2) for a future
/// name→ordinal consumer.
#[allow(dead_code)]
pub(crate) fn slab_layer(stackup: &Stackup, name: &str) -> Option<Layer> {
    copper_layers_z(stackup)
        .into_iter()
        .zip(stackup.copper_slabs())
        .find(|((_, _), s)| s.name == name)
        .map(|((l, _), _)| l)
}

/// World-frame copper as converged [`NetFeature`]s — every trace, via, and netted pad
/// reduced to a Feature prism, each paired with the single copper [`Layer`] it sits on.
/// A trace is one `Conductor` prism on its layer's slab; a via **fans out** to one prism
/// per copper slab it spans; a netted pad uses
/// [`PinDef::pad_features`](crate::part::PinDef::pad_features) (its `Void` drill is not
/// copper and is dropped here). Every emitted feature is single-slab, so a different-net
/// pair that z-overlaps necessarily shares that slab — which is what lets
/// [`check_drc`](super::check_drc) gate clearance with
/// [`Feature::clears`](crate::geom::Feature::clears) (z-overlap ∧ distance) and report on
/// that one layer. This is the converged producer that replaced the former discrete
/// same-layer copper-piece model.
pub(crate) fn net_features(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
    stackup: &Stackup,
) -> Vec<(String, NetFeature)> {
    let mut pin_net: BTreeMap<PinRef, NetId> = BTreeMap::new();
    for (nid, pins) in netlist {
        for (pr, _) in pins {
            pin_net.insert(pr.clone(), nid.clone());
        }
    }
    let cu = stackup.copper_slabs();
    let mut out: Vec<(String, NetFeature)> = Vec::new();

    // Traces: one Conductor prism on the trace's named copper slab. An unresolvable /
    // non-copper name contributes nothing (a committed trace always resolves — the
    // commit-time slab gate in `command::apply`).
    for t in doc.traces.values() {
        if let Some(z) = cu.iter().find(|s| s.name == t.layer).map(|s| s.z) {
            let f = Feature::prism(Role::Conductor, Shape2D::trace(t.path.clone(), t.width), z);
            out.push((t.layer.clone(), NetFeature::new(Some(t.net.clone()), f)));
        }
    }

    // Vias: one Conductor prism per copper slab the via spans (single-slab fan-out).
    for v in doc.vias.values() {
        for s in v.spanned_slabs(&cu) {
            let f = Feature::prism(Role::Conductor, Shape2D::disc(v.at, v.pad / 2), s.z);
            out.push((s.name.clone(), NetFeature::new(Some(v.net.clone()), f)));
        }
    }

    // Pads: reuse the Phase-1 lowering. Attribute each Conductor feature to its copper
    // slab by a **forward** per-slab query — a pad feature's z *is* one copper slab's z
    // (a surface pad sits on one, a Through pad fans out to one feature per slab), so we
    // scan the stackup's copper slabs and keep the one whose z it matches. Identity flows
    // forward from the stackup; it is never reconstructed from the derived z (Decision 13
    // rule 3 — no inverse projections).
    for c in doc.components.values() {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for pin in &def.pins {
            let Some(net) = pin_net.get(&PinRef::new(&c.id, &pin.number)) else {
                continue;
            };
            for f in pin.pad_features(c, stackup) {
                if f.role != Role::Conductor {
                    continue; // the drill / mask-opening Void is not copper geometry
                }
                let Extent::Prism { z, .. } = &f.extent;
                if let Some(s) = cu.iter().find(|s| s.z == *z) {
                    out.push((s.name.clone(), NetFeature::new(Some(net.clone()), f)));
                }
            }
        }
    }
    out
}

// ----------------------------------------------------------------------------
// The unified world-frame feature producer (Decision 16c).
// ----------------------------------------------------------------------------

/// Resolve a region's **slab name** to its copper z (Decision 13): the slab must be a
/// copper slab. `None` if the name is unknown or names a non-copper slab — a net-bound
/// pour on silk is nonsense, rejected up front by [`crate::elaborate::features`], the
/// materialization gate; here it contributes no pour.
fn region_copper_z(su: &Stackup, name: &str) -> Option<ZRange> {
    su.copper_slabs()
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.z)
}

/// **The** single producer of world-frame [`Feature`]s (Decision 16c): one query that
/// emits *everything* physical — the substrate, solder-mask solids, board-authored
/// keep-outs / voids / markings, every placed pad (copper + drill/mask `Void`s),
/// footprint graphics + text, routed traces and vias (+ their drill `Void`s), and copper
/// pours — each paired with the net it carries (an annotation, never a field on
/// `Feature`; Decision 12.1). DRC, the autorouter self-check, and every exporter are
/// *filters over this one stream* by role / net, replacing the former parallel copper
/// producer that left keep-outs unenforced (issue 0023).
///
/// Fallible only through the slab-name materialization gate ([`crate::elaborate::features`]):
/// an unknown slab name is a hard error. A committed `Doc` always resolves cleanly.
///
/// `rules` is read only for the pour-knockout clearance (a pour's fill is a derived fab
/// artifact of the authored outline minus the clearance-expanded foreign copper;
/// Decision 4). Emission order is stable (source geometry, then per-component pad
/// `Void`s + graphics + text, then routed copper, then pours in source order) so every
/// derived export stays byte-stable.
pub fn world_features(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
    rules: &super::model::DesignRules,
    su: &Stackup,
) -> Result<Vec<NetFeature>, String> {
    // Source-only geometry: substrate `Area`, mask solids, keep-outs, region voids, and
    // lowered board text. (Conductor pours are *not* emitted there — they need the
    // placed copper to knock out, so they are lowered below.)
    let mut out = crate::elaborate::features(&doc.source)?;

    // Routed + placed copper conductors (traces, vias fanned per spanned slab, and pad
    // copper) via the shared lowering — kept as an internal helper of this producer (it
    // is also the autorouter's self-check input).
    let copper = net_features(doc, lib, netlist, su);

    // Via drills become geometry (Decision 5 / 16b): each via a full-stackup **plated**
    // through-cut `Void` (a disc of the drill diameter). `Via.drill` was a scalar that
    // never reached the drill file — now it is an enumerable `Void`, like a pad drill.
    if let Some(full) = su.full_z() {
        for v in doc.vias.values() {
            out.push(NetFeature::netless(
                Feature::prism(Role::Void, Shape2D::disc(v.at, v.drill / 2), full)
                    .with_material(Material::named("copper")),
            ));
        }
    }

    // Per-component non-conductor pad features (plated drill `Void`s + mask openings) and
    // footprint graphics + text (Markings / a fab Datum). The pad *conductor* copper rode
    // in through `copper` above; this completes the stream with the rest so the one
    // producer carries every pad + footprint feature. `refdes` is a whole-document
    // annotation query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);
    // Doc-wide outline font (Decision 17), resolved once per pass; `None` ⇒ the stroke
    // font. Same resolve-once pattern as the SVG/silk producers.
    let font = crate::elaborate::resolve_font(&doc.source);
    for (id, c) in &doc.components {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for pin in &def.pins {
            for f in pin.pad_features(c, su) {
                if f.role != Role::Conductor {
                    out.push(NetFeature::netless(f));
                }
            }
        }
        for f in crate::part::graphic_features(def, c, su) {
            out.push(NetFeature::netless(f));
        }
        let rd = refdes.get(id).map(String::as_str).unwrap_or("");
        let lbl = crate::annotate::label(c, def, &reg);
        for f in crate::part::text_features(def, c, su, rd, &lbl, font.as_ref()) {
            out.push(NetFeature::netless(f));
        }
    }

    // Copper conductors into the stream (net annotation preserved; consumers re-derive
    // the slab pairing via `feature_slab`).
    out.extend(copper.iter().map(|(_, nf)| nf.clone()));

    // Copper pours: each authored `Conductor` region lowers to a `NetFeature` whose
    // `Feature` is a filled `Shape2D::Area` — the outline ∖ the clearance-expanded
    // foreign copper (same-net copper is what the pour connects to, so it is *not*
    // knocked out). Emitted in source order for byte-stable export.
    for r in crate::elaborate::regions(&doc.source) {
        if r.role != Role::Conductor {
            continue;
        }
        let Some(name) = &r.net else { continue };
        let Some(z) = region_copper_z(su, &r.layer) else {
            continue;
        };
        let net = NetId::new(name.clone());
        let outline = shape_to_region(&r.shape, DEFAULT_CIRCLE_SEGS);
        let obstacles: Vec<Region> = copper
            .iter()
            .filter(|(l, nf)| *l == r.layer && nf.net.as_ref() != Some(&net))
            .map(|(_, nf)| {
                let Extent::Prism { shape, .. } = &nf.feature.extent;
                shape_to_region(&shape.inflated(rules.min_clearance), DEFAULT_CIRCLE_SEGS)
            })
            .collect();
        let fill = difference(&outline, &union_all(obstacles));
        out.push(NetFeature::new(
            Some(net),
            Feature::prism(Role::Conductor, Shape2D::Area { region: fill }, z),
        ));
    }
    Ok(out)
}

/// A copper pour materialised for export/DRC rendering: its `net`, the copper slab
/// **name** it fills (Decision 13), and its knocked-out `fill` region. A thin view over
/// the [`Shape2D::Area`] conductor features [`world_features`] emits, so pour geometry
/// has exactly one source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pour {
    pub net: NetId,
    pub layer: String,
    pub fill: Region,
}

/// Every copper pour of a document as [`Pour`]s, read from the unified [`world_features`]
/// stream (its `Conductor` `Area` features). The pour-rendering exporters (Gerber region
/// fills, SVG pour paths) fold through this, so pours are the same features DRC sees.
/// Deterministic (source order). Panics only if `world_features` errors, which cannot
/// happen on a committed doc (the commit-time slab gate) — see [`check_drc`](super::check_drc).
pub fn pours(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
    rules: &super::model::DesignRules,
    su: &Stackup,
) -> Vec<Pour> {
    let world = world_features(doc, lib, netlist, rules, su)
        .expect("world_features on a committed doc (slab gate enforced at commit)");
    world
        .into_iter()
        .filter_map(|nf| {
            let net = nf.net?;
            if nf.feature.role != Role::Conductor {
                return None;
            }
            let layer = super::drc::feature_slab(su, &nf.feature)?;
            let Extent::Prism { shape, .. } = nf.feature.extent;
            match shape {
                Shape2D::Area { region } => Some(Pour {
                    net,
                    layer,
                    fill: region,
                }),
                _ => None,
            }
        })
        .collect()
}
