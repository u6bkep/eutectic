//! The purposed-regions physical model: [`Feature`] = `(role, material?, extent)`,
//! the z-stackup ([`ZRange`]/[`Slab`]/[`Stackup`]), and [`NetFeature`]. See the
//! [`geom`](crate::geom) module docs and docs/architecture.md §8.

use super::limits::{BOARD_THICKNESS, COPPER_THICKNESS, MASK_THICKNESS, SILK_THICKNESS};
use super::shape::{Shape2D, clearance_violated};
use crate::coord::Nm;
use crate::id::{EntityId, NetId, TraceId, ViaId};

// ----------------------------------------------------------------------------
// z-stackup, roles, materials, features.
// ----------------------------------------------------------------------------

/// A vertical extent in nm, `lo ≤ hi`. z increases upward; the board bottom face is
/// `0` and the top face is the board thickness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZRange {
    pub lo: Nm,
    pub hi: Nm,
}

impl ZRange {
    pub fn new(lo: Nm, hi: Nm) -> ZRange {
        ZRange {
            lo: lo.min(hi),
            hi: lo.max(hi),
        }
    }
    /// Do two z-ranges overlap (touching counts)? This is the 2.5D "same/adjacent
    /// layer" test once z comes from discrete stackup slabs.
    pub fn overlaps(&self, o: &ZRange) -> bool {
        self.lo <= o.hi && o.lo <= self.hi
    }
}

/// What a region *is* — kept small and physical. Named PCB features (fiducials,
/// mouse-bites, thermal relief) are compositions over these, not new roles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// Electrically active copper (a pad, trace, via annulus, pour).
    Conductor,
    /// Board body / dielectric. Its outline boundary *is* the board edge.
    Substrate,
    /// Absence of material: a drill, board cutout, milled pocket.
    Void,
    /// Reserved space nothing may intrude into, by kind.
    Keepout(KeepoutKind),
    /// Surface marking (silkscreen).
    Marking,
    /// Solder-mask material (positive). Openings are `Void` deletion volumes at mask
    /// z, not a negative layer (Decision 13 — no negative layers).
    Mask,
    /// A mechanical/reference datum (e.g. an MCAD fit point).
    Datum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeepoutKind {
    Copper,
    Component,
    Drill,
    Route,
}

/// A physical material. Carries a name now; physical properties (resistivity,
/// permittivity, thermal) attach here later so simulation reads the same model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Material {
    pub name: String,
}

impl Material {
    pub fn named(name: &str) -> Material {
        Material { name: name.into() }
    }
}

/// Where a feature is in space. `Prism` is the 2.5D case (a 2D shape over a z-range);
/// `Solid` is reserved for arbitrary 3D (not built — keeps 3D representable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Extent {
    Prism { shape: Shape2D, z: ZRange },
}

/// A purposed region of space: the physical-geometry unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    pub role: Role,
    pub material: Option<Material>,
    pub extent: Extent,
}

impl Feature {
    pub fn prism(role: Role, shape: Shape2D, z: ZRange) -> Feature {
        Feature {
            role,
            material: None,
            extent: Extent::Prism { shape, z },
        }
    }
    pub fn with_material(mut self, m: Material) -> Feature {
        self.material = Some(m);
        self
    }
    fn prism_parts(&self) -> (&Shape2D, &ZRange) {
        match &self.extent {
            Extent::Prism { shape, z } => (shape, z),
        }
    }
    /// Pure-geometry clash: z-ranges overlap **and** the 2D shapes are within
    /// `min_clr` edge-to-edge. *Role/net policy is the caller's* (DRC decides which
    /// feature pairs warrant a check — e.g. different-net conductors).
    pub fn clears(&self, other: &Feature, min_clr: Nm) -> bool {
        let (sa, za) = self.prism_parts();
        let (sb, zb) = other.prism_parts();
        !(za.overlaps(zb) && clearance_violated(sa, sb, min_clr))
    }
}

