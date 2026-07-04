//! The design-rule check (tier-3): the [`Violation`] domain type and the pure
//! [`check_drc`] query body the incremental engine in `query.rs` runs. All geometry
//! is integer nanometres; distance comparisons are exact `i128` against squared
//! thresholds, so no float nondeterminism leaks into a violation set.

use crate::doc::{Doc, Nm};
use crate::elaborate::stackup;
use crate::geom::kernel::{DEFAULT_CIRCLE_SEGS, Region, difference, shape_to_region, union_all};
use crate::geom::{Extent, Feature, KeepoutKind, Role, Shape2D, Stackup};
use crate::id::{NetId, TraceId};
use crate::part::{PartLib, PinRole, pin_world};
use std::collections::BTreeMap;

use super::connect::{PinPoint, pin_islands, pin_point};
use super::model::{DesignRules, Trace, Via};
use super::world::world_features;

/// A single DRC violation. Deliberately small and `Ord` so the violation *set* is
/// canonical and cheaply comparable — that is what lets the query engine's early
/// cutoff fire (an edit that does not change this set does not propagate).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Violation {
    /// Copper of two different nets is closer than the clearance rule allows on a
    /// copper slab (named — Decision 13). Net ids are stored sorted so a pair is
    /// reported once regardless of which side was scanned first.
    Clearance { a: NetId, b: NetId, layer: String },
    /// A trace narrower than the minimum width rule.
    MinWidth { trace: TraceId, width: Nm },
    /// A net whose pins are not all electrically joined by the routing (ratsnest):
    /// `islands` is how many disconnected pin groups remain (>1 ⇒ unrouted /
    /// partially routed; the net is fully routed iff this would be 1).
    Unrouted { net: NetId, islands: usize },
    /// Copper of `net` intrudes a `Role::Keepout` region of the given [`KeepoutKind`]
    /// (issue 0023). Only copper-relevant kinds ([`KeepoutKind::Copper`]/`Route`) are
    /// checked; component keep-outs (courtyards) are a placement concern, not DRC.
    Keepout { net: NetId, kind: KeepoutKind },
    /// Copper of `net` is closer to the board edge (or a cutout wall) than the
    /// edge-clearance rule allows, or spills outside the board entirely (issue 0023).
    EdgeClearance { net: NetId },
}

/// DRC violations stay a typed domain result (the autorouter consumes them as
/// data); this renders them into the shared diagnostic vocabulary for display.
impl crate::diagnostic::Diagnose for Violation {
    fn diagnostics(&self) -> Vec<crate::diagnostic::Diagnostic> {
        use crate::diagnostic::{Diagnostic, Location};
        let d = match self {
            Violation::Clearance { a, b, layer } => Diagnostic::error(
                "E_DRC_CLEARANCE",
                format!("nets `{a}` and `{b}` are closer than clearance on `{layer}`"),
                Location::Net(a.clone()),
            ),
            Violation::MinWidth { trace, width } => Diagnostic::error(
                "E_DRC_MIN_WIDTH",
                format!("trace `{trace}` width {width}nm is below the minimum"),
                Location::Trace(*trace),
            ),
            Violation::Unrouted { net, islands } => Diagnostic::error(
                "E_DRC_UNROUTED",
                format!("net `{net}` is not fully routed ({islands} disconnected islands)"),
                Location::Net(net.clone()),
            ),
            Violation::Keepout { net, kind } => Diagnostic::error(
                "E_DRC_KEEPOUT",
                format!("copper of net `{net}` intrudes a {kind:?} keep-out"),
                Location::Net(net.clone()),
            ),
            Violation::EdgeClearance { net } => Diagnostic::error(
                "E_DRC_EDGE_CLEARANCE",
                format!("copper of net `{net}` is too close to the board edge"),
                Location::Net(net.clone()),
            ),
        };
        vec![d]
    }
}

