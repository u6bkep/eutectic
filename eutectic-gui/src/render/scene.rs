//! Scene types — the renderer's ingest contract (renderer-spec §2).
//!
//! A [`Scene`] is appearance-free geometry: an **anchor** + content bounds,
//! an ordered list of **planes** (stable [`PlaneKey`]s in back-to-front
//! composite order), and per plane a list of **primitives** ([`Prim`]) each
//! carrying a **semantic id** (compact index into the semantic state buffer,
//! §5) and a **style class** (resolved through the per-plane style tables,
//! §8). Plane *appearance* (color, alpha, dim, visibility) lives in
//! [`StyleTables`](super::style::StyleTables), never here — a layer toggle or
//! theme swap is a uniform rewrite, not a scene rebuild.
//!
//! # Determinism contract
//!
//! Producers guarantee deterministic order: planes enumerate in a fixed
//! stackup-derived order, primitives in the source stream's stable emission
//! order, and semantic ids intern in first-appearance order over that stream
//! (index 0 is always the [`SemanticKey::Chrome`] sentinel). Building the
//! same scene twice from the same document yields **equal** values (asserted
//! by the determinism test in [`board`](super::board)) — scenes are rebuilt
//! per doc revision, never per frame, camera change, or interaction.
//!
//! # Precision
//!
//! Primitives store integer-nm coordinates (or f64 nm where a derived center
//! is not a lattice point — arc centers). The GPU upload is **anchor-relative
//! f32** ([`instance`](super::instance)); the anchor is the content-bounds
//! center, so a board far from the origin costs no mantissa (renderer-spec
//! §7).

use eutectic_core::coord::{Nm, Point};
use eutectic_core::id::{EntityId, NetId, TraceId, ViaId};
use std::collections::BTreeMap;

/// The reserved semantic id of [`SemanticKey::Chrome`] — geometry with no
/// domain identity (grid furniture, previews, unattributable fab artifacts).
/// Interners pin it at index 0, and the state buffer never flags it.
pub const SEM_CHROME: u32 = 0;

/// The domain identity behind a compact semantic id: what a primitive *is*
/// for hover/selection/emphasis purposes. Net where the feature carries one,
/// owning entity otherwise, [`Chrome`](SemanticKey::Chrome) as the sentinel
/// for identity-free geometry (renderer-spec §2). This deliberately mirrors
/// the picker's id vocabulary ([`crate::pick::SemanticId`]) without importing
/// it — the renderer stays app-type-free; the app maps between the two at
/// the selection seam.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SemanticKey {
    /// No domain identity (sentinel, always index [`SEM_CHROME`]).
    Chrome,
    /// A whole net — every feature that carries a net id resolves here, so a
    /// net highlight is one state-buffer write lighting every member feature.
    Net(NetId),
    /// A routed trace that (unexpectedly) carries no net.
    Trace(TraceId),
    /// A via barrel/drill, by id (its drill `Void` is netless but selectable).
    Via(ViaId),
    /// A netless pad or a pad's drill / mask-opening `Void`, by owning
    /// component + **pad number** (the `PinRef` join key, never the pin name).
    Pin { comp: EntityId, pad: String },
    /// A placed component's footprint marking (silk / fab graphics + text).
    Part(EntityId),
    /// The board itself: substrate, outline, mask solids.
    Board,
    /// A board-authored text/silk marking (top-level `Text` directive).
    BoardText,
}