/// The **owning source entity** a derived [`Feature`] was lowered from — the
/// provenance annotation the render/pick/findings consumers key on (issue 0031).
///
/// `world_features` derives one flat stream of physical geometry from many source
/// entities (traces, vias, pads, pours, the board, silk/text). The net is an
/// *electrical* annotation (Decision 12); this is the orthogonal *structural*
/// annotation — which authored/routed entity a piece of geometry belongs to. It is
/// populated at derivation, where the source entity is in hand, and lets a consumer
/// map a rendered feature back to a selectable id **without a second walk over the
/// doc** (the GUI hit-test's former double-derivation, issue 0031).
///
/// Every variant names an entity using the existing id vocabulary
/// ([`TraceId`]/[`ViaId`]/[`EntityId`]/[`NetId`] from `id.rs`, pad numbers as the
/// `PinRef` join key) — no parallel id space. [`Unattributed`](FeatureOrigin::Unattributed)
/// is the explicit escape hatch for geometry that genuinely has no single owning
/// entity worth naming today (the pierce-everything drill/mask `Void`s); it is a
/// named variant, not a silent `None`, so an engine test can assert *exactly* which
/// feature kinds are unattributed and a future regression surfaces loudly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureOrigin {
    /// A routed trace's copper, by its [`TraceId`].
    Trace(TraceId),
    /// A via's copper (its annular barrel on one spanned copper slab), by [`ViaId`].
    Via(ViaId),
    /// A placed component's **pad** copper, by the owning component ([`EntityId`]) and
    /// the **pad number** — the `PinRef`/net-membership join key (the stable
    /// symbol↔footprint identity), *never* the functional pin name (a review-blocked
    /// bug class; see `eutectic-gui`'s `SemanticId::Pin`).
    Pad { comp: EntityId, pad: String },
    /// A placed component's footprint **graphic or text** marking (silk / fab), by the
    /// owning component. Not electrically netted; distinct from board-authored markings
    /// so a consumer can attribute footprint silk to its part.
    ComponentMarking(EntityId),
    /// An authored **region** — a copper pour, keep-out, or filled void — by its net
    /// (when it carries one) + the slab name it targets. A copper pour's `net` is
    /// `Some` and its net+layer *is* its stable authored identity (a pour has no id of
    /// its own, matching `eutectic-gui`'s `SemanticId::Pour`); a keep-out / netless region
    /// carries `net == None`.
    Region { net: Option<NetId>, layer: String },
    /// The board **substrate / outline** body (and the mask solids derived from that
    /// same board region): geometry whose owning "entity" is the board itself.
    Board,
    /// A **board-authored** text/silk marking (a top-level `Text` directive), distinct
    /// from footprint markings which carry their component.
    BoardText,
    /// Genuinely unattributable derived geometry: the pierce-everything plated drill
    /// `Void`s (via barrels + pad drills) and mask-opening `Void`s, which are fab
    /// artifacts of an entity but are not themselves a selectable owning entity in the
    /// GUI's vocabulary. Named (not a silent absence) so the set stays asserted.
    Unattributed,
}

/// A physical [`Feature`] paired with the electrical **net** it carries, if any, and
/// the source-entity [`FeatureOrigin`] it was derived from.
///
/// This is the converged copper-clearance currency (it replaced the former ad-hoc
/// copper-piece type): the net is an
/// *annotation alongside* the geometry, **never a field on [`Feature`]** —
/// connectivity is authoritative and lives separately (see
/// docs/log/d12-phase0-foundation.md). `net == None` means no
/// electrical identity: board substrate, a silk marking, a void, or a floating pad.
///
/// `origin` is the orthogonal *structural* provenance (issue 0031): which authored /
/// routed entity the geometry belongs to. It is **additive** — the two constructors
/// default it to [`FeatureOrigin::Unattributed`], so every existing consumer (DRC,
/// export, autoroute ingest) is untouched; a derivation site that knows the source
/// entity tags it via [`with_origin`](NetFeature::with_origin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetFeature {
    pub net: Option<NetId>,
    pub feature: Feature,
    pub origin: FeatureOrigin,
}

impl NetFeature {
    /// A netted-or-netless feature with **unattributed** origin. Existing consumers
    /// call this; a derivation that knows its source entity chains
    /// [`with_origin`](NetFeature::with_origin).
    pub fn new(net: Option<NetId>, feature: Feature) -> NetFeature {
        NetFeature {
            net,
            feature,
            origin: FeatureOrigin::Unattributed,
        }
    }
    /// A feature with no electrical identity (substrate, silk, void), unattributed
    /// origin.
    pub fn netless(feature: Feature) -> NetFeature {
        NetFeature::new(None, feature)
    }
    /// Attach the source-entity provenance (builder; issue 0031). Additive: callers
    /// that do not set it leave [`FeatureOrigin::Unattributed`].
    pub fn with_origin(mut self, origin: FeatureOrigin) -> NetFeature {
        self.origin = origin;
        self
    }
    /// Do two features belong to the **same** net? Two unnetted pieces are *not* the
    /// same net — an unnetted piece shares identity with nothing. (The different-net
    /// clearance *policy* stays in the caller; this is just net identity.)
    pub fn same_net(&self, other: &NetFeature) -> bool {
        matches!((&self.net, &other.net), (Some(a), Some(b)) if a == b)
    }
}

