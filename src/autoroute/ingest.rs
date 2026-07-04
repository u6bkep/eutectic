//! Reading the doc into the router's working form ([`Pad`], [`doc_netlist`],
//! [`pad_layers`]) and the honesty backstop [`verify_and_prune`] that re-checks the
//! proposed copper against the real DRC before the run reports a net routed.

use crate::command::Command;
use crate::doc::{Doc, PinRef, Point};
use crate::elaborate::stackup;
use crate::geom::{Extent, Feature, KeepoutKind, NetFeature, Role, Shape2D, Stackup};
use crate::id::NetId;
use crate::part::{PartLib, PinRole};
use crate::route::{DesignRules, Layer, layer_slab_name, world_features};
use std::collections::{BTreeMap, BTreeSet};

use super::AutorouteResult;

/// A pad: its world centre, the copper layer ordinals its geometry occupies, and whether
/// it actually carries pad copper (a bare terminal has none — see [`pad_layers`]).
pub(super) struct Pad {
    pub(super) at: Point,
    pub(super) layers: Vec<usize>,
    pub(super) has_copper: bool,
}

/// The membership-only netlist a `Doc` carries (roles are irrelevant to clearance /
/// world_features, so a `Passive` placeholder is fine).
pub(super) fn doc_netlist(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|m| (m.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}

/// The grid-layer ordinals a pin's pad copper occupies, and whether it carries any pad
/// copper at all. Scan the pad's conductor features and match each feature's slab z to a
/// copper slab, returning its ordinal: an SMD pad matches one layer; a through-hole pad
/// matches every copper slab. A pin with **no** pad copper (a bare connection point — the
/// toy library's parts, or a footprint whose pads the stackup lacks) is layer-agnostic,
/// so it may seed on *any* layer (mirroring the ratsnest, which treats pads as all-layer
/// points) — the returned `has_copper` is `false` so the caller instead adds it as a
/// point obstacle for other nets.
pub(super) fn pad_layers(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
    pr: &PinRef,
    layers: &[Layer],
) -> (Vec<usize>, bool) {
    let all = || (0..layers.len()).collect::<Vec<_>>();
    let cu = su.copper_slabs();
    let Some(c) = doc.components.get(&pr.comp) else {
        return (all(), false);
    };
    let Some(def) = lib.get(&c.part) else {
        return (all(), false);
    };
    let Some(pin) = def.pins.iter().find(|p| p.number == pr.pin) else {
        return (all(), false);
    };
    let mut out = Vec::new();
    for f in pin.pad_features(c, su) {
        if f.role != Role::Conductor {
            continue;
        }
        let Extent::Prism { z, .. } = &f.extent;
        if let Some(slab) = cu.iter().find(|s| s.z == *z)
            && let Some(k) = layers
                .iter()
                .position(|&l| layer_slab_name(su, l).as_deref() == Some(slab.name.as_str()))
            && !out.contains(&k)
        {
            out.push(k);
        }
    }
    if out.is_empty() {
        return (all(), false);
    }
    out.sort_unstable();
    (out, true)
}

/// Self-honesty (issue 0003): the grid's "clearance-clean by construction" invariant
/// fails at sub-grid pitch (and never covered the off-grid pad stubs), so do not
/// trust it. Check each *proposed* piece of copper against all other-net copper
/// (existing pads / pre-existing traces+vias + other proposed copper) with the same
/// `geom` clearance DRC uses, and additionally against copper/route keep-outs and the
/// board edge (issue 0023 — the router now masks these out during search, so a clash
/// here means a construction slip; checking it makes `routed` mean *DRC-clean*, not
/// merely copper-vs-copper clean). Any routed net whose proposed copper clashes is
/// dropped — its commands removed, the net moved to `unrouted`. Dropping every clashing
/// net is conservative (it can drop a net that a smarter order/rip-up would keep — that
/// is future work, issue 0008); the point here is honesty, not optimality.
///
/// Decision 19a — the pours re-derive with the proposed copper included. The proposed
/// commands are applied to a scratch `Doc` clone and `world_features` is re-run on it, so
/// the pour fills this check sees have **already retreated** around the proposed vias/
/// traces (the automatic anti-pad). A via punched through a foreign plane verifies clean
/// because the re-derived fill is no longer under it; a via too close to another net's
/// *non-pour* copper (a trace/pad/via) still fails. Pour-vs-solid is therefore skipped
/// here exactly as [`crate::route::check_drc`] skips it — the pour was knocked out at this
/// same `min_clearance`, so any residual proximity is tessellation slop, not a real short.
/// Cost: one extra `world_features` pass (one re-derivation of every pour) per autoroute
/// call — the pour boolean ops dominate; measured acceptable on the PoC (see the branch
/// report).
pub(super) fn verify_and_prune(
    doc: &Doc,
    lib: &PartLib,
    rules: &DesignRules,
    result: &mut AutorouteResult,
) {
    let su = stackup(&doc.source);
    let cu = su.copper_slabs();

    // This run's proposed copper, lowered the same way `net_features` lowers the doc's:
    // a trace is one Conductor prism on its named slab; a via fans out to one prism per
    // copper slab it spans (so every feature is single-slab). Kept as the by-net set we
    // judge; the re-derived `world` below is the obstacle field it is judged against.
    let mut proposed: Vec<NetFeature> = Vec::new();
    for cmd in &result.commands {
        match cmd {
            Command::AddTrace(_, t) => {
                if let Some(z) = cu.iter().find(|s| s.name == t.layer).map(|s| s.z) {
                    let f =
                        Feature::prism(Role::Conductor, Shape2D::trace(t.path.clone(), t.width), z);
                    proposed.push(NetFeature::new(Some(t.net.clone()), f));
                }
            }
            Command::AddVia(_, v) => {
                for s in v.spanned_slabs(&cu) {
                    let f = Feature::prism(Role::Conductor, Shape2D::disc(v.at, v.pad / 2), s.z);
                    proposed.push(NetFeature::new(Some(v.net.clone()), f));
                }
            }
            _ => {}
        }
    }

    // Apply the proposed commands to a scratch clone and re-derive the world from it: the
    // pours retreat around the proposed copper (Decision 19a), so `world` is the fill that
    // will actually exist post-commit — not the stale pre-route fill. `command::apply` is
    // the same atomic path a real commit takes; on the rare chance the transaction is
    // rejected (it should not be — these are ordinary AddTrace/AddVia) fall back to the
    // pre-route world, which is stricter (stale fill blocks more), never falsely clean.
    let txn = crate::command::Transaction(result.commands.clone());
    let scratch = crate::command::apply(doc, &txn, lib, 0).unwrap_or_else(|_| doc.clone());
    let world = world_features(&scratch, lib, &doc_netlist(&scratch), rules, &su)
        .expect("world_features on a committed doc (slab gate enforced at commit)");

    // Keep-out features (copper/route kinds only) and the board substrate region, for the
    // keepout + edge checks that make `routed` mean DRC-clean.
    let keepouts: Vec<&Feature> = world
        .iter()
        .filter(|nf| {
            matches!(
                nf.feature.role,
                Role::Keepout(KeepoutKind::Copper | KeepoutKind::Route)
            )
        })
        .map(|nf| &nf.feature)
        .collect();
    let substrate: Option<&crate::region::Region> = world.iter().find_map(|nf| {
        if nf.feature.role != Role::Substrate {
            return None;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        shape.region()
    });

    // Is a conductor feature a derived pour (the `is_pour ⟺ Shape2D::Area` invariant,
    // Decision 16)? Pour-vs-solid is skipped (the pour retreated at `min_clearance`).
    let is_pour = |f: &Feature| matches!(&f.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. }));

    let mut unclean: BTreeSet<NetId> = BTreeSet::new();
    for p in &proposed {
        let Some(pnet) = &p.net else { continue };
        if unclean.contains(pnet) {
            continue;
        }
        // Copper-vs-copper against the re-derived world (which already contains the
        // proposed copper, so proposed-vs-proposed different-net clashes are caught here
        // too). A proposed solid piece is skipped against foreign *pours*, exactly as
        // `check_drc` skips pour-vs-solid: the re-derived fill retreated around it at this
        // same `min_clearance` (Decision 19a's automatic anti-pad), so any residual
        // proximity is tessellation slop of the circle-approximated fill boundary, not a
        // real short — checking it would false-positive and prune legitimate vias-in-plane
        // (confirmed: removing this skip prunes `via_inside_foreign_plane_verifies_clean`).
        // The via is still checked against foreign SOLID copper, keep-outs, and the edge.
        let copper_clash = world.iter().any(|o| {
            o.feature.role == Role::Conductor
                && o.net.as_ref() != Some(pnet)
                && !is_pour(&o.feature)
                && !p.feature.clears(&o.feature, rules.min_clearance)
        });
        // Keep-out intrusion (z-gated by `Feature::clears`).
        let keepout_clash = keepouts
            .iter()
            .any(|kf| !p.feature.clears(kf, rules.keepout_clearance));
        // Board-edge: solid copper grown by the edge rule must stay inside the board.
        let edge_clash = substrate.is_some_and(|board| {
            let Extent::Prism { shape, .. } = &p.feature.extent;
            let grown = crate::region::shape_to_region(
                &shape.inflated(rules.edge_clearance),
                crate::region::DEFAULT_CIRCLE_SEGS,
            );
            !crate::region::difference(&grown, board).is_empty()
        });
        if copper_clash || keepout_clash || edge_clash {
            unclean.insert(pnet.clone());
        }
    }
    if unclean.is_empty() {
        return;
    }
    result.commands.retain(|c| match c {
        Command::AddTrace(_, t) => !unclean.contains(&t.net),
        Command::AddVia(_, v) => !unclean.contains(&v.net),
        _ => true,
    });
    result.routed.retain(|n| !unclean.contains(n));
    result.unrouted.extend(unclean);
    result.unrouted.sort();
    result.unrouted.dedup();
}