/// Stable identity + z-order key of one visual plane. The scene lists planes
/// in **back-to-front composite order**; the key names the plane for style
/// lookup and WP2's layer panel. Slab-keyed variants carry the authored slab
/// name (`"F.Cu"`, `"B.SilkS"`, …).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaneKey {
    /// The board body (outline ∖ cutouts) as a filled area — composited
    /// first, behind everything.
    Substrate,
    /// The board-edge dashed outline (derived, not a stackup slab).
    Outline,
    /// A copper slab's **pour** fills (`Shape2D::Area` conductors). A
    /// separate plane from the slab's discrete copper so pours can composite
    /// translucent (the outline reads through) while pads/traces stay opaque
    /// — per-plane alpha is a composite uniform, never per-primitive data.
    CopperPour(String),
    /// A copper slab's discrete copper (traces, vias, pads).
    Copper(String),
    /// A solder-mask slab's solids (openings already subtracted at lowering).
    Mask(String),
    /// A silkscreen slab (`Role::Marking`).
    Silk(String),
    /// A fab-documentation slab (`Role::Datum`).
    Fab(String),
    /// Drills / holes — composited after copper, **paints the background
    /// color** (absence-through-everything, matching fab semantics;
    /// renderer-spec §4). Coverage max-blend never needs subtraction.
    Drills,
    /// Schematic drawn wires (WP3) — composited first of the schematic tiers
    /// so wires read *under* symbols, matching the stream's §20d draw order.
    SchematicWire,
    /// Schematic line-art ink (symbol outlines, pin stubs) **and** its
    /// same-color annotation text (headers, pin names, nc marks) — one plane
    /// because they share one appearance; the schematic drawing has no
    /// translucency, so binning by *color role* rather than by shape kind
    /// keeps compositing trivial (see `render::schematic` module docs).
    SchematicInk,
    /// Schematic net tags (annotation text in the tag accent).
    SchematicTag,
    /// Schematic non-semantic chrome: the unplaced-bin divider (dashed) and
    /// its label — composited last, like the stream emits it.
    SchematicChrome,
    /// The dynamic overlay (previews, halos) — not part of a produced scene;
    /// the renderer composites the overlay buffer under this key's style.
    Overlay,
}

/// How a primitive is drawn, resolved through the per-plane style tables:
/// filled coverage or a dashed stroke (pattern id indexes
/// [`StyleTables::dash`](super::style::StyleTables)). Appearance (color /
/// alpha) is *not* here — this is the geometry-shaping part of style only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleClass {
    /// Plain filled coverage.
    Fill,
    /// Dashed stroke; the pattern id selects an (on, off) nm pair from the
    /// style tables. The dash is evaluated procedurally in the fragment
    /// shader from the primitive's accumulated path length ([`Prim::len0`]),
    /// so the pattern flows continuously through corners.
    Dash(u8),
}

/// Text justification for [`PrimShape::TextRun`] (annotation text, §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Justify {
    Left,
    Center,
    Right,
}

/// One appearance-free primitive shape (renderer-spec §2). Coordinates are
/// board-frame nm, y-up.
#[derive(Clone, Debug, PartialEq)]
pub enum PrimShape {
    /// A stroked segment (trace segment, polyline edge, pin): the
    /// `r`-inflation of segment `a`–`b`. `a == b` degenerates to a disc.
    Capsule { a: Point, b: Point, r: Nm },
    /// A filled disc (via, round pad, junction dot).
    Disc { c: Point, r: Nm },
    /// An arc stroke: `half_width`-inflation of the circular arc of `radius`
    /// around `center`, from angle `a0` sweeping to `a1` (radians, board
    /// frame / y-up, CCW positive; `a1 - a0` is the **signed** sweep, |sweep|
    /// ≤ 2π). Center/radius are f64 nm — derived circumcenters are not
    /// lattice points.
    ArcStroke {
        center: [f64; 2],
        radius: f64,
        a0: f64,
        a1: f64,
        half_width: Nm,
    },
    /// A filled polygon interior with holes: oriented rings (CCW islands, CW
    /// holes — the region kernel's convention), already flattened to lines at
    /// the kernel's fixed nm tolerance. Triangulated CPU-side at buffer-build
    /// time ([`tess`](super::tess)).
    Polygon { rings: Vec<Vec<Point>> },
    /// Annotation text (renderer-spec §6) — carried in the scene types for
    /// the schematic producer (WP3); **WP1 renders no text** (the board
    /// producer emits none: silk/fab ink arrives as `Polygon` glyph geometry,
    /// because there the glyphs are the artifact). Renders through an MSDF
    /// glyph atlas when WP3 lands.
    TextRun {
        pos: Point,
        height: Nm,
        justify: Justify,
        content: String,
    },
    // Reserved seam (do not build in WP1): `Mesh` — the 3D mode's vocabulary
    // (gw-09). A sibling *shading strategy* rides the same scene contract by
    // adding a triangle-soup variant here plus a mesh pass; nothing else in
    // the ingest changes. See renderer-spec §2/§11.
}