/// A copper/dielectric/etc. slab: a named z-range with a default role + material.
/// A "layer" in the familiar sense is one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slab {
    pub name: String,
    pub z: ZRange,
    pub role: Role,
    pub material: Option<Material>,
}

/// The board stackup: the ordered set of slabs that gives a "layer" its real z. The
/// 2.5D view is a projection of this; defaults let a project ignore z entirely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stackup {
    pub slabs: Vec<Slab>,
}

impl Stackup {
    /// The familiar default: 1.6 mm board, 1 oz copper top and bottom, with solder
    /// mask and silkscreen at honest z on each side. Bottom copper at `[0, C]`, top
    /// copper at `[T−C, T]`, core dielectric between; the mask/silk slabs extend
    /// contiguously outward from the outer copper (Decision 13 — silk/mask are named
    /// z-intervals, resolved away at elaboration).
    pub fn default_2layer() -> Stackup {
        let t = BOARD_THICKNESS;
        let c = COPPER_THICKNESS;
        let mask = MASK_THICKNESS;
        let silk = SILK_THICKNESS;
        Stackup {
            slabs: vec![
                Slab {
                    name: "B.SilkS".into(),
                    z: ZRange::new(-mask - silk, -mask),
                    role: Role::Marking,
                    material: Some(Material::named("ink")),
                },
                Slab {
                    name: "B.Mask".into(),
                    z: ZRange::new(-mask, 0),
                    role: Role::Mask,
                    material: Some(Material::named("soldermask")),
                },
                Slab {
                    name: "B.Cu".into(),
                    z: ZRange::new(0, c),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                },
                Slab {
                    name: "core".into(),
                    z: ZRange::new(c, t - c),
                    role: Role::Substrate,
                    material: Some(Material::named("FR4")),
                },
                Slab {
                    name: "F.Cu".into(),
                    z: ZRange::new(t - c, t),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                },
                Slab {
                    name: "F.Mask".into(),
                    z: ZRange::new(t, t + mask),
                    role: Role::Mask,
                    material: Some(Material::named("soldermask")),
                },
                Slab {
                    name: "F.SilkS".into(),
                    z: ZRange::new(t + mask, t + mask + silk),
                    role: Role::Marking,
                    material: Some(Material::named("ink")),
                },
            ],
        }
    }

    /// The z-range of a named slab (the bridge a 2.5D "place this on F.Cu" uses).
    pub fn slab_z(&self, name: &str) -> Option<ZRange> {
        self.slab(name).map(|s| s.z)
    }

    /// The named slab itself (z **and** role/material). A graphic's role comes from
    /// its slab — silk slabs are `Role::Marking`, a fab slab is `Role::Datum`
    /// (Decision 15) — so lowering forward-queries the slab rather than hardcoding.
    pub fn slab(&self, name: &str) -> Option<&Slab> {
        self.slabs.iter().find(|s| s.name == name)
    }

    /// The conductor slabs, ordered **top-most first** (descending z). This is the
    /// bridge from an abstract copper layer to its real z: the top outer copper is
    /// index `0`, the bottom outer copper is the last entry, and inner copper layers
    /// fall in between in physical stack order — matching
    /// [`route::Layer::depth`](crate::route::Layer::depth) (`Top` = 0, `Inner(n)` =
    /// `1+n`, `Bottom` = last).
    pub fn copper_slabs(&self) -> Vec<&Slab> {
        let mut cu: Vec<&Slab> = self
            .slabs
            .iter()
            .filter(|s| s.role == Role::Conductor)
            .collect();
        cu.sort_by_key(|s| std::cmp::Reverse(s.z.hi));
        cu
    }

    /// The z-range of the `i`-th copper layer counting from the top (0 = top outer).
    /// `None` if the stackup has fewer than `i+1` copper layers.
    pub fn nth_copper_from_top(&self, i: usize) -> Option<ZRange> {
        self.copper_slabs().get(i).map(|s| s.z)
    }

    /// The top outer copper z-range (highest-z conductor slab).
    pub fn top_copper(&self) -> Option<ZRange> {
        self.copper_slabs().first().map(|s| s.z)
    }

    /// The bottom outer copper z-range (lowest-z conductor slab).
    pub fn bottom_copper(&self) -> Option<ZRange> {
        self.copper_slabs().last().map(|s| s.z)
    }

