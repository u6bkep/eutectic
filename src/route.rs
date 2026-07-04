//! Routing: the trace/via/layer representation (tier-2 materialized state) and the
//! geometry + connectivity kernel the DRC query (tier-3) runs on.
//!
//! Per docs/architecture.md, routed copper is **tier-2 materialized state** that
//! lives in the `Doc` alongside component placement, each carrying a `Provenance`
//! bit: a hand-routed trace is `Pinned` (user-authored, treated by a future
//! autorouter as a fixed obstacle), a `Free` trace is solver/auto-driven and
//! regen-able. One provenance ladder governs placement and routing alike — there
//! is no separate "auto" subsystem.
//!
//! The DRC checks themselves are tier-3 (pure, deterministic, cheaply comparable):
//! [`check_drc`] is the reusable query body, called from the incremental engine in
//! `query.rs` (mirroring how ERC is computed there). All geometry is integer
//! nanometres; distance comparisons are done in exact `i128` arithmetic against
//! *squared* thresholds, so no float nondeterminism leaks into a violation set.

use crate::doc::{Doc, MM, Nm, PinRef, Point};
use crate::elaborate::stackup;
use crate::geom::{
    Extent, Feature, KeepoutKind, Material, NetFeature, Role, Shape2D, Stackup, ZRange,
};
use crate::id::{NetId, TraceId};
use crate::part::{PartLib, PinRole, pin_world};
use crate::region::{DEFAULT_CIRCLE_SEGS, Region, difference, shape_to_region, union_all};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// A copper layer *ordinal* — a router-internal working form, not stored identity.
/// `Top`/`Bottom` are the outer copper; `Inner(n)` keeps the model trivially
/// extensible to multilayer boards (n = 0-based inner-layer index). The ordering is
/// the physical stack-up top→bottom, which is what via spans test.
///
/// Per Decision 13 rule 2 / Decision 18, layer *identity* is a slab **name**
/// ([`Trace::layer`], [`Via::span`]); this ordinal survives only inside the
/// autorouter's grid and the DRC/export forward-query at their own boundaries
/// (`slab_layer` ↔ `copper_layers_z`), never as persisted state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Top,
    Inner(u8),
    Bottom,
}

impl Layer {
    /// Position in the physical stack-up (top = 0, descending). Used to test
    /// whether a layer falls within a via's spanned range. `Bottom` sits below any
    /// representable inner layer.
    pub fn depth(self) -> i32 {
        match self {
            Layer::Top => 0,
            Layer::Inner(n) => 1 + n as i32,
            Layer::Bottom => 1 + 256,
        }
    }
}

// Order layers by physical depth so a `Violation` set sorts canonically.
impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.depth().cmp(&other.depth())
    }
}

/// A routed copper polyline on one copper slab, belonging to one net. `layer` is the
/// slab **name** (Decision 13 rule 2 — identity is the name, never a positional
/// ordinal); `width` is the finished copper width (nm); `prov` is `Pinned` for
/// hand/agent routing and `Free` for a future autorouter's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub net: NetId,
    pub layer: String,
    /// Polyline centreline. Two or more points; consecutive points are segments.
    pub path: Vec<Point>,
    pub width: Nm,
    pub prov: crate::doc::Provenance,
}

/// A via: a plated point connecting copper across the copper slabs it spans. Modelled
/// by its centre `at`, a `drill`, a `pad` (annular copper diameter) — a disc of that
/// diameter on every copper slab it spans — and a `span`.
///
/// `span` is `None` for the common **through** via (the full copper extent, top-most
/// to bottom-most copper slab — Decision 18's full-span default), or
/// `Some((from, to))` naming the two copper slabs a blind/buried via terminates on
/// (order-insensitive; the barrel spans every copper slab between them inclusive). The
/// two-name form is parseable and stored even though multilayer stackups are rare
/// today, so blind/buried vias round-trip when they arrive. Names, not ordinals
/// (Decision 13 rule 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Via {
    pub net: NetId,
    pub at: Point,
    pub span: Option<(String, String)>,
    pub drill: Nm,
    pub pad: Nm,
    pub prov: crate::doc::Provenance,
}