/// Run the design-rule check over a document's routing. Pure and deterministic:
/// the only inputs are the routed copper (tier-2), the placement geometry (for pad
/// world positions, via `lib`), and the resolved netlist (`netlist`, which fixes
/// which pins each net must join). The query engine records those three as its
/// dependencies; this function does no dependency tracking itself.
///
/// Returns a canonical (sorted, de-duplicated) `Vec<Violation>`.
pub fn check_drc(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(crate::doc::PinRef, PinRole)>>,
    rules: &DesignRules,
) -> Vec<Violation> {
    let mut out: Vec<Violation> = Vec::new();

    // --- 1. Minimum width: every trace's width >= the rule. ---
    for (tid, t) in &doc.traces {
        if t.width < rules.min_trace_width {
            out.push(Violation::MinWidth {
                trace: *tid,
                width: t.width,
            });
        }
    }

    // World position (pad *centre*) of every net-member pad, per net, paired with the
    // copper slabs the pad copper occupies (Decision 19c). The ratsnest joins pads by
    // incidence at these points; pour-island incidence is now layer-honest (a pad joins a
    // plane only where its copper is on that plane's slab). Clearance, separately, uses
    // the pads' real copper geometry.
    let su = stackup(&doc.source);
    let mut net_pads: BTreeMap<NetId, Vec<PinPoint>> = BTreeMap::new();
    for (nid, pins) in netlist {
        let mut pts = Vec::new();
        for (pr, _role) in pins {
            if let Some(c) = doc.components.get(&pr.comp)
                && let Some(def) = lib.get(&c.part)
                && let Some(p) = pin_world(c, def, &pr.pin)
            {
                pts.push(pin_point(doc, lib, &su, p, pr));
            }
        }
        net_pads.insert(nid.clone(), pts);
    }

    // Everything physical, in the world frame, from the one unified producer
    // ([`world_features`], Decision 16c) — copper (pads/traces/vias/pours), the
    // substrate, keep-outs, voids, mask and markings. DRC filters it by role/net; the
    // former parallel copper producer (`net_features` alone) is gone, so keep-outs now
    // reach DRC (issue 0023).
    //
    // A committed `Doc` always resolves its slab names — `elaborate` (run on every
    // commit, `command::apply`) rejects any Region/Text on an unknown or non-copper slab
    // with the `E_UNKNOWN_SLAB`/`E_POUR_NON_COPPER` family, so `world_features` cannot
    // fail here. An `Err` means an un-elaborated doc bypassed the commit gate — a broken
    // invariant, made loud on purpose: returning an empty (⇒ "clean") violation set for a
    // doc that never materialised would silently pass a shorted board, the worst failure.
    let world = world_features(doc, lib, netlist, rules, &su)
        .expect("world_features on a committed doc (slab gate enforced at commit)");

    // Netted copper conductors, each paired with the copper slab **name** it sits on
    // (forward-derived from its z — Decision 13 rule 3) and whether it is a **pour**
    // (`Shape2D::Area`) vs solid copper (a trace/via/pad). The slab name is the
    // clearance-report granularity.
    let conductors: Vec<(String, bool, &Feature, &NetId)> = world
        .iter()
        .filter_map(|nf| {
            let net = nf.net.as_ref()?;
            if nf.feature.role != Role::Conductor {
                return None;
            }
            let layer = feature_slab(&su, &nf.feature)?;
            let is_pour = matches!(&nf.feature.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. }));
            Some((layer, is_pour, &nf.feature, net))
        })
        .collect();
    // Keep-out features (any kind) and the board substrate region, both netless.
    let keepouts: Vec<(KeepoutKind, &Feature)> = world
        .iter()
        .filter_map(|nf| match nf.feature.role {
            Role::Keepout(kind) => Some((kind, &nf.feature)),
            _ => None,
        })
        .collect();
    // The board region for the edge-clearance check. `world_features` emits exactly one
    // `Substrate` feature (the `board_region` — "last `Board` wins" already collapses to
    // one outline ∖ cutouts), so taking the first is taking the only one; if multi-body
    // substrates ever land, edge clearance must union them.
    let substrate: Option<&Region> = world.iter().find_map(|nf| {
        if nf.feature.role != Role::Substrate {
            return None;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        shape.region()
    });

    // --- 2. Clearance: copper of *different* nets must be >= min_clearance. ---
    // `Feature::clears` fuses the z-overlap (same/adjacent slab) and edge-to-edge
    // distance tests. Solid-vs-solid and pour-vs-pour are checked; a pour is knocked
    // out around the solid copper it was built from (by construction, at exactly the
    // clearance), so pour-vs-solid is clean and skipped — checking it would only
    // surface tessellation-slop false positives.
    //
    // COUPLING: the skip is sound *only* because `world_features` knocked pours out at
    // the same `rules.min_clearance` this check uses. Both read that one scalar, so they
    // cannot diverge today. A future **per-net** clearance would break the equivalence
    // (a pour knocked out at net A's clearance vs a solid on net B checked at B's) — at
    // which point pour-vs-solid must be re-enabled. The `is_pour` classification the skip
    // pivots on is asserted below so a mislabelled feature can never silently un-check a
    // real short.
    for i in 0..conductors.len() {
        for j in (i + 1)..conductors.len() {
            let (la, pa, fa, na) = &conductors[i];
            let (_lb, pb, fb, nb) = &conductors[j];
            // A pour is exactly the `Area`-shaped conductors; solid copper is never
            // `Area`. If this ever drifts, the `pa != pb` skip below would drop a real
            // pair — so pin it in debug builds.
            debug_assert_eq!(
                *pa,
                matches!(&fa.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. })),
                "is_pour must track Shape2D::Area"
            );
            if na == nb || pa != pb {
                continue;
            }
            if !fa.clears(fb, rules.min_clearance) {
                out.push(clearance(na, nb, la.clone()));
            }
        }
    }

    // --- 2b. Keep-out enforcement (issue 0023). Copper (incl. pours) must clear a
    // copper/route keep-out. Component keep-outs (courtyards) are a placement concern,
    // not DRC, so they are not checked here. z-overlap gates the keep-out to its slab.
    for (_la, _pa, f, net) in &conductors {
        for (kind, kf) in &keepouts {
            if !matches!(kind, KeepoutKind::Copper | KeepoutKind::Route) {
                continue;
            }
            if !f.clears(kf, rules.keepout_clearance) {
                out.push(Violation::Keepout {
                    net: (*net).clone(),
                    kind: *kind,
                });
            }
        }
    }

    // --- 2c. Board-edge clearance (issue 0023). Solid copper grown by the edge rule
    // must stay inside the board region (outline ∖ cutouts); a piece that spills out —
    // over the edge or into a cutout wall — leaves a non-empty remainder. Pours are
    // exempt (they are authored inset; edge pull-back is a fill concern).
    if let Some(board) = substrate {
        for (_la, is_pour, f, net) in &conductors {
            if *is_pour {
                continue;
            }
            let Extent::Prism { shape, .. } = &f.extent;
            let grown = shape_to_region(&shape.inflated(rules.edge_clearance), DEFAULT_CIRCLE_SEGS);
            if !difference(&grown, board).is_empty() {
                out.push(Violation::EdgeClearance {
                    net: (*net).clone(),
                });
            }
        }
    }

    // --- 3. Connectivity completeness (ratsnest) via union-find. ---
    for (nid, pins) in netlist {
        let pts = &net_pads[nid];
        // A net with fewer than two pins is trivially "routed".
        if pins.len() < 2 {
            continue;
        }
        let net_traces: Vec<&Trace> = doc.traces.values().filter(|t| t.net == *nid).collect();
        let net_vias: Vec<&Via> = doc.vias.values().filter(|v| v.net == *nid).collect();
        // This net's pour copper as `(slab name, island)` pairs, sourced from the same
        // unified stream. Same-net fills on a slab are unioned *before* islanding, so
        // overlapping same-net pours merge into one island (no spurious split); the slab
        // name is kept so trace/via incidence can be gated by it (copper on a different
        // slab reaches the pour only through a via). A pad/trace/via on an island joins
        // everything else on it, so a pour collapses the ratsnest; a pour fragmented by
        // its knockouts leaves pads on different islands disconnected (honest DRC).
        let mut by_layer: BTreeMap<String, Vec<Region>> = BTreeMap::new();
        for (la, is_pour, f, net) in &conductors {
            if !is_pour || *net != nid {
                continue;
            }
            let Extent::Prism { shape, .. } = &f.extent;
            if let Some(region) = shape.region() {
                by_layer.entry(la.clone()).or_default().push(region.clone());
            }
        }
        let net_islands: Vec<(String, Region)> = by_layer
            .into_iter()
            .flat_map(|(layer, fills)| {
                union_all(fills)
                    .islands()
                    .into_iter()
                    .map(move |i| (layer.clone(), i))
            })
            .collect();
        let islands = pin_islands(
            pts,
            &net_traces,
            &net_vias,
            &net_islands,
            &su,
            rules.touch_tol,
        );
        if islands > 1 {
            out.push(Violation::Unrouted {
                net: nid.clone(),
                islands,
            });
        }
    }

    out.sort();
    out.dedup();
    out
}

/// The copper slab **name** a single-slab copper [`Feature`] sits on, by matching its z
/// to the stackup's copper slabs (a **forward** query — identity flows from the stackup,
/// never reconstructed heuristically; Decision 13 rule 3). `None` if the feature's z is
/// not a copper slab. The one place DRC/export turns a converged feature back into the
/// slab name its report/file is keyed on.
pub(crate) fn feature_slab(su: &Stackup, f: &Feature) -> Option<String> {
    let Extent::Prism { z, .. } = &f.extent;
    su.copper_slabs()
        .iter()
        .find(|s| s.z == *z)
        .map(|s| s.name.clone())
}

/// Normalised clearance violation: net ids sorted so a pair reports once.
pub(super) fn clearance(a: &NetId, b: &NetId, layer: String) -> Violation {
    let (lo, hi) = if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    };
    Violation::Clearance {
        a: lo,
        b: hi,
        layer,
    }
}