    /// The solder-mask slab immediately **outboard** of the top outer copper — the
    /// nearest `Role::Mask` slab sitting above (higher z than) the top copper, i.e. the
    /// mask a top-side pad opens. Resolved by **role + z-position**, not by a hardcoded
    /// slab name (Decision 13 — names are the authored-reference vocabulary, but a
    /// derived lookup queries the stackup): a custom stackup whose mask slab is named
    /// `TopMask` resolves just the same. `None` if there is no top copper or no mask
    /// slab above it (that side simply has no mask to open).
    pub fn top_mask(&self) -> Option<ZRange> {
        let top = self.top_copper()?;
        self.slabs
            .iter()
            .filter(|s| s.role == Role::Mask && s.z.lo >= top.hi)
            .min_by_key(|s| s.z.lo)
            .map(|s| s.z)
    }

    /// The solder-mask slab immediately **outboard** of the bottom outer copper — the
    /// nearest `Role::Mask` slab below (lower z than) the bottom copper. The mirror of
    /// [`top_mask`](Self::top_mask); same role + z-position query.
    pub fn bottom_mask(&self) -> Option<ZRange> {
        let bot = self.bottom_copper()?;
        self.slabs
            .iter()
            .filter(|s| s.role == Role::Mask && s.z.hi <= bot.lo)
            .max_by_key(|s| s.z.hi)
            .map(|s| s.z)
    }

    /// The **outer** copper slabs (top face / bottom face of the copper stack) that no
    /// `Role::Mask` slab covers — the *forgot-one-side* footgun (issue 0024). Names in
    /// deterministic order (top before bottom). The "does a mask cover this side" test
    /// reuses the very [`top_mask`](Self::top_mask)/[`bottom_mask`](Self::bottom_mask)
    /// resolution a pad opening uses ([`PinDef::pad_features`](crate::part::PinDef::pad_features)) —
    /// side is derived from role + z-position, never a parallel notion.
    ///
    /// Returns empty when the stackup has **no** `Role::Mask` slab at all: that is a
    /// deliberately maskless board, not a mistake, so it is silent (per the ticket).
    /// Inner copper is never included — only an outer face is ever exposed to mask.
    pub fn unmasked_outer_copper(&self) -> Vec<String> {
        // A board with zero mask slabs anywhere is deliberately bare copper — silent.
        if !self.slabs.iter().any(|s| s.role == Role::Mask) {
            return Vec::new();
        }
        let cu = self.copper_slabs();
        let mut out = Vec::new();
        if self.top_mask().is_none()
            && let Some(top) = cu.first()
        {
            out.push(top.name.clone());
        }
        // The `out.last()` guard is defensive: a single-copper stackup has one slab that
        // is both top and bottom outer, and if a mask slab existed but sat *inside* that
        // copper's own z-span (outboard of neither face — a physically-nonsensical
        // stackup), both branches would name it. Unreachable for any real stackup.
        if self.bottom_mask().is_none()
            && let Some(bot) = cu.last()
            && out.last() != Some(&bot.name)
        {
            out.push(bot.name.clone());
        }
        out
    }

    /// The physical **board body** vertical extent — the span of the conductor and
    /// substrate slabs (copper + dielectric), lowest face to highest. This is the z a
    /// board substrate prism or a through-hole/plated barrel spans; it deliberately
    /// **excludes** the surface mask and silk slabs, which sit outside the board body
    /// (a drill through the body is what matters, not the ink on top). Falls back to
    /// the full slab span if the stackup has no conductor/substrate slabs at all.
    pub fn board_z(&self) -> Option<ZRange> {
        let body: Vec<&Slab> = self
            .slabs
            .iter()
            .filter(|s| matches!(s.role, Role::Conductor | Role::Substrate))
            .collect();
        // Fall back to all slabs only if there is no board body at all.
        let slabs: Vec<&Slab> = if body.is_empty() {
            self.slabs.iter().collect()
        } else {
            body
        };
        let lo = slabs.iter().map(|s| s.z.lo).min()?;
        let hi = slabs.iter().map(|s| s.z.hi).max()?;
        Some(ZRange::new(lo, hi))
    }

    /// The **full** stackup vertical extent — the span of *every* slab, mask and silk
    /// included. This is the z a through-cut spans: a milled board cutout or a drill
    /// physically pierces the mask and silk as well as the board body, unlike
    /// [`board_z`](Self::board_z) (the body-only extent a substrate prism or a plated
    /// barrel occupies). `None` only for an empty stackup.
    pub fn full_z(&self) -> Option<ZRange> {
        let lo = self.slabs.iter().map(|s| s.z.lo).min()?;
        let hi = self.slabs.iter().map(|s| s.z.hi).max()?;
        Some(ZRange::new(lo, hi))
    }
}