impl Via {
    /// Does this via connect copper on the slab with z-range `z`, given the stackup's
    /// copper slabs top-down (`cu`, as [`Stackup::copper_slabs`] orders them)? A `None`
    /// span is the full copper extent (every copper slab); a `Some((from, to))` span is
    /// every copper slab whose depth lies between `from` and `to` inclusive. An
    /// unresolvable named endpoint spans nothing (a committed via always resolves — the
    /// commit-time slab gate).
    pub fn spans_z(&self, cu: &[&crate::geom::Slab], z: &ZRange) -> bool {
        let Some(idx) = cu.iter().position(|s| s.z == *z) else {
            return false;
        };
        match &self.span {
            None => true,
            Some((from, to)) => {
                let (Some(a), Some(b)) = (
                    cu.iter().position(|s| s.name == *from),
                    cu.iter().position(|s| s.name == *to),
                ) else {
                    return false;
                };
                a.min(b) <= idx && idx <= a.max(b)
            }
        }
    }

    /// The copper slabs (top-down, from `cu`) this via's barrel connects — used by the
    /// connectivity check to know which trace/pour layers a via bridges.
    pub fn spanned_slabs<'a>(&self, cu: &'a [&'a crate::geom::Slab]) -> Vec<&'a crate::geom::Slab> {
        cu.iter()
            .filter(|s| self.spans_z(cu, &s.z))
            .copied()
            .collect()
    }
}

/// Manufacturing design rules consumed by DRC. Defaults are a generic 2-layer
/// process: 0.15 mm clearance and 0.15 mm minimum trace width. In production these
/// would be read from the board/source; the prototype carries them as a constant
/// (the DRC query uses `DesignRules::default()`), documented as the one knob to
/// wire to the source when a stack-up/process definition exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesignRules {
    /// Minimum copper-to-copper gap between *different* nets (edge to edge).
    pub min_clearance: Nm,
    /// Minimum finished trace width.
    pub min_trace_width: Nm,
    /// Tolerance for geometric incidence ("touching") in the connectivity/ratsnest
    /// check. Hand-placed coordinates are exact integers, so coincident endpoints
    /// have distance 0; this small slop absorbs deliberate near-misses without
    /// fusing genuinely separate copper.
    pub touch_tol: Nm,
    /// Minimum gap from copper to a copper/route **keep-out** region's edge (issue
    /// 0023). Default `0`: copper that *overlaps* the keep-out violates, but copper
    /// merely touching its edge (an exact zero-gap tangency) does not — the underlying
    /// clearance test is strict `<`. Set a positive value for a real pull-back margin.
    pub keepout_clearance: Nm,
    /// Minimum gap from copper to the **board edge** — the substrate boundary and the
    /// walls of any cutout (issue 0023). Copper closer than this, or spilling outside
    /// the board, violates. Applies to traces/vias/pads; a copper pour is expected to
    /// be authored inset (its relationship to the edge is a fill-pullback concern), so
    /// pours are exempt from this check — see [`check_drc`].
    pub edge_clearance: Nm,
}

impl Default for DesignRules {
    fn default() -> Self {
        DesignRules {
            min_clearance: 150_000,   // 0.15 mm
            min_trace_width: 150_000, // 0.15 mm
            touch_tol: MM / 100,      // 0.01 mm
            keepout_clearance: 0,     // a keep-out edge is a hard boundary
            edge_clearance: 200_000,  // 0.2 mm from the board edge
        }
    }
}

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
fn feature_slab(su: &Stackup, f: &Feature) -> Option<String> {
    let Extent::Prism { z, .. } = &f.extent;
    su.copper_slabs()
        .iter()
        .find(|s| s.z == *z)
        .map(|s| s.name.clone())
}