/// One primitive: shape + semantic id + style class + accumulated path
/// length.
#[derive(Clone, Debug, PartialEq)]
pub struct Prim {
    /// Compact index into the semantic state buffer ([`Scene::semantics`]).
    pub sem: u32,
    /// Geometry-shaping style, resolved through the per-plane tables.
    pub class: StyleClass,
    /// Accumulated path length (nm) at this primitive's **start**, along the
    /// stroke it belongs to — dash patterns flow continuously through
    /// corners because the fragment shader offsets its along-axis parameter
    /// by this (renderer-spec §2). `0.0` for non-stroke primitives.
    pub len0: f64,
    pub shape: PrimShape,
}

impl Prim {
    /// A filled primitive with no dash phase.
    pub fn fill(sem: u32, shape: PrimShape) -> Prim {
        Prim {
            sem,
            class: StyleClass::Fill,
            len0: 0.0,
            shape,
        }
    }
}

/// One scene plane: a stable key plus its primitives in deterministic order.
#[derive(Clone, Debug, PartialEq)]
pub struct Plane {
    pub key: PlaneKey,
    pub prims: Vec<Prim>,
}

/// A produced scene (renderer-spec §2): anchor + content bounds, planes in
/// back-to-front composite order, and the semantic-id table.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    /// The anchor every GPU upload is relative to: the integer-nm center of
    /// the content bounds (renderer-spec §7 precision rule).
    pub anchor: Point,
    /// Content bounds in nm `(x0, y0, x1, y1)`, margin included — what
    /// fit-to-content frames.
    pub bounds: (Nm, Nm, Nm, Nm),
    /// Planes in back-to-front composite order. Empty planes are still
    /// enumerated (stable indices for style tables and WP2's layer panel).
    pub planes: Vec<Plane>,
    /// The semantic-id table: index = the compact id primitives carry;
    /// `[0]` is always [`SemanticKey::Chrome`]. The state buffer (§5) is
    /// sized to this and WP2's selection seam maps domain ids through it.
    pub semantics: Vec<SemanticKey>,
}

impl Scene {
    /// Total primitive count across planes (diagnostics / tests).
    pub fn prim_count(&self) -> usize {
        self.planes.iter().map(|p| p.prims.len()).sum()
    }

    /// The plane with `key`, if enumerated.
    pub fn plane(&self, key: &PlaneKey) -> Option<&Plane> {
        self.planes.iter().find(|p| &p.key == key)
    }
}

/// First-appearance-order interner for [`SemanticKey`]s: producers push keys
/// as the deterministic feature stream yields them, so equal documents
/// produce equal id tables. Index 0 is pinned to [`SemanticKey::Chrome`].
#[derive(Debug, Default)]
pub struct SemanticInterner {
    keys: Vec<SemanticKey>,
    map: BTreeMap<SemanticKey, u32>,
}

impl SemanticInterner {
    pub fn new() -> SemanticInterner {
        let mut i = SemanticInterner {
            keys: Vec::new(),
            map: BTreeMap::new(),
        };
        let chrome = i.intern(SemanticKey::Chrome);
        debug_assert_eq!(chrome, SEM_CHROME);
        i
    }

    /// The compact id for `key`, interning it on first appearance.
    pub fn intern(&mut self, key: SemanticKey) -> u32 {
        if let Some(&id) = self.map.get(&key) {
            return id;
        }
        let id = self.keys.len() as u32;
        self.keys.push(key.clone());
        self.map.insert(key, id);
        id
    }

    /// Finish: the id table for [`Scene::semantics`].
    pub fn into_table(self) -> Vec<SemanticKey> {
        self.keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interner_pins_chrome_at_zero_and_dedupes() {
        let mut i = SemanticInterner::new();
        let n1 = i.intern(SemanticKey::Net(NetId::new("GND")));
        let n2 = i.intern(SemanticKey::Net(NetId::new("VBUS")));
        let n1b = i.intern(SemanticKey::Net(NetId::new("GND")));
        assert_eq!(i.intern(SemanticKey::Chrome), SEM_CHROME);
        assert_eq!((n1, n2, n1b), (1, 2, 1));
        let table = i.into_table();
        assert_eq!(table[0], SemanticKey::Chrome);
        assert_eq!(table.len(), 3);
    }
}
