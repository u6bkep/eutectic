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

    // World position (pad *centre*) of every net-member pad, per net. The ratsnest
    // joins pads by incidence at these points; clearance, separately, uses the pads'
    // real copper geometry. Through-hole assumption: a pad participates on every layer.
    let mut net_pads: BTreeMap<NetId, Vec<Point>> = BTreeMap::new();
    for (nid, pins) in netlist {
        let mut pts = Vec::new();
        for (pr, _role) in pins {
            if let Some(c) = doc.components.get(&pr.comp)
                && let Some(def) = lib.get(&c.part)
                && let Some(p) = pin_world(c, def, &pr.pin)
            {
                pts.push(p);
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
    let su = stackup(&doc.source);
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
/// joins everything else on that island). Distinct islands are *not* joined to each
/// other, so a pour fragmented by its knockouts leaves pads on different islands in
/// separate components — reported honestly as remaining ratsnest islands.
fn pin_islands(
    pins: &[Point],
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
            if seg_within(pins[i], pins[i], pins[j], pins[j], tol, false) {
                uf.union(i, j);
            }
        }
    }
    // pin ↔ trace, pin ↔ via
    for (pi, p) in pins.iter().enumerate() {
        for (ti, t) in traces.iter().enumerate() {
            if point_on_polyline(*p, &t.path, tol) {
                uf.union(pi, trace_node(ti));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            if seg_within(*p, *p, v.at, v.at, tol, false) {
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
    // electrically that island. Pins are all-layer points (matching the pad model, as
    // pin↔trace incidence already is), so a same-net pad under its pour connects
    // regardless of layer. Traces and vias ARE layer-specific, so they join an island
    // only on the pour's own layer (a trace/via overlapping the pour in XY but on
    // another layer reaches it only through a via — never implicitly).
    for (ii, (layer, isl)) in pour_islands.iter().enumerate() {
        for (pi, p) in pins.iter().enumerate() {
            if isl.contains_point(*p) {
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
mod pour_tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::doc::{MM, Point};
    use crate::elaborate::{GenDirective as G, RegionDecl, board_rect};
    use crate::geom::{Material, Role, Shape2D, Slab, ZRange};
    use crate::history::History;
    use crate::part::part_library;

    /// The router's ordinal↔name boundary (Decision 13 rule 2): `Top`/`Bottom` resolve
    /// to the outer copper slab names and round-trip through `slab_layer`; `Inner(0)`
    /// must NOT alias onto Bottom on a 2-layer stackup (there is no inner copper).
    #[test]
    fn router_layer_name_boundary_round_trips() {
        let su = crate::geom::Stackup::default_2layer();
        assert_eq!(layer_slab_name(&su, Layer::Top).as_deref(), Some("F.Cu"));
        assert_eq!(layer_slab_name(&su, Layer::Bottom).as_deref(), Some("B.Cu"));
        assert_eq!(
            layer_slab_name(&su, Layer::Inner(0)),
            None,
            "a 2-layer stackup has no inner copper layer"
        );
        // Names round-trip back to ordinals.
        assert_eq!(slab_layer(&su, "F.Cu"), Some(Layer::Top));
        assert_eq!(slab_layer(&su, "B.Cu"), Some(Layer::Bottom));
        // A non-copper / unknown name resolves to no ordinal.
        assert_eq!(slab_layer(&su, "F.SilkS"), None);
        assert_eq!(slab_layer(&su, "Nope"), None);
    }

    /// Netlist (membership only; roles irrelevant to pours) from a doc's nets.
    fn netlist_of(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
        doc.nets
            .iter()
            .map(|(nid, net)| {
                (
                    nid.clone(),
                    net.members
                        .iter()
                        .map(|pr| (pr.clone(), PinRole::Passive))
                        .collect(),
                )
            })
            .collect()
    }

    /// One single-pad footprint on the given copper layer, so a placed instance's pad
    /// copper sits exactly at the instance origin (1mm square).
    fn one_pad(layer: &str) -> crate::part::PartDef {
        crate::kicad::import_footprint(&format!(
            r#"(footprint "P1" (pad "1" smd rect (at 0 0) (size 1 1) (layers "{layer}")))"#
        ))
        .unwrap()
    }

    fn board_pour_scene(sig_layer: &str) -> (Doc, PartLib) {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        lib.insert("PS".into(), one_pad(sig_layer));
        // A board-covering GND pour on F.Cu; a GND pad at (5,5), a foreign SIG pad at
        // (15,5).
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 20),
            Point::mm(0, 20),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "g".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "s".into(),
                part: "PS".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "s".into(),
                pos: Point::mm(15, 5),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("s".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: outline,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "pour")
            .expect("elaborates");
        (h.doc().clone(), lib)
    }

    /// The `world_features` text seam (Decision 17): footprint labels lowered through the
    /// unified producer honour the doc-wide `font`. A part with an `O` literal anchor,
    /// under a `font` directive resolving to the test TTF, yields a `Role::Marking`
    /// **filled `Area`** (outline glyph) in the world-feature stream — proving the font is
    /// threaded to `world_features`' `part::text_features` call, not just the export one.
    #[test]
    fn world_features_footprint_text_honours_ttf_font() {
        // The test TTF, written to a temp file (fonts resolve by path).
        let mut path = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("ecad-route-ttf-{}-{stamp}.ttf", std::process::id()));
        std::fs::write(&path, crate::ttf::build_test_ttf()).unwrap();

        // A footprint carrying a single silk text anchor.
        let mut lib = part_library();
        lib.insert(
            "LBL".into(),
            crate::part::PartDef {
                name: "LBL".into(),
                pins: vec![],
                interfaces: std::collections::BTreeMap::new(),
                graphics: vec![],
                texts: vec![crate::part::FpText {
                    kind: crate::part::FpTextKind::Literal("O".into()),
                    at: Point { x: 0, y: 0 },
                    height: MM,
                    layer: "F.SilkS".into(),
                    orient: crate::doc::Orient::default(),
                    hide: false,
                }],
                courtyard: None,
                class: None,
            },
        );
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Font {
                path: path.to_string_lossy().into_owned(),
            },
            G::Instance {
                path: "u".into(),
                part: "LBL".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "u".into(),
                pos: Point::mm(5, 5),
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "ttf")
            .expect("elaborates");
        let doc = h.doc().clone();

        let su = stackup(&doc.source);
        let world =
            world_features(&doc, &lib, &netlist_of(&doc), &DesignRules::default(), &su).unwrap();
        let ttf_marks = world
            .iter()
            .filter(|nf| {
                nf.feature.role == Role::Marking
                    && matches!(&nf.feature.extent, Extent::Prism { shape, .. } if matches!(shape, Shape2D::Area { .. }))
            })
            .count();
        assert!(
            ttf_marks >= 1,
            "footprint text reached world_features as a filled Area (TTF), got {ttf_marks}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pour_knocks_out_foreign_keeps_same_net() {
        let (doc, lib) = board_pour_scene("F.Cu");
        let nl = netlist_of(&doc);
        let fills = pours(
            &doc,
            &lib,
            &nl,
            &DesignRules::default(),
            &stackup(&doc.source),
        );
        assert_eq!(fills.len(), 1, "one conductor pour");
        let f = &fills[0];
        assert_eq!(f.net, NetId::new("GND"));
        assert_eq!(f.layer, "F.Cu");
        // Same-net pad stays inside the pour (it connects to it).
        assert!(
            f.fill.contains_point(Point::mm(5, 5)),
            "GND pad inside the pour"
        );
        // Foreign pad is knocked out, with clearance: its centre and a point just
        // inside the clearance ring are not copper; a point beyond the ring is.
        assert!(
            !f.fill.contains_point(Point::mm(15, 5)),
            "SIG pad knocked out"
        );
        assert!(
            !f.fill.contains_point(Point {
                x: 14_400_000,
                y: 5 * MM
            }),
            "inside clearance ring"
        );
        assert!(
            f.fill.contains_point(Point::mm(14, 5)),
            "beyond the clearance ring is copper"
        );
        // Open board area is copper.
        assert!(f.fill.contains_point(Point::mm(10, 15)));
    }

    #[test]
    fn pour_ignores_foreign_copper_on_other_layers() {
        // The SIG pad now lives on B.Cu; a Top pour must not knock it out.
        let (doc, lib) = board_pour_scene("B.Cu");
        let nl = netlist_of(&doc);
        let fills = pours(
            &doc,
            &lib,
            &nl,
            &DesignRules::default(),
            &stackup(&doc.source),
        );
        assert!(
            fills[0].fill.contains_point(Point::mm(15, 5)),
            "different-layer copper is not knocked out"
        );
    }

    #[test]
    fn pour_on_unknown_net_is_rejected() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Instance {
                path: "g".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
                role: Role::Conductor,
                net: Some("GDN".into()), // typo
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "bad")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_NET"),
            "typo'd pour net is a hard fault: {err:?}"
        );
    }

    #[test]
    fn conductor_pour_without_net_is_rejected() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
                role: Role::Conductor,
                net: None,
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "nonet")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_POUR_NO_NET"),
            "netless conductor pour rejected: {err:?}"
        );
    }

    #[test]
    fn conductor_pour_on_non_copper_slab_is_rejected() {
        // A net-bound copper pour targeting the silk slab is nonsense (Decision 13): a
        // hard commit fault, and `pour_fills` never sees it.
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.SilkS".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        // (The unconnected net also faults; collect-all surfaces both — we assert the
        // slab fault is present.)
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "silkpour")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_POUR_NON_COPPER"),
            "pour on silk rejected: {err:?}"
        );
    }

    #[test]
    fn region_on_unknown_slab_is_rejected() {
        let lib = part_library();
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(10, 0), Point::mm(10, 10)]),
                role: Role::Keepout(crate::geom::KeepoutKind::Copper),
                net: None,
                layer: "Z.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "badslab")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
            "unknown slab rejected: {err:?}"
        );
    }

    #[test]
    fn pours_are_deterministic() {
        let (doc, lib) = board_pour_scene("F.Cu");
        let nl = netlist_of(&doc);
        let rules = DesignRules::default();
        assert_eq!(
            pours(&doc, &lib, &nl, &rules, &stackup(&doc.source)),
            pours(&doc, &lib, &nl, &rules, &stackup(&doc.source))
        );
    }

    fn drc(doc: &Doc, lib: &PartLib) -> Vec<Violation> {
        check_drc(doc, lib, &netlist_of(doc), &DesignRules::default())
    }

    /// Two GND pads with no traces are unrouted — until a GND pour covers them, which
    /// collapses the ratsnest (the headline pour win).
    #[test]
    fn pour_connects_same_net_pads() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 20),
            Point::mm(0, 20),
        ]);
        let base = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "g1".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "g2".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g1".into(),
                pos: Point::mm(5, 5),
            },
            G::Place {
                path: "g2".into(),
                pos: Point::mm(15, 15),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
            },
        ];
        // Without a pour and without traces: GND's two pads are disconnected.
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(base.clone())),
            &lib,
            "no-pour",
        )
        .unwrap();
        assert!(
            drc(h.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
            "GND is unrouted without a pour"
        );
        // Add the GND pour: the two pads now share its island ⇒ no longer unrouted.
        let mut with_pour = base;
        with_pour.push(G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "F.Cu".into(),
        }));
        let mut h2 = History::new(Default::default());
        h2.commit(
            Transaction::one(Command::SetSource(with_pour)),
            &lib,
            "pour",
        )
        .unwrap();
        assert!(
            !drc(h2.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
            "the pour connects both GND pads: {:?}",
            drc(h2.doc(), &lib)
        );
    }

    /// A foreign trace cutting fully across the pour splits it into two islands; GND
    /// pads on opposite sides stay disconnected — honest fragmentation reporting.
    #[test]
    fn fragmented_pour_leaves_pads_unrouted() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let outline = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(20, 0),
            Point::mm(20, 20),
            Point::mm(0, 20),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "g1".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "g2".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "s".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g1".into(),
                pos: Point::mm(5, 5),
            }, // below the cut
            G::Place {
                path: "g2".into(),
                pos: Point::mm(5, 15),
            }, // above the cut
            G::Place {
                path: "s".into(),
                pos: Point::mm(10, 10),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("s".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: outline,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "frag")
            .unwrap();
        // A full-width SIG trace at y=10 cuts the GND pour into top/bottom islands.
        let cut = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![Point::mm(0, 10), Point::mm(20, 10)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), cut)),
            &lib,
            "cut",
        )
        .unwrap();
        assert!(
            drc(h.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Unrouted { net, islands } if *net == NetId::new("GND") && *islands == 2
            )),
            "the split pour leaves GND in two islands: {:?}",
            drc(h.doc(), &lib)
        );
    }

    /// Review regression (BUG 1): a same-net trace on a *different* layer that passes
    /// under a pour must NOT be joined through it — cross-layer copper connects only
    /// via a via. Here a B.Cu GND trace runs under an F.Cu GND pour with no via, so
    /// the two GND pads stay disconnected.
    #[test]
    fn cross_layer_trace_not_joined_through_pour() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let left_pour = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(15, 0),
            Point::mm(15, 10),
            Point::mm(0, 10),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(30, 10)),
            G::Instance {
                path: "g1".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "g2".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g1".into(),
                pos: Point::mm(5, 5),
            }, // under the F.Cu pour
            G::Place {
                path: "g2".into(),
                pos: Point::mm(25, 5),
            }, // outside the pour
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: left_pour,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "xlayer")
            .unwrap();
        // A B.Cu GND trace from g2 running left *under* the F.Cu pour (x=10 is inside
        // the pour), but on the bottom layer with no via.
        let t = Trace {
            net: NetId::new("GND"),
            layer: "B.Cu".into(),
            path: vec![Point::mm(25, 5), Point::mm(10, 5)],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "btrace",
        )
        .unwrap();
        assert!(
            drc(h.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Unrouted { net, .. } if *net == NetId::new("GND")
            )),
            "B.Cu trace must not connect through the F.Cu pour without a via: {:?}",
            drc(h.doc(), &lib)
        );
    }

    /// Review regression (BUG 2): two overlapping same-net pours on one layer are one
    /// blob of copper — they must be unioned before islanding, so pads split between
    /// them are connected (not falsely reported as two islands).
    #[test]
    fn overlapping_same_net_pours_merge() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let a = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(18, 0),
            Point::mm(18, 10),
            Point::mm(0, 10),
        ]);
        let b = Shape2D::polygon(vec![
            Point::mm(12, 0),
            Point::mm(30, 0),
            Point::mm(30, 10),
            Point::mm(12, 10),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(30, 10)),
            G::Instance {
                path: "g1".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "g2".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "g1".into(),
                pos: Point::mm(5, 5),
            }, // pour A only
            G::Place {
                path: "g2".into(),
                pos: Point::mm(25, 5),
            }, // pour B only
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: a,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
            G::Region(RegionDecl {
                shape: b,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "twopours")
            .unwrap();
        assert!(
            !drc(h.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Unrouted { net, .. } if *net == NetId::new("GND")
            )),
            "overlapping same-net pours are one island connecting both pads: {:?}",
            drc(h.doc(), &lib)
        );
    }

    /// Mask generation must not perturb DRC. The mask-opening `Void`s that
    /// `pad_features` now emits (and the mask solids `elaborate::features` emits) are
    /// non-conductor geometry; the DRC copper producer (`net_features`) filters to
    /// `Role::Conductor`, so none of it reaches clearance or connectivity, and the
    /// violation set is exactly the copper-only result. This guards that invariant.
    #[test]
    fn mask_generation_does_not_perturb_drc() {
        let (doc, lib) = board_pour_scene("B.Cu");
        let nl = netlist_of(&doc);
        let su = stackup(&doc.source);

        // Sanity: the scene's pads DO generate mask-opening `Void`s, so the exclusion
        // below is a real guard rather than vacuous.
        let produces_openings = doc.components.values().any(|c| {
            lib.get(&c.part).is_some_and(|def| {
                def.pins
                    .iter()
                    .flat_map(|p| p.pad_features(c, &su))
                    .any(|f| f.role == crate::geom::Role::Void)
            })
        });
        assert!(produces_openings, "scene pads produce mask-opening Voids");

        // The DRC copper producer is copper-only: no mask/void feature reaches it, so
        // the violation set is unchanged by the presence of mask geometry.
        let feats = net_features(&doc, &lib, &nl, &su);
        assert!(
            feats
                .iter()
                .all(|(_, nf)| nf.feature.role == crate::geom::Role::Conductor),
            "net_features carries only copper — mask/void never enters DRC"
        );
        assert!(
            !feats.is_empty(),
            "the scene has copper features (the check is non-trivial)"
        );
    }

    /// A fab graphic on a zero-height `Role::Datum` slab (Decision 15) must never
    /// register a physical clash, even where it lies directly over foreign copper and
    /// z-*touches* it (`ZRange::overlaps` is closed). This is the Datum analogue of
    /// `mask_generation_does_not_perturb_drc`: DRC's copper producer (`net_features`)
    /// filters to `Role::Conductor`, and footprint graphics never enter DRC at all, so
    /// a Datum graphic sitting on foreign copper is not a short.
    #[test]
    fn datum_graphic_over_copper_is_not_a_clash() {
        let mut lib = part_library();
        // A plain SIG pad at the origin, and a GND part whose F.Fab graphic runs from
        // its own pad back across the origin — so the fab line lands on the SIG copper.
        lib.insert("SIG".into(), one_pad("F.Cu"));
        lib.insert(
            "GDFAB".into(),
            crate::kicad::import_footprint(
                r#"(footprint "GDFAB"
                    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
                    (fp_line (start 0 0) (end -10 0) (layer "F.Fab") (stroke (width 0.5))))"#,
            )
            .unwrap(),
        );
        // An authored stackup whose zero-height F.Fab datum slab sits at the F.Cu top
        // face, so a fab graphic z-*touches* copper (`lo == hi == 1_600_000`).
        let c = 35_000;
        let t = 1_600_000;
        let stack = |name: &str, lo: Nm, hi: Nm, role: Role, mat: Option<&str>| {
            G::Slab(Slab {
                name: name.into(),
                z: ZRange::new(lo, hi),
                role,
                material: mat.map(Material::named),
            })
        };
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            stack("B.Cu", 0, c, Role::Conductor, Some("copper")),
            stack("core", c, t - c, Role::Substrate, Some("FR4")),
            stack("F.Cu", t - c, t, Role::Conductor, Some("copper")),
            stack("F.Fab", t, t, Role::Datum, None),
            G::Instance {
                path: "sig".into(),
                part: "SIG".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "gd".into(),
                part: "GDFAB".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "sig".into(),
                pos: Point::mm(0, 0),
            },
            G::Place {
                path: "gd".into(),
                pos: Point::mm(10, 0),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("sig".into(), "1".into())],
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("gd".into(), "1".into())],
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "datum")
            .expect("elaborates");
        let doc = h.doc();
        let su = stackup(&doc.source);

        // Sanity (non-vacuous): the fab graphic really does lower to a single
        // `Role::Datum` feature that z-touches AND x/y-overlaps the SIG copper — so if
        // Datum were treated as copper this pair *would* clash geometrically.
        let gd = doc.components.values().find(|c| c.part == "GDFAB").unwrap();
        let gd_def = lib.get(&gd.part).unwrap();
        let datum: Vec<_> = crate::part::graphic_features(gd_def, gd, &su);
        assert_eq!(datum.len(), 1, "one fab graphic → one feature");
        assert_eq!(datum[0].role, Role::Datum, "role comes from the F.Fab slab");
        let sig = doc.components.values().find(|c| c.part == "SIG").unwrap();
        let sig_cu = lib.get(&sig.part).unwrap().pins[0]
            .pad_features(sig, &su)
            .into_iter()
            .find(|f| f.role == Role::Conductor)
            .unwrap();
        assert!(
            !datum[0].clears(&sig_cu, DesignRules::default().min_clearance),
            "the datum graphic geometrically clashes the SIG copper (touch in z, \
             overlap in x/y) — the exclusion below is a real guard"
        );

        // The guard: no SIG/GND clearance violation, because the Datum graphic is
        // netless non-copper and never enters the clearance check.
        assert!(
            !drc(doc, &lib).iter().any(|v| matches!(
                v,
                Violation::Clearance { a, b, .. }
                    if [a, b].contains(&&NetId::new("SIG"))
                        && [a, b].contains(&&NetId::new("GND"))
            )),
            "datum graphic over foreign copper is not a clash: {:?}",
            drc(doc, &lib)
        );
    }

    /// Two different-net pours overlapping on the same layer is a short.
    #[test]
    fn overlapping_pours_short() {
        let mut lib = part_library();
        lib.insert("PT".into(), one_pad("F.Cu"));
        let left = Shape2D::polygon(vec![
            Point::mm(0, 0),
            Point::mm(12, 0),
            Point::mm(12, 12),
            Point::mm(0, 12),
        ]);
        let right = Shape2D::polygon(vec![
            Point::mm(8, 8),
            Point::mm(20, 8),
            Point::mm(20, 20),
            Point::mm(8, 20),
        ]);
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "a".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Instance {
                path: "b".into(),
                part: "PT".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "a".into(),
                pos: Point::mm(2, 2),
            },
            G::Place {
                path: "b".into(),
                pos: Point::mm(18, 18),
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("a".into(), "1".into())],
            },
            G::ConnectPins {
                net: "PWR".into(),
                pins: vec![("b".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: left,
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
            G::Region(RegionDecl {
                shape: right,
                role: Role::Conductor,
                net: Some("PWR".into()),
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "shorts")
            .unwrap();
        assert!(
            drc(h.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Clearance { a, b, .. }
                    if *a == NetId::new("GND") && *b == NetId::new("PWR")
            )),
            "overlapping GND/PWR pours short: {:?}",
            drc(h.doc(), &lib)
        );
    }

    /// Issue 0023: an authored **copper keep-out** now excludes copper — DRC gates the
    /// unified stream's copper against `Role::Keepout` features. A trace crossing a F.Cu
    /// copper keep-out flags `Violation::Keepout`; a keep-out on the *other* layer does
    /// not (z-overlap gates it to its slab).
    #[test]
    fn copper_keepout_is_enforced() {
        use crate::geom::KeepoutKind;
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));
        let mk = |layer: &str| {
            // A SIG pad in a safe corner establishes the net (so its trace may be added);
            // the trace then runs through the keep-out square.
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(3, 3),
                },
                G::ConnectPins {
                    net: "SIG".into(),
                    pins: vec![("p".into(), "1".into())],
                },
                G::Region(RegionDecl {
                    shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                    role: Role::Keepout(KeepoutKind::Copper),
                    net: None,
                    layer: layer.into(),
                }),
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
                .unwrap();
            // A Top trace running straight through the keep-out square's centre.
            let t = Trace {
                net: NetId::new("SIG"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(6, 10), Point::mm(14, 10)],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            };
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), t)),
                &lib,
                "t",
            )
            .unwrap();
            h
        };
        // Keep-out on the trace's own layer (F.Cu): the trace intrudes it.
        let same = mk("F.Cu");
        assert!(
            drc(same.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Keepout { net, kind }
                    if *net == NetId::new("SIG") && *kind == KeepoutKind::Copper
            )),
            "a Top trace crossing a F.Cu copper keep-out must flag: {:?}",
            drc(same.doc(), &lib)
        );
        // Keep-out on B.Cu: a Top trace does not overlap it in z, so no keep-out fault.
        let other = mk("B.Cu");
        assert!(
            !drc(other.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::Keepout { .. })),
            "a B.Cu keep-out must not gate a Top trace: {:?}",
            drc(other.doc(), &lib)
        );
    }

    /// Issue 0023: copper too close to the board edge flags `EdgeClearance`. A trace
    /// hugging the left edge (0.1 mm in, under the 0.2 mm rule) violates; a trace routed
    /// through the board interior does not.
    #[test]
    fn copper_near_board_edge_flags_edge_clearance() {
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));
        let scene = |x_mm: i64| {
            // A centred SIG pad establishes the net; the trace under test runs vertically
            // at `x_mm`/10 mm from the left edge.
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(10, 10),
                },
                G::ConnectPins {
                    net: "SIG".into(),
                    pins: vec![("p".into(), "1".into())],
                },
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "board")
                .unwrap();
            let t = Trace {
                net: NetId::new("SIG"),
                layer: "F.Cu".into(),
                path: vec![
                    Point {
                        x: x_mm * MM / 10,
                        y: 2 * MM,
                    },
                    Point {
                        x: x_mm * MM / 10,
                        y: 18 * MM,
                    },
                ],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            };
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), t)),
                &lib,
                "t",
            )
            .unwrap();
            h
        };
        // Centreline 0.1 mm from the x=0 edge (x_mm/10 = 1 → 0.1 mm): within the rule.
        let near = scene(1);
        assert!(
            drc(near.doc(), &lib).iter().any(
                |v| matches!(v, Violation::EdgeClearance { net } if *net == NetId::new("SIG"))
            ),
            "copper 0.1mm from the edge must flag: {:?}",
            drc(near.doc(), &lib)
        );
        // Centreline 10 mm in: comfortably clear.
        let mid = scene(100);
        assert!(
            !drc(mid.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::EdgeClearance { .. })),
            "interior copper must not flag edge clearance: {:?}",
            drc(mid.doc(), &lib)
        );
    }

    /// A `Route` keep-out is enforced like a `Copper` one; a `Component` keep-out (a
    /// courtyard — a placement concern) is NOT a DRC copper fault (guards against
    /// double-reporting vs the placement courtyard verify).
    #[test]
    fn route_keepout_enforced_component_keepout_ignored() {
        use crate::geom::KeepoutKind;
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));
        let mk = |kind: KeepoutKind| {
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(3, 3),
                },
                G::ConnectPins {
                    net: "SIG".into(),
                    pins: vec![("p".into(), "1".into())],
                },
                G::Region(RegionDecl {
                    shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                    role: Role::Keepout(kind),
                    net: None,
                    layer: "F.Cu".into(),
                }),
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
                .unwrap();
            let t = Trace {
                net: NetId::new("SIG"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(6, 10), Point::mm(14, 10)],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            };
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), t)),
                &lib,
                "t",
            )
            .unwrap();
            h
        };
        let route = mk(KeepoutKind::Route);
        assert!(
            drc(route.doc(), &lib).iter().any(|v| matches!(
                v,
                Violation::Keepout { kind, .. } if *kind == KeepoutKind::Route
            )),
            "a Route keep-out gates copper: {:?}",
            drc(route.doc(), &lib)
        );
        let comp = mk(KeepoutKind::Component);
        assert!(
            !drc(comp.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::Keepout { .. })),
            "a Component keep-out (courtyard) is not a DRC copper fault: {:?}",
            drc(comp.doc(), &lib)
        );
    }

    /// Boundary at clearance 0: copper whose edge is *exactly tangent* to a keep-out
    /// (zero gap) does not violate — the clearance test is strict `<`. Only overlap does.
    #[test]
    fn keepout_tangent_does_not_violate() {
        use crate::geom::KeepoutKind;
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::Place {
                path: "p".into(),
                pos: Point::mm(3, 3),
            },
            G::ConnectPins {
                net: "SIG".into(),
                pins: vec![("p".into(), "1".into())],
            },
            // Keep-out square spans x ∈ [8mm, 12mm].
            G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                role: Role::Keepout(KeepoutKind::Copper),
                net: None,
                layer: "F.Cu".into(),
            }),
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "ko")
            .unwrap();
        // A vertical trace (width 0.15mm ⇒ r = 0.075mm) whose centreline is 0.075mm left
        // of the keep-out edge, so its right copper edge lands exactly on x = 8mm.
        let t = Trace {
            net: NetId::new("SIG"),
            layer: "F.Cu".into(),
            path: vec![
                Point {
                    x: 8 * MM - 75_000,
                    y: 6 * MM,
                },
                Point {
                    x: 8 * MM - 75_000,
                    y: 14 * MM,
                },
            ],
            width: 150_000,
            prov: crate::doc::Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "t",
        )
        .unwrap();
        assert!(
            !drc(h.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::Keepout { .. })),
            "copper tangent to the keep-out edge (gap 0) must not violate: {:?}",
            drc(h.doc(), &lib)
        );
    }

    /// Edge clearance: copper fully outside the board flags, copper inside a cutout hole
    /// flags, and a copper pour reaching the board edge is exempt (pull-back is a fill
    /// concern, not a DRC fault).
    #[test]
    fn edge_clearance_outside_cutout_and_pour_exempt() {
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));

        // (a) A trace entirely outside the 10×10 board (at x = 12mm).
        let outside = {
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(10, 10)),
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(5, 5),
                },
                G::ConnectPins {
                    net: "SIG".into(),
                    pins: vec![("p".into(), "1".into())],
                },
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "o")
                .unwrap();
            let t = Trace {
                net: NetId::new("SIG"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(12, 2), Point::mm(12, 8)],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            };
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), t)),
                &lib,
                "t",
            )
            .unwrap();
            h
        };
        assert!(
            drc(outside.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::EdgeClearance { .. })),
            "copper outside the board must flag edge clearance: {:?}",
            drc(outside.doc(), &lib)
        );

        // (b) A trace inside a cutout hole (the cutout wall is a board edge).
        let in_cutout = {
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Cutout {
                    shape: Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM),
                },
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(3, 3),
                },
                G::ConnectPins {
                    net: "SIG".into(),
                    pins: vec![("p".into(), "1".into())],
                },
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "c")
                .unwrap();
            // A short trace inside the [8,12]² cutout.
            let t = Trace {
                net: NetId::new("SIG"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(9, 10), Point::mm(11, 10)],
                width: 150_000,
                prov: crate::doc::Provenance::Pinned,
            };
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), t)),
                &lib,
                "t",
            )
            .unwrap();
            h
        };
        assert!(
            drc(in_cutout.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::EdgeClearance { .. })),
            "copper inside a cutout must flag edge clearance: {:?}",
            drc(in_cutout.doc(), &lib)
        );

        // (c) A board-covering pour reaches the edge but is EXEMPT from edge clearance.
        let pour = {
            let src = vec![
                board_rect(Point::mm(0, 0), Point::mm(20, 20)),
                G::Instance {
                    path: "p".into(),
                    part: "P".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                G::Place {
                    path: "p".into(),
                    pos: Point::mm(10, 10),
                },
                G::ConnectPins {
                    net: "GND".into(),
                    pins: vec![("p".into(), "1".into())],
                },
                G::Region(RegionDecl {
                    shape: Shape2D::polygon(vec![
                        Point::mm(0, 0),
                        Point::mm(20, 0),
                        Point::mm(20, 20),
                        Point::mm(0, 20),
                    ]),
                    role: Role::Conductor,
                    net: Some("GND".into()),
                    layer: "F.Cu".into(),
                }),
            ];
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(src)), &lib, "p")
                .unwrap();
            h
        };
        assert!(
            !drc(pour.doc(), &lib)
                .iter()
                .any(|v| matches!(v, Violation::EdgeClearance { .. })),
            "a pour at the board edge is exempt from edge clearance: {:?}",
            drc(pour.doc(), &lib)
        );
    }

    /// The commit gate that makes `world_features`' fail-loud sound: a `SetSource` naming
    /// a typo'd slab is REJECTED at commit (via `elaborate`), so no doc with an
    /// unresolvable slab ever reaches DRC. (Companion to `region_on_unknown_slab_is_rejected`,
    /// pinning the Conductor-pour variant the reviewer flagged.)
    #[test]
    fn setsource_conductor_on_bad_slab_is_rejected_at_commit() {
        let mut lib = part_library();
        lib.insert("P".into(), one_pad("F.Cu"));
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(10, 10)),
            G::Instance {
                path: "p".into(),
                part: "P".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            G::ConnectPins {
                net: "GND".into(),
                pins: vec![("p".into(), "1".into())],
            },
            G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(5, 5), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cuu".into(), // typo
            }),
        ];
        let mut h = History::new(Default::default());
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "typo")
            .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_SLAB"),
            "a Conductor pour on a typo'd slab is rejected at commit: {err:?}"
        );
    }

    /// Fail-loud, not fail-silent (the reviewer's finding 1): if a doc that bypassed the
    /// commit gate (so its slab does not resolve) somehow reaches DRC, `check_drc` must
    /// PANIC — never return an empty (⇒ "clean") bill for a board that never
    /// materialised. Here we hand-build such a `Doc` directly, without committing.
    #[test]
    #[should_panic(expected = "committed doc")]
    fn drc_on_unmaterialized_bad_slab_doc_panics() {
        let doc = Doc {
            source: vec![G::Region(RegionDecl {
                shape: Shape2D::rect(Point::mm(1, 1), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cuu".into(), // never resolves
            })],
            ..Default::default()
        };
        // Must panic (world_features errors on the unresolvable slab), not return empty.
        let _ = check_drc(
            &doc,
            &part_library(),
            &BTreeMap::new(),
            &DesignRules::default(),
        );
    }
}