/// Normalised clearance violation: net ids sorted so a pair reports once.
fn clearance(a: &NetId, b: &NetId, layer: String) -> Violation {
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
/// pair that z-overlaps necessarily shares that slab — which is what lets [`check_drc`]
/// gate clearance with [`Feature::clears`](crate::geom::Feature::clears) (z-overlap ∧
/// distance) and report on that one layer. This is the converged producer that replaced
/// the former discrete same-layer copper-piece model.
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
    rules: &DesignRules,
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
/// happen on a committed doc (the commit-time slab gate) — see [`check_drc`].
pub fn pours(
    doc: &Doc,
    lib: &PartLib,
    netlist: &BTreeMap<NetId, Vec<(PinRef, PinRole)>>,
    rules: &DesignRules,
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
            let layer = feature_slab(su, &nf.feature)?;
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

// ----------------------------------------------------------------------------
// Connectivity: union-find over a net's pins + traces + vias by geometric
// incidence. Two pins are electrically joined iff they end up in one component.
// ----------------------------------------------------------------------------

/// A pin's world centre plus the copper slabs its pad copper actually occupies — the
/// datum layer-honest pour incidence (Decision 19c) pivots on. `slabs` is the set of
/// copper-slab **names** the pad's `Conductor` features land on: one slab for an SMD
/// pad, every copper slab for a drilled/through pad. `all_layers` is the padless
/// compatibility case (Decision 19c): a pin whose footprint carries **no** pad copper
/// (the toy library's bare terminals — real footprints always have copper) keeps the
/// old all-layer incidence, joining any same-net island it sits over regardless of slab.
#[derive(Clone, Debug)]
struct PinPoint {
    at: Point,
    /// Copper-slab names the pad copper occupies; empty iff `all_layers`.
    slabs: std::collections::BTreeSet<String>,
    /// True for a bare (padless) terminal — join islands on any slab.
    all_layers: bool,
}

impl PinPoint {
    /// Does this pin's copper exist on the copper slab named `layer` (so it may join a
    /// pour island there)? The padless-compatibility pin exists on every layer.
    fn on_slab(&self, layer: &str) -> bool {
        self.all_layers || self.slabs.contains(layer)
    }
}

/// The copper slabs a pin's pad occupies, as a [`PinPoint`]. Derives occupancy from the
/// *same* pad-feature lowering `world_features`/`net_features` use (a pad's `Conductor`
/// feature z matched to a copper slab) — never a parallel notion. A pin whose footprint
/// carries no pad copper (or whose component/part/pin cannot be resolved) is flagged
/// `all_layers` (Decision 19c padless compatibility).
fn pin_point(doc: &Doc, lib: &PartLib, su: &Stackup, at: Point, pr: &PinRef) -> PinPoint {
    let cu = su.copper_slabs();
    let mut slabs = std::collections::BTreeSet::new();
    if let Some(c) = doc.components.get(&pr.comp)
        && let Some(def) = lib.get(&c.part)
        && let Some(pin) = def.pins.iter().find(|p| p.number == pr.pin)
    {
        for f in pin.pad_features(c, su) {
            if f.role != Role::Conductor {
                continue;
            }
            let Extent::Prism { z, .. } = &f.extent;
            if let Some(s) = cu.iter().find(|s| s.z == *z) {
                slabs.insert(s.name.clone());
            }
        }
    }
    let all_layers = slabs.is_empty();
    PinPoint {
        at,
        slabs,
        all_layers,
    }
}

struct UnionFind {
    parent: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Number of connected components among a net's *pins*, joining them through the
/// net's copper. Nodes: pins, then traces, then vias, then **pour islands**. Incidence
/// (within `tol`): pin↔pin (coincident pads), pin↔trace and pin↔via (pads are
/// all-layer points), trace↔trace (same layer), trace↔via and via↔via (via must span
/// the layer), and copper↔island (a pin/trace/via landing on a filled pour island
/// joins everything else on that island). Pin↔island incidence is **layer-honest**
/// (Decision 19c): a pin joins a plane only where its pad copper exists on that plane's
/// slab (a [`PinPoint`] carries the occupancy), so an SMD pad does not falsely connect to
/// an inner plane it merely overlaps. Distinct islands are *not* joined to each other, so
/// a pour fragmented by its knockouts leaves pads on different islands in separate
/// components — reported honestly as remaining ratsnest islands.
fn pin_islands(
    pins: &[PinPoint],
    traces: &[&Trace],
    vias: &[&Via],
    pour_islands: &[(String, crate::region::Region)],
    su: &Stackup,
    tol: Nm,
) -> usize {
    let cu = su.copper_slabs();
    // A via's spanned copper-slab index range (from `cu`, top-down), for the span-overlap
    // tests. An unresolvable named span occupies nothing.
    let via_span = |v: &Via| -> Option<(usize, usize)> {
        match &v.span {
            None => (!cu.is_empty()).then(|| (0, cu.len() - 1)),
            Some((from, to)) => {
                let a = cu.iter().position(|s| s.name == *from)?;
                let b = cu.iter().position(|s| s.name == *to)?;
                Some((a.min(b), a.max(b)))
            }
        }
    };
    // Does a via span the copper slab named `name`?
    let via_spans_name = |v: &Via, name: &str| -> bool {
        let (Some((lo, hi)), Some(idx)) = (via_span(v), cu.iter().position(|s| s.name == name))
        else {
            return false;
        };
        lo <= idx && idx <= hi
    };
    let (np, nt, nv) = (pins.len(), traces.len(), vias.len());
    let mut uf = UnionFind::new(np + nt + nv + pour_islands.len());
    let trace_node = |i: usize| np + i;
    let via_node = |i: usize| np + nt + i;
    let island_node = |i: usize| np + nt + nv + i;

    // pin ↔ pin
    for i in 0..np {
        for j in (i + 1)..np {
            if seg_within(pins[i].at, pins[i].at, pins[j].at, pins[j].at, tol, false) {
                uf.union(i, j);
            }
        }
    }
    // pin ↔ trace, pin ↔ via
    for (pi, p) in pins.iter().enumerate() {
        for (ti, t) in traces.iter().enumerate() {
            if point_on_polyline(p.at, &t.path, tol) {
                uf.union(pi, trace_node(ti));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if seg_within(p.at, p.at, v.at, v.at, tol, false) {
                uf.union(pi, via_node(vi));
            }
        }
    }
    // trace ↔ trace (same layer)
    for i in 0..nt {
        for j in (i + 1)..nt {
            if traces[i].layer == traces[j].layer
                && polylines_closer_than_inc(&traces[i].path, &traces[j].path, tol)
            {
                uf.union(trace_node(i), trace_node(j));
            }
        }
    }
    // trace ↔ via (via spans the trace's layer)
    for (ti, t) in traces.iter().enumerate() {
        for (vi, v) in vias.iter().enumerate() {
            if via_spans_name(v, &t.layer) && point_on_polyline(v.at, &t.path, tol) {
                uf.union(trace_node(ti), via_node(vi));
            }
        }
    }
    // via ↔ via (coincident, spans overlap)
    for i in 0..nv {
        for j in (i + 1)..nv {
            let (u, w) = (vias[i], vias[j]);
            let overlap = match (via_span(u), via_span(w)) {
                (Some((ulo, uhi)), Some((wlo, whi))) => ulo <= whi && wlo <= uhi,
                _ => false,
            };
            if overlap && seg_within(u.at, u.at, w.at, w.at, tol, false) {
                uf.union(via_node(i), via_node(j));
            }
        }
    }
    // copper ↔ pour island: a pad/trace/via whose copper lands on a filled island is
    // electrically that island. A pin joins an island only where its pad copper actually
    // exists on that island's slab (Decision 19c, PoC finding F1): an SMD pad on F.Cu is
    // NOT connected to an inner-layer plane it merely overlaps in XY — that would report
    // connectivity with zero stitching vias, the one direction this model never lies. A
    // drilled/through pad, whose barrel spans every copper slab, joins an island on any
    // slab it sits over. The padless-compatibility pin (bare toy-library terminal, no pad
    // copper) keeps all-layer incidence — see [`PinPoint`]. Traces and vias ARE
    // layer-specific, so they join an island only on the pour's own layer (a trace/via
    // overlapping the pour in XY but on another layer reaches it only through a via).
    for (ii, (layer, isl)) in pour_islands.iter().enumerate() {
        for (pi, p) in pins.iter().enumerate() {
            if p.on_slab(layer) && isl.contains_point(p.at) {
                uf.union(pi, island_node(ii));
            }
        }
        for (ti, t) in traces.iter().enumerate() {
            if t.layer == *layer && t.path.iter().any(|p| isl.contains_point(*p)) {
                uf.union(trace_node(ti), island_node(ii));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if via_spans_name(v, layer) && isl.contains_point(v.at) {
                uf.union(via_node(vi), island_node(ii));
            }
        }
    }

    let mut roots = std::collections::BTreeSet::new();
    for i in 0..np {
        let r = uf.find(i);
        roots.insert(r);
    }
    roots.len()
}

// ----------------------------------------------------------------------------
// Integer geometry. All comparisons are exact (i128, squared thresholds); nothing
// here uses floating point.
// ----------------------------------------------------------------------------

/// `(a-o) × (b-o)` — twice the signed area of triangle o,a,b. Sign = orientation.
fn cross(o: Point, a: Point, b: Point) -> i128 {
    let (ax, ay) = ((a.x - o.x) as i128, (a.y - o.y) as i128);
    let (bx, by) = ((b.x - o.x) as i128, (b.y - o.y) as i128);
    ax * by - ay * bx
}

/// Is collinear point `p` within the bounding box of segment a–b?
fn on_segment(a: Point, b: Point, p: Point) -> bool {
    a.x.min(b.x) <= p.x && p.x <= a.x.max(b.x) && a.y.min(b.y) <= p.y && p.y <= a.y.max(b.y)
}

/// Do segments a–b and c–d intersect (proper crossing or collinear touch)?
fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let d1 = cross(c, d, a);
    let d2 = cross(c, d, b);
    let d3 = cross(a, b, c);
    let d4 = cross(a, b, d);
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    (d1 == 0 && on_segment(c, d, a))
        || (d2 == 0 && on_segment(c, d, b))
        || (d3 == 0 && on_segment(a, b, c))
        || (d4 == 0 && on_segment(a, b, d))
}

/// Exact squared distance from point `p` to segment a–b, as a rational `num/den`
/// (`den` > 0). A degenerate segment (a == b) yields the point-to-point distance.
fn point_seg_dist2(p: Point, a: Point, b: Point) -> (i128, i128) {
    let (vx, vy) = ((b.x - a.x) as i128, (b.y - a.y) as i128);
    let (wx, wy) = ((p.x - a.x) as i128, (p.y - a.y) as i128);
    let den = vx * vx + vy * vy; // |v|^2
    if den == 0 {
        return (wx * wx + wy * wy, 1);
    }
    let tnum = wx * vx + wy * vy; // w·v
    if tnum <= 0 {
        return (wx * wx + wy * wy, 1); // closest endpoint a
    }
    if tnum >= den {
        let (ux, uy) = ((p.x - b.x) as i128, (p.y - b.y) as i128);
        return (ux * ux + uy * uy, 1); // closest endpoint b
    }
    // Interior: |w|^2 - (w·v)^2 / |v|^2  ==  (|w|^2·|v|^2 - (w·v)^2) / |v|^2.
    let ww = wx * wx + wy * wy;
    (ww * den - tnum * tnum, den)
}

/// Compare `dist(p, seg a–b)` against threshold `t` (t >= 0): orders the real
/// distance by comparing squared values, exact in i128.
fn point_seg_cmp(p: Point, a: Point, b: Point, t: Nm) -> Ordering {
    let (num, den) = point_seg_dist2(p, a, b);
    let t = t as i128;
    (num).cmp(&(t * t * den))
}

/// Is the minimum distance between segments a–b and c–d within `t`? `strict`
/// selects `< t` (clearance: violation) vs `<= t` (incidence: touching).
fn seg_within(a: Point, b: Point, c: Point, d: Point, t: Nm, strict: bool) -> bool {
    if segments_intersect(a, b, c, d) {
        return if strict { t > 0 } else { true };
    }
    let hit = |ord: Ordering| {
        if strict {
            ord == Ordering::Less
        } else {
            ord != Ordering::Greater
        }
    };
    hit(point_seg_cmp(a, c, d, t))
        || hit(point_seg_cmp(b, c, d, t))
        || hit(point_seg_cmp(c, a, b, t))
        || hit(point_seg_cmp(d, a, b, t))
}

/// Iterate the segments of a polyline (a lone point becomes a degenerate segment).
fn segments(path: &[Point]) -> Vec<(Point, Point)> {
    match path.len() {
        0 => Vec::new(),
        1 => vec![(path[0], path[0])],
        _ => path.windows(2).map(|w| (w[0], w[1])).collect(),
    }
}

/// Is point `p` within `tol` (inclusive) of any segment of `path`?
fn point_on_polyline(p: Point, path: &[Point], tol: Nm) -> bool {
    segments(path)
        .iter()
        .any(|(a, b)| seg_within(p, p, *a, *b, tol, false))
}

/// Are two polylines within `tol` (inclusive) anywhere? (incidence)
fn polylines_closer_than_inc(p: &[Point], q: &[Point], tol: Nm) -> bool {
    let (sp, sq) = (segments(p), segments(q));
    sp.iter().any(|(a, b)| {
        sq.iter()
            .any(|(c, d)| seg_within(*a, *b, *c, *d, tol, false))
    })
}

#[cfg(test)]
mod pour_tests;
